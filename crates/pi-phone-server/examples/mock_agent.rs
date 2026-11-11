//! Tiny mock agent for exercising the central relay/PWA without a real pir
//! session. It registers one session and answers prompts with a canned reply.

use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::Message;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let mut server = String::from("ws://127.0.0.1:18987/agent");
    let mut token = String::new();
    let mut device_id = String::new();
    let mut device_name = String::from("Mock device");
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--server" => server = args.next().unwrap_or(server),
            "--token" => token = args.next().unwrap_or_default(),
            "--device-id" => device_id = args.next().unwrap_or_default(),
            "--device-name" => device_name = args.next().unwrap_or(device_name),
            _ => {}
        }
    }
    if token.is_empty() {
        anyhow::bail!("--token is required");
    }
    if device_id.is_empty() {
        device_id = uuid::Uuid::new_v4().to_string();
    }
    if !server.ends_with("/agent") {
        server = format!("{}/agent", server.trim_end_matches('/'));
    }

    let mut request = server.into_client_request()?;
    request.headers_mut().insert(
        "Authorization",
        HeaderValue::from_str(&format!("Bearer {token}"))?,
    );
    let (mut socket, _) = tokio_tungstenite::connect_async(request).await?;
    let hello = json!({
        "type": "hello",
        "version": 1,
        "deviceId": device_id,
        "deviceName": device_name,
        "platform": "mock",
        "sessions": [{
            "id": "parent-session",
            "sessionId": "mock-native-session",
            "label": "Mock session",
            "kind": "parent",
            "cwd": "/mock/workspace",
            "model": { "provider": "mock", "id": "mock-1", "name": "Mock Model" },
            "thinkingLevel": "medium",
            "isStreaming": false,
            "hasPendingUiRequest": true,
            "lastActivityAt": 1729000000000i64,
            "messageCount": 0
        }]
    });
    socket.send(Message::Text(hello.to_string().into())).await?;

    let mut messages: Vec<Value> = Vec::new();
    loop {
        let Some(message) = socket.next().await else {
            break;
        };
        let Message::Text(text) = message? else {
            continue;
        };
        let value: Value = serde_json::from_str(&text)?;
        match value.get("type").and_then(Value::as_str) {
            Some("hello-ok") => {
                send_frame(
                    &mut socket,
                    json!({
                        "channel": "server",
                        "event": "status",
                        "data": {
                            "sessionId": "parent-session",
                            "sessionName": "Mock session",
                            "cwd": "/mock/workspace",
                            "model": { "provider": "mock", "id": "mock-1", "name": "Mock Model" },
                            "thinkingLevel": "medium",
                            "isStreaming": false,
                            "isCompacting": false,
                            "messageCount": messages.len(),
                            "lastError": "",
                            "hasPendingUiRequest": true,
                            "lastActivityAt": 1729000000000i64,
                            "controlOwner": "server"
                        }
                    }),
                )
                .await?;
            }
            Some("command") => {
                let line = value.get("line").and_then(Value::as_str).unwrap_or("{}");
                let line: Value = serde_json::from_str(line).unwrap_or(Value::Null);
                let kind = line.get("kind").and_then(Value::as_str).unwrap_or("");
                if kind == "refresh" {
                    send_frame(
                        &mut socket,
                        json!({
                            "channel": "snapshot",
                            "state": {
                                "model": { "provider": "mock", "id": "mock-1", "name": "Mock Model" },
                                "thinkingLevel": "medium",
                                "isStreaming": false,
                                "isCompacting": false,
                                "messageCount": messages.len()
                            },
                            "messages": messages.clone(),
                            "commands": [],
                            "liveAssistantMessage": null,
                            "liveTools": [],
                            "historyTruncated": false,
                            "totalMessageCount": messages.len()
                        }),
                    )
                    .await?;
                    continue;
                }
                if kind != "rpc" {
                    eprintln!("mock non-rpc command: {kind} line={line}");
                    continue;
                }
                let id = line
                    .get("id")
                    .and_then(Value::as_str)
                    .or_else(|| {
                        line.get("command")
                            .and_then(|command| command.get("id"))
                            .and_then(Value::as_str)
                    })
                    .unwrap_or("mock-1");
                let command = line
                    .get("command")
                    .and_then(|command| command.get("type"))
                    .and_then(Value::as_str)
                    .unwrap_or("prompt");
                eprintln!("mock command: {command} id={id}");
                if command == "get_messages" {
                    send_frame(
                        &mut socket,
                        json!({
                            "channel": "rpc",
                            "payload": {
                                "type": "response",
                                "id": id,
                                "command": command,
                                "success": true,
                                "data": {
                                    "messages": messages.clone(),
                                    "totalMessageCount": messages.len(),
                                    "historyTruncated": false
                                }
                            }
                        }),
                    )
                    .await?;
                    continue;
                }
                let assistant_timestamp = if command == "prompt" {
                    let prompt_text = line
                        .get("command")
                        .and_then(|command| command.get("message"))
                        .and_then(Value::as_str)
                        .unwrap_or("");
                    let timestamp = pir_phone_server::protocol::now_ms() as u64;
                    messages.push(json!({
                        "role": "user",
                        "timestamp": timestamp,
                        "content": [{ "type": "text", "text": prompt_text }]
                    }));
                    messages.push(json!({
                        "role": "assistant",
                        "timestamp": timestamp + 1,
                        "model": "Mock Model",
                        "content": [{ "type": "text", "text": "Hello from the mock central agent." }]
                    }));
                    timestamp + 1
                } else {
                    0
                };
                send_frame(
                    &mut socket,
                    json!({
                        "channel": "rpc",
                        "payload": {
                            "type": "response",
                            "id": id,
                            "command": command,
                            "success": true,
                            "data": { "disposition": "started" }
                        }
                    }),
                )
                .await?;
                if command == "prompt" {
                    eprintln!("mock sending delta for {id}");
                    send_frame(
                        &mut socket,
                        json!({
                            "channel": "rpc",
                            "payload": {
                                "type": "message_update",
                                "message": { "role": "assistant", "timestamp": assistant_timestamp },
                                "assistantMessageEvent": {
                                    "type": "text_delta",
                                    "delta": "Hello from the mock central agent."
                                }
                            }
                        }),
                    )
                    .await?;
                    send_frame(
                        &mut socket,
                        json!({
                            "channel": "rpc",
                            "payload": { "type": "agent_end" }
                        }),
                    )
                    .await?;
                }
            }
            Some("resync") => {
                send_frame(
                    &mut socket,
                    json!({
                        "channel": "server",
                        "event": "status",
                        "data": {
                            "sessionId": "parent-session",
                            "sessionName": "Mock session",
                            "cwd": "/mock/workspace",
                            "model": { "provider": "mock", "id": "mock-1", "name": "Mock Model" },
                            "thinkingLevel": "medium",
                            "isStreaming": false,
                            "isCompacting": false,
                            "messageCount": messages.len(),
                            "lastError": "",
                            "hasPendingUiRequest": true,
                            "lastActivityAt": 1729000000000i64,
                            "controlOwner": "server"
                        }
                    }),
                )
                .await?;
                send_frame(
                    &mut socket,
                    json!({
                        "channel": "snapshot",
                        "state": {
                            "model": { "provider": "mock", "id": "mock-1", "name": "Mock Model" },
                            "thinkingLevel": "medium",
                            "isStreaming": false,
                            "isCompacting": false,
                            "messageCount": messages.len()
                        },
                        "messages": messages.clone(),
                        "commands": [],
                        "liveAssistantMessage": null,
                        "liveTools": [],
                        "historyTruncated": false,
                        "totalMessageCount": messages.len()
                    }),
                )
                .await?;
            }
            Some("ping") => {
                socket
                    .send(Message::Text(json!({"type":"pong"}).to_string().into()))
                    .await?;
            }
            _ => {}
        }
    }
    Ok(())
}

async fn send_frame(
    socket: &mut tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
    payload: Value,
) -> anyhow::Result<()> {
    socket
        .send(Message::Text(
            json!({
                "type": "frame",
                "sessionId": "parent-session",
                "payload": payload
            })
            .to_string()
            .into(),
        ))
        .await?;
    Ok(())
}
