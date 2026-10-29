//! One-time startup migrations (Rust equivalent of upstream
//! `packages/coding-agent/src/migrations.ts`), run once per launch before any
//! mode host starts:
//!
//! 1. legacy `oauth.json` + `settings.json` `apiKeys` → `auth.json`
//!    ([`migrate_auth_to_auth_json`]);
//! 2. session files that a buggy writer left directly in the agent root into
//!    the per-cwd session directories ([`migrate_sessions_from_agent_root`]);
//! 3. managed `fd`/`rg` binaries `tools/` → `bin/` ([`migrate_tools_to_bin`]);
//! 4. `commands/` → `prompts/` for the global agent dir and the project
//!    `.pi` dir ([`migrate_commands_to_prompts`]), collecting deprecation
//!    warnings for the legacy `hooks/` and `tools/` directories.
//!
//! pir's `keybindings.json` migration is NOT duplicated here: it is owned by
//! the keybindings loader (`src/config/keybindings.rs`), which rewrites a
//! migrated file in place on every load (upstream keeps both a standalone
//! file migration and the loader-side one; pir folds them into the loader).
//!
//! Session-format note: only upstream-format JSONL headers
//! (`{"type":"session","cwd":...}`) are relocated. pir never migrated its own
//! legacy v4 session files (MAPPING.md) — a v4 file left in the agent root is
//! skipped exactly like any non-session file.

use std::io::IsTerminal;
use std::path::{Path, PathBuf};

/// Extension migration guide (upstream `MIGRATION_GUIDE_URL`).
pub const MIGRATION_GUIDE_URL: &str = "https://github.com/earendil-works/pi/blob/main/packages/coding-agent/CHANGELOG.md#extensions-migration";
/// Extensions documentation (upstream `EXTENSIONS_DOC_URL`).
pub const EXTENSIONS_DOC_URL: &str =
    "https://github.com/earendil-works/pi/blob/main/packages/coding-agent/docs/extensions.md";

/// The results of [`run_migrations`].
#[derive(Debug, Default, Clone)]
pub struct MigrationResult {
    /// Provider ids migrated into `auth.json` (upstream
    /// `migratedAuthProviders`).
    pub migrated_auth_providers: Vec<String>,
    /// Deprecation warnings for the interactive host to surface (upstream
    /// `deprecationWarnings`; pir prints them before the TUI starts).
    pub deprecation_warnings: Vec<String>,
}

/// Run all migrations (upstream `runMigrations`), called once on startup.
/// Every step is best-effort: unreadable or mid-migration files are skipped.
pub fn run_migrations(cwd: &Path) -> MigrationResult {
    run_migrations_in(&crate::coding_agent::context::agent_dir(), cwd)
}

/// [`run_migrations`] against an explicit agent dir (tests and embedders that
/// already hold the resolved dir — keeps the env-free path testable).
pub fn run_migrations_in(agent_dir: &Path, cwd: &Path) -> MigrationResult {
    let migrated_auth_providers = migrate_auth_to_auth_json(agent_dir);
    migrate_sessions_from_agent_root(agent_dir);
    migrate_tools_to_bin(agent_dir);
    // Project scope: `<cwd>/.pi` (upstream `CONFIG_DIR_NAME`).
    let project_dir = cwd.join(".pi");
    migrate_commands_to_prompts(agent_dir, "Global");
    migrate_commands_to_prompts(&project_dir, "Project");
    let deprecation_warnings = [
        check_deprecated_extension_dirs(agent_dir, "Global"),
        check_deprecated_extension_dirs(&project_dir, "Project"),
    ]
    .concat();
    MigrationResult {
        migrated_auth_providers,
        deprecation_warnings,
    }
}

// ---------------------------------------------------------------------------
// auth storage → auth.json (upstream `migrateAuthToAuthJson`)
// ---------------------------------------------------------------------------

/// Migrate legacy `oauth.json` and `settings.json` `apiKeys` into
/// `auth.json`. Skipped entirely when `auth.json` already exists. Returns the
/// migrated provider ids in migration order (oauth entries first, then api
/// keys).
///
/// The destination is written (atomically, mode `0o600`) BEFORE either source
/// is consumed: a failed `auth.json` write leaves `oauth.json` and the
/// `settings.json` `apiKeys` untouched, so a full disk / read-only agent dir
/// cannot silently destroy the only copy of a credential.
pub fn migrate_auth_to_auth_json(agent_dir: &Path) -> Vec<String> {
    let auth_path = agent_dir.join("auth.json");
    let oauth_path = agent_dir.join("oauth.json");
    let settings_path = agent_dir.join("settings.json");

    // Skip if auth.json already exists.
    if auth_path.exists() {
        return Vec::new();
    }

    let mut migrated = serde_json::Map::new();
    let mut providers = Vec::new();
    let mut oauth_parsed = false;

    // Read oauth.json (do not consume it yet).
    if oauth_path.exists() {
        if let Ok(contents) = std::fs::read_to_string(&oauth_path) {
            if let Ok(oauth) = serde_json::from_str::<serde_json::Value>(strip_bom(&contents)) {
                if let Some(entries) = oauth.as_object() {
                    for (provider, cred) in entries {
                        let mut credential = match cred {
                            serde_json::Value::Object(map) => map.clone(),
                            _ => continue,
                        };
                        credential.insert(
                            "type".to_string(),
                            serde_json::Value::String("oauth".to_string()),
                        );
                        migrated.insert(provider.clone(), serde_json::Value::Object(credential));
                        providers.push(provider.clone());
                    }
                    oauth_parsed = true;
                }
            }
        }
    }

    // Read settings.json apiKeys (the file is only rewritten after auth.json
    // has been written successfully).
    let mut settings_without_api_keys: Option<serde_json::Map<String, serde_json::Value>> = None;
    if settings_path.exists() {
        if let Ok(contents) = std::fs::read_to_string(&settings_path) {
            if let Ok(mut settings) = serde_json::from_str::<
                serde_json::Map<String, serde_json::Value>,
            >(strip_bom(&contents))
            {
                if let Some(api_keys) = settings
                    .get("apiKeys")
                    .filter(|keys| keys.is_object())
                    .cloned()
                {
                    if let Some(entries) = api_keys.as_object() {
                        for (provider, key) in entries {
                            if migrated.contains_key(provider) {
                                continue;
                            }
                            if let Some(key) = key.as_str() {
                                migrated.insert(
                                    provider.clone(),
                                    serde_json::json!({ "type": "api_key", "key": key }),
                                );
                                providers.push(provider.clone());
                            }
                        }
                    }
                    settings.remove("apiKeys");
                    settings_without_api_keys = Some(settings);
                }
            }
        }
    }

    if !migrated.is_empty() {
        let rendered = match serde_json::to_string_pretty(&migrated) {
            Ok(rendered) => rendered + "\n",
            Err(error) => {
                eprintln!("Warning: could not migrate credentials to auth.json: {error}");
                return Vec::new();
            }
        };
        if let Err(error) =
            crate::config::settings::write_private_text_atomic(&auth_path, &rendered)
        {
            eprintln!(
                "Warning: could not migrate credentials to {}: {error}",
                auth_path.display()
            );
            return Vec::new();
        }
    }

    // The credentials are safely persisted: consume the legacy sources.
    if oauth_parsed {
        let _ = std::fs::rename(&oauth_path, oauth_path.with_extension("json.migrated"));
    }
    if let Some(settings) = settings_without_api_keys {
        if let Ok(rendered) = serde_json::to_string_pretty(&settings).map(|json| json + "\n") {
            if let Err(error) =
                crate::config::settings::write_text_atomic(&settings_path, &rendered)
            {
                eprintln!(
                    "Warning: could not remove apiKeys from {}: {error}",
                    settings_path.display()
                );
            }
        }
    }

    providers
}

/// Strip a leading UTF-8 BOM (upstream `stripBom`).
fn strip_bom(contents: &str) -> &str {
    contents.trim_start_matches('\u{feff}')
}

/// Upper bound for the session-header line read by
/// [`migrate_sessions_from_agent_root`]. Real headers are a few hundred bytes;
/// a longer "first line" cannot be a valid session header anyway.
const MAX_SESSION_HEADER_BYTES: u64 = 64 * 1024;

/// Read at most `max_bytes` of the first line of `path` (UTF-8 lossily
/// decoded). `None` when the file cannot be opened or is empty.
fn read_first_line_bounded(path: &Path, max_bytes: u64) -> Option<String> {
    use std::io::{BufRead, BufReader, Read};
    let file = std::fs::File::open(path).ok()?;
    let mut line = Vec::new();
    BufReader::new(file)
        .take(max_bytes)
        .read_until(b'\n', &mut line)
        .ok()?;
    if line.is_empty() {
        return None;
    }
    Some(String::from_utf8_lossy(&line).into_owned())
}

// ---------------------------------------------------------------------------
// stray agent-root sessions → per-cwd session dirs (upstream
// `migrateSessionsFromAgentRoot`)
// ---------------------------------------------------------------------------

/// Move `*.jsonl` files sitting directly in the agent dir into the proper
/// per-cwd session directory derived from the `cwd` in their session header
/// (upstream v0.30.0 bug, `pi-mono#320`). Files without a parseable upstream
/// session header — including pir's skipped legacy v4 format — are left in
/// place.
pub fn migrate_sessions_from_agent_root(agent_dir: &Path) {
    let entries = match std::fs::read_dir(agent_dir) {
        Ok(entries) => entries,
        Err(_) => return,
    };
    for entry in entries.filter_map(std::result::Result::ok) {
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("jsonl") {
            continue;
        }
        // Only the header line is needed. Read it through a bounded buffer:
        // a stray multi-gigabyte session file must not be read into memory at
        // every startup just to decide whether to move it.
        let Some(first_line) = read_first_line_bounded(&path, MAX_SESSION_HEADER_BYTES) else {
            continue;
        };
        if first_line.trim().is_empty() {
            continue;
        }
        let Ok(header) = serde_json::from_str::<serde_json::Value>(&first_line) else {
            continue;
        };
        if header.get("type").and_then(serde_json::Value::as_str) != Some("session") {
            continue;
        }
        let Some(cwd) = header.get("cwd").and_then(serde_json::Value::as_str) else {
            continue;
        };

        // The correct session directory: pir's own session-manager encoding
        // of the header cwd (upstream computes the same shape from the raw
        // header value; pir routes through `encode_cwd_dir_name` +
        // `resolve_path_str` so the file lands where pir's lookup finds it).
        let file_name = match path.file_name() {
            Some(name) => name.to_owned(),
            None => continue,
        };
        let target_dir =
            agent_dir
                .join("sessions")
                .join(crate::session_manager::encode_cwd_dir_name(
                    &crate::session_manager::resolve_path_str(cwd),
                ));
        if std::fs::create_dir_all(&target_dir).is_err() {
            continue;
        }
        let new_path = target_dir.join(&file_name);
        if new_path.exists() {
            continue;
        }
        let _ = std::fs::rename(&path, &new_path);
    }
}

// ---------------------------------------------------------------------------
// managed binaries tools/ → bin/ (upstream `migrateToolsToBin`)
// ---------------------------------------------------------------------------

/// The managed binary dir (upstream `config.ts getBinDir`).
pub fn bin_dir(agent_dir: &Path) -> PathBuf {
    agent_dir.join("bin")
}

/// Move `fd`/`rg` binaries from `tools/` to `bin/` if they exist. Returns
/// whether anything was moved (a message is printed when it was).
pub fn migrate_tools_to_bin(agent_dir: &Path) -> bool {
    let tools_dir = agent_dir.join("tools");
    let bin_dir = bin_dir(agent_dir);

    if !tools_dir.exists() {
        return false;
    }

    let binaries = ["fd", "rg", "fd.exe", "rg.exe"];
    let mut moved_any = false;

    for binary in binaries {
        let old_path = tools_dir.join(binary);
        let new_path = bin_dir.join(binary);

        if old_path.exists() {
            if !bin_dir.exists() && std::fs::create_dir_all(&bin_dir).is_err() {
                continue;
            }
            if !new_path.exists() {
                if std::fs::rename(&old_path, &new_path).is_ok() {
                    moved_any = true;
                }
            } else {
                // Target exists, just delete the old one.
                let _ = std::fs::remove_file(&old_path);
            }
        }
    }

    if moved_any {
        // stderr: migrations run before the mode host is known, and a
        // migration notice must never corrupt an RPC/JSON stdout stream.
        eprintln!(
            "{}",
            styled_stderr("32", "Migrated managed binaries tools/ → bin/")
        );
    }
    moved_any
}

// ---------------------------------------------------------------------------
// commands/ → prompts/ + deprecated dirs (upstream `migrateExtensionSystem`)
// ---------------------------------------------------------------------------

/// Rename `commands/` → `prompts/` once (works for directories and symlinks).
/// Prints the upstream green migration message, or a yellow warning when the
/// rename fails. Returns whether the migration happened.
pub fn migrate_commands_to_prompts(base_dir: &Path, label: &str) -> bool {
    let commands_dir = base_dir.join("commands");
    let prompts_dir = base_dir.join("prompts");

    if commands_dir.exists() && !prompts_dir.exists() {
        match std::fs::rename(&commands_dir, &prompts_dir) {
            Ok(()) => {
                eprintln!(
                    "{}",
                    styled_stderr("32", &format!("Migrated {label} commands/ → prompts/"))
                );
                return true;
            }
            Err(error) => {
                eprintln!(
                    "{}",
                    styled_stderr(
                        "33",
                        &format!(
                            "Warning: Could not migrate {label} commands/ to prompts/: {error}"
                        )
                    )
                );
            }
        }
    }
    false
}

/// Collect deprecation warnings for the legacy `hooks/` and `tools/`
/// directories (upstream `checkDeprecatedExtensionDirs`). `tools/` only warns
/// when it holds something other than the auto-extracted `fd`/`rg` binaries
/// and hidden files.
pub fn check_deprecated_extension_dirs(base_dir: &Path, label: &str) -> Vec<String> {
    let mut warnings = Vec::new();

    let hooks_dir = base_dir.join("hooks");
    if hooks_dir.exists() {
        warnings.push(format!(
            "{label} hooks/ directory found. Hooks have been renamed to extensions."
        ));
    }

    let tools_dir = base_dir.join("tools");
    if tools_dir.exists() {
        if let Ok(entries) = std::fs::read_dir(&tools_dir) {
            let custom_tools = entries.filter_map(std::result::Result::ok).filter(|entry| {
                let name = entry.file_name();
                let name = name.to_string_lossy();
                let lower = name.to_lowercase();
                lower != "fd"
                    && lower != "rg"
                    && lower != "fd.exe"
                    && lower != "rg.exe"
                    && !name.starts_with('.')
            });
            if custom_tools.count() > 0 {
                warnings.push(format!(
                    "{label} tools/ directory contains custom tools. Custom tools have been merged into extensions."
                ));
            }
        }
    }

    warnings
}

// ---------------------------------------------------------------------------
// deprecation warnings (upstream `showDeprecationWarnings`)
// ---------------------------------------------------------------------------

/// Print the deprecation warnings and — when stdin is a terminal — wait for
/// any keypress (upstream `showDeprecationWarnings`, raw-mode single read).
pub fn show_deprecation_warnings(warnings: &[String]) {
    if warnings.is_empty() {
        return;
    }

    for warning in warnings {
        println!("{}", styled("33", &format!("Warning: {warning}")));
    }
    println!(
        "{}",
        styled("33", "\nMove your extensions to the extensions/ directory.")
    );
    println!(
        "{}",
        styled("33", &format!("Migration guide: {MIGRATION_GUIDE_URL}"))
    );
    println!(
        "{}",
        styled("33", &format!("Documentation: {EXTENSIONS_DOC_URL}"))
    );
    println!("{}", styled("2", "\nPress any key to continue..."));

    wait_for_any_keypress();
    println!();
}

/// SGR `code`-color `text` when stdout is a terminal (chalk colors to stdout;
/// pir inlines the ANSI codes so non-TTY output stays clean).
fn styled(code: &str, text: &str) -> String {
    styled_for(code, text, std::io::stdout().is_terminal())
}

/// [`styled`] for diagnostics written to stderr.
fn styled_stderr(code: &str, text: &str) -> String {
    styled_for(code, text, std::io::stderr().is_terminal())
}

fn styled_for(code: &str, text: &str, is_terminal: bool) -> String {
    if is_terminal {
        format!("\x1b[{code}m{text}\x1b[0m")
    } else {
        text.to_string()
    }
}

/// One raw-mode read (upstream waits for the first stdin `data` event);
/// silently skipped when raw mode is unavailable.
fn wait_for_any_keypress() {
    // Raw mode is a TUI facility: without the TUI the wait is compiled out
    // (the same path this takes when raw mode is unavailable).
    #[cfg(feature = "tui")]
    {
        use crossterm::event::read;
        if !std::io::stdin().is_terminal() {
            return;
        }
        if crossterm::terminal::enable_raw_mode().is_err() {
            return;
        }
        let _ = read();
        let _ = crossterm::terminal::disable_raw_mode();
    }
}
