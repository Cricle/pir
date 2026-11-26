//! pi settings.json access for the provider stack (global + project scopes).
//!
//! The full settings surface (schema, migrations, deep merge with the
//! `defaultTools` modifier rule, trust gating, persistence) lives in
//! [`crate::config::settings`]; this module is the provider-side projection.

use std::path::{Path, PathBuf};

use crate::config::settings::Settings;

/// Agent dir, the single source of truth for every module: `$PIR_AGENT_DIR`
/// (pir), `$PIR_CODING_AGENT_DIR` (pir alias), `$PI_CODING_AGENT_DIR` (pi
/// compatibility), else `~/.pi/agent`.
pub fn agent_dir() -> PathBuf {
    resolve_agent_dir_at(
        [
            std::env::var_os("PIR_AGENT_DIR"),
            std::env::var_os("PIR_CODING_AGENT_DIR"),
            std::env::var_os("PI_CODING_AGENT_DIR"),
        ],
        std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")),
    )
}

/// The resolution [`agent_dir`] performs, against explicitly supplied variable
/// values: the three agent-dir spellings in precedence order plus the home
/// directory used for the `~/.pi/agent` fallback.
///
/// The one implementation both faces share, so the precedence (and the
/// empty-value rule) cannot drift between the process form and a caller that
/// passes its own values — a harness proving the ordering needs no process
/// environment writes.
pub fn resolve_agent_dir_at(
    candidates: [Option<std::ffi::OsString>; 3],
    home: Option<std::ffi::OsString>,
) -> PathBuf {
    for dir in candidates.into_iter().flatten() {
        if !dir.is_empty() {
            return PathBuf::from(dir);
        }
    }
    let home = home
        .map(PathBuf::from)
        .unwrap_or_else(|| Path::new("~").to_path_buf());
    home.join(".pi").join("agent")
}

/// Project config dir: `<cwd>/.pi`.
pub fn project_config_dir(cwd: &Path) -> PathBuf {
    cwd.join(".pi")
}

/// Load the merged settings snapshot (global `~/.pi/agent/settings.json`,
/// overlaid by `<cwd>/.pi/settings.json` when the project is trusted).
///
/// Missing files are not errors, and neither are malformed, unreadable or
/// non-object files: [`crate::config::settings::SettingsManager`] records
/// those as diagnostics, keeps the defaults for the scope and never fails the
/// load (upstream `SettingsManager.create` — a settings file cannot abort
/// startup). The CLI startup printer reports the recorded failures once via
/// [`crate::config::settings::collect_settings_diagnostics`], so this loader
/// must NOT turn them into a provider error: a JSON typo in `settings.json`
/// is not an authentication failure and must not stop a run that can still
/// proceed on the defaults (PIR-BIN-071).
///
/// Deciding the policy here is deliberate: every provider-facing caller
/// (`load_pi_environment`, `load_pi_config_with`, the SDK registry, the
/// server) consumes the degraded snapshot, exactly like `--list-models` and
/// the `sessions` command already did.
pub fn load_settings(cwd: Option<&Path>, project_trusted: bool) -> Settings {
    load_settings_in(&agent_dir(), cwd, project_trusted)
}

/// [`load_settings`] with an EXPLICIT global agent dir, for callers that own
/// one instead of reading the process environment (the SDK's
/// per-agent-directory path). Same tolerant policy: a broken scope degrades
/// to its defaults and is recorded on the manager, never returned as an error.
pub fn load_settings_in(global_dir: &Path, cwd: Option<&Path>, project_trusted: bool) -> Settings {
    let project_dir = cwd
        .map(project_config_dir)
        .unwrap_or_else(|| PathBuf::from("."));
    crate::config::settings::SettingsManager::load(
        global_dir,
        &project_dir,
        cwd.unwrap_or_else(|| Path::new(".")),
        project_trusted,
    )
    .settings
}
