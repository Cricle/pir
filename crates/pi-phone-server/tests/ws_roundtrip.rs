use std::time::Duration;

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use futures_util::{SinkExt, StreamExt};
use pir_phone_server::config::Config;
use pir_phone_server::http;
use pir_phone_server::state::AppState;
use pir_phone_server::store::Store;
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::Message;

type Ws =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

async fn next_json(ws: &mut Ws) -> anyhow::Result<Value> {
    loop {
        let message = tokio::time::timeout(std::time::Duration::from_secs(5), ws.next())
            .await
            .map_err(|_| anyhow::anyhow!("timed out waiting for a WebSocket frame"))?;
        match message {
            Some(Ok(Message::Text(text))) => {
                let value: Value = serde_json::from_str(&text)?;
                eprintln!("recv: {value}");
                return Ok(value);
            }
            Some(Ok(Message::Ping(_))) | Some(Ok(Message::Pong(_))) => {}
            Some(Ok(Message::Close(frame))) => anyhow::bail!("socket closed: {frame:?}"),
            Some(Ok(other)) => anyhow::bail!("unexpected message: {other:?}"),
            Some(Err(error)) => return Err(error.into()),
            None => anyhow::bail!("socket ended"),
        }
    }
}

async fn next_until(ws: &mut Ws, predicate: impl Fn(&Value) -> bool) -> anyhow::Result<Value> {
    for _ in 0..20 {
        let value = next_json(ws).await?;
        if predicate(&value) {
            return Ok(value);
        }
    }
    anyhow::bail!("expected frame not found")
}

#[tokio::test]
async fn agent_client_roundtrip() -> anyhow::Result<()> {
    let dir = std::env::temp_dir().join(format!("pir-phone-test-{}", uuid::Uuid::new_v4()));
    let store = Store::open(&dir)?;
    let user = store.create_user("alice", None)?;
    let (user_token, _) = store.create_token(&user.id, "user", Some("test"), None)?;
    let device = store.create_device(&user.id, "test-device", None)?;
    let (device_token, _) =
        store.create_token(&user.id, "device", Some("test"), Some(&device.id))?;

    let config = Config::load(dir.clone(), "127.0.0.1:0".parse()?, None, vec![])?;
    let state = AppState::new(config, store);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let server = tokio::spawn(async move {
        axum::serve(listener, http::router(state)).await.unwrap();
    });

    let mut agent_request = format!("ws://{addr}/agent").into_client_request()?;
    agent_request.headers_mut().insert(
        "Authorization",
        HeaderValue::from_str(&format!("Bearer {device_token}"))?,
    );
    let (mut agent, _) = tokio_tungstenite::connect_async(agent_request).await?;
    agent
        .send(Message::Text(
            json!({
                "type": "hello",
                "version": 1,
                "deviceId": device.id,
                "deviceName": "test-device",
                "platform": "test",
                "sessions": [{
                    "id": "parent-session",
                    "label": "test session",
                    "kind": "parent",
                    "cwd": "/tmp",
                    "model": { "provider": "test", "id": "model", "name": "Model" },
                    "thinkingLevel": "medium",
                    "isStreaming": false,
                    "messageCount": 1
                }]
            })
            .to_string()
            .into(),
        ))
        .await?;
    eprintln!("agent hello sent");
    let hello_ok = next_json(&mut agent).await?;
    assert_eq!(hello_ok["type"], "hello-ok");
    assert_eq!(
        hello_ok["sessionIds"][0],
        format!("{}:parent-session", device.id)
    );

    let encoded = URL_SAFE_NO_PAD.encode(user_token.as_bytes());
    let mut client_request = format!("ws://{addr}/ws").into_client_request()?;
    client_request.headers_mut().insert(
        "Sec-WebSocket-Protocol",
        HeaderValue::from_str(&format!("pi-phone.v1, bearer.{encoded}"))?,
    );
    let (mut client, _) = tokio_tungstenite::connect_async(client_request).await?;
    eprintln!("client connected");

    let status = next_until(&mut client, |value| {
        value["channel"] == "server" && value["event"] == "status"
    })
    .await?;
    assert_eq!(status["data"]["protocolVersion"], 1);

    let catalog = next_until(&mut client, |value| {
        value["channel"] == "sessions" && value["event"] == "catalog"
    })
    .await?;
    let session_id = catalog["data"]["sessions"][0]["id"]
        .as_str()
        .expect("catalog session id")
        .to_owned();
    eprintln!("catalog session: {session_id}");

    client
        .send(Message::Text(
            json!({"kind": "session-select", "sessionId": session_id})
                .to_string()
                .into(),
        ))
        .await?;
    let selected = next_until(&mut client, |value| {
        value["channel"] == "sessions" && value["event"] == "selected"
    })
    .await?;
    assert_eq!(selected["data"]["sessionId"], session_id);

    client
        .send(Message::Text(
            json!({
                "kind": "rpc",
                "command": { "type": "prompt", "message": "hello", "id": "c1" }
            })
            .to_string()
            .into(),
        ))
        .await?;

    eprintln!("prompt sent");
    let command = next_until(&mut agent, |value| value["type"] == "command").await?;
    let line: Value = serde_json::from_str(command["line"].as_str().unwrap())?;
    let forward_id = line["command"]["id"].as_str().unwrap().to_owned();
    assert!(forward_id.starts_with("f-"));
    assert_eq!(command["sessionId"], "parent-session");

    agent
        .send(Message::Text(
            json!({
                "type": "frame",
                "sessionId": "parent-session",
                "payload": {
                    "channel": "rpc",
                    "payload": {
                        "type": "response",
                        "id": forward_id,
                        "command": "prompt",
                        "success": true,
                        "data": { "disposition": "started" }
                    }
                }
            })
            .to_string()
            .into(),
        ))
        .await?;

    let response = next_until(&mut client, |value| {
        value["channel"] == "rpc" && value["payload"]["type"] == "response"
    })
    .await?;
    assert_eq!(response["payload"]["id"], "c1");
    assert_eq!(response["payload"]["success"], true);

    agent
        .send(Message::Text(
            json!({
                "type": "frame",
                "sessionId": "parent-session",
                "payload": {
                    "channel": "rpc",
                    "payload": {
                        "type": "message_update",
                        "message": { "role": "assistant", "timestamp": 1 },
                        "assistantMessageEvent": { "type": "text_delta", "delta": "hi" }
                    }
                }
            })
            .to_string()
            .into(),
        ))
        .await?;
    let update = next_until(&mut client, |value| {
        value["channel"] == "rpc" && value["payload"]["type"] == "message_update"
    })
    .await?;
    assert_eq!(update["payload"]["assistantMessageEvent"]["delta"], "hi");

    server.abort();
    let _ = std::fs::remove_dir_all(dir);
    Ok(())
}

#[tokio::test]
async fn device_revoke_closes_the_agent_socket() -> anyhow::Result<()> {
    let dir = std::env::temp_dir().join(format!("pir-phone-revoke-{}", uuid::Uuid::new_v4()));
    let store = Store::open(&dir)?;
    let user = store.create_user("alice", None)?;
    let (user_token, _) = store.create_token(&user.id, "user", Some("test"), None)?;
    let device = store.create_device(&user.id, "test-device", None)?;
    let (device_token, _) =
        store.create_token(&user.id, "device", Some("test"), Some(&device.id))?;
    let config = Config::load(dir.clone(), "127.0.0.1:0".parse()?, None, vec![])?;
    let state = AppState::new(config, store);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, http::router(state)).await;
    });

    let mut request = format!("ws://{addr}/agent").into_client_request()?;
    request.headers_mut().insert(
        "Authorization",
        HeaderValue::from_str(&format!("Bearer {device_token}"))?,
    );
    let (mut agent, _) = tokio_tungstenite::connect_async(request).await?;
    agent
        .send(Message::Text(
            json!({
                "type": "hello",
                "version": 1,
                "deviceId": device.id,
                "deviceName": "test-device",
                "platform": "test",
                "sessions": [{
                    "id": "parent-session",
                    "label": "test session",
                    "kind": "parent",
                    "cwd": "/tmp"
                }]
            })
            .to_string()
            .into(),
        ))
        .await?;
    assert_eq!(next_json(&mut agent).await?["type"], "hello-ok");

    let delete = format!(
        "DELETE /api/devices/{} HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\
         Authorization: Bearer {user_token}\r\n\r\n",
        device.id
    );
    let mut http = tokio::net::TcpStream::connect(addr).await?;
    http.write_all(delete.as_bytes()).await?;
    let mut response = Vec::new();
    http.read_to_end(&mut response).await?;
    let response = String::from_utf8_lossy(&response);
    assert!(
        response.starts_with("HTTP/1.1 200"),
        "revoke response: {response}"
    );

    let closed = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match agent.next().await {
                Some(Ok(Message::Close(_))) | None | Some(Err(_)) => return true,
                Some(Ok(_)) => {}
            }
        }
    })
    .await
    .unwrap_or(false);
    assert!(closed, "revoked agent socket did not close promptly");

    server.abort();
    let _ = std::fs::remove_dir_all(dir);
    Ok(())
}
