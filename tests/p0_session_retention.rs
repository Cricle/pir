//! P0-4: session retention boundaries, active-session protection, safe
//! deletion (sidecars + non-session refusal), and empty-directory behavior.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use pir::ai::types::*;
use pir::session_manager::retention::{
    plan_session_prune, prune_session_dir, resolve_all_prune_root, scan_session_files,
    session_sidecar_paths, PruneReason, SessionFileInfo, SessionRetentionPolicy,
};
use pir::session_manager::SessionManager;

fn temp_dir(label: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "pir-p0-retention-{label}-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or(0)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");
    dir
}

fn user_message(text: &str) -> Message {
    Message::User(UserMessage {
        role: UserRole::User,
        content: vec![ContentBlock::Text(TextContent {
            text: text.to_owned(),
            text_signature: None,
        })],
        tools_added: None,
        tools_removed: None,
        timestamp: 1,
        sections: None,
    })
}

fn create_session(dir: &Path, text: &str) -> PathBuf {
    let mut manager = SessionManager::create(".", dir).expect("create session");
    manager.append_message(user_message(text)).expect("append");
    let path = manager.path().to_path_buf();
    drop(manager);
    path
}

/// Sidecar `.<name>.lock` path for a transcript.
fn lock_path(session: &Path) -> PathBuf {
    session_sidecar_paths(session)
        .into_iter()
        .find(|path| path.to_string_lossy().ends_with(".lock"))
        .expect("writer lock path")
}

/// Spawn a child, record its start token, wait for it, and return its now-dead
/// pid. The process has been reaped, so a local liveness probe must report it
/// dead. Recording the start token also exercises pid-reuse detection.
fn exited_child_pid() -> (u32, Option<u64>) {
    let mut command = if cfg!(windows) {
        let mut command = std::process::Command::new("cmd");
        command.args(["/C", "exit", "0"]);
        command
    } else {
        std::process::Command::new("true")
    };
    let mut child = command.spawn().expect("spawn short-lived child");
    let pid = child.id();
    let start_token = pir::jobs::__test_process_start_token(pid);
    let _ = child.wait().expect("reap short-lived child");
    (pid, start_token)
}

fn dead_lock_contents(pid: u32, start_token: Option<u64>) -> String {
    let mut content = format!("pid={pid}\ntoken=dead-token\nstarted=1\n");
    if let Some(start_token) = start_token {
        content.push_str(&format!("start={start_token}\n"));
    }
    content
}

fn set_mtime(path: &Path, modified: SystemTime) {
    let file = std::fs::OpenOptions::new()
        .write(true)
        .open(path)
        .expect("open session");
    file.set_times(std::fs::FileTimes::new().set_modified(modified))
        .expect("set mtime");
}

fn info(path: &str, modified: SystemTime, bytes: u64) -> SessionFileInfo {
    SessionFileInfo {
        path: PathBuf::from(path),
        modified,
        bytes,
    }
}

#[test]
fn age_boundary_removes_only_sessions_older_than_the_limit() {
    let now = SystemTime::UNIX_EPOCH + Duration::from_secs(2_000_000_000);
    let policy = SessionRetentionPolicy {
        enabled: true,
        max_age_days: Some(30),
        max_sessions: None,
        max_bytes: None,
    };
    let files = vec![
        info("old.jsonl", now - Duration::from_secs(31 * 86_400), 10),
        info("fresh.jsonl", now - Duration::from_secs(29 * 86_400), 10),
        info("edge.jsonl", now - Duration::from_secs(30 * 86_400), 10),
    ];
    let plan = plan_session_prune(files, &policy, now, None);
    assert_eq!(plan.remove.len(), 1);
    assert_eq!(plan.remove[0].path, PathBuf::from("old.jsonl"));
    assert_eq!(plan.remove[0].reason, PruneReason::Expired);
    assert_eq!(plan.kept, 2);
}

#[test]
fn max_sessions_keeps_newest_and_reserves_the_active_slot() {
    let now = SystemTime::UNIX_EPOCH + Duration::from_secs(2_000_000_000);
    let policy = SessionRetentionPolicy {
        enabled: true,
        max_age_days: None,
        max_sessions: Some(3),
        max_bytes: None,
    };
    let files = vec![
        info("s1.jsonl", now - Duration::from_secs(5), 1),
        info("s2.jsonl", now - Duration::from_secs(4), 1),
        info("s3.jsonl", now - Duration::from_secs(3), 1),
        info("s4.jsonl", now - Duration::from_secs(2), 1),
        info("old-active.jsonl", now - Duration::from_secs(100), 1),
    ];
    let plan = plan_session_prune(files, &policy, now, Some(Path::new("old-active.jsonl")));
    // Two newest non-active + the active session fill the three slots.
    assert_eq!(plan.kept, 3, "plan: {plan:?}");
    let removed: Vec<&str> = plan
        .remove
        .iter()
        .map(|entry| entry.path.to_str().unwrap())
        .collect();
    assert_eq!(removed, vec!["s2.jsonl", "s1.jsonl"]);
}

#[test]
fn max_bytes_keeps_newest_until_the_budget_is_reached() {
    let now = SystemTime::UNIX_EPOCH + Duration::from_secs(2_000_000_000);
    let policy = SessionRetentionPolicy {
        enabled: true,
        max_age_days: None,
        max_sessions: None,
        max_bytes: Some(250),
    };
    let files = vec![
        info("new.jsonl", now - Duration::from_secs(1), 100),
        info("mid.jsonl", now - Duration::from_secs(2), 100),
        info("old.jsonl", now - Duration::from_secs(3), 100),
    ];
    let plan = plan_session_prune(files, &policy, now, None);
    assert_eq!(plan.kept, 2, "plan: {plan:?}");
    assert_eq!(plan.remove.len(), 1);
    assert_eq!(plan.remove[0].path, PathBuf::from("old.jsonl"));
    assert_eq!(plan.remove[0].reason, PruneReason::OverMaxBytes);
}

#[test]
fn active_session_survives_every_bound() {
    let now = SystemTime::UNIX_EPOCH + Duration::from_secs(2_000_000_000);
    let policy = SessionRetentionPolicy {
        enabled: true,
        max_age_days: Some(1),
        max_sessions: Some(0),
        max_bytes: Some(0),
    };
    let files = vec![
        info(
            "active.jsonl",
            now - Duration::from_secs(365 * 86_400),
            1_000,
        ),
        info("new.jsonl", now - Duration::from_secs(1), 10),
    ];
    let plan = plan_session_prune(files, &policy, now, Some(Path::new("active.jsonl")));
    assert!(
        plan.remove
            .iter()
            .all(|entry| entry.path != Path::new("active.jsonl")),
        "the active session must never be scheduled: {plan:?}"
    );
    assert_eq!(plan.remove.len(), 1);
    assert_eq!(plan.remove[0].path, PathBuf::from("new.jsonl"));
}

#[test]
fn empty_directory_is_a_clean_noop() {
    let dir = temp_dir("empty");
    let policy = SessionRetentionPolicy {
        enabled: true,
        max_age_days: Some(1),
        max_sessions: Some(1),
        max_bytes: Some(1),
    };
    let report = prune_session_dir(&dir, &policy, None);
    assert!(report.removed.is_empty());
    assert_eq!(report.kept, 0);
    assert!(report.errors.is_empty());
    assert!(scan_session_files(&dir).is_empty());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn prune_deletes_old_transcripts_and_their_sidecars_but_not_the_active_one() {
    let dir = temp_dir("execute");
    let old = create_session(&dir, "old transcript");
    let active = create_session(&dir, "active transcript");
    set_mtime(&old, SystemTime::now() - Duration::from_secs(40 * 86_400));
    // A checkpoint sidecar next to the old transcript must go with it.
    let sidecar = session_sidecar_paths(&old)
        .into_iter()
        .find(|path| path.to_string_lossy().ends_with(".checkpoints"))
        .expect("checkpoint sidecar");
    std::fs::create_dir_all(&sidecar).expect("sidecar dir");
    std::fs::write(sidecar.join("cp-1"), b"snapshot").expect("sidecar file");

    let policy = SessionRetentionPolicy {
        enabled: true,
        max_age_days: Some(30),
        max_sessions: None,
        max_bytes: None,
    };
    let report = prune_session_dir(&dir, &policy, Some(&active));
    assert_eq!(report.removed.len(), 1, "report: {report:?}");
    assert_eq!(report.removed[0].path, old);
    assert!(!old.exists(), "the expired transcript must be deleted");
    assert!(!sidecar.exists(), "the sidecar must be deleted");
    assert!(active.exists(), "the active session must survive");
    assert!(report.errors.is_empty(), "errors: {:?}", report.errors);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn non_session_jsonl_is_never_deleted() {
    let dir = temp_dir("invalid");
    let junk = dir.join("not-a-session.jsonl");
    std::fs::write(&junk, b"{\"hello\":\"world\"}\n").expect("junk file");
    let policy = SessionRetentionPolicy {
        enabled: true,
        max_age_days: None,
        max_sessions: Some(0),
        max_bytes: None,
    };
    let report = prune_session_dir(&dir, &policy, None);
    assert!(junk.exists(), "a non-session .jsonl must never be deleted");
    assert!(!report.errors.is_empty(), "the refusal must be reported");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn junk_jsonl_does_not_consume_max_sessions_quota() {
    let dir = temp_dir("junk-quota");
    let real = create_session(&dir, "real session");
    // Make the junk file the newest: with a header-blind scan it would win
    // the single slot and push the real (older) session into the plan.
    set_mtime(&real, SystemTime::now() - Duration::from_secs(2 * 86_400));
    let junk = dir.join("newest-junk.jsonl");
    std::fs::write(&junk, b"{\"hello\":\"world\"}\n").expect("junk file");

    let policy = SessionRetentionPolicy {
        enabled: true,
        max_age_days: None,
        max_sessions: Some(1),
        max_bytes: None,
    };
    let report = prune_session_dir(&dir, &policy, None);
    assert!(real.exists(), "the real session must survive: {report:?}");
    assert!(junk.exists(), "junk is never deleted: {report:?}");
    assert!(report.removed.is_empty(), "report: {report:?}");
    assert!(
        report
            .errors
            .iter()
            .any(|error| error.contains("non-session file")),
        "the ignored junk must stay diagnosable: {report:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn prune_skips_a_session_held_by_a_live_writer() {
    let dir = temp_dir("writer-lock");
    let path = create_session(&dir, "active transcript");
    // A second manager stands in for another process: it holds the advisory
    // `.<name>.lock` while the session is old enough for age pruning.
    let holder = SessionManager::open(&path).expect("hold session");
    let lock = lock_path(&path);
    // Requirement: liveness is pid-based, NOT lock-mtime based. Backdate the
    // lock far beyond the stale window while its owner is still alive.
    set_mtime(&lock, SystemTime::now() - Duration::from_secs(5 * 60 * 60));
    set_mtime(&path, SystemTime::now() - Duration::from_secs(40 * 86_400));

    let policy = SessionRetentionPolicy {
        enabled: true,
        max_age_days: Some(30),
        max_sessions: None,
        max_bytes: None,
    };
    let report = prune_session_dir(&dir, &policy, None);
    assert!(
        path.exists(),
        "a live writer must protect its session even with an old lock"
    );
    assert!(lock.exists(), "prune must not delete a live writer's lock");
    assert!(report.removed.is_empty(), "report: {report:?}");
    assert_eq!(report.skipped_active, 1, "report: {report:?}");

    drop(holder);
    set_mtime(&path, SystemTime::now() - Duration::from_secs(40 * 86_400));
    let report = prune_session_dir(&dir, &policy, None);
    assert!(
        !path.exists(),
        "after the lock is released it may be pruned"
    );
    assert_eq!(report.removed.len(), 1, "report: {report:?}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn create_branched_session_moves_the_writer_lock_to_the_branch() {
    let dir = temp_dir("branch-lock");
    let mut manager = SessionManager::create(".", &dir).expect("create session");
    let leaf = manager
        .append_message(user_message("branch source"))
        .expect("append source");
    let source = manager.path().to_path_buf();
    let source_lock = lock_path(&source);
    assert!(source_lock.exists(), "the source session must be locked");

    let branch = manager
        .create_branched_session(&leaf)
        .expect("branch")
        .expect("persisted branch path");
    let branch_lock = lock_path(&branch);
    assert!(
        branch_lock.exists(),
        "the branch must hold its own writer lock"
    );
    assert!(
        !source_lock.exists(),
        "the source lock must be released after the branch switch"
    );

    // A live manager whose branch has been idle for days still protects it:
    // both the transcript and the lock are far older than every window.
    set_mtime(
        &branch,
        SystemTime::now() - Duration::from_secs(40 * 86_400),
    );
    set_mtime(
        &branch_lock,
        SystemTime::now() - Duration::from_secs(5 * 60 * 60),
    );
    let policy = SessionRetentionPolicy {
        enabled: true,
        max_age_days: Some(30),
        max_sessions: None,
        max_bytes: None,
    };
    let report = prune_session_dir(&dir, &policy, None);
    assert!(
        branch.exists(),
        "a branch held by a live manager must survive: {report:?}"
    );
    assert_eq!(report.skipped_active, 1, "report: {report:?}");

    // Every append refreshes the lock mtime (the pid probe is primary, the
    // touch is the second layer for diagnostics / old binaries).
    manager
        .append_message(user_message("after branch"))
        .expect("append branch");
    let age = SystemTime::now()
        .duration_since(
            std::fs::metadata(&branch_lock)
                .expect("branch lock metadata")
                .modified()
                .expect("branch lock mtime"),
        )
        .expect("lock mtime in the past");
    assert!(
        age < Duration::from_secs(60),
        "append must refresh the lock mtime, age={age:?}"
    );
    drop(manager);
    assert!(!branch_lock.exists(), "Drop must release the branch lock");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_new_writer_reclaims_a_dead_pid_stale_lock() {
    let dir = temp_dir("dead-lock-reclaim");
    let path = create_session(&dir, "crashed writer");
    let lock = lock_path(&path);
    let (dead_pid, start_token) = exited_child_pid();
    std::fs::write(&lock, dead_lock_contents(dead_pid, start_token)).expect("fake stale lock");
    set_mtime(&lock, SystemTime::now() - Duration::from_secs(2 * 60 * 60));

    let manager = SessionManager::open(&path).expect("open must reclaim the stale lock");
    let content = std::fs::read_to_string(&lock).expect("new lock");
    assert!(
        !content.contains("dead-token"),
        "the dead writer's lock must be replaced: {content}"
    );
    assert!(
        content.contains(&format!("pid={}", std::process::id())),
        "the new lock must name this process: {content}"
    );
    drop(manager);
    assert!(!lock.exists(), "Drop must release the reclaimed lock");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_dead_pid_lock_is_reclaimed_only_after_the_stale_window() {
    let dir = temp_dir("dead-lock-stale");
    let path = create_session(&dir, "crashed writer");
    let lock = lock_path(&path);
    let (dead_pid, start_token) = exited_child_pid();
    std::fs::write(&lock, dead_lock_contents(dead_pid, start_token)).expect("fake stale lock");
    set_mtime(&path, SystemTime::now() - Duration::from_secs(40 * 86_400));
    let policy = SessionRetentionPolicy {
        enabled: true,
        max_age_days: Some(30),
        max_sessions: None,
        max_bytes: None,
    };

    // Dead pid but a young lock: conservatively keep everything this pass.
    let report = prune_session_dir(&dir, &policy, None);
    assert!(path.exists(), "young dead lock must not unblock deletion");
    assert_eq!(report.skipped_active, 1, "report: {report:?}");

    // Past the stale window the lock is reclaimed and the transcript pruned.
    set_mtime(&lock, SystemTime::now() - Duration::from_secs(2 * 60 * 60));
    let report = prune_session_dir(&dir, &policy, None);
    assert!(!path.exists(), "stale lock must not protect a dead session");
    assert!(
        !lock.exists(),
        "the stale lock must be removed with the session"
    );
    assert_eq!(report.removed.len(), 1, "report: {report:?}");
    assert!(report.errors.is_empty(), "errors: {:?}", report.errors);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_lock_from_another_host_is_never_reclaimed_locally() {
    let dir = temp_dir("foreign-host-lock");
    let path = create_session(&dir, "foreign host writer");
    let lock = lock_path(&path);
    let (dead_pid, start_token) = exited_child_pid();
    let mut content = format!(
        "pid={dead_pid}\ntoken=foreign-token\nstarted=1\nhost=definitely-another-host-{}\n",
        std::process::id()
    );
    if let Some(start_token) = start_token {
        content.push_str(&format!("start={start_token}\n"));
    }
    std::fs::write(&lock, content).expect("foreign host lock");
    set_mtime(&lock, SystemTime::now() - Duration::from_secs(5 * 60 * 60));
    set_mtime(&path, SystemTime::now() - Duration::from_secs(40 * 86_400));
    let policy = SessionRetentionPolicy {
        enabled: true,
        max_age_days: Some(30),
        max_sessions: None,
        max_bytes: None,
    };

    let report = prune_session_dir(&dir, &policy, None);
    assert!(
        path.exists(),
        "a foreign host's pid cannot be probed locally; keep the session: {report:?}"
    );
    assert!(lock.exists(), "the foreign lock must stay untouched");
    assert_eq!(report.skipped_active, 1, "report: {report:?}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_foreign_live_process_with_an_old_lock_blocks_pruning() {
    let dir = temp_dir("cross-process-live");
    let path = create_session(&dir, "cross-process writer");
    let lock = lock_path(&path);
    let mut child = if cfg!(windows) {
        std::process::Command::new("ping")
            .args(["-n", "30", "127.0.0.1"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
    } else {
        std::process::Command::new("sleep").arg("30").spawn()
    }
    .expect("spawn long-lived child");
    std::fs::write(
        &lock,
        format!("pid={}\ntoken=live-child\nstarted=1\n", child.id()),
    )
    .expect("foreign lock");
    set_mtime(&lock, SystemTime::now() - Duration::from_secs(5 * 60 * 60));
    set_mtime(&path, SystemTime::now() - Duration::from_secs(40 * 86_400));
    let policy = SessionRetentionPolicy {
        enabled: true,
        max_age_days: Some(30),
        max_sessions: None,
        max_bytes: None,
    };

    let report = prune_session_dir(&dir, &policy, None);
    assert!(
        path.exists(),
        "a live process on this host must protect the session: {report:?}"
    );
    assert_eq!(report.skipped_active, 1, "report: {report:?}");

    let _ = child.kill();
    let _ = child.wait();
    set_mtime(&lock, SystemTime::now() - Duration::from_secs(5 * 60 * 60));
    let report = prune_session_dir(&dir, &policy, None);
    assert!(
        !path.exists(),
        "after the foreign process exits its old lock is reclaimable: {report:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Child-process half of [`a_session_manager_in_another_process_holds_the_lock`].
/// Ignored in the normal suite; the parent re-executes this test binary with
/// `--ignored --exact` and points it at the session to hold.
#[test]
#[ignore = "helper process for the cross-process SessionManager lock test"]
fn cross_process_lock_holder() {
    let Some(session) = std::env::var_os("PIR_TEST_HOLDER_SESSION") else {
        return;
    };
    let Some(marker) = std::env::var_os("PIR_TEST_HOLDER_MARKER") else {
        return;
    };
    let _manager = SessionManager::open(PathBuf::from(session)).expect("child opens the session");
    std::fs::write(marker, b"ready").expect("write holder-ready marker");
    std::thread::sleep(Duration::from_secs(60));
}

#[test]
fn a_session_manager_in_another_process_holds_the_lock() {
    let dir = temp_dir("cross-process-manager");
    let path = create_session(&dir, "held by another SessionManager");
    let marker = dir.join("holder-ready");
    let mut child = std::process::Command::new(std::env::current_exe().expect("test binary"))
        .args(["cross_process_lock_holder", "--ignored", "--test-threads=1"])
        .env("PIR_TEST_HOLDER_SESSION", &path)
        .env("PIR_TEST_HOLDER_MARKER", &marker)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn holder test process");
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    while !marker.exists() {
        if let Some(status) = child.try_wait().expect("poll holder") {
            panic!("holder process exited before ready: {status}");
        }
        assert!(
            std::time::Instant::now() < deadline,
            "holder did not become ready in time"
        );
        std::thread::sleep(Duration::from_millis(25));
    }
    let lock = lock_path(&path);
    let content = std::fs::read_to_string(&lock).expect("child writer lock");
    assert!(
        content.contains(&format!("pid={}", child.id())),
        "the lock must belong to the child process: {content}"
    );

    // The child is alive but idle; both its lock and the transcript are far
    // older than every stale/safety window.
    set_mtime(&lock, SystemTime::now() - Duration::from_secs(5 * 60 * 60));
    set_mtime(&path, SystemTime::now() - Duration::from_secs(40 * 86_400));
    let policy = SessionRetentionPolicy {
        enabled: true,
        max_age_days: Some(30),
        max_sessions: None,
        max_bytes: None,
    };
    let report = prune_session_dir(&dir, &policy, None);
    assert!(
        path.exists(),
        "a SessionManager in another process must protect its session: {report:?}"
    );
    assert_eq!(report.skipped_active, 1, "report: {report:?}");

    let _ = child.kill();
    let _ = child.wait();
    // A killed process leaves its lock behind (no Drop); the stale reclaim
    // path then takes over.
    set_mtime(&lock, SystemTime::now() - Duration::from_secs(5 * 60 * 60));
    let report = prune_session_dir(&dir, &policy, None);
    assert!(
        !path.exists(),
        "after the writer process dies its stale lock is reclaimable: {report:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn fresh_files_are_skipped_by_the_safety_window() {
    let dir = temp_dir("safety-window");
    let path = create_session(&dir, "just written");
    let policy = SessionRetentionPolicy {
        enabled: true,
        max_age_days: None,
        max_sessions: Some(0),
        max_bytes: None,
    };
    let report = prune_session_dir(&dir, &policy, None);
    assert!(path.exists(), "a just-written file must survive");
    assert_eq!(report.skipped_active, 1, "report: {report:?}");
    assert!(report.removed.is_empty(), "report: {report:?}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn disabled_retention_does_not_delete_on_startup() {
    let agent = temp_dir("cli-agent-disabled");
    let sessions = temp_dir("cli-sessions-disabled");
    std::fs::write(
        agent.join("settings.json"),
        br#"{"sessionRetention":{"enabled":false,"maxAgeDays":30}}"#,
    )
    .expect("settings");
    let old = create_session(&sessions, "expired transcript");
    set_mtime(&old, SystemTime::now() - Duration::from_secs(40 * 86_400));

    let output = std::process::Command::new(env!("CARGO_BIN_EXE_pir"))
        .args([
            "--mode",
            "json",
            "--session-dir",
            sessions.to_str().expect("utf8 session dir"),
        ])
        .env("PIR_AGENT_DIR", &agent)
        .env("HOME", &agent)
        .env("USERPROFILE", &agent)
        .current_dir(&agent)
        .output()
        .expect("pir binary should start");
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        old.exists(),
        "disabled retention must not delete any session on startup"
    );
    let _ = std::fs::remove_dir_all(&agent);
    let _ = std::fs::remove_dir_all(&sessions);
}

#[test]
fn sessions_prune_dry_run_reports_without_deleting() {
    let agent = temp_dir("cli-agent-dry-run");
    let sessions = temp_dir("cli-sessions-dry-run");
    let old = create_session(&sessions, "expired transcript");
    set_mtime(&old, SystemTime::now() - Duration::from_secs(40 * 86_400));

    let output = std::process::Command::new(env!("CARGO_BIN_EXE_pir"))
        .args([
            "sessions",
            "prune",
            "--dry-run",
            "--session-dir",
            sessions.to_str().expect("utf8 session dir"),
        ])
        .env("PIR_AGENT_DIR", &agent)
        .env("HOME", &agent)
        .env("USERPROFILE", &agent)
        .current_dir(&agent)
        .output()
        .expect("pir binary should start");
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("dry run:") && stdout.contains("removed"),
        "dry-run must report the planned deletion: {stdout}"
    );
    assert!(old.exists(), "dry run must never delete a session");
    let _ = std::fs::remove_dir_all(&agent);
    let _ = std::fs::remove_dir_all(&sessions);
}

#[test]
fn startup_retention_deletes_expired_sessions_when_enabled() {
    let agent = temp_dir("cli-agent");
    let sessions = temp_dir("cli-sessions");
    std::fs::write(
        agent.join("settings.json"),
        br#"{"sessionRetention":{"enabled":true,"maxAgeDays":30}}"#,
    )
    .expect("settings");
    let old = create_session(&sessions, "expired transcript");
    set_mtime(&old, SystemTime::now() - Duration::from_secs(40 * 86_400));
    let fresh = create_session(&sessions, "fresh transcript");

    let output = std::process::Command::new(env!("CARGO_BIN_EXE_pir"))
        .args([
            "--mode",
            "json",
            "--session-dir",
            sessions.to_str().expect("utf8 session dir"),
        ])
        .env("PIR_AGENT_DIR", &agent)
        .env("HOME", &agent)
        .env("USERPROFILE", &agent)
        .current_dir(&agent)
        .output()
        .expect("pir binary should start");
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        !old.exists(),
        "startup retention must delete the expired session"
    );
    assert!(
        fresh.exists(),
        "fresh sessions must survive startup retention"
    );
    let _ = std::fs::remove_dir_all(&agent);
    let _ = std::fs::remove_dir_all(&sessions);
}

#[test]
fn all_prune_uses_only_the_known_sessions_root() {
    let agent_dir = temp_dir("all-root");
    let sessions_root = agent_dir.join("sessions");
    std::fs::create_dir_all(&sessions_root).expect("sessions root");

    // The default layout `<sessions-root>/<encoded-cwd>` keeps resolving to
    // the sessions root, so `--all` behavior is unchanged there.
    let default_dir = sessions_root.join("--work--");
    assert_eq!(
        resolve_all_prune_root(&default_dir, &sessions_root).expect("default layout"),
        sessions_root
    );

    // A deep custom `--session-dir` outside the sessions root must be
    // refused instead of pruning its parent.
    let deep = agent_dir
        .join("custom")
        .join("nested")
        .join("project")
        .join("sessions");
    std::fs::create_dir_all(&deep).expect("deep custom session dir");
    let error = resolve_all_prune_root(&deep, &sessions_root).expect_err("must refuse");
    assert!(error.contains("outside the sessions root"), "{error}");
    assert!(error.contains("without `--all`"), "{error}");

    // `..` must not smuggle a sibling outside the root back under it, and a
    // sibling whose name merely starts with "sessions" is not the root.
    let escaped = sessions_root
        .join("--work--")
        .join("..")
        .join("..")
        .join("evil");
    assert!(resolve_all_prune_root(&escaped, &sessions_root).is_err());
    let sibling = agent_dir.join("sessions-other").join("--work--");
    assert!(resolve_all_prune_root(&sibling, &sessions_root).is_err());

    let _ = std::fs::remove_dir_all(&agent_dir);
}

#[test]
fn all_prune_accepts_the_sessions_root_itself() {
    let root = temp_dir("all-root-self");
    assert_eq!(
        resolve_all_prune_root(&root, &root).expect("sessions root"),
        root
    );
    let _ = std::fs::remove_dir_all(&root);
}
