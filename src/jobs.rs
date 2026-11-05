//! Persistent background jobs (P0-2).
//!
//! `BackgroundRegistry` is process-local: a `background: true` tool call is a
//! Tokio task owned by the session, so a TUI/agent exit aborts it. This module
//! is the cross-process alternative:
//!
//! ```text
//! <agent-dir>/jobs/<job-id>/
//!   meta.json     command, cwd, status, pid, exit code, timestamps
//!   output.log    combined stdout/stderr (supervisor writes it)
//! ```
//!
//! [`JobStore::spawn`] starts a detached supervisor (`pir __jobs-supervise`)
//! with stdin null and stdout/stderr captured. The supervisor survives the
//! parent process (Windows `DETACHED_PROCESS`, Unix a new session unless the
//! caller's OS would deliver SIGHUP — see the residual notes in the P0-2
//! report) and writes `meta.json` when the child exits. The log is capped at
//! [`MAX_LOG_BYTES`]; readers also cap at [`MAX_LOG_LINES`].
//!
//! No external dependencies: the store is plain files, and the CLI/`jobs`
//! surface is [`crate::coding_agent::cli::JobsCommand`].

use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};

/// Hard cap on one job's `output.log`.
pub const MAX_LOG_BYTES: u64 = 256 * 1024 * 1024;
/// Default lines returned by [`JobStore::read_log`].
pub const MAX_LOG_LINES: usize = 4096;
/// Default bytes returned by [`JobStore::read_log`].
pub const DEFAULT_READ_BYTES: u64 = 256 * 1024;
/// Terminal jobs older than this are removed by `jobs clean` by default.
pub const DEFAULT_CLEAN_AGE_DAYS: u64 = 7;

static JOB_SEQ: AtomicU64 = AtomicU64::new(0);
static META_TMP_SEQ: AtomicU64 = AtomicU64::new(0);

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

/// Validate a caller-supplied job id before it is joined to the store root.
/// Only one plain path component of `[A-Za-z0-9._-]` is accepted, which also
/// rejects separators, absolute paths, drive/UNC prefixes, `.` and `..`.
fn validate_job_id(id: &str) -> Result<(), String> {
    let invalid = || format!("invalid job id: {id:?}");
    if id.is_empty() || id == "." || id == ".." {
        return Err(invalid());
    }
    if !id
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
    {
        return Err(invalid());
    }
    let mut components = Path::new(id).components();
    match (components.next(), components.next()) {
        (Some(std::path::Component::Normal(component)), None)
            if component == std::ffi::OsStr::new(id) =>
        {
            Ok(())
        }
        _ => Err(invalid()),
    }
}

/// Lifecycle state of one job.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum JobStatus {
    Starting,
    Running,
    Exited,
    Failed,
    Killed,
    Lost,
}

impl JobStatus {
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            JobStatus::Exited | JobStatus::Failed | JobStatus::Killed | JobStatus::Lost
        )
    }

    pub fn as_str(self) -> &'static str {
        match self {
            JobStatus::Starting => "starting",
            JobStatus::Running => "running",
            JobStatus::Exited => "exited",
            JobStatus::Failed => "failed",
            JobStatus::Killed => "killed",
            JobStatus::Lost => "lost",
        }
    }
}

/// What to run. The supervisor executes `program` with `args` + `command`
/// exactly as the shell tool would (`bash -c <command>` etc.).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JobSpec {
    pub command: String,
    pub cwd: PathBuf,
    pub program: String,
    pub args: Vec<String>,
    /// Additional environment variables for the child (the supervisor also
    /// inherits the parent environment).
    pub env: Vec<(String, String)>,
}

/// Persisted record. `meta.json` is rewritten atomically on every transition.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JobMeta {
    pub version: u32,
    pub id: String,
    pub spec: JobSpec,
    pub status: JobStatus,
    /// Supervisor process id (the process that waits and records the exit).
    pub supervisor_pid: Option<u32>,
    /// Platform process start token for `supervisor_pid` (see the private
    /// `process_start_token` helper). Guards against a recycled pid being
    /// mistaken for the original supervisor. `None` on legacy records, which
    /// fall back to a pid-existence probe.
    #[serde(default)]
    pub supervisor_start_token: Option<u64>,
    /// Child process id (the actual command shell).
    pub pid: Option<u32>,
    /// Platform process start token for `pid`; same semantics as
    /// `supervisor_start_token`.
    #[serde(default)]
    pub child_start_token: Option<u64>,
    pub created_at_ms: u64,
    pub started_at_ms: Option<u64>,
    pub ended_at_ms: Option<u64>,
    pub exit_code: Option<i32>,
    pub error: Option<String>,
    pub log_bytes: u64,
    pub log_truncated: bool,
    pub reaped_at_ms: Option<u64>,
}

impl JobMeta {
    pub fn status_line(&self) -> String {
        match self.exit_code {
            Some(code) => format!("{} (exit {code})", self.status.as_str()),
            None => self.status.as_str().to_owned(),
        }
    }
}

/// Result of reading a job log.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct JobLog {
    pub text: String,
    pub bytes: u64,
    /// More bytes exist before the returned tail.
    pub truncated: bool,
}

/// Result of `reap`: the terminal record plus its output tail.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobReap {
    pub meta: JobMeta,
    pub log: JobLog,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CleanReport {
    pub removed: Vec<String>,
    pub errors: Vec<String>,
}

/// Rooted persistent job store. `new(agent_dir)` uses
/// `<agent-dir>/jobs`; tests can point [`Self::with_root`] at a temp dir and
/// inject a supervisor executable with [`Self::with_supervisor`].
#[derive(Debug, Clone)]
pub struct JobStore {
    root: PathBuf,
    supervisor_program: Option<PathBuf>,
}

impl JobStore {
    pub fn new(agent_dir: &Path) -> Self {
        Self {
            root: agent_dir.join("jobs"),
            supervisor_program: None,
        }
    }

    pub fn with_root(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            supervisor_program: None,
        }
    }

    /// Override the supervisor executable (the `pir` binary in production).
    pub fn with_supervisor(mut self, program: impl Into<PathBuf>) -> Self {
        self.supervisor_program = Some(program.into());
        self
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn job_dir(&self, id: &str) -> Result<PathBuf, String> {
        validate_job_id(id)?;
        let candidate = self.root.join(id);
        // Lexical validation already guarantees one normal component; when the
        // candidate exists, also resolve it so a symlink planted inside the
        // store cannot redirect a caller outside the root.
        if let (Ok(root), Ok(resolved)) = (self.root.canonicalize(), candidate.canonicalize()) {
            if !resolved.starts_with(&root) {
                return Err(format!("job id escapes the store root: {id:?}"));
            }
        }
        Ok(candidate)
    }

    /// Start a detached job. Returns after the supervisor process was
    /// successfully spawned; status transitions are written by it.
    pub fn spawn(&self, spec: &JobSpec) -> Result<JobMeta, String> {
        if spec.command.trim().is_empty() {
            return Err("job command must not be empty".to_owned());
        }
        let id = format!(
            "job-{}-{}-{}",
            now_secs(),
            std::process::id(),
            JOB_SEQ.fetch_add(1, Ordering::Relaxed)
        );
        let dir = self.job_dir(&id)?;
        std::fs::create_dir_all(&dir).map_err(|error| format!("create job dir: {error}"))?;
        let meta = JobMeta {
            version: 1,
            id: id.clone(),
            spec: spec.clone(),
            status: JobStatus::Starting,
            supervisor_pid: None,
            supervisor_start_token: None,
            pid: None,
            child_start_token: None,
            created_at_ms: now_ms(),
            started_at_ms: None,
            ended_at_ms: None,
            exit_code: None,
            error: None,
            log_bytes: 0,
            log_truncated: false,
            reaped_at_ms: None,
        };
        write_meta(&dir, &meta)?;

        let supervisor = self
            .supervisor_program
            .clone()
            .or_else(|| std::env::current_exe().ok())
            .ok_or_else(|| "cannot resolve the pir executable for the job supervisor".to_owned())?;
        let mut command = std::process::Command::new(&supervisor);
        command
            .arg("__jobs-supervise")
            .arg("--job-dir")
            .arg(&dir)
            .arg("--program")
            .arg(&spec.program);
        for arg in &spec.args {
            command.arg("--arg").arg(arg);
        }
        command.arg("--command").arg(&spec.command);
        command
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .current_dir(&spec.cwd);
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            // DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW:
            // the supervisor must outlive the TUI and must not flash a console.
            command.creation_flags(0x0000_0008 | 0x0000_0200 | 0x0800_0000);
        }
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            // Put the supervisor in its own process group so terminal signals
            // (Ctrl-C/SIGHUP) do not reach it and `kill` can target the whole
            // job tree with one negative-pid signal.
            command.process_group(0);
        }
        let child = command
            .spawn()
            .map_err(|error| format!("spawn job supervisor: {error}"))?;
        // The re-read in `record_supervisor_start` keeps a fast supervisor's
        // child-pid/terminal record instead of overwriting it with `Starting`.
        record_supervisor_start(&dir, child.id(), process_start_token(child.id()))
    }

    /// All jobs, newest first.
    pub fn list(&self) -> Vec<JobMeta> {
        let Ok(entries) = std::fs::read_dir(&self.root) else {
            return Vec::new();
        };
        let mut jobs: Vec<JobMeta> = entries
            .flatten()
            .filter_map(|entry| {
                let path = entry.path();
                if !path.is_dir() {
                    return None;
                }
                read_meta(&path)
            })
            .collect();
        jobs.sort_by_key(|job| std::cmp::Reverse(job.created_at_ms));
        jobs
    }

    /// Read one job's persisted record. Reconciles a crashed supervisor: a
    /// stale `running` record with no live supervisor/child becomes `lost`.
    pub fn status(&self, id: &str) -> Option<JobMeta> {
        let dir = self.job_dir(id).ok()?;
        let mut meta = read_meta(&dir)?;
        if meta.status.is_terminal() {
            return Some(meta);
        }
        let supervisor_alive =
            recorded_process_is_alive(meta.supervisor_pid, meta.supervisor_start_token);
        let child_alive = recorded_process_is_alive(meta.pid, meta.child_start_token);
        if supervisor_alive || child_alive {
            if meta.status == JobStatus::Starting {
                meta.status = JobStatus::Running;
            }
            return Some(meta);
        }
        // Give the supervisor a short grace period to write the terminal
        // record before declaring it lost.
        if now_ms().saturating_sub(meta.started_at_ms.unwrap_or(meta.created_at_ms)) < 2_000 {
            return Some(meta);
        }
        // Re-read before publishing `Lost`: the supervisor may have written
        // the terminal record between our first read and this point.
        if let Some(latest) = read_meta(&dir) {
            if latest.status.is_terminal() {
                return Some(latest);
            }
        }
        meta.status = JobStatus::Lost;
        meta.ended_at_ms = Some(now_ms());
        meta.error = Some("supervisor and child are gone without a terminal record".to_owned());
        let _ = write_meta(&dir, &meta);
        Some(meta)
    }

    /// Read the tail of a job's log. `max_bytes` is clamped to the file size;
    /// `max_lines` is clamped to [`MAX_LOG_LINES`].
    pub fn read_log(&self, id: &str, max_bytes: u64, max_lines: usize) -> Option<JobLog> {
        let dir = self.job_dir(id).ok()?;
        let path = dir.join("output.log");
        let metadata = std::fs::metadata(&path).ok()?;
        let len = metadata.len();
        let want = max_bytes.clamp(1, MAX_LOG_BYTES).min(len);
        let mut file = std::fs::File::open(&path).ok()?;
        file.seek(SeekFrom::Start(len.saturating_sub(want))).ok()?;
        let mut bytes = Vec::with_capacity(want as usize);
        file.read_to_end(&mut bytes).ok()?;
        let text = String::from_utf8_lossy(&bytes).into_owned();
        let max_lines = max_lines.clamp(1, MAX_LOG_LINES);
        let mut lines: Vec<&str> = text.lines().collect();
        let line_truncated = lines.len() > max_lines;
        if line_truncated {
            lines = lines[lines.len() - max_lines..].to_vec();
        }
        Some(JobLog {
            text: lines.join("\n"),
            bytes: len,
            truncated: len > want || line_truncated,
        })
    }

    /// Wait for a terminal state, polling the persisted record.
    pub fn wait(&self, id: &str, timeout: Duration) -> Option<JobMeta> {
        let deadline = std::time::Instant::now() + timeout;
        loop {
            let meta = self.status(id)?;
            if meta.status.is_terminal() {
                return Some(meta);
            }
            if std::time::Instant::now() >= deadline {
                return Some(meta);
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    /// Kill the supervisor and its child tree (explicit abort path). A job
    /// that already exited is a no-op.
    pub fn kill(&self, id: &str) -> Result<bool, String> {
        let dir = self.job_dir(id)?;
        let mut meta = read_meta(&dir).ok_or_else(|| format!("job not found: {id}"))?;
        if meta.status.is_terminal() {
            return Ok(false);
        }
        // Never signal a recycled pid: only accept a recorded pid whose start
        // token still matches (legacy token-less records keep the old probe).
        let supervisor_pid = meta
            .supervisor_pid
            .filter(|pid| recorded_process_is_alive(Some(*pid), meta.supervisor_start_token));
        let child_pid = meta
            .pid
            .filter(|pid| recorded_process_is_alive(Some(*pid), meta.child_start_token));
        let Some(pid) = supervisor_pid.or(child_pid) else {
            return Ok(false);
        };
        #[cfg(windows)]
        {
            let status = std::process::Command::new("taskkill")
                .args(["/T", "/F", "/PID", &pid.to_string()])
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status();
            let _ = status;
        }
        #[cfg(unix)]
        {
            // The supervisor is a process-group leader (see `spawn`), so a
            // negative pid reaches it and every child in the job tree.
            let group = format!("-{pid}");
            let _ = std::process::Command::new("kill")
                .args(["-TERM", "--", &group])
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status();
        }
        // Do not overwrite a terminal record written while we were signalling.
        if let Some(latest) = read_meta(&dir) {
            if latest.status.is_terminal() {
                return Ok(true);
            }
        }
        meta.status = JobStatus::Killed;
        meta.ended_at_ms = Some(now_ms());
        meta.error = Some("killed by user".to_owned());
        write_meta(&dir, &meta)?;
        Ok(true)
    }

    /// Collect a terminal job's output and mark it reaped. Running jobs are
    /// refused (use [`Self::wait`] first).
    pub fn reap(&self, id: &str, max_bytes: u64, max_lines: usize) -> Result<JobReap, String> {
        let dir = self.job_dir(id)?;
        let mut meta = read_meta(&dir).ok_or_else(|| format!("job not found: {id}"))?;
        if !meta.status.is_terminal() {
            return Err(format!(
                "job {id} is still {}; wait for it before reaping",
                meta.status.as_str()
            ));
        }
        let log = self.read_log(id, max_bytes, max_lines).unwrap_or_default();
        meta.reaped_at_ms = Some(now_ms());
        write_meta(&dir, &meta)?;
        Ok(JobReap { meta, log })
    }

    /// Remove terminal job directories older than `older_than` (or every
    /// terminal job when `all`). Running jobs are never touched. Directory
    /// names are the only trusted ids here: `meta.id` is never joined to a
    /// path, so a forged record cannot point `clean` outside the store root.
    pub fn clean(&self, older_than: Duration, all: bool) -> CleanReport {
        let mut report = CleanReport::default();
        let now = now_ms();
        let Ok(entries) = std::fs::read_dir(&self.root) else {
            return report;
        };
        for entry in entries.flatten() {
            let dir = entry.path();
            // A symlink is not a job directory, even if it points at one.
            if !entry.file_type().is_ok_and(|file_type| file_type.is_dir()) {
                continue;
            }
            let Some(id) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            if validate_job_id(&id).is_err() {
                continue;
            }
            let Some(meta) = read_meta(&dir) else {
                continue;
            };
            if !meta.status.is_terminal() {
                continue;
            }
            let ended = meta.ended_at_ms.unwrap_or(meta.created_at_ms);
            let age_ms = now.saturating_sub(ended);
            if !all && age_ms < older_than.as_millis() as u64 {
                continue;
            }
            match std::fs::remove_dir_all(&dir) {
                Ok(()) => report.removed.push(id),
                Err(error) => report
                    .errors
                    .push(format!("failed to remove {}: {error}", dir.display())),
            }
        }
        report
    }
}

fn write_meta(dir: &Path, meta: &JobMeta) -> Result<(), String> {
    let json = serde_json::to_vec_pretty(meta).map_err(|error| error.to_string())?;
    let target = dir.join("meta.json");
    // A unique temp name per writer: two supervisors/rewriters must never
    // share one temp file, or their bytes would interleave before publish.
    let tmp = dir.join(format!(
        "meta.json.tmp-{}-{}-{}",
        std::process::id(),
        META_TMP_SEQ.fetch_add(1, Ordering::Relaxed),
        uuid::Uuid::new_v4().simple()
    ));
    let publish = (|| -> Result<(), String> {
        let mut file = std::fs::File::create(&tmp)
            .map_err(|error| format!("create job meta temp: {error}"))?;
        file.write_all(&json)
            .map_err(|error| format!("write job meta: {error}"))?;
        file.flush()
            .map_err(|error| format!("flush job meta: {error}"))?;
        file.sync_all()
            .map_err(|error| format!("sync job meta: {error}"))?;
        // The handle must be closed before the rename: Windows cannot rename
        // a file whose handle is still open without delete sharing.
        drop(file);
        // `std::fs::rename` replaces an existing destination atomically on
        // Unix and on Windows (the standard library maps it to
        // `MoveFileExW(MOVEFILE_REPLACE_EXISTING)`). Removing the target
        // first would open a window where readers see no `meta.json`.
        std::fs::rename(&tmp, &target).map_err(|error| format!("publish job meta: {error}"))?;
        Ok(())
    })();
    if publish.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    publish?;
    sync_dir(dir);
    Ok(())
}

/// Publish the freshly spawned supervisor into a record that is still
/// `Starting`. The supervisor may already have advanced the record (child pid,
/// or even a terminal state) before the parent regains the CPU; in that case
/// leave the newer record untouched instead of overwriting it with the
/// parent's stale snapshot.
fn record_supervisor_start(
    dir: &Path,
    supervisor_pid: u32,
    supervisor_start_token: Option<u64>,
) -> Result<JobMeta, String> {
    let mut meta = read_meta(dir)
        .ok_or_else(|| "job meta disappeared after spawning the supervisor".to_owned())?;
    if meta.status != JobStatus::Starting {
        return Ok(meta);
    }
    meta.status = JobStatus::Running;
    meta.supervisor_pid = Some(supervisor_pid);
    meta.supervisor_start_token = supervisor_start_token;
    meta.started_at_ms = Some(now_ms());
    write_meta(dir, &meta)?;
    Ok(meta)
}

/// Fsync the job directory on Unix so the rename is durable; a no-op where
/// directory fsync is not supported.
fn sync_dir(dir: &Path) {
    #[cfg(unix)]
    {
        if let Ok(handle) = std::fs::File::open(dir) {
            let _ = handle.sync_all();
        }
    }
    #[cfg(not(unix))]
    let _ = dir;
}

fn read_meta(dir: &Path) -> Option<JobMeta> {
    let bytes = std::fs::read(dir.join("meta.json")).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// Test-only visibility hook: rewrites a job's `meta.json` through the exact
/// atomic publish path used by the supervisor.
#[doc(hidden)]
pub fn __test_write_meta(dir: &Path, meta: &JobMeta) -> Result<(), String> {
    write_meta(dir, meta)
}

/// Test-only visibility hook: reads a job's `meta.json` directly.
#[doc(hidden)]
pub fn __test_read_meta(dir: &Path) -> Option<JobMeta> {
    read_meta(dir)
}

/// Test-only visibility hook: the platform process start token for `pid`.
#[doc(hidden)]
pub fn __test_process_start_token(pid: u32) -> Option<u64> {
    process_start_token(pid)
}

/// Test-only visibility hook: the conditional post-spawn supervisor update.
#[doc(hidden)]
pub fn __test_record_supervisor_start(
    dir: &Path,
    supervisor_pid: u32,
    supervisor_start_token: Option<u64>,
) -> Result<JobMeta, String> {
    record_supervisor_start(dir, supervisor_pid, supervisor_start_token)
}

#[cfg(unix)]
fn process_is_alive(pid: u32) -> bool {
    // SAFETY: `kill(pid, 0)` performs only an existence/permission probe; it
    // sends no signal.
    #[allow(unsafe_code)]
    let result = unsafe { libc::kill(pid as i32, 0) };
    if result == 0 {
        return true;
    }
    std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

#[cfg(windows)]
fn process_is_alive(pid: u32) -> bool {
    // SAFETY: the pid is a plain identifier; the returned handle is checked
    // and closed below.
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
    // SAFETY: handle was just opened and is owned here.
    let state = unsafe { WaitForSingleObject(handle, 0) };
    // SAFETY: closing the handle opened above.
    unsafe {
        CloseHandle(handle);
    }
    state != WAIT_OBJECT_0
}

#[cfg(not(any(unix, windows)))]
fn process_is_alive(_pid: u32) -> bool {
    true
}

/// A recorded pid is only this job's process when its start token still
/// matches. A legacy record without a token keeps the old pid-only probe.
#[cfg(any(unix, windows))]
fn recorded_process_is_alive(pid: Option<u32>, token: Option<u64>) -> bool {
    let Some(pid) = pid else {
        return false;
    };
    match token {
        // Identity alone is not liveness: a terminated process can still have
        // an openable process object (notably on Windows, where `OpenProcess`
        // succeeds until the last handle closes), so require both.
        Some(expected) => process_is_alive(pid) && process_start_token(pid) == Some(expected),
        None => process_is_alive(pid),
    }
}

#[cfg(not(any(unix, windows)))]
fn recorded_process_is_alive(pid: Option<u32>, _token: Option<u64>) -> bool {
    pid.is_some_and(process_is_alive)
}

/// Platform-specific process start token used to detect pid reuse.
///
/// Unix: field 22 (`starttime`) of `/proc/<pid>/stat`, in clock ticks since
/// boot. Windows: the process creation time from `GetProcessTimes`, as a
/// 100 ns FILETIME value. Other platforms have no portable equivalent and
/// return `None` (legacy pid-only behavior).
#[cfg(unix)]
fn process_start_token(pid: u32) -> Option<u64> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // The comm field (2) may contain spaces and parentheses, so split after
    // its last ')'. The remainder starts at field 3 (state); `starttime` is
    // field 22, i.e. index 19 in that remainder.
    let rest = &stat[stat.rfind(')')? + 1..];
    rest.split_whitespace().nth(19)?.parse().ok()
}

#[cfg(windows)]
fn process_start_token(pid: u32) -> Option<u64> {
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
    // SAFETY: `handle` was opened above and the four FILETIME locals are live
    // out-parameters.
    let ok = unsafe { GetProcessTimes(handle, &mut creation, &mut exit, &mut kernel, &mut user) };
    // SAFETY: closing the handle opened above.
    unsafe {
        CloseHandle(handle);
    }
    if ok == 0 {
        return None;
    }
    Some((creation.dwHighDateTime as u64) << 32 | creation.dwLowDateTime as u64)
}

#[cfg(not(any(unix, windows)))]
fn process_start_token(_pid: u32) -> Option<u64> {
    None
}

// ---------------------------------------------------------------------------
// Supervisor (`pir __jobs-supervise`)
// ---------------------------------------------------------------------------

/// Entry point for the hidden `__jobs-supervise` command. Returns the process
/// exit status; the job's real status is in `meta.json`.
pub fn run_supervisor(args: &[String]) -> i32 {
    let mut job_dir: Option<PathBuf> = None;
    let mut program: Option<String> = None;
    let mut program_args: Vec<String> = Vec::new();
    let mut command: Option<String> = None;
    let mut index = 0usize;
    while index < args.len() {
        match args[index].as_str() {
            "--job-dir" => {
                index += 1;
                job_dir = args.get(index).map(PathBuf::from);
            }
            "--program" => {
                index += 1;
                program = args.get(index).cloned();
            }
            "--arg" => {
                index += 1;
                if let Some(arg) = args.get(index) {
                    program_args.push(arg.clone());
                }
            }
            "--command" => {
                index += 1;
                command = args.get(index).cloned();
            }
            other => {
                eprintln!("__jobs-supervise: unknown argument {other}");
                return 2;
            }
        }
        index += 1;
    }
    let (Some(dir), Some(program), Some(command)) = (job_dir, program, command) else {
        eprintln!("__jobs-supervise: --job-dir, --program and --command are required");
        return 2;
    };
    let Ok(mut meta) = read_meta(&dir).ok_or_else(|| "missing meta".to_owned()) else {
        eprintln!("__jobs-supervise: unreadable job meta in {}", dir.display());
        return 2;
    };
    meta.status = JobStatus::Running;
    let supervisor_pid = std::process::id();
    meta.supervisor_pid = Some(supervisor_pid);
    meta.supervisor_start_token = process_start_token(supervisor_pid);
    meta.started_at_ms = Some(now_ms());
    let _ = write_meta(&dir, &meta);

    let log_path = dir.join("output.log");
    let log = match std::fs::OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(&log_path)
    {
        Ok(file) => file,
        Err(error) => return finish_spawn_error(&dir, &mut meta, format!("open log: {error}")),
    };
    let sink = std::sync::Arc::new(LogSink::new(log));
    let mut command_builder = std::process::Command::new(&program);
    command_builder
        .args(&program_args)
        .arg(&command)
        .current_dir(&meta.spec.cwd)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    for (key, value) in &meta.spec.env {
        command_builder.env(key, value);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command_builder.creation_flags(0x0800_0000);
    }
    let mut child = match command_builder.spawn() {
        Ok(child) => child,
        Err(error) => {
            return finish_spawn_error(&dir, &mut meta, format!("spawn command: {error}"));
        }
    };
    meta.pid = Some(child.id());
    meta.child_start_token = process_start_token(child.id());
    let _ = write_meta(&dir, &meta);

    let mut readers = Vec::new();
    if let Some(stdout) = child.stdout.take() {
        let sink = std::sync::Arc::clone(&sink);
        readers.push(std::thread::spawn(move || copy_pipe(stdout, sink)));
    }
    if let Some(stderr) = child.stderr.take() {
        let sink = std::sync::Arc::clone(&sink);
        readers.push(std::thread::spawn(move || copy_pipe(stderr, sink)));
    }
    let status = child.wait();
    for reader in readers {
        let _ = reader.join();
    }
    let (log_bytes, truncated) = sink.finish();
    meta.log_bytes = log_bytes;
    meta.log_truncated = truncated;
    meta.ended_at_ms = Some(now_ms());
    match status {
        Ok(status) => {
            meta.exit_code = status.code();
            meta.status = JobStatus::Exited;
            if status.code().is_none() {
                meta.status = JobStatus::Killed;
                meta.error = Some("terminated by signal".to_owned());
            }
        }
        Err(error) => {
            meta.status = JobStatus::Failed;
            meta.error = Some(format!("wait failed: {error}"));
        }
    }
    let _ = write_meta(&dir, &meta);
    0
}

fn finish_spawn_error(dir: &Path, meta: &mut JobMeta, error: String) -> i32 {
    meta.status = JobStatus::Failed;
    meta.error = Some(error);
    meta.ended_at_ms = Some(now_ms());
    let _ = write_meta(dir, meta);
    1
}

struct LogSink {
    file: std::sync::Mutex<std::fs::File>,
    written: AtomicU64,
    truncated: std::sync::atomic::AtomicBool,
}

impl LogSink {
    fn new(file: std::fs::File) -> Self {
        Self {
            file: std::sync::Mutex::new(file),
            written: AtomicU64::new(0),
            truncated: std::sync::atomic::AtomicBool::new(false),
        }
    }

    fn write(&self, bytes: &[u8]) {
        let Ok(mut file) = self.file.lock() else {
            return;
        };
        let written = self.written.load(Ordering::Relaxed);
        if written >= MAX_LOG_BYTES {
            self.truncated.store(true, Ordering::Relaxed);
            return;
        }
        let remaining = MAX_LOG_BYTES - written;
        let take = (bytes.len() as u64).min(remaining) as usize;
        if take < bytes.len() {
            self.truncated.store(true, Ordering::Relaxed);
        }
        if file.write_all(&bytes[..take]).is_ok() {
            self.written.fetch_add(take as u64, Ordering::Relaxed);
        }
    }

    fn finish(&self) -> (u64, bool) {
        if let Ok(mut file) = self.file.lock() {
            let _ = file.flush();
        }
        (
            self.written.load(Ordering::Relaxed),
            self.truncated.load(Ordering::Relaxed),
        )
    }
}

fn copy_pipe(mut pipe: impl Read, sink: std::sync::Arc<LogSink>) {
    let mut buffer = [0u8; 16 * 1024];
    loop {
        match pipe.read(&mut buffer) {
            Ok(0) | Err(_) => break,
            Ok(n) => sink.write(&buffer[..n]),
        }
    }
}
