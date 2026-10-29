//! [`McpManager`]: one MCP server registry plus its live connections.
//!
//! Shared by the `mcp` builtin plugin (session lifetime) and the `pir mcp`
//! CLI (one-shot checks). Servers connect lazily on first use (upstream
//! behavior - server processes start when needed, not at startup) and stay
//! alive for the session; a dropped connection reconnects on the next call.
//!
//! # One root per manager
//!
//! The agent directory handed to [`McpManager::new`] is resolved once, at
//! construction, and is this manager's single source of truth for every path
//! it derives outside a server process:
//!
//! * the global `mcp.json` read by [`McpManager::load_config_files`],
//! * the MCP log (`<root>/mcp.log`), and
//! * the OAuth credential store (`<root>/mcp-auth.json`).
//!
//! A runtime agent-dir change (`PIR_AGENT_DIR` edited while a manager lives)
//! never moves a live manager by itself: every derivation reads the ONE root,
//! so config discovery cannot follow the environment while the log and the
//! credentials stay on the construction-time directory (the split-brain this
//! module used to allow). A caller that pushes a different directory into
//! [`McpManager::load_config_files`] is reported and pinned back to the root
//! instead of silently splitting the two.
//!
//! A changed agent dir takes effect through [`McpManager::re_root`] — the
//! plugin's `/reload` path. It runs the same teardown as
//! [`McpManager::shutdown`] (every live connection is closed, so no transport
//! keeps using the old sign-in state or diagnostics) and then replaces the
//! root, its log and its credential store as ONE unit. Re-rooting only the log
//! and the credential store would split servers (discovered from the new
//! directory) from the state they were opened with, so the three move together
//! or not at all.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use serde_json::Value as JsonValue;

use crate::config::mcp::{McpExposure, McpHttpServerConfig, McpServerConfig, McpStdioServerConfig};

use super::client::{DiscoveryInvalidation, McpClient, ServerEvent};
use super::error::McpError;
use super::http;
use super::log::McpLog;
use super::protocol::{CallToolResult, McpTool};
use super::registry::{get_mcp_tool_exposure, McpServerEntry, McpServerRegistry, McpServerSource};

/// Connection retry delays for HTTP servers that failed transiently
/// (upstream `CONNECT_RETRY_DELAYS_MS`); stdio servers get one attempt.
pub const CONNECT_RETRY_DELAYS_MS: [u64; 2] = [250, 1_000];

/// Characters of a stdio server's stderr kept for a connect-failure message
/// (upstream `STDERR_TAIL_CHARS`, runtime.ts:49).
const STDERR_TAIL_CHARS: usize = 2_000;

/// How long an automatic OAuth scope step-up waits for the browser flow.
///
/// Only reached when a server answered `insufficient_scope`: a refresh can
/// never widen a grant, so the alternative is failing the call. Matches the
/// CLI login default (`pir mcp login --timeout 300`); the caller's abort
/// signal cancels the wait.
const STEP_UP_TIMEOUT: Duration = Duration::from_secs(300);

/// The headless prompt of an automatic scope step-up (see
/// [`ServerConnection::step_up_oauth_token`]): the authorization URL goes to
/// the server's MCP log and the system browser, and the loopback callback (or
/// the step-up deadline) finishes the flow. A session has no terminal to paste
/// a redirect URL into, so the paste prompt never resolves — the same contract
/// as the CLI's non-interactive prompt.
struct StepUpSignInPrompt {
    log: Arc<McpLog>,
    server: String,
}

impl crate::mcp::oauth::McpSignInPrompt for StepUpSignInPrompt {
    fn show_authorization_url(&self, url: &str) {
        self.log
            .write_event(&self.server, &format!("authorize at {url}"));
        crate::ai::auth::oauth::open_browser(url);
    }

    fn prompt_for_redirect_url(&self) -> crate::mcp::oauth::PromptFuture<'_> {
        Box::pin(std::future::pending())
    }
}

/// The OAuth challenge carried by an authorization-required failure, when the
/// server sent a `WWW-Authenticate` header.
///
/// rmcp renders that header verbatim inside its transport error
/// (`authorization required: Bearer ...`, `insufficient scope: Bearer ...`) and
/// [`http::map_service_error`] keeps the flattened message, so the header is
/// recovered from the typed error's text and parsed with the shared
/// [`crate::mcp::oauth::parse_www_authenticate`]. `None` for a challenge that
/// carries nothing actionable.
pub fn authorization_challenge(error: &McpError) -> Option<crate::mcp::oauth::OAuthChallenge> {
    let text = match error {
        McpError::Http { body, .. } => body.as_str(),
        McpError::Transport(message) => message.as_str(),
        _ => return None,
    };
    // ASCII-lowercasing preserves byte offsets, so the index found here is
    // valid in the original text (a Unicode lowercase could change lengths).
    let lowered = text.to_ascii_lowercase();
    let start = ["bearer ", "dpop "]
        .iter()
        .filter_map(|scheme| lowered.find(scheme))
        .min()?;
    let challenge = crate::mcp::oauth::parse_www_authenticate(Some(&text[start..]));
    (challenge != crate::mcp::oauth::OAuthChallenge::default()).then_some(challenge)
}

/// Whether an authorization failure asks for MORE SCOPE rather than for a
/// fresh token: the `insufficient_scope` error code, or a 403 whose challenge
/// names the scopes it wants. A plain 401/expired-token challenge is not one
/// (a refresh may fix it).
pub fn is_scope_challenge(error: &McpError, challenge: &crate::mcp::oauth::OAuthChallenge) -> bool {
    challenge.error.as_deref() == Some("insufficient_scope")
        || (challenge.scope.is_some() && matches!(error, McpError::Http { status: 403, .. }))
}

/// The last `max` characters of `text` (upstream `slice(-N)` on a JS string).
fn stderr_tail_chars(text: &str, max: usize) -> String {
    let count = text.chars().count();
    if count <= max {
        return text.to_string();
    }
    text.chars().skip(count - max).collect()
}

/// Whether two agent directories name the same place.
///
/// Path equality alone misses the same directory spelled differently (a
/// trailing separator, a relative vs absolute form, a symlinked temp root), so
/// the comparison falls back to canonicalization when the plain forms differ.
/// Only used to decide whether a caller's request diverges from the manager's
/// root; a canonicalization failure (a directory that does not exist yet)
/// keeps the paths distinct, which only produces a diagnostic. Also used by the
/// `mcp` builtin to report a live `PIR_AGENT_DIR` that moved away from its
/// resolved root.
pub(crate) fn same_dir(left: &Path, right: &Path) -> bool {
    if left == right {
        return true;
    }
    match (std::fs::canonicalize(left), std::fs::canonicalize(right)) {
        (Ok(left), Ok(right)) => left == right,
        _ => false,
    }
}

/// How long a failed lazy discovery is not retried.
///
/// The tool-call wait path ([`McpManager::ensure_server_ready`]) caches a
/// failed server for this window and returns the recorded error immediately:
/// an unreachable server pays its connect timeout once, not once per tool
/// call. The window is bounded, so a server that recovers without a config
/// change is retried once it expires; [`McpManager::disconnect`] (config
/// change / reconnect) clears it immediately, and every explicit discovery
/// ([`McpManager::discover_server`], `pir mcp check`) bypasses it.
pub const READINESS_FAILURE_BACKOFF: Duration = Duration::from_secs(30);

/// Default aggregate budget of the SESSION-START discovery (upstream
/// `DEFAULT_STARTUP_WAIT_MS`, `extensions/mcp/index.ts:93`): the first prompt
/// waits at most this long for its servers and then continues, with the ones
/// that are still connecting reported as pending. Their tools become
/// available once they connect (the session's lazy wait path keeps
/// discovering them), so the cap bounds the first prompt without disabling
/// MCP.
pub const DEFAULT_STARTUP_DISCOVERY_BUDGET: Duration = Duration::from_millis(10_000);

/// Maximum characters of a diagnostic reason (see [`bounded_reason`]).
const MAX_REASON_CHARS: usize = 200;

/// One-line, length-bounded reason text for diagnostics.
///
/// A diagnostic names the server and the failure; it must not dump a whole
/// server config, a multi-line stderr tail, or a long HTTP body into the UI or
/// the model-visible status. Later lines are dropped (the first line of a
/// connect error carries the cause) and the result is capped at 200 chars.
pub fn bounded_reason(text: &str) -> String {
    let first = text.lines().next().unwrap_or("").trim();
    if first.chars().count() <= MAX_REASON_CHARS {
        return first.to_owned();
    }
    let mut bounded: String = first.chars().take(MAX_REASON_CHARS).collect();
    bounded.push('…');
    bounded
}

/// One server problem from a bounded startup discovery.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StartupDiscoveryProblem {
    /// The server the problem belongs to.
    pub server: String,
    /// The bounded reason (see [`bounded_reason`]).
    pub message: String,
}

/// Outcome of [`McpManager::discover_all_bounded`].
#[derive(Debug, Clone, Default)]
pub struct StartupDiscoveryReport {
    /// The discovered snapshot the batch published (the unbounded
    /// [`McpManager::discover_all`] returns this value alone).
    pub snapshot: BTreeMap<String, Vec<DiscoveredTool>>,
    /// Servers whose connect or `tools/list` FAILED inside the budget. Each
    /// keeps its `ConnectionState::Failed` record, its bounded reason is
    /// logged, and its previous snapshot entry is released like an unbounded
    /// [`McpManager::discover_all`] failure would.
    pub failed: Vec<StartupDiscoveryProblem>,
    /// Servers skipped because the aggregate budget elapsed while they were
    /// still connecting. Their previous snapshot entry (if any) is kept:
    /// nothing is known to have changed, and the connection attempt was
    /// cancelled, not failed — a later `ensure_server_ready` / tool-call wait
    /// retries it.
    pub pending: Vec<String>,
    /// Total time the batch spent (diagnostics).
    pub elapsed_ms: u64,
}

impl StartupDiscoveryReport {
    /// Whether every server was discovered within the budget.
    pub fn is_clean(&self) -> bool {
        self.failed.is_empty() && self.pending.is_empty()
    }
}

/// Per-server lazy-discovery readiness, the state behind the tool-call wait
/// cache.
///
/// The cache key is `(live client instance, tools/list_changed generation)`:
/// `Ready` is valid only while the connection still carries the client the
/// discovery listed (`McpClient::instance_id`) and no `tools/list_changed`
/// arrived since (`DiscoveryInvalidation::tools_generation`). Both are
/// atomics, so the hot-path check is lock-free apart from one short read lock
/// on the invalidation handle and performs no JSON work.
///
/// Transitions:
/// - `Idle -> Discovering` — a lazy or explicit discovery starts;
/// - `Discovering -> Ready` — connect + `tools/list` succeeded; the client
///   instance and generation are recorded;
/// - `Discovering -> Failed` — connect/list failed; the error is cached for
///   [`READINESS_FAILURE_BACKOFF`];
/// - `Discovering -> Stale` — the discovery future was cancelled;
/// - `Ready -> Stale` — `tools/list_changed` arrived, or the client was
///   replaced/dropped (reconnect);
/// - `Ready/Stale/Failed -> Idle` — the connection is disconnected or shut
///   down, which also clears the failure backoff;
/// - `Failed -> Idle` — the backoff elapsed (checked lazily);
/// - `Stale -> Discovering` — the next call re-lists.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServerReadiness {
    /// No discovery yet (or the connection was reset).
    Idle,
    /// A connect and/or `tools/list` is in flight.
    Discovering,
    /// Connected, tools listed, no invalidation since.
    Ready,
    /// Listed once, but a `tools/list_changed` (or reconnect) invalidated it.
    Stale,
    /// The last discovery failed; retries wait for the failure backoff.
    Failed,
}

impl ServerReadiness {
    fn code(self) -> u8 {
        match self {
            Self::Idle => 0,
            Self::Discovering => 1,
            Self::Ready => 2,
            Self::Stale => 3,
            Self::Failed => 4,
        }
    }

    fn from_code(code: u8) -> Self {
        match code {
            1 => Self::Discovering,
            2 => Self::Ready,
            3 => Self::Stale,
            4 => Self::Failed,
            _ => Self::Idle,
        }
    }
}

/// The cached failure of one server's last discovery.
struct ReadinessFailure {
    error: McpError,
    retry_after: Instant,
}

/// Model-facing text cap of an MCP result (upstream `MCP_OUTPUT_MAX_BYTES`).
pub const MCP_OUTPUT_MAX_BYTES: usize = 20 * 1024;

/// One tool discovered from a server's `tools/list`.
#[derive(Debug, Clone)]
pub struct DiscoveredTool {
    pub server: String,
    pub tool: McpTool,
    /// The resolved exposure for this tool.
    pub exposure: crate::config::mcp::McpExposure,
}

/// Connection lifecycle states (upstream `ServerState`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConnectionState {
    /// Not started yet (lazy).
    Idle,
    Connecting,
    Connected,
    /// The connection dropped; the next call reconnects.
    Disconnected {
        error: Option<String>,
    },
    /// The last connect failed.
    Failed {
        error: String,
    },
    /// The last connect was answered with an OAuth challenge no stored
    /// credential satisfies: the user must sign in (upstream
    /// `RuntimeServerState`'s `"needs-auth"`, runtime.ts:326 `markNeedsAuth`,
    /// reached from `connectFailed`, runtime.ts:437-446). Distinct from
    /// [`ConnectionState::Failed`] because the remedy is a sign-in, not a
    /// retry — the manager used to record the sign-in text as a `Failed`
    /// error and lose the distinction.
    NeedsAuth {
        /// The sign-in instruction (upstream `needsSignIn`'s message, the
        /// `LoginRequired` text).
        message: String,
    },
    /// Shut down.
    Closed,
}

impl ConnectionState {
    /// Whether the connection is live right now.
    pub fn is_connected(&self) -> bool {
        matches!(self, Self::Connected)
    }

    /// The error text the state carries, when it carries one (upstream
    /// `McpServerConnection.error`). `None` for `Idle`, `Connecting`,
    /// `Connected` and `Closed`, and for a `Disconnected` without a cause.
    pub fn error(&self) -> Option<&str> {
        match self {
            Self::Disconnected { error } => error.as_deref(),
            Self::Failed { error } => Some(error.as_str()),
            Self::NeedsAuth { message } => Some(message.as_str()),
            Self::Idle | Self::Connecting | Self::Connected | Self::Closed => None,
        }
    }
}

/// One configured server's live connection.
struct ServerConnection {
    /// The server's registry name: the key [`Self::set_state`] writes the
    /// synchronous state mirror under.
    name: String,
    entry: tokio::sync::RwLock<McpServerEntry>,
    client: tokio::sync::Mutex<Option<Arc<McpClient>>>,
    /// Single-flight connect coordination. `connecting` is the token exactly
    /// one caller holds while it performs the connect (the slot mutex is
    /// released for the whole retry/backoff run); `settled` is bumped when a
    /// connect or a close ends, waking the callers that waited instead of
    /// blocking on the slot mutex. The token is cleared by a drop guard, so a
    /// cancelled connect cannot leave the server stuck behind a stale flag.
    connecting: AtomicBool,
    settled: tokio::sync::watch::Sender<u64>,
    /// Serializes lazy/explicit tool-list discoveries for this server: one
    /// caller connects and lists, concurrent tool calls wait for that flight
    /// instead of issuing their own `tools/list`.
    discovery: tokio::sync::Mutex<()>,
    /// The instance id of the live client (`McpClient::instance_id`; 0 while
    /// no client is installed). A readiness record from an older client is
    /// stale by construction (reconnect invalidation).
    client_instance: AtomicU64,
    /// [`ServerReadiness`] as a `u8` (see [`ServerReadiness::code`]).
    readiness: AtomicU8,
    /// The `tools/list_changed` generation the last successful discovery
    /// listed under.
    ready_generation: AtomicU64,
    /// The client instance id the last successful discovery ran on.
    ready_client_instance: AtomicU64,
    /// The invalidation signal of the listed client (the generation source of
    /// the readiness cache key).
    ready_invalidation: RwLock<Option<Arc<DiscoveryInvalidation>>>,
    /// The last failed discovery and its backoff deadline.
    readiness_failure: RwLock<Option<ReadinessFailure>>,
    /// Set while a client taken out of `client` is being closed: callers must
    /// not start the next connect over the still-closing session. Cleared by
    /// a drop guard (a cancelled close cannot leave callers waiting forever).
    closing: AtomicBool,
    state: RwLock<ConnectionState>,
    /// Set when the connection leaves the manager's map (or is shut down):
    /// the object is then out of every manager path's reach, so it must never
    /// open another transport — a call already holding its `Arc` would
    /// otherwise reconnect through its own stale `entry` (the replaced or
    /// removed entry's config), and `McpManager::shutdown` could never stop
    /// that child, which would outlive the session.
    ///
    /// Ordering requirement (see [`McpManager::disconnect`]): the flag is set
    /// while the `connections` map lock is still held in the critical section
    /// that removes the entry, so no observer can get a fresh connection for
    /// the name before this object has stopped being reconnectable. It is
    /// never cleared: a detached object is dropped, never re-mapped (the next
    /// use builds a NEW connection from the registry's current entry).
    detached: AtomicBool,
    cwd: PathBuf,
    log: Arc<McpLog>,
    /// The manager's agent directory at construction: the OAuth stores live
    /// next to it, and `auth: { provider }` reads that directory's
    /// `auth.json` for the provider token (the same root as every other
    /// credential this connection uses).
    agent_dir: PathBuf,
    /// The MCP OAuth credential store (`<agentDir>/mcp-auth.json`).
    oauth: crate::mcp::oauth::McpOAuthCredentialStore,
    /// The bearer token the live connection carries — the stored OAuth access
    /// token or the `auth.provider` token — for the 401 stale-check and the
    /// provider re-read (upstream transport `lastToken` / runtime `authProvider`).
    last_auth_token: std::sync::Mutex<Option<String>>,
    /// Stderr tail of the last stdio connect attempt (upstream
    /// `stderrTail`): a failed handshake appends it to the error message, then
    /// clears it so a later success does not report stale diagnostics.
    stderr_tail: std::sync::Mutex<Option<String>>,
    /// The manager's [`McpManager::shutting_down`] flag: set when the teardown
    /// begins, so a detached-but-still-referenced connection cannot spawn a
    /// child after `shutdown` returned. Checked at [`ServerConnection::get_client`]
    /// entry and again before every connect attempt installs a client.
    shutting_down: Arc<AtomicBool>,
    /// The manager's [`McpManager::states_mirror`]: [`Self::set_state`] writes
    /// the new state there so the synchronous `mcp_servers` prompt section
    /// reads the same value without taking the async connections lock.
    mirror: Arc<RwLock<BTreeMap<String, ConnectionState>>>,
    /// The manager's [`McpManager::shutdown_epoch`]: read when a connect
    /// attempt starts, re-checked when its client is installed.
    shutdown_epoch: Arc<AtomicU64>,
}

/// The paths derived from one resolved agent directory.
///
/// Held behind [`McpManager::root`] and replaced as ONE value by
/// [`McpManager::re_root`]: config discovery, the log and the OAuth
/// credential store cannot move independently.
#[derive(Clone)]
struct ManagerRoot {
    /// The resolved agent directory, empty for a manager built without one
    /// ([`McpManager::without_log`]).
    agent_dir: PathBuf,
    log: Arc<McpLog>,
    oauth: crate::mcp::oauth::McpOAuthCredentialStore,
}

/// Registry + connections + discovered tools.
pub struct McpManager {
    registry: Arc<McpServerRegistry>,
    connections: tokio::sync::Mutex<BTreeMap<String, Arc<ServerConnection>>>,
    discovered: RwLock<BTreeMap<String, Vec<DiscoveredTool>>>,
    /// Set when the discovered tools changed in anything the plugin registry
    /// re-registration consumes (the per-server entry's tool names, their
    /// order, or the per-tool definitions: description/title, input schema,
    /// output schema, exposure) and the registry needs a rebuild; consumed
    /// (cleared) by the host seam.
    tools_dirty: AtomicBool,
    cwd: PathBuf,
    /// The one configured agent directory every derived path shares: the
    /// global `mcp.json` source (see [`McpManager::load_config_files`]), the
    /// log next to it, and the OAuth credential store next to that. It also
    /// feeds status messages ("Add them to <agentDir>/mcp.json or
    /// .pi/mcp.json"). Empty for a manager built without one
    /// ([`McpManager::without_log`]).
    ///
    /// Resolved by the caller at construction and never re-read from the
    /// environment; a changed directory is adopted as one unit by
    /// [`McpManager::re_root`]. See the module docs ("One root per manager")
    /// for the runtime-change policy.
    root: RwLock<ManagerRoot>,
    /// The parse/validation problems of the last config load (upstream
    /// `configErrors`, index.ts:284/1116). Written by
    /// [`McpManager::load_config_files`] and by the plugin's config pass via
    /// [`McpManager::set_config_errors`]; read by [`McpManager::status_snapshot`].
    config_errors: RwLock<Vec<String>>,
    /// Set for the whole [`McpManager::shutdown`] teardown (before the
    /// connection map is drained, cleared once every drained connection is
    /// closed): no path may open a transport while the teardown runs.
    ///
    /// Without it, a `connection()` blocked on the connections-map lock across
    /// the drain builds a NEW connection object — not in the drained map, so
    /// the teardown can never close its child — and spawns a child after
    /// `shutdown` returned. Checked in [`McpManager::connection`] (before and
    /// again under the map lock), and in [`ServerConnection::get_client`]
    /// before each connect attempt, through the shared `Arc`.
    ///
    /// Cleared at the end rather than kept forever: `/reload` calls
    /// [`McpManager::re_root`], which runs this teardown and then reuses the
    /// same manager, so a permanent flag would disable MCP for the rest of the
    /// session. A use that starts after the teardown returned is a legitimate
    /// fresh start — it builds a connection the NEXT teardown reaches.
    ///
    /// The flag alone closes the *entry* window (a caller that checks it before
    /// the drain sees `true`). The *in-flight* window — a connect already past
    /// its checks when the teardown starts, whose `install` lands after the
    /// drain — is closed by [`Self::shutdown_epoch`]: `shutdown` bumps it, and
    /// the install refuses a client whose connect began under an older epoch
    /// and closes it, so the child never outlives the teardown.
    shutting_down: Arc<AtomicBool>,
    /// The manager's synchronous state mirror (`name` -> last
    /// [`ConnectionState`] published by [`ServerConnection::set_state`]).
    ///
    /// The tokio-mutex connections map can only be read from a future; the
    /// system prompt's `mcp_servers` section is built from synchronous code
    /// (session build/reload, print's per-prompt cache) and must not do IO or
    /// block a prompt. Every `set_state` writes this mirror under a std
    /// `RwLock`, so a reader sees the same state the async status surface
    /// would — one prompt behind at worst when a transition lands mid-prompt.
    states_mirror: Arc<RwLock<BTreeMap<String, ConnectionState>>>,
    /// Bumped by every [`McpManager::shutdown`], read when a connect attempt
    /// starts: an install whose epoch is stale (the teardown began while the
    /// transport was opening) is refused and the fresh client is closed.
    shutdown_epoch: Arc<AtomicU64>,
    /// The background continuations of a bounded STARTUP discovery
    /// ([`McpManager::discover_all_bounded`]): one task per server that was
    /// still connecting when the aggregate budget elapsed. Aborted by
    /// [`McpManager::shutdown`] before the connection drain (aborting drops
    /// the task's future, which owns any transport it opened) and pruned of
    /// finished entries on every new batch, so the list cannot grow across a
    /// session.
    background_discovery: std::sync::Mutex<Vec<tokio::task::JoinHandle<()>>>,
}

impl McpManager {
    /// Build a manager rooted at `cwd` with the log at `<agentDir>/mcp.log`.
    ///
    /// `agent_dir` is resolved ONCE, by the caller: it roots the log, the
    /// OAuth credential store (`<agentDir>/mcp-auth.json`) and the global
    /// `mcp.json` discovery until [`Self::re_root`] replaces all three
    /// together. Callers that want the process's current agent directory
    /// resolve it once (`crate::coding_agent::context::agent_dir()`) and pass
    /// it in; the manager itself never re-reads the environment (see the
    /// module docs, "One root per manager").
    pub fn new(agent_dir: &Path, cwd: &Path) -> Arc<Self> {
        Self::build(
            Arc::new(McpLog::new(agent_dir.join("mcp.log"))),
            crate::mcp::oauth::McpOAuthCredentialStore::new(agent_dir.join("mcp-auth.json")),
            cwd,
            agent_dir,
        )
    }

    /// Build a manager with an explicit log path (the OAuth store and the
    /// global `mcp.json` root live next to it, like the log lives next to the
    /// agent dir it is given).
    pub fn with_log(log_path: PathBuf, cwd: &Path) -> Arc<Self> {
        let parent = log_path.parent().map(Path::to_path_buf).unwrap_or_default();
        let auth_path = log_path
            .parent()
            .map(|dir| dir.join("mcp-auth.json"))
            .unwrap_or_else(crate::mcp::oauth::McpOAuthCredentialStore::default_path);
        Self::build(
            Arc::new(McpLog::new(log_path)),
            crate::mcp::oauth::McpOAuthCredentialStore::new(auth_path),
            cwd,
            &parent,
        )
    }

    /// A manager with a no-op log (tests).
    pub fn without_log(cwd: &Path) -> Arc<Self> {
        Self::build(
            Arc::new(McpLog::disabled()),
            crate::mcp::oauth::McpOAuthCredentialStore::default_store(),
            cwd,
            Path::new(""),
        )
    }

    fn build(
        log: Arc<McpLog>,
        oauth: crate::mcp::oauth::McpOAuthCredentialStore,
        cwd: &Path,
        agent_dir: &Path,
    ) -> Arc<Self> {
        Arc::new(Self {
            registry: McpServerRegistry::new(),
            connections: tokio::sync::Mutex::new(BTreeMap::new()),
            discovered: RwLock::new(BTreeMap::new()),
            tools_dirty: AtomicBool::new(false),
            cwd: cwd.to_path_buf(),
            root: RwLock::new(ManagerRoot {
                agent_dir: agent_dir.to_path_buf(),
                log,
                oauth,
            }),
            config_errors: RwLock::new(Vec::new()),
            shutting_down: Arc::new(AtomicBool::new(false)),
            states_mirror: Arc::new(RwLock::new(BTreeMap::new())),
            shutdown_epoch: Arc::new(AtomicU64::new(0)),
            background_discovery: std::sync::Mutex::new(Vec::new()),
        })
    }

    /// One consistent snapshot of the root-derived paths: a re-root either
    /// happened before this read or after it, never halfway.
    fn root(&self) -> ManagerRoot {
        self.root
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// The MCP OAuth credential store (`<agentDir>/mcp-auth.json`), cloned
    /// from the manager's current root.
    pub fn oauth_store(&self) -> crate::mcp::oauth::McpOAuthCredentialStore {
        self.root().oauth
    }

    /// The server registry.
    pub fn registry(&self) -> &Arc<McpServerRegistry> {
        &self.registry
    }

    /// The MCP log (`<agentDir>/mcp.log`) for the manager's current root.
    pub fn log(&self) -> Arc<McpLog> {
        self.root().log
    }

    /// The working directory transports resolve relative paths against.
    pub fn cwd(&self) -> &Path {
        &self.cwd
    }

    /// Whether the discovered tools changed since the last rebuild request
    /// (tool names, their order, or the per-tool definitions re-registration
    /// feeds the model); clears the flag.
    pub fn take_tools_dirty(&self) -> bool {
        self.tools_dirty.swap(false, Ordering::SeqCst)
    }

    /// Mark the discovered tool snapshot as changed so the next host refresh
    /// rebuilds and re-registers the `mcp__*` bridges.
    ///
    /// Discovery sets this itself when a published list differs; the `/mcp`
    /// enable/disable/exposure/remove actions set it explicitly because their
    /// rebuild input changes WITHOUT a new tool list (a disabled server's
    /// bridges must re-register hidden, an enabled one's exposure must move
    /// back, a removed server's bridges must disappear).
    pub fn mark_tools_dirty(&self) {
        self.tools_dirty.store(true, Ordering::SeqCst);
    }

    /// Snapshot of the discovered tools, server by server.
    pub fn discovered_tools(&self) -> BTreeMap<String, Vec<DiscoveredTool>> {
        self.discovered
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Snapshot of the connection states.
    pub async fn connection_states(&self) -> BTreeMap<String, ConnectionState> {
        let mut states = BTreeMap::new();
        for (name, connection) in self.connections.lock().await.iter() {
            let state = connection
                .state
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone();
            states.insert(name.clone(), state);
        }
        states
    }

    /// The SYNCHRONOUS connection-state snapshot (see
    /// [`McpManager::states_mirror`]): the last state each connection
    /// published, readable without the async connections lock.
    ///
    /// This is the seam the system prompt's `mcp_servers` section uses: the
    /// session build/reload and print's per-prompt cache are synchronous and
    /// must never do IO or block a prompt, so they read this mirror instead of
    /// [`Self::connection_states`]. A transition that lands while a prompt is
    /// being assembled shows on the NEXT prompt, never mid-prompt.
    pub fn connection_states_snapshot(&self) -> BTreeMap<String, ConnectionState> {
        self.states_mirror
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Forget one server's mirrored state (a removed entry must not leave a
    /// stale row behind for the prompt section). Called by
    /// [`McpManager::disconnect`] and the [`McpManager::shutdown`] drain.
    fn forget_mirrored_state(&self, server: &str) {
        self.states_mirror
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(server);
    }

    /// The `mcp_servers` prompt-section rows for one prompt build: every
    /// ENABLED server, sorted by name, with how its tools are reached, its
    /// readiness from the synchronous mirror, and its published tool count.
    ///
    /// Cheap and IO-free (two short std locks plus the registry snapshot), so
    /// the session build/reload and print's per-prompt cache can call it
    /// without doing disk or network work per prompt:
    ///
    /// - `Ready` = the connection published `Connected` AND the discovered
    ///   snapshot has an entry: a connected server always lists its tools at
    ///   least once, but the snapshot is the model-visible evidence;
    /// - `Pending` = the startup budget elapsed (`Connecting`, `Idle`) or the
    ///   connection was never opened;
    /// - `Failed` = `Failed` / `NeedsAuth` / `Closed` / a `Disconnected` with
    ///   a cause, each with its bounded one-line reason.
    ///
    /// `builtin_tools` is the plugin's per-server `builtinTools` default
    /// resolution, so the reach label matches the exposure the registration
    /// pass applied.
    pub fn prompt_server_listings(
        &self,
        builtin_tools: impl Fn(&str) -> bool,
    ) -> Vec<crate::system_prompt::McpServerListing> {
        use crate::system_prompt::{McpServerListing, McpServerState};
        let states = self.connection_states_snapshot();
        // Only the discovered tool COUNT is needed: read the snapshot under
        // the lock and keep the lengths, never cloning the tool definitions
        // (schemas included) for a prompt render.
        let counts: BTreeMap<String, usize> = self
            .discovered
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .map(|(server, tools)| (server.clone(), tools.len()))
            .collect();
        let mut listings: Vec<McpServerListing> = self
            .registry
            .list()
            .into_iter()
            .filter(|entry| entry.config.is_enabled())
            .map(|entry| {
                let applies = builtin_tools(&entry.name);
                let reach = exposure_label(entry.config.effective_exposure(applies));
                let state = match states.get(&entry.name) {
                    // A connected server is `ready` even when its tool list
                    // has not been published yet (`pir mcp check` probes
                    // without publishing): the model needs the state, and
                    // the count is what is known right now.
                    Some(ConnectionState::Connected) => McpServerState::Ready {
                        tool_count: counts.get(&entry.name).copied().unwrap_or_default(),
                    },
                    Some(ConnectionState::Failed { error }) => McpServerState::Failed {
                        reason: bounded_reason(error),
                    },
                    Some(ConnectionState::NeedsAuth { message }) => McpServerState::Failed {
                        reason: bounded_reason(message),
                    },
                    Some(ConnectionState::Disconnected { error: Some(error) }) => {
                        McpServerState::Failed {
                            reason: bounded_reason(error),
                        }
                    }
                    // `Closed` only appears after the drain (which also clears
                    // the mirror); treat it like a not-yet-open connection.
                    Some(ConnectionState::Idle | ConnectionState::Connecting)
                    | Some(ConnectionState::Disconnected { error: None })
                    | Some(ConnectionState::Closed)
                    | None => McpServerState::Pending,
                };
                McpServerListing {
                    name: entry.name,
                    reach,
                    state,
                }
            })
            .collect();
        listings.sort_by(|left, right| left.name.cmp(&right.name));
        listings
    }

    /// The agent directory this manager is currently rooted at (empty when it
    /// was built without one): the ONE root behind its global `mcp.json`, its
    /// log and its OAuth credential store. See the module docs, "One root per
    /// manager".
    pub fn agent_dir(&self) -> PathBuf {
        self.root().agent_dir
    }

    /// Re-root the manager at `agent_dir` (the plugin `/reload` path).
    ///
    /// Runs the same teardown as [`Self::shutdown`] FIRST — every drained
    /// connection is closed, so no live transport keeps logging to, or
    /// refreshing credentials from, the old directory — and only then
    /// replaces the root, its log (`<agent_dir>/mcp.log`) and its OAuth
    /// credential store (`<agent_dir>/mcp-auth.json`) as ONE value. That
    /// ordering is what keeps the two faces together: after this returns,
    /// config discovery, diagnostics and sign-in state all name the new
    /// directory, and before it returns they all named the old one.
    ///
    /// A same-directory call still performs the shutdown (the historical
    /// `/reload` semantics) and keeps the existing log writer, so a reload
    /// that did not move the directory does not rotate or truncate `mcp.log`.
    ///
    /// The caller follows with its normal config pass; the registry and the
    /// discovered snapshot are not cleared here, so that pass can diff the
    /// new files against them and release only what the new root dropped
    /// (`McpPlugin::sync_config_files`).
    pub async fn re_root(&self, agent_dir: &Path) {
        self.shutdown().await;
        if same_dir(agent_dir, &self.agent_dir()) {
            return;
        }
        let root = ManagerRoot {
            agent_dir: agent_dir.to_path_buf(),
            log: Arc::new(McpLog::new(agent_dir.join("mcp.log"))),
            oauth: crate::mcp::oauth::McpOAuthCredentialStore::new(agent_dir.join("mcp-auth.json")),
        };
        *self
            .root
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = root;
    }

    /// Resolve a caller's requested agent directory against this manager's
    /// single root: [`Self::agent_dir`] for a manager built with one, the
    /// request itself for a manager built without one
    /// ([`Self::without_log`], which has no log or credential path to stay
    /// consistent with).
    ///
    /// Every config-discovery path that must not split from the manager's log
    /// and credential store resolves the directory here instead of reading the
    /// environment again — [`Self::load_config_files`] does, and the `mcp`
    /// builtin's config pass ([`crate::plugins::builtin::McpPlugin`]) does, so
    /// a runtime `PIR_AGENT_DIR` change cannot make servers come from one
    /// directory while sign-in state goes to another.
    pub fn resolve_config_root(&self, requested: &Path) -> PathBuf {
        let root = self.agent_dir();
        if root.as_os_str().is_empty() {
            requested.to_path_buf()
        } else {
            root
        }
    }

    /// The whole status surface in one cheap, non-blocking snapshot: every
    /// registered server with its state, exposure, tool count and error, plus
    /// the config problems (upstream `/mcp`'s `servers` + `configErrors`,
    /// index.ts:1017-1042).
    ///
    /// No network and no connection handshake: a server with no live
    /// connection object reports [`McpServerStatus::state`] `Idle`
    /// (rendered "configured"/lazy), never a connect. The only lock taken
    /// outside a `std::sync::RwLock` is one short `connections` map lock
    /// — no per-connection await, no `tools/list`. Exposure comes from the
    /// entry's current config through
    /// [`crate::config::mcp::McpServerConfig::effective_exposure`] (the same
    /// resolution [`crate::config::mcp::McpServerConfig::effective_tool_exposure`]
    /// applies per tool), so a UI never has to re-read `mcp.json`.
    ///
    /// `builtin_tools` is the file-level `builtinTools` default for a server
    /// (see [`crate::config::mcp::McpServerConfigBase::effective_exposure`]);
    /// an entry with no per-server value gets `false`, upstream's `codemode`
    /// default.
    pub async fn status_snapshot(&self, builtin_tools: impl Fn(&str) -> bool) -> McpStatusSnapshot {
        let discovered = self.discovered_tools();
        let states = self.connection_states().await;
        let servers = self
            .registry
            .list()
            .into_iter()
            .map(|entry| {
                let enabled = entry.config.is_enabled();
                let state = states
                    .get(&entry.name)
                    .cloned()
                    .unwrap_or(ConnectionState::Idle);
                let tools: Vec<String> = discovered
                    .get(&entry.name)
                    .map(|tools| tools.iter().map(|tool| tool.tool.name.clone()).collect())
                    .unwrap_or_default();
                let applies = builtin_tools(&entry.name);
                McpServerStatus {
                    exposure: entry.config.effective_exposure(applies),
                    builtin_tools: applies,
                    tools,
                    transport: transport_label(&entry.config).to_owned(),
                    source: entry.source.clone(),
                    enabled,
                    state,
                    name: entry.name,
                    config: entry.config,
                }
            })
            .collect();
        McpStatusSnapshot {
            servers,
            config_errors: self.config_errors(),
            agent_dir: self.agent_dir(),
            overridden: Vec::new(),
        }
    }

    /// Whether a connection object exists for the server - the lazy-connect
    /// assertion used by tests (`connect` objects are only created on first
    /// use; the underlying transport only on `get_client`).
    pub async fn has_connection(&self, server: &str) -> bool {
        self.connections.lock().await.contains_key(server)
    }

    /// Register every server from the global + (trust-gated) project
    /// `mcp.json` files. Returns the config problems found and records them
    /// for [`McpManager::status_snapshot`] (upstream `configErrors`).
    ///
    /// The global file is read from the manager's ONE root
    /// ([`Self::agent_dir`]) — the same directory its log and OAuth
    /// credential store were derived from. `agent_dir` is the caller's
    /// request; a manager built with a root ignores a different value and
    /// records the divergence instead of discovering servers from one
    /// directory while writing sign-in state and diagnostics to another (the
    /// split-brain this method used to allow). A manager built without a root
    /// ([`Self::without_log`]) has nothing to stay consistent with and follows
    /// the request.
    pub fn load_config_files(&self, agent_dir: &Path, project_trusted: bool) -> Vec<String> {
        // One snapshot: the root, the log line and the credential store this
        // call may touch all come from the same re-root generation.
        let state = self.root();
        let root = if state.agent_dir.as_os_str().is_empty() {
            agent_dir.to_path_buf()
        } else {
            state.agent_dir.clone()
        };
        let mut errors = Vec::new();
        if !same_dir(agent_dir, &root) {
            let text = format!(
                "{}: ignored; this MCP manager is rooted at {} (its config, log and credentials stay together; a changed agent dir takes effect on /reload)",
                agent_dir.display(),
                root.display()
            );
            state.log.write_event("(config)", &text);
            errors.push(text);
        }
        let global_path = crate::config::mcp::global_mcp_path(&root);
        let loaded = crate::config::mcp::load_mcp_configs(&root, &self.cwd, project_trusted);
        for file in loaded {
            let is_global = file.path == global_path;
            for error in &file.errors {
                let text = format!("{}: {error}", file.path.display());
                state.log.write_event("(config)", &text);
                errors.push(text);
            }
            for (name, config) in file.servers {
                let source = if is_global {
                    super::registry::McpServerSource::GlobalConfig(file.path.clone())
                } else {
                    super::registry::McpServerSource::ProjectConfig(file.path.clone())
                };
                state
                    .log
                    .write_event(&name, &format!("registered from {}", file.path.display()));
                self.registry.register(McpServerEntry {
                    name,
                    config,
                    source,
                });
            }
        }
        self.set_config_errors(errors.clone());
        errors
    }

    /// Replace the recorded config problems (upstream `configErrors`): the
    /// plugin's config pass calls this with the problems of every reloaded
    /// `mcp.json`, since it loads the files itself (it also needs the
    /// per-file `builtinTools` / wrapper shape).
    pub fn set_config_errors(&self, errors: Vec<String>) {
        *self
            .config_errors
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = errors;
    }

    /// The parsed `mcp.json` documents for a config pass that registers the
    /// entries itself (the `mcp` builtin's [`crate::plugins::builtin::McpPlugin`]
    /// pass: it needs the per-file `builtinTools` / wrapper shape, which
    /// [`Self::load_config_files`] flattens away).
    ///
    /// The files are loaded from [`Self::resolve_config_root`] — the same root
    /// the log and the OAuth credential store were derived from — and the
    /// returned diagnostics carry the divergence line when `requested` is a
    /// different directory, so such a pass cannot discover servers from one
    /// agent dir while sign-in state and diagnostics go to another. A pass
    /// that loads the files through
    /// [`crate::config::mcp::load_mcp_configs`] with a live-resolved directory
    /// instead re-opens exactly that split.
    pub fn load_config_documents(
        &self,
        requested: &Path,
        project_trusted: bool,
    ) -> (Vec<crate::config::mcp::LoadedMcpConfig>, Vec<String>) {
        let state = self.root();
        let root = if state.agent_dir.as_os_str().is_empty() {
            requested.to_path_buf()
        } else {
            state.agent_dir.clone()
        };
        let mut errors = Vec::new();
        if !same_dir(requested, &root) {
            let text = format!(
                "{}: ignored; this MCP manager is rooted at {} (its config, log and credentials stay together; a changed agent dir takes effect on /reload)",
                requested.display(),
                root.display()
            );
            state.log.write_event("(config)", &text);
            errors.push(text);
        }
        (
            crate::config::mcp::load_mcp_configs(&root, &self.cwd, project_trusted),
            errors,
        )
    }

    /// The parse/validation problems of the last config load, as formatted
    /// `"<path>: <problem>"` lines (upstream `configErrors`).
    pub fn config_errors(&self) -> Vec<String> {
        self.config_errors
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// The connection for `server`, created (not connected) on demand.
    ///
    /// Fails closed once [`McpManager::shutdown`] has begun: a connection
    /// object (and the child its `get_client` would spawn) created after the
    /// drain outlives the teardown, which can no longer reach it.
    async fn connection(&self, server: &str) -> Result<Arc<ServerConnection>, McpError> {
        if self.shutting_down.load(Ordering::SeqCst) {
            return Err(McpError::ConnectionClosed);
        }
        let entry = self
            .registry
            .get(server)
            .ok_or_else(|| McpError::UnknownServer(server.to_owned()))?;
        if !entry.config.is_enabled() {
            return Err(McpError::ServerDisabled(server.to_owned()));
        }
        let mut connections = self.connections.lock().await;
        // Re-check under the map lock: `shutdown` sets the flag before taking
        // the map, so a torn read here can only see either "not shutting down"
        // with a still-present map entry, or the flag already set.
        if self.shutting_down.load(Ordering::SeqCst) {
            return Err(McpError::ConnectionClosed);
        }
        if let Some(existing) = connections.get(server) {
            return Ok(existing.clone());
        }
        // The connection captures the log and credential store it was opened
        // with; take both from ONE root snapshot so a concurrent `re_root`
        // cannot hand it the new log with the old credentials (re_root shuts
        // every connection down before swapping, so this is belt and braces).
        let root = self.root();
        let (settled, _) = tokio::sync::watch::channel(0u64);
        let connection = Arc::new(ServerConnection {
            name: server.to_owned(),
            entry: tokio::sync::RwLock::new(entry),
            client: tokio::sync::Mutex::new(None),
            connecting: AtomicBool::new(false),
            settled,
            discovery: tokio::sync::Mutex::new(()),
            client_instance: AtomicU64::new(0),
            readiness: AtomicU8::new(ServerReadiness::Idle.code()),
            ready_generation: AtomicU64::new(0),
            ready_client_instance: AtomicU64::new(0),
            ready_invalidation: RwLock::new(None),
            readiness_failure: RwLock::new(None),
            closing: AtomicBool::new(false),
            state: RwLock::new(ConnectionState::Idle),
            detached: AtomicBool::new(false),
            cwd: self.cwd.clone(),
            log: root.log,
            agent_dir: root.agent_dir,
            oauth: root.oauth,
            last_auth_token: std::sync::Mutex::new(None),
            stderr_tail: std::sync::Mutex::new(None),
            shutting_down: Arc::clone(&self.shutting_down),
            mirror: Arc::clone(&self.states_mirror),
            shutdown_epoch: Arc::clone(&self.shutdown_epoch),
        });
        connections.insert(server.to_owned(), connection.clone());
        Ok(connection)
    }

    /// The live client for `server`, connecting lazily with HTTP retry
    /// backoff (upstream `getClient`).
    pub async fn get_client(&self, server: &str) -> Result<Arc<McpClient>, McpError> {
        let connection = self.connection(server).await?;
        connection.get_client().await
    }

    /// Connect to every enabled registered server and refresh the discovered
    /// tool snapshot. Per-server failures are recorded (and logged), not
    /// fatal. Discovery runs sequentially: each server's connect is bounded
    /// by its configured timeout, and tool lists are small.
    pub async fn discover_all(&self) -> BTreeMap<String, Vec<DiscoveredTool>> {
        let names: Vec<String> = self
            .registry
            .list()
            .into_iter()
            .filter(|entry| entry.config.is_enabled())
            .map(|entry| entry.name)
            .collect();
        let mut snapshot = self.discovered_tools();
        // Any server added/removed, or gaining/losing/reordering tools, or
        // keeping its names while a tool's re-registration input changed
        // (description, schemas, exposure), marks the plugin registry dirty
        // (the host seam rebuilds and re-registers the tools). The comparison
        // is incremental per server, so unchanged servers cost nothing.
        let mut changed = false;
        for name in names {
            match self.discover_server(&name).await {
                Ok(tools) => {
                    changed |= match snapshot.get(&name) {
                        Some(previous) => tool_definitions_differ(previous, &tools),
                        None => true,
                    };
                    snapshot.insert(name, tools);
                }
                Err(error) => {
                    self.log()
                        .write_event(&name, &format!("discovery failed: {error}"));
                    changed |= snapshot.remove(&name).is_some();
                }
            }
        }
        self.set_discovered_snapshot(snapshot.clone());
        if changed {
            self.tools_dirty.store(true, Ordering::SeqCst);
        }
        snapshot
    }

    /// The SESSION-START discovery: connect every enabled server
    /// CONCURRENTLY and wait at most `budget` for the whole batch (upstream
    /// `waitForDirectServers`' `DEFAULT_STARTUP_WAIT_MS`,
    /// `extensions/mcp/index.ts:93,1157-1174`).
    ///
    /// The per-server timeout ([`McpServerEntry::timeout_ms`]) still bounds
    /// each connect; the aggregate budget bounds the FIRST PROMPT. Servers
    /// that are still connecting when it elapses are cancelled (their pending
    /// future owns the spawned child, so cancelling it kills the child) and
    /// reported as [`StartupDiscoveryReport::pending`]; a background
    /// continuation ([`Self::background_discovery`]) retries each of them
    /// through [`McpManager::ensure_server_ready`], so their tools become
    /// available once connected instead of being silently disabled. The tool
    /// call wait path ([`McpManager::ensure_server_ready`] via
    /// `McpPlugin::wait_for_tool_call`) covers them in the meantime.
    ///
    /// Failures inside the budget are recorded exactly like
    /// [`McpManager::discover_all`] does (the manager log keeps the full error)
    /// and returned as a bounded one-line reason, so the host can report
    /// which server failed instead of surfacing only a later
    /// "Tool not found in registry".
    pub async fn discover_all_bounded(
        self: &Arc<Self>,
        budget: Duration,
    ) -> StartupDiscoveryReport {
        let names: Vec<String> = self
            .registry
            .list()
            .into_iter()
            .filter(|entry| entry.config.is_enabled())
            .map(|entry| entry.name)
            .collect();
        let started = Instant::now();
        let deadline = started + budget;
        let mut report = StartupDiscoveryReport::default();
        let mut unfinished: std::collections::BTreeSet<String> = names.iter().cloned().collect();
        let mut tasks: tokio::task::JoinSet<(String, Result<(), McpError>)> =
            tokio::task::JoinSet::new();
        for name in names {
            let manager = Arc::clone(self);
            // `ensure_server_ready` is the PUBLISHING path (the lazy wait's
            // own), so a server that finishes after the first prompt still
            // lands in the discovered snapshot and marks the registry dirty.
            tasks.spawn(async move {
                let outcome = manager.ensure_server_ready(&name).await;
                (name, outcome)
            });
        }
        while !tasks.is_empty() {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                break;
            }
            match tokio::time::timeout(left, tasks.join_next()).await {
                Ok(Some(Ok((name, Ok(()))))) => {
                    unfinished.remove(&name);
                }
                Ok(Some(Ok((name, Err(error))))) => {
                    unfinished.remove(&name);
                    self.log()
                        .write_event(&name, &format!("discovery failed: {error}"));
                    // Like the unbounded [`Self::discover_all`]: a failed
                    // startup discovery releases the server's previous
                    // snapshot entry, so no bridge outlives a failed connect.
                    if self.has_snapshot_entry(&name) {
                        self.forget_discovered(&name);
                        self.tools_dirty.store(true, Ordering::SeqCst);
                    }
                    report.failed.push(StartupDiscoveryProblem {
                        server: name,
                        message: bounded_reason(&error.to_string()),
                    });
                }
                // A panicked task (or one cancelled by a concurrent teardown):
                // its server stays pending and is retried in the background.
                Ok(Some(Err(join_error))) => {
                    crate::ext_diag!("[ext] MCP startup discovery task failed: {join_error}");
                }
                Ok(None) => break,
                Err(_) => break,
            }
        }
        // Still running at the deadline: cancel them (dropping the future
        // drops the transport it opened, killing a spawned child) and hand
        // them to the background continuation.
        tasks.abort_all();
        drop(tasks);
        report.pending = unfinished.into_iter().collect();
        report
            .failed
            .sort_by(|left, right| left.server.cmp(&right.server));
        report.snapshot = self.discovered_tools();
        report.elapsed_ms = started.elapsed().as_millis() as u64;
        self.spawn_background_discovery(&report.pending);
        report
    }

    /// Retry the servers a bounded startup discovery left pending, one
    /// background task each, so a slow server never delays a fast sibling.
    ///
    /// The tasks are tracked ([`Self::background_discovery`]) and aborted by
    /// [`McpManager::shutdown`] BEFORE its drain: aborting drops the task's
    /// future, which owns any transport it opened, so a continuation can
    /// never spawn a child past the teardown. A task also refuses to start
    /// when a teardown was requested (the flag) or completed (the epoch)
    /// since the batch began.
    fn spawn_background_discovery(self: &Arc<Self>, servers: &[String]) {
        if servers.is_empty() {
            return;
        }
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            // No runtime to continue on (a synchronous harness): the servers
            // stay lazy and the tool-call wait path retries them on use.
            return;
        };
        let mut tasks = self
            .background_discovery
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        tasks.retain(|task| !task.is_finished());
        let epoch = self.shutdown_epoch.load(Ordering::SeqCst);
        let weak = Arc::downgrade(self);
        for server in servers {
            let weak = weak.clone();
            let server = server.clone();
            tasks.push(handle.spawn(async move {
                // A WEAK handle: the tracked `JoinHandle` lives on the
                // manager, so an `Arc` here would be a cycle that outlives
                // every drop of the manager (the task would keep the child
                // alive until its own connect timeout). The manager going
                // away ends the continuation.
                let Some(manager) = weak.upgrade() else {
                    return;
                };
                if manager.shutting_down.load(Ordering::SeqCst)
                    || manager.shutdown_epoch.load(Ordering::SeqCst) != epoch
                {
                    return;
                }
                if let Err(error) = manager.ensure_server_ready(&server).await {
                    // The lazy path is best effort: the failure is already on
                    // the connection state and the next use retries it.
                    manager.log().write_event(
                        &server,
                        &format!(
                            "background discovery failed: {}",
                            bounded_reason(&error.to_string())
                        ),
                    );
                }
            }));
        }
    }

    /// Connect to one server and list its tools WITHOUT touching the
    /// discovered snapshot: the pure probe used by [`McpManager::discover_all`]
    /// (which writes the whole batch) and `pir mcp check`
    /// ([`McpManager::check_server`] / [`McpManager::check_server_report`]).
    /// Publication is the caller's move: the lazy tool-call wait path calls
    /// [`McpManager::ensure_server_ready`], the notification refresh
    /// [`McpManager::refresh_discovered_tools`].
    ///
    /// The discovery runs under the connection's single-flight lock and
    /// VALIDATES the readiness cache: a successful list records the client
    /// epoch and the `tools/list_changed` generation it listed under, so the
    /// next [`McpManager::ensure_server_ready`] can skip the round trip.
    pub async fn discover_server(&self, server: &str) -> Result<Vec<DiscoveredTool>, McpError> {
        let connection = self.connection(server).await?;
        let _flight = connection.discovery.lock().await;
        let _guard = connection.begin_discovery();
        match self.list_server_tools(server, &connection).await {
            Ok(tools) => Ok(tools),
            Err(error) => {
                connection.mark_failed(error.clone());
                Err(error)
            }
        }
    }

    /// Make `server` ready for a tool call with the readiness cache: a server
    /// that is already `Ready` (connected and unchanged since its last
    /// `tools/list`) returns with only the atomic/generation check — no
    /// `tools/list` round trip and no tool-list clone.
    ///
    /// A server whose last discovery failed inside
    /// [`READINESS_FAILURE_BACKOFF`] returns the recorded error immediately;
    /// the wait path swallows it (a failing server never blocks the call), so
    /// an unreachable server pays its connect timeout once, not on every
    /// call. Concurrent callers serialize on the per-server discovery lock
    /// (single flight): one lists, the rest observe `Ready`.
    ///
    /// A successful lazy discovery is PUBLISHED (the snapshot entry is
    /// replaced and the plugin registry is marked dirty when the tools
    /// changed), exactly like [`McpManager::discover_server_and_publish`]: the
    /// wait hook's tail rebuild registers the fresh `mcp__*` bridges.
    pub async fn ensure_server_ready(&self, server: &str) -> Result<(), McpError> {
        let connection = self.connection(server).await?;
        if connection.readiness() == ServerReadiness::Ready && self.has_snapshot_entry(server) {
            return Ok(());
        }
        if let Some(error) = connection.failure_backoff_error() {
            return Err(error);
        }
        let _flight = connection.discovery.lock().await;
        // Another caller may have completed the discovery while this one
        // waited for the flight.
        if connection.readiness() == ServerReadiness::Ready && self.has_snapshot_entry(server) {
            return Ok(());
        }
        if let Some(error) = connection.failure_backoff_error() {
            return Err(error);
        }
        let _guard = connection.begin_discovery();
        match self.list_server_tools(server, &connection).await {
            Ok(tools) => {
                self.publish_discovered_tools(server, &tools);
                Ok(())
            }
            Err(error) => {
                connection.mark_failed(error.clone());
                Err(error)
            }
        }
    }

    /// The current readiness of `server` (transitioning an expired `Failed`
    /// backoff to `Idle` and a stale `Ready` to `Stale`), or `None` when no
    /// connection object exists yet.
    pub async fn server_readiness(&self, server: &str) -> Option<ServerReadiness> {
        self.connections
            .lock()
            .await
            .get(server)
            .map(|connection| connection.readiness())
    }

    /// Whether `server` has a discovered snapshot entry. A readiness record
    /// only counts as cached for the lazy path when the tools were also
    /// published: a pure probe (`discover_server`, `pir mcp check`) records
    /// readiness but never publishes, so the next tool call must still list
    /// and publish. This is a map lookup, not a tool-list clone.
    fn has_snapshot_entry(&self, server: &str) -> bool {
        self.discovered
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .contains_key(server)
    }

    /// The shared body of [`McpManager::discover_server`] and
    /// [`McpManager::ensure_server_ready`]; the caller holds the connection's
    /// discovery flight and has marked the readiness `Discovering`.
    async fn list_server_tools(
        &self,
        server: &str,
        connection: &ServerConnection,
    ) -> Result<Vec<DiscoveredTool>, McpError> {
        let client = connection.get_client().await?;
        let invalidation = Arc::clone(client.discovery_invalidation());
        // The generation the list below is expected to reflect. Any event
        // recorded after this read but not covered by the list leaves the
        // generation newer than the recorded one, i.e. the server stale.
        let mut generation = invalidation.tools_generation();
        let mut tools = client.list_all_tools().await?;
        // A `tools/list_changed` recorded WHILE connecting/listing means the
        // list just read may already be stale: re-list once (upstream
        // `refreshTools` on the notification). A notification that was
        // already recorded before this discovery is not a reason to list
        // twice — it is what made the readiness stale in the first place; the
        // generation captured above already covers it.
        let after = invalidation.tools_generation();
        if after != generation {
            generation = after;
            tools = client.list_all_tools().await?;
        }
        // Drain `notifications/message` into the server log (and drop any
        // recorded list_changed whose effect the re-list above already
        // covered: its generation is <= `after`).
        let _ = self.consume_recorded_notifications(server, &client).await;
        let entry = connection.entry.read().await.clone();
        if connection.detached.load(Ordering::SeqCst) {
            return Err(McpError::ConnectionClosed);
        }
        let discovered: Vec<DiscoveredTool> = tools
            .into_iter()
            .map(|tool| DiscoveredTool {
                server: server.to_owned(),
                exposure: get_mcp_tool_exposure(&entry.config, &tool.name),
                tool,
            })
            .collect();
        connection.mark_ready(&client, generation);
        Ok(discovered)
    }

    /// [`McpManager::discover_server`] plus publication: the listed tools
    /// replace `server`'s entry in the discovered snapshot, and the plugin
    /// registry is marked dirty when the published entry differs from the
    /// previous one — the tool names (and their order) or any tool's
    /// description/title, schemas, or exposure (the host seam rebuilds and
    /// re-registers the `mcp__*` bridges).
    ///
    /// This is the EXPLICIT refresh (the config-sync follow-up after a
    /// release, tests, tooling): it always lists. The lazy tool-call path
    /// uses the readiness cache ([`McpManager::ensure_server_ready`]) so an
    /// unchanged connected server does not pay a `tools/list` per call.
    pub async fn discover_server_and_publish(
        &self,
        server: &str,
    ) -> Result<Vec<DiscoveredTool>, McpError> {
        let tools = self.discover_server(server).await?;
        self.publish_discovered_tools(server, &tools);
        Ok(tools)
    }

    /// Call `server`'s tool `tool` with `arguments`.
    ///
    /// `signal` is the caller's abort signal (upstream `McpClient`'s
    /// per-request signal, #10598): the retry loop checks it before every
    /// dispatch, so a call whose caller gave up while a shared OAuth refresh
    /// was in flight does not reach the server. The refresh itself is not
    /// aborted — other requests waiting on it still succeed.
    pub async fn call_tool(
        &self,
        server: &str,
        tool: &str,
        arguments: &JsonValue,
        signal: Option<&crate::ai::types::AbortSignal>,
    ) -> Result<CallToolResult, McpError> {
        let connection = self.connection(server).await?;
        let result = connection.call_tool(tool, arguments, signal).await;
        // React to notifications that arrived while the call ran (upstream
        // runtime.ts:386-404): log messages land in `mcp.log`, and a
        // `tools/list_changed` refreshes the discovered snapshot before the
        // next plugin-registry rebuild.
        if let Some(client) = connection.current_client().await {
            if self.consume_recorded_notifications(server, &client).await {
                self.refresh_discovered_tools(server).await;
            }
        }
        result
    }

    /// Drain the notifications the client recorded and react (upstream
    /// runtime.ts `onNotification` wiring): `notifications/message` appends
    /// to the server log (`log.write`, line 386). Returns whether a
    /// `notifications/tools/list_changed` was seen, i.e. the caller should
    /// refresh (upstream `refreshTools`, lines 399-401).
    /// `resources/list_changed` / `prompts/list_changed` /
    /// `resources/updated` are recorded by the client but have no consumer
    /// here: pir exposes tools only, and upstream reacts to
    /// `resources/list_changed` solely by refreshing its resources surface.
    async fn consume_recorded_notifications(&self, server: &str, client: &McpClient) -> bool {
        let mut tools_changed = false;
        for event in client.take_notifications().await {
            match event {
                ServerEvent::ToolsListChanged => tools_changed = true,
                ServerEvent::LoggingMessage(message) => {
                    self.log().write_notification(server, &message);
                }
                _ => {}
            }
        }
        tools_changed
    }

    /// Re-list `server`'s tools, replace its discovered snapshot, and mark
    /// the plugin registry dirty when the published entry differs from the
    /// previous one — names, order, or a tool definition re-registration
    /// reads (upstream `refreshTools` -> `onTools`/`changed`,
    /// runtime.ts:458-468). The write shape lives in
    /// [`McpManager::publish_discovered_tools`], shared with the lazy wait
    /// path ([`McpManager::discover_server_and_publish`]).
    async fn refresh_discovered_tools(&self, server: &str) {
        match self.discover_server(server).await {
            Ok(tools) => {
                self.publish_discovered_tools(server, &tools);
            }
            Err(error) => {
                // Upstream records the failure and keeps the old list.
                self.log()
                    .write_event(server, &format!("tool refresh failed: {error}"));
            }
        }
    }

    /// Test-connect `server` with a bounded overall deadline and report its
    /// tool names (`pir mcp check`).
    pub async fn check_server(
        &self,
        server: &str,
        deadline: Duration,
    ) -> Result<Vec<String>, McpError> {
        match tokio::time::timeout(deadline, self.discover_server(server)).await {
            Ok(Ok(tools)) => Ok(tools.iter().map(|tool| tool.tool.name.clone()).collect()),
            Ok(Err(error)) => Err(error),
            Err(_) => Err(McpError::Timeout {
                timeout_ms: deadline.as_millis() as u64,
            }),
        }
    }

    /// Test-connect `server` (same bounded deadline as
    /// [`McpManager::check_server`]) and report its tool names plus whether it
    /// offers resources, for the `pir mcp check` report (upstream `list()`'s
    /// `resources: true` flag). The resources themselves are never listed
    /// while connecting (upstream #10526): the lists only fed the counts, and
    /// a slow listing kept the server's tools out of the first prompt.
    pub async fn check_server_report(
        &self,
        server: &str,
        deadline: Duration,
    ) -> Result<(Vec<String>, bool), McpError> {
        match tokio::time::timeout(deadline, self.discover_server(server)).await {
            Ok(Ok(tools)) => {
                let has_resources = match self.get_client(server).await {
                    Ok(client) => client.has_resources(),
                    Err(_) => false,
                };
                Ok((
                    tools.iter().map(|tool| tool.tool.name.clone()).collect(),
                    has_resources,
                ))
            }
            Ok(Err(error)) => Err(error),
            Err(_) => Err(McpError::Timeout {
                timeout_ms: deadline.as_millis() as u64,
            }),
        }
    }

    /// Shut every connection down (plugin `on_shutdown` / host teardown).
    /// Child processes are killed and reaped by the transports. Each drained
    /// connection is detached (see [`ServerConnection::detached`]), so a call
    /// still holding one of their `Arc`s fails closed instead of reconnecting
    /// a child this method could no longer reach.
    pub async fn shutdown(&self) {
        // Set the flag BEFORE the drain: a `connection()`/`get_client()` that
        // arrives after this point must fail closed rather than build a
        // connection (and spawn a child) the drain could no longer reach. The
        // flag is a plain atomic, so no busy-wait and no map lock is needed to
        // observe it.
        self.shutting_down.store(true, Ordering::SeqCst);
        // Bump the epoch BEFORE the drain: a connect already past its entry
        // checks (its transport is opening) reads the epoch when it starts, so
        // the bump makes its install refuse the client and close it — the
        // "started before, finishes after the drain" window, which the flag
        // alone cannot close.
        self.shutdown_epoch.fetch_add(1, Ordering::SeqCst);
        // Abort the bounded-startup continuations BEFORE the drain: each is a
        // task whose future owns a transport a later batch started, so
        // aborting it drops the transport (and kills a spawned child) instead
        // of leaving a connection this teardown cannot reach.
        self.abort_background_discovery();
        // Drain the map under the lock BEFORE the shutdown loop: a connection
        // registered while the loop is running is taken by the drain instead
        // of leaking past the old snapshot-then-clear sequence (a snapshot
        // could miss it, and the trailing clear would drop it un-shut-down).
        let drained: BTreeMap<String, Arc<ServerConnection>> =
            std::mem::take(&mut *self.connections.lock().await);
        for connection in drained.into_values() {
            connection.shutdown().await;
        }
        // Every drained connection published `Closed` into the mirror on its
        // way out (`ServerConnection::shutdown` -> `set_state`); a torn-down
        // manager must not keep rows for a prompt section to render.
        self.states_mirror
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clear();
        // Cleared only after every drained connection is down: the flag's job
        // is to fail closed for callers racing THIS teardown (they would build
        // a connection the drain already passed). A caller that starts after
        // this point builds a connection the next teardown reaches — which is
        // what the plugin `/reload` path (`on_reload` -> `shutdown`, then
        // re-discovery on the same manager) depends on.
        self.shutting_down.store(false, Ordering::SeqCst);
    }

    /// Abort every background continuation of a bounded startup discovery
    /// (see [`Self::spawn_background_discovery`]). Called at the START of the
    /// teardown, before the connection drain.
    fn abort_background_discovery(&self) {
        let mut tasks = self
            .background_discovery
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for task in tasks.drain(..) {
            task.abort();
        }
    }

    /// Number of live background discovery continuations (diagnostics/tests).
    pub fn background_discovery_count(&self) -> usize {
        let mut tasks = self
            .background_discovery
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        tasks.retain(|task| !task.is_finished());
        tasks.len()
    }

    /// Drop one server's live connection (its registry entry was removed or
    /// changed, see [`crate::plugins::builtin::McpPlugin`]'s config sync).
    /// The child process is killed and reaped by the transport; the next use
    /// connects again from the registry's CURRENT entry, so a config change
    /// takes effect and a re-added entry never runs the removed entry's
    /// command. A no-op without a live connection.
    ///
    /// The connection is DETACHED before it leaves the map (see
    /// [`ServerConnection::detached`]): a call already holding its `Arc` must
    /// not reconnect through it — `McpManager::shutdown` cannot reach an
    /// object that is no longer in the map, so that child would outlive the
    /// session. The flag is set BEFORE the removal, both under the same
    /// `connections` lock, so no observer can obtain a fresh connection for
    /// the name before the old object stopped being reconnectable.
    pub async fn disconnect(&self, server: &str) {
        let connection = {
            let mut connections = self.connections.lock().await;
            if let Some(connection) = connections.get(server) {
                connection.detach();
            }
            connections.remove(server)
        };
        if let Some(connection) = connection {
            connection.shutdown().await;
        }
        // The row belonged to the connection that just left the map; the next
        // use builds a fresh one from the registry's CURRENT entry, so the
        // prompt section must not keep reporting the old state.
        self.forget_mirrored_state(server);
    }

    /// Drop `server`'s discovered snapshot entry: the removal of a registry
    /// entry must release the state derived from it (the entry's absence
    /// keeps the next rebuild's registration pass from re-registering
    /// `mcp__*` bridges for a server that no longer exists, and a re-added
    /// entry must be discovered afresh instead of inheriting the removed
    /// entry's tools).
    ///
    /// The flag stays untouched: the only caller is the plugin's config sync,
    /// which runs inside a registry rebuild whose registration pass reads the
    /// snapshot right after — that rebuild already reflects the removal, and
    /// marking dirty would only schedule a redundant one.
    pub fn forget_discovered(&self, server: &str) {
        self.discovered
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(server);
    }

    fn set_discovered_snapshot(&self, snapshot: BTreeMap<String, Vec<DiscoveredTool>>) {
        *self
            .discovered
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = snapshot;
    }

    /// Publish one server's freshly listed tools: replace its entry in the
    /// discovered snapshot and mark the plugin registry dirty when the
    /// published entry differs from the previous one (the host seam consumes
    /// the flag and rebuilds). Returns whether it differed.
    ///
    /// "Differs" covers everything the re-registration pass feeds to the
    /// model, not just the tool-name set: the entry appearing at all, its
    /// ordered tool names, and each tool's description (its title fallback
    /// included), input schema, output schema and exposure — the fields
    /// [`crate::plugins::builtin::McpToolBridge`] bakes into the registered
    /// tool. A server that keeps its names while a schema changes must
    /// therefore rebuild too; comparing names alone would leave the model on
    /// the stale definitions until some other change.
    ///
    /// The comparison is per server and per tool index (order-sensitive, like
    /// the name-set comparison it replaces) and runs under the same
    /// `discovered` write lock that publishes, which no await is held across:
    /// the cost is this server's tool list, not the whole snapshot.
    fn publish_discovered_tools(&self, server: &str, tools: &[DiscoveredTool]) -> bool {
        let changed = {
            let mut snapshot = self
                .discovered
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let changed = match snapshot.get(server) {
                Some(previous) => tool_definitions_differ(previous, tools),
                // A server without an entry always joins the registry, even
                // with an empty tool list (the entry itself is new).
                None => true,
            };
            snapshot.insert(server.to_owned(), tools.to_vec());
            changed
        };
        if changed {
            self.tools_dirty.store(true, Ordering::SeqCst);
        }
        changed
    }
}

/// The transport kind of a server config (`"stdio"` / `"http"`), the label
/// `pir mcp check` prints.
fn transport_label(config: &McpServerConfig) -> &'static str {
    match config {
        McpServerConfig::Stdio(_) => "stdio",
        McpServerConfig::Http(_) => "http",
    }
}

/// One server's status row (upstream `/mcp`'s per-server line,
/// index.ts:1021-1037): everything a manager UI shows without connecting.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpServerStatus {
    pub name: String,
    /// The entry's CURRENT config (the same object the connection and the
    /// registration pass read), so per-tool exposure resolves here instead of
    /// a UI re-reading `mcp.json`.
    pub config: McpServerConfig,
    /// Whether the file-level `builtinTools` default applies to this entry
    /// (see [`crate::config::mcp::McpServerConfigBase::effective_exposure`]).
    pub builtin_tools: bool,
    /// The entry's effective BASE exposure under that default (upstream
    /// `exposureOf`, index.ts:136). Per-tool overrides are resolved by
    /// [`Self::exposure_for_tool`].
    pub exposure: McpExposure,
    /// Whether the entry is enabled (`enabled: false` keeps it configured but
    /// unconnected, upstream `isEnabled`).
    pub enabled: bool,
    /// The live connection state; `Idle` when no connection object exists
    /// (configured, never used yet).
    pub state: ConnectionState,
    /// The tools of the last successful discovery for this server. Empty
    /// until a discovery published a snapshot, and kept (upstream
    /// `connection.tools`) across a drop.
    pub tools: Vec<String>,
    /// `"stdio"` or `"http"`.
    pub transport: String,
    /// Which `mcp.json` / extension registered the server.
    pub source: McpServerSource,
}

impl McpServerStatus {
    /// The tools this server currently offers (the last discovery).
    pub fn tool_count(&self) -> usize {
        self.tools.len()
    }

    /// The exposure of one of this server's tools: its `toolExposure`
    /// override first, then the base — exactly
    /// [`crate::config::mcp::McpServerConfig::effective_tool_exposure`] under
    /// this server's `builtinTools` applicability.
    pub fn exposure_for_tool(&self, tool: &str) -> McpExposure {
        self.config
            .effective_tool_exposure(tool, self.builtin_tools)
    }

    /// The error text shown next to the state (upstream `connection.error`).
    pub fn error(&self) -> Option<&str> {
        self.state.error()
    }

    /// The bare state word (upstream `formatStatus`'s `state` variable,
    /// index.ts:1028-1032): `disabled` wins over the connection state, and an
    /// unconnected server is `configured` (not started yet — upstream's
    /// "starting" is a connect already queued, which pir expresses as
    /// `Connecting`). The error is NOT folded in; [`Self::state_label`] adds
    /// it for the menu form.
    pub fn state_word(&self) -> String {
        if !self.enabled {
            return "disabled".to_owned();
        }
        match &self.state {
            ConnectionState::Idle => "configured".to_owned(),
            ConnectionState::Connecting => "connecting".to_owned(),
            ConnectionState::Connected => "connected".to_owned(),
            ConnectionState::Disconnected { .. } => {
                "disconnected, reconnects on next call".to_owned()
            }
            ConnectionState::Failed { .. } => "failed".to_owned(),
            ConnectionState::NeedsAuth { .. } => "needs sign-in".to_owned(),
            ConnectionState::Closed => "closed".to_owned(),
        }
    }

    /// The short state label of the one-line menu form (upstream
    /// `describeState(server, true)`, index.ts:143-162): [`Self::state_word`]
    /// with the first line of a connect failure appended. Only `failed`
    /// folds the error in — `needs-auth`, `disconnected` and `closed` are
    /// labels of their own upstream.
    pub fn state_label(&self) -> String {
        match &self.state {
            ConnectionState::Failed { error } if self.enabled => {
                format!("failed: {}", first_line_text(error))
            }
            _ => self.state_word(),
        }
    }
}

fn first_line_text(text: &str) -> &str {
    text.lines().next().unwrap_or(text)
}

/// The full status surface: every configured server plus the config problems
/// and the extension registrations an `mcp.json` entry shadows (upstream
/// `/mcp`'s `servers` / `configErrors` / `overridden`, index.ts:1017-1042).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpStatusSnapshot {
    pub servers: Vec<McpServerStatus>,
    /// Formatted `"<path>: <problem>"` config problems (upstream
    /// `configErrors`).
    pub config_errors: Vec<String>,
    /// Extension registrations an `mcp.json` entry of the same name shadows
    /// (upstream `overridden`, index.ts:345-361). Filled by the plugin's
    /// registration pass, which is the only place that sees a registration
    /// lose to a config entry; empty for a bare manager.
    pub overridden: Vec<String>,
    /// The agent dir the global `mcp.json` comes from (for the empty-state
    /// message).
    pub agent_dir: PathBuf,
}

impl McpStatusSnapshot {
    /// The plain-text status a non-TUI caller can print (upstream
    /// `formatStatus`, index.ts:1017-1042 — same content shape):
    /// one `name: <state>, <n> tools (<exposure>)` line per server with the
    /// error text indented under it, then `config error: ...` and
    /// `overridden: ...` lines.
    pub fn format_status(&self) -> String {
        if self.servers.is_empty() && self.config_errors.is_empty() && self.overridden.is_empty() {
            return format!(
                "No MCP servers configured. Add them to {} or .pi/mcp.json.",
                crate::config::mcp::global_mcp_path(&self.agent_dir).display()
            );
        }
        let mut lines = Vec::new();
        for server in &self.servers {
            if let ConnectionState::NeedsAuth { .. } = server.state {
                lines.push(format!(
                    "{}: needs sign-in, run /mcp login {} ({})",
                    server.name,
                    server.name,
                    exposure_label(server.exposure)
                ));
                continue;
            }
            // Upstream prints the tool count only for a connected server: a
            // dropped connection's list is what it last offered, not what it
            // offers now (`formatStatus` line 1027).
            let tools = if server.state.is_connected() {
                format!(
                    ", {} {}",
                    server.tool_count(),
                    if server.tool_count() == 1 {
                        "tool"
                    } else {
                        "tools"
                    }
                )
            } else {
                String::new()
            };
            // The bare state word: upstream prints the error on its own
            // indented lines below the row (index.ts:1033-1036), never
            // folded into the state text.
            let error = match (&server.state, server.error()) {
                (ConnectionState::Connected, _) | (_, None) => String::new(),
                (_, Some(error)) => {
                    let indented = error.split('\n').collect::<Vec<_>>().join("\n    ");
                    format!("\n    {indented}")
                }
            };
            lines.push(format!(
                "{}: {}{} ({}){}",
                server.name,
                server.state_word(),
                tools,
                exposure_label(server.exposure),
                error
            ));
        }
        for error in &self.config_errors {
            lines.push(format!("config error: {error}"));
        }
        for line in &self.overridden {
            lines.push(format!("overridden: {line}"));
        }
        lines.join("\n")
    }
}

/// The short exposure label (`exposureOf`'s wire spelling).
pub fn exposure_label(exposure: McpExposure) -> &'static str {
    match exposure {
        McpExposure::Codemode => "codemode",
        McpExposure::Deferred => "deferred",
        McpExposure::Direct => "direct",
        McpExposure::Hidden => "hidden",
    }
}

/// Whether two freshly listed tool lists differ in anything the
/// re-registration pass reads: the per-index tool name (so a reordering or a
/// rename counts), description, title (the bridge's description fallback),
/// input schema, output schema, or exposure. Length differences are
/// differences. Used for the dirty-flag comparisons of
/// [`McpManager::discover_all`] and [`McpManager::publish_discovered_tools`].
fn tool_definitions_differ(previous: &[DiscoveredTool], current: &[DiscoveredTool]) -> bool {
    previous.len() != current.len()
        || previous.iter().zip(current).any(|(previous, current)| {
            previous.tool.name != current.tool.name
                || previous.tool.description != current.tool.description
                || previous.tool.title != current.tool.title
                || previous.tool.input_schema != current.tool.input_schema
                || previous.tool.output_schema != current.tool.output_schema
                || previous.exposure != current.exposure
        })
}

impl ServerConnection {
    /// The current readiness, resolving the two lazy transitions:
    /// `Ready -> Stale` when the client was replaced or a
    /// `tools/list_changed` arrived since the recorded discovery, and
    /// `Failed -> Idle` once the failure backoff elapsed.
    fn readiness(&self) -> ServerReadiness {
        let state = ServerReadiness::from_code(self.readiness.load(Ordering::SeqCst));
        match state {
            ServerReadiness::Ready if !self.ready_is_current() => {
                let _ = self.readiness.compare_exchange(
                    ServerReadiness::Ready.code(),
                    ServerReadiness::Stale.code(),
                    Ordering::SeqCst,
                    Ordering::SeqCst,
                );
                ServerReadiness::Stale
            }
            ServerReadiness::Failed if !self.failure_backoff_active() => {
                let _ = self.readiness.compare_exchange(
                    ServerReadiness::Failed.code(),
                    ServerReadiness::Idle.code(),
                    Ordering::SeqCst,
                    Ordering::SeqCst,
                );
                ServerReadiness::Idle
            }
            state => state,
        }
    }

    /// Whether the recorded discovery still matches the live client and the
    /// notification generation (the readiness cache key).
    fn ready_is_current(&self) -> bool {
        if self.ready_client_instance.load(Ordering::SeqCst)
            != self.client_instance.load(Ordering::SeqCst)
        {
            return false;
        }
        let invalidation = self
            .ready_invalidation
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match invalidation.as_ref() {
            Some(invalidation) => {
                invalidation.tools_generation() == self.ready_generation.load(Ordering::SeqCst)
            }
            None => false,
        }
    }

    /// Whether the cached failure still suppresses retries.
    fn failure_backoff_active(&self) -> bool {
        self.readiness_failure
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
            .is_some_and(|failure| Instant::now() < failure.retry_after)
    }

    /// The cached failure while its backoff is active.
    fn failure_backoff_error(&self) -> Option<McpError> {
        let failure = self
            .readiness_failure
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match failure.as_ref() {
            Some(failure) if Instant::now() < failure.retry_after => Some(failure.error.clone()),
            _ => None,
        }
    }

    /// Enter `Discovering`; the drop guard restores `Stale` when the
    /// discovery future is cancelled before it records `Ready`/`Failed`.
    fn begin_discovery(&self) -> DiscoveryGuard<'_> {
        self.readiness
            .store(ServerReadiness::Discovering.code(), Ordering::SeqCst);
        DiscoveryGuard { connection: self }
    }

    /// Record a successful `tools/list`: the cache key becomes
    /// `(client instance, generation)` and the state `Ready`.
    fn mark_ready(&self, client: &McpClient, generation: u64) {
        let invalidation = Arc::clone(client.discovery_invalidation());
        *self
            .ready_invalidation
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(invalidation);
        self.ready_generation.store(generation, Ordering::SeqCst);
        self.ready_client_instance
            .store(client.instance_id(), Ordering::SeqCst);
        *self
            .readiness_failure
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
        self.readiness
            .store(ServerReadiness::Ready.code(), Ordering::SeqCst);
    }

    /// Record a failed discovery and start its bounded retry backoff.
    fn mark_failed(&self, error: McpError) {
        *self
            .ready_invalidation
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
        *self
            .readiness_failure
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(ReadinessFailure {
            error,
            retry_after: Instant::now() + READINESS_FAILURE_BACKOFF,
        });
        self.readiness
            .store(ServerReadiness::Failed.code(), Ordering::SeqCst);
    }

    /// Forget the cached readiness AND the failure backoff (connection
    /// dropped, reconnected, or disconnected by a config change).
    fn reset_readiness(&self) {
        *self
            .ready_invalidation
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
        *self
            .readiness_failure
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
        self.readiness
            .store(ServerReadiness::Idle.code(), Ordering::SeqCst);
    }

    /// Mark this connection as detached from the manager (see
    /// [`ServerConnection::detached`]): it must never open another transport.
    /// [`McpManager::disconnect`] calls this under the `connections` map lock,
    /// before the removal, so no observer can obtain a fresh connection for
    /// the name while this object is still reconnectable; the shutdown paths
    /// ([`ServerConnection::shutdown`]) call it as the transport closes.
    fn detach(&self) {
        self.detached.store(true, Ordering::SeqCst);
    }

    /// The live client, connecting lazily with HTTP retry backoff (upstream
    /// `getClient` / `open`).
    ///
    /// A DETACHED connection (see [`ServerConnection::detached`]) never
    /// reconnects: it is out of the manager's map, so a transport opened here
    /// would use the replaced or removed entry's config and no manager path
    /// (`McpManager::shutdown` included) could stop it. The check runs at
    /// entry — covering every reconnect, `call_tool`'s post-drop retry
    /// included — and again before each connect attempt and before its client
    /// is installed.
    ///
    /// The slot mutex is never held across the connect's retry/backoff waits
    /// or a `close()`: one caller takes the `connecting` token and performs
    /// the connect, while concurrent callers wait for `settled` and re-check
    /// the slot. Callers therefore observe the finished connect (or retry
    /// after a failure) instead of serializing behind an unreachable server,
    /// and `drop_client` / `disconnect` stay responsive.
    async fn get_client(&self) -> Result<Arc<McpClient>, McpError> {
        loop {
            if self.detached.load(Ordering::SeqCst) || self.shutting_down.load(Ordering::SeqCst) {
                return Err(McpError::ConnectionClosed);
            }
            // Subscribe BEFORE inspecting the slot: a watch receiver observes
            // a version bump that happened after this point even if it only
            // awaits later, so a completion cannot be slept through.
            let mut settled = self.settled.subscribe();
            // Clone the live client out of the slot and release the slot lock
            // BEFORE probing its transport: `transport_is_closed` awaits the
            // client's own service lock, and holding the slot across that
            // await would stall `drop_client` (which needs the slot) for as
            // long as the service lock is busy.
            let installed = self.client.lock().await.clone();
            let dead = match installed.as_ref() {
                // A live transport: only return a client that is still the
                // installed one (a concurrent close may have taken this
                // session out of the slot while the probe ran); a replacement
                // is re-examined by the next loop iteration.
                Some(client) if !client.transport_is_closed().await => {
                    if self.slot_holds(client).await {
                        return Ok(Arc::clone(client));
                    }
                    continue;
                }
                // The transport dropped (server exit, closed HTTP stream):
                // release the dead session before reconnecting.
                Some(_) => true,
                None => false,
            };
            let closing = self.closing.load(Ordering::SeqCst);
            if dead {
                // The close runs outside the slot lock (the client bounds it by
                // `CLOSE_TIMEOUT`). Only the dead client this loop probed is
                // released: a concurrent reconnect may already have installed
                // a replacement, which must not be torn down here.
                if let Some(client) = installed.as_ref() {
                    if self.slot_holds(client).await {
                        self.drop_client().await;
                    }
                }
                continue;
            }
            if closing {
                // A concurrent close owns the previous session; start no
                // connect over it and wait for the close to settle.
                let _ = settled.changed().await;
                continue;
            }
            if self
                .connecting
                .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
                .is_err()
            {
                // Another caller owns the single-flight connect: wait for it
                // to settle, then re-check the installed client (or retry).
                let _ = settled.changed().await;
                continue;
            }
            let flight = ConnectFlight { connection: self };
            // Boxed as a `dyn Future + Send` so this extra future layer does
            // not deepen the caller's generator type (the RPC dispatch future
            // already sits close to rustc's auto-trait recursion limit); the
            // allocation happens once per actual connect, not per call.
            let connect: std::pin::Pin<
                Box<dyn std::future::Future<Output = Result<Arc<McpClient>, McpError>> + Send + '_>,
            > = Box::pin(self.connect_single_flight());
            let result = connect.await;
            drop(flight);
            return result;
        }
    }

    /// Whether `client` is still the one installed in the slot. One short
    /// slot-lock acquisition, never held across an await: the transport probe
    /// and the close both run with the lock released (see
    /// [`ServerConnection::get_client`]).
    async fn slot_holds(&self, client: &Arc<McpClient>) -> bool {
        self.client
            .lock()
            .await
            .as_ref()
            .is_some_and(|current| Arc::ptr_eq(current, client))
    }

    /// The connect body of [`ServerConnection::get_client`], run by the one
    /// caller holding the `connecting` token. Holds the slot mutex only to
    /// install the client; the retry delays and transport connects run
    /// without it.
    async fn connect_single_flight(&self) -> Result<Arc<McpClient>, McpError> {
        if self.detached.load(Ordering::SeqCst) || self.shutting_down.load(Ordering::SeqCst) {
            return Err(McpError::ConnectionClosed);
        }
        // The epoch this connect runs under. `shutdown` bumps it before the
        // drain, so an install that started under an older epoch is refused
        // (see the install site): that closes the window where a transport was
        // already opening when the teardown began, which the flag alone misses
        // because the flag is only read before the attempt.
        let epoch = self.shutdown_epoch.load(Ordering::SeqCst);
        let entry = self.entry.read().await.clone();
        let retries: Vec<u64> = match &entry.config {
            McpServerConfig::Http(_) => CONNECT_RETRY_DELAYS_MS.to_vec(),
            McpServerConfig::Stdio(_) => Vec::new(),
        };
        self.set_state(ConnectionState::Connecting);
        self.log
            .write_event(&entry.name, &format!("connecting ({})", source_of(&entry)));
        let mut last_error = McpError::ConnectionClosed;
        for (attempt, delay) in std::iter::once(0u64).chain(retries).enumerate() {
            if attempt > 0 {
                tokio::time::sleep(Duration::from_millis(delay)).await;
            }
            if self.detached.load(Ordering::SeqCst) || self.shutting_down.load(Ordering::SeqCst) {
                return Err(McpError::ConnectionClosed);
            }
            // Every attempt starts with no diagnostics of its own (upstream
            // `open()`: `this.stderrTail = undefined`).
            *self
                .stderr_tail
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
            match self.connect_once(&entry).await {
                Ok(client) => {
                    let installed = {
                        let mut slot = self.client.lock().await;
                        // The epoch check closes the in-flight window: a
                        // teardown that began while this transport was opening
                        // bumped it, so the client is refused (and closed
                        // below) instead of reaching the state `Connected`.
                        let teardown_landed = self.shutdown_epoch.load(Ordering::SeqCst) != epoch;
                        if self.detached.load(Ordering::SeqCst)
                            || self.shutting_down.load(Ordering::SeqCst)
                            || teardown_landed
                        {
                            false
                        } else {
                            *slot = Some(client.clone());
                            // A new client invalidates every readiness record
                            // made against the previous one (reconnect).
                            self.client_instance
                                .store(client.instance_id(), Ordering::SeqCst);
                            self.set_state(ConnectionState::Connected);
                            true
                        }
                    };
                    if !installed {
                        // A disconnect/shutdown landed while the transport was
                        // opening: do not leave that child behind.
                        client.close().await;
                        return Err(McpError::ConnectionClosed);
                    }
                    self.log.write_event(&entry.name, "connected");
                    return Ok(client);
                }
                Err(error) => {
                    last_error = error;
                    let transient = match &last_error {
                        McpError::Http { status, .. } => http::is_transient_status(*status),
                        McpError::ConnectionClosed => true,
                        // Only reset-class transport failures are transient;
                        // DNS/connect/timeout failures carry their cause and
                        // are not retried.
                        McpError::Transport(message) => {
                            http::is_transient_transport_message(message)
                        }
                        _ => false,
                    };
                    if !transient {
                        break;
                    }
                }
            }
        }
        // An OAuth or `auth: { provider }` server that rejected the (possibly
        // absent) bearer token needs a sign-in; the connection reports it as
        // such instead of the raw 401 (upstream `connectFailed` ->
        // `needsSignIn`/`markNeedsAuth`, runtime.ts:437-446). The remedy comes
        // off the config: `/mcp` for OAuth, `/login <provider>` for a
        // provider token (runtime.ts:77-80).
        if crate::mcp::oauth::has_sign_in_remedy(&entry.config)
            && crate::mcp::oauth::is_authorization_required(&last_error)
        {
            let sign_in =
                crate::mcp::oauth::sign_in_required_message_for(&entry.config, &entry.name);
            self.set_state(ConnectionState::NeedsAuth {
                message: sign_in.clone(),
            });
            self.log.write_event(&entry.name, &sign_in);
            return Err(match crate::mcp::oauth::auth_provider(&entry.config) {
                Some(provider) => McpError::ProviderLoginRequired {
                    server: entry.name.clone(),
                    provider: provider.to_owned(),
                },
                None => McpError::LoginRequired {
                    server: entry.name.clone(),
                },
            });
        }
        let stderr = self
            .stderr_tail
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        let message = match stderr {
            // A server that died during `initialize` usually says why on
            // stderr; upstream `connectFailed` appends the tail (bounded by
            // `STDERR_TAIL_CHARS`).
            Some(stderr) => format!(
                "MCP server \"{}\" failed to connect: {last_error}\n{}",
                entry.name,
                stderr_tail_chars(&stderr, STDERR_TAIL_CHARS)
            ),
            None => format!(
                "MCP server \"{}\" failed to connect: {last_error}",
                entry.name
            ),
        };
        self.set_state(ConnectionState::Failed {
            error: message.clone(),
        });
        self.log.write_event(&entry.name, &message);
        Err(last_error)
    }

    /// The current client, if one is connected (never connects; the
    /// notification drain must not start a server).
    async fn current_client(&self) -> Option<Arc<McpClient>> {
        self.client.lock().await.clone()
    }

    async fn connect_once(&self, entry: &McpServerEntry) -> Result<Arc<McpClient>, McpError> {
        let timeout = Duration::from_millis(entry.timeout_ms());
        let client = match &entry.config {
            McpServerConfig::Stdio(config) => {
                let options = stdio_options(config, &self.cwd, entry)?;
                // Spawn first, connect second: a handshake that fails still
                // leaves the server's stderr reachable for the failure
                // message (upstream runtime.ts `connectOnce`'s stderrTail).
                let connection =
                    crate::mcp::stdio::spawn(options, Some(Arc::clone(&self.log)), &entry.name)?;
                let tail = Arc::clone(&connection.stderr_tail);
                let result = McpClient::connect_stdio_spawned(connection, timeout).await;
                let tail = {
                    let bytes = tail
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    String::from_utf8_lossy(&bytes).trim().to_string()
                };
                *self
                    .stderr_tail
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) =
                    (!tail.is_empty()).then_some(tail);
                result?
            }
            McpServerConfig::Http(config) => {
                let headers = resolve_headers(config, entry)?;
                // `auth: { provider }` sends that pi provider's current token
                // (upstream `modelRegistry.getApiKeyForProvider`); every other
                // server carries the stored OAuth bearer token, refreshed when
                // it is about to expire (upstream `createMcpAuthProvider`).
                let auth_token = match config.auth.as_ref() {
                    Some(auth) => self.provider_token(&auth.provider).await,
                    None => self.resolve_oauth_token(config, entry).await,
                };
                *self
                    .last_auth_token
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = auth_token.clone();
                McpClient::connect_http_with_auth(config.url.clone(), headers, auth_token, timeout)
                    .await?
            }
        };
        Ok(Arc::new(client))
    }

    /// The current token of the pi provider a server's `auth` names, resolved
    /// through the normal credential chain
    /// ([`crate::ai::auth::resolve::resolve_provider_token`]: a stored OAuth
    /// credential with the flow's locked refresh, a stored api key, then the
    /// provider's `models.json` key and environment keys) on every call, so a
    /// provider refresh applies without MCP storing a copy (upstream
    /// `providerToken: async (provider) => modelRegistry?.getApiKeyForProvider(provider)`,
    /// runtime.ts:196-199).
    ///
    /// Resolution failures leave the connection without a token (the server's
    /// 401 then reports the provider sign-in) and are logged for diagnosis.
    async fn provider_token(&self, provider: &str) -> Option<String> {
        match crate::ai::auth::resolve::resolve_provider_token(&self.agent_dir, provider).await {
            Ok(token) => token,
            Err(error) => {
                self.log.write_event(
                    &self.name,
                    &format!(
                        "could not resolve the {provider} provider token: {}",
                        error.user_message()
                    ),
                );
                None
            }
        }
    }

    /// Resolve the provider token again after the server rejected it with a
    /// 401, refreshing a stored OAuth credential even when it has not reached
    /// `expires` (the server can revoke it early). Returns the token to retry
    /// with, or `None` when the provider has nothing newer to offer — the user
    /// then has to sign in (upstream `authProvider` has no provider-token
    /// refresh path; this is the local equivalent of the OAuth `onUnauthorized`
    /// refresh, scoped to the provider credential).
    async fn refresh_provider_token(
        &self,
        provider: &str,
        rejected: Option<&str>,
    ) -> Option<String> {
        match crate::ai::auth::resolve::refresh_provider_token_after_rejection(
            &self.agent_dir,
            provider,
            rejected,
        )
        .await
        {
            Ok(token) => token,
            Err(error) => {
                self.log.write_event(
                    &self.name,
                    &format!(
                        "could not refresh the {provider} provider token: {}",
                        error.user_message()
                    ),
                );
                None
            }
        }
    }

    /// The stored OAuth access token for an OAuth-capable HTTP server
    /// (upstream `usesOAuth` + `createMcpAuthProvider().token`). Servers with
    /// an `Authorization` header or an `auth` provider are left untouched;
    /// resolution failures fall through to the stored (or absent) token.
    async fn resolve_oauth_token(
        &self,
        config: &crate::config::mcp::McpHttpServerConfig,
        entry: &McpServerEntry,
    ) -> Option<String> {
        let url = crate::mcp::oauth::oauth_server_url(&entry.config)?;
        let settings =
            crate::mcp::oauth::McpOAuthSettings::from_config(config, &entry.name).ok()?;
        let store = self.oauth.for_server(&entry.name, url);
        crate::mcp::oauth::resolve_access_token(&store, &settings, url).await
    }

    /// Refresh the OAuth tokens after a 401 (upstream transport
    /// `onUnauthorized` -> `refresh`). Returns whether a fresh token is now
    /// stored for the next connection.
    async fn renew_oauth_token(&self) -> bool {
        let entry = self.entry.read().await.clone();
        let McpServerConfig::Http(config) = &entry.config else {
            return false;
        };
        let Some(url) = crate::mcp::oauth::oauth_server_url(&entry.config) else {
            return false;
        };
        let Ok(settings) = crate::mcp::oauth::McpOAuthSettings::from_config(config, &entry.name)
        else {
            return false;
        };
        let store = self.oauth.for_server(&entry.name, url);
        let stale = self
            .last_auth_token
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        match crate::mcp::oauth::refresh_access_token(&store, &settings, url, stale.as_deref())
            .await
        {
            Ok(token) => {
                *self
                    .last_auth_token
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(token);
                true
            }
            Err(_) => false,
        }
    }

    /// Re-authorize after a SCOPE challenge (`insufficient_scope`).
    ///
    /// A token refresh cannot widen a grant (RFC 6749 §6: a refresh may only
    /// narrow the scope), so the plain [`Self::renew_oauth_token`] path is
    /// guaranteed to fail again on the same server answer. The step-up runs the
    /// authorization-code flow instead with the challenged scopes merged onto
    /// the granted ones (`step_up_scope`, upstream `stepUpScope`), which is
    /// what [`crate::mcp::oauth::sign_in`] does when handed a challenge whose
    /// `error` is `insufficient_scope` (it also skips the futile refresh).
    ///
    /// Headless by design: the prompt renders the authorization URL to the
    /// server's MCP log and opens the system browser, then waits for the
    /// loopback callback (there is no terminal to paste a redirect URL into —
    /// the same contract as the CLI's non-interactive prompt). The wait is
    /// bounded by [`STEP_UP_TIMEOUT`] and the caller's abort signal.
    /// Returns whether a fresh token is now stored.
    async fn step_up_oauth_token(
        &self,
        challenge: &crate::mcp::oauth::OAuthChallenge,
        signal: Option<&crate::ai::types::AbortSignal>,
    ) -> bool {
        let entry = self.entry.read().await.clone();
        let McpServerConfig::Http(config) = &entry.config else {
            return false;
        };
        let Some(url) = crate::mcp::oauth::oauth_server_url(&entry.config) else {
            return false;
        };
        let Ok(settings) = crate::mcp::oauth::McpOAuthSettings::from_config(config, &entry.name)
        else {
            return false;
        };
        let store = self.oauth.for_server(&entry.name, url);
        // A scope challenge is what the transport classified this as; make it
        // explicit so `sign_in` merges the granted scope and skips the refresh
        // even when the server omitted the `error` parameter from its
        // `WWW-Authenticate` header.
        let mut challenge = challenge.clone();
        challenge.error = Some("insufficient_scope".to_owned());
        let escalated = if let Some(granted) = store.tokens().await.and_then(|tokens| tokens.scope)
        {
            crate::mcp::oauth::step_up_scope(Some(&granted), challenge.scope.as_deref())
                .or(challenge.scope.clone())
        } else {
            challenge.scope.clone()
        };
        self.log.write_event(
            &entry.name,
            &format!(
                "authorization requires more scope ({}); starting a step-up sign-in",
                escalated.as_deref().unwrap_or("unknown")
            ),
        );
        let prompt = StepUpSignInPrompt {
            log: Arc::clone(&self.log),
            server: entry.name.clone(),
        };
        let sign_in = crate::mcp::oauth::sign_in(
            &store,
            &settings,
            url,
            Some(&challenge),
            &prompt,
            STEP_UP_TIMEOUT,
        );
        let authorized = match signal {
            Some(signal) => tokio::select! {
                result = sign_in => result.is_ok(),
                () = signal.cancelled() => false,
            },
            None => sign_in.await.is_ok(),
        };
        if authorized {
            let token = store.tokens().await.map(|tokens| tokens.access_token);
            *self
                .last_auth_token
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = token;
        } else {
            self.log.write_event(
                &entry.name,
                "scope step-up sign-in did not complete; run `/mcp login` to authorize",
            );
        }
        authorized
    }

    /// Call one tool, retrying once on a dropped/expired session (upstream
    /// `withClient`).
    async fn call_tool(
        &self,
        tool: &str,
        arguments: &JsonValue,
        signal: Option<&crate::ai::types::AbortSignal>,
    ) -> Result<CallToolResult, McpError> {
        for attempt in 0..2 {
            // Check before every dispatch (upstream `StreamableHttpTransport`
            // checks its per-request signal before each `authorizedFetch`
            // attempt, #10598): a call that was cancelled or timed out while
            // waiting here — for example on the shared OAuth refresh below —
            // must not be dispatched once that wait returns.
            if signal.is_some_and(crate::ai::types::AbortSignal::is_aborted) {
                return Err(McpError::Aborted);
            }
            let client = self.get_client().await?;
            // Re-check right before the request itself: connecting may have
            // taken as long as the explicit wait this gate exists for.
            if signal.is_some_and(crate::ai::types::AbortSignal::is_aborted) {
                return Err(McpError::Aborted);
            }
            match client.call_tool_with_signal(tool, arguments, signal).await {
                Ok(result) => return Ok(result),
                // The caller gave up mid-flight: the request was cancelled on
                // the server (`notifications/cancelled`) and must not be
                // retried on a fresh connection.
                Err(error @ McpError::Aborted) => return Err(error),
                // `SessionExpired` is kept for upstream parity
                // (`McpSessionExpiredError`); rmcp transparently recovers
                // expired sessions (`session_recovery_timeout`), so pir never
                // constructs it and this arm is exercised via
                // `ConnectionClosed`.
                Err(error @ (McpError::SessionExpired | McpError::ConnectionClosed)) => {
                    // The server no longer knows the session (restart,
                    // deploy), so it did not run the request: retry once on a
                    // fresh connection.
                    self.drop_client().await;
                    if attempt == 1 {
                        return Err(error);
                    }
                }
                // A 401/403 bearer challenge. An `auth: { provider }` server
                // sends a provider token, not an OAuth grant: a 401 re-reads
                // that provider's CURRENT token and retries once on a fresh
                // connection (the token is attached when the transport is
                // built), never starting an OAuth flow. A scope challenge
                // (`insufficient_scope`) cannot be satisfied by a refresh —
                // RFC 6749 §6 lets a refresh narrow scopes, never widen them —
                // so it re-authorizes through the step-up flow with the
                // challenged scopes merged onto the granted ones. Everything
                // else refreshes the stored tokens; without a usable refresh
                // token the server needs a fresh sign-in. Non-OAuth servers
                // keep their original error.
                Err(error) if crate::mcp::oauth::is_authorization_required(&error) => {
                    let (auth_provider, oauth_capable, server_name) = {
                        let entry = self.entry.read().await;
                        (
                            crate::mcp::oauth::auth_provider(&entry.config).map(str::to_owned),
                            crate::mcp::oauth::oauth_server_url(&entry.config).is_some(),
                            entry.name.clone(),
                        )
                    };
                    if let Some(provider) = auth_provider {
                        // Only a 401 is a stale/missing provider token; a 403
                        // scope challenge is the OAuth step-up case and must
                        // not be answered by re-reading a provider token.
                        if !crate::mcp::oauth::is_unauthorized(&error) {
                            return Err(error);
                        }
                        self.drop_client().await;
                        if attempt == 1 {
                            // Upstream `markNeedsAuth` (runtime.ts:326): the
                            // provider's current token was already rejected, so
                            // the user must sign in to the provider.
                            let message = crate::mcp::oauth::sign_in_required_message_provider(
                                &server_name,
                                &provider,
                            );
                            self.set_state(ConnectionState::NeedsAuth {
                                message: message.clone(),
                            });
                            self.log.write_event(&server_name, &message);
                            return Err(McpError::ProviderLoginRequired {
                                server: server_name,
                                provider,
                            });
                        }
                        // Refresh the provider's credential BEFORE deciding the
                        // server needs a sign-in: a stored OAuth access token
                        // can be revoked while it still looks valid, and only a
                        // refresh can recover it. The re-resolution also picks
                        // up a `/login`/refresh that landed since the
                        // connection opened.
                        let rejected = self
                            .last_auth_token
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .clone();
                        match self
                            .refresh_provider_token(&provider, rejected.as_deref())
                            .await
                        {
                            Some(token) if Some(token.as_str()) != rejected.as_deref() => {
                                // Reconnect below reads the provider token
                                // again (now the refreshed credential); nothing
                                // is cached here.
                                self.log.write_event(
                                    &server_name,
                                    "the server rejected the provider token; refreshed it and reconnecting",
                                );
                                continue;
                            }
                            // No newer token: there is no OAuth refresh token,
                            // the refresh failed, or the provider returned the
                            // same value. Retrying cannot help, so the user
                            // must sign in to the provider.
                            _ => {
                                let message = crate::mcp::oauth::sign_in_required_message_provider(
                                    &server_name,
                                    &provider,
                                );
                                self.set_state(ConnectionState::NeedsAuth {
                                    message: message.clone(),
                                });
                                self.log.write_event(&server_name, &message);
                                return Err(McpError::ProviderLoginRequired {
                                    server: server_name,
                                    provider,
                                });
                            }
                        }
                    }
                    if !oauth_capable {
                        return Err(error);
                    }
                    let renewed = match authorization_challenge(&error) {
                        Some(challenge) if is_scope_challenge(&error, &challenge) => {
                            self.step_up_oauth_token(&challenge, signal).await
                        }
                        _ => self.renew_oauth_token().await,
                    };
                    if !renewed {
                        // Upstream `markNeedsAuth` (runtime.ts:326): a 401 the
                        // stored credentials cannot satisfy makes the state
                        // `needs-auth`, not `connected` with a stale error.
                        self.set_state(ConnectionState::NeedsAuth {
                            message: crate::mcp::oauth::sign_in_required_message(&server_name),
                        });
                        return Err(McpError::LoginRequired {
                            server: server_name,
                        });
                    }
                    self.drop_client().await;
                    if attempt == 1 {
                        return Err(error);
                    }
                }
                Err(other) => return Err(other),
            }
        }
        Err(McpError::ConnectionClosed)
    }

    /// Bump the settle generation, waking every caller waiting for a connect
    /// or close to finish (`watch` retains the bump until they observe it).
    fn notify_settled(&self) {
        self.settled
            .send_modify(|version| *version = version.wrapping_add(1));
    }

    /// Take the live client out under the slot lock, mark the close in
    /// progress, and await `close()` — bounded by `CLOSE_TIMEOUT` inside the
    /// client — WITHOUT holding the slot lock. Callers re-check the slot
    /// while the close runs; the marker keeps them from starting the next
    /// connect over the still-closing session.
    async fn drop_client(&self) {
        let taken = {
            let mut slot = self.client.lock().await;
            let taken = slot.take();
            if taken.is_some() {
                self.client_instance.store(0, Ordering::SeqCst);
                self.closing.store(true, Ordering::SeqCst);
            }
            self.set_state(ConnectionState::Disconnected { error: None });
            taken
        };
        let Some(client) = taken else {
            return;
        };
        // The previous client's tool list is no longer authoritative; the
        // reconnect's discovery must list again (readiness invalidation).
        self.reset_readiness();
        let _closing = ClosingGuard { connection: self };
        client.close().await;
    }

    /// Close the live client and mark the connection detached
    /// ([`ServerConnection::detached`]). [`McpManager::disconnect`] sets the
    /// flag itself, under the map lock (the ordering requirement lives
    /// there); this assignment covers `McpManager::shutdown`, whose drained
    /// connections are afterwards equally unreachable by every manager path,
    /// so a call still holding the `Arc` must not reopen a transport either.
    async fn shutdown(&self) {
        self.detach();
        self.drop_client().await;
        self.reset_readiness();
        self.set_state(ConnectionState::Closed);
    }

    fn set_state(&self, state: ConnectionState) {
        *self
            .state
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = state.clone();
        // Mirror the transition for the synchronous `mcp_servers` prompt
        // section: one short std lock, never held across an await.
        self.mirror
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(self.name.clone(), state);
    }
}

/// Restores `Stale` when a discovery future ends without recording a result
/// (cancelled by a timeout/abort), so the next call retries instead of
/// observing a permanently `Discovering` server.
struct DiscoveryGuard<'a> {
    connection: &'a ServerConnection,
}

impl Drop for DiscoveryGuard<'_> {
    fn drop(&mut self) {
        let _ = self.connection.readiness.compare_exchange(
            ServerReadiness::Discovering.code(),
            ServerReadiness::Stale.code(),
            Ordering::SeqCst,
            Ordering::SeqCst,
        );
    }
}

/// Clears the single-flight token and wakes the waiters when a connect ends —
/// by finishing, failing, or being cancelled (the drop runs on cancellation
/// too, so an abandoned connect never leaves the server permanently busy).
struct ConnectFlight<'a> {
    connection: &'a ServerConnection,
}

impl Drop for ConnectFlight<'_> {
    fn drop(&mut self) {
        self.connection.connecting.store(false, Ordering::SeqCst);
        self.connection.notify_settled();
    }
}

/// Clears the "close in progress" marker and wakes the waiters when a close
/// ends, cancellation included.
struct ClosingGuard<'a> {
    connection: &'a ServerConnection,
}

impl Drop for ClosingGuard<'_> {
    fn drop(&mut self) {
        self.connection.closing.store(false, Ordering::SeqCst);
        self.connection.notify_settled();
    }
}

fn source_of(entry: &McpServerEntry) -> String {
    match &entry.source {
        super::registry::McpServerSource::GlobalConfig(path)
        | super::registry::McpServerSource::ProjectConfig(path) => path.display().to_string(),
        super::registry::McpServerSource::Extension(path) => path.clone(),
    }
}

/// Expand `~` and `~/...` to the home directory (upstream `expandHome`).
fn expand_home(value: &str) -> String {
    let home = std::env::var("USERPROFILE")
        .or_else(|_| std::env::var("HOME"))
        .unwrap_or_default();
    if home.is_empty() {
        return value.to_owned();
    }
    if value == "~" {
        return home;
    }
    if let Some(rest) = value
        .strip_prefix("~/")
        .or_else(|| value.strip_prefix("~\\"))
    {
        return Path::new(&home).join(rest).to_string_lossy().into_owned();
    }
    value.to_owned()
}

/// Resolve a config value (`${VAR}` template or `!command`), erroring when a
/// referenced variable is missing (upstream `resolveConfigValueOrThrow`).
fn resolve_value(value: &str, server: &str, field: &str) -> Result<String, McpError> {
    match crate::ai::providers::config_value::resolve_config_value(value, None) {
        Some(resolved) => Ok(resolved),
        None => Err(McpError::Config(format!(
            "MCP server \"{server}\" {field} \"{value}\" does not resolve (missing environment variable or failed command)"
        ))),
    }
}

/// Build the stdio spawn options: resolve env values, expand `~`, resolve the
/// working directory (upstream `createDefaultTransport` stdio branch).
fn stdio_options(
    config: &McpStdioServerConfig,
    cwd: &Path,
    entry: &McpServerEntry,
) -> Result<super::stdio::StdioSpawnOptions, McpError> {
    let mut env = BTreeMap::new();
    for (key, value) in config.env.iter().flatten() {
        env.insert(
            key.clone(),
            resolve_value(value, &entry.name, &format!("env \"{key}\""))?,
        );
    }
    let relative = config
        .cwd
        .as_deref()
        .map(expand_home)
        .unwrap_or_else(|| ".".to_owned());
    let resolved_cwd = if Path::new(&relative).is_absolute() {
        PathBuf::from(relative)
    } else {
        cwd.join(relative)
    };
    Ok(super::stdio::StdioSpawnOptions {
        command: expand_home(&config.command),
        args: config
            .args
            .iter()
            .flatten()
            .map(|arg| expand_home(arg))
            .collect(),
        cwd: resolved_cwd,
        env,
        max_message_bytes: super::transport::DEFAULT_MAX_MESSAGE_BYTES,
    })
}

/// Resolve the HTTP headers; values that do not resolve are an error
/// (upstream `resolveHeadersOrThrow`).
fn resolve_headers(
    config: &McpHttpServerConfig,
    entry: &McpServerEntry,
) -> Result<Vec<(String, String)>, McpError> {
    let mut resolved = Vec::new();
    for (key, value) in config.headers.iter().flatten() {
        resolved.push((
            key.clone(),
            resolve_value(value, &entry.name, &format!("header \"{key}\""))?,
        ));
    }
    Ok(resolved)
}

/// The current token of pi provider `provider` as an explicit agent root
/// resolves it (upstream `modelRegistry.getApiKeyForProvider`), through the
/// same credential chain a model request uses:
///
/// 1. a stored OAuth credential in `<agent_dir>/auth.json`, refreshed through
///    the provider's flow when it expires soon;
/// 2. a stored api-key credential;
/// 3. the provider's `models.json` `apiKey` when nothing is stored;
/// 4. the provider's environment api keys.
///
/// Read on every call (never cached): a provider refresh or `/login` applies
/// to the next MCP connect/re-read. A manager built without an agent root
/// ([`McpManager::without_log`]) has no auth store and resolves only step 4.
pub async fn provider_token_in(agent_dir: &Path, provider: &str) -> Option<String> {
    crate::ai::auth::resolve::resolve_provider_token(agent_dir, provider)
        .await
        .ok()
        .flatten()
}
