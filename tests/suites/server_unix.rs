//! Unix-domain-socket transport coverage (upstream unix transport preset).
//! Only compiled on Unix; Windows hosts use the TCP transport.

#![cfg(unix)]

use pir::server::{ServerSecurityConfig, UnixServer};
use std::time::Duration;

#[tokio::test]
async fn unix_server_serves_a_full_session_roundtrip() {
    let dir =
        std::env::temp_dir().join(format!("pir-unix-server-{}-roundtrip", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    let socket_path = dir.join("test.sock");
    let _ = std::fs::remove_file(&socket_path);

    let server = UnixServer::bind_with_security(&socket_path, ServerSecurityConfig::default())
        .await
        .expect("bind unix server");
    let server_task = tokio::spawn(async move { server.run().await });

    // Wait for the socket file to appear.
    let mut bound = false;
    for _ in 0..50 {
        if socket_path.exists() {
            bound = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(bound, "unix socket file was not created");

    // 0600 permissions (upstream DEFAULT_SOCKET_MODE).
    use std::os::unix::fs::PermissionsExt;
    let mode = std::fs::metadata(&socket_path)
        .expect("socket metadata")
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o600, "socket must be owner-only");

    // A live socket must not be stolen: the second bind fails with AddrInUse
    // and leaves the serving path untouched. Match explicitly: `expect_err`
    // would require `UnixServer: Debug`, which is not part of its public API.
    let rebound = match UnixServer::bind(&socket_path).await {
        Ok(_) => panic!("binding a live socket must fail"),
        Err(error) => error,
    };
    assert_eq!(rebound.kind(), std::io::ErrorKind::AddrInUse, "{rebound}");
    assert!(socket_path.exists(), "a live socket must not be removed");

    // Full protocol roundtrip over the socket: length-framed CBOR hello →
    // hello reply (the wire is NOT newline-delimited JSON).
    use pir::protocol::{
        encode_client_value, is_server_id, FrameOptions, ServerMessageDecoder, PROTOCOL_VERSION,
    };
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let mut stream = tokio::net::UnixStream::connect(&socket_path)
        .await
        .expect("connect to unix socket");
    let hello = serde_json::json!({ "type": "hello", "version": PROTOCOL_VERSION });
    let frame = encode_client_value(&hello, FrameOptions::default()).expect("encode hello");
    stream.write_all(&frame).await.expect("write hello");
    stream.flush().await.expect("flush hello");

    let mut decoder = ServerMessageDecoder::new(FrameOptions::default()).expect("decoder");
    let mut buffer = [0_u8; 8192];
    let reply = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let read = stream.read(&mut buffer).await.expect("read hello reply");
            assert!(read > 0, "server closed before hello reply");
            if let Some(frame) = decoder
                .push(&buffer[..read])
                .expect("decode hello reply")
                .into_iter()
                .next()
            {
                break frame;
            }
        }
    })
    .await
    .expect("hello reply within timeout");
    assert_eq!(reply["type"], "hello", "{reply}");
    assert_eq!(reply["version"], PROTOCOL_VERSION);
    let server_id = reply["serverId"].as_str().expect("serverId");
    assert!(is_server_id(server_id), "serverId: {server_id}");

    server_task.abort();
    let _ = std::fs::remove_file(&socket_path);
}

#[tokio::test]
async fn unix_server_recovers_a_stale_socket_file() {
    let dir = std::env::temp_dir().join(format!("pir-unix-server-{}-stale", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    let socket_path = dir.join("test.sock");
    let _ = std::fs::remove_file(&socket_path);

    // Dropping a listener without unlinking leaves the file a crashed server
    // would leave behind.
    let stale = std::os::unix::net::UnixListener::bind(&socket_path).expect("create stale socket");
    drop(stale);
    assert!(socket_path.exists());

    let server = UnixServer::bind(&socket_path)
        .await
        .expect("stale socket must be replaced");
    assert_eq!(server.socket_path(), socket_path.as_path());
    server.close();
    assert!(!socket_path.exists(), "close must remove the socket");
}
