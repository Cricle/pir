//! Remote-only phone transport state. The old embedded HTTP/WS server was
//! removed; the plugin now always talks to the central `pir-phone-server`.

use std::sync::{Arc, Mutex as StdMutex, PoisonError};

use serde_json::Value as JsonValue;
use tokio::sync::mpsc;

use crate::extensions::ExtensionHandle;

use super::remote::RemoteRuntime;
use super::session::{BridgeConfig, HostCommands, PoolConfig, SessionBridge, SessionPool};

pub const STATUS_KEY: &str = "pi-phone";
pub const COMMAND_NAMES: [&str; 4] = ["phone-start", "phone-stop", "phone-status", "phone-token"];

pub fn status_line(running: bool) -> String {
    if running {
        "📱 ● phone on".to_owned()
    } else {
        "📱 ○ phone off".to_owned()
    }
}

/// The bridge handle shared with event producers and the remote link.
#[derive(Clone)]
pub struct BridgeRuntime {
    pub bridge: SessionBridge,
    pub host_tx: mpsc::Sender<JsonValue>,
}

/// Most recent live-session publications, used by the remote link's boot
/// frames and `/api/health`-style status surfaces.
#[derive(Default)]
pub struct HostPublishedState {
    status: Option<JsonValue>,
    catalog: Option<JsonValue>,
    snapshot: Option<JsonValue>,
}

pub type PhonePoolCell = Arc<StdMutex<Option<Arc<SessionPool>>>>;

pub struct PhoneState {
    lifecycle: tokio::sync::Mutex<()>,
    runtime: StdMutex<Option<ExtensionHandle>>,
    host_commands: Arc<StdMutex<Option<Arc<dyn HostCommands>>>>,
    bridge: Arc<StdMutex<Option<BridgeRuntime>>>,
    host_published: Arc<StdMutex<HostPublishedState>>,
    remote: StdMutex<Option<RemoteRuntime>>,
    pool: PhonePoolCell,
}

impl PhoneState {
    pub fn from_env() -> Self {
        Self {
            lifecycle: tokio::sync::Mutex::new(()),
            runtime: StdMutex::new(None),
            host_commands: Arc::new(StdMutex::new(None)),
            bridge: Arc::new(StdMutex::new(None)),
            host_published: Arc::new(StdMutex::new(HostPublishedState::default())),
            remote: StdMutex::new(None),
            pool: Arc::new(StdMutex::new(None)),
        }
    }

    pub fn pool_cell(&self) -> PhonePoolCell {
        Arc::clone(&self.pool)
    }

    pub fn pool(&self) -> Option<Arc<SessionPool>> {
        self.pool
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Create the pool lazily; its child events feed the same SessionBridge
    /// the parent session uses.
    pub fn ensure_pool(&self) -> Result<Arc<SessionPool>, String> {
        if let Some(pool) = self.pool() {
            return Ok(pool);
        }
        let mut pool_config = PoolConfig::default();
        if let Ok(executable) = std::env::var("PI_PHONE_SESSION_POOL_EXECUTABLE") {
            if !executable.trim().is_empty() {
                pool_config.executable = executable.into();
            }
        }
        let (pool, mut events) = SessionPool::new(pool_config);
        let pool = Arc::new(pool);
        let bridge = self
            .bridge
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .as_ref()
            .map(|runtime| runtime.bridge.clone());
        *self.pool.lock().unwrap_or_else(PoisonError::into_inner) = Some(Arc::clone(&pool));
        if let Some(bridge) = bridge {
            tokio::spawn(async move {
                while let Some(event) = events.recv().await {
                    bridge.dispatch_host_event(event.into_value());
                }
            });
        }
        Ok(pool)
    }

    pub fn set_host_commands(&self, host: Arc<dyn HostCommands>) {
        *self
            .host_commands
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(host);
    }

    pub fn host_commands(&self) -> Option<Arc<dyn HostCommands>> {
        self.host_commands
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    pub fn set_host_status(&self, status: JsonValue) {
        self.host_published
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .status = Some(status);
    }

    pub fn set_host_catalog(&self, catalog: JsonValue) {
        self.host_published
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .catalog = Some(catalog);
    }

    pub fn set_host_snapshot(&self, snapshot: JsonValue) {
        self.host_published
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .snapshot = Some(snapshot);
    }

    pub fn host_status(&self) -> Option<JsonValue> {
        self.host_published
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .status
            .clone()
    }

    pub fn host_catalog(&self) -> Option<JsonValue> {
        self.host_published
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .catalog
            .clone()
    }

    pub fn host_snapshot(&self) -> Option<JsonValue> {
        self.host_published
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .snapshot
            .clone()
    }

    pub fn attach_runtime(&self, js: Option<ExtensionHandle>) {
        let text = status_line(self.is_running());
        *self.runtime.lock().unwrap_or_else(PoisonError::into_inner) = js.clone();
        if let Some(js) = js {
            spawn_status_publish(js, text);
        }
    }

    pub fn sync_status(&self) {
        let js = self
            .runtime
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        if let Some(js) = js {
            spawn_status_publish(js, status_line(self.is_running()));
        }
    }

    pub fn forward_host_event(&self, event: JsonValue) -> bool {
        let sender = self
            .bridge
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .as_ref()
            .map(|runtime| runtime.host_tx.clone());
        let Some(sender) = sender else {
            return false;
        };
        sender.try_send(event).is_ok()
    }

    pub fn has_bridge(&self) -> bool {
        self.bridge
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .is_some()
    }

    /// Live "needs input" flag for one session, read from the bridge's
    /// pending-dialog tracker (false while no bridge is attached).
    pub fn pending_ui_request(&self, session_id: &str) -> bool {
        self.bridge
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .as_ref()
            .is_some_and(|runtime| runtime.bridge.pending_ui_request(session_id))
    }

    pub fn is_running(&self) -> bool {
        self.remote
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .is_some()
    }

    pub fn is_remote(&self) -> bool {
        self.is_running()
    }

    pub fn remote_info(&self) -> Option<(String, bool, String)> {
        let remote = self.remote.lock().unwrap_or_else(PoisonError::into_inner);
        remote.as_ref().map(|remote| {
            (
                remote.server_url.clone(),
                remote.status.is_connected(),
                remote.device_id.clone(),
            )
        })
    }

    pub async fn start_remote_command(&self, args: &str) -> Result<String, String> {
        let _lifecycle = self.lifecycle.lock().await;
        if self.is_running() {
            return Err(
                "Pi Phone is already connected to a central server; run /phone-stop first."
                    .to_owned(),
            );
        }
        let config = super::remote::RemoteConfig::parse(args)?;
        let host = self.host_commands().ok_or_else(|| {
            "the live phone host is not installed; this session cannot connect to a server"
                .to_owned()
        })?;
        let (bridge, host_tx) = SessionBridge::start(BridgeConfig::default(), host);
        let runtime = RemoteRuntime::spawn(config, bridge.clone());
        let text = runtime.status.status_text();
        *self.bridge.lock().unwrap_or_else(PoisonError::into_inner) =
            Some(BridgeRuntime { bridge, host_tx });
        *self.remote.lock().unwrap_or_else(PoisonError::into_inner) = Some(runtime);
        self.ensure_pool()?;
        self.sync_status();
        Ok(text)
    }

    pub async fn stop_remote_command(&self) -> Result<String, String> {
        let remote = self
            .remote
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        let Some(remote) = remote else {
            return Err("Pi Phone is not connected to a central server.".to_owned());
        };
        remote.stop().await;
        let pool = self
            .pool
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        if let Some(pool) = pool {
            pool.shutdown().await;
        }
        *self.bridge.lock().unwrap_or_else(PoisonError::into_inner) = None;
        self.sync_status();
        Ok("Pi Phone disconnected from the central server.".to_owned())
    }

    pub async fn status_command(&self) -> Result<String, String> {
        let remote = self.remote.lock().unwrap_or_else(PoisonError::into_inner);
        match remote.as_ref() {
            Some(remote) => Ok(remote.status.status_text()),
            None => Ok(
                "Pi Phone is not connected. Run /phone-start <ws(s)://server> <device-token>."
                    .to_owned(),
            ),
        }
    }

    pub async fn token_command(&self) -> Result<String, String> {
        let remote = self.remote.lock().unwrap_or_else(PoisonError::into_inner);
        match remote.as_ref() {
            Some(remote) => Ok(format!(
                "Pi Phone server device token ({}): {}",
                remote.device_id, remote.token
            )),
            None => Ok("No phone server device token is configured.".to_owned()),
        }
    }

    pub async fn shutdown(&self) {
        let _lifecycle = self.lifecycle.lock().await;
        let remote = self
            .remote
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        if let Some(remote) = remote {
            remote.stop().await;
            *self.bridge.lock().unwrap_or_else(PoisonError::into_inner) = None;
            self.sync_status();
        }
        let pool = self
            .pool
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        if let Some(pool) = pool {
            pool.shutdown().await;
        }
    }
}

fn spawn_status_publish(js: ExtensionHandle, text: String) {
    let key = serde_json::to_string(STATUS_KEY).unwrap_or_else(|_| "\"pi-phone\"".to_owned());
    let text = serde_json::to_string(&text).unwrap_or_else(|_| "\"\"".to_owned());
    let code = format!("__pir_host.setStatus({key}, {text}); JSON.stringify(true);");
    if let Ok(handle) = tokio::runtime::Handle::try_current() {
        handle.spawn(async move {
            let _ = js.eval_json(&code).await;
        });
    }
}
