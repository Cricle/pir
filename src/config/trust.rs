//! Project trust: the `<agentDir>/trust.json` store (`{canonicalPath: bool |
//! null}`, nearest-ancestor lookup), the trust-gated resource list, and the
//! `defaultProjectTrust` resolution flow (upstream `core/trust-manager.ts`
//! and `core/project-trust.ts`).

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::config::settings::DefaultProjectTrust;

/// Project-local resources under `<cwd>/.pi` that require project trust
/// (upstream `TRUST_REQUIRING_PROJECT_CONFIG_RESOURCES`).
pub const TRUST_REQUIRING_PROJECT_CONFIG_RESOURCES: [&str; 8] = [
    "settings.json",
    "mcp.json",
    "extensions",
    "skills",
    "prompts",
    "themes",
    "SYSTEM.md",
    "APPEND_SYSTEM.md",
];

/// A trust decision: `true` (trusted), `false` (not trusted), or `null`
/// (the key is removed from the store on write).
pub type ProjectTrustDecision = Option<bool>;

/// One stored trust decision resolved for a directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectTrustStoreEntry {
    /// The canonical path the decision is stored under.
    pub path: PathBuf,
    pub decision: bool,
}

/// Normalize `cwd` for store identity: canonicalized (symlinks resolved,
/// Windows extended-length prefixes stripped).
pub fn normalize_cwd(cwd: &Path) -> PathBuf {
    let canonical = fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf());
    let text = canonical.to_string_lossy();
    if let Some(rest) = text.strip_prefix(r"\\?\UNC\") {
        PathBuf::from(format!(r"\\{rest}"))
    } else if let Some(rest) = text.strip_prefix(r"\\?\") {
        PathBuf::from(rest)
    } else {
        canonical
    }
}

/// The trust options offered by the interactive selector (upstream
/// `getProjectTrustOptions`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectTrustOption {
    pub label: String,
    pub trusted: bool,
    /// Store updates to apply when the option is chosen (`decision: None`
    /// removes the key).
    pub updates: Vec<(PathBuf, ProjectTrustDecision)>,
    /// True for the "this session only" options (upstream options created with
    /// `includeSessionOnly`): they carry no `updates`, so the decision applies
    /// to the current run and is never written to `trust.json`.
    pub session_only: bool,
}

/// The trust file shape: `{ "<canonicalPath>": true | false | null }`.
type TrustFile = BTreeMap<String, Value>;

pub fn read_trust_file(path: &Path) -> Result<TrustFile, String> {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(BTreeMap::new());
        }
        Err(error) => return Err(format!("cannot read {}: {error}", path.display())),
    };
    let text = text.strip_prefix('\u{feff}').unwrap_or(&text);
    let parsed: Value = serde_json::from_str(text)
        .map_err(|error| format!("failed to read trust store {}: {error}", path.display()))?;
    let map = parsed
        .as_object()
        .ok_or_else(|| format!("invalid trust store {}: expected an object", path.display()))?;
    let mut data = BTreeMap::new();
    for (key, value) in map {
        if !matches!(value, Value::Bool(_) | Value::Null) {
            return Err(format!(
                "invalid trust store {}: value for \"{key}\" must be true, false, or null",
                path.display()
            ));
        }
        data.insert(key.clone(), value.clone());
    }
    Ok(data)
}

/// Bounded acquisition attempts for the trust-file lock (upstream
/// `acquireTrustLockSync` retries 10 times with a 20ms sleep).
const TRUST_LOCK_ATTEMPTS: u32 = 10;
/// Delay between lock acquisition attempts.
const TRUST_LOCK_RETRY_DELAY: std::time::Duration = std::time::Duration::from_millis(20);
/// A lock older than this window is considered abandoned (its owner died
/// mid-write) and is taken over (the crate-level stale-lock window used by
/// the other file locks in this crate, `ai/auth/storage.rs`).
const TRUST_LOCK_STALE_AFTER: std::time::Duration = std::time::Duration::from_secs(10);

/// RAII release for the trust-file lock. The owner token is checked on
/// release so a process that lost a stale-lock takeover race cannot delete the
/// new owner's lock.
struct TrustFileLock {
    path: PathBuf,
    token: String,
}

impl Drop for TrustFileLock {
    fn drop(&mut self) {
        if fs::read_to_string(&self.path)
            .ok()
            .is_some_and(|contents| contents == self.token)
        {
            let _ = fs::remove_file(&self.path);
        }
    }
}

/// Acquire `<trust.json>.lock` exclusively (`create_new`), retrying like
/// upstream `acquireTrustLockSync` and taking over a stale lock after
/// [`TRUST_LOCK_STALE_AFTER`]. The parent directory is created first
/// (upstream `mkdirSync(trustDir)`).
fn acquire_trust_file_lock(trust_path: &Path) -> Result<TrustFileLock, String> {
    use std::io::Write;
    use std::sync::atomic::{AtomicU64, Ordering};

    static LOCK_SEQUENCE: AtomicU64 = AtomicU64::new(0);

    if let Some(parent) = trust_path.parent() {
        if !parent.exists() {
            fs::create_dir_all(parent)
                .map_err(|error| format!("cannot create {}: {error}", parent.display()))?;
        }
    }
    let lock_path = PathBuf::from(format!("{}.lock", trust_path.display()));
    let token = format!(
        "{}-{}",
        std::process::id(),
        LOCK_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    );
    for attempt in 0..TRUST_LOCK_ATTEMPTS {
        match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&lock_path)
        {
            Ok(mut file) => {
                if let Err(error) = file.write_all(token.as_bytes()) {
                    let _ = fs::remove_file(&lock_path);
                    return Err(format!("cannot write {}: {error}", lock_path.display()));
                }
                return Ok(TrustFileLock {
                    path: lock_path,
                    token,
                });
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                let stale = fs::metadata(&lock_path)
                    .and_then(|metadata| metadata.modified())
                    .map(|modified| modified.elapsed().unwrap_or_default() > TRUST_LOCK_STALE_AFTER)
                    .unwrap_or(false);
                if stale {
                    let _ = fs::remove_file(&lock_path);
                    continue;
                }
                if attempt + 1 < TRUST_LOCK_ATTEMPTS {
                    std::thread::sleep(TRUST_LOCK_RETRY_DELAY);
                }
            }
            Err(error) => {
                return Err(format!("cannot create {}: {error}", lock_path.display()));
            }
        }
    }
    Err(format!(
        "failed to acquire trust store lock: {}",
        lock_path.display()
    ))
}

/// Run one trust-store operation under the cross-process lock (upstream
/// `withTrustFileLock`). Trade-off vs upstream: the lock serializes
/// read-modify-write sequences, while the atomic temp-file rename in
/// [`write_trust_file`] additionally keeps lock-free readers (other
/// processes mid-read) from ever observing a torn store; the lock file is
/// best-effort — a crashed holder is taken over after the stale window.
fn with_trust_file_lock<T>(
    trust_path: &Path,
    operation: impl FnOnce() -> Result<T, String>,
) -> Result<T, String> {
    let _lock = acquire_trust_file_lock(trust_path)?;
    operation()
}

/// Write the trust file atomically: serialize to a same-directory temp file,
/// then rename it over the target (upstream `writeTrustFile` writes in place
/// under the lock file; the temp+rename keeps the store consistent even
/// without the lock). The temp name is per-process and callers hold the
/// lock, so a leftover temp from a crashed writer is never picked up.
fn write_trust_file(path: &Path, data: &TrustFile) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        if !parent.exists() {
            fs::create_dir_all(parent)
                .map_err(|error| format!("cannot create {}: {error}", parent.display()))?;
        }
    }
    let serialized = serde_json::to_string_pretty(data)
        .map_err(|error| format!("cannot serialize trust store {}: {error}", path.display()))?;
    let file_name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "trust.json".to_owned());
    let temp_path = path
        .parent()
        .map(|parent| parent.join(format!("{file_name}.tmp-{}", std::process::id())))
        .unwrap_or_else(|| PathBuf::from(format!("{file_name}.tmp-{}", std::process::id())));
    fs::write(&temp_path, format!("{serialized}\n"))
        .map_err(|error| format!("cannot write {}: {error}", temp_path.display()))?;
    // Same-directory rename is an atomic replace (POSIX rename; Windows
    // `MoveFileEx` with `MOVEFILE_REPLACE_EXISTING`).
    fs::rename(&temp_path, path).map_err(|error| {
        let _ = fs::remove_file(&temp_path);
        format!("cannot replace {}: {error}", path.display())
    })
}

/// Persistent project-trust store backed by `<agentDir>/trust.json`.
pub struct ProjectTrustStore {
    path: PathBuf,
}

impl ProjectTrustStore {
    pub fn new(agent_dir: &Path) -> Self {
        Self {
            path: agent_dir.join("trust.json"),
        }
    }

    /// The store file path.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The nearest-ancestor trust decision for `cwd`: walk from `cwd` up to
    /// the filesystem root and return the first entry with a `true`/`false`
    /// decision (upstream `findNearestTrustEntry`). The read runs under the
    /// trust-file lock (upstream `getEntry` → `withTrustFileLock`).
    pub fn get(&self, cwd: &Path) -> Option<ProjectTrustStoreEntry> {
        let data = match with_trust_file_lock(&self.path, || read_trust_file(&self.path)) {
            Ok(data) => data,
            // A broken trust store (or an unacquirable lock) must not
            // silently trust the project. (Upstream `getEntry` throws; the
            // pir lookup API maps every failure to `None`.)
            Err(_) => return None,
        };
        let mut current = normalize_cwd(cwd);
        loop {
            if let Some(value) = data.get(&current.to_string_lossy().into_owned()) {
                if let Some(decision) = value.as_bool() {
                    return Some(ProjectTrustStoreEntry {
                        path: current,
                        decision,
                    });
                }
            }
            let parent = current.parent()?;
            if parent == current {
                return None;
            }
            current = parent.to_path_buf();
        }
    }

    /// Store one decision for `path` (`None` removes the entry).
    pub fn set(&self, path: &Path, decision: ProjectTrustDecision) -> Result<(), String> {
        self.set_many(std::iter::once((path.to_path_buf(), decision)))
    }

    /// Store several decisions atomically (upstream `setMany`): the
    /// read-modify-write runs under the trust-file lock, so concurrent
    /// writers cannot drop each other's entries.
    pub fn set_many<I>(&self, decisions: I) -> Result<(), String>
    where
        I: IntoIterator<Item = (PathBuf, ProjectTrustDecision)>,
    {
        with_trust_file_lock(&self.path, || {
            let mut data = read_trust_file(&self.path)?;
            for (path, decision) in decisions {
                let key = normalize_cwd(&path).to_string_lossy().into_owned();
                match decision {
                    Some(decision) => {
                        data.insert(key, Value::Bool(decision));
                    }
                    None => {
                        data.remove(&key);
                    }
                }
            }
            write_trust_file(&self.path, &data)
        })
    }
}

/// The parent directory used by the "Trust parent folder" option (upstream
/// `getProjectTrustParentPath`).
pub fn get_project_trust_parent_path(cwd: &Path) -> Option<PathBuf> {
    let trust_path = normalize_cwd(cwd);
    trust_path.parent().map(Path::to_path_buf)
}

/// The trust selector options for `cwd` (upstream `getProjectTrustOptions`
/// without `includeSessionOnly`): Trust, Trust parent folder, Do not trust.
/// This is the `/trust` selector list, whose decisions always persist.
pub fn get_project_trust_options(cwd: &Path) -> Vec<ProjectTrustOption> {
    get_project_trust_options_inner(cwd, false)
}

/// The STARTUP trust selector list (upstream
/// `getProjectTrustOptions(cwd, { includeSessionOnly: true })`): the
/// persistent options plus "Trust (this session only)" and "Do not trust
/// (this session only)". The session-only options have no store updates, so
/// choosing one decides the current run without writing `trust.json`
/// (upstream `saveProjectTrustPromptResult` skips empty `updates`).
pub fn get_project_trust_options_including_session_only(cwd: &Path) -> Vec<ProjectTrustOption> {
    get_project_trust_options_inner(cwd, true)
}

fn get_project_trust_options_inner(
    cwd: &Path,
    include_session_only: bool,
) -> Vec<ProjectTrustOption> {
    let trust_path = normalize_cwd(cwd);
    let mut options = vec![ProjectTrustOption {
        label: "Trust".to_string(),
        trusted: true,
        updates: vec![(trust_path.clone(), Some(true))],
        session_only: false,
    }];
    if let Some(parent) = get_project_trust_parent_path(cwd) {
        options.push(ProjectTrustOption {
            label: format!("Trust parent folder ({})", parent.display()),
            trusted: true,
            updates: vec![(parent, Some(true)), (trust_path.clone(), None)],
            session_only: false,
        });
    }
    if include_session_only {
        options.push(ProjectTrustOption {
            label: "Trust (this session only)".to_string(),
            trusted: true,
            updates: Vec::new(),
            session_only: true,
        });
    }
    options.push(ProjectTrustOption {
        label: "Do not trust".to_string(),
        trusted: false,
        updates: vec![(trust_path, Some(false))],
        session_only: false,
    });
    if include_session_only {
        options.push(ProjectTrustOption {
            label: "Do not trust (this session only)".to_string(),
            trusted: false,
            updates: Vec::new(),
            session_only: true,
        });
    }
    options
}

/// Whether `cwd` has project-local resources that must be gated by project
/// trust: trust-requiring entries under `cwd/.pi`, or `.agents/skills` in
/// `cwd` or one of its ancestors. The user-global `~/.agents/skills` directory
/// is always treated as a trusted user resource and ignored (upstream
/// `hasTrustRequiringProjectResources`).
pub fn has_trust_requiring_project_resources(cwd: &Path) -> bool {
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("~"));
    let user_agents_skills_dir = normalize_cwd(&home.join(".agents").join("skills"));
    let mut current = normalize_cwd(cwd);

    let config_dir = current.join(".pi");
    if TRUST_REQUIRING_PROJECT_CONFIG_RESOURCES
        .iter()
        .any(|entry| config_dir.join(entry).exists())
    {
        return true;
    }

    loop {
        // Canonicalize only an existing candidate: most ancestors have no
        // `.agents/skills`, and canonicalizing a missing path is a wasted
        // filesystem call on every startup.
        let agents_skills_dir = current.join(".agents").join("skills");
        if agents_skills_dir.exists() && normalize_cwd(&agents_skills_dir) != user_agents_skills_dir
        {
            return true;
        }
        let Some(parent) = current.parent() else {
            return false;
        };
        if parent == current {
            return false;
        }
        current = parent.to_path_buf();
    }
}

/// The result of [`resolve_project_trusted`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProjectTrustResolution {
    /// The decision is known; persist nothing.
    Resolved(bool),
    /// The `ask` default with a UI available: show the trust selector
    /// (upstream trust-manager.ts:67-97) and apply the chosen option.
    Ask,
}

/// Resolve whether `cwd` is trusted (upstream `resolveProjectTrusted`):
/// explicit override first, then the resource check (a project with no
/// trust-requiring resources is implicitly trusted), the store's
/// nearest-ancestor decision, and finally the `defaultProjectTrust` setting —
/// `ask` becomes a selector prompt when a UI is available and "not trusted"
/// in headless mode.
pub fn resolve_project_trusted(
    cwd: &Path,
    store: &ProjectTrustStore,
    trust_override: Option<bool>,
    default_project_trust: DefaultProjectTrust,
    has_ui: bool,
) -> ProjectTrustResolution {
    if let Some(over) = trust_override {
        return ProjectTrustResolution::Resolved(over);
    }
    if !has_trust_requiring_project_resources(cwd) {
        return ProjectTrustResolution::Resolved(true);
    }
    if let Some(entry) = store.get(cwd) {
        return ProjectTrustResolution::Resolved(entry.decision);
    }
    match default_project_trust {
        DefaultProjectTrust::Always => ProjectTrustResolution::Resolved(true),
        DefaultProjectTrust::Never => ProjectTrustResolution::Resolved(false),
        DefaultProjectTrust::Ask => {
            if has_ui {
                ProjectTrustResolution::Ask
            } else {
                ProjectTrustResolution::Resolved(false)
            }
        }
    }
}

/// Apply one chosen selector option: persist its updates (upstream
/// `saveProjectTrustPromptResult`). The session-only options carry no
/// updates, so they leave the store untouched (the decision lives in the
/// returned `trusted` flag for this run only).
pub fn save_project_trust_option(
    store: &ProjectTrustStore,
    option: &ProjectTrustOption,
) -> Result<(), String> {
    if option.updates.is_empty() {
        return Ok(());
    }
    store.set_many(option.updates.iter().cloned())
}

/// One run's resolved project-trust decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RunTrustResolution {
    /// Whether the project scope may be read/written for this run.
    pub trusted: bool,
    /// Whether the interactive selector was offered. It always runs BEFORE
    /// the caller goes on to execute the command (upstream resolves trust in
    /// `createRuntime`, before the mode host starts).
    pub prompted: bool,
}

/// Resolve one run's project trust with the caller's prompt, keeping the
/// upstream ordering (`main.ts:640-652,726,773`): the decision is resolved
/// BEFORE the command runs, and a metadata command passes `has_ui = false` —
/// upstream's `trustPromptMode` — so the selector can never sit between the
/// user and `--list-models`/`--export` output. `prompt` receives the selector
/// options and returns the chosen index (`None` = cancelled, which resolves
/// to "not trusted" without persisting, like upstream's cancel path).
///
/// The option list includes the "this session only" rows (upstream
/// `resolveProjectTrusted` -> `getProjectTrustOptions(..., {
/// includeSessionOnly: true })`): choosing one applies the decision to this
/// run and writes nothing to `trust.json`.
pub fn resolve_run_trust<F>(
    cwd: &Path,
    store: &ProjectTrustStore,
    trust_override: Option<bool>,
    default_project_trust: DefaultProjectTrust,
    has_ui: bool,
    prompt: F,
) -> RunTrustResolution
where
    F: FnOnce(&[ProjectTrustOption]) -> Option<usize>,
{
    match resolve_project_trusted(cwd, store, trust_override, default_project_trust, has_ui) {
        ProjectTrustResolution::Resolved(trusted) => RunTrustResolution {
            trusted,
            prompted: false,
        },
        ProjectTrustResolution::Ask => {
            let options = get_project_trust_options_including_session_only(cwd);
            let Some(index) = prompt(&options) else {
                return RunTrustResolution {
                    trusted: false,
                    prompted: true,
                };
            };
            let Some(option) = options.get(index) else {
                return RunTrustResolution {
                    trusted: false,
                    prompted: true,
                };
            };
            let _ = save_project_trust_option(store, option);
            RunTrustResolution {
                trusted: option.trusted,
                prompted: true,
            }
        }
    }
}

/// The headless trust resolution used by hosts that only need the boolean
/// (provider loading, print/RPC, shell preferences, MCP config discovery):
/// the run's pinned decision first, then the headless order
/// (`PIR_TRUST_PROJECT=1` operator override, recorded `project_trust`
/// decision), then [`resolve_project_trusted`] with no UI (an `ask` default
/// resolves to "not trusted").
///
/// The pinned decision is the single source for consumers that run AFTER the
/// startup gate: [`crate::extensions::project_trust::pin_decision`] already
/// folded in the explicit `--approve`/`--no-approve` and the environment
/// override, so re-applying `PIR_TRUST_PROJECT=1` here would undo a
/// `--no-approve`. Only an unpinned call (a library/headless entry that never
/// ran the startup gate) still applies the environment override as its
/// initial resolution input.
pub fn resolve_cwd_trust(cwd: &Path, agent_dir: &Path) -> bool {
    if let Some(trusted) = crate::extensions::project_trust::resolved_decision(cwd) {
        return trusted;
    }
    // A pinned decision always carries a recorded boolean; treating a missing
    // value as "not trusted" keeps a partially initialized pin from falling
    // through to the store.
    if crate::extensions::project_trust::is_decision_pinned(cwd) {
        return false;
    }
    // The `defaultProjectTrust` policy is global-only, so an untrusted probe
    // load of the global scope is enough to read it.
    let default_project_trust =
        crate::config::settings::SettingsManager::load(agent_dir, &cwd.join(".pi"), cwd, false)
            .default_project_trust();
    let store = ProjectTrustStore::new(agent_dir);
    matches!(
        resolve_project_trusted(cwd, &store, None, default_project_trust, false),
        ProjectTrustResolution::Resolved(true)
    )
}
