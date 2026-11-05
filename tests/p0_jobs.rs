//! P0-2: persistent cross-process job records, detached execution, log
//! reading caps, kill, reap and cleanup.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
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

/// Minimal record for tests that only exercise id/path handling.
fn manual_meta(root: &Path, id: &str, status: JobStatus) -> JobMeta {
    JobMeta {
        version: 1,
        id: id.to_owned(),
        spec: JobSpec {
            command: "true".to_owned(),
            cwd: root.to_path_buf(),
            program: "sh".to_owned(),
            args: vec!["-c".to_owned()],
            env: Vec::new(),
        },
        status,
        supervisor_pid: None,
        supervisor_start_token: None,
        pid: None,
        child_start_token: None,
        created_at_ms: 1,
        started_at_ms: Some(1),
        ended_at_ms: Some(2),
        exit_code: Some(0),
        error: None,
        log_bytes: 0,
        log_truncated: false,
        reaped_at_ms: None,
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
    // A very fast supervisor may already have published a terminal record
    // before `spawn` returns; anything but Lost is a valid post-spawn state.
    assert!(
        !matches!(meta.status, JobStatus::Lost),
        "spawn returned a lost record: {meta:?}"
    );

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
        supervisor_start_token: None,
        pid: None,
        child_start_token: None,
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
fn concurrent_meta_rewrites_never_expose_a_missing_record() {
    let root = temp_root("meta-race");
    let jobs_root = root.join("jobs");
    let dir = jobs_root.join("job-race");
    std::fs::create_dir_all(&dir).expect("job dir");
    let spec = JobSpec {
        command: "true".to_owned(),
        cwd: root.clone(),
        program: "sh".to_owned(),
        args: vec!["-c".to_owned()],
        env: Vec::new(),
    };
    let base = JobMeta {
        version: 1,
        id: "job-race".to_owned(),
        spec,
        status: JobStatus::Exited,
        supervisor_pid: None,
        supervisor_start_token: None,
        pid: None,
        child_start_token: None,
        created_at_ms: 1,
        started_at_ms: Some(1),
        ended_at_ms: Some(2),
        exit_code: Some(0),
        error: None,
        log_bytes: 0,
        log_truncated: false,
        reaped_at_ms: None,
    };
    pir::jobs::__test_write_meta(&dir, &base).expect("initial meta");

    let stop = Arc::new(AtomicBool::new(false));
    let reader_stop = Arc::clone(&stop);
    let reader_jobs_root = jobs_root.clone();
    let reader_dir = dir.clone();
    let reader = std::thread::spawn(move || {
        let store = JobStore::with_root(&reader_jobs_root);
        let mut reads = 0u64;
        let mut misses = 0u64;
        while !reader_stop.load(Ordering::Relaxed) {
            if store.status("job-race").is_none() {
                misses += 1;
            }
            if pir::jobs::__test_read_meta(&reader_dir).is_none() {
                misses += 1;
            }
            reads += 2;
        }
        (reads, misses)
    });

    // Thousands of rewrites, mirroring the concurrent supervisor/status load
    // that exposed the remove-then-rename window.
    for index in 0..2000 {
        let mut meta = base.clone();
        meta.exit_code = Some(index);
        pir::jobs::__test_write_meta(&dir, &meta).expect("rewrite meta");
    }
    stop.store(true, Ordering::Relaxed);
    let (reads, misses) = reader.join().expect("reader thread");
    assert!(reads > 0, "reader never observed the record");
    assert_eq!(
        misses, 0,
        "{misses} of {reads} concurrent reads observed no meta.json"
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn status_rejects_a_recycled_pid_with_a_different_start_token() {
    let root = temp_root("pid-identity");
    let jobs_root = root.join("jobs");
    let dir = jobs_root.join("job-identity");
    std::fs::create_dir_all(&dir).expect("job dir");
    let own_pid = std::process::id();
    let Some(own_token) = pir::jobs::__test_process_start_token(own_pid) else {
        // No start token on this platform: only the legacy pid probe exists.
        let _ = std::fs::remove_dir_all(&root);
        return;
    };
    let spec = JobSpec {
        command: "true".to_owned(),
        cwd: root.clone(),
        program: "sh".to_owned(),
        args: vec!["-c".to_owned()],
        env: Vec::new(),
    };
    let mut meta = JobMeta {
        version: 1,
        id: "job-identity".to_owned(),
        spec,
        status: JobStatus::Running,
        supervisor_pid: Some(own_pid),
        supervisor_start_token: Some(own_token),
        pid: None,
        child_start_token: None,
        // Old enough to bypass the 2 s supervisor grace period.
        created_at_ms: 1,
        started_at_ms: Some(1),
        ended_at_ms: None,
        exit_code: None,
        error: None,
        log_bytes: 0,
        log_truncated: false,
        reaped_at_ms: None,
    };
    pir::jobs::__test_write_meta(&dir, &meta).expect("meta");
    let store = JobStore::with_root(&jobs_root);

    // Matching token: the recorded supervisor really is this process.
    let status = store.status("job-identity").expect("status");
    assert_eq!(status.status, JobStatus::Running);

    // A genuine legacy record has no token fields at all: it must still load
    // (serde default) and fall back to the pid-only probe.
    let mut legacy = serde_json::to_value(&meta).expect("meta json");
    let object = legacy.as_object_mut().expect("meta object");
    object.remove("supervisorStartToken");
    object.remove("childStartToken");
    std::fs::write(
        dir.join("meta.json"),
        serde_json::to_vec(&legacy).expect("legacy json"),
    )
    .expect("write legacy meta");
    let status = store.status("job-identity").expect("status");
    assert_eq!(status.status, JobStatus::Running);

    // Recycled pid: same pid, different creation time must not be Running.
    meta.supervisor_start_token = Some(own_token.wrapping_add(1));
    pir::jobs::__test_write_meta(&dir, &meta).expect("meta");
    let status = store.status("job-identity").expect("status");
    assert_eq!(status.status, JobStatus::Lost, "meta: {status:?}");

    // Windows keeps the process object openable after exit while the `Child`
    // handle is held, so a matching token alone must not count as alive.
    #[cfg(windows)]
    {
        use std::process::{Command, Stdio};
        let mut child = Command::new("cmd")
            .args(["/C", "ping -n 2 127.0.0.1 >nul"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn short-lived child");
        let dead_pid = child.id();
        let dead_token =
            pir::jobs::__test_process_start_token(dead_pid).expect("token before exit");
        child.wait().expect("wait for child");
        meta.supervisor_pid = Some(dead_pid);
        meta.supervisor_start_token = Some(dead_token);
        pir::jobs::__test_write_meta(&dir, &meta).expect("meta");
        let status = store.status("job-identity").expect("status");
        assert_eq!(status.status, JobStatus::Lost, "meta: {status:?}");
        drop(child);
    }
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn rejects_ids_that_escape_the_store_root() {
    let root = temp_root("traversal");
    let jobs_root = root.join("jobs");
    std::fs::create_dir_all(&jobs_root).expect("jobs root");
    let victim = root.join("victim");
    std::fs::create_dir_all(&victim).expect("victim dir");
    let victim_meta = manual_meta(&root, "victim", JobStatus::Exited);
    pir::jobs::__test_write_meta(&victim, &victim_meta).expect("victim meta");
    std::fs::write(victim.join("sentinel.txt"), b"keep").expect("sentinel");
    let store = JobStore::with_root(&jobs_root);

    for id in [
        "",
        ".",
        "..",
        "../victim",
        "..\\victim",
        "a/b",
        "a\\b",
        "/victim",
        "C:\\victim",
        "C:/victim",
        "\\\\server\\share",
        "job/../victim",
        "job id",
        "job\0id",
    ] {
        assert!(store.status(id).is_none(), "status accepted {id:?}");
        assert!(
            store.read_log(id, 1024, 10).is_none(),
            "read_log accepted {id:?}"
        );
        assert!(store.reap(id, 1024, 10).is_err(), "reap accepted {id:?}");
        assert!(store.kill(id).is_err(), "kill accepted {id:?}");
        assert!(
            store.wait(id, Duration::ZERO).is_none(),
            "wait accepted {id:?}"
        );
    }
    assert!(victim.join("sentinel.txt").exists());
    assert!(pir::jobs::__test_read_meta(&victim).is_some());
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn clean_removes_the_directory_not_a_forged_meta_id() {
    let root = temp_root("clean-traversal");
    let jobs_root = root.join("jobs");
    std::fs::create_dir_all(&jobs_root).expect("jobs root");
    let victim = root.join("victim");
    std::fs::create_dir_all(&victim).expect("victim dir");
    let victim_meta = manual_meta(&root, "victim", JobStatus::Exited);
    pir::jobs::__test_write_meta(&victim, &victim_meta).expect("victim meta");
    std::fs::write(victim.join("sentinel.txt"), b"keep").expect("sentinel");

    let evil_dir = jobs_root.join("job-evil");
    std::fs::create_dir_all(&evil_dir).expect("evil dir");
    let evil = manual_meta(&root, "../../victim", JobStatus::Exited);
    pir::jobs::__test_write_meta(&evil_dir, &evil).expect("evil meta");

    let good_dir = jobs_root.join("job-good");
    std::fs::create_dir_all(&good_dir).expect("good dir");
    let good = manual_meta(&root, "job-good", JobStatus::Exited);
    pir::jobs::__test_write_meta(&good_dir, &good).expect("good meta");

    let report = JobStore::with_root(&jobs_root).clean(Duration::ZERO, true);
    assert!(
        report.removed.contains(&"job-evil".to_owned()),
        "{report:?}"
    );
    assert!(
        report.removed.contains(&"job-good".to_owned()),
        "{report:?}"
    );
    assert!(
        !report.removed.iter().any(|id| id.contains("victim")),
        "{report:?}"
    );
    assert!(
        victim.join("sentinel.txt").exists(),
        "clean followed a forged meta.id"
    );
    assert!(pir::jobs::__test_read_meta(&victim).is_some());
    assert!(
        !evil_dir.exists(),
        "the actual job directory must be removed"
    );
    assert!(!good_dir.exists());
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn post_spawn_update_never_overwrites_a_newer_record() {
    let root = temp_root("spawn-update");
    let dir = root.join("jobs").join("job-spawn");
    std::fs::create_dir_all(&dir).expect("job dir");

    let starting = manual_meta(&root, "job-spawn", JobStatus::Starting);
    pir::jobs::__test_write_meta(&dir, &starting).expect("starting meta");
    let updated = pir::jobs::__test_record_supervisor_start(&dir, 4242, Some(77)).expect("update");
    assert_eq!(updated.status, JobStatus::Running);
    assert_eq!(updated.supervisor_pid, Some(4242));
    assert_eq!(updated.supervisor_start_token, Some(77));
    assert!(updated.started_at_ms.is_some());

    // A fast supervisor already recorded the child: keep that record.
    let mut running = manual_meta(&root, "job-spawn", JobStatus::Running);
    running.pid = Some(999);
    running.child_start_token = Some(5);
    pir::jobs::__test_write_meta(&dir, &running).expect("running meta");
    let kept = pir::jobs::__test_record_supervisor_start(&dir, 4242, Some(77)).expect("update");
    assert_eq!(kept.status, JobStatus::Running);
    assert_eq!(kept.pid, Some(999));
    assert_eq!(kept.supervisor_pid, None);

    // A terminal record must never be replaced.
    let mut terminal = manual_meta(&root, "job-spawn", JobStatus::Exited);
    terminal.exit_code = Some(3);
    terminal.ended_at_ms = Some(4);
    pir::jobs::__test_write_meta(&dir, &terminal).expect("terminal meta");
    let kept = pir::jobs::__test_record_supervisor_start(&dir, 4242, Some(77)).expect("update");
    assert_eq!(kept.status, JobStatus::Exited);
    assert_eq!(kept.exit_code, Some(3));
    assert_eq!(kept.supervisor_pid, None);
    let on_disk = pir::jobs::__test_read_meta(&dir).expect("meta on disk");
    assert_eq!(on_disk.status, JobStatus::Exited);
    assert_eq!(on_disk.exit_code, Some(3));
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
