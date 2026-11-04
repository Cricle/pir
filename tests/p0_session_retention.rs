//! P0-4: session retention boundaries, active-session protection, safe
//! deletion (sidecars + non-session refusal), and empty-directory behavior.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use pir::ai::types::*;
use pir::session_manager::retention::{
    plan_session_prune, prune_session_dir, scan_session_files, session_sidecar_paths, PruneReason,
    SessionFileInfo, SessionRetentionPolicy,
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
            .all(|entry| entry.path != PathBuf::from("active.jsonl")),
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
