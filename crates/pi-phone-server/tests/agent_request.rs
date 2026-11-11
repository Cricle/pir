use futures_util::{SinkExt, StreamExt};
use pir_phone_server::config::Config;
use pir_phone_server::http;
use pir_phone_server::state::AppState;
use pir_phone_server::store::Store;
use serde_json::{json, Value};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::Message;

type Ws =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

async fn next_json(ws: &mut Ws) -> anyhow::Result<Value> {
    loop {
        let message = tokio::time::timeout(std::time::Duration::from_secs(5), ws.next())
            .await
            .map_err(|_| anyhow::anyhow!("timeout"))?;
        match message {
            Some(Ok(Message::Text(text))) => return Ok(serde_json::from_str(&text)?),
            Some(Ok(Message::Ping(_))) | Some(Ok(Message::Pong(_))) => {}
            Some(Ok(Message::Close(frame))) => anyhow::bail!("closed: {frame:?}"),
            Some(Err(error)) => return Err(error.into()),
            None => anyhow::bail!("ended"),
            _ => {}
        }
    }
}

#[tokio::test]
async fn http_request_agent_roundtrip() -> anyhow::Result<()> {
    let dir = std::env::temp_dir().join(format!("pir-phone-req-{}", uuid::Uuid::new_v4()));
    let store = Store::open(&dir)?;
    let user = store.create_user("alice", None)?;
    let device = store.create_device(&user.id, "device", None)?;
    let (device_token, _) =
        store.create_token(&user.id, "device", Some("test"), Some(&device.id))?;

    let config = Config::load(dir.clone(), "127.0.0.1:0".parse()?, None, vec![])?;
    let state = AppState::new(config, store);
    let state_for_test = state.clone();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let server = tokio::spawn(async move {
        let _ = http::serve(listener, state).await;
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
                "deviceName": "device",
                "platform": "test",
                "sessions": [{
                    "id": "parent-session",
                    "label": "test",
                    "kind": "parent",
                    "cwd": "/tmp"
                }]
            })
            .to_string()
            .into(),
        ))
        .await?;
    assert_eq!(next_json(&mut agent).await?["type"], "hello-ok");

    let responder = tokio::spawn(async move {
        loop {
            let message = agent.next().await;
            let Some(Ok(Message::Text(text))) = message else {
                break;
            };
            let value: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
            if value["type"] != "command" {
                continue;
            }
            let line: Value =
                serde_json::from_str(value["line"].as_str().unwrap_or("{}")).unwrap_or(Value::Null);
            let forward_id = line["command"]["id"].as_str().unwrap_or("f-0");
            let command = line["command"]["type"].as_str().unwrap_or("unknown");
            let data = if command == "get_messages" {
                json!({ "messages": [{ "role": "user", "content": [{ "type": "text", "text": "hello" }] }] })
            } else if command == "get_available_models" {
                json!({ "models": [{ "provider": "mock", "id": "m1", "name": "Mock" }] })
            } else {
                json!({})
            };
            let frame = json!({
                "type": "frame",
                "sessionId": "parent-session",
                "payload": {
                    "channel": "rpc",
                    "payload": {
                        "type": "response",
                        "id": forward_id,
                        "command": command,
                        "success": true,
                        "data": data
                    }
                }
            });
            if agent
                .send(Message::Text(frame.to_string().into()))
                .await
                .is_err()
            {
                break;
            }
        }
    });

    let session_id = format!("{}:parent-session", device.id);
    let response = pir_phone_server::ws::request_agent(
        &state_for_test,
        &user.id,
        &session_id,
        json!({"kind":"rpc","command":{"type":"get_messages"}}),
        std::time::Duration::from_secs(5),
    )
    .await
    .map_err(anyhow::Error::msg)?;
    assert_eq!(
        response["payload"]["data"]["messages"][0]["content"][0]["text"],
        "hello"
    );

    let models = pir_phone_server::ws::request_agent(
        &state_for_test,
        &user.id,
        &session_id,
        json!({"kind":"rpc","command":{"type":"get_available_models"}}),
        std::time::Duration::from_secs(5),
    )
    .await
    .map_err(anyhow::Error::msg)?;
    assert_eq!(models["payload"]["data"]["models"][0]["id"], "m1");

    responder.abort();
    server.abort();
    let _ = std::fs::remove_dir_all(dir);
    Ok(())
}
