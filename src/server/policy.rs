//! Security and resource limit configuration for the TCP server.
//!
//! Three policy decisions are documented here because they change what a
//! remote client can make the server process do:
//!
//! * **Authentication is required by default.** A server with no
//!   `auth_token` refuses to bind unless the host explicitly opts into
//!   unauthenticated local development via
//!   [`ServerSecurityConfig::unauthenticated_local`] (the CLI spells this
//!   `--allow-unauthenticated`). Loopback is not a user boundary: any local
//!   user/process can connect to `127.0.0.1`, so a token-less loopback server
//!   would let other local accounts create sessions and read/write files
//!   under the server's working directory.
//! * `allow_remote_bash` — when `false` (the default), the bash tool is
//!   **excluded from every session's tool registry**, so a remote prompt run
//!   can never execute shell commands on the host even if a model asks for
//!   one. `allow_remote_agent` gates whether prompts may be sent at all;
//!   `allow_remote_bash` gates what those prompts may execute.
//! * `create` with an explicit `cwd` — a remote client that names an
//!   arbitrary working directory points the whole agent (tools, context
//!   files, settings loading) at any path on the host. The default therefore
//!   **rejects any non-default cwd** with a typed policy error. An operator
//!   can widen this by setting `PIR_SERVER_ALLOWED_ROOTS` to a
//!   `;`-separated list of roots; a requested cwd is accepted only when it is
//!   inside one of them (and `allow_remote_agent` is on). See
//!   [`ServerSecurityConfig::allows_cwd`].

use std::fmt;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::config::RedactedToken;

const DEFAULT_MAX_CONNECTIONS: usize = 128;
const DEFAULT_MAX_CONNECTION_BODY_BYTES: usize = 64 * 1024 * 1024;
const DEFAULT_MAX_SESSIONS: usize = 256;
const DEFAULT_MAX_SUBSCRIPTIONS: usize = 64;
/// Generous idle bound on one read after the handshake (reset on every
/// activity): a connection that stops reading or writing is reaped instead of
/// holding its slot (slowloris) or growing its frame channel without bound.
const DEFAULT_IDLE_TIMEOUT: Duration = Duration::from_secs(300);

/// Environment variables overriding the session/subscription caps and the
/// idle timeout. Only the connection/body bounds had host-level overrides
/// before; these close the gap so an operator can size a server without a
/// code change. Values outside the ranges below are ignored (the default is
/// kept) instead of producing an unusable configuration.
const MAX_SESSIONS_ENV: &str = "PIR_SERVER_MAX_SESSIONS";
const MAX_SUBSCRIPTIONS_ENV: &str = "PIR_SERVER_MAX_SUBSCRIPTIONS";
const IDLE_TIMEOUT_SECS_ENV: &str = "PIR_SERVER_IDLE_TIMEOUT_SECS";

/// Range guard for the configurable caps: a server must admit at least one
/// session/subscription and cannot be configured into an effectively
/// unbounded registry by a typo.
const MAX_CONFIGURABLE_CAP: usize = 1_000_000;
/// Range guard for the idle timeout: at least one second, at most one day.
const MAX_CONFIGURABLE_IDLE_SECS: u64 = 86_400;

/// Bound on the whole pre-greeting phase (auth preface + hello), measured
/// from connection start — upstream `DEFAULT_HANDSHAKE_TIMEOUT_MS`
/// (server.ts:42). Without it an unauthenticated connection could hold a
/// connection slot for the full idle timeout on every read (the pre-greet
/// read wait used to be governed by `idle_timeout` alone). After the
/// greeting the per-read idle timeout governs again.
pub(super) const DEFAULT_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);

/// Environment variable holding the `;`-separated roots a remote `create`
/// may use as the session cwd. Unset (the default) means only the default
/// cwd is accepted.
pub(super) const ALLOWED_ROOTS_ENV: &str = "PIR_SERVER_ALLOWED_ROOTS";

/// Security and resource limits for the TCP server.
///
/// Authentication is required by default: binding a token-less server fails
/// with [`std::io::ErrorKind::PermissionDenied`] unless
/// [`Self::unauthenticated_local`] / the CLI `--allow-unauthenticated` flag
/// explicitly opts in. Once a token is configured, clients must send the
/// private preface before the normal CBOR protocol and unauthenticated sockets
/// are closed without a protocol response. The token itself is never printed
/// by `Debug` and this type is not `Serialize`.
#[derive(Clone, PartialEq, Eq)]
pub struct ServerSecurityConfig {
    pub auth_token: Option<String>,
    /// Explicit opt-in for running without `auth_token`. Only for local
    /// development and tests: loopback TCP is reachable by every local user
    /// and process, so it is not a user-isolation boundary.
    pub allow_unauthenticated: bool,
    pub max_connections: usize,
    pub max_request_body_bytes: usize,
    pub max_connection_body_bytes: usize,
    /// Maximum number of live sessions; `create` is rejected beyond it.
    pub max_sessions: usize,
    /// Maximum `pir.session` subscriptions per connection; extra `subscribe`
    /// calls are rejected (each subscription amplifies every broadcast into
    /// one frame for this connection).
    pub max_subscriptions: usize,
    /// Bound on one read wait after the handshake (reset on activity).
    pub idle_timeout: Duration,
    pub allow_remote_bash: bool,
    pub allow_remote_agent: bool,
}

impl fmt::Debug for ServerSecurityConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ServerSecurityConfig")
            .field("auth_token", &RedactedToken(self.auth_token.as_deref()))
            .field("allow_unauthenticated", &self.allow_unauthenticated)
            .field("max_connections", &self.max_connections)
            .field("max_request_body_bytes", &self.max_request_body_bytes)
            .field("max_connection_body_bytes", &self.max_connection_body_bytes)
            .field("max_sessions", &self.max_sessions)
            .field("max_subscriptions", &self.max_subscriptions)
            .field("idle_timeout", &self.idle_timeout)
            .field("allow_remote_bash", &self.allow_remote_bash)
            .field("allow_remote_agent", &self.allow_remote_agent)
            .finish()
    }
}

impl Default for ServerSecurityConfig {
    fn default() -> Self {
        Self::from_env_lookup(|name| std::env::var(name).ok())
    }
}

impl ServerSecurityConfig {
    /// Build the default configuration with the
    /// `PIR_SERVER_MAX_SESSIONS` / `PIR_SERVER_MAX_SUBSCRIPTIONS` /
    /// `PIR_SERVER_IDLE_TIMEOUT_SECS` overrides applied.
    ///
    /// `lookup` is the environment accessor (the process environment in
    /// [`Default`], an explicit map in tests, so the test never mutates
    /// process-wide state). A value that does not parse or falls outside the
    /// documented range is ignored: the built-in default is kept.
    #[doc(hidden)]
    pub fn from_env_lookup(lookup: impl Fn(&str) -> Option<String>) -> Self {
        let mut config = Self {
            auth_token: None,
            // A token-less server is refused at bind time unless the host
            // explicitly opts in (local development / tests only).
            allow_unauthenticated: false,
            max_connections: DEFAULT_MAX_CONNECTIONS,
            // Keep library defaults compatible with the historical 16 MiB
            // protocol frame limit; the CLI config defaults to 1 MiB.
            max_request_body_bytes: crate::protocol::framing::DEFAULT_MAX_FRAME_LENGTH,
            max_connection_body_bytes: DEFAULT_MAX_CONNECTION_BODY_BYTES,
            max_sessions: DEFAULT_MAX_SESSIONS,
            max_subscriptions: DEFAULT_MAX_SUBSCRIPTIONS,
            idle_timeout: DEFAULT_IDLE_TIMEOUT,
            allow_remote_bash: false,
            // Preserve the historical Server::bind prompt behavior.
            allow_remote_agent: true,
        };
        if let Some(value) = lookup(MAX_SESSIONS_ENV)
            .and_then(|value| value.trim().parse::<usize>().ok())
            .filter(|value| (1..=MAX_CONFIGURABLE_CAP).contains(value))
        {
            config.max_sessions = value;
        }
        if let Some(value) = lookup(MAX_SUBSCRIPTIONS_ENV)
            .and_then(|value| value.trim().parse::<usize>().ok())
            .filter(|value| (1..=MAX_CONFIGURABLE_CAP).contains(value))
        {
            config.max_subscriptions = value;
        }
        if let Some(value) = lookup(IDLE_TIMEOUT_SECS_ENV)
            .and_then(|value| value.trim().parse::<u64>().ok())
            .filter(|value| (1..=MAX_CONFIGURABLE_IDLE_SECS).contains(value))
        {
            config.idle_timeout = Duration::from_secs(value);
        }
        config
    }

    /// Explicit opt-in for a token-less local server (the CLI
    /// `--allow-unauthenticated` flag). This disables authentication for
    /// every client that can reach the listener — on loopback that includes
    /// all local users/processes, not only the invoking user. Never use it
    /// on a shared host.
    pub fn unauthenticated_local() -> Self {
        Self {
            allow_unauthenticated: true,
            ..Self::default()
        }
    }
}

impl ServerSecurityConfig {
    pub(super) fn validate(&self) -> io::Result<()> {
        if self.auth_token.as_deref().is_some_and(str::is_empty)
            || self.max_connections == 0
            || self.max_request_body_bytes == 0
            || self.max_request_body_bytes > u32::MAX as usize
            || self.max_connection_body_bytes == 0
            || self.max_sessions == 0
            || self.max_subscriptions == 0
            || self.idle_timeout.is_zero()
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid server security configuration",
            ));
        }
        Ok(())
    }

    /// Validate the limits and enforce the authentication invariant before a
    /// listener is created. A missing token is only accepted after an explicit
    /// [`Self::unauthenticated_local`] opt-in; this check runs before bind so
    /// a refused server never briefly listens (and never race-binds the
    /// default port).
    pub(super) fn validate_for_bind(&self) -> io::Result<()> {
        self.validate()?;
        if self.auth_token.is_none() && !self.allow_unauthenticated {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "refusing to start an unauthenticated server: set PIR_SERVER_AUTH_TOKEN \
                 or pass --allow-unauthenticated for local development (loopback does not \
                 isolate other local users)",
            ));
        }
        Ok(())
    }

    /// Whether a remote `create` may use `cwd` as the session working
    /// directory.
    ///
    /// The default cwd (`"."` — the server's own working directory) is always
    /// allowed, including its absolute form (a persisted session header stores
    /// the path `"."` resolved to). Any other cwd requires remote prompts to
    /// be enabled *and* an explicit `PIR_SERVER_ALLOWED_ROOTS` allowlist
    /// (`;`-separated roots); anything else is a policy rejection. Paths
    /// containing `.`/`..` components are rejected outright — a purely textual
    /// prefix check would otherwise be bypassable
    /// (`C:\root\a\..\..\Windows`).
    pub(super) fn allows_cwd(&self, cwd: &str) -> bool {
        self.resolve_cwd(cwd).is_some()
    }

    /// Resolve `cwd` to the canonical path a session should be created with,
    /// or `None` when the policy rejects it.
    ///
    /// Both the candidate and every allowlist root are `canonicalize`d before
    /// comparing path components: a symlink placed inside an allowed root
    /// would otherwise point the session outside it, and on Windows a
    /// differently-cased sibling directory would pass a folding comparison.
    /// Comparison is component-wise (`Path::starts_with`), case-insensitive
    /// only on Windows. This closes the static symlink/alias escape; it does
    /// not claim to defeat a TOCTOU replacement of the path after the check.
    ///
    /// A relative path that is not the default `"."` is rejected before
    /// `canonicalize`: resolving it against the server's own cwd could
    /// accidentally place a remote `sub` inside an allowed root.
    ///
    /// The default cwd can also arrive in its ABSOLUTE form: a persisted
    /// session header records the path that `SessionManager` resolved `"."`
    /// to, so a restart/attach re-checks the absolute path of what was the
    /// default cwd at create time. Equality (not containment) with the
    /// server's current directory is treated exactly like the literal `"."`;
    /// a subdirectory of the server cwd still needs the allowlist.
    pub(super) fn resolve_cwd(&self, cwd: &str) -> Option<PathBuf> {
        if cwd == "." {
            return Some(PathBuf::from("."));
        }
        if path_has_relative_components(cwd) || !Path::new(cwd).is_absolute() {
            return None;
        }
        let candidate = std::fs::canonicalize(cwd).ok()?;
        // "Default cwd" in absolute form: the header write resolves `"."`
        // against the session manager's process cwd, so an attach after a
        // restart sees an absolute path. Accept it only when it IS the
        // server's own cwd (both directions of `path_within` = equality,
        // case-insensitive on Windows) — never a path below it.
        if let Some(server_cwd) = canonical_server_cwd() {
            if path_within(&candidate, &server_cwd) && path_within(&server_cwd, &candidate) {
                return Some(candidate);
            }
        }
        if !self.allow_remote_agent {
            return None;
        }
        let roots = allowed_roots()?;
        roots
            .iter()
            .filter_map(|root| std::fs::canonicalize(root).ok())
            .any(|root| path_within(&candidate, &root))
            .then_some(candidate)
    }
}

/// The server's current working directory, canonicalized. `None` when it
/// cannot be resolved; callers then fall back to the allowlist check (fail
/// closed).
fn canonical_server_cwd() -> Option<PathBuf> {
    std::fs::canonicalize(std::env::current_dir().ok()?).ok()
}

/// Whether `path` contains `.` or `..` components (which a textual prefix
/// check cannot evaluate safely).
fn path_has_relative_components(path: &str) -> bool {
    path.split(['/', '\\'])
        .any(|component| component == "." || component == "..")
}

/// Parse `PIR_SERVER_ALLOWED_ROOTS` (`;`-separated). `None` when unset.
fn allowed_roots() -> Option<Vec<String>> {
    std::env::var(ALLOWED_ROOTS_ENV).ok().map(|value| {
        value
            .split(';')
            .map(str::trim)
            .filter(|root| !root.is_empty())
            .map(str::to_owned)
            .collect()
    })
}

/// Whether `candidate` is `root` itself or a path below it, compared by
/// whole path components on already-canonicalized paths.
///
/// Case-insensitive on Windows (the filesystem is), case-sensitive elsewhere
/// so a differently-cased sibling directory cannot alias an allowed root on
/// Unix.
fn path_within(candidate: &Path, root: &Path) -> bool {
    #[cfg(windows)]
    {
        let candidate = candidate.to_string_lossy().to_ascii_lowercase();
        let root = root.to_string_lossy().to_ascii_lowercase();
        Path::new(&candidate).starts_with(root)
    }
    #[cfg(not(windows))]
    {
        candidate.starts_with(root)
    }
}
