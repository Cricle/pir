//! The streamed file-body transport path: a request body serialized into a
//! temp file is streamed with a `Content-Length`, and a retry re-reads the
//! same file instead of re-serializing the payload.
//!
//! Standalone binary (the `tests/suites/` and `g_*` harnesses belong to other
//! lanes); the module lived inline in `src/ai/transport.rs`, which the source
//! hygiene gate forbids.

use std::collections::BTreeMap;
use std::io::Read as _;
use std::io::Write as _;
use std::net::TcpListener;
use std::time::Duration;

use pir::ai::error::AiResult;
use pir::ai::transport::{ChunkSink, ProviderRequestClient};
use pir::ai::types::ProviderStreamHooks;

#[derive(Default)]
struct CollectSink(Vec<u8>);

impl ChunkSink for CollectSink {
    fn send<'a>(
        &'a mut self,
        chunk: &'a [u8],
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = AiResult<()>> + Send + 'a>> {
        self.0.extend_from_slice(chunk);
        Box::pin(std::future::ready(Ok(())))
    }
}

/// Read one HTTP request (headers + Content-Length body) from `stream`.
fn read_request(stream: &mut std::net::TcpStream) -> (String, Vec<u8>) {
    let mut received = Vec::new();
    let mut buffer = [0u8; 16 * 1024];
    let head_end = loop {
        let read = stream.read(&mut buffer).expect("read request");
        assert!(read > 0, "connection closed before request headers");
        received.extend_from_slice(&buffer[..read]);
        if let Some(index) = received.windows(4).position(|window| window == b"\r\n\r\n") {
            break index;
        }
    };
    let head = String::from_utf8_lossy(&received[..head_end]).into_owned();
    let content_length = head
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            if name.eq_ignore_ascii_case("content-length") {
                value.trim().parse::<usize>().ok()
            } else {
                None
            }
        })
        .expect("Content-Length header");
    let mut body = received[head_end + 4..].to_vec();
    while body.len() < content_length {
        let read = stream.read(&mut buffer).expect("read request body");
        assert!(read > 0, "connection closed before request body");
        body.extend_from_slice(&buffer[..read]);
    }
    (head, body)
}

#[tokio::test]
async fn file_payload_streams_content_length_and_retries() {
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind");
    let address = listener.local_addr().expect("address");
    let server = std::thread::spawn(move || {
        let mut sizes = Vec::new();
        for attempt in 0..2 {
            let (mut stream, _) = listener.accept().expect("accept");
            stream
                .set_read_timeout(Some(Duration::from_secs(10)))
                .expect("read timeout");
            let (_head, body) = read_request(&mut stream);
            sizes.push(body.len());
            let response = if attempt == 0 {
                "HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            } else {
                "HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nOK"
            };
            stream
                .write_all(response.as_bytes())
                .expect("write response");
        }
        sizes
    });

    let client = ProviderRequestClient::with_default_config().expect("client");
    let payload_value = serde_json::json!({
        "messages": [{"role": "user", "content": "x".repeat(256 * 1024)}],
    });
    let payload = client
        .serialize_payload_to_file(&payload_value)
        .expect("file payload");
    let payload_len = payload.len() as usize;
    let payload_path = payload.path().to_path_buf();
    assert!(payload_path.exists(), "temp body file exists");

    let mut sink = CollectSink::default();
    let url = format!("http://{address}/v1/chat/completions");
    let (status, _headers) = client
        .post_payload_file_streaming_with(
            &url,
            &BTreeMap::new(),
            &payload,
            None,
            &ProviderStreamHooks::default(),
            &mut sink,
        )
        .await
        .expect("file streaming request");
    assert_eq!(status, 200);
    assert_eq!(sink.0, b"OK");

    let sizes = server.join().expect("server");
    assert_eq!(sizes, vec![payload_len, payload_len]);

    drop(payload);
    assert!(!payload_path.exists(), "temp body file is removed on drop");
}
