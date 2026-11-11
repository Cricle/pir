//! End-to-end load/resource test for pir-phone-server.
//!
//! ```text
//! cargo run --release -p pir-phone-server --example load_test
//! cargo run --release -p pir-phone-server --example load_test -- --clients 100 --events 500
//! ```
//!
//! Reports catalog latency at 10k sessions, multi-client fan-out throughput,
//! reconnect latency, RSS (current/peak), cache bytes and process CPU time.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use futures_util::{SinkExt, StreamExt};
use pir_phone_server::config::Config;
use pir_phone_server::http;
use pir_phone_server::metrics::{memory_snapshot, MemorySnapshot};
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

struct Args {
    sessions: usize,
    clients: usize,
    events: usize,
    iterations: usize,
}

impl Args {
    fn parse() -> Self {
        let mut args = Self {
            sessions: 10_000,
            clients: 100,
            events: 500,
            iterations: 20,
        };
        let mut iter = std::env::args().skip(1);
        while let Some(arg) = iter.next() {
            let mut value = || iter.next().and_then(|value| value.parse::<usize>().ok());
            match arg.as_str() {
                "--sessions" => args.sessions = value().unwrap_or(args.sessions).max(1),
                "--clients" => args.clients = value().unwrap_or(args.clients).clamp(1, 500),
                "--events" => args.events = value().unwrap_or(args.events).max(1),
                "--iterations" => args.iterations = value().unwrap_or(args.iterations).max(1),
                other => eprintln!("ignoring unknown argument: {other}"),
            }
        }
        args
    }
}

struct Env {
    addr: std::net::SocketAddr,
    user_token: String,
    device_token: String,
    device_id: String,
    state: AppState,
    dir: PathBuf,
    server: JoinHandle<()>,
}

impl Env {
    async fn start(sessions: usize) -> anyhow::Result<Self> {
        let dir = std::env::temp_dir().join(format!("pir-phone-load-{}", uuid::Uuid::new_v4()));
        let store = Store::open(&dir)?;
        let user = store.create_user("load", None)?;
        let (user_token, _) = store.create_token(&user.id, "user", Some("load"), None)?;
        let device = store.create_device(&user.id, "load-device", Some("load"))?;
        let (device_token, _) =
            store.create_token(&user.id, "device", Some("load"), Some(&device.id))?;

        if sessions > 1 {
            // Chunk the seed so a 1M-session catalog never materializes a
            // million descriptors at once.
            let mut start = 1usize;
            while start < sessions {
                let end = (start + 10_000).min(sessions);
                let descriptors: Vec<SessionDescriptor> = (start..end)
                    .map(|index| SessionDescriptor {
                        id: format!("session-{index}"),
                        label: Some(format!("Load session {index}")),
                        kind: Some("parent".into()),
                        cwd: Some(format!("/load/{index}")),
                        model: Some(pir_phone_server::protocol::ModelDescriptor {
                            provider: "load".into(),
                            id: "load-model".into(),
                            name: Some("Load Model".into()),
                        }),
                        thinking_level: Some("low".into()),
                        is_streaming: Some(false),
                        is_compacting: Some(false),
                        message_count: Some(index as u64),
                        last_error: None,
                    })
                    .collect();
                store.upsert_sessions_batch(&user.id, &device.id, &descriptors, false)?;
                start = end;
            }
        }

        let config = Config::load(dir.clone(), "127.0.0.1:0".parse()?, None, Vec::new())?;
        let state = AppState::new(config, store);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let addr = listener.local_addr()?;
        let server_state = state.clone();
        let server = tokio::spawn(async move {
            let _ = http::serve(listener, server_state).await;
        });
        Ok(Self {
            addr,
            user_token,
            device_token,
            device_id: device.id,
            state,
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
        let message = tokio::time::timeout(Duration::from_secs(15), ws.next())
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

async fn agent_connect(env: &Env) -> anyhow::Result<Ws> {
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
                "deviceId": env.device_id,
                "deviceName": "load-device",
                "platform": "load",
                "sessions": [{
                    "id": "session-0",
                    "label": "Load session 0",
                    "kind": "parent",
                    "cwd": "/load/0",
                    "model": { "provider": "load", "id": "load-model", "name": "Load Model" },
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

fn agent_frame(index: usize) -> Message {
    Message::Text(
        json!({
            "type": "frame",
            "sessionId": "session-0",
            "payload": {
                "channel": "rpc",
                "payload": {
                    "type": "message_update",
                    "message": { "role": "assistant", "timestamp": index },
                    "assistantMessageEvent": { "type": "text_delta", "delta": "x" }
                }
            }
        })
        .to_string()
        .into(),
    )
}

fn percentile(samples: &mut [Duration], p: f64) -> Duration {
    if samples.is_empty() {
        return Duration::ZERO;
    }
    samples.sort();
    let index = ((samples.len() - 1) as f64 * p).round() as usize;
    samples[index]
}

fn ms(value: Duration) -> f64 {
    value.as_secs_f64() * 1000.0
}

fn round_mb(bytes: u64) -> f64 {
    (bytes as f64 / (1024.0 * 1024.0) * 10.0).round() / 10.0
}

#[cfg(target_os = "linux")]
fn cpu_millis() -> u64 {
    let Ok(stat) = std::fs::read_to_string("/proc/self/stat") else {
        return 0;
    };
    let Some(rest) = stat.rsplit(')').next() else {
        return 0;
    };
    let fields: Vec<&str> = rest.split_whitespace().collect();
    let ticks = fields
        .get(11)
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(0)
        .saturating_add(
            fields
                .get(12)
                .and_then(|value| value.parse::<u64>().ok())
                .unwrap_or(0),
        );
    ticks.saturating_mul(10)
}

#[cfg(not(target_os = "linux"))]
fn cpu_millis() -> u64 {
    0
}

async fn catalog_latency(env: &Env, iterations: usize) -> anyhow::Result<Vec<Duration>> {
    let mut client = client_connect(env).await?;
    // Warm the B-tree pages before measuring steady-state page latency.
    for _ in 0..3 {
        client
            .send(Message::Text(
                json!({ "kind": "sessions/list", "limit": 50 })
                    .to_string()
                    .into(),
            ))
            .await?;
        recv_until(
            &mut client,
            |frame| frame["channel"] == "sessions" && frame["event"] == "catalog",
            20,
        )
        .await?;
    }
    let mut samples = Vec::with_capacity(iterations);
    for _ in 0..iterations {
        let started = Instant::now();
        client
            .send(Message::Text(
                json!({ "kind": "sessions/list", "limit": 50 })
                    .to_string()
                    .into(),
            ))
            .await?;
        let frame = recv_until(
            &mut client,
            |frame| frame["channel"] == "sessions" && frame["event"] == "catalog",
            20,
        )
        .await?;
        assert_eq!(frame["data"]["sessions"].as_array().map(Vec::len), Some(50));
        samples.push(started.elapsed());
    }
    Ok(samples)
}

async fn fanout(
    env: &Env,
    clients_count: usize,
    events: usize,
) -> anyhow::Result<(Vec<Duration>, Duration, u128)> {
    let mut agent = agent_connect(env).await?;
    let session_id = format!("{}:session-0", env.device_id);
    let session_id = session_id.as_str();
    let mut clients = Vec::with_capacity(clients_count);
    for _ in 0..clients_count {
        let mut client = client_connect(env).await?;
        select(&mut client, session_id).await?;
        clients.push(client);
    }

    let mut readers = Vec::with_capacity(clients_count);
    for mut client in clients {
        readers.push(tokio::spawn(async move {
            let mut count = 0usize;
            while count < events {
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
    for index in 0..events {
        agent.send(agent_frame(index)).await?;
    }
    let mut total = 0usize;
    for reader in readers {
        total += reader.await?;
    }
    let elapsed = started.elapsed();
    assert_eq!(total, clients_count * events);

    // Reconnect latency with a cached snapshot.
    let snapshot = json!({
        "channel": "snapshot",
        "state": { "isStreaming": false, "messageCount": 20 },
        "messages": (0..20).map(|index| json!({
            "role": "user",
            "timestamp": index,
            "content": [{ "type": "text", "text": "x".repeat(20 * 1024) }]
        })).collect::<Vec<_>>(),
        "commands": [],
        "historyTruncated": false,
        "totalMessageCount": 20
    });
    let snapshot_for_resync = snapshot.clone();
    agent
        .send(Message::Text(
            json!({ "type": "frame", "sessionId": "session-0", "payload": snapshot })
                .to_string()
                .into(),
        ))
        .await?;
    let mut first = client_connect(env).await?;
    select(&mut first, session_id).await?;
    recv_until(&mut agent, |frame| frame["type"] == "resync", 20).await?;
    agent
        .send(Message::Text(
            json!({ "type": "frame", "sessionId": "session-0", "payload": snapshot_for_resync })
                .to_string()
                .into(),
        ))
        .await?;
    recv_until(&mut first, |frame| frame["channel"] == "snapshot", 20).await?;
    first.close(None).await?;

    let mut reconnect_samples = Vec::new();
    for _ in 0..5 {
        let started = Instant::now();
        let mut client = client_connect(env).await?;
        select(&mut client, session_id).await?;
        recv_until(&mut agent, |frame| frame["type"] == "resync", 20).await?;
        agent
            .send(Message::Text(
                json!({
                    "type": "frame",
                    "sessionId": "session-0",
                    "payload": snapshot_for_resync.clone()
                })
                .to_string()
                .into(),
            ))
            .await?;
        recv_until(&mut client, |frame| frame["channel"] == "snapshot", 20).await?;
        reconnect_samples.push(started.elapsed());
        client.close(None).await?;
    }

    Ok((reconnect_samples, elapsed, (clients_count * events) as u128))
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let baseline: MemorySnapshot = memory_snapshot();
    let cpu_start = cpu_millis();

    let env = Env::start(args.sessions).await?;
    let after_seed = memory_snapshot();
    println!("# pi-phone server load test");
    println!();
    println!(
        "sessions={} clients={} events={} iterations={}",
        args.sessions, args.clients, args.events, args.iterations
    );

    let mut catalog = catalog_latency(&env, args.iterations).await?;
    let catalog_p50 = percentile(&mut catalog, 0.50);
    let catalog_p95 = percentile(&mut catalog, 0.95);
    let catalog_p99 = percentile(&mut catalog, 0.99);
    let catalog_min = catalog.first().copied().unwrap_or_default();
    let catalog_max = catalog.last().copied().unwrap_or_default();

    let (mut reconnect, fanout_elapsed, messages) = fanout(&env, args.clients, args.events).await?;
    let reconnect_p50 = percentile(&mut reconnect, 0.50);
    let reconnect_p95 = percentile(&mut reconnect, 0.95);
    let reconnect_p99 = percentile(&mut reconnect, 0.99);
    let fanout_rps = messages as f64 / fanout_elapsed.as_secs_f64();
    let after_fanout = memory_snapshot();
    let counters = env.state.metrics.snapshot();
    let registry = env.state.registry.read().await;
    let cache_bytes = registry.cache_bytes();
    let cached_sessions = registry.cached_sessions();
    let subscribers = registry.subscriber_count();
    drop(registry);
    let cpu_total = cpu_millis().saturating_sub(cpu_start);

    println!();
    println!("| metric | value |");
    println!("|---|---:|");
    println!("| sessions seeded | {} |", args.sessions);
    println!(
        "| catalog {} sessions p50 / p95 / p99 | {:.2} / {:.2} / {:.2} ms |",
        args.sessions,
        ms(catalog_p50),
        ms(catalog_p95),
        ms(catalog_p99)
    );
    println!(
        "| catalog min / max | {:.2} / {:.2} ms |",
        ms(catalog_min),
        ms(catalog_max)
    );
    println!(
        "| fan-out | {} clients × {} events = {} msgs in {:.2} ms |",
        args.clients,
        args.events,
        messages,
        ms(fanout_elapsed)
    );
    println!("| fan-out throughput | {:.0} msgs/s |", fanout_rps);
    println!(
        "| reconnect resync p50 / p95 / p99 | {:.2} / {:.2} / {:.2} ms |",
        ms(reconnect_p50),
        ms(reconnect_p95),
        ms(reconnect_p99)
    );
    println!(
        "| RSS start / seeded / fan-out | {:.1} / {:.1} / {:.1} MiB |",
        round_mb(baseline.rss_bytes),
        round_mb(after_seed.rss_bytes),
        round_mb(after_fanout.rss_bytes)
    );
    println!(
        "| RSS peak | {:.1} MiB |",
        round_mb(after_fanout.peak_rss_bytes)
    );
    println!(
        "| cache bytes / sessions | {} / {} |",
        cache_bytes, cached_sessions
    );
    println!("| subscribers at end | {} |", subscribers);
    println!("| CPU total | {} ms |", cpu_total);
    println!(
        "| counters | agent={} event={} broadcast={} deliveries={} catalog={} commands={} ws={} |",
        counters.agent_frames,
        counters.event_frames,
        counters.broadcast_frames,
        counters.broadcast_deliveries,
        counters.catalog_broadcasts,
        counters.commands,
        counters.ws_connections
    );

    let json = json!({
        "sessions": args.sessions,
        "clients": args.clients,
        "events": args.events,
        "iterations": args.iterations,
        "catalog_ms": {
            "p50": ms(catalog_p50), "p95": ms(catalog_p95), "p99": ms(catalog_p99),
            "min": ms(catalog_min), "max": ms(catalog_max)
        },
        "fanout": {
            "messages": messages,
            "elapsed_ms": ms(fanout_elapsed),
            "throughput_msgs_s": fanout_rps
        },
        "reconnect_ms": {"p50": ms(reconnect_p50), "p95": ms(reconnect_p95), "p99": ms(reconnect_p99)},
        "rss_mib": {
            "start": round_mb(baseline.rss_bytes),
            "seeded": round_mb(after_seed.rss_bytes),
            "fanout": round_mb(after_fanout.rss_bytes),
            "peak": round_mb(after_fanout.peak_rss_bytes)
        },
        "cache_bytes": cache_bytes,
        "cached_sessions": cached_sessions,
        "subscribers": subscribers,
        "cpu_millis": cpu_total,
        "counters": counters
    });
    println!("LOAD_JSON {json}");
    env.close();
    Ok(())
}
