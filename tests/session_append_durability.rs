//! Session append persistence (P4/L8).
//!
//! Appends must be complete and readable by a fresh open after EVERY entry
//! without an intervening `sync_all`: the reusable append handle only defers
//! the durability barrier to `SessionManager::sync_now` (turn end) / `Drop`
//! (session close). Rewrites and path switches replace the file/path, so the
//! cached handle must be invalidated or later appends would vanish into the
//! superseded file.

use std::path::{Path, PathBuf};

use pir::ai::types::{ContentBlock, Message, TextContent, UserMessage, UserRole};
use pir::session_manager::SessionManager;

fn temp_dir(name: &str) -> PathBuf {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("pir-session-append-{name}-{stamp}"));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

struct TempDirGuard(PathBuf);

impl Drop for TempDirGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn guard_temp_dir(name: &str) -> TempDirGuard {
    TempDirGuard(temp_dir(name))
}

fn user_message(text: &str) -> Message {
    Message::User(UserMessage {
        role: UserRole::User,
        content: vec![ContentBlock::Text(TextContent {
            text: text.into(),
            text_signature: None,
        })],
        tools_added: None,
        tools_removed: None,
        timestamp: pir::ai::util::now_ms(),
        sections: None,
    })
}

fn lines_of(path: &Path) -> Vec<serde_json::Value> {
    let content = std::fs::read_to_string(path).unwrap();
    content
        .lines()
        .map(|line| serde_json::from_str(line).expect("every line is complete JSON"))
        .collect()
}

fn last_user_text(lines: &[serde_json::Value]) -> &str {
    lines
        .last()
        .and_then(|line| line.get("message"))
        .and_then(|message| message.get("content"))
        .and_then(|content| content.get(0))
        .and_then(|block| block.get("text"))
        .and_then(serde_json::Value::as_str)
        .expect("the last entry is a user message")
}

#[test]
fn every_append_is_a_complete_line_without_an_intervening_sync() {
    let dir = guard_temp_dir("visible");
    let mut session = SessionManager::create("C:/repo", &dir.0).unwrap();
    let path = session.path().to_path_buf();
    const APPENDS: usize = 64;
    for index in 0..APPENDS {
        session
            .append_message(user_message(&format!("prompt {index}")))
            .unwrap();
    }

    // A fresh open (the writer is still alive; no `sync_now`, no Drop) must
    // see every append as one complete JSONL line.
    let content = std::fs::read_to_string(&path).unwrap();
    assert!(
        content.ends_with('\n'),
        "the file must end on a completed line: {:?}",
        &content[content.len().saturating_sub(40)..]
    );
    let lines = lines_of(&path);
    assert_eq!(lines.len(), APPENDS + 1, "header + one line per append");
    assert_eq!(lines[0]["type"], "session");
    assert_eq!(last_user_text(&lines), format!("prompt {}", APPENDS - 1));

    // A second manager opening the same file while the append handle is open
    // sees the same transcript (the handle shares read/write on Windows).
    let reopened = SessionManager::open(&path).unwrap();
    assert_eq!(reopened.entries().len(), APPENDS);
    drop(reopened);

    // The explicit turn/session boundary still succeeds (and is idempotent;
    // no handle is opened for a manager that never appended).
    session.sync_now().unwrap();
    session.sync_now().unwrap();
    drop(session);
}

#[test]
fn rewrite_and_path_switch_invalidate_the_cached_append_handle() {
    let dir = guard_temp_dir("rewrite");
    let mut session = SessionManager::create("C:/repo", &dir.0).unwrap();
    let source_id = session.append_message(user_message("source")).unwrap();
    let source_path = session.path().to_path_buf();
    // The append handle is now cached on `source_path`.

    // The branch rewrite writes a DIFFERENT file and then appends go there;
    // a live handle on the source file would silently swallow them.
    let branch_path = session
        .create_branched_session(&source_id)
        .unwrap()
        .expect("a persisted session branches to a new file");
    assert_ne!(branch_path, source_path);
    session
        .append_message(user_message("after branch"))
        .unwrap();

    let branch_lines = lines_of(&branch_path);
    assert_eq!(branch_lines.len(), 3, "header + copied path + append");
    assert_eq!(last_user_text(&branch_lines), "after branch");
    let source_lines = lines_of(&source_path);
    assert_eq!(
        source_lines.len(),
        2,
        "the post-branch append never reaches the superseded source file"
    );

    // `set_session_file` re-points the manager at an existing file: the next
    // append reopens it in append mode and lands after the existing lines.
    session.set_session_file(&branch_path, None).unwrap();
    session
        .append_message(user_message("after re-open"))
        .unwrap();
    let branch_lines = lines_of(&branch_path);
    assert_eq!(branch_lines.len(), 4);
    assert_eq!(last_user_text(&branch_lines), "after re-open");
    drop(session);
}

#[test]
fn a_failed_append_rolls_back_memory_and_disk_and_the_next_append_stays_parented() {
    let dir = guard_temp_dir("failure-rollback");
    let mut session = SessionManager::create("C:/repo", &dir.0).unwrap();
    let kept_id = session
        .append_message(user_message("kept"))
        .expect("first append");
    let path = session.path().to_path_buf();
    let before = std::fs::read_to_string(&path).unwrap();

    // A short write (10 bytes of the new line) must be truncated back to the
    // previous line boundary, and the entry must not join the in-memory tree:
    // the failed entry must never become the leaf a later append parents to.
    session.fail_next_append_for_test(pir::session_manager::AppendFailure::PartialWrite(10));
    let error = session
        .append_message(user_message("dropped"))
        .expect_err("an injected short write must fail the append");
    assert!(
        error
            .to_string()
            .contains("injected session append failure"),
        "unexpected error: {error}"
    );
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        before,
        "a partial line must not survive the failed append"
    );
    assert_eq!(
        session.leaf_id(),
        Some(kept_id.as_str()),
        "the failed entry never became the leaf"
    );
    assert_eq!(
        session.entries().len(),
        1,
        "the failed entry must not be in memory"
    );

    // A failure before any byte is written takes the same rollback path.
    session.fail_next_append_for_test(pir::session_manager::AppendFailure::BeforeWrite);
    session
        .append_message(user_message("dropped too"))
        .expect_err("an injected pre-write failure must fail the append");
    assert_eq!(std::fs::read_to_string(&path).unwrap(), before);
    assert_eq!(session.leaf_id(), Some(kept_id.as_str()));
    assert_eq!(session.entries().len(), 1);

    // The retry is correctly parented: the leaf is the last PERSISTED entry,
    // not a phantom from either failed append.
    let after_id = session
        .append_message(user_message("after"))
        .expect("append after the failures");
    let lines = lines_of(&path);
    assert_eq!(lines.len(), 3, "header + kept + after");
    assert_eq!(lines[1]["id"], kept_id);
    assert_eq!(lines[2]["id"], after_id);
    assert_eq!(
        lines[2]["parentId"], kept_id,
        "the recovered append parents against the persisted leaf"
    );

    // A fresh open preserves the full prior transcript and the complete
    // parent chain (no truncation at a missing parent).
    let reopened = SessionManager::open(&path).unwrap();
    assert_eq!(reopened.leaf_id(), Some(after_id.as_str()));
    assert_eq!(reopened.entries().len(), 2, "both persisted messages");
    assert_eq!(
        reopened.branch_entries(None).unwrap().len(),
        2,
        "the leaf path carries both messages"
    );
    assert_eq!(last_user_text(&lines), "after");
    drop(reopened);
    drop(session);
}

#[test]
fn an_externally_deleted_session_file_still_fails_the_append() {
    let dir = guard_temp_dir("delete");
    let mut session = SessionManager::create("C:/repo", &dir.0).unwrap();
    session.append_message(user_message("kept")).unwrap();
    let path = session.path().to_path_buf();

    // The cached handle must not hide a deleted session file: the append has
    // to fail (the old per-append open failed with ENOENT), not write into
    // the unlinked inode and report success.
    std::fs::remove_file(&path).unwrap();
    let error = session
        .append_message(user_message("lost"))
        .expect_err("an append to a deleted session file must fail");
    assert!(
        error.to_string().contains("cannot append to session"),
        "unexpected error: {error}"
    );

    // Recreating the path lets the next append reopen it (the stale handle
    // was dropped, never written through).
    std::fs::write(&path, "").unwrap();
    session.append_message(user_message("recovered")).unwrap();
    let lines = lines_of(&path);
    assert_eq!(lines.len(), 1);
    assert_eq!(last_user_text(&lines), "recovered");
    drop(session);
}
