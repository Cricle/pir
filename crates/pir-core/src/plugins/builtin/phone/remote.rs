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

/// The remote connection states and the device-identity filename are the
/// engine-free crate's (the relay validates the same document).
use pir_builtin_phone::{
    DEVICE_FILE, STATE_CONNECTED, STATE_CONNECTING, STATE_OFF, STATE_RECONNECTING,
};

/// The resolved connection settings and the CLI/env/stored-file resolution are
/// the engine-free crate's (the relay validates the same document): this
/// module supplies the process environment and the device file it owns.
pub use pir_builtin_phone::{ConfigError, RemoteConfig};

/// Resolve `args` plus the process environment into a [`RemoteConfig`],
/// finishing with the host-owned device-identity file.
///
/// The flag grammar, precedence and diagnostics are the crate's `resolve`; this
/// wrapper only injects the process environment and persists a first-run device
/// id (the crate is engine-free and holds no filesystem).
pub fn parse_remote_config(args: &str) -> Result<RemoteConfig, String> {
    let inputs = pir_builtin_phone::ConfigInputs {
        env_server_url: std::env::var(pir_builtin_phone::ENV_SERVER_URL).ok(),
        env_device_token: std::env::var(pir_builtin_phone::ENV_DEVICE_TOKEN).ok(),
        env_token: std::env::var(pir_builtin_phone::ENV_TOKEN).ok(),
        env_device_name: std::env::var(pir_builtin_phone::ENV_DEVICE_NAME).ok(),
        env_device_id: std::env::var(pir_builtin_phone::ENV_DEVICE_ID).ok(),
        default_device_name: default_device_name(),
        stored_device: None,
        generated_device_id: None,
    };
    let mut config = RemoteConfig::resolve(args, &inputs).map_err(|error| error.to_string())?;
    // The host owns the device-identity file: resolve it only when no device id
    // came from the arguments or the environment.
    if config.device_id.trim().is_empty() {
        config.device_id = load_or_create_device_id(&mut config.device_name)?;
    }
    Ok(config)
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
        let connected_secs = self
            .connected_at
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .as_ref()
            .map(|at| at.elapsed().as_secs());
        // The status wording is a pure state→text rule, owned by the
        // engine-free crate so the client and any other surface cannot phrase
        // the same state differently.
        pir_builtin_phone::remote_status_text(
            state,
            &self.server_url,
            &self.device_name,
            &self.device_id,
            connected_secs,
            &error,
        )
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

    // The hello frame is a wire contract shared with the relay: its shape is
    // built by the engine-free crate so the client and the server-side
    // descriptor cannot drift.
    let cwd = std::env::current_dir()
        .ok()
        .map(|path| path.to_string_lossy().into_owned());
    let hello = pir_builtin_phone::hello_frame(
        &config.device_id,
        &config.device_name,
        &format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH),
        cwd.as_deref(),
        bridge.pending_ui_request(pir_builtin_phone::PARENT_SESSION_ID),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_millis() as i64)
            .unwrap_or(0),
    );
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
                        // The upsert frame (and the parent-session skip) is a
                        // wire contract shared with the relay: built by the
                        // engine-free crate.
                        let Some(frame) = pir_builtin_phone::session_upsert_frame(&row) else {
                            continue;
                        };
                        if let Some(local_id) = row.get("id").and_then(Value::as_str) {
                            current.insert(local_id.to_owned());
                        }
                        write.send(Message::Text(frame.to_string())).await?;
                    }
                    for stale in known_pool_sessions.difference(&current) {
                        write
                            .send(Message::Text(
                                pir_builtin_phone::session_remove_frame(stale).to_string(),
                            ))
                            .await?;
                    }
                    known_pool_sessions = current;
                }
                // Take the id as an owned string first: `payload` is moved into
                // the frame below.
                let frame_session_id = payload
                    .get("sessionWorkerId")
                    .and_then(Value::as_str)
                    .unwrap_or(pir_builtin_phone::PARENT_SESSION_ID)
                    .to_owned();
                let frame = pir_builtin_phone::frame_for(Some(&frame_session_id), payload);
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
    pir_builtin_phone::resync_client_lines(session_id)
}

/// URL normalization and the device-name default are pure string rules owned by
/// the engine-free crate (the relay derives the same endpoint), so this module
/// uses them directly instead of carrying second copies.
use pir_builtin_phone::default_device_name_with;

fn default_device_name() -> String {
    default_device_name_with(
        std::env::var("COMPUTERNAME").ok().as_deref(),
        std::env::var("HOSTNAME").ok().as_deref(),
    )
}

fn device_file() -> PathBuf {
    crate::ai::providers::agent_dir().join(DEVICE_FILE)
}

fn load_or_create_device_id(device_name: &mut String) -> Result<String, String> {
    let path = device_file();
    if let Ok(text) = std::fs::read_to_string(&path) {
        // The identity file's shape is owned by the engine-free crate: the
        // relay validates the same document.
        if let Some(stored) = pir_builtin_phone::parse_device_file(&text) {
            if let Some(name) = stored.device_name {
                if device_name.is_empty() || *device_name == default_device_name() {
                    *device_name = name;
                }
            }
            return Ok(stored.device_id);
        }
    }
    let id = uuid::Uuid::new_v4().to_string();
    let text = pir_builtin_phone::device_file_json(&id, device_name);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(&path, text);
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
