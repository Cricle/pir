//! Repeatable pi-phone server benchmarks.
//!
//! ```text
//! cargo run --release -p pir-phone-server --example bench
//! cargo run --release -p pir-phone-server --example bench -- --iterations 10
//! ```
//!
//! The harness uses fixed counts and deterministic IDs. It prints a Markdown
//! table and a JSON line suitable for CI artifacts.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use futures_util::{SinkExt, StreamExt};
use pir_phone_server::config::Config;
use pir_phone_server::http;
use pir_phone_server::protocol::SessionDescriptor;
use pir_phone_server::state::AppState;
use pir_phone_server::store::Store;
use serde_json::{json, Value};
use tokio::net::TcpStream;
use tokio::task::JoinHandle;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::Message;

type Ws = tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<TcpStream>>;

struct Env {
    addr: std::net::SocketAddr,
    user_token: String,
    device_token: String,
    dir: PathBuf,
    server: JoinHandle<()>,
}

impl Env {
    async fn start() -> anyhow::Result<Self> {
        let dir = std::env::temp_dir().join(format!("pir-phone-bench-{}", uuid::Uuid::new_v4()));
        let store = Store::open(&dir)?;
        let user = store.create_user("bench", None)?;
        let (user_token, _) = store.create_token(&user.id, "user", Some("bench"), None)?;
        let (device_token, _) = store.create_token(&user.id, "device", Some("bench"), None)?;
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

    fn close(self) {
        self.server.abort();
        let _ = std::fs::remove_dir_all(self.dir);
    }
}

async fn next_json(ws: &mut Ws) -> anyhow::Result<Value> {
    loop {
        let message = tokio::time::timeout(Duration::from_secs(10), ws.next())
            .await
            .map_err(|_| anyhow::anyhow!("websocket timeout"))?;
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
    anyhow::bail!("frame not found")
}

async fn agent_connect(env: &Env, device_id: &str) -> anyhow::Result<Ws> {
    let mut request = format!("ws://{}/agent", env.addr).into_client_request()?;
    request.headers_mut().insert(
        "Authorization",
        HeaderValue::from_str(&format!("Bearer {}", env.device_token))?,
    );
    let (mut agent, _) = tokio_tungstenite::connect_async(request).await?;
    agent
        .send(Message::Text(
            json!({
                "type": "hello",
                "version": 1,
                "deviceId": device_id,
                "deviceName": device_id,
                "platform": "bench",
                "sessions": [{
                    "id": "session-0",
                    "label": "Bench session",
                    "kind": "parent",
                    "cwd": "/bench",
                    "model": { "provider": "bench", "id": "m", "name": "Bench" },
                    "thinkingLevel": "low",
                    "isStreaming": false,
                    "messageCount": 0
                }]
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

async fn client_connect(env: &Env) -> anyhow::Result<Ws> {
    let mut request = format!("ws://{}/ws", env.addr).into_client_request()?;
    request.headers_mut().insert(
        "Authorization",
        HeaderValue::from_str(&format!("Bearer {}", env.user_token))?,
    );
    let (mut client, _) = tokio_tungstenite::connect_async(request).await?;
    recv_until(
        &mut client,
        |frame| frame["channel"] == "sessions" && frame["event"] == "catalog",
        20,
    )
    .await?;
    Ok(client)
}

async fn select(client: &mut Ws, session_id: &str) -> anyhow::Result<()> {
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

fn percentile(sorted: &[Duration], p: f64) -> Duration {
    if sorted.is_empty() {
        return Duration::ZERO;
    }
    let index = ((sorted.len() - 1) as f64 * p).round() as usize;
    sorted[index]
}

struct Stats {
    name: &'static str,
    samples: Vec<Duration>,
    note: String,
}

impl Stats {
    fn new(name: &'static str, note: impl Into<String>) -> Self {
        Self {
            name,
            samples: Vec::new(),
            note: note.into(),
        }
    }

    fn record(&mut self, elapsed: Duration) {
        self.samples.push(elapsed);
    }

    fn print(&self) {
        let mut sorted = self.samples.clone();
        sorted.sort();
        let p50 = percentile(&sorted, 0.50);
        let p95 = percentile(&sorted, 0.95);
        let min = sorted.first().copied().unwrap_or_default();
        let max = sorted.last().copied().unwrap_or_default();
        println!(
            "| {} | {} | {:.2} | {:.2} | {:.2} | {:.2} | {} |",
            self.name,
            self.samples.len(),
            p50.as_secs_f64() * 1000.0,
            p95.as_secs_f64() * 1000.0,
            min.as_secs_f64() * 1000.0,
            max.as_secs_f64() * 1000.0,
            self.note
        );
        let json = json!({
            "scenario": self.name,
            "iterations": self.samples.len(),
            "p50_ms": p50.as_secs_f64() * 1000.0,
            "p95_ms": p95.as_secs_f64() * 1000.0,
            "min_ms": min.as_secs_f64() * 1000.0,
            "max_ms": max.as_secs_f64() * 1000.0,
            "note": self.note,
        });
        println!("BENCH_JSON {json}");
    }
}

async fn bench_store_catalog(iterations: usize) -> anyhow::Result<Stats> {
    let dir = std::env::temp_dir().join(format!("pir-phone-bench-cat-{}", uuid::Uuid::new_v4()));
    let store = Store::open(&dir)?;
    let user = store.create_user("bench", None)?;
    let device = store.create_device(&user.id, "bench", None)?;
    let descriptors: Vec<SessionDescriptor> = (0..10_000)
        .map(|index| SessionDescriptor {
            id: format!("session-{index}"),
            label: Some(format!("Session {index}")),
            kind: Some("parent".into()),
            cwd: Some(format!("/bench/{index}")),
            model: None,
            thinking_level: None,
            is_streaming: None,
            is_compacting: None,
            message_count: Some(index),
            last_error: None,
        })
        .collect();
    store.upsert_sessions_batch(&user.id, &device.id, &descriptors, true)?;
    let mut stats = Stats::new("store_catalog_10k", "10,000 sessions; first 50-row page");
    for _ in 0..iterations {
        let started = Instant::now();
        let page = store.list_sessions(&user.id, 0, 50, None, None)?;
        assert_eq!(page.len(), 50);
        stats.record(started.elapsed());
    }
    let _ = std::fs::remove_dir_all(dir);
    Ok(stats)
}

async fn bench_fanout(iterations: usize, clients_count: usize) -> anyhow::Result<Stats> {
    let mut stats = Stats::new(
        "fanout_50_clients",
        format!("{clients_count} phones, 100 live events each"),
    );
    for _ in 0..iterations {
        let env = Env::start().await?;
        let mut agent = agent_connect(&env, "bench-fanout").await?;
        let session_id = "bench-fanout:session-0";
        let mut clients = Vec::new();
        for _ in 0..clients_count {
            let mut client = client_connect(&env).await?;
            select(&mut client, session_id).await?;
            clients.push(client);
        }
        let mut readers = Vec::new();
        for mut client in clients {
            readers.push(tokio::spawn(async move {
                let mut count = 0usize;
                while count < 100 {
                    let Ok(frame) = next_json(&mut client).await else {
                        break;
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
        assert_eq!(total, clients_count * 100);
        stats.record(started.elapsed());
        env.close();
    }
    Ok(stats)
}

async fn bench_large_snapshot(iterations: usize) -> anyhow::Result<Stats> {
    let mut stats = Stats::new("large_snapshot_400k", "20 messages x 20 KiB");
    for _ in 0..iterations {
        let env = Env::start().await?;
        let mut agent = agent_connect(&env, "bench-large").await?;
        let mut client = client_connect(&env).await?;
        select(&mut client, "bench-large:session-0").await?;
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
                "model": { "provider": "bench", "id": "m", "name": "Bench" },
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
        let started = Instant::now();
        agent.send(agent_frame("session-0", snapshot)).await?;
        let received = recv_until(&mut client, |frame| frame["channel"] == "snapshot", 20).await?;
        assert_eq!(received["messages"].as_array().unwrap().len(), 20);
        stats.record(started.elapsed());
        env.close();
    }
    Ok(stats)
}

async fn bench_reconnect(iterations: usize) -> anyhow::Result<Stats> {
    let mut stats = Stats::new(
        "client_reconnect_resync",
        "reconnect + select + agent resync snapshot",
    );
    for _ in 0..iterations {
        let env = Env::start().await?;
        let mut agent = agent_connect(&env, "bench-reconnect").await?;
        let mut client = client_connect(&env).await?;
        select(&mut client, "bench-reconnect:session-0").await?;
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
            "state": { "isStreaming": false, "messageCount": 20 },
            "messages": messages,
            "commands": [],
            "historyTruncated": false,
            "totalMessageCount": 20
        });
        let snapshot_for_resync = snapshot.clone();
        agent.send(agent_frame("session-0", snapshot)).await?;
        recv_until(&mut client, |frame| frame["channel"] == "snapshot", 20).await?;
        client.close(None).await?;
        drop(client);

        let started = Instant::now();
        let mut client = client_connect(&env).await?;
        select(&mut client, "bench-reconnect:session-0").await?;
        recv_until(&mut agent, |frame| frame["type"] == "resync", 20).await?;
        agent
            .send(agent_frame("session-0", snapshot_for_resync))
            .await?;
        let fresh = recv_until(&mut client, |frame| frame["channel"] == "snapshot", 20).await?;
        assert_eq!(fresh["messages"].as_array().unwrap().len(), 20);
        stats.record(started.elapsed());
        env.close();
    }
    Ok(stats)
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut iterations = 5usize;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        if arg == "--iterations" {
            iterations = args
                .next()
                .and_then(|value| value.parse().ok())
                .unwrap_or(5);
        }
    }
    println!("# pi-phone server benchmark");
    println!();
    println!("| scenario | iterations | p50 ms | p95 ms | min ms | max ms | note |");
    println!("|---|---:|---:|---:|---:|---:|---|");

    let mut stats = vec![bench_store_catalog(iterations.max(5)).await?];
    stats.push(bench_fanout(iterations, 50).await?);
    stats.push(bench_large_snapshot(iterations).await?);
    stats.push(bench_reconnect(iterations).await?);
    for stat in &stats {
        stat.print();
    }
    Ok(())
}
