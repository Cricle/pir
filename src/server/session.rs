//! Session registry, ownership leases, and service update broadcasts.
//!
//! The prompt/steer/abort/compaction orchestration is delegated to
//! [`crate::agent_session::AgentSession`]: each session record lazily builds
//! ONE agent session (core tools + threshold compaction + steering/abort
//! inboxes) and every prompt runs through the same facade the TUI and RPC
//! hosts use. The registry keeps owning the protocol-facing concerns:
//! attachment leases, phases, snapshots, and progress broadcasts.
//!
//! Prompt runs are **spawned per-session tasks**: `prompt` only validates,
//! flips the phase, arms the run's abort signal, and returns after spawning,
//! so the owning connection's read loop stays free and abort/steer/cancel
//! frames remain reachable while a run is in flight. The prompt response is
//! delivered by the run task through its completion sender.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::Arc;

use serde_json::Value;
use tokio::sync::{broadcast, oneshot, RwLock};
use uuid::Uuid;

use crate::ai::provider::ProviderRegistry;
use crate::ai::types::{AbortSignal, Context, StopReason, ThinkingLevel};

use crate::protocol::{
    session_progress_update_in, session_snapshot_update_in, ModelRef, ServerSnapshot,
    SessionMetadata, SessionSnapshot, StateUpdateFormat, TranscriptItem, UserTranscriptItem,
};
use crate::session_manager::SessionManager;

use super::empty_snapshot;
use super::policy::ServerSecurityConfig;
use super::progress::agent_event_to_progress;

const UPDATE_CHANNEL_CAPACITY: usize = 1024;

/// Bound on the tail window a persisted session is first opened with when
/// attaching (`SessionManager::open_paged`). A session that fits inside these
/// caps is opened fully (the paged open falls back); a larger one parses only
/// the window and the blocking task materializes the rest only because attach
/// needs the branch settings/projection — never on a runtime worker.
const ATTACH_PAGE_MAX_BYTES: usize = 8 * 1024 * 1024;
const ATTACH_PAGE_MAX_ENTRIES: usize = 8192;

/// Injected stall for the NEXT persisted-session attach of one id (test
/// seam; always `None` in production). Executed inside the blocking open
/// task, so a caller can prove the open does not block the async runtime.
static ATTACH_OPEN_STALL: std::sync::Mutex<Option<(String, std::time::Duration)>> =
    std::sync::Mutex::new(None);

/// Stall the next [`ServerState::attach_session`] reopen of `session_id`
/// (test/profiling seam). Matching on the id keeps concurrently running
/// tests from consuming each other's stall.
#[doc(hidden)]
pub fn stall_next_attach_open_for_test(session_id: impl Into<String>, stall: std::time::Duration) {
    let mut slot = ATTACH_OPEN_STALL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    *slot = Some((session_id.into(), stall));
}

fn take_attach_open_stall(session_id: &str) -> Option<std::time::Duration> {
    let mut slot = ATTACH_OPEN_STALL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    match slot.as_ref() {
        Some((id, _)) if id == session_id => slot.take().map(|(_, stall)| stall),
        _ => None,
    }
}

/// The prompt-run lifecycle of a session record: `idle` = no run in flight,
/// `turn` = a prompt run is in flight. The wire form is the `phase` string of
/// [`SessionSnapshot`](crate::protocol::SessionSnapshot) — the protocol keeps
/// the string shape, only the record's internal discrimination is typed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionPhase {
    /// No prompt run in flight.
    Idle,
    /// A prompt run is in flight (set when the run task spawns, cleared in
    /// [`ServerState::finish_prompt`]).
    Turn,
}

impl SessionPhase {
    /// The wire string carried in `SessionSnapshot.phase`.
    pub fn as_str(self) -> &'static str {
        match self {
            SessionPhase::Idle => "idle",
            SessionPhase::Turn => "turn",
        }
    }
}

/// An in-memory session record storing the data needed to build protocol snapshots.
/// (No `Clone`/`Debug`: the record owns the live [`AgentSession`].)
pub struct SessionRecord {
    /// The backend session state (entries, metadata, lanes).
    pub state: super::session_state::SessionState,
    /// Working directory at creation time.
    pub cwd: String,
    /// Model reference (default if not provided).
    pub model: ModelRef,
    /// Thinking level.
    pub thinking_level: String,
    /// Current phase.
    pub phase: SessionPhase,
    /// Connections holding attachments (upstream `HostedSession.attachments`:
    /// the same session may be attached by several clients at once; every
    /// attached connection fences its calls with its own attachment id).
    pub attachments: Vec<String>,
    /// Whether the session is locked.
    pub locked: bool,
    /// Runtime conversation context.
    pub context: Context,
    /// Steering prompts waiting for the next turn.
    pub queued_steer: Vec<String>,
    /// Abort signal for an active prompt. Armed at prompt start (before the
    /// run task attaches the agent), so abort/disconnect land even in the
    /// pre-attach window.
    pub abort_signal: Option<AbortSignal>,
    /// Whether the in-flight run was cancelled through the `cancel` frame
    /// (upstream `Server.handleCancel` aborting the identified request's
    /// controller). Unlike a plain `abort`, a cancelled run's terminal
    /// response reports the `cancelled` error (upstream server.ts:394-400)
    /// instead of the snapshot. Cleared when a run starts and consumed (with
    /// the rest of the run lifecycle) by [`ServerState::finish_prompt`].
    pub cancel_requested: bool,
    /// Monotonic sequence for snapshot `service_update` emissions.
    pub update_sequence: i64,
    /// The lazily built [`AgentSession`] driving prompts for this record.
    /// Rebuilt when the model or thinking level changes; prompts, steering,
    /// aborts, and threshold compaction all run through it.
    pub agent: Option<std::sync::Arc<crate::agent_session::AgentSession>>,
    /// Durable JSONL storage backing this session (upstream
    /// `SessionRouter` + `ServerHost.openSession` opening a persisted
    /// session). Held here until the first prompt moves it into the built
    /// agent; `None` when the server was bound without a session directory.
    pub storage: Option<SessionManager>,
    /// Shared handle on the storage currently inside [`Self::agent`]
    /// (`AgentSession::session_manager`). Kept alongside the agent so a
    /// rebuild (set_model/set_thinking) can recover the storage instead of
    /// abandoning the session file.
    pub persisted: Option<Arc<tokio::sync::Mutex<SessionManager>>>,
    /// Tool names a prompt in this session registers (the security policy
    /// decides at creation time; with `allow_remote_bash` off the shell tools
    /// `bash` and `powershell` are excluded).
    pub tool_names: Vec<String>,
}

impl SessionRecord {
    /// Build a new session record with the given id and default parameters.
    pub fn new(id: impl Into<String>, cwd: impl Into<String>, timestamp: u64) -> Self {
        let state = super::session_state::SessionState::new(id, timestamp);
        Self {
            state,
            cwd: cwd.into(),
            model: ModelRef {
                provider: "default".to_owned(),
                id: "default".to_owned(),
            },
            thinking_level: "off".to_owned(),
            phase: SessionPhase::Idle,
            attachments: Vec::new(),
            locked: false,
            context: Context::default(),
            queued_steer: Vec::new(),
            abort_signal: None,
            cancel_requested: false,
            update_sequence: 0,
            agent: None,
            storage: None,
            persisted: None,
            tool_names: Vec::new(),
        }
    }

    /// Build a protocol `SessionSnapshot` from this record.
    ///
    /// The wire `transcript` is intentionally empty: v8 delivers transcript
    /// items as incremental `progress` updates, and a snapshot is a lifecycle
    /// / queued-steer replacement only. A client that lost progress deltas
    /// cannot recover their content from this snapshot (there is no durable
    /// transcript projection here), so snapshots must not be described as a
    /// transcript replay.
    pub fn to_snapshot(&self) -> SessionSnapshot {
        SessionSnapshot {
            id: self.state.metadata.id.clone(),
            name: self.state.metadata.name.clone(),
            cwd: self.cwd.clone(),
            created_at: self.state.metadata.created_at as i64,
            updated_at: self.state.metadata.updated_at as i64,
            phase: self.phase.as_str().to_owned(),
            model: self.model.clone(),
            thinking_level: self.thinking_level.clone(),
            attached: !self.attachments.is_empty(),
            locked: self.locked,
            revision: self.update_sequence,
            transcript: Vec::<TranscriptItem>::new(),
            queued_steer: self
                .queued_steer
                .iter()
                .enumerate()
                .map(|(index, text)| UserTranscriptItem {
                    id: format!("steer-{index}"),
                    role: "user".to_owned(),
                    content: vec![crate::protocol::schemas::UserContent::Text(
                        crate::protocol::schemas::TextContent {
                            type_: "text".to_owned(),
                            text: text.clone(),
                        },
                    )],
                    timestamp: crate::ai::util::now_ms() as i64,
                })
                .collect(),
            queued_steer_count: self.queued_steer.len() as i64,
        }
    }

    /// Build a protocol `SessionMetadata` from this record.
    pub fn to_metadata(&self) -> SessionMetadata {
        SessionMetadata {
            id: self.state.metadata.id.clone(),
            created_at: self.state.metadata.created_at as i64,
            updated_at: Some(self.state.metadata.updated_at as i64),
            parent_session_id: None,
            session_name: self.state.metadata.name.clone(),
            cwd: Some(self.cwd.clone()),
        }
    }
}

/// Shared server state, accessible by all connections.
#[derive(Clone)]
pub struct ServerState {
    server_id: Arc<String>,
    snapshot: Arc<RwLock<ServerSnapshot>>,
    sessions: Arc<RwLock<HashMap<String, SessionRecord>>>,
    providers: Arc<ProviderRegistry>,
    security: ServerSecurityConfig,
    /// Host-resolved global tool selection (`--tools` / `--exclude-tools` /
    /// `--no-tools` / `--no-builtin-tools` plus `settings.defaultTools`).
    /// `None` preserves the historical registration-active set.
    ///
    /// Upstream has no server host; applying the CLI selection here is pir's
    /// consistency rule (the same contract print, the TUI and RPC honor), and
    /// the selection is resolved against the server's full core registry —
    /// the same built-ins every CLI host registers, with no extension tools —
    /// AFTER the `allow_remote_bash` shell policy removed `bash`/`powershell`
    /// ([`resolve_server_tool_selection`]).
    tool_selection: Option<crate::coding_agent::ToolSelection>,
    /// Directory backing session persistence ([`Self::with_session_dir`]).
    /// `None` keeps the historical in-memory-only registry.
    sessions_dir: Option<Arc<std::path::PathBuf>>,
    /// Draining flag (upstream `SessionRouterOptions.isClosing`): once set,
    /// new creates/attaches/prompts are rejected with `server_draining` and
    /// in-flight runs are aborted.
    draining: Arc<AtomicBool>,
    /// Wire vocabulary used for published pir.session `state` updates. Delta
    /// (the chord vocabulary) is the default; [`StateUpdateFormat::Legacy`]
    /// keeps the historical `{op, value}` objects for pinned clients.
    state_update_format: StateUpdateFormat,
    pub(super) updates: broadcast::Sender<Value>,
}

impl ServerState {
    /// Construct state from an initial server snapshot.
    pub fn new(snapshot: ServerSnapshot) -> Self {
        let (updates, _) = broadcast::channel(UPDATE_CHANNEL_CAPACITY);
        let providers = Arc::new(ProviderRegistry::default());
        // Remote catalog overlays (upstream model-runtime `withRemoteCatalog`):
        // background ETag revalidation, delayed first hop, PI_OFFLINE-gated.
        // Spawns outside a tokio runtime degrade to a 0-count (never a panic).
        crate::ai::providers::install_remote_catalog_overlays(&providers);
        Self {
            server_id: Arc::new(snapshot.server_id.clone()),
            snapshot: Arc::new(RwLock::new(snapshot)),
            sessions: Arc::new(RwLock::new(HashMap::new())),
            providers,
            security: ServerSecurityConfig::default(),
            tool_selection: None,
            sessions_dir: None,
            draining: Arc::new(AtomicBool::new(false)),
            state_update_format: StateUpdateFormat::default(),
            updates,
        }
    }

    /// Construct state from an initial snapshot and a prebuilt provider
    /// registry (embedder/test seam: the registry must still be mutable, so
    /// providers are attached before the state is shared).
    pub fn with_providers(snapshot: ServerSnapshot, providers: Arc<ProviderRegistry>) -> Self {
        let (updates, _) = broadcast::channel(UPDATE_CHANNEL_CAPACITY);
        Self {
            server_id: Arc::new(snapshot.server_id.clone()),
            snapshot: Arc::new(RwLock::new(snapshot)),
            sessions: Arc::new(RwLock::new(HashMap::new())),
            providers,
            security: ServerSecurityConfig::default(),
            tool_selection: None,
            sessions_dir: None,
            draining: Arc::new(AtomicBool::new(false)),
            state_update_format: StateUpdateFormat::default(),
            updates,
        }
    }

    /// Back sessions with durable JSONL files under `dir` (upstream
    /// `SessionRouter` over a `ServerHost` that opens persisted sessions):
    /// created sessions carry a `crate::session_manager::SessionManager`
    /// file (lazy header, written with the first message), `list` merges the
    /// directory's on-disk sessions, and attaching an on-disk session reopens
    /// it — so sessions survive disconnects and server restarts.
    pub fn with_session_dir(mut self, dir: impl Into<std::path::PathBuf>) -> Self {
        self.sessions_dir = Some(Arc::new(dir.into()));
        self
    }

    /// Whether the server is draining (no new sessions/attaches/prompts).
    pub fn is_draining(&self) -> bool {
        self.draining.load(Ordering::SeqCst)
    }

    /// Begin draining (upstream `SessionRouter.close` semantics, minus the
    /// handle teardown): reject new creates/attaches/prompts with
    /// `server_draining` and abort every in-flight prompt run. The persisted
    /// session files stay in place.
    pub async fn begin_drain(&self) {
        self.draining.store(true, Ordering::SeqCst);
        // Abort first under the read lock (unchanged semantics), then run the
        // graceful plugin teardown (`AgentSession::shutdown` → every plugin's
        // `on_shutdown`, the MCP stdio close chain) outside the lock so the
        // drain path matches the TUI worker-exit / child-run-end teardown.
        let agents = {
            let sessions = self.sessions.read().await;
            for record in sessions.values() {
                if let Some(signal) = &record.abort_signal {
                    signal.abort();
                }
                if let Some(agent) = &record.agent {
                    agent.abort();
                }
            }
            sessions
                .values()
                .filter_map(|record| record.agent.clone())
                .collect::<Vec<_>>()
        };
        for agent in &agents {
            agent.shutdown().await;
        }
    }

    /// Apply the security/limit policy that governs this state's sessions
    /// (bash exclusion, session/subscription caps, cwd policy). Called by the
    /// TCP host at bind time; library users building a `ServerState` directly
    /// keep the defaults.
    pub fn with_security(mut self, security: ServerSecurityConfig) -> Self {
        self.security = security;
        self
    }

    /// Apply the host-resolved global tool selection
    /// ([`resolve_server_tool_selection`] over this server's policy-filtered
    /// core registry). `None` keeps the historical registration-active set.
    ///
    /// Upstream has no server host; this is a pir consistency rule, not
    /// parity: `pir server` honors the same global flags as print, the TUI and
    /// RPC, and the selection is never allowed to re-introduce a shell tool
    /// the `allow_remote_bash` policy removed.
    pub fn with_tool_selection(
        mut self,
        selection: Option<crate::coding_agent::ToolSelection>,
    ) -> Self {
        self.tool_selection = selection;
        self
    }

    /// Select the wire vocabulary of the published pir.session `state`
    /// updates (progress and snapshot members). The default is
    /// [`StateUpdateFormat::Delta`], the chord tuple vocabulary upstream
    /// clients decode; [`StateUpdateFormat::Legacy`] keeps the historical
    /// `{op, value}` objects for clients pinned to that shape. Both are
    /// valid v8 payloads and a client using
    /// [`crate::protocol::StateUpdateDecoder`] consumes either one.
    pub fn with_state_update_format(mut self, format: StateUpdateFormat) -> Self {
        self.state_update_format = format;
        self
    }

    /// Shortcut for [`Self::with_state_update_format`] with
    /// [`StateUpdateFormat::Legacy`] (compatibility mode).
    pub fn with_legacy_state_updates(self) -> Self {
        self.with_state_update_format(StateUpdateFormat::Legacy)
    }

    /// The vocabulary this state publishes `state` updates in.
    pub fn state_update_format(&self) -> StateUpdateFormat {
        self.state_update_format
    }

    /// The logical server id echoed in the v8 hello and required in targets.
    pub fn server_id(&self) -> &str {
        &self.server_id
    }

    /// Return a consistent copy of the current compatibility snapshot.
    pub async fn snapshot(&self) -> ServerSnapshot {
        self.snapshot.read().await.clone()
    }

    /// Return the current session map (for building snapshot metadata).
    ///
    /// When a session directory is bound, on-disk sessions not currently held
    /// in memory are merged in (upstream `resolveSession` over durable
    /// metadata), so sessions survive disconnects and server restarts.
    ///
    /// The registry lock is held only to SNAPSHOT the in-memory metadata: the
    /// directory scan is a blocking read of every session file's header/name
    /// and runs on the blocking pool outside the lock, so one slow directory
    /// cannot stall creates/attaches/prompts. The scan uses
    /// [`SessionManager::list_session_headers`] (header + session_info name +
    /// mtime) — never the message peek that builds a search corpus the server
    /// does not consume.
    pub async fn session_metadata(&self) -> Vec<SessionMetadata> {
        let mut meta: Vec<SessionMetadata> = {
            let sessions = self.sessions.read().await;
            sessions.values().map(|r| r.to_metadata()).collect()
        };
        if let Some(dir) = &self.sessions_dir {
            let known: std::collections::HashSet<String> =
                meta.iter().map(|metadata| metadata.id.clone()).collect();
            let dir = Arc::clone(dir);
            let listing = match tokio::runtime::Handle::try_current() {
                Ok(handle) => handle
                    .spawn_blocking(move || SessionManager::list_session_headers(&dir, None))
                    .await
                    .unwrap_or_default(),
                // A caller driving this future outside a Tokio runtime keeps
                // the historical synchronous behavior (there is no runtime
                // worker to park).
                Err(_) => SessionManager::list_session_headers(&dir, None),
            };
            for summary in listing {
                if known.contains(&summary.id) {
                    continue;
                }
                let modified_ms = summary
                    .modified
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|duration| duration.as_millis() as i64)
                    .unwrap_or(0);
                meta.push(SessionMetadata {
                    id: summary.id,
                    created_at: modified_ms,
                    updated_at: Some(modified_ms),
                    parent_session_id: None,
                    session_name: summary.name,
                    cwd: Some(summary.cwd),
                });
            }
        }
        meta.sort_by_key(|b| std::cmp::Reverse(b.created_at));
        meta
    }

    /// Build the full snapshot `state` update of EVERY live session, in the
    /// configured vocabulary. This is the recovery payload for a connection's
    /// update forwarder that fell behind the broadcast ring
    /// (`broadcast::RecvError::Lagged`): snapshot-level state (phase, revision,
    /// queued steer) is a full replacement, so a fresh snapshot per
    /// subscribed session can restore it. Transcript items are delivered as
    /// incremental progress updates and are NOT part of a snapshot — a client
    /// that lost those deltas cannot rebuild the transcript from here.
    pub(super) async fn snapshot_updates(&self) -> Vec<Value> {
        let sessions = self.sessions.read().await;
        sessions
            .values()
            .map(|record| {
                session_snapshot_update_in(
                    &record.state.metadata.id,
                    &record.to_snapshot(),
                    record.update_sequence,
                    self.state_update_format,
                )
            })
            .collect()
    }

    /// The tool names a prompt in the given session activates. `None` when
    /// the session does not exist. With `allow_remote_bash` off (the default)
    /// the registry carries neither shell-execution tool (`bash` nor
    /// `powershell`) — the fail-closed remote-execution control.
    ///
    /// When the server state carries a global tool selection
    /// ([`ServerState::with_tool_selection`]), the names are the selection's
    /// active set filtered to the registry this server actually builds; with
    /// no selection they are every registered name (the historical result,
    /// `powershell` included when `allow_remote_bash` is on).
    pub async fn session_tool_names(&self, session_id: &str) -> Option<Vec<String>> {
        let sessions = self.sessions.read().await;
        sessions
            .get(session_id)
            .map(|record| record.tool_names.clone())
    }

    /// Broadcast one session service update to subscribed connections.
    fn publish_session_update(&self, update: Value) {
        let _ = self.updates.send(update);
    }

    /// Build and broadcast a snapshot `state` update for one session in the
    /// configured vocabulary (delta by default — see
    /// [`Self::with_state_update_format`]).
    ///
    /// Every snapshot update is a full replacement rather than an incremental
    /// diff: the control service does not replay the current state on
    /// subscribe (`instances` is empty), so a subscriber that joins mid-stream
    /// must be able to render the first update it receives.
    fn publish_snapshot(&self, session_id: &str, record: &SessionRecord) {
        let update = session_snapshot_update_in(
            session_id,
            &record.to_snapshot(),
            record.update_sequence,
            self.state_update_format,
        );
        self.publish_session_update(update);
    }

    /// Broadcast the `closed` service-provider update for one removed or
    /// invalidated session. The shape is the registered upstream vocabulary
    /// (`parse_service_provider_update` "closed" with the instance address —
    /// `{"type":"closed","instance":{key,generation}}`), matching the
    /// instance key/generation the progress and snapshot updates already
    /// carry. This is pir's counterpart of upstream `invalidate`
    /// (session-router.ts:302-311) notifying attachments that their session
    /// route died; the same helper is the hook for future abnormal
    /// invalidations (there is no async `handle.terminated` signal in pir —
    /// the durable file is the recovery source, so a session dies only by
    /// removal today).
    fn publish_session_closed(&self, session_id: &str) {
        let update = serde_json::json!({
            "type": "closed",
            "instance": {"key": session_id, "generation": 1},
        });
        self.publish_session_update(update);
    }

    fn next_sequence(record: &mut SessionRecord) {
        record.update_sequence += 1;
    }

    /// Recover the durable storage from the record's agent ahead of an agent
    /// rebuild (set_model/set_thinking/prompt). The agent owns the moved
    /// [`SessionManager`]; dropping the agent releases its shared handle, and
    /// when this record holds the last one the manager is unwrapped back into
    /// [`SessionRecord::storage`]. If another task still holds a handle (a
    /// run task that has not fully returned yet), the shared handle is kept
    /// for a retry at the next rebuild and the rebuild continues without
    /// storage (fail-soft, logged).
    fn recover_persisted_storage(record: &mut SessionRecord) {
        if record.storage.is_some() {
            return;
        }
        let Some(shared) = record.persisted.take() else {
            return;
        };
        // Drop the agent first so it releases its handle on the storage.
        record.agent = None;
        match Arc::try_unwrap(shared) {
            // tokio's Mutex does not poison: into_inner hands the storage back.
            Ok(mutex) => record.storage = Some(mutex.into_inner()),
            Err(shared) => {
                record.persisted = Some(shared);
                tracing::warn!(
                    session_id = %record.state.metadata.id,
                    "persisted server session storage is still shared; \
                     the next agent rebuild retries the recovery"
                );
            }
        }
    }

    /// Rebuild the server snapshot's session list from the registry.
    async fn refresh_snapshot_sessions(&self, sessions: &HashMap<String, SessionRecord>) {
        let mut snap = self.snapshot.write().await;
        let mut meta_list: Vec<SessionMetadata> = sessions
            .values()
            .map(|record| record.to_metadata())
            .collect();
        meta_list.sort_by_key(|b| std::cmp::Reverse(b.created_at));
        snap.revision += 1;
        snap.sessions = meta_list;
    }

    /// Create a new session and return its snapshot.
    ///
    /// Fails with [`SessionCommandError::Limit`] when the session cap is
    /// reached: sessions are released from the registry when their last
    /// attachment disconnects, but an unbounded registry would otherwise grow
    /// without bound under unauthenticated creates. When a session directory
    /// is bound, the session is backed by a durable JSONL file; a storage
    /// failure degrades to the in-memory registry (logged, never fatal).
    pub async fn create_session(
        &self,
        id: String,
        cwd: Option<String>,
        name: Option<String>,
        model: Option<ModelRef>,
        thinking_level: Option<String>,
        timestamp: u64,
    ) -> Result<SessionSnapshot, SessionCommandError> {
        if self.is_draining() {
            return Err(SessionCommandError::Draining);
        }
        let cwd = cwd.unwrap_or_else(|| ".".to_owned());
        let mut record = SessionRecord::new(&id, cwd.clone(), timestamp);
        record.state.metadata.name = name;
        if let Some(m) = model {
            record.model = m;
        }
        if let Some(tl) = thinking_level {
            record.thinking_level = tl;
        }
        record.tool_names = self.resolved_tool_names();
        if let Some(dir) = &self.sessions_dir {
            match SessionManager::create_with_id(&cwd, dir, &id) {
                Ok(manager) => record.storage = Some(manager),
                Err(error) => {
                    tracing::warn!(
                        %error,
                        session_id = %id,
                        "server session persistence unavailable; continuing in memory"
                    );
                }
            }
        }
        let snapshot = record.to_snapshot();
        // Insert into session map, enforcing the cap BEFORE any publish: a
        // rejected create must not broadcast a snapshot update (nor bump the
        // sequence) for a session that was never admitted.
        {
            let mut sessions = self.sessions.write().await;
            if sessions.len() >= self.security.max_sessions {
                return Err(SessionCommandError::Limit);
            }
            Self::next_sequence(&mut record);
            self.publish_snapshot(&id, &record);
            sessions.insert(id, record);
        }

        // Update the server snapshot (rebuild + sort, like every other
        // registry mutation).
        {
            let sessions = self.sessions.read().await;
            self.refresh_snapshot_sessions(&sessions).await;
        }

        Ok(snapshot)
    }

    /// Look up a session by id and return its snapshot, or `None` if not found.
    pub async fn get_session(&self, id: &str) -> Option<SessionSnapshot> {
        let sessions = self.sessions.read().await;
        sessions.get(id).map(|r| r.to_snapshot())
    }

    /// Attach a connection to a session. Like upstream
    /// `SessionRouter.attachClientNow`, one session may carry SEVERAL
    /// attachments: each connection fences its calls with its own attachment
    /// id (its connection id) and events broadcast to every subscriber.
    ///
    /// A session that is not held in memory but exists in the bound session
    /// directory is reopened from its JSONL file first (restart recovery,
    /// upstream `ServerHost.openSession` over durable metadata) — the agent
    /// rebuilds its context from the persisted transcript.
    pub async fn attach_session(
        &self,
        id: &str,
        connection_id: &str,
    ) -> Result<SessionSnapshot, SessionCommandError> {
        if self.is_draining() {
            return Err(SessionCommandError::Draining);
        }
        if !self.sessions.read().await.contains_key(id) {
            self.open_persisted_session(id).await?;
        }
        let mut sessions = self.sessions.write().await;
        let record = sessions.get_mut(id).ok_or(SessionCommandError::NotFound)?;
        if !record.attachments.iter().any(|a| a == connection_id) {
            record.attachments.push(connection_id.to_owned());
        }
        Self::next_sequence(record);
        let snapshot = record.to_snapshot();
        self.publish_snapshot(id, record);
        Ok(snapshot)
    }

    /// Reopen a session from the bound session directory into the registry
    /// (no-op when the server has no directory, or the id is unknown/the file
    /// cannot be read — attach then answers `not_found` as before).
    ///
    /// The directory scan, the file open, the paged widen, and the branch
    /// projection are blocking work over a transcript-sized file, so they all
    /// run in ONE blocking task; the registry write lock is taken only for the
    /// admission check and insert. The admission reuses the same session cap as
    /// [`Self::create_session`], so attaching a directory of persisted
    /// sessions cannot bypass `max_sessions`.
    async fn open_persisted_session(&self, id: &str) -> Result<(), SessionCommandError> {
        let Some(dir) = &self.sessions_dir else {
            return Ok(());
        };
        let dir = Arc::clone(dir);
        let registry = Arc::clone(&self.providers);
        let id_owned = id.to_owned();
        let stall = take_attach_open_stall(id);
        let prepared = match tokio::runtime::Handle::try_current() {
            Ok(handle) => handle
                .spawn_blocking(move || {
                    prepare_persisted_session(&dir, &id_owned, registry.as_ref(), stall)
                })
                .await
                .unwrap_or(None),
            // Outside a Tokio runtime there is no worker to block; keep the
            // historical synchronous behavior.
            Err(_) => prepare_persisted_session(&dir, id, registry.as_ref(), stall),
        };
        let Some(prepared) = prepared else {
            return Ok(());
        };
        let mut record = SessionRecord::new(id, prepared.cwd, crate::ai::util::now_ms());
        record.state.metadata.name = prepared.name;
        record.tool_names = self.resolved_tool_names();
        if let Some((provider, model_id)) = prepared.model {
            record.model = ModelRef {
                provider,
                id: model_id,
            };
        }
        record.thinking_level = prepared.thinking_level;
        record.storage = Some(prepared.manager);
        {
            let mut sessions = self.sessions.write().await;
            if sessions.contains_key(id) {
                // Lost a race with another attacher; the in-memory record wins.
                return Ok(());
            }
            // Same admission rule as `create_session`: a persisted session
            // attached into memory counts toward the cap. Checked with the
            // same write lock that inserts it (no check/insert window).
            if sessions.len() >= self.security.max_sessions {
                return Err(SessionCommandError::Limit);
            }
            Self::next_sequence(&mut record);
            self.publish_snapshot(id, &record);
            sessions.insert(id.to_owned(), record);
        }
        // Snapshot rebuild runs OUTSIDE the write guard (tokio's RwLock is not
        // reentrant: read-locking inside the write lock would self-deadlock —
        // the same block structure create_session uses).
        {
            let sessions = self.sessions.read().await;
            self.refresh_snapshot_sessions(&sessions).await;
        }
        Ok(())
    }

    /// The tool names a prompt in this server's sessions activates: the
    /// global selection filtered to the registry this server actually builds
    /// (`active_names`), else every registered name (historical behavior).
    fn resolved_tool_names(&self) -> Vec<String> {
        match self.tool_selection.as_ref() {
            // The active/declared set, filtered to what this server
            // registers: a selected `bash` under `allow_remote_bash: false`
            // is not in the registry and therefore not reported active.
            Some(selection) => session_tool_registry(self.security.allow_remote_bash)
                .active_names(Some(&selection.enabled)),
            // No selection: every name of the full core registry (the same
            // set the CLI hosts register).
            None => server_tool_names(self.security.allow_remote_bash),
        }
    }

    pub async fn detach_session(
        &self,
        id: &str,
        connection_id: &str,
    ) -> Result<SessionSnapshot, SessionCommandError> {
        let mut sessions = self.sessions.write().await;
        let record = sessions.get_mut(id).ok_or(SessionCommandError::NotFound)?;
        ensure_attached(record, connection_id)?;
        if record.phase != SessionPhase::Idle {
            return Err(SessionCommandError::Busy);
        }
        record.attachments.retain(|a| a != connection_id);
        let last = record.attachments.is_empty();
        if last {
            // Mirror `disconnect`'s last-attachment path: an explicitly
            // detached session must not keep its agent/background work or a
            // registry slot alive forever.
            if let Some(signal) = &record.abort_signal {
                signal.abort();
            }
            if let Some(agent) = &record.agent {
                agent.abort();
            }
            record.abort_signal = None;
            record.phase = SessionPhase::Idle;
        }
        Self::next_sequence(record);
        let snapshot = record.to_snapshot();
        self.publish_snapshot(id, record);
        if last {
            sessions.remove(id);
        }
        drop(sessions);
        if last {
            let sessions = self.sessions.read().await;
            self.refresh_snapshot_sessions(&sessions).await;
        }
        Ok(snapshot)
    }

    /// Remove a session entirely (pir extension member `remove_session`).
    ///
    /// Upstream `SessionRouter.removeSession` (session-router.ts:72-89) is a
    /// SERVER-INTERNAL method: the v8 wire protocol carries no removeSession
    /// command (protocol.ts only frames hello/request/cancel), so pir adds it
    /// as an ADDITIVE `pir.session` member without bumping
    /// [`PROTOCOL_VERSION`](crate::protocol::schemas::PROTOCOL_VERSION) — a new
    /// call member changes no existing wire semantics.
    ///
    /// Semantics (fencing reused from every other session call): the caller
    /// must hold an attachment on the session, any in-flight run is aborted
    /// first, the durable JSONL file is deleted, the in-memory record is
    /// evicted, and a `closed` service update is broadcast to subscribers (the
    /// invalidation propagation of upstream `handle.terminated` →
    /// `invalidate` releasing every attachment, session-router.ts:293-311;
    /// pir approximates "terminated" with the disk file being the recovery
    /// source, so removal takes the file with it). Deletion runs BEFORE the
    /// eviction/broadcast because it is the only irreversible step: a failed
    /// delete leaves the session fully intact (registry + file) and the client
    /// sees the Storage error without having received a `closed` update for a
    /// session that would re-attach as if it were never deleted. A run task
    /// that settles after the eviction finds no record and answers
    /// `not_found`; its late storage append can at worst recreate an empty
    /// file — the same close-race upstream tolerates between `removeSession`
    /// and in-flight operations.
    pub async fn remove_session(
        &self,
        id: &str,
        connection_id: &str,
    ) -> Result<(), SessionCommandError> {
        if self.is_draining() {
            return Err(SessionCommandError::Draining);
        }
        {
            let mut sessions = self.sessions.write().await;
            let Some(record) = sessions.get_mut(id) else {
                return Err(SessionCommandError::NotFound);
            };
            ensure_attached(record, connection_id)?;
            // Abort the in-flight run BEFORE anything else (unchanged
            // semantics): the run task observes the signal and settles
            // promptly.
            if let Some(signal) = &record.abort_signal {
                signal.abort();
            }
            if let Some(agent) = &record.agent {
                agent.abort();
            }
        }
        // Delete the durable JSONL file outside the registry lock (I/O must
        // not stall other session commands) and before the eviction: on
        // failure the session stays live in memory and on disk. The file is
        // located by header id so a storage handle parked inside a run task's
        // agent does not matter.
        if let Some(dir) = &self.sessions_dir {
            if let Some(path) = SessionManager::find_session_by_id(dir, id) {
                if let Err(error) = std::fs::remove_file(&path) {
                    tracing::warn!(
                        %error,
                        session_id = %id,
                        path = %path.display(),
                        "removed session file could not be deleted"
                    );
                    return Err(SessionCommandError::Storage(error.to_string()));
                }
            }
        }
        {
            let mut sessions = self.sessions.write().await;
            sessions.remove(id);
        }
        // Invalidation broadcast: every attached connection's subscription
        // forwarder turns this into a `service_update` frame (upstream
        // `invalidate` releasing every attachment).
        self.publish_session_closed(id);
        {
            let sessions = self.sessions.read().await;
            self.refresh_snapshot_sessions(&sessions).await;
        }
        Ok(())
    }

    /// Release the disconnected connection's attachments. A session whose
    /// LAST attachment went away aborts any in-flight run and leaves the
    /// registry (its durable JSONL file — when the server persists — keeps
    /// the conversation for a later attach or restart); sessions still held
    /// by other attachments are untouched (upstream `disconnect` releasing
    /// only the client's own attachment).
    pub async fn disconnect(&self, connection_id: &str) {
        let attached: Vec<String> = {
            let sessions = self.sessions.read().await;
            sessions
                .values()
                .filter(|record| record.attachments.iter().any(|a| a == connection_id))
                .map(|record| record.state.metadata.id.clone())
                .collect()
        };
        if attached.is_empty() {
            return;
        }
        let mut sessions = self.sessions.write().await;
        for id in attached {
            let Some(mut record) = sessions.remove(&id) else {
                continue;
            };
            record.attachments.retain(|a| a != connection_id);
            if record.attachments.is_empty() {
                // Last attachment gone: abort the in-flight run and release
                // the record. Persistence (per-append, fsynced) has already
                // written every message, so there is nothing left to flush.
                if let Some(signal) = &record.abort_signal {
                    signal.abort();
                }
                if let Some(agent) = &record.agent {
                    agent.abort();
                }
                record.abort_signal = None;
                record.phase = SessionPhase::Idle;
                Self::next_sequence(&mut record);
                self.publish_snapshot(&record.state.metadata.id, &record);
            } else {
                // Other attachments survive the disconnect.
                Self::next_sequence(&mut record);
                self.publish_snapshot(&record.state.metadata.id, &record);
                sessions.insert(id, record);
            }
        }
        self.refresh_snapshot_sessions(&sessions).await;
    }

    pub async fn set_model(
        &self,
        id: &str,
        connection_id: &str,
        model: ModelRef,
    ) -> Result<SessionSnapshot, SessionCommandError> {
        let mut sessions = self.sessions.write().await;
        let record = sessions.get_mut(id).ok_or(SessionCommandError::NotFound)?;
        ensure_attached(record, connection_id)?;
        ensure_idle(record)?;
        record.model = model;
        // The cached agent session is pinned to the model it was built with;
        // recover its durable storage first so the next prompt's rebuild
        // reopens the same session file, then drop the agent.
        Self::recover_persisted_storage(record);
        Self::next_sequence(record);
        let snapshot = record.to_snapshot();
        self.publish_snapshot(id, record);
        Ok(snapshot)
    }

    pub async fn set_thinking(
        &self,
        id: &str,
        connection_id: &str,
        level: String,
    ) -> Result<SessionSnapshot, SessionCommandError> {
        let mut sessions = self.sessions.write().await;
        let record = sessions.get_mut(id).ok_or(SessionCommandError::NotFound)?;
        ensure_attached(record, connection_id)?;
        ensure_idle(record)?;
        record.thinking_level = level;
        // Same rebuild rule as set_model: the agent bakes the thinking level
        // into its stream options. Recover the durable storage first so the
        // rebuild keeps the session file.
        Self::recover_persisted_storage(record);
        Self::next_sequence(record);
        let snapshot = record.to_snapshot();
        self.publish_snapshot(id, record);
        Ok(snapshot)
    }

    pub async fn steer(
        &self,
        id: &str,
        connection_id: &str,
        text: String,
    ) -> Result<SessionSnapshot, SessionCommandError> {
        let mut sessions = self.sessions.write().await;
        let record = sessions.get_mut(id).ok_or(SessionCommandError::NotFound)?;
        ensure_attached(record, connection_id)?;
        if record.phase == SessionPhase::Idle {
            return Err(SessionCommandError::Invalid(
                "steer requires an active prompt".to_owned(),
            ));
        }
        // The run task attaches the agent shortly after the prompt begins;
        // steering landing in that window queues for the run's inbox (the run
        // drains the queue right after attaching). Once attached, steering
        // goes straight into the agent's inbox and is consumed before the
        // next model request (upstream semantics).
        match &record.agent {
            Some(agent) => {
                let _ = agent.steer(&text);
            }
            None => record.queued_steer.push(text),
        }
        Self::next_sequence(record);
        let snapshot = record.to_snapshot();
        self.publish_snapshot(id, record);
        Ok(snapshot)
    }

    pub async fn abort(
        &self,
        id: &str,
        connection_id: &str,
    ) -> Result<SessionSnapshot, SessionCommandError> {
        let mut sessions = self.sessions.write().await;
        let record = sessions.get_mut(id).ok_or(SessionCommandError::NotFound)?;
        ensure_attached(record, connection_id)?;
        // The in-flight run observes the abort through the agent session's
        // signal (loop + tools + extension calls) and, before the agent is
        // attached, through the record's run signal (checked by the run task
        // after attaching and by its select loop).
        if let Some(agent) = &record.agent {
            agent.abort();
        }
        if let Some(signal) = &record.abort_signal {
            signal.abort();
        }
        Ok(record.to_snapshot())
    }

    /// Cancel one in-flight prompt run — the `cancel` frame path (upstream
    /// `Server.handleCancel`, server.ts:298-304). Same fencing and abort
    /// routing as [`ServerState::abort`], but the run is additionally marked
    /// [`SessionRecord::cancel_requested`] so its deferred response settles
    /// with the `cancelled` error (upstream server.ts:394-400) instead of
    /// the snapshot a plain abort returns.
    pub async fn cancel(
        &self,
        id: &str,
        connection_id: &str,
    ) -> Result<SessionSnapshot, SessionCommandError> {
        let mut sessions = self.sessions.write().await;
        let record = sessions.get_mut(id).ok_or(SessionCommandError::NotFound)?;
        ensure_attached(record, connection_id)?;
        record.cancel_requested = true;
        if let Some(agent) = &record.agent {
            agent.abort();
        }
        if let Some(signal) = &record.abort_signal {
            signal.abort();
        }
        Ok(record.to_snapshot())
    }

    /// Begin one prompt run through this session's [`AgentSession`] — the
    /// same orchestration facade the TUI and RPC hosts drive (tools,
    /// steering, threshold compaction, persistence-shaped transcript).
    ///
    /// The call validates the request, flips the phase to `turn`, arms the
    /// run's abort signal, and **spawns the run as a per-session task** before
    /// returning. The run's outcome is delivered through `finished` (the
    /// caller turns it into the response frame with the matching request id),
    /// which keeps the owning connection's read loop free so abort/steer/
    /// cancel frames are processed while the run is in flight.
    ///
    /// No provider is treated as a protocol error, not a fake success.
    pub async fn prompt(
        &self,
        id: &str,
        connection_id: &str,
        text: String,
        finished: oneshot::Sender<Result<SessionSnapshot, SessionCommandError>>,
    ) -> Result<(), SessionCommandError> {
        if self.is_draining() {
            return Err(SessionCommandError::Draining);
        }
        let signal = AbortSignal::new();
        let (queued, model_ref) = {
            let mut sessions = self.sessions.write().await;
            let record = sessions.get_mut(id).ok_or(SessionCommandError::NotFound)?;
            ensure_attached(record, connection_id)?;
            ensure_idle(record)?;
            // The phase transition is broadcast immediately so subscribers
            // observe the turn lifecycle, and it doubles as the re-entry
            // guard: a second prompt while the run task is in flight fails
            // with `busy` (the phase flips back in `finish_prompt`).
            record.phase = SessionPhase::Turn;
            // Arm the run's abort signal before spawning: abort/disconnect
            // that land before the agent attaches still stop the run. A new
            // run starts uncancelled (a stale cancel cannot leak into it).
            record.abort_signal = Some(signal.clone());
            record.cancel_requested = false;
            let queued = std::mem::take(&mut record.queued_steer);
            Self::next_sequence(record);
            self.publish_snapshot(id, record);
            (queued, record.model.clone())
        };
        let state = self.clone();
        let id = id.to_owned();
        let connection_id = connection_id.to_owned();
        tokio::spawn(async move {
            let outcome = state
                .run_prompt(&id, &connection_id, text, queued, model_ref, signal)
                .await;
            let _ = finished.send(outcome);
        });
        Ok(())
    }

    /// Drive one prompt run to completion (runs as a spawned task; see
    /// [`ServerState::prompt`]).
    async fn run_prompt(
        &self,
        id: &str,
        _connection_id: &str,
        text: String,
        queued: Vec<String>,
        model_ref: ModelRef,
        signal: AbortSignal,
    ) -> Result<SessionSnapshot, SessionCommandError> {
        // Resolve (or build) the run's agent and drain the attach-window
        // steering queue under ONE write lock. `steer` sends straight to the
        // agent inbox as soon as `record.agent` is visible, so taking the
        // queue here keeps every text that arrived BEFORE the attach ahead of
        // the texts that arrive after it; two lock sections let a later
        // direct steer overtake the queued ones.
        let (attached_agent, queued_while_attaching) = {
            let mut sessions = self.sessions.write().await;
            let Some(record) = sessions.get_mut(id) else {
                return Err(SessionCommandError::NotFound);
            };
            let agent = match &record.agent {
                Some(agent) => Some(agent.clone()),
                None => match self.providers.get_model(&model_ref.provider, &model_ref.id) {
                    Some(model) => {
                        // A rebuild after set_model/set_thinking must pick the
                        // durable storage back up before the old agent's
                        // handle is gone.
                        Self::recover_persisted_storage(record);
                        let agent = Arc::new(build_session_agent(
                            self.providers.clone(),
                            model,
                            &record.thinking_level,
                            &record.cwd,
                            self.security.allow_remote_bash,
                            record.storage.take(),
                            self.tool_selection.clone(),
                        ));
                        record.agent = Some(agent.clone());
                        record.persisted = agent.session_manager();
                        Some(agent)
                    }
                    None => None,
                },
            };
            let queued_while_attaching = if agent.is_some() {
                std::mem::take(&mut record.queued_steer)
            } else {
                // The provider-missing path restores the whole queue below;
                // nothing has been handed to an agent.
                Vec::new()
            };
            (agent, queued_while_attaching)
        };
        let Some(agent) = attached_agent else {
            // The run never reached the model: steering that was queued for it
            // must not silently vanish, so it goes back to the session queue
            // (in prompt order, before anything that arrived meanwhile) for the
            // next prompt. `finish_prompt(preserve_steering = true)` keeps it.
            {
                let mut sessions = self.sessions.write().await;
                if let Some(record) = sessions.get_mut(id) {
                    let mut restored = queued;
                    restored.append(&mut record.queued_steer);
                    record.queued_steer = restored;
                }
            }
            self.finish_prompt(id, true).await;
            return Err(SessionCommandError::Provider(format!(
                "provider/model is not configured: {}/{}",
                model_ref.provider, model_ref.id
            )));
        };
        // Steering queued between the prompt beginning and the agent
        // attaching (fresh-session edge) joins the run's inbox before the
        // first model request, in prompt order: the texts `prompt()` took
        // before spawning are older than the attach-window ones. The texts
        // are NOT tracked for re-queueing: the inbox owns both delivery and
        // the undelivered remainder across runs, so a failed run neither
        // loses nor re-delivers them.
        let mut run_steers = queued;
        run_steers.extend(queued_while_attaching);
        for steer in &run_steers {
            agent.steer(steer);
        }
        // An abort armed before the agent attached must not start the run.
        if signal.is_aborted() {
            agent.abort();
        }
        let events = self.progress_handler(id);
        let run = agent.prompt(&text, events);
        tokio::pin!(run);
        // The record's signal (abort/disconnect before or during the run)
        // stops the agent run; the run then finishes on its own. The abort
        // branch is armed ONCE: `AbortSignal::cancelled` resolves immediately
        // and stays ready after the token fires, so re-arming it under
        // `biased` would starve the run poll forever (a hot spin that never
        // delivered the terminal response and left the phase at `turn`).
        // After the first fire the branch is disabled and only the run is
        // polled until it settles. The future is pinned outside the loop, so
        // an abort wakes the select immediately instead of at a poll tick.
        let abort = signal.cancelled();
        tokio::pin!(abort);
        let mut abort_armed = true;
        let terminal = loop {
            tokio::select! {
                biased;
                outcome = &mut run => break outcome,
                _ = &mut abort, if abort_armed => {
                    abort_armed = false;
                    agent.abort();
                }
            }
        };
        let terminal = terminal.map_err(|error| SessionCommandError::Provider(error.to_string()));
        let terminal = match terminal {
            Ok(terminal) => terminal,
            Err(error) => {
                // The run reached the agent, so every steer was handed to its
                // inbox. Whatever the agent did not consume stays queued there
                // and is delivered by the next run; re-adding the texts to the
                // session queue would replay the consumed ones and duplicate
                // the still-pending ones.
                self.finish_prompt(id, false).await;
                return Err(error);
            }
        };
        let failed = terminal.stop_reason == StopReason::Error;
        // A drain-induced abort reports `server_draining` instead of a plain
        // abort success (upstream `ServerDrainingError` on the in-flight
        // operation).
        if self.is_draining() && signal.is_aborted() {
            // The server is going away; queued steering has no next run.
            self.finish_prompt(id, false).await;
            return Err(SessionCommandError::Draining);
        }
        let message = terminal
            .error_message
            .clone()
            .unwrap_or_else(|| "provider request failed".to_owned());
        // A failed run's steering is NOT re-queued here: the agent inbox is
        // the session's pending-message queue, and it already holds exactly
        // the undelivered messages. Re-adding the run's texts would replay
        // the consumed ones (duplicate user turns) and duplicate the pending
        // ones. Only the provider-missing path above re-queues, because no
        // agent ever received those texts.
        //
        // A run cancelled through the `cancel` frame settles as `cancelled`
        // (upstream server.ts:394-400 answers the aborted request with the
        // `cancelled` code) instead of a fake success; a plain abort (or a
        // mid-run disconnect) still returns the snapshot, and the upstream
        // precedence `controller.signal.aborted ? cancelled : error` holds
        // because this check runs before the provider-error mapping.
        if self.finish_prompt(id, false).await {
            return Err(SessionCommandError::Cancelled);
        }
        if failed {
            return Err(SessionCommandError::Provider(message));
        }
        self.get_session(id)
            .await
            .ok_or(SessionCommandError::NotFound)
    }

    /// Build an agent event handler that broadcasts transcript progress
    /// updates for one session.
    fn progress_handler(&self, session_id: &str) -> crate::agent::agent_loop::EventHandler {
        let updates = self.updates.clone();
        let session_id = session_id.to_owned();
        // Resolved once at prompt start: the format is immutable for the
        // lifetime of a `ServerState` (the builder consumes `self`).
        let format = self.state_update_format;
        // One stable message id per prompt turn: AssistantMessage carries no
        // id of its own, and assistant_delta requires a nonempty messageId.
        let message_uuid = Arc::new(Uuid::new_v4().to_string());
        let sequence = Arc::new(AtomicI64::new(1));
        Arc::new(move |event: crate::agent::agent_loop::AgentEvent| {
            let Some(progress) = agent_event_to_progress(&event, &message_uuid) else {
                return;
            };
            let seq = sequence.fetch_add(1, Ordering::Relaxed);
            let update = session_progress_update_in(&session_id, &progress, seq, format);
            let _ = updates.send(update);
        })
    }

    /// Clear the run's lifecycle state and report whether the run was
    /// cancelled (`cancel` frame): the caller then answers the deferred
    /// response with the `cancelled` error instead of the snapshot.
    ///
    /// `preserve_steering` keeps the session queue for a run that never
    /// reached an agent (the provider-missing path put the texts back there);
    /// every other outcome clears any leftover session-queue entry with a
    /// diagnostic. Steering that DID reach an agent lives in the agent inbox
    /// and is never duplicated into the session queue.
    async fn finish_prompt(&self, id: &str, preserve_steering: bool) -> bool {
        let mut sessions = self.sessions.write().await;
        let Some(record) = sessions.get_mut(id) else {
            return false;
        };
        if !preserve_steering && !record.queued_steer.is_empty() {
            tracing::warn!(
                session_id = %id,
                count = record.queued_steer.len(),
                "discarding steering queued for a completed/cancelled run"
            );
            record.queued_steer.clear();
        } else if preserve_steering && !record.queued_steer.is_empty() {
            tracing::debug!(
                session_id = %id,
                count = record.queued_steer.len(),
                "run without an agent kept its steering in the session queue"
            );
        }
        let cancelled = record.cancel_requested;
        record.cancel_requested = false;
        record.phase = SessionPhase::Idle;
        record.abort_signal = None;
        Self::next_sequence(record);
        self.publish_snapshot(id, record);
        cancelled
    }
}

/// The blocking part of [`ServerState::open_persisted_session`]: locate the
/// session file, open it (tail-first for a large file), restore the virtual
/// model registrations recorded on its branch, and read the branch's
/// model/thinking settings. Everything here touches the file or builds a
/// projection, so it runs inside one blocking task.
struct PreparedPersistedSession {
    manager: SessionManager,
    cwd: String,
    name: Option<String>,
    model: Option<(String, String)>,
    thinking_level: String,
}

fn prepare_persisted_session(
    dir: &std::path::Path,
    id: &str,
    registry: &ProviderRegistry,
    stall: Option<std::time::Duration>,
) -> Option<PreparedPersistedSession> {
    if let Some(stall) = stall {
        std::thread::sleep(stall);
    }
    let path = SessionManager::find_session_by_id(dir, id)?;
    // Large files: parse only the tail window first; a file inside the caps
    // opens exactly like `SessionManager::open`. Attach needs the branch
    // settings (model/thinking) and the virtual-model registrations, which
    // live on the leaf path, so a genuinely paged open widens here — on this
    // blocking task, never on a runtime worker.
    let mut manager =
        match SessionManager::open_paged(&path, ATTACH_PAGE_MAX_BYTES, ATTACH_PAGE_MAX_ENTRIES) {
            Ok(manager) => manager,
            // A paged open can refuse what the full loader would repair (a
            // legacy v1/v2 file wants a rewrite, which a paged manager must
            // never do): fall back to the historical full open instead of
            // failing the attach.
            Err(_) => match SessionManager::open(&path) {
                Ok(manager) => manager,
                Err(error) => {
                    tracing::warn!(
                        %error,
                        session_id = %id,
                        "persisted server session could not be reopened"
                    );
                    return None;
                }
            },
        };
    if manager.is_paged() {
        if let Err(error) = manager.widen_to_full() {
            tracing::warn!(
                %error,
                session_id = %id,
                "paged persisted server session could not be widened"
            );
            return None;
        }
    }
    let cwd = manager.cwd().to_owned();
    let name = manager.session_name();
    // The branch may record a VIRTUAL model (`record_virtual_selection`),
    // whose catalog entry only exists once its persisted
    // `pi.virtual-model-state` registration is in the registry overlay.
    // Restore that FIRST: resolving `projection.model` against an overlay
    // without it left the create-time default in place, so a resumed
    // virtual-model session silently routed to the wrong model.
    crate::agent_session::restore_virtual_models_from(registry, &manager);
    // Best-effort restore of model/thinking state along the PROJECTION path
    // (P2-k): scanning the file in order would also apply
    // `model_change`/`thinking_level_change` entries parked on ABANDONED
    // branches. `build_session_projection` walks the leaf→root parent chain
    // (upstream `getSessionContextSettings`), so only the switches on the
    // active path win; the model additionally falls back to the latest
    // assistant message's model on that path, like upstream. A projection
    // failure keeps the create-time defaults (fail-soft; the client re-selects
    // via set_model as before). A recorded model the registry cannot serve
    // (including a virtual id whose registration did not restore) is still
    // recorded here: the prompt then rejects it with the existing
    // `provider/model is not configured` error rather than silently driving
    // the create-time default.
    let (model, thinking_level) = match manager.build_session_projection() {
        Ok(projection) => (projection.model, projection.thinking_level),
        Err(_) => (None, "off".to_owned()),
    };
    Some(PreparedPersistedSession {
        manager,
        cwd,
        name,
        model,
        thinking_level,
    })
}

/// Build the tool registry a server session's prompts run with: the SAME
/// full core registry every CLI host builds
/// ([`register_core_tools`](crate::coding_agent::tools::register_core_tools) —
/// `read`, `bash`, `powershell`, `edit`, `write`, `grep`, `find`, `ls`, and
/// the model-facing `background_status`/`background_abort` controls), with
/// the fail-closed remote-execution policy applied on top.
///
/// With `allow_remote_bash` off (the default) both shell-execution tools
/// (`bash` and `powershell`) are excluded, so a remote prompt cannot execute
/// shell commands on the host. `powershell` is default-inactive but still
/// registered, and the scheduler executes a model-emitted name from the
/// registry (not from the declared set), so removing only `bash` would leave
/// the remote-execution gate open.
///
/// The server deliberately registers no extension/plugin builtins (`todo`,
/// `subagent`, `python`, `ctx_*`, `tool_search`, `codemode`, the MCP bridges)
/// and no TUI-only surfaces: [`build_session_agent`] passes `extensions:
/// None` — the server host loads no JS extension runtime, so those tools have
/// no execution backend here. The core set is therefore the server's actual
/// support range and matches `cli`'s built-in registry exactly.
fn session_tool_registry(allow_remote_bash: bool) -> crate::agent::ToolRegistry {
    let mut tools = crate::agent::ToolRegistry::new();
    crate::coding_agent::tools::register_core_tools(&mut tools);
    if !allow_remote_bash {
        tools.remove("bash");
        tools.remove("powershell");
    }
    tools
}

/// The names the server's full core registry exposes under the given shell
/// policy (see [`session_tool_registry`]).
///
/// This is the server-side counterpart of the CLI's built-in tool set: the
/// resolved [`crate::coding_agent::tools::CORE_TOOL_NAMES`] minus `bash` and
/// `powershell` when `allow_remote_bash` is off. Hosts and tests can compare
/// it with `CORE_TOOL_NAMES` to assert parity without reaching into
/// `ServerState` internals.
pub fn server_tool_names(allow_remote_bash: bool) -> Vec<String> {
    session_tool_registry(allow_remote_bash)
        .names()
        .into_iter()
        .map(str::to_owned)
        .collect()
}

/// Resolve the global tool-selection request against the server's
/// policy-filtered core registry.
///
/// `allow_remote_bash == false` removes `bash` and `powershell` BEFORE
/// matching, so `--tools bash` selects nothing on a locked-down server (a
/// documented consequence of the fail-closed shell policy, not a new
/// diagnostic). The shared resolver is the same one print, the TUI and RPC
/// use, so patterns, `+name`/`-name` modifiers, `--no-tools`,
/// `--no-builtin-tools`, settings `defaultTools` and the compiled-out-tool
/// diagnostics behave identically here.
///
/// Upstream has no server host (`C:/Users/XPENG_USER/Desktop/github/pi/
/// packages` contains no server package); this is pir's consistency rule.
pub fn resolve_server_tool_selection(
    request: &crate::coding_agent::tool_selection::ToolSelectionRequest,
    allow_remote_bash: bool,
) -> Result<Option<crate::coding_agent::ToolSelection>, String> {
    if !request.requested() {
        return Ok(None);
    }
    let registry = session_tool_registry(allow_remote_bash);
    let available = crate::coding_agent::tool_selection::AvailableTool::from_registry(&registry);
    request.resolve(&available)
}

/// Build the [`AgentSession`] a server session record drives: the shared
/// provider registry, the record's model/thinking level, the security-filtered
/// core tools, and threshold compaction. `session` carries the durable JSONL
/// storage ([`SessionManager`]) when the server persists sessions — the agent
/// appends every user/assistant/compaction entry to the file (per-append
/// fsync, so a disconnect needs no extra flush) and rebuilds its context from
/// the persisted transcript on reopen.
fn build_session_agent(
    registry: Arc<ProviderRegistry>,
    model: crate::ai::types::Model,
    thinking_level: &str,
    cwd: &str,
    allow_remote_bash: bool,
    session: Option<SessionManager>,
    tool_selection: Option<crate::coding_agent::ToolSelection>,
) -> crate::agent_session::AgentSession {
    let tools = session_tool_registry(allow_remote_bash);
    let agent = crate::agent_session::AgentSession::new(crate::agent_session::AgentSessionConfig {
        provider_registry: registry,
        model,
        api_key: None,
        thinking: parse_thinking_level(thinking_level),
        tools,
        session,
        compaction: Some(crate::compaction::CompactionConfig::default()),
        // The non-interactive turn cap (`PIR_MAX_TURNS`, default 1000): the
        // server has no settings.json layer, so it resolves the same process
        // knob as print/RPC. A turn is one model request; all tool calls in
        // that assistant message execute as one batch and cost no extra turn.
        max_turns: crate::config::env::max_turns(),
        system_prompt: None,
        cwd: Some(std::path::PathBuf::from(cwd)),
        event_stream: None,
        // The server host does not load a JS extension runtime; extension
        // events stay a local-host capability.
        extensions: None,
        // The host-resolved global selection (see
        // `ServerState::with_tool_selection`): the ACTIVE override over the
        // policy-filtered core registry, never a registry filter (F12).
        tool_selection,
    });
    // A server client's steering is a batch of user messages that must reach
    // the next request together (the RPC protocol has no per-message mode UI);
    // the session-level attach-window queue is drained into the same first
    // request.
    agent.set_steering_mode(crate::config::settings::SteeringMode::All);
    agent
}

/// Parse the protocol thinking-level string into a [`ThinkingLevel`]
/// (unknown values fall back to `medium`, the RPC host default).
fn parse_thinking_level(level: &str) -> ThinkingLevel {
    match level {
        "off" => ThinkingLevel::Off,
        "minimal" => ThinkingLevel::Minimal,
        "low" => ThinkingLevel::Low,
        "high" => ThinkingLevel::High,
        "xhigh" => ThinkingLevel::XHigh,
        "max" => ThinkingLevel::Max,
        _ => ThinkingLevel::Medium,
    }
}

#[derive(Debug, thiserror::Error)]
pub enum SessionCommandError {
    #[error("session not found")]
    NotFound,
    #[error("session is owned by another connection")]
    Locked,
    #[error("session is busy")]
    Busy,
    #[error("session limit reached")]
    Limit,
    #[error("server is draining")]
    Draining,
    /// The request was cancelled through a `cancel` frame (upstream
    /// `{code:"cancelled"}` on the aborted request, server.ts:394-400).
    #[error("request cancelled")]
    Cancelled,
    #[error("invalid request: {0}")]
    Invalid(String),
    #[error("provider error: {0}")]
    Provider(String),
    #[error("session storage error: {0}")]
    Storage(String),
    #[error("remote execution is disabled")]
    Policy,
}

/// The connection must hold an attachment on the session (upstream
/// `requireAttachment`: any attached connection may act, and the call's
/// attachment id IS the connection id).
fn ensure_attached(record: &SessionRecord, connection_id: &str) -> Result<(), SessionCommandError> {
    if record.attachments.iter().any(|a| a == connection_id) {
        Ok(())
    } else {
        Err(SessionCommandError::Locked)
    }
}

fn ensure_idle(record: &SessionRecord) -> Result<(), SessionCommandError> {
    if matches!(record.phase, SessionPhase::Idle) {
        Ok(())
    } else {
        Err(SessionCommandError::Busy)
    }
}

impl Default for ServerState {
    fn default() -> Self {
        // The v8 hello and every RPC target carry the server id, and target
        // ids must satisfy the upstream ServerId pattern, so the default id
        // is a freshly generated UUIDv4 string.
        Self::new(empty_snapshot(Uuid::new_v4().to_string()))
    }
}
