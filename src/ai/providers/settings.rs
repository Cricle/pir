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
    for var in [
        "PIR_AGENT_DIR",
        "PIR_CODING_AGENT_DIR",
        "PI_CODING_AGENT_DIR",
    ] {
        if let Some(dir) = std::env::var_os(var) {
            if !dir.is_empty() {
                return PathBuf::from(dir);
            }
        }
    }
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
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
/// Missing files are not errors; malformed files are recorded on the manager.
pub fn load_settings(cwd: Option<&Path>, project_trusted: bool) -> Result<Settings, String> {
    let global_dir = agent_dir();
    let project_dir = cwd
        .map(project_config_dir)
        .unwrap_or_else(|| PathBuf::from("."));
    let manager = crate::config::settings::SettingsManager::load(
        &global_dir,
        &project_dir,
        cwd.unwrap_or_else(|| Path::new(".")),
        project_trusted,
    );
    let errors = manager.error_messages();
    if !errors.is_empty() {
        return Err(errors.join("; "));
    }
    Ok(manager.settings)
}
