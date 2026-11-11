use std::path::PathBuf;
use std::time::{Duration, Instant};

use futures_util::{SinkExt, StreamExt};
use pir_phone_server::config::Config;
use pir_phone_server::http;
use pir_phone_server::state::AppState;
use pir_phone_server::store::Store;
use serde_json::{json, Value};
use tokio::net::TcpStream;
use tokio::task::JoinHandle;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::Message;

type Ws = tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<TcpStream>>;

struct TestEnv {
    addr: std::net::SocketAddr,
    user_token: String,
    device_token: String,
    dir: PathBuf,
    server: JoinHandle<()>,
}

impl TestEnv {
    async fn start() -> anyhow::Result<Self> {
        let dir = std::env::temp_dir().join(format!("pir-phone-stress-{}", uuid::Uuid::new_v4()));
        let store = Store::open(&dir)?;
        let user = store.create_user("stress", None)?;
        let (user_token, _) = store.create_token(&user.id, "user", Some("stress"), None)?;
        let (device_token, _) = store.create_token(&user.id, "device", Some("stress"), None)?;
        let config = Config::load(dir.clone(), "127.0.0.1:0".parse()?, None, Vec::new())?;
        let state = AppState::new(config, store);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let addr = listener.local_addr()?;
        let server = tokio::spawn(async move {
            let _ = http::serve(listener, state).await;
        });
        Ok(Self {
            addr,
            user_token,
            device_token,
            dir,
            server,
        })
    }

    async fn close(self) {
        self.server.abort();
        let _ = std::fs::remove_dir_all(self.dir);
    }
}

async fn next_json(ws: &mut Ws) -> anyhow::Result<Value> {
    loop {
        let message = tokio::time::timeout(Duration::from_secs(10), ws.next())
            .await
            .map_err(|_| anyhow::anyhow!("websocket frame timeout"))?;
        match message {
            Some(Ok(Message::Text(text))) => return Ok(serde_json::from_str(&text)?),
            Some(Ok(Message::Ping(_))) | Some(Ok(Message::Pong(_))) => {}
            Some(Ok(Message::Close(frame))) => anyhow::bail!("closed: {frame:?}"),
            Some(Err(error)) => return Err(error.into()),
            None => anyhow::bail!("websocket ended"),
            _ => {}
        }
    }
}

async fn recv_until(
    ws: &mut Ws,
    predicate: impl Fn(&Value) -> bool,
    limit: usize,
) -> anyhow::Result<Value> {
    for _ in 0..limit {
        let value = next_json(ws).await?;
        if predicate(&value) {
            return Ok(value);
        }
    }
    anyhow::bail!("expected frame not found")
}

async fn agent_connect(env: &TestEnv, device_id: &str, session_count: usize) -> anyhow::Result<Ws> {
    let mut request = format!("ws://{}/agent", env.addr).into_client_request()?;
    request.headers_mut().insert(
        "Authorization",
        HeaderValue::from_str(&format!("Bearer {}", env.device_token))?,
    );
    let (mut agent, _) = tokio_tungstenite::connect_async(request).await?;
    let sessions: Vec<Value> = (0..session_count)
        .map(|index| {
            json!({
                "id": format!("session-{index}"),
                "label": format!("Session {index}"),
                "kind": "parent",
                "cwd": "/stress",
                "model": { "provider": "stress", "id": "m", "name": "Stress" },
                "thinkingLevel": "low",
                "isStreaming": false,
                "messageCount": 0
            })
        })
        .collect();
    agent
        .send(Message::Text(
            json!({
                "type": "hello",
                "version": 1,
                "deviceId": device_id,
                "deviceName": device_id,
                "platform": "stress",
                "sessions": sessions,
            })
            .to_string()
            .into(),
        ))
        .await?;
    let hello = next_json(&mut agent).await?;
    if hello["type"] != "hello-ok" {
        anyhow::bail!("agent hello failed: {hello}");
    }
    Ok(agent)
}

async fn client_connect(env: &TestEnv) -> anyhow::Result<(Ws, Value)> {
    let mut request = format!("ws://{}/ws", env.addr).into_client_request()?;
    request.headers_mut().insert(
        "Authorization",
        HeaderValue::from_str(&format!("Bearer {}", env.user_token))?,
    );
    let (mut client, _) = tokio_tungstenite::connect_async(request).await?;
    let catalog = recv_until(
        &mut client,
        |frame| frame["channel"] == "sessions" && frame["event"] == "catalog",
        20,
    )
    .await?;
    Ok((client, catalog))
}

async fn select_session(client: &mut Ws, session_id: &str) -> anyhow::Result<()> {
    client
        .send(Message::Text(
            json!({ "kind": "session-select", "sessionId": session_id })
                .to_string()
                .into(),
        ))
        .await?;
    recv_until(
        client,
        |frame| frame["channel"] == "sessions" && frame["event"] == "selected",
        20,
    )
    .await?;
    Ok(())
}

fn agent_frame(session_id: &str, payload: Value) -> Message {
    Message::Text(
        json!({
            "type": "frame",
            "sessionId": session_id,
            "payload": payload,
        })
        .to_string()
        .into(),
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn fanout_to_many_clients() -> anyhow::Result<()> {
    let env = TestEnv::start().await?;
    let mut agent = agent_connect(&env, "device-fanout", 1).await?;
    let session_id = "device-fanout:session-0";

    let mut clients = Vec::new();
    for _ in 0..50 {
        let (mut client, _) = client_connect(&env).await?;
        select_session(&mut client, session_id).await?;
        clients.push(client);
    }

    let mut readers = Vec::new();
    for mut client in clients {
        readers.push(tokio::spawn(async move {
            let mut count = 0usize;
            while count < 100 {
                let frame = match next_json(&mut client).await {
                    Ok(frame) => frame,
                    Err(_) => break,
                };
                if frame["channel"] == "rpc" && frame["payload"]["type"] == "message_update" {
                    count += 1;
                }
            }
            count
        }));
    }

    let started = Instant::now();
    for index in 0..100 {
        agent
            .send(agent_frame(
                "session-0",
                json!({
                    "channel": "rpc",
                    "payload": {
                        "type": "message_update",
                        "message": { "role": "assistant", "timestamp": index },
                        "assistantMessageEvent": { "type": "text_delta", "delta": "x" }
                    }
                }),
            ))
            .await?;
    }

    let mut total = 0usize;
    for reader in readers {
        total += reader.await?;
    }
    let elapsed = started.elapsed();
    assert_eq!(total, 100 * 50, "fanout to 50 clients");
    assert!(elapsed < Duration::from_secs(10), "fanout took {elapsed:?}");
    env.close().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn large_snapshot_and_reconnect_resync() -> anyhow::Result<()> {
    let env = TestEnv::start().await?;
    let mut agent = agent_connect(&env, "device-large", 1).await?;
    let session_id = "device-large:session-0";
    let (mut client, _) = client_connect(&env).await?;
    select_session(&mut client, session_id).await?;

    // 20 x 20 KiB text: large enough to exercise a multi-hundred-KiB snapshot,
    // small enough to stay under the per-snapshot cache cap.
    let messages: Vec<Value> = (0..20)
        .map(|index| {
            json!({
                "role": "user",
                "timestamp": index,
                "content": [{ "type": "text", "text": "x".repeat(20 * 1024) }]
            })
        })
        .collect();
    let snapshot = json!({
        "channel": "snapshot",
        "state": {
            "model": { "provider": "stress", "id": "m", "name": "Stress" },
            "thinkingLevel": "low",
            "isStreaming": false,
            "isCompacting": false,
            "messageCount": messages.len()
        },
        "messages": messages,
        "commands": [],
        "liveAssistantMessage": null,
        "liveTools": [],
        "historyTruncated": false,
        "totalMessageCount": 20
    });
    let snapshot_for_resync = snapshot.clone();
    let started = Instant::now();
    agent.send(agent_frame("session-0", snapshot)).await?;
    let received = recv_until(&mut client, |frame| frame["channel"] == "snapshot", 20).await?;
    assert_eq!(received["messages"].as_array().unwrap().len(), 20);
    assert!(started.elapsed() < Duration::from_secs(5));

    // Drop the client and reconnect: the server must not retain transcript
    // content, so it asks the agent to resync and relays a fresh snapshot.
    client.close(None).await?;
    drop(client);
    let (mut client, _) = client_connect(&env).await?;
    select_session(&mut client, session_id).await?;
    recv_until(&mut agent, |frame| frame["type"] == "resync", 20).await?;
    agent
        .send(agent_frame("session-0", snapshot_for_resync))
        .await?;
    let fresh = recv_until(&mut client, |frame| frame["channel"] == "snapshot", 20).await?;
    assert_eq!(fresh["messages"].as_array().unwrap().len(), 20);

    drop(agent);
    env.close().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn multi_megabyte_snapshot_is_delivered() -> anyhow::Result<()> {
    let env = TestEnv::start().await?;
    let mut agent = agent_connect(&env, "device-huge", 1).await?;
    let session_id = "device-huge:session-0";
    let (mut client, _) = client_connect(&env).await?;
    select_session(&mut client, session_id).await?;

    // ~3 MiB snapshot: under the 8 MiB frame cap; the relay forwards it
    // without retaining any transcript content.
    let messages: Vec<Value> = (0..30)
        .map(|index| {
            json!({
                "role": "user",
                "timestamp": index,
                "content": [{ "type": "text", "text": "y".repeat(100 * 1024) }]
            })
        })
        .collect();
    let started = Instant::now();
    agent
        .send(agent_frame(
            "session-0",
            json!({
                "channel": "snapshot",
                "state": { "isStreaming": false, "messageCount": messages.len() },
                "messages": messages,
                "commands": [],
                "liveAssistantMessage": null,
                "liveTools": [],
                "historyTruncated": false,
                "totalMessageCount": 30
            }),
        ))
        .await?;
    let received = recv_until(&mut client, |frame| frame["channel"] == "snapshot", 20).await?;
    assert_eq!(received["messages"].as_array().unwrap().len(), 30);
    assert!(started.elapsed() < Duration::from_secs(10));
    env.close().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn agent_disconnect_and_reconnect() -> anyhow::Result<()> {
    let env = TestEnv::start().await?;
    let mut agent = agent_connect(&env, "device-reconnect", 1).await?;
    let session_id = "device-reconnect:session-0";
    let (mut client, _) = client_connect(&env).await?;
    select_session(&mut client, session_id).await?;

    agent.close(None).await?;
    drop(agent);
    let offline = recv_until(
        &mut client,
        |frame| frame["channel"] == "server" && frame["event"] == "device-offline",
        40,
    )
    .await?;
    assert_eq!(offline["data"]["deviceId"], "device-reconnect");

    let _agent = agent_connect(&env, "device-reconnect", 1).await?;
    // An offline catalog from the disconnect cleanup may already be queued;
    // wait for the catalog that reflects the reconnected agent.
    let catalog = recv_until(
        &mut client,
        |frame| {
            frame["channel"] == "sessions"
                && frame["event"] == "catalog"
                && frame["data"]["sessions"]
                    .as_array()
                    .is_some_and(|sessions| {
                        sessions
                            .iter()
                            .any(|session| session["id"] == session_id && session["online"] == true)
                    })
        },
        40,
    )
    .await?;
    let session = catalog["data"]["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|session| session["id"] == session_id)
        .expect("reconnected session");
    assert_eq!(session["online"], true);

    env.close().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn slow_client_does_not_block_fast_client() -> anyhow::Result<()> {
    let env = TestEnv::start().await?;
    let mut agent = agent_connect(&env, "device-slow", 1).await?;
    let session_id = "device-slow:session-0";

    let (mut fast, _) = client_connect(&env).await?;
    select_session(&mut fast, session_id).await?;
    let (slow, _) = client_connect(&env).await?;
    // Do not read from `slow`; its queue fills and the server must drop it
    // without affecting the fast subscriber.
    let _ = slow;

    let reader = tokio::spawn(async move {
        let mut count = 0usize;
        while count < 600 {
            let frame = match next_json(&mut fast).await {
                Ok(frame) => frame,
                Err(_) => break,
            };
            if frame["channel"] == "rpc" && frame["payload"]["type"] == "message_update" {
                count += 1;
            }
        }
        count
    });

    for index in 0..600 {
        agent
            .send(agent_frame(
                "session-0",
                json!({
                    "channel": "rpc",
                    "payload": {
                        "type": "message_update",
                        "message": { "role": "assistant", "timestamp": index },
                        "assistantMessageEvent": { "type": "text_delta", "delta": "x".repeat(2048) }
                    }
                }),
            ))
            .await?;
    }
    let count = reader.await?;
    assert_eq!(count, 600, "fast client must keep receiving");
    env.close().await;
    Ok(())
}
