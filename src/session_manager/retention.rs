//! Session retention / garbage collection (P0-4).
//!
//! The session store is append-only and currently grows without bound: only
//! the manual single-session delete and the crash-log cap ever remove files.
//! This module adds a conservative, testable retention pass:
//!
//! - `maxAgeDays`: delete sessions whose mtime is older than the bound;
//! - `maxSessions`: keep at most N sessions per directory (newest first);
//! - `maxBytes`: keep the newest sessions until the byte budget is reached.
//!
//! The ACTIVE session is always retained, even when every bound is exceeded.
//! Files are only removed after proving they are real pi session files (a
//! valid header), and known sidecars (checkpoint directories, temp files,
//! lock files) are removed together with their transcript. The policy is
//! opt-in (`enabled: false` by default); the explicit `pir sessions prune`
//! command can apply it without persisting the opt-in.

use std::path::{Component, Path, PathBuf};
use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};

/// Retention policy for persisted sessions. `enabled` only gates automatic
/// startup pruning; `pir sessions prune` can run a one-shot pass regardless.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct SessionRetentionPolicy {
    pub enabled: bool,
    /// Delete sessions older than this many days.
    pub max_age_days: Option<u64>,
    /// Keep at most this many sessions per directory (the active session does
    /// not consume a deletable slot; it is always retained).
    pub max_sessions: Option<usize>,
    /// Keep the newest sessions until this many bytes are retained.
    pub max_bytes: Option<u64>,
}

impl Default for SessionRetentionPolicy {
    fn default() -> Self {
        Self {
            enabled: false,
            max_age_days: Some(30),
            max_sessions: Some(500),
            max_bytes: Some(2 * 1024 * 1024 * 1024),
        }
    }
}

impl SessionRetentionPolicy {
    /// A pass with no bounds configured is a no-op.
    pub fn is_noop(&self) -> bool {
        self.max_age_days.is_none() && self.max_sessions.is_none() && self.max_bytes.is_none()
    }
}

/// One candidate session file: a regular `*.jsonl` whose first parseable line
/// is a valid pi session header. Junk `.jsonl` files do not consume retention
/// quota (they are reported as diagnostics instead).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionFileInfo {
    pub path: PathBuf,
    pub modified: SystemTime,
    pub bytes: u64,
}

/// A writer lock whose OWNER IS PROVABLY DEAD must be at least this old before
/// it is reclaimed. Liveness is decided by pid (see
/// [`session_lock_snapshot`]), never by this age alone: a live process that
/// idled for more than an hour still protects its transcript.
pub(crate) const SESSION_LOCK_STALE_AFTER: Duration = Duration::from_secs(60 * 60);

/// A session file modified less than this ago is treated as actively written
/// even when no lock exists (older writers, lock-acquisition failures).
pub(crate) const ACTIVE_WRITE_SAFETY_WINDOW: Duration = Duration::from_secs(60 * 60);

/// Sidecar lock path for a session transcript: `.<file-name>.jsonl.lock`.
pub(crate) fn session_lock_path(session_file: &Path) -> Option<PathBuf> {
    let name = session_file.file_name()?.to_str()?;
    let parent = session_file.parent().unwrap_or_else(|| Path::new("."));
    Some(parent.join(format!(".{name}.lock")))
}

/// Claim path for a session ID whose transcript file may not exist yet.
///
/// Transcript names embed a creation timestamp, so two processes that both
/// see "no file for this id" can pick two different names and each create a
/// file for the SAME session id. This lock is keyed on the id alone: whoever
/// creates it wins the right to materialize the first transcript; the loser
/// must fail instead of creating a second same-id file. Validation mirrors
/// [`assert_valid_session_id`](super::assert_valid_session_id) so the id
/// cannot escape the session directory.
pub(crate) fn session_id_lock_path(session_dir: &Path, session_id: &str) -> Option<PathBuf> {
    let valid = !session_id.is_empty()
        && session_id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))
        && session_id
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphanumeric())
        && session_id
            .chars()
            .next_back()
            .is_some_and(|c| c.is_ascii_alphanumeric());
    if !valid {
        return None;
    }
    Some(session_dir.join(format!(".session-{session_id}.lock")))
}

/// The pid recorded in a lock file, used only for the "session already in
/// use" diagnostic. A lock without a parseable owner yields `None`.
pub(crate) fn session_lock_owner_pid(lock: &Path) -> Option<u32> {
    std::fs::read_to_string(lock)
        .ok()
        .and_then(|content| parse_lock_owner(&content))
        .map(|owner| owner.pid)
}

/// This machine's name, recorded in lock files so a lock written by a process
/// on another host is never probed via the local pid table (a foreign pid can
/// collide with an unrelated local process, or look dead). `None` when the
/// environment does not expose a stable name; callers then fall back to the
/// legacy same-host assumption.
pub(crate) fn current_host_id() -> Option<String> {
    let env_name = if cfg!(windows) {
        std::env::var("COMPUTERNAME").ok()
    } else {
        std::env::var("HOSTNAME").ok()
    };
    let name = env_name
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
        .or_else(|| {
            #[cfg(unix)]
            {
                std::fs::read_to_string("/proc/sys/kernel/hostname")
                    .ok()
                    .or_else(|| std::fs::read_to_string("/etc/hostname").ok())
                    .map(|value| value.trim().to_owned())
                    .filter(|value| !value.is_empty())
            }
            #[cfg(not(unix))]
            {
                None
            }
        });
    name
}
/// The owner identity recorded in a writer lock. `host`/`start_token` are
/// optional so locks from older builds still parse; their absence only skips
/// the extra safeguards, never the pid probe.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SessionLockOwner {
    pub pid: u32,
    pub host: Option<String>,
    pub start_token: Option<u64>,
}

/// Result of inspecting a writer lock.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SessionLockState {
    /// No lock file (or it vanished while being inspected).
    Missing,
    /// A writer may still append: the pid is alive, the lock came from a
    /// different host, its mtime is young, or the owner cannot be determined.
    Live,
    /// The pid is provably dead on this host and the lock is older than
    /// [`SESSION_LOCK_STALE_AFTER`]. Safe to reclaim.
    Stale,
}

/// A lock inspection plus the `(len, mtime)` fingerprint used to make sure the
/// file did not change between the stale decision and its removal.
#[derive(Debug, Clone, Copy)]
pub(crate) struct SessionLockSnapshot {
    pub state: SessionLockState,
    pub len: u64,
    pub modified: Option<SystemTime>,
}

impl SessionLockSnapshot {
    fn missing() -> Self {
        Self {
            state: SessionLockState::Missing,
            len: 0,
            modified: None,
        }
    }

    fn live(len: u64, modified: Option<SystemTime>) -> Self {
        Self {
            state: SessionLockState::Live,
            len,
            modified,
        }
    }
}

fn parse_lock_owner(content: &str) -> Option<SessionLockOwner> {
    let mut pid = None;
    let mut host = None;
    let mut start_token = None;
    for line in content.lines() {
        let Some((key, value)) = line.trim().split_once('=') else {
            continue;
        };
        match key.trim() {
            "pid" => pid = value.trim().parse::<u32>().ok(),
            "host" => {
                let value = value.trim();
                if !value.is_empty() {
                    host = Some(value.to_owned());
                }
            }
            "start" => start_token = value.trim().parse::<u64>().ok(),
            _ => {}
        }
    }
    Some(SessionLockOwner {
        pid: pid?,
        host,
        start_token,
    })
}

/// Inspect a writer lock. The decision deliberately errs toward `Live`:
/// unreadable/unparseable locks, foreign hosts, unknown platforms and future
/// mtimes all protect the transcript instead of exposing it to deletion.
pub(crate) fn session_lock_snapshot(lock: &Path) -> SessionLockSnapshot {
    // Distinguish "no lock" from "lock exists but cannot be inspected": the
    // latter must stay Live, never Missing (Missing would unblock deletion).
    let path_metadata = match std::fs::symlink_metadata(lock) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return SessionLockSnapshot::missing();
        }
        Err(_) => return SessionLockSnapshot::live(0, None),
    };
    if !path_metadata.is_file() {
        return SessionLockSnapshot::live(path_metadata.len(), path_metadata.modified().ok());
    }
    let Ok(file) = std::fs::File::open(lock) else {
        return SessionLockSnapshot::live(path_metadata.len(), path_metadata.modified().ok());
    };
    let metadata = file.metadata().unwrap_or(path_metadata);
    let modified = metadata.modified().ok();
    let mut content = String::new();
    if std::io::Read::read_to_string(&mut &file, &mut content).is_err() {
        return SessionLockSnapshot::live(metadata.len(), modified);
    }
    let Some(owner) = parse_lock_owner(&content) else {
        // No pid means we cannot prove the owner is dead: conservative live.
        return SessionLockSnapshot::live(metadata.len(), modified);
    };
    // A lock written on another machine cannot be probed through the local
    // pid table. Treat it as live until a local process can reclaim it.
    if let Some(lock_host) = &owner.host {
        match current_host_id() {
            Some(local) if &local == lock_host => {}
            _ => return SessionLockSnapshot::live(metadata.len(), modified),
        }
    }
    let alive = process_is_alive(owner.pid);
    let same_process = match (alive, owner.start_token, process_start_token(owner.pid)) {
        // A live pid whose start time differs is a REUSED pid: the lock's
        // owner is dead even though some unrelated process now has its pid.
        (true, Some(expected), Some(actual)) => expected == actual,
        (true, _, _) => true,
        (false, _, _) => false,
    };
    if same_process {
        return SessionLockSnapshot::live(metadata.len(), modified);
    }
    let old_enough = modified
        .and_then(|modified| SystemTime::now().duration_since(modified).ok())
        .is_some_and(|age| age >= SESSION_LOCK_STALE_AFTER);
    if old_enough {
        SessionLockSnapshot {
            state: SessionLockState::Stale,
            len: metadata.len(),
            modified,
        }
    } else {
        // Dead pid but a young lock: another process may be mid-acquisition
        // (or the filesystem clock is skewed). Leave it alone this pass.
        SessionLockSnapshot::live(metadata.len(), modified)
    }
}

/// Path of the short-lived claim file that serializes stale-lock reclamation
/// among writers. Kept next to (and named after) the lock so no process can
/// mistake it for a session.
fn session_lock_claim_path(lock: &Path) -> Option<PathBuf> {
    let name = lock.file_name()?.to_str()?;
    let parent = lock.parent().unwrap_or_else(|| Path::new("."));
    Some(parent.join(format!("{name}.reclaim")))
}

fn acquire_session_lock_claim(claim: &Path) -> bool {
    if let Some(parent) = claim.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(claim)
    {
        Ok(mut file) => {
            use std::io::Write as _;
            let _ = writeln!(file, "pid={}", std::process::id());
            true
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            // A crashed reclaimer must not block reclamation forever.
            let stale_claim = std::fs::symlink_metadata(claim)
                .ok()
                .and_then(|metadata| metadata.modified().ok())
                .and_then(|modified| SystemTime::now().duration_since(modified).ok())
                .is_some_and(|age| age >= SESSION_LOCK_STALE_AFTER);
            if !stale_claim {
                return false;
            }
            let _ = std::fs::remove_file(claim);
            std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(claim)
                .is_ok()
        }
        Err(_) => false,
    }
}

/// Reclaim a lock that [`session_lock_snapshot`] proved stale. Writers
/// serialize through a `create_new` claim file, then re-inspect the lock and
/// compare its `(len, mtime)` fingerprint before removing it, so a fresh lock
/// created in the meantime is never blindly deleted.
pub(crate) fn reclaim_stale_session_lock(lock: &Path) -> bool {
    let Some(claim) = session_lock_claim_path(lock) else {
        return false;
    };
    if !acquire_session_lock_claim(&claim) {
        return false;
    }
    let first = session_lock_snapshot(lock);
    let reclaimed = if first.state == SessionLockState::Stale {
        let second = session_lock_snapshot(lock);
        if second.state == SessionLockState::Stale
            && second.len == first.len
            && second.modified == first.modified
        {
            match std::fs::remove_file(lock) {
                Ok(()) => true,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => true,
                Err(_) => false,
            }
        } else {
            false
        }
    } else {
        false
    };
    let _ = std::fs::remove_file(&claim);
    reclaimed
}

#[cfg(unix)]
pub(crate) fn process_is_alive(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    // SAFETY: `kill(pid, 0)` only probes existence/permission; it sends no
    // signal. Mirrors `jobs::process_is_alive`.
    #[allow(unsafe_code)]
    let result = unsafe { libc::kill(pid as i32, 0) };
    if result == 0 {
        return true;
    }
    std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

#[cfg(windows)]
pub(crate) fn process_is_alive(pid: u32) -> bool {
    // SAFETY: the pid is a plain identifier; the handle is closed below.
    #![allow(unsafe_code)]
    use windows_sys::Win32::Foundation::{CloseHandle, ERROR_ACCESS_DENIED, WAIT_OBJECT_0};
    use windows_sys::Win32::Storage::FileSystem::SYNCHRONIZE;
    use windows_sys::Win32::System::Threading::{
        OpenProcess, WaitForSingleObject, PROCESS_QUERY_LIMITED_INFORMATION,
    };

    // SAFETY: plain pid; handle ownership is checked below.
    let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION | SYNCHRONIZE, 0, pid) };
    if handle == 0 {
        return std::io::Error::last_os_error().raw_os_error() == Some(ERROR_ACCESS_DENIED as i32);
    }
    // SAFETY: the handle was just opened and is owned here.
    let state = unsafe { WaitForSingleObject(handle, 0) };
    // SAFETY: closing the handle opened above.
    unsafe {
        CloseHandle(handle);
    }
    state != WAIT_OBJECT_0
}

/// Unknown platforms cannot prove a pid dead; keep the lock live.
#[cfg(not(any(unix, windows)))]
pub(crate) fn process_is_alive(_pid: u32) -> bool {
    true
}

/// Platform start token used to detect pid reuse (same source as
/// `jobs::process_start_token`). `None` where unavailable.
#[cfg(unix)]
pub(crate) fn process_start_token(pid: u32) -> Option<u64> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let rest = &stat[stat.rfind(')')? + 1..];
    rest.split_whitespace().nth(19)?.parse().ok()
}

#[cfg(windows)]
pub(crate) fn process_start_token(pid: u32) -> Option<u64> {
    #![allow(unsafe_code)]
    use windows_sys::Win32::Foundation::{CloseHandle, FILETIME};
    use windows_sys::Win32::System::Threading::{
        GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    };

    // SAFETY: plain pid; the returned handle is checked and closed below.
    let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if handle == 0 {
        return None;
    }
    let mut creation = FILETIME {
        dwLowDateTime: 0,
        dwHighDateTime: 0,
    };
    let mut exit = creation;
    let mut kernel = creation;
    let mut user = creation;
    // SAFETY: handle is live and the FILETIME locals are out-parameters.
    let ok = unsafe { GetProcessTimes(handle, &mut creation, &mut exit, &mut kernel, &mut user) };
    // SAFETY: closing the handle opened above.
    unsafe {
        CloseHandle(handle);
    }
    if ok == 0 {
        return None;
    }
    Some(((creation.dwHighDateTime as u64) << 32) | creation.dwLowDateTime as u64)
}

#[cfg(not(any(unix, windows)))]
pub(crate) fn process_start_token(_pid: u32) -> Option<u64> {
    None
}

/// Why a session is scheduled for deletion.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PruneReason {
    Expired,
    OverMaxSessions,
    OverMaxBytes,
}

impl PruneReason {
    pub fn as_str(self) -> &'static str {
        match self {
            PruneReason::Expired => "expired",
            PruneReason::OverMaxSessions => "over-max-sessions",
            PruneReason::OverMaxBytes => "over-max-bytes",
        }
    }
}

/// One planned deletion.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlannedPrune {
    pub path: PathBuf,
    pub bytes: u64,
    pub reason: PruneReason,
    /// File mtime at scan time (re-checked before deletion).
    pub modified: Option<SystemTime>,
}

/// Deterministic retention decision before any filesystem mutation.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PrunePlan {
    pub remove: Vec<PlannedPrune>,
    pub kept: usize,
}

/// Result of an executed prune pass.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PruneReport {
    pub removed: Vec<PlannedPrune>,
    pub kept: usize,
    pub bytes_freed: u64,
    /// Files skipped because a writer lock or the safety window marked them
    /// as actively written (cross-process protection).
    #[serde(default)]
    pub skipped_active: usize,
    /// Per-file failures (unreadable/non-session files are never deleted).
    pub errors: Vec<String>,
}

impl PruneReport {
    pub fn merge(&mut self, other: PruneReport) {
        self.removed.extend(other.removed);
        self.kept += other.kept;
        self.bytes_freed = self.bytes_freed.saturating_add(other.bytes_freed);
        self.skipped_active += other.skipped_active;
        self.errors.extend(other.errors);
    }
}

fn normalized(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

fn is_active(path: &Path, active: Option<&Path>) -> bool {
    active.is_some_and(|active| normalized(path) == normalized(active))
}

/// Scan one directory for valid session candidates. A `.jsonl` file only
/// counts when its first parseable line is a pi session header; junk files are
/// returned separately as diagnostics so they never consume `maxSessions` /
/// `maxBytes` quota. Symlinks are followed for metadata but non-files
/// (FIFOs/devices/dirs) are rejected.
pub fn scan_session_files(dir: &Path) -> Vec<SessionFileInfo> {
    scan_session_files_with_ignored(dir).0
}

/// [`scan_session_files`] plus one diagnostic line per ignored `.jsonl`.
pub fn scan_session_files_with_ignored(dir: &Path) -> (Vec<SessionFileInfo>, Vec<String>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return (Vec::new(), Vec::new());
    };
    let mut ignored = Vec::new();
    let mut files: Vec<SessionFileInfo> = entries
        .flatten()
        .filter_map(|entry| {
            let path = entry.path();
            if path.extension().and_then(|value| value.to_str()) != Some("jsonl") {
                return None;
            }
            let metadata = path.metadata().ok()?;
            if !metadata.is_file() {
                return None;
            }
            // A2-02: only real sessions participate in retention budgeting.
            if super::read_session_header_for_discovery(&path).is_none() {
                ignored.push(format!(
                    "ignoring non-session file (not counted by retention): {}",
                    path.display()
                ));
                return None;
            }
            Some(SessionFileInfo {
                path,
                modified: metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH),
                bytes: metadata.len(),
            })
        })
        .collect();
    files.sort_by(|a, b| {
        b.modified
            .cmp(&a.modified)
            .then_with(|| a.path.cmp(&b.path))
    });
    (files, ignored)
}

/// Compute a retention plan. Pure: no filesystem mutation, so every boundary
/// (age, count, bytes, active protection) is directly testable.
pub fn plan_session_prune(
    mut files: Vec<SessionFileInfo>,
    policy: &SessionRetentionPolicy,
    now: SystemTime,
    active: Option<&Path>,
) -> PrunePlan {
    // Newest first; ties are path-stable so the plan is deterministic.
    files.sort_by(|a, b| {
        b.modified
            .cmp(&a.modified)
            .then_with(|| a.path.cmp(&b.path))
    });

    let active_present = files.iter().any(|file| is_active(&file.path, active));
    let mut remove: Vec<PlannedPrune> = Vec::new();
    let mut kept: Vec<&SessionFileInfo> = Vec::new();

    // 1. Age. The active session is exempt.
    for file in &files {
        if is_active(&file.path, active) {
            kept.push(file);
            continue;
        }
        let expired = policy.max_age_days.is_some_and(|days| {
            now.duration_since(file.modified).unwrap_or(Duration::ZERO)
                > Duration::from_secs(days.saturating_mul(86_400))
        });
        if expired {
            remove.push(PlannedPrune {
                path: file.path.clone(),
                bytes: file.bytes,
                reason: PruneReason::Expired,
                modified: Some(file.modified),
            });
        } else {
            kept.push(file);
        }
    }

    // 2. Count. `kept` is newest-first; the active session reserves its slot
    // even when it sorts beyond `max_sessions`.
    if let Some(max_sessions) = policy.max_sessions {
        let mut count = usize::from(active_present);
        let mut survivors: Vec<&SessionFileInfo> = Vec::with_capacity(kept.len());
        for file in kept {
            if is_active(&file.path, active) {
                survivors.push(file);
                continue;
            }
            if count >= max_sessions {
                remove.push(PlannedPrune {
                    path: file.path.clone(),
                    bytes: file.bytes,
                    reason: PruneReason::OverMaxSessions,
                    modified: Some(file.modified),
                });
            } else {
                count += 1;
                survivors.push(file);
            }
        }
        kept = survivors;
    }

    // 3. Bytes. Newest-first accumulation; the active session is retained and
    // its bytes count against the budget.
    if let Some(max_bytes) = policy.max_bytes {
        let mut bytes = kept
            .iter()
            .filter(|file| is_active(&file.path, active))
            .map(|file| file.bytes)
            .sum::<u64>();
        let mut survivors: Vec<&SessionFileInfo> = Vec::with_capacity(kept.len());
        for file in kept {
            if is_active(&file.path, active) {
                survivors.push(file);
                continue;
            }
            if bytes.saturating_add(file.bytes) > max_bytes {
                remove.push(PlannedPrune {
                    path: file.path.clone(),
                    bytes: file.bytes,
                    reason: PruneReason::OverMaxBytes,
                    modified: Some(file.modified),
                });
            } else {
                bytes = bytes.saturating_add(file.bytes);
                survivors.push(file);
            }
        }
        kept = survivors;
    }

    PrunePlan {
        remove,
        kept: kept.len(),
    }
}

/// Known sidecars stored next to a session transcript. Keep this list in one
/// place so retention and the checkpoint store cannot drift.
pub fn session_sidecar_paths(path: &Path) -> Vec<PathBuf> {
    let Some(name) = path.file_name().and_then(|value| value.to_str()) else {
        return Vec::new();
    };
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let mut paths = vec![
        parent.join(format!("{name}.checkpoints")),
        parent.join(format!("{name}.tmp")),
        parent.join(format!("{name}.bak")),
    ];
    if let Some(lock) = session_lock_path(path) {
        paths.push(lock);
    }
    paths
}

fn remove_path(path: &Path) -> std::io::Result<()> {
    let metadata = std::fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() || metadata.is_file() {
        std::fs::remove_file(path)
    } else if metadata.is_dir() {
        std::fs::remove_dir_all(path)
    } else {
        std::fs::remove_file(path)
    }
}

/// Execute a prune pass over one session directory.
pub fn prune_session_dir(
    dir: &Path,
    policy: &SessionRetentionPolicy,
    active: Option<&Path>,
) -> PruneReport {
    let (files, ignored) = scan_session_files_with_ignored(dir);
    let plan = plan_session_prune(files, policy, SystemTime::now(), active);
    let mut report = execute_plan(plan, active);
    report.errors.extend(ignored);
    report
}

fn execute_plan(plan: PrunePlan, active: Option<&Path>) -> PruneReport {
    let mut report = PruneReport {
        kept: plan.kept,
        ..PruneReport::default()
    };
    for entry in plan.remove {
        if is_active(&entry.path, active) {
            // Defense in depth: the planner already protects it.
            report.kept += 1;
            continue;
        }
        // Never delete a random `.jsonl`: prove the header parses.
        if super::read_session_header_for_discovery(&entry.path).is_none() {
            report.errors.push(format!(
                "refusing to prune non-session file: {}",
                entry.path.display()
            ));
            continue;
        }
        // B-02: a live writer lock means another process has the session
        // open. Liveness is pid-based, so an idle-but-alive writer (even with
        // a lock mtime older than an hour) is never mistaken for a crash.
        let lock_path = session_lock_path(&entry.path);
        if lock_path
            .as_deref()
            .is_some_and(|lock| session_lock_snapshot(lock).state == SessionLockState::Live)
        {
            report.skipped_active += 1;
            report.kept += 1;
            continue;
        }
        // Second stat closes the TOCTOU gap between scan and unlink: a file
        // that grew/shifted mtime since the scan, or one modified inside the
        // safety window, may still have an active writer without a lock.
        let current = std::fs::symlink_metadata(&entry.path);
        let metadata = match current {
            Ok(metadata) if metadata.is_file() => metadata,
            _ => {
                report.kept += 1;
                continue;
            }
        };
        let recently_written = metadata
            .modified()
            .ok()
            .and_then(|modified| SystemTime::now().duration_since(modified).ok())
            .is_some_and(|age| age < ACTIVE_WRITE_SAFETY_WINDOW);
        let changed_since_scan = metadata.len() != entry.bytes
            || entry
                .modified
                .is_some_and(|scan_modified| metadata.modified().ok() != Some(scan_modified));
        if recently_written || changed_since_scan {
            report.skipped_active += 1;
            report.kept += 1;
            continue;
        }
        // Final live-lock re-check immediately before the unlink. A writer
        // that opened the session after the checks above (including the
        // lock-free window between scan and this point) has created its lock
        // by now, so the prune observes it and skips. Residual window: a
        // writer that creates its lock after this snapshot but before
        // `remove_file` below can still lose the race (no atomic
        // unlink-if-lock-absent primitive exists here); it is reduced to a
        // few syscalls and relies on the per-append lock touch plus the 1h
        // session-mtime safety window.
        if lock_path
            .as_deref()
            .is_some_and(|lock| session_lock_snapshot(lock).state == SessionLockState::Live)
        {
            report.skipped_active += 1;
            report.kept += 1;
            continue;
        }
        match std::fs::remove_file(&entry.path) {
            Ok(()) => {
                report.bytes_freed = report.bytes_freed.saturating_add(entry.bytes);
                for sidecar in session_sidecar_paths(&entry.path) {
                    // Never delete a lock a writer acquired while the
                    // transcript was being removed: a third process could
                    // then believe the (now missing) transcript is unheld.
                    if lock_path.as_deref() == Some(sidecar.as_path())
                        && matches!(
                            session_lock_snapshot(&sidecar).state,
                            SessionLockState::Live
                        )
                    {
                        report.errors.push(format!(
                            "removed {} but kept a live writer lock: {}",
                            entry.path.display(),
                            sidecar.display()
                        ));
                        continue;
                    }
                    if sidecar.exists() {
                        if let Err(error) = remove_path(&sidecar) {
                            report.errors.push(format!(
                                "removed {} but could not remove sidecar {}: {error}",
                                entry.path.display(),
                                sidecar.display()
                            ));
                        }
                    }
                }
                report.removed.push(entry);
            }
            Err(error) => report.errors.push(format!(
                "failed to remove {}: {error}",
                entry.path.display()
            )),
        }
    }
    report
}

/// Recursively prune a sessions ROOT (`<agent-dir>/sessions`) or a single
/// per-cwd directory. Only directories containing `*.jsonl` files are
/// visited; symlinked directories are not followed.
pub fn prune_sessions_root(
    root: &Path,
    policy: &SessionRetentionPolicy,
    active: Option<&Path>,
) -> PruneReport {
    let mut report = PruneReport::default();
    prune_dir_recursive(root, policy, active, &mut report, 0);
    report
}

/// Resolve the recursion root for a `--all` prune.
///
/// `--all` means "every project directory under the sessions root", and the
/// only root this process can vouch for is `<agent-dir>/sessions`
/// ([`super::sessions_dir`]). The configured session directory can come from
/// `--session-dir`, an environment override, or settings and may point at
/// any directory, so its PARENT must never be used as a recursion root:
/// that would delete unrelated `*.jsonl` files next to a custom directory.
///
/// Returns `sessions_root` when `session_dir` is that root or lies below it;
/// otherwise returns a message telling the caller to use a directory-level
/// prune instead. Pure path logic over already-resolved inputs, so the
/// decision is testable without touching the process environment.
pub fn resolve_all_prune_root(
    session_dir: &Path,
    sessions_root: &Path,
) -> std::result::Result<PathBuf, String> {
    let session_dir = lexical_normalize(session_dir);
    let sessions_root = lexical_normalize(sessions_root);
    if path_within(&session_dir, &sessions_root) {
        Ok(sessions_root)
    } else {
        Err(format!(
            "refusing `sessions prune --all`: {} is outside the sessions root {}; \
             run a directory-level prune (`pir sessions prune` without `--all`) instead",
            session_dir.display(),
            sessions_root.display()
        ))
    }
}

/// Resolve `.` / `..` components lexically (no filesystem access).
fn lexical_normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if matches!(out.components().next_back(), Some(Component::Normal(_))) {
                    out.pop();
                } else if out.as_os_str().is_empty() {
                    out.push(component.as_os_str());
                }
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Component-wise prefix test (case-insensitive on Windows, where the
/// filesystem is).
fn path_within(candidate: &Path, root: &Path) -> bool {
    let key = |component: Component<'_>| {
        let text = component.as_os_str().to_string_lossy().into_owned();
        if cfg!(windows) {
            text.to_lowercase()
        } else {
            text
        }
    };
    let root_parts: Vec<String> = root.components().map(key).collect();
    if root_parts.is_empty() {
        return false;
    }
    let candidate_parts: Vec<String> = candidate.components().map(key).collect();
    candidate_parts.len() >= root_parts.len()
        && root_parts
            .iter()
            .zip(candidate_parts.iter())
            .all(|(root, candidate)| root == candidate)
}

fn prune_dir_recursive(
    dir: &Path,
    policy: &SessionRetentionPolicy,
    active: Option<&Path>,
    report: &mut PruneReport,
    depth: usize,
) {
    if depth > 8 {
        report.errors.push(format!(
            "session tree deeper than 8 levels at {}; stopped",
            dir.display()
        ));
        return;
    }
    report.merge(prune_session_dir(dir, policy, active));
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(metadata) = std::fs::symlink_metadata(&path) else {
            continue;
        };
        if metadata.is_dir() && !metadata.file_type().is_symlink() {
            prune_dir_recursive(&path, policy, active, report, depth + 1);
        }
    }
}

/// Human-readable one-line summary used by the CLI.
pub fn format_prune_report(report: &PruneReport) -> String {
    let mut out = format!(
        "pruned {} session(s), kept {}, skipped {} active, freed {} bytes",
        report.removed.len(),
        report.kept,
        report.skipped_active,
        report.bytes_freed
    );
    for entry in &report.removed {
        out.push_str(&format!(
            "\n  removed {} ({}, {} bytes)",
            entry.path.display(),
            entry.reason.as_str(),
            entry.bytes
        ));
    }
    for error in &report.errors {
        out.push_str(&format!("\n  warning: {error}"));
    }
    out
}
