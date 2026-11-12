//! Environment variable surface (upstream `config.ts` + args.ts help):
//! `PI_CODING_AGENT_DIR` (pir keeps `PIR_AGENT_DIR` too),
//! `PI_CODING_AGENT_SESSION_DIR`, `PI_OFFLINE` (+`PI_SKIP_VERSION_CHECK`),
//! `PI_TELEMETRY`, `PI_PACKAGE_DIR`. `PI_CLEAR_ON_SHRINK` and
//! `PI_HARDWARE_CURSOR` are folded into their settings accessors
//! (upstream `getClearOnShrink` / `getShowHardwareCursor`).

use std::path::PathBuf;

/// Whether startup network operations are disabled (`PI_OFFLINE=1/true/yes`,
/// also set by the `--offline` flag).
pub fn is_offline() -> bool {
    env_flag("PI_OFFLINE")
}

/// Whether the version/update check is skipped (`PI_SKIP_VERSION_CHECK` or
/// offline mode).
pub fn skip_version_check() -> bool {
    env_flag("PI_SKIP_VERSION_CHECK") || is_offline()
}

/// The `PI_TELEMETRY` install-telemetry override: `1/true/yes` forces on,
/// `0/false/no` forces off, unset/other leaves the setting in charge.
pub fn telemetry_override() -> Option<bool> {
    match std::env::var("PI_TELEMETRY") {
        Ok(value) => match value.trim().to_lowercase().as_str() {
            "1" | "true" | "yes" => Some(true),
            "0" | "false" | "no" => Some(false),
            _ => None,
        },
        Err(_) => None,
    }
}

/// The package asset directory (upstream `getPackageDir`): the `docs` section
/// of the default system prompt anchors its README/docs/examples paths here.
///
/// Resolution order:
/// 1. `PI_PACKAGE_DIR` when set and non-empty (the upstream override, for
///    Nix/Guix style store paths);
/// 2. the directory of the running executable when it looks like a pir
///    package root — a `README.md` file or a `docs` directory sits next to the
///    binary, which is exactly what the release archives ship (`pir`,
///    `pir-phone-server`, `README.md`). This is the analogue of upstream's
///    compiled-binary branch (`dirname(process.execPath)`,
///    `packages/coding-agent/src/config.ts:400-404`), where a bunded executable
///    cannot walk up to a `node_modules` package root;
/// 3. `None` (a bare `cargo build`/`cargo install` tree with no assets next to
///    the binary), and the prompt then omits the `docs` section — the same
///    outcome as upstream raising no package root.
///
/// The probe lives HERE, at resolution time, exactly like upstream's
/// `getPackageDir` walk: the prompt builder itself never touches the
/// filesystem for this (`system_prompt::package_docs_paths`).
pub fn package_dir() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("PI_PACKAGE_DIR").filter(|value| !value.is_empty()) {
        return Some(PathBuf::from(dir));
    }
    // The executable is fixed for the process life, so the probe runs once.
    // `PI_PACKAGE_DIR` is still consulted on every call (above): tests and the
    // D6 session pin change it mid-process.
    static EXECUTABLE_ROOT: std::sync::OnceLock<Option<PathBuf>> = std::sync::OnceLock::new();
    EXECUTABLE_ROOT
        .get_or_init(|| {
            let dir = std::env::current_exe().ok()?.parent()?.to_path_buf();
            let is_package_root = dir.join("README.md").is_file() || dir.join("docs").is_dir();
            is_package_root.then_some(dir)
        })
        .clone()
}

/// The session-dir environment override: `PI_CODING_AGENT_SESSION_DIR` (the
/// upstream name) first, then the pir-native `PIR_SESSION_DIR`.
pub fn session_dir_override() -> Option<String> {
    ["PI_CODING_AGENT_SESSION_DIR", "PIR_SESSION_DIR"]
        .into_iter()
        .find_map(|name| std::env::var(name).ok().filter(|value| !value.is_empty()))
}

/// `PIR_MAX_TURNS`: per-prompt agent-loop turn cap of the non-interactive
/// hosts (print/RPC/server/SDK).
///
/// A turn is one model request, not one tool call. Only a positive integer is
/// accepted: `0`, a negative value or junk is ignored and the host default
/// ([`crate::config::defaults::DEFAULT_AGENT_MAX_TURNS`]) applies, so a typo
/// cannot disable the runaway guard. The interactive TUI does not read this
/// variable (it keeps [`crate::config::defaults::INTERACTIVE_AGENT_MAX_TURNS`]).
pub const MAX_TURNS_ENV: &str = "PIR_MAX_TURNS";

/// Resolve [`MAX_TURNS_ENV`] through an explicit environment accessor. The
/// process environment is [`max_turns`]; tests pass a map so they never
/// mutate process-wide state (the same pattern as
/// `ServerSecurityConfig::from_env_lookup`).
pub fn max_turns_from_lookup(lookup: impl Fn(&str) -> Option<String>) -> usize {
    lookup(MAX_TURNS_ENV)
        .and_then(|value| value.trim().parse::<usize>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(crate::config::defaults::DEFAULT_AGENT_MAX_TURNS)
}

/// The effective non-interactive turn cap for this process.
pub fn max_turns() -> usize {
    max_turns_from_lookup(|name| std::env::var(name).ok())
}

/// Whether an environment flag is set to a truthy value
/// (`1`/`true`/`yes`, case-insensitive; unset or any other value is false).
pub fn env_flag(name: &str) -> bool {
    std::env::var(name)
        .map(|value| {
            let value = value.trim().to_lowercase();
            matches!(value.as_str(), "1" | "true" | "yes")
        })
        .unwrap_or(false)
}
