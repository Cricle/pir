use super::*;

use std::sync::{Mutex, OnceLock, PoisonError};

// ---------------------------------------------------------------------------
// Process-wide helpers
// ---------------------------------------------------------------------------

/// One cached projection of the global `settings.json` used by provider
/// transports. The stamp is the file's length+mtime: a changed file misses
/// and is re-parsed, while repeated client construction on the same request
/// path stops re-reading and re-parsing the file for every HTTP client.
struct GlobalSettingsCache {
    agent_dir: PathBuf,
    stamp: Option<FileStamp>,
    settings: Settings,
}

fn global_settings_cache() -> &'static Mutex<Option<GlobalSettingsCache>> {
    static CACHE: OnceLock<Mutex<Option<GlobalSettingsCache>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(None))
}

/// The global-scope settings snapshot, cached by file stamp. Global-only and
/// trust-independent, matching [`current_http_proxy`]'s contract.
fn cached_global_settings(agent_dir: &Path, cwd: &Path) -> Settings {
    let stamp = file_stamp(&agent_dir.join("settings.json"));
    {
        let cache = global_settings_cache()
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if let Some(entry) = cache.as_ref() {
            if entry.agent_dir == agent_dir && entry.stamp == stamp {
                return entry.settings.clone();
            }
        }
    }
    let manager = SettingsManager::load(agent_dir, &cwd.join(".pi"), cwd, false);
    let settings = manager.settings.clone();
    *global_settings_cache()
        .lock()
        .unwrap_or_else(PoisonError::into_inner) = Some(GlobalSettingsCache {
        agent_dir: agent_dir.to_path_buf(),
        stamp,
        settings: settings.clone(),
    });
    settings
}

/// The `httpProxy` setting from the global scope, for hosts that build HTTP
/// clients without a full settings snapshot (upstream applies the setting as
/// HTTP_PROXY/HTTPS_PROXY for Pi-managed HTTP clients).
pub fn current_http_proxy() -> Option<String> {
    let global_dir = crate::coding_agent::context::agent_dir();
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    // The proxy applies to Pi-managed clients regardless of project trust
    // (global scope only, matching the cost/side-effect profile of
    // `cacheWarming`).
    cached_global_settings(&global_dir, &cwd)
        .http_proxy
        .filter(|proxy| !proxy.is_empty())
}

/// The `httpIdleTimeoutMs` setting from the global scope, for hosts that
/// build HTTP clients without a full settings snapshot (upstream applies the
/// value to the global undici dispatcher's `headersTimeout`/`bodyTimeout`;
/// `0` disables the timeout). Mirrors [`current_http_proxy`]'s global-only,
/// trust-independent scope.
pub fn current_http_idle_timeout_ms() -> u64 {
    let global_dir = crate::coding_agent::context::agent_dir();
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    match cached_global_settings(&global_dir, &cwd).http_idle_timeout_ms {
        Some(TimeoutSetting::Milliseconds(ms)) => ms,
        Some(TimeoutSetting::Disabled) => 0,
        None => crate::config::defaults::HTTP_IDLE_TIMEOUT_MS,
    }
}

/// Effective shell preferences for the bash tool (upstream
/// `settingsManager.getShellCommandPrefix()` + `getShellPath()`, with the
/// `~` expansion of `normalizePath`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShellPrefs {
    /// `shellCommandPrefix`: prepended to every command followed by a newline
    /// (upstream `${prefix}\n${command}`).
    pub command_prefix: Option<String>,
    /// `shellPath` with `~` expanded: the bash executable for the bash tool.
    pub shell_path: Option<PathBuf>,
    /// `shellPool`: run bash commands through a persistent pooled shell
    /// process (default on; `false` spawns a fresh shell per call).
    pub pool: bool,
    /// `tools.commandTimeoutMs` as written: `None` = unset (the 120 s pir
    /// default applies), `Some(0)` = explicitly disabled (upstream's
    /// no-default-timeout behavior), `Some(ms)` = that cap. An explicit
    /// per-call `timeout` always wins over this default.
    pub command_timeout_ms: Option<u64>,
}

impl Default for ShellPrefs {
    fn default() -> Self {
        Self {
            command_prefix: None,
            shell_path: None,
            pool: true,
            // Unset, not disabled: the applied default is 120 s.
            command_timeout_ms: None,
        }
    }
}

impl ShellPrefs {
    /// The effective `tools.commandTimeoutMs` cap. Unset applies the pir
    /// default ([`DEFAULT_COMMAND_TIMEOUT_MS`], a deliberate deviation from
    /// upstream); `0` disables the cap (upstream semantics).
    pub fn command_timeout(&self) -> Option<std::time::Duration> {
        match self.command_timeout_ms {
            Some(0) => None,
            Some(ms) => Some(std::time::Duration::from_millis(ms)),
            None => Some(std::time::Duration::from_millis(DEFAULT_COMMAND_TIMEOUT_MS)),
        }
    }

    /// The effective cap in milliseconds (`0` = disabled), for diagnostics and
    /// tests.
    pub fn effective_command_timeout_ms(&self) -> u64 {
        self.command_timeout()
            .map_or(0, |timeout| timeout.as_millis() as u64)
    }
}

/// Read the shell preferences for the current cwd (project scope loads only
/// when the project is trusted, mirroring the settings load everywhere else).
/// This is the ambient boundary: the settings-file read AND the home resolution
/// for `~` live here, so the projection in [`shell_prefs_from`] stays pure.
pub fn current_shell_prefs() -> ShellPrefs {
    shell_prefs_in(&crate::coding_agent::context::agent_dir())
}

/// One cached shell-prefs projection with the stamps that validate it.
struct ShellPrefsCacheEntry {
    agent_dir: PathBuf,
    cwd: PathBuf,
    home: Option<PathBuf>,
    global_stamp: Option<FileStamp>,
    project_stamp: Option<FileStamp>,
    trust_stamp: Option<FileStamp>,
    decision: Option<bool>,
    prefs: ShellPrefs,
}

fn shell_prefs_cache() -> &'static Mutex<Option<ShellPrefsCacheEntry>> {
    static CACHE: OnceLock<Mutex<Option<ShellPrefsCacheEntry>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(None))
}

/// [`current_shell_prefs`] against an EXPLICIT agent directory (D1).
///
/// A live session passes the directory it was built with, so the `!command`
/// shell resolution keeps reading the settings the session started with when
/// `PIR_AGENT_DIR` changes mid-process. The cwd, the trust decision and the
/// `~` home lookup stay ambient (they are per-invocation/per-process inputs,
/// not agent-dir state).
///
/// The projection is cached and validated by the length+mtime stamps of the
/// two settings files, the trust store, and the process-level trust decision.
/// Bash tools call this before every command, so an unchanged configuration
/// (the normal case) no longer re-reads and re-parses `settings.json` and
/// `trust.json` for each command. An edited settings file, a changed trust
/// entry, or a `/trust` decision invalidates the entry.
pub fn shell_prefs_in(agent_dir: &Path) -> ShellPrefs {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from);
    let global_stamp = file_stamp(&agent_dir.join("settings.json"));
    let project_stamp = file_stamp(&cwd.join(".pi").join("settings.json"));
    let trust_path = crate::config::trust::ProjectTrustStore::new(agent_dir)
        .path()
        .to_path_buf();
    let trust_stamp = file_stamp(&trust_path);
    let decision = crate::extensions::project_trust::resolved_decision(&cwd);
    {
        let cache = shell_prefs_cache()
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if let Some(entry) = cache.as_ref() {
            if entry.agent_dir == agent_dir
                && entry.cwd == cwd
                && entry.home == home
                && entry.global_stamp == global_stamp
                && entry.project_stamp == project_stamp
                && entry.trust_stamp == trust_stamp
                && entry.decision == decision
            {
                return entry.prefs.clone();
            }
        }
    }
    let trusted = crate::config::trust::resolve_cwd_trust(&cwd, agent_dir);
    let manager = SettingsManager::load(agent_dir, &cwd.join(".pi"), &cwd, trusted);
    let prefs = shell_prefs_from(&manager.settings, home.as_deref());
    *shell_prefs_cache()
        .lock()
        .unwrap_or_else(PoisonError::into_inner) = Some(ShellPrefsCacheEntry {
        agent_dir: agent_dir.to_path_buf(),
        cwd,
        home,
        global_stamp,
        project_stamp,
        trust_stamp,
        decision,
        prefs: prefs.clone(),
    });
    prefs
}

/// Project a merged [`Settings`] snapshot into [`ShellPrefs`] (pure; the
/// settings-file read and the home lookup live in [`current_shell_prefs`]).
/// A blank `shellCommandPrefix` / `shellPath` is treated as unset, and
/// `shellPath` gets the `~` expansion of upstream `normalizePath` against the
/// EXPLICIT `home` (`None` leaves a leading `~` verbatim).
pub fn shell_prefs_from(settings: &Settings, home: Option<&Path>) -> ShellPrefs {
    ShellPrefs {
        command_prefix: settings
            .shell_command_prefix
            .clone()
            .filter(|prefix| !prefix.trim().is_empty()),
        shell_path: settings
            .shell_path
            .clone()
            .filter(|path| !path.trim().is_empty())
            .map(|path| expand_tilde_in_path(&path, home)),
        pool: settings.shell_pool.unwrap_or(true),
        command_timeout_ms: settings
            .tools
            .as_ref()
            .and_then(|tools| tools.command_timeout_ms),
    }
}

/// Expand a leading `~` / `~/` (and Windows `~\`) against an explicit `home`
/// — the `normalizePath` tilde rule. `home` is the caller's home directory
/// (`$HOME`, else `$USERPROFILE`), resolved once at the boundary so a compiled
/// path never captures an ambient read (the [`expand_tilde_in_pattern`]
/// precedent); `None` returns the path verbatim.
pub fn expand_tilde_in_path(path: &str, home: Option<&Path>) -> PathBuf {
    let Some(home) = home else {
        return PathBuf::from(path);
    };
    if path == "~" {
        return home.to_path_buf();
    }
    if let Some(rest) = path.strip_prefix("~/").or_else(|| path.strip_prefix("~\\")) {
        return home.join(rest);
    }
    PathBuf::from(path)
}

/// [`expand_tilde_in_path`] at the string level: the leading `~` of a
/// resource *filter pattern* expands against `home` — the caller's home
/// directory, resolved once at the boundary so a compiled pattern never
/// captures an ambient read — while the rest stays verbatim, so glob
/// wildcards survive (upstream `normalizePath` applied by `matchesAnyPattern`
/// / `normalizeExactPattern`, package-manager.ts:667-699). Only a bare `~`,
/// a `~/` and a Windows `~\` prefix expand; anything else (including `~user`)
/// is returned unchanged, as is a pattern compiled with `home = None`.
pub fn expand_tilde_in_pattern(pattern: &str, home: Option<&Path>) -> String {
    let Some(home) = home else {
        return pattern.to_owned();
    };
    let home = home.to_string_lossy();
    if pattern == "~" {
        return home.into_owned();
    }
    if let Some(rest) = pattern
        .strip_prefix("~/")
        .or_else(|| pattern.strip_prefix("~\\"))
    {
        // Join with the platform separator so a Windows home keeps its shape
        // (the caller normalizes separators to `/` afterwards).
        return PathBuf::from(home.as_ref())
            .join(rest)
            .to_string_lossy()
            .into_owned();
    }
    pattern.to_owned()
}
