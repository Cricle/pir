//! P0-2: persistent cross-process job records, detached execution, log
//! reading caps, kill, reap and cleanup.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use pir::jobs::{JobMeta, JobSpec, JobStatus, JobStore, MAX_LOG_LINES};

fn temp_root(label: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "pir-p0-jobs-{label}-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or(0)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp root");
    dir
}

fn job_store(root: &Path) -> JobStore {
    JobStore::with_root(root.join("jobs")).with_supervisor(env!("CARGO_BIN_EXE_pir"))
}

fn shell_spec(command: &str) -> JobSpec {
    let config = pir::coding_agent::tools::resolve_shell_config(None).expect("shell");
    assert!(!config.stdin_transport(), "test shell must support -c");
    JobSpec {
        command: command.to_owned(),
        cwd: std::env::current_dir().expect("cwd"),
        program: config.program().to_owned(),
        args: config.args().iter().map(|arg| (*arg).to_owned()).collect(),
        env: Vec::new(),
    }
}

#[test]
fn job_runs_detached_and_records_exit_code_and_output() {
    let root = temp_root("run");
    let store = job_store(&root);
    let meta = store
        .spawn(&shell_spec("printf 'hello-job\\n'; exit 7"))
        .expect("spawn");
    assert!(!meta.id.is_empty());
    assert!(matches!(
        meta.status,
        JobStatus::Starting | JobStatus::Running
    ));

    let done = store.wait(&meta.id, Duration::from_secs(60)).expect("wait");
    assert_eq!(done.status, JobStatus::Exited, "meta: {done:?}");
    assert_eq!(done.exit_code, Some(7));
    assert!(done.pid.is_some(), "the child pid must be recorded");
    assert!(
        done.supervisor_pid.is_some(),
        "the supervisor pid must be recorded"
    );

    // Reopening the store (the TUI/agent is gone) still sees the result.
    let reopened = job_store(&root);
    let status = reopened.status(&meta.id).expect("status after reopen");
    assert_eq!(status.status, JobStatus::Exited);
    let log = reopened
        .read_log(&meta.id, 64 * 1024, MAX_LOG_LINES)
        .expect("log");
    assert!(log.text.contains("hello-job"), "log: {:?}", log.text);

    let reaped = reopened
        .reap(&meta.id, 64 * 1024, MAX_LOG_LINES)
        .expect("reap");
    assert_eq!(reaped.meta.exit_code, Some(7));
    assert!(reaped.meta.reaped_at_ms.is_some());
    assert!(reaped.log.text.contains("hello-job"));
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn running_job_is_visible_then_kill_marks_it_terminal() {
    let root = temp_root("kill");
    let store = job_store(&root);
    let meta = store.spawn(&shell_spec("sleep 30")).expect("spawn");
    let running = store.status(&meta.id).expect("status");
    assert!(
        matches!(running.status, JobStatus::Running | JobStatus::Starting),
        "meta: {running:?}"
    );
    let killed = store.kill(&meta.id).expect("kill");
    assert!(killed);
    let after = store.status(&meta.id).expect("status after kill");
    assert_eq!(after.status, JobStatus::Killed);
    assert!(store.reap(&meta.id, 4096, 100).is_ok());
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn read_log_returns_a_bounded_tail() {
    let root = temp_root("log");
    let jobs = root.join("jobs").join("job-manual");
    std::fs::create_dir_all(&jobs).expect("job dir");
    let spec = JobSpec {
        command: "true".to_owned(),
        cwd: root.clone(),
        program: "sh".to_owned(),
        args: vec!["-c".to_owned()],
        env: Vec::new(),
    };
    let meta = JobMeta {
        version: 1,
        id: "job-manual".to_owned(),
        spec,
        status: JobStatus::Exited,
        supervisor_pid: None,
        pid: None,
        created_at_ms: 1,
        started_at_ms: Some(1),
        ended_at_ms: Some(2),
        exit_code: Some(0),
        error: None,
        log_bytes: 0,
        log_truncated: false,
        reaped_at_ms: None,
    };
    std::fs::write(
        jobs.join("meta.json"),
        serde_json::to_vec(&meta).expect("meta"),
    )
    .expect("write meta");
    let mut output = String::new();
    for index in 0..(MAX_LOG_LINES + 500) {
        output.push_str(&format!("line-{index}\n"));
    }
    std::fs::write(jobs.join("output.log"), output).expect("log");

    let store = JobStore::with_root(root.join("jobs"));
    let log = store
        .read_log("job-manual", 64 * 1024, 100)
        .expect("read log");
    assert_eq!(log.text.lines().count(), 100);
    assert!(log.text.lines().next().unwrap().starts_with("line-"));
    assert!(log.truncated);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn job_survives_the_process_that_started_it() {
    let root = temp_root("survive");
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_pir"))
        .args(["jobs", "start", "--", "printf 'survived\\n'; exit 3"])
        .env("PIR_AGENT_DIR", &root)
        .env("PIR_NO_CONTEXT_FILES", "1")
        .stdin(std::process::Stdio::null())
        .output()
        .expect("run pir jobs start");
    assert!(
        output.status.success(),
        "start failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let id = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    assert!(!id.is_empty(), "the start command must print the job id");

    // The process that spawned the job has already exited. A fresh store must
    // see the supervisor's record and the final exit code.
    let store = JobStore::with_root(root.join("jobs"));
    let deadline = std::time::Instant::now() + Duration::from_secs(60);
    let done = loop {
        let meta = store.status(&id).expect("job record after parent exit");
        if meta.status.is_terminal() || std::time::Instant::now() >= deadline {
            break meta;
        }
        std::thread::sleep(Duration::from_millis(100));
    };
    assert_eq!(done.status, JobStatus::Exited, "meta: {done:?}");
    assert_eq!(done.exit_code, Some(3));
    let log = store.read_log(&id, 4096, 100).expect("log");
    assert!(log.text.contains("survived"), "log: {:?}", log.text);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn clean_removes_terminal_jobs_only_when_allowed() {
    let root = temp_root("clean");
    let store = job_store(&root);
    let meta = store.spawn(&shell_spec("exit 0")).expect("spawn");
    let done = store.wait(&meta.id, Duration::from_secs(60)).expect("wait");
    assert!(done.status.is_terminal());
    // Not old enough: without `all` it survives.
    let report = store.clean(Duration::from_secs(86_400), false);
    assert!(report.removed.is_empty());
    assert!(store.status(&meta.id).is_some());
    let report = store.clean(Duration::ZERO, true);
    assert_eq!(report.removed, vec![meta.id.clone()]);
    assert!(store.status(&meta.id).is_none());
    let _ = std::fs::remove_dir_all(&root);
}
