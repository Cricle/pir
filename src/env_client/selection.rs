//! Which environment the `bash`/`read`/`write`/`edit` tools operate on.
//!
//! **Default: local.** [`configured_remote_env`] returns `None` unless a remote
//! environment is *explicitly* configured, and a tool that gets `None` runs its
//! original code path unchanged. Nothing here reads a settings file, opens a
//! socket, or spawns anything until that explicit configuration exists.
//!
//! Two explicit ways to configure one:
//!
//! 1. `PI_ENV_SSH_HOST` (plus `PI_ENV_SSH_USER`, `PI_ENV_SSH_PORT`,
//!    `PI_ENV_SSH_KEY`, `PI_ENV_SSH_PROGRAM`, `PI_ENV_SSH_KNOWN_HOSTS`,
//!    `PI_ENV_SSH_ALIAS`): the same variable names upstream's own
//!    external-SSH suite uses (`packages/env/test/ssh-external.test.ts:22-36`),
//!    so a host already set up to run that suite needs nothing new. The
//!    connection is upstream's lazy `sshConnection`: detection, host-key check
//!    and deployment happen on the first request, so this function stays
//!    synchronous. Two further variables carry the **remote side's own**
//!    construction options — `PI_ENV_REMOTE_CWD` →
//!    `RemoteExecutionEnvOptions.cwd` and `PI_ENV_REMOTE_SHELL_PATH` →
//!    `RemoteExecutionEnvOptions.shellPath` (see [`remote_side_values`]).
//! 2. [`install_override`]: a host that builds its own `Connection` (a session
//!    with an already-deployed daemon, a test) installs the environment
//!    directly. [`clear_override`] restores the default.
//!
//! Upstream has no *library-level* switch of its own: `packages/env`'s callers
//! pass a `Connection` in (`RemoteExecutionEnv({ connection, … })`), and the
//! only caller in the upstream tree is its test suite. In pir the tool layer is
//! that caller, so the choice it would make is expressed here — as an explicit,
//! documented configuration surface rather than a default.
//!
//! One consequence of being that caller: a remote environment's `shellPath`
//! names a path on the **remote** machine, so it is configured here
//! (`PI_ENV_REMOTE_SHELL_PATH`) and never taken from the settings `shellPath`,
//! which names this machine's shell (upstream feeds that setting only to
//! `createLocalBashOperations`, `agent-session.ts:3643/3881`). See
//! [`remote_side_values`].

use std::sync::{Arc, Mutex, OnceLock};

use super::errors::{FileErrorCode, RemoteError};
use super::remote_env::{Context, RemoteExecutionEnv, RemoteExecutionEnvOptions};
use super::ssh::{ssh_connection, SshConnectOptions, SshTarget};

/// A remote environment installed by the process itself (a host, a test).
fn override_slot() -> &'static Mutex<Option<Arc<RemoteExecutionEnv>>> {
    static SLOT: OnceLock<Mutex<Option<Arc<RemoteExecutionEnv>>>> = OnceLock::new();
    SLOT.get_or_init(|| Mutex::new(None))
}

/// The environment built from the configuration variables, with the key of the
/// configuration it was built from, so a changed variable rebuilds it.
type ConfiguredSlot = Mutex<Option<(String, Arc<RemoteExecutionEnv>)>>;

/// The environment built from the configuration variables, kept so consecutive
/// tool calls share one connection.
fn configured_slot() -> &'static ConfiguredSlot {
    static SLOT: OnceLock<ConfiguredSlot> = OnceLock::new();
    SLOT.get_or_init(|| Mutex::new(None))
}

/// Install a remote environment for the tool layer, overriding the
/// configuration variables.
pub fn install_override(environment: Arc<RemoteExecutionEnv>) {
    *lock(override_slot()) = Some(environment);
}

/// Drop a previously installed override; the configuration variables (if any)
/// apply again.
pub fn clear_override() {
    *lock(override_slot()) = None;
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// The remote environment the tools should use, or `None` for the local one.
///
/// This is the tools' single decision point: `None` (the default — no override
/// and no `PI_ENV_SSH_HOST`) means every tool keeps its original local code
/// path.
pub fn configured_remote_env() -> Option<Arc<RemoteExecutionEnv>> {
    if let Some(environment) = lock(override_slot()).clone() {
        return Some(environment);
    }
    let host = read_variable("PI_ENV_SSH_HOST").unwrap_or_default();
    if host.is_empty() {
        // Nothing configured: reset the cache so a value set later starts fresh.
        *lock(configured_slot()) = None;
        return None;
    }
    let agent_dir = crate::coding_agent::context::agent_dir();
    let key = configuration_key(&host, &read_variable, &agent_dir);
    if let Some((cached_key, environment)) = lock(configured_slot()).clone() {
        if cached_key == key {
            return Some(environment);
        }
    }
    let environment = Arc::new(build_ssh_environment(&host, &read_variable, &agent_dir));
    *lock(configured_slot()) = Some((key, environment.clone()));
    Some(environment)
}

/// A description of the active remote environment, for logs and diagnostics.
pub fn tool_env_description() -> Option<String> {
    configured_remote_env().map(|environment| format!("remote:{}", environment.id))
}

/// Read one configuration variable: unset and empty are the same (`None`), so a
/// blank variable means "not configured" everywhere below.
fn read_variable(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|value| !value.is_empty())
}

/// The configuration variables, as one comparable key (so a changed variable
/// rebuilds the environment instead of reusing a stale one). The reader is a
/// parameter so the mapping stays testable without the process environment.
///
/// **The key is derived from the values the environment is actually built
/// with, not from the environment they came from** — the design rule the rest
/// of this migration follows. So the host-key entry is the *resolved*
/// `known_hosts` path ([`resolve_known_hosts`]): `PI_ENV_SSH_KNOWN_HOSTS`
/// unset means the default path under `agent_dir`, and keying on that resolved
/// path is what makes an agent-dir change rebuild the environment (otherwise
/// the cached connection would keep writing host keys into the previous
/// directory's file — env-audit §D11). Keying on the variable *and* the agent
/// dir would over-invalidate instead: with an explicit
/// `PI_ENV_SSH_KNOWN_HOSTS` the agent dir no longer feeds the path at all, so
/// a change to it must not tear down a connection whose target is unchanged.
///
/// Every other field is a raw value with no derivation, so it is keyed
/// verbatim; `PI_ENV_SSH_ALIAS` likewise defaults to something the host
/// already covers.
fn configuration_key(
    host: &str,
    read: &impl Fn(&str) -> Option<String>,
    agent_dir: &std::path::Path,
) -> String {
    let value = |name: &str| read(name).unwrap_or_default();
    format!(
        "{host}\u{1}{}\u{1}{}\u{1}{}\u{1}{}\u{1}{}\u{1}{}\u{1}{}\u{1}{}",
        value("PI_ENV_SSH_USER"),
        value("PI_ENV_SSH_PORT"),
        value("PI_ENV_SSH_KEY"),
        value("PI_ENV_SSH_PROGRAM"),
        // The resolved file the connection writes host keys into, not the
        // variable it may have come from (see above).
        resolve_known_hosts(read, agent_dir),
        value("PI_ENV_SSH_ALIAS"),
        value("PI_ENV_REMOTE_CWD"),
        value("PI_ENV_REMOTE_SHELL_PATH"),
    )
}

/// The host-key file the environment uses: `PI_ENV_SSH_KNOWN_HOSTS` verbatim
/// when set, else the agent dir's own default. Both the key and the built
/// environment go through this one function, so they cannot disagree.
fn resolve_known_hosts(
    read: &impl Fn(&str) -> Option<String>,
    agent_dir: &std::path::Path,
) -> String {
    read("PI_ENV_SSH_KNOWN_HOSTS").unwrap_or_else(|| default_known_hosts(agent_dir))
}

/// `#[doc(hidden)]` test seam: the cache key the selection layer derives for
/// `host` with the given configuration `values` and `agent_dir`. The real
/// reader is the process environment, which a test must not depend on; the
/// key's whole contract (what invalidates a cached environment) is otherwise
/// unreachable from `tests/`.
#[doc(hidden)]
pub fn configuration_key_for_test(
    host: &str,
    values: &[(&str, &str)],
    agent_dir: &std::path::Path,
) -> String {
    let lookup = |name: &str| {
        values
            .iter()
            .find(|(key, _)| *key == name)
            .map(|(_, value)| (*value).to_owned())
    };
    configuration_key(host, &lookup, agent_dir)
}

/// The default host-key file: beside the rest of the agent's state, so host
/// keys never mix with the user's own `~/.ssh/known_hosts`.
fn default_known_hosts(agent_dir: &std::path::Path) -> String {
    agent_dir
        .join("mobile")
        .join("known_hosts")
        .to_string_lossy()
        .into_owned()
}

/// The remote-side values the configuration variables carry — both name paths
/// on the **remote** machine, and both mirror construction options of upstream's
/// `RemoteExecutionEnv`:
///
/// * `PI_ENV_REMOTE_CWD` → `RemoteExecutionEnvOptions.cwd` (upstream README's
///   `cwd`; unset = the daemon's own `hello.cwd`);
/// * `PI_ENV_REMOTE_SHELL_PATH` → `RemoteExecutionEnvOptions.shellPath`, sent
///   verbatim as the exec request's `shellPath` (`packages/env/docs/
///   protocol.md`), i.e. the shell the daemon runs a command **string**
///   through; unset = the daemon's own probe, and a path it cannot find fails
///   with `shell_unavailable` (`docs/semantics.md`).
///
/// The settings `shellPath` is deliberately NOT read here: it names the shell
/// on **this** machine (upstream's `settingsManager.getShellPath()` feeds only
/// `createLocalBashOperations`, `agent-session.ts:3643/3881`, i.e. the local
/// arm). Sending it to another machine would ask the daemon for a path that in
/// general does not exist there — every exec would fail with
/// `shell_unavailable`. Upstream's `RemoteExecutionEnv.shellPath` is a
/// construction option its tree leaves to the caller for the same reason.
fn remote_side_values(read: &impl Fn(&str) -> Option<String>) -> (Option<String>, Option<String>) {
    (read("PI_ENV_REMOTE_CWD"), read("PI_ENV_REMOTE_SHELL_PATH"))
}

/// Build the SSH environment from the configuration variables. The connection
/// is lazy (upstream `sshConnection`): this does not touch the network.
/// `agent_dir` is passed in (rather than re-resolved) so the `known_hosts`
/// default and the cache key that guards it come from the same value.
fn build_ssh_environment(
    host: &str,
    read: &impl Fn(&str) -> Option<String>,
    agent_dir: &std::path::Path,
) -> RemoteExecutionEnv {
    let known_hosts = resolve_known_hosts(read, agent_dir);
    let alias = read("PI_ENV_SSH_ALIAS").unwrap_or_else(|| format!("pi-env-{host}"));
    let mut target = SshTarget::new(host, known_hosts, alias);
    target.user = read("PI_ENV_SSH_USER");
    target.port = read("PI_ENV_SSH_PORT").and_then(|port| port.parse().ok());
    target.identity_file = read("PI_ENV_SSH_KEY");
    target.ssh = read("PI_ENV_SSH_PROGRAM");
    let connection = ssh_connection(SshConnectOptions::new(target));
    let mut options =
        RemoteExecutionEnvOptions::new(format!("pi-env:ssh:{host}"), connection.connection);
    let (cwd, shell_path) = remote_side_values(read);
    options.cwd = cwd;
    options.shell_path = shell_path;
    RemoteExecutionEnv::new(options)
}

/// The remote analogue of `tools::util::workspace_path`: the same
/// "inside the workspace root" rule, evaluated on the *remote* file system
/// (the tool layer's confinement rule, applied identically to both arms).
///
/// The existing prefix is canonicalized through the daemon's `realpath`, so a
/// symlink cannot point a read or write outside the root, exactly as the local
/// rule canonicalizes the existing portion of the path.
pub async fn workspace_path_remote(
    environment: &RemoteExecutionEnv,
    requested: &str,
) -> Result<String, String> {
    let paths = environment
        .paths()
        .await
        .map_err(|error| format!("Cannot access workspace root: {}", error.message))?;
    // Parent-directory components are rejected even when they would eventually
    // land back inside the workspace.
    if requested
        .split(|character| paths.is_separator(character))
        .any(|segment| segment == "..")
    {
        return Err(format!("Path '{}' escapes the workspace root", requested));
    }
    let root_lexical = environment
        .root()
        .await
        .map_err(|error| format!("Cannot access workspace root: {}", error.message))?;
    let root = canonicalize_existing(environment, &root_lexical)
        .await
        .unwrap_or(root_lexical);
    let candidate = environment
        .resolve(requested)
        .await
        .map_err(|error| format!("Cannot access workspace root: {}", error.message))?;
    let existing = canonicalize_existing(environment, &candidate)
        .await
        .map_err(|_| format!("Invalid path '{}'", requested))?;
    if !paths.contains(&root, &existing) {
        return Err(format!("Path '{}' escapes the workspace root", requested));
    }
    Ok(candidate)
}

/// The daemon's `realpath` of the longest existing prefix of `path` (the remote
/// form of `std::fs::canonicalize`, which also fails on a missing path).
async fn canonicalize_existing(
    environment: &RemoteExecutionEnv,
    path: &str,
) -> Result<String, RemoteError> {
    let paths = environment.paths().await?;
    let mut current = path.to_string();
    loop {
        match environment
            .canonical_path(&current, &Context::default())
            .await
        {
            Ok(canonical) => return Ok(canonical),
            Err(error) if error.code == FileErrorCode::NotFound => {
                let parent = paths.dirname(&current);
                if parent == current || parent.is_empty() {
                    return Err(RemoteError::new(
                        "ENOENT",
                        format!("no existing prefix of {path}"),
                    ));
                }
                current = parent;
            }
            Err(error) => {
                return Err(RemoteError::new("unknown", error.message));
            }
        }
    }
}
