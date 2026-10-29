//! pi-phone, ported natively: the phone-first remote UI.
//!
//! A local HTTP + WebSocket server mirrors the live session to a mobile web
//! app, can spawn parallel `pir --mode rpc` sessions, and supports optional
//! Tailscale Serve exposure. This builtin is **default off** (it binds a local
//! port and holds a server task): it joins the plan only when named
//! explicitly (`builtin:phone` in settings/packages or `-e`), and the JS
//! `@malinamnam/pi-phone` package is default-disabled in the loader.
//!
//! Module layout:
//! - `server`: HTTP/WS server, static assets, start/stop/status commands, and
//!   the [`PhoneHost`] host seam.
//! - `session`: the [`SessionBridge`] (WS fan-out + host-event mapping) and the
//!   parallel RPC session pool.
//! - `assets`: the embedded browser client (served from `assets/`).
//! - `tailscale`: Tailscale Serve route management (`/phone-<port>`), with an
//!   injectable runner so tests never invoke a real `tailscale`.
//! - `runtime_state`: the persisted per-`host:port` runtime record used for
//!   stale-session detection (digest only, never the token).
//!
//! ## Bridge wiring
//!
//! `/phone-start` creates one [`SessionBridge`] per listener. Each `/ws`
//! connection subscribes to a bounded per-client queue and feeds every JSON
//! text frame to [`SessionBridge::handle_client_line`]; the bridge routes the
//! parsed command to [`HostCommands`] and fans mapped host events back out.
//! `/phone-stop` closes the bridge, so every subscriber and connection task
//! observes `ServerClosed` even if the listener is already gone.
//!
//! ## Host seam
//!
//! The live-session adapter ([`LivePhoneHost`]) is installed by
//! `AgentSession::new_with_plugins` (through
//! [`PhonePlugin::ensure_live_session_slot`]) and bound to the session `Arc`
//! by `AgentSession::install_phone_host`; `AgentSession::new_arc_with_plugins`
//! does both. It receives every phone command (prompt with images and
//! `steer`/`followUp` semantics, abort, model/thinking changes, compaction,
//! stats, tree, fork, session new/switch/list, raw pi-RPC passthrough) through
//! [`HostCommands::dispatch`] and runs it on the session's own async methods.
//!
//! [`PhonePlugin::on_event`] feeds the agent/session events back into the same
//! bridge through [`SessionBridge::dispatch_host_event`], reusing the bridge's
//! mapping table (`agent_start`, `message_*`, `tool_execution_*`,
//! `queue_update`, `compaction_*`, `auto_retry_*`, `extension_ui_request`,
//! `status`, ...). At every run boundary the host publishes real status,
//! catalog and snapshot state ([`PhonePlugin::publish_status`],
//! [`PhonePlugin::publish_catalog`], [`PhonePlugin::publish_snapshot`]) and
//! stores the saved-session listing ([`PhonePlugin::publish_session_list`]).
//! [`PhonePlugin::published_health_json`] is the host half the server lane must
//! merge into `/api/health` and the boot `server/status` frame; without it the
//! endpoint keeps reporting the static placeholder.
//!
//! Without an installed adapter [`PhoneHost`] answers only standalone
//! introspection commands; state-changing commands fail honestly.
//!
//! ## Registered surface (mirrors `register-phone-extension.ts`)
//!
//! Four commands, with the reference names, descriptions and argument shapes:
//!
//! | command | args |
//! |---|---|
//! | `phone-start` | `[port] [token] [--cwd path] [--host 127.0.0.1] [--idle-mins 20]` (also `--port`, `--token`, `--idle-secs`, `=` forms, `-` disables the token) |
//! | `phone-stop` | none |
//! | `phone-status` | none |
//! | `phone-token` | none (reports the token; the reference never rotates it from a command) |
//!
//! Status key `pi-phone`: `📱 ● phone on` / `📱 ○ phone off` ([`STATUS_KEY`],
//! [`server::status_line`]). The plugin implements [`AgentPlugin::statuses`],
//! so the native host channel (`PluginHost::statuses`) carries the live entry
//! in a builtin-only plan. When a JS runtime is attached the same entry is
//! additionally published through `__pir_host.setStatus`
//! (`PhoneState::attach_runtime` / `sync_status`) for JS-facing surfaces; the
//! footer merge dedupes by key (native wins), so the two paths never render
//! a duplicate line.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex as StdMutex, Weak};

use serde_json::{json, Value as JsonValue};

use crate::extensions::{CommandOutcome, ExtensionHandle};
use crate::plugins::plugin::{
    AgentPlugin, CommandBackend, CommandHandler, EventInterest, PluginEvent, PluginEventOutcome,
    PluginFuture,
};
use crate::plugins::{PluginInfo, RegistrationContext};

pub mod assets;
pub mod runtime_state;
pub mod server;
pub mod session;
pub mod tailscale;

pub use runtime_state::{
    digest_control_token, host_key, is_process_running, ownership_message, reconcile_runtime,
    PersistedPhoneRuntime, Reconcile, RuntimeStore, RUNTIME_STATE_DIR,
};
pub use server::{
    generate_token, parse_start_args, status_line, BridgeRuntime, ParsedPhoneArgs, PhoneConfig,
    PhoneHost, PhoneState, COMMAND_NAMES, CONTROL_STOP_PATH, DEFAULT_IDLE_TIMEOUT_MS,
    DEFAULT_PHONE_PORT, MAX_AUTO_PORT, STATUS_KEY,
};
pub use session::{
    BridgeConfig, BridgeStats, BridgeSubscriber, CloseReason, ForwardedCommand, HostCommands,
    HostFuture, HostReply, PhoneCommand, SessionBridge, SubscribeError,
};
pub use tailscale::{
    disable_args, enable_args, serve_path, serve_status_args, CommandOutput, DisableResult,
    EnableResult, NoopTailscaleRunner, ProcessTailscaleRunner, ServeInfo, Tailscale,
    TailscaleRunner, TAILSCALE_TIMEOUT,
};

/// Events whose payloads the plugin forwards to the bridge's host feed.
///
/// These are exactly the event names [`session::SessionBridge`] maps to phone
/// envelopes (plus RPC responses, which the host returns through
/// [`HostReply`]). Keep this list in sync with the mapping table in
/// `session.rs`; the bridge itself owns the shape normalization.
pub const PHONE_HOST_EVENT_NAMES: [&str; 15] = [
    "agent_start",
    "agent_end",
    "message_start",
    "message_update",
    "message_end",
    "tool_execution_start",
    "tool_execution_update",
    "tool_execution_end",
    "queue_update",
    "compaction_start",
    "compaction_end",
    "auto_retry_start",
    "auto_retry_end",
    "extension_ui_request",
    "status",
];

/// `/phone-start` description, verbatim from the reference registration.
pub const PHONE_START_DESCRIPTION: &str = "Start the phone web UI. Usage: /phone-start [port] [token] [--cwd path] [--host 127.0.0.1] [--idle-mins 20]";
/// `/phone-stop` description.
pub const PHONE_STOP_DESCRIPTION: &str =
    "Stop the phone web UI server and remove the matching Tailscale Serve route";
/// `/phone-status` description.
pub const PHONE_STATUS_DESCRIPTION: &str = "Show phone server and Tailscale Serve status";
/// `/phone-token` description.
pub const PHONE_TOKEN_DESCRIPTION: &str = "Show the current phone UI token";

/// The binding slot shared by a [`PhonePlugin`] and its live-session adapter.
///
/// The host installs the adapter at session construction
/// ([`PhonePlugin::ensure_live_session_slot`]) and binds the session once it
/// is reachable as an `Arc` ([`PhonePlugin::bind_live_session`], normally
/// `AgentSession::install_phone_host`). The slot is the only bridge between
/// the two: the adapter holds a `Weak` so the session -> plugin host -> phone
/// plugin -> adapter chain cannot form a reference cycle.
#[doc(hidden)]
#[derive(Default)]
pub struct PhoneSessionSlot {
    session: StdMutex<Option<Weak<crate::agent_session::AgentSession>>>,
}

impl PhoneSessionSlot {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Bind (or replace) the live session this slot mirrors.
    pub fn bind(&self, session: Weak<crate::agent_session::AgentSession>) {
        *self
            .session
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(session);
    }

    /// The live session, when it is still alive.
    pub fn upgrade(&self) -> Option<Arc<crate::agent_session::AgentSession>> {
        self.session
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
            .and_then(Weak::upgrade)
    }

    pub fn is_bound(&self) -> bool {
        self.upgrade().is_some()
    }
}

/// The production [`HostCommands`] adapter: every phone command runs against
/// the live [`crate::agent_session::AgentSession`] the host bound to the slot.
///
/// Installed by `AgentSession::new_with_plugins` (via
/// [`PhonePlugin::ensure_live_session_slot`]) and bound by
/// `AgentSession::install_phone_host`. Until a session is bound, dispatch
/// fails honestly instead of pretending the standalone placeholders are live
/// state.
pub struct LivePhoneHost {
    slot: Arc<PhoneSessionSlot>,
}

impl LivePhoneHost {
    pub fn new(slot: Arc<PhoneSessionSlot>) -> Self {
        Self { slot }
    }
}

impl HostCommands for LivePhoneHost {
    fn dispatch(&self, command: ForwardedCommand) -> HostFuture {
        let session = self.slot.upgrade();
        Box::pin(async move {
            let Some(session) = session else {
                return Err(
                    "The live session is not bound to the phone host yet: this session \
                     was built without a plugin host or its phone plugin was created \
                     standalone."
                        .to_owned(),
                );
            };
            session.phone_dispatch_command(command).await
        })
    }
}

/// The most recent live state the host published into the plugin.
///
/// `/api/health` and the boot `server/status` frame are server-owned today, so
/// this is the host half the server lane needs to merge in (see
/// [`PhonePlugin::published_health_json`]).
#[doc(hidden)]
#[derive(Default)]
pub struct PublishedPhoneState {
    status: Option<JsonValue>,
    snapshot: Option<JsonValue>,
    catalog: Option<JsonValue>,
    session_list: Option<JsonValue>,
}

/// Shared bookkeeping captured by the plugin's command closures.
#[derive(Default)]
struct PhoneShared {
    /// The live-session binding slot (installed with the adapter).
    live_session: StdMutex<Option<Arc<PhoneSessionSlot>>>,
    /// Last host-published status/snapshot/catalog/session list.
    published: StdMutex<PublishedPhoneState>,
    /// `true` once a phone write took ownership while idle.
    phone_owner: AtomicBool,
    /// A phone write happened since the last run boundary. A write DURING a
    /// CLI run (abort, steer, model change) keeps ownership on the phone when
    /// that run settles.
    phone_dirty: AtomicBool,
    /// Phone-initiated runs currently in flight (a counter so a settle can
    /// tell whether the run it just finished belongs to the phone).
    phone_runs_active: AtomicUsize,
}

/// The builtin plugin instance.
pub struct PhonePlugin {
    info: PluginInfo,
    state: Arc<PhoneState>,
    shared: Arc<PhoneShared>,
}

impl PhonePlugin {
    /// A plugin whose Tailscale integration is a no-op.
    ///
    /// This is the default (and test) constructor: it can never spawn the
    /// `tailscale` CLI, so constructing or starting a plugin from tests or an
    /// embedder cannot reconfigure a real Serve setup. The builtin factory
    /// ([`new_phone_plugin`]) builds the production instance with the real
    /// [`tailscale::ProcessTailscaleRunner`].
    pub fn new() -> Self {
        Self::with_tailscale(Tailscale::disabled())
    }

    /// Build a plugin with an injected Tailscale runner (test seam).
    ///
    /// Mirrors [`PhonePlugin::set_tailscale_for_test`] but applies at
    /// construction time, which is what most tests want.
    #[doc(hidden)]
    pub fn with_tailscale_runner(runner: Arc<dyn tailscale::TailscaleRunner>) -> Self {
        Self::with_tailscale(Tailscale::with_runner(runner))
    }

    fn with_tailscale(tailscale: Tailscale) -> Self {
        Self {
            info: PluginInfo::builtin("phone"),
            state: Arc::new(PhoneState::from_env_with_tailscale(tailscale)),
            shared: Arc::new(PhoneShared::default()),
        }
    }

    /// The injected Tailscale runner's kind (`"process"`, `"noop"`, or
    /// `"custom"`), for test/assembly inspection.
    #[doc(hidden)]
    pub fn tailscale_runner_kind(&self) -> &'static str {
        self.state.tailscale_runner_kind()
    }

    /// The status-line entry this plugin owns, in the reference format.
    ///
    /// Exposed for the host lane and tests; the live channels are
    /// [`AgentPlugin::statuses`] (native) and the JS `setStatus` bridge
    /// described in the module docs.
    pub fn status_entry(&self) -> (&'static str, String) {
        (STATUS_KEY, status_line(self.state.is_running()))
    }

    /// Whether a listener is currently running.
    pub fn is_running(&self) -> bool {
        self.state.is_running()
    }

    /// The bound listener address, when running.
    pub fn local_addr(&self) -> Option<std::net::SocketAddr> {
        self.state.local_addr()
    }

    /// Direct test seam: run `/phone-start` and return the bound port.
    ///
    /// `args` is the same command-argument string the slash command receives;
    /// `"0"` requests an ephemeral port so a hermetic test can bind without a
    /// fixed-port collision.
    #[doc(hidden)]
    pub async fn start_for_test(&self, args: &str) -> Result<u16, String> {
        self.state.start_for_test(args).await
    }

    /// Direct test seam: stop the server; true when one was running.
    #[doc(hidden)]
    pub async fn stop_for_test(&self) -> bool {
        self.state.stop_for_test().await
    }

    /// Direct test seam: replace the Tailscale client with a fake runner.
    ///
    /// Production always uses the default `tailscale` binary; tests inject a
    /// recording runner so they assert the exact argv without a real tailnet.
    #[doc(hidden)]
    pub fn set_tailscale_for_test(&self, client: tailscale::Tailscale) {
        self.state.set_tailscale(client);
    }

    /// Direct test seam: point the persisted runtime state at a temp directory.
    #[doc(hidden)]
    pub fn set_runtime_store_for_test(&self, store: runtime_state::RuntimeStore) {
        self.state.set_runtime_store(store);
    }

    /// Direct test seam: insert a base64 image into the `/api/image` store.
    #[doc(hidden)]
    pub fn store_image_for_test(&self, data: &str, mime_type: &str) -> Option<String> {
        self.state.store_image(data, mime_type)
    }

    /// Direct test seam: `(entry count, decoded bytes)` of the `/api/image`
    /// store (the reference `PhoneImageStore.stats`).
    #[doc(hidden)]
    pub fn image_stats(&self) -> (usize, usize) {
        self.state.image_stats()
    }

    /// Direct test seam: age every stored image past the TTL (the real wait
    /// is two hours).
    #[doc(hidden)]
    pub fn age_images_for_test(&self, age: std::time::Duration) {
        self.state.age_images_for_test(age);
    }

    /// Direct test seam: read one stored image through the TTL-throttled path.
    #[doc(hidden)]
    pub fn image_for_test(&self, key: &str) -> Option<(std::sync::Arc<[u8]>, String)> {
        self.state.image_for_test(key)
    }

    /// Install a `HostCommands` adapter: the documented plugin/host seam.
    ///
    /// Production uses [`LivePhoneHost`], installed at session construction
    /// ([`PhonePlugin::ensure_live_session_slot`]) and bound by
    /// `AgentSession::install_phone_host`. This setter remains for recording
    /// hosts (tests, embedders) and takes effect immediately because
    /// [`PhoneHost`] shares the adapter slot.
    #[doc(hidden)]
    pub fn set_host_commands(&self, host: Arc<dyn HostCommands>) {
        self.state.set_host_commands(host);
    }

    /// The bridge of the running server (test/host-lane inspection seam).
    #[doc(hidden)]
    pub fn bridge_for_test(&self) -> Option<SessionBridge> {
        self.state.bridge_for_test()
    }

    /// Whether a `HostCommands` adapter is installed (the production
    /// [`LivePhoneHost`] or a recording host from a test).
    #[doc(hidden)]
    pub fn has_host_commands(&self) -> bool {
        self.state.host_commands().is_some()
    }

    /// Whether the installed adapter is bound to a live session.
    #[doc(hidden)]
    pub fn has_live_session(&self) -> bool {
        self.live_session_slot().is_some_and(|slot| slot.is_bound())
    }

    /// The live-session slot, when the host installed one.
    #[doc(hidden)]
    pub fn live_session_slot(&self) -> Option<Arc<PhoneSessionSlot>> {
        self.shared
            .live_session
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Install the production adapter (`LivePhoneHost`) if no host adapter is
    /// installed yet, and return the binding slot.
    ///
    /// Called by `AgentSession::new_with_plugins` whenever the phone plugin is
    /// part of the plan. A host already installed through
    /// [`PhonePlugin::set_host_commands`] is never replaced.
    #[doc(hidden)]
    pub fn ensure_live_session_slot(&self) -> Arc<PhoneSessionSlot> {
        let mut slot_guard = self
            .shared
            .live_session
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(slot) = slot_guard.as_ref() {
            return Arc::clone(slot);
        }
        let slot = PhoneSessionSlot::new();
        *slot_guard = Some(Arc::clone(&slot));
        if !self.has_host_commands() {
            self.state
                .set_host_commands(Arc::new(LivePhoneHost::new(Arc::clone(&slot))));
        }
        slot
    }

    /// Bind the live session the adapter dispatches to.
    ///
    /// Idempotent; re-binding replaces the previous session. Returns `false`
    /// when the session has no plugin host (the phone plugin cannot be part
    /// of such a session).
    #[doc(hidden)]
    pub fn bind_live_session(&self, session: Weak<crate::agent_session::AgentSession>) -> bool {
        let slot = self.ensure_live_session_slot();
        slot.bind(session);
        true
    }

    pub fn has_bridge(&self) -> bool {
        self.state.has_bridge()
    }

    /// Record a state-changing phone write: while idle the phone owns control.
    pub fn note_phone_write(&self) {
        self.shared.phone_owner.store(true, Ordering::SeqCst);
        self.shared.phone_dirty.store(true, Ordering::SeqCst);
    }

    /// Wrap a phone-initiated prompt run. While the counter is non-zero, the
    /// run boundary does not hand ownership back to the CLI (the run is the
    /// phone's write).
    pub fn begin_phone_run(&self) {
        self.shared.phone_runs_active.fetch_add(1, Ordering::SeqCst);
    }

    pub fn end_phone_run(&self) {
        self.shared.phone_runs_active.fetch_sub(1, Ordering::SeqCst);
    }

    /// The run boundary: a run the phone did not start takes control back
    /// (`controlOwner` was already `cli` while it streamed; it stays `cli`
    /// after settling until the next phone write).
    pub fn note_run_started(&self) {
        if self.shared.phone_runs_active.load(Ordering::SeqCst) == 0 {
            self.shared.phone_owner.store(false, Ordering::SeqCst);
            self.shared.phone_dirty.store(false, Ordering::SeqCst);
        }
    }

    /// The run settled. With no phone-initiated run active and no phone write
    /// during the run, the CLI owns the idle session again. A phone write
    /// DURING the run (abort/steer/model change) keeps ownership on the phone.
    pub fn note_run_settled(&self) {
        if self.shared.phone_runs_active.load(Ordering::SeqCst) == 0 {
            if !self.shared.phone_dirty.swap(false, Ordering::SeqCst) {
                self.shared.phone_owner.store(false, Ordering::SeqCst);
            }
        } else {
            self.shared.phone_dirty.store(false, Ordering::SeqCst);
        }
    }

    /// The minimal ownership rule ported from the JS runtime:
    /// `cli` while a run streams, `phone` after a phone write while idle.
    pub fn control_owner(&self, streaming: bool) -> &'static str {
        if streaming {
            "cli"
        } else if self.shared.phone_owner.load(Ordering::SeqCst) {
            "phone"
        } else {
            "cli"
        }
    }

    /// Store + forward one host-owned status object.
    ///
    /// Server-owned fields (`isRunning`, `host`, `port`, `connectedClients`,
    /// `singleClientMode`) are merged here so a WebSocket `status` frame never
    /// replaces the client's status with a partial object (the JS boot status
    /// is replaced wholesale by the client).
    pub fn publish_status(&self, mut status: JsonValue) {
        if let Some(object) = status.as_object_mut() {
            let running = self.state.is_running();
            let streaming = object
                .get("isStreaming")
                .and_then(JsonValue::as_bool)
                .unwrap_or(false);
            object.insert("isRunning".to_owned(), json!(running));
            object.insert("serverRunning".to_owned(), json!(running));
            object.insert("singleClientMode".to_owned(), json!(true));
            object.insert(
                "controlOwner".to_owned(),
                json!(self.control_owner(streaming)),
            );
            if let Some(addr) = self.state.local_addr() {
                object.insert("host".to_owned(), json!(addr.ip().to_string()));
                object.insert("port".to_owned(), json!(addr.port()));
            }
            if let Some(bridge) = self.state.bridge_for_test() {
                object.insert("connectedClients".to_owned(), json!(bridge.client_count()));
            }
        }
        self.shared
            .published
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .status = Some(status.clone());
        // The server lane reads this cell for `/api/health` and the boot
        // `status` frame, so a live host replaces the static placeholders.
        self.state.set_host_status(status.clone());
        if self.state.has_bridge() {
            self.state
                .forward_host_event(json!({ "type": "status", "data": status }));
        }
    }

    /// Store + forward the catalog (live session rows).
    pub fn publish_catalog(&self, mut catalog: JsonValue) {
        if let Some(object) = catalog.as_object_mut() {
            object
                .entry("activeSessionId")
                .or_insert_with(|| json!("parent-session"));
        }
        self.shared
            .published
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .catalog = Some(catalog.clone());
        self.state.set_host_catalog(catalog.clone());
        if self.state.has_bridge() {
            self.state
                .forward_host_event(json!({ "type": "catalog", "data": catalog }));
        }
    }

    /// Store + forward one snapshot envelope body (`state`/`messages`/...).
    ///
    /// Inline base64 image parts are replaced with `/api/image/<key>` URLs
    /// first (the reference `PhoneImageStore.extractImages`): a snapshot with
    /// screenshots would otherwise re-send hundreds of KB of base64 on every
    /// refresh. The snapshot is owned here, so the rewrite mutates it in place
    /// and costs one copy per image, never one per message.
    pub fn publish_snapshot(&self, mut snapshot: JsonValue) {
        self.state.extract_snapshot_images(&mut snapshot);
        self.shared
            .published
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .snapshot = Some(snapshot.clone());
        self.state.set_host_snapshot(snapshot.clone());
        if self.state.has_bridge() {
            let mut event = snapshot;
            if let Some(object) = event.as_object_mut() {
                object.insert("type".to_owned(), json!("snapshot"));
            }
            self.state.forward_host_event(event);
        }
    }

    /// Store the saved-session listing (answered from `phone_list_sessions`).
    pub fn publish_session_list(&self, list: JsonValue) {
        self.shared
            .published
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .session_list = Some(list);
    }

    /// Forward one agent-loop event to the bridge (no-op without a bridge).
    pub fn publish_agent_event(&self, event: &crate::agent::agent_loop::AgentEvent) {
        if !self.state.has_bridge() {
            return;
        }
        self.state
            .forward_host_event(crate::agent_session::agent_event_to_json(event));
    }

    /// Surface an out-of-band host error (a spawned run that failed after the
    /// command response was already acknowledged).
    pub fn publish_client_error(&self, message: &str) {
        if !self.state.has_bridge() {
            return;
        }
        self.state.forward_host_event(json!({
            "type": "client-error",
            "data": { "message": message },
        }));
    }

    /// Forward one session-level event (`queue_update`, `compaction_*`,
    /// `auto_retry_*`).
    pub fn publish_session_event(&self, event: &crate::agent_session::SessionEvent) {
        if !self.state.has_bridge() {
            return;
        }
        self.state
            .forward_host_event(crate::agent_session::session_event_to_json(event));
    }

    /// The last status the host published (`None` before the first publish).
    #[doc(hidden)]
    pub fn published_status(&self) -> Option<JsonValue> {
        self.shared
            .published
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .status
            .clone()
    }

    /// Whether a snapshot was ever published (the run boundary skips the
    /// expensive transcript build while no phone server is listening).
    #[doc(hidden)]
    pub fn has_published_snapshot(&self) -> bool {
        self.shared
            .published
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .snapshot
            .is_some()
    }

    /// The last snapshot the host published.
    #[doc(hidden)]
    pub fn published_snapshot(&self) -> Option<JsonValue> {
        self.shared
            .published
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .snapshot
            .clone()
    }

    /// The last catalog the host published.
    #[doc(hidden)]
    pub fn published_catalog(&self) -> Option<JsonValue> {
        self.shared
            .published
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .catalog
            .clone()
    }

    /// The last saved-session listing the host published.
    #[doc(hidden)]
    pub fn published_session_list(&self) -> Option<JsonValue> {
        self.shared
            .published
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .session_list
            .clone()
    }

    /// The real status `/api/health` and the boot `server/status` frame must
    /// merge from (finding 3 of the adversarial review).
    ///
    /// When no host state was published yet, only the live server fields are
    /// reported; the server keeps its own `pid`/`host`/`port`/`hasToken`.
    #[doc(hidden)]
    pub fn published_health_json(&self) -> JsonValue {
        let status = self
            .shared
            .published
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .status
            .clone();
        match status {
            Some(status) => status,
            None => {
                let running = self.state.is_running();
                json!({
                    "isRunning": running,
                    "serverRunning": running,
                    "singleClientMode": true,
                    "controlOwner": self.control_owner(false),
                })
            }
        }
    }
}

impl Default for PhonePlugin {
    fn default() -> Self {
        Self::new()
    }
}

fn command_outcome(result: Result<String, String>) -> CommandOutcome {
    match result {
        Ok(text) => CommandOutcome {
            ok: true,
            error: Some(text),
        },
        Err(message) => CommandOutcome {
            ok: false,
            error: Some(message),
        },
    }
}

impl AgentPlugin for PhonePlugin {
    fn info(&self) -> &PluginInfo {
        &self.info
    }

    /// Downcast support for the host's generic live-adapter install
    /// (`AgentSession::new_with_plugins` reaches the phone plugin through
    /// `PluginHost::plugins()` and `set_host_commands`).
    fn as_any(&self) -> Option<&dyn std::any::Any> {
        Some(self)
    }

    /// The live `pi-phone` entry: `📱 ● phone on` while the server runs,
    /// `📱 ○ phone off` otherwise (the reference status text — the JS face
    /// adds no Tailscale suffix to the status line; the Serve URL is reported
    /// by `/phone-status` notifications instead).
    ///
    /// The plugin has no external status state of its own, so this reflects
    /// the shared [`PhoneState`] on every poll and flips as start/stop does.
    fn statuses(&self) -> Vec<(String, String)> {
        let (key, text) = self.status_entry();
        vec![(key.to_owned(), text)]
    }

    fn register(&self, ctx: &mut RegistrationContext<'_>) -> anyhow::Result<()> {
        let state = Arc::clone(&self.state);
        let shared = Arc::clone(&self.shared);
        let start: CommandHandler = Arc::new(move |args: &str| {
            let state = Arc::clone(&state);
            let shared = Arc::clone(&shared);
            let args = args.to_owned();
            Box::pin(async move {
                let result = state.start_command(&args).await;
                if result.is_ok() {
                    // Seed the host half of the health/status surface right
                    // after the listener exists. The server lane merges
                    // `published_health_json()` into its own status object.
                    let session = shared
                        .live_session
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .as_ref()
                        .and_then(|slot| slot.upgrade());
                    if let Some(session) = session {
                        tokio::spawn(async move {
                            session.publish_phone_state_now().await;
                        });
                    }
                }
                command_outcome(result)
            })
        });
        ctx.register_command(
            COMMAND_NAMES[0],
            Some(PHONE_START_DESCRIPTION),
            Some(CommandBackend::Rust(start)),
        );

        let state = Arc::clone(&self.state);
        let shared = Arc::clone(&self.shared);
        let stop: CommandHandler = Arc::new(move |_args: &str| {
            let state = Arc::clone(&state);
            let shared = Arc::clone(&shared);
            Box::pin(async move {
                let stopped = state.stop_command().await;
                shared.phone_owner.store(false, Ordering::SeqCst);
                command_outcome(stopped)
            })
        });
        ctx.register_command(
            COMMAND_NAMES[1],
            Some(PHONE_STOP_DESCRIPTION),
            Some(CommandBackend::Rust(stop)),
        );

        let state = Arc::clone(&self.state);
        let status: CommandHandler = Arc::new(move |_args: &str| {
            let state = Arc::clone(&state);
            Box::pin(async move { command_outcome(state.status_command().await) })
        });
        ctx.register_command(
            COMMAND_NAMES[2],
            Some(PHONE_STATUS_DESCRIPTION),
            Some(CommandBackend::Rust(status)),
        );

        let state = Arc::clone(&self.state);
        let token: CommandHandler = Arc::new(move |_args: &str| {
            let state = Arc::clone(&state);
            Box::pin(async move { command_outcome(state.token_command().await) })
        });
        ctx.register_command(
            COMMAND_NAMES[3],
            Some(PHONE_TOKEN_DESCRIPTION),
            Some(CommandBackend::Rust(token)),
        );

        // The `pi-phone` entry rides the native status channel
        // (`AgentPlugin::statuses` → `PluginHost::statuses`); the JS runtime
        // bridge in `on_runtime_attached` stays for JS-facing surfaces.
        Ok(())
    }

    fn events(&self) -> EventInterest {
        EventInterest::List(
            PHONE_HOST_EVENT_NAMES
                .iter()
                .map(|name| (*name).to_owned())
                .collect(),
        )
    }

    /// Forward one agent/session event into the running phone bridge.
    ///
    /// The payload is cloned and given its `type` when the emitter omitted it
    /// (`status` is the common case); shape mapping stays in `session.rs` so
    /// there is exactly one host-event -> phone-envelope table. The feed uses
    /// the bridge's bounded queue with `try_send`, so a slow phone client can
    /// never block the agent actor.
    fn on_event<'a>(&'a self, event: PluginEvent<'a>) -> PluginFuture<'a, PluginEventOutcome> {
        let name = event.name.as_ref();
        let interested = PHONE_HOST_EVENT_NAMES.contains(&name);
        // Skip the payload clone when no bridge is listening: `message_update`
        // payloads carry the cumulative assistant message and this runs on the
        // streaming hot path.
        if interested && self.state.has_bridge() {
            let mut value = event.payload.clone();
            let typed = value.get("type").and_then(JsonValue::as_str) == Some(name);
            if !typed {
                match value.as_object_mut() {
                    Some(object) => {
                        object.insert("type".to_owned(), JsonValue::String(name.to_owned()));
                    }
                    None => {
                        value = json!({ "type": name, "data": value });
                    }
                }
            }
            self.state.forward_host_event(value);
        }
        Box::pin(async { PluginEventOutcome::None })
    }

    fn on_runtime_attached(&self, js: Option<ExtensionHandle>) {
        self.state.attach_runtime(js);
    }

    fn on_shutdown(&self) -> PluginFuture<'_, anyhow::Result<()>> {
        let state = Arc::clone(&self.state);
        Box::pin(async move {
            state.shutdown().await;
            Ok(())
        })
    }
}

/// The factory the builtin table names.
///
/// The production instance always uses the real
/// [`tailscale::ProcessTailscaleRunner`]; [`PhonePlugin::new`] deliberately
/// installs a no-op runner instead.
pub fn new_phone_plugin() -> Arc<dyn AgentPlugin> {
    Arc::new(PhonePlugin::with_tailscale_runner(Arc::new(
        tailscale::ProcessTailscaleRunner::new("tailscale"),
    )))
}
