//! Security and resource limit configuration for the TCP server.
//!
//! Two policy decisions are documented here because they change what a remote
//! client can make the server process do:
//!
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

use std::io;
use std::time::Duration;

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
/// Authentication is intentionally opt-in so that loopback/library users retain
/// the original protocol handshake. Once a token is configured, clients must
/// send the private preface before the normal CBOR protocol and unauthenticated
/// sockets are closed without a protocol response.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ServerSecurityConfig {
    pub auth_token: Option<String>,
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

    /// Whether a remote `create` may use `cwd` as the session working
    /// directory.
    ///
    /// The default cwd (`"."` — the server's own working directory) is always
    /// allowed. Any other cwd requires remote prompts to be enabled *and* an
    /// explicit `PIR_SERVER_ALLOWED_ROOTS` allowlist (`;`-separated roots);
    /// anything else is a policy rejection. Paths containing `.`/`..`
    /// components are rejected outright — a purely textual prefix check would
    /// otherwise be bypassable (`C:\root\a\..\..\Windows`).
    pub(super) fn allows_cwd(&self, cwd: &str) -> bool {
        if cwd == "." {
            return true;
        }
        if !self.allow_remote_agent {
            return false;
        }
        if path_has_relative_components(cwd) {
            return false;
        }
        let Some(roots) = allowed_roots() else {
            return false;
        };
        roots.iter().any(|root| path_within(cwd, root))
    }
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

/// Whether `candidate` is `root` itself or a path below it. Textual,
/// case-insensitive comparison with separator normalization — no filesystem
/// access, so the check never fails open on an unreadable path.
fn path_within(candidate: &str, root: &str) -> bool {
    fn normalize(path: &str) -> String {
        path.replace('/', "\\")
            .trim_end_matches('\\')
            .to_ascii_lowercase()
    }
    let (candidate, root) = (normalize(candidate), normalize(root));
    if root.is_empty() {
        return false;
    }
    candidate == root || candidate.starts_with(&(root + "\\"))
}
