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
    /// Child process id (the actual command shell).
    pub pid: Option<u32>,
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

    fn job_dir(&self, id: &str) -> PathBuf {
        self.root.join(id)
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
        let dir = self.job_dir(&id);
        std::fs::create_dir_all(&dir).map_err(|error| format!("create job dir: {error}"))?;
        let meta = JobMeta {
            version: 1,
            id: id.clone(),
            spec: spec.clone(),
            status: JobStatus::Starting,
            supervisor_pid: None,
            pid: None,
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
        let child = command
            .spawn()
            .map_err(|error| format!("spawn job supervisor: {error}"))?;
        let mut meta = meta;
        meta.status = JobStatus::Running;
        meta.supervisor_pid = Some(child.id());
        meta.started_at_ms = Some(now_ms());
        write_meta(&dir, &meta)?;
        Ok(meta)
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
        let dir = self.job_dir(id);
        let mut meta = read_meta(&dir)?;
        if meta.status.is_terminal() {
            return Some(meta);
        }
        let supervisor_alive = meta.supervisor_pid.is_some_and(process_is_alive);
        let child_alive = meta.pid.is_some_and(process_is_alive);
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
        meta.status = JobStatus::Lost;
        meta.ended_at_ms = Some(now_ms());
        meta.error = Some("supervisor and child are gone without a terminal record".to_owned());
        let _ = write_meta(&dir, &meta);
        Some(meta)
    }

    /// Read the tail of a job's log. `max_bytes` is clamped to the file size;
    /// `max_lines` is clamped to [`MAX_LOG_LINES`].
    pub fn read_log(&self, id: &str, max_bytes: u64, max_lines: usize) -> Option<JobLog> {
        let dir = self.job_dir(id);
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
        let dir = self.job_dir(id);
        let mut meta = read_meta(&dir).ok_or_else(|| format!("job not found: {id}"))?;
        if meta.status.is_terminal() {
            return Ok(false);
        }
        let pid = meta.supervisor_pid.or(meta.pid);
        let Some(pid) = pid else {
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
            let _ = std::process::Command::new("kill")
                .args(["-TERM", &pid.to_string()])
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status();
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
        let dir = self.job_dir(id);
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
    /// terminal job when `all`). Running jobs are never touched.
    pub fn clean(&self, older_than: Duration, all: bool) -> CleanReport {
        let mut report = CleanReport::default();
        let now = now_ms();
        for meta in self.list() {
            if !meta.status.is_terminal() {
                continue;
            }
            let ended = meta.ended_at_ms.unwrap_or(meta.created_at_ms);
            let age_ms = now.saturating_sub(ended);
            if !all && age_ms < older_than.as_millis() as u64 {
                continue;
            }
            let dir = self.job_dir(&meta.id);
            match std::fs::remove_dir_all(&dir) {
                Ok(()) => report.removed.push(meta.id),
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
    let tmp = dir.join("meta.json.tmp");
    std::fs::write(&tmp, &json).map_err(|error| format!("write job meta: {error}"))?;
    // Windows rename-over-existing is not guaranteed by std; remove first.
    if target.exists() {
        let _ = std::fs::remove_file(&target);
    }
    std::fs::rename(&tmp, &target).map_err(|error| format!("publish job meta: {error}"))
}

fn read_meta(dir: &Path) -> Option<JobMeta> {
    let bytes = std::fs::read(dir.join("meta.json")).ok()?;
    serde_json::from_slice(&bytes).ok()
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
    meta.supervisor_pid = Some(std::process::id());
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
