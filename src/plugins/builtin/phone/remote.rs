//! pi-phone agent client: outbound WebSocket link to the central phone server.
//!
//! The plugin no longer has to bind a local HTTP port. `phone-start` with a
//! `ws://`/`wss://` URL (or `--server URL`) creates the same `SessionBridge`
//! used by the local server lane, then keeps an outbound connection to the
//! central relay. The relay never executes the agent; it only routes phone
//! commands to this process and this process's events back to the phones.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::sync::watch;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::Message;

use super::session::SessionBridge;

const STATE_OFF: u8 = 0;
const STATE_CONNECTING: u8 = 1;
const STATE_CONNECTED: u8 = 2;
const STATE_RECONNECTING: u8 = 3;
const DEVICE_FILE: &str = "phone-device.json";

#[derive(Debug, Clone)]
pub struct RemoteConfig {
    pub server_url: String,
    pub token: String,
    pub device_id: String,
    pub device_name: String,
    pub reconnect: bool,
}

impl RemoteConfig {
    pub fn parse(args: &str) -> Result<Self, String> {
        let mut server_url = std::env::var("PI_PHONE_SERVER_URL").unwrap_or_default();
        let mut token = std::env::var("PI_PHONE_DEVICE_TOKEN")
            .or_else(|_| std::env::var("PI_PHONE_TOKEN"))
            .unwrap_or_default();
        let mut device_name =
            std::env::var("PI_PHONE_DEVICE_NAME").unwrap_or_else(|_| default_device_name());
        let mut device_id = std::env::var("PI_PHONE_DEVICE_ID").unwrap_or_default();
        let mut reconnect = true;

        let tokens: Vec<&str> = args.split_whitespace().collect();
        let mut index = 0;
        while index < tokens.len() {
            let token_arg = tokens[index];
            match token_arg {
                "--server" | "--url" if index + 1 < tokens.len() => {
                    server_url = tokens[index + 1].to_owned();
                    index += 2;
                }
                "--token" if index + 1 < tokens.len() => {
                    token = tokens[index + 1].to_owned();
                    index += 2;
                }
                "--device-name" | "--name" if index + 1 < tokens.len() => {
                    device_name = tokens[index + 1].to_owned();
                    index += 2;
                }
                "--device-id" if index + 1 < tokens.len() => {
                    device_id = tokens[index + 1].to_owned();
                    index += 2;
                }
                "--no-reconnect" => {
                    reconnect = false;
                    index += 1;
                }
                value if value.starts_with("--server=") => {
                    server_url = value["--server=".len()..].to_owned();
                    index += 1;
                }
                value if value.starts_with("--url=") => {
                    server_url = value["--url=".len()..].to_owned();
                    index += 1;
                }
                value if value.starts_with("--token=") => {
                    token = value["--token=".len()..].to_owned();
                    index += 1;
                }
                value if value.starts_with("--device-name=") => {
                    device_name = value["--device-name=".len()..].to_owned();
                    index += 1;
                }
                value if value.starts_with("--device-id=") => {
                    device_id = value["--device-id=".len()..].to_owned();
                    index += 1;
                }
                value
                    if !value.starts_with('-')
                        && (value.starts_with("ws://")
                            || value.starts_with("wss://")
                            || value.starts_with("http://")
                            || value.starts_with("https://")) =>
                {
                    server_url = value.to_owned();
                    index += 1;
                }
                value if !value.starts_with('-') && token.is_empty() => {
                    token = value.to_owned();
                    index += 1;
                }
                _ => {
                    index += 1;
                }
            }
        }

        if server_url.trim().is_empty() {
            return Err(
                "missing phone server URL. Usage: /phone-start <ws(s)://server> [device-token] \
                 [--device-name name]"
                    .to_owned(),
            );
        }
        let server_url = normalize_url(&server_url);
        if device_id.trim().is_empty() {
            device_id = load_or_create_device_id(&mut device_name)?;
        }
        if token.trim().is_empty() {
            return Err(
                "missing device token. Pass it after the URL, use --token, or set \
                 PI_PHONE_DEVICE_TOKEN."
                    .to_owned(),
            );
        }
        Ok(Self {
            server_url,
            token,
            device_id,
            device_name,
            reconnect,
        })
    }
}

#[derive(Clone)]
pub struct RemoteStatus {
    state: Arc<AtomicU8>,
    last_error: Arc<Mutex<String>>,
    connected_at: Arc<Mutex<Option<Instant>>>,
    server_url: Arc<String>,
    device_id: Arc<String>,
    device_name: Arc<String>,
}

impl RemoteStatus {
    fn new(config: &RemoteConfig) -> Self {
        Self {
            state: Arc::new(AtomicU8::new(STATE_OFF)),
            last_error: Arc::new(Mutex::new(String::new())),
            connected_at: Arc::new(Mutex::new(None)),
            server_url: Arc::new(config.server_url.clone()),
            device_id: Arc::new(config.device_id.clone()),
            device_name: Arc::new(config.device_name.clone()),
        }
    }

    fn set(&self, state: u8) {
        self.state.store(state, Ordering::SeqCst);
        if state == STATE_CONNECTED {
            *self
                .connected_at
                .lock()
                .unwrap_or_else(|poison| poison.into_inner()) = Some(Instant::now());
        }
    }

    fn set_error(&self, error: String) {
        *self
            .last_error
            .lock()
            .unwrap_or_else(|poison| poison.into_inner()) = error;
    }

    pub fn is_connected(&self) -> bool {
        self.state.load(Ordering::SeqCst) == STATE_CONNECTED
    }

    pub fn status_text(&self) -> String {
        let state = self.state.load(Ordering::SeqCst);
        let error = self
            .last_error
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .clone();
        match state {
            STATE_CONNECTED => {
                let elapsed = self
                    .connected_at
                    .lock()
                    .unwrap_or_else(|poison| poison.into_inner())
                    .map(|at| at.elapsed().as_secs())
                    .unwrap_or(0);
                format!(
                    "Pi Phone connected to {} as {} ({}) for {}s",
                    self.server_url, self.device_name, self.device_id, elapsed
                )
            }
            STATE_CONNECTING => format!("Pi Phone connecting to {}…", self.server_url),
            STATE_RECONNECTING => format!(
                "Pi Phone reconnecting to {}{}",
                self.server_url,
                if error.is_empty() {
                    String::new()
                } else {
                    format!(": {error}")
                }
            ),
            _ => "Pi Phone disconnected".to_owned(),
        }
    }
}

pub struct RemoteRuntime {
    pub status: RemoteStatus,
    pub token: String,
    pub server_url: String,
    pub device_id: String,
    pub device_name: String,
    shutdown: watch::Sender<bool>,
    task: tokio::task::JoinHandle<()>,
    bridge: SessionBridge,
}

impl RemoteRuntime {
    pub fn spawn(config: RemoteConfig, bridge: SessionBridge) -> Self {
        let status = RemoteStatus::new(&config);
        let (shutdown, shutdown_rx) = watch::channel(false);
        let task = tokio::spawn(remote_loop(
            config.clone(),
            bridge.clone(),
            status.clone(),
            shutdown_rx,
        ));
        Self {
            status,
            token: config.token,
            server_url: config.server_url,
            device_id: config.device_id,
            device_name: config.device_name,
            shutdown,
            task,
            bridge,
        }
    }

    pub async fn stop(self) {
        let _ = self.shutdown.send(true);
        let _ = tokio::time::timeout(Duration::from_secs(3), self.task).await;
        self.bridge.close();
    }
}

async fn remote_loop(
    config: RemoteConfig,
    bridge: SessionBridge,
    status: RemoteStatus,
    mut shutdown: watch::Receiver<bool>,
) {
    let mut attempt = 0u32;
    loop {
        if *shutdown.borrow() {
            break;
        }
        status.set(STATE_CONNECTING);
        match connect_once(&config, &bridge, &status, &mut shutdown).await {
            Ok(()) => {
                attempt = 0;
            }
            Err(error) => {
                status.set_error(error.to_string());
            }
        }
        if *shutdown.borrow() {
            break;
        }
        if !config.reconnect {
            break;
        }
        status.set(STATE_RECONNECTING);
        attempt = attempt.saturating_add(1);
        let secs = 1u64 << attempt.saturating_sub(1).min(5);
        let delay = Duration::from_secs(secs.min(30));
        tokio::select! {
            _ = tokio::time::sleep(delay) => {}
            _ = shutdown.changed() => break,
        }
    }
    status.set(STATE_OFF);
}

async fn connect_once(
    config: &RemoteConfig,
    bridge: &SessionBridge,
    status: &RemoteStatus,
    shutdown: &mut watch::Receiver<bool>,
) -> anyhow::Result<()> {
    let mut request = config.server_url.clone().into_client_request()?;
    request.headers_mut().insert(
        "Authorization",
        HeaderValue::from_str(&format!("Bearer {}", config.token))
            .map_err(|error| anyhow::anyhow!("invalid device token header: {error}"))?,
    );
    let (socket, _) = tokio_tungstenite::connect_async(request).await?;
    let (mut write, mut read) = socket.split();

    let hello = json!({
        "type": "hello",
        "version": 1,
        "deviceId": config.device_id,
        "deviceName": config.device_name,
        "platform": format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH),
        "capabilities": ["sessions", "tools", "extension-ui.v2", "session-pool", "custom-events.v1"],
        "customEvents": ["phone.custom.*"],
        "sessions": [{
            "id": "parent-session",
            "label": config.device_name,
            "kind": "parent",
            "cwd": std::env::current_dir().ok().map(|path| path.to_string_lossy().into_owned()),
            "hasPendingUiRequest": bridge.pending_ui_request("parent-session"),
            "lastActivityAt": std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|duration| duration.as_millis() as i64)
                .unwrap_or(0),
        }]
    });
    write.send(Message::Text(hello.to_string())).await?;

    let mut subscriber = bridge
        .subscribe()
        .map_err(|error| anyhow::anyhow!("bridge subscribe failed: {error:?}"))?;
    // Ask the host to publish status/catalog/snapshot for a fresh client.
    bridge
        .handle_client_line(subscriber.id(), "{\"kind\":\"refresh\"}")
        .await;
    let mut known_pool_sessions: HashSet<String> = HashSet::new();

    loop {
        tokio::select! {
            line = subscriber.recv() => {
                let Some(line) = line else {
                    return Err(anyhow::anyhow!("bridge subscriber closed"));
                };
                let payload: Value = serde_json::from_str(&line).unwrap_or(Value::Null);
                if payload.get("channel").and_then(Value::as_str) == Some("sessions")
                    && payload.get("event").and_then(Value::as_str) == Some("catalog")
                {
                    let rows = payload
                        .get("data")
                        .and_then(|data| data.get("sessions"))
                        .and_then(Value::as_array)
                        .cloned()
                        .unwrap_or_default();
                    let mut current = HashSet::new();
                    for row in rows {
                        let Some(local_id) = row.get("id").and_then(Value::as_str) else {
                            continue;
                        };
                        if local_id == "parent-session" {
                            continue;
                        }
                        current.insert(local_id.to_owned());
                        write
                            .send(Message::Text(
                                json!({
                                    "type": "session-upsert",
                                    "session": {
                                        "id": local_id,
                                        "sessionId": row.get("sessionId").cloned().unwrap_or(Value::Null),
                                        "label": row.get("label").cloned().unwrap_or(Value::Null),
                                        "kind": row.get("kind").cloned().unwrap_or_else(|| json!("parallel")),
                                        "cwd": row.get("cwd").cloned().unwrap_or(Value::Null),
                                        "model": row.get("model").cloned().unwrap_or(Value::Null),
                                        "thinkingLevel": row.get("thinkingLevel").cloned().unwrap_or(Value::Null),
                                        "isStreaming": row.get("isStreaming").cloned().unwrap_or_else(|| json!(false)),
                                        "hasPendingUiRequest": row.get("hasPendingUiRequest").cloned().unwrap_or_else(|| json!(false)),
                                        "lastActivityAt": row.get("lastActivityAt").cloned().unwrap_or(Value::Null),
                                        "messageCount": row.get("messageCount").cloned().unwrap_or_else(|| json!(0)),
                                    }
                                })
                                .to_string(),
                            ))
                            .await?;
                    }
                    for stale in known_pool_sessions.difference(&current) {
                        write
                            .send(Message::Text(
                                json!({ "type": "session-remove", "sessionId": stale })
                                    .to_string(),
                            ))
                            .await?;
                    }
                    known_pool_sessions = current;
                }
                let frame_session_id = payload
                    .get("sessionWorkerId")
                    .and_then(Value::as_str)
                    .unwrap_or("parent-session");
                let frame = json!({
                    "type": "frame",
                    "sessionId": frame_session_id,
                    "payload": payload,
                });
                write.send(Message::Text(frame.to_string())).await?;
            }
            message = read.next() => {
                match message {
                    Some(Ok(Message::Text(text))) => {
                        let value: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
                        match value.get("type").and_then(Value::as_str) {
                            Some("command") => {
                                if let Some(line) = value.get("line").and_then(Value::as_str) {
                                    bridge.handle_client_line(subscriber.id(), line).await;
                                }
                            }
                            Some("resync") => {
                                let session_id =
                                    value.get("sessionId").and_then(Value::as_str);
                                for line in resync_client_lines(session_id) {
                                    bridge.handle_client_line(subscriber.id(), &line).await;
                                }
                            }
                            Some("ping") => {
                                write
                                    .send(Message::Text(json!({"type":"pong"}).to_string()))
                                    .await?;
                            }
                            Some("hello-ok") => {
                                status.set(STATE_CONNECTED);
                            }
                            Some("error") => {
                                status.set_error(
                                    value
                                        .get("error")
                                        .and_then(|error| error.get("message"))
                                        .and_then(Value::as_str)
                                        .unwrap_or("server error")
                                        .to_owned(),
                                );
                            }
                            _ => {}
                        }
                    }
                    Some(Ok(Message::Ping(_))) | Some(Ok(Message::Pong(_))) => {}
                    Some(Ok(Message::Close(_))) | None => {
                        return Err(anyhow::anyhow!("server closed the connection"));
                    }
                    Some(Err(error)) => return Err(error.into()),
                    _ => {}
                }
            }
            _ = shutdown.changed() => {
                let _ = write.send(Message::Close(None)).await;
                return Ok(());
            }
        }
    }
}

/// Build the bridge client lines for a relay `resync` message.
///
/// The relay sends `resync` for the parent session whenever a phone joins.
/// That session is the bridge's default; synthesizing
/// `session-select parent-session` would route it through the parallel-session
/// pool and be rejected as an unknown parallel session (spurious
/// `server/client-error` on the phone). Only pool children need an explicit
/// select. Every resync still asks the host for a fresh
/// status/catalog/snapshot.
///
/// `#[doc(hidden)] pub`: asserted directly by `tests/suites/phone_units.rs`
/// (the only production caller is the WS client task loop).
#[doc(hidden)]
pub fn resync_client_lines(session_id: Option<&str>) -> Vec<String> {
    let mut lines = Vec::new();
    if let Some(session_id) = session_id {
        if session_id != "parent-session" {
            lines.push(
                json!({
                    "kind": "session-select",
                    "sessionId": session_id,
                })
                .to_string(),
            );
        }
    }
    lines.push("{\"kind\":\"refresh\"}".to_owned());
    lines
}

fn normalize_url(raw: &str) -> String {
    let mut url = raw.trim().to_owned();
    if let Some(rest) = url.strip_prefix("https://") {
        url = format!("wss://{rest}");
    } else if let Some(rest) = url.strip_prefix("http://") {
        url = format!("ws://{rest}");
    }
    let url = url.trim_end_matches('/').to_owned();
    if url.ends_with("/agent") {
        url
    } else {
        format!("{url}/agent")
    }
}

fn default_device_name() -> String {
    std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .unwrap_or_else(|_| "pir-device".to_owned())
}

fn device_file() -> PathBuf {
    crate::ai::providers::agent_dir().join(DEVICE_FILE)
}

fn load_or_create_device_id(device_name: &mut String) -> Result<String, String> {
    let path = device_file();
    if let Ok(text) = std::fs::read_to_string(&path) {
        if let Ok(value) = serde_json::from_str::<Value>(&text) {
            if let Some(id) = value.get("deviceId").and_then(Value::as_str) {
                if let Some(name) = value.get("deviceName").and_then(Value::as_str) {
                    if device_name.is_empty() || *device_name == default_device_name() {
                        *device_name = name.to_owned();
                    }
                }
                return Ok(id.to_owned());
            }
        }
    }
    let id = uuid::Uuid::new_v4().to_string();
    let value = json!({ "deviceId": id, "deviceName": device_name });
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(&path, value.to_string());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(metadata) = std::fs::metadata(&path) {
            let mut permissions = metadata.permissions();
            permissions.set_mode(0o600);
            let _ = std::fs::set_permissions(&path, permissions);
        }
    }
    Ok(id)
}
