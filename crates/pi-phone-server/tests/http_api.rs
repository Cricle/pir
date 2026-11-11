use pir_phone_server::config::Config;
use pir_phone_server::http;
use pir_phone_server::state::AppState;
use pir_phone_server::store::Store;
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

async fn request(addr: std::net::SocketAddr, raw: String) -> (u16, Value) {
    let mut stream = TcpStream::connect(addr).await.expect("connect");
    stream.write_all(raw.as_bytes()).await.expect("write");
    let mut bytes = Vec::new();
    stream.read_to_end(&mut bytes).await.expect("read");
    let text = String::from_utf8_lossy(&bytes);
    let (head, body) = text.split_once("\r\n\r\n").expect("http response");
    let status = head
        .split_whitespace()
        .nth(1)
        .and_then(|value| value.parse::<u16>().ok())
        .expect("status");
    let body = serde_json::from_str(body).unwrap_or(Value::Null);
    (status, body)
}

fn get(path: &str, token: Option<&str>) -> String {
    let auth = token
        .map(|token| format!("Authorization: Bearer {token}\r\n"))
        .unwrap_or_default();
    format!("GET {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n{auth}\r\n")
}

#[tokio::test]
async fn http_surface_and_device_creation() -> anyhow::Result<()> {
    let dir = std::env::temp_dir().join(format!("pir-phone-http-{}", uuid::Uuid::new_v4()));
    let store = Store::open(&dir)?;
    let user = store.create_user("alice", None)?;
    let (token, _) = store.create_token(&user.id, "user", Some("test"), None)?;
    let config = Config::load(dir.clone(), "127.0.0.1:0".parse()?, None, vec![])?;
    let state = AppState::new(config, store);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let server = tokio::spawn(async move {
        let _ = http::serve(listener, state).await;
    });

    let (status, health) = request(addr, get("/api/health", None)).await;
    assert_eq!(status, 200);
    assert_eq!(health["protocolVersion"], 1);
    assert_eq!(health["users"], 1);

    let (status, me) = request(addr, get("/api/me", Some(&token))).await;
    assert_eq!(status, 200);
    assert_eq!(me["user"]["username"], "alice");

    let (status, _) = request(addr, get("/api/me", None)).await;
    assert_eq!(status, 401);

    let body = r#"{"name":"MacBook","platform":"test"}"#;
    let create = format!(
        "POST /api/devices HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\
         Authorization: Bearer {token}\r\nContent-Type: application/json\r\n\
         Content-Length: {}\r\n\r\n{body}",
        body.len()
    );
    let (status, created) = request(addr, create).await;
    assert_eq!(status, 200);
    assert!(created["token"].as_str().unwrap().starts_with("pp_"));
    assert_eq!(created["device"]["name"], "MacBook");

    let (status, devices) = request(addr, get("/api/devices", Some(&token))).await;
    assert_eq!(status, 200);
    assert_eq!(devices["devices"].as_array().unwrap().len(), 1);

    server.abort();
    let _ = std::fs::remove_dir_all(dir);
    Ok(())
}
