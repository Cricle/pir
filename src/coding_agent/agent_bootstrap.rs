//! Best-effort pi-compatible agent-directory bootstrap at startup.
//!
//! Upstream pi 1.4 creates two files during a normal startup, before the
//! command's own work runs:
//!
//! - `<agent-dir>/auth.json` containing `{}` — `AuthStorage.create()`
//!   (`packages/coding-agent/src/core/auth-storage.ts`, `AuthStorage`
//!   constructor → `reload()` → `FileAuthStorageBackend.withLock` →
//!   `ensureFileExists`) creates it with mode `0o600`, and
//!   `ensureParentDir` creates the agent directory with mode `0o700`.
//! - `<agent-dir>/models-store.json` containing `{}` — the startup
//!   `ModelRuntime.create()` refresh reads every provider's entry through
//!   `FileModelsStore`, whose `FileAuthStorageBackend` creates the missing
//!   store with the same `{}` / `0o600` rule
//!   (`packages/coding-agent/src/core/models-store.ts`).
//!
//! Verified against the built bundle in an isolated `HOME`: `pi
//! --list-models`, `pi --help` and `pi print` all leave `auth.json` and
//! `models-store.json` behind. `settings.json` and `models.json` are NOT
//! created at startup (settings are only written on an explicit persist), so
//! this bootstrap deliberately does not create them either.
//!
//! The per-cwd `sessions/<encoded-cwd>/` directory is likewise not part of
//! startup: `SessionManager` creates it when a persistent session is created
//! (and metadata commands such as `--list-models` use an in-memory session),
//! so it stays out of this module. pir's `SessionManager::new` already
//! mirrors that behavior.
//!
//! Everything here is best-effort: an existing file is never modified, a
//! concurrent creation is accepted, and a permission/IO failure is reported
//! as a warning without aborting startup (a read-only agent directory must
//! not make the CLI unusable).

use std::fs;
use std::io::{ErrorKind, Write as _};
use std::path::Path;

use crate::ai::auth::storage::auth_path_in;
use crate::ai::providers::models_store::models_store_path;

/// Bootstrap `<agent_dir>` with the files upstream pi creates at startup.
///
/// Never overwrites an existing `auth.json` / `models-store.json`, and never
/// creates `settings.json`, `models.json` or the sessions tree.
pub fn bootstrap_agent_dir(agent_dir: &Path) {
    if let Err(error) = ensure_agent_dir(agent_dir) {
        tracing::warn!(
            "could not initialize agent directory {}: {error}",
            agent_dir.display()
        );
        return;
    }
    for path in [auth_path_in(agent_dir), models_store_path(agent_dir)] {
        if let Err(error) = ensure_empty_json_file(&path) {
            // A failed bootstrap must not fail startup: report and continue.
            tracing::warn!("could not initialize {}: {error}", path.display());
        }
    }
}

/// [`bootstrap_agent_dir`] for the process-resolved agent directory
/// (`PIR_AGENT_DIR` / `PIR_CODING_AGENT_DIR` / `PI_CODING_AGENT_DIR`, else
/// `~/.pi/agent`).
pub fn bootstrap_default_agent_dir() {
    bootstrap_agent_dir(&crate::ai::providers::agent_dir());
}

/// Create `dir` (and missing parents) if needed. Only directories created by
/// this call get mode `0o700`, matching upstream `mkdirSync(..., {recursive:
/// true, mode: 0o700})`: an existing directory keeps its administrator-managed
/// mode and ACLs.
fn ensure_agent_dir(dir: &Path) -> std::io::Result<()> {
    if dir.exists() {
        return Ok(());
    }
    // Collect the missing ancestors BEFORE creating anything so the mode is
    // applied exactly to the directories this call created.
    let mut missing = Vec::new();
    let mut current = Some(dir);
    while let Some(path) = current {
        if path.as_os_str().is_empty() || path.exists() {
            break;
        }
        missing.push(path.to_path_buf());
        current = path.parent();
    }
    fs::create_dir_all(dir)?;
    #[cfg(unix)]
    {
        for path in missing.iter().rev() {
            // Best-effort: a chmod failure (odd filesystem/ACL) must not
            // prevent the files from being created.
            let _ = set_dir_mode_0700(path);
        }
    }
    Ok(())
}

/// Create `path` with the literal contents `{}` when it does not exist.
///
/// `create_new` makes the existence check race-safe: a file created
/// concurrently by another pi process is left untouched (`AlreadyExists` is
/// success). On Unix the mode applies only on creation (`0o600`), exactly like
/// upstream's `writeFileSync(path, "{}", {mode: 0o600})`.
fn ensure_empty_json_file(path: &Path) -> std::io::Result<()> {
    if path.exists() {
        return Ok(());
    }
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = match options.open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == ErrorKind::AlreadyExists => return Ok(()),
        Err(error) => return Err(error),
    };
    if let Err(error) = file.write_all(b"{}") {
        // Do not leave a truncated bootstrap file behind for the next run to
        // treat as "already exists".
        drop(file);
        let _ = fs::remove_file(path);
        return Err(error);
    }
    Ok(())
}

#[cfg(unix)]
fn set_dir_mode_0700(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let metadata = fs::metadata(path)?;
    let mut permissions = metadata.permissions();
    permissions.set_mode(0o700);
    fs::set_permissions(path, permissions)
}
