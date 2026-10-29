//! §7 #3: rename/delete in the tab-style `/sessions` picker.
//!
//! The upstream session selector owns three management ids
//! (`app.session.rename`, `app.session.delete`,
//! `app.session.deleteNoninvasive`). These tests pin the picker's dispatch
//! decisions (including the non-deletable rows and the ctrl+backspace
//! filter-vs-delete rule) and the safe rename/delete operations: renames only
//! append a `session_info` entry through the session manager, deletes validate
//! the target as a pi session first, and failures are reported verbatim.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use pir::ai::types::{ContentBlock, Message, TextContent, UserMessage, UserRole};
use pir::coding_agent::session_pick::{
    delete_session, rename_session, stored_session_name, SessionDeleteMethod,
};
use pir::session_manager::SessionManager;
use pir::tui::terminal::{
    session_manage_decision, SessionManageDecision, SessionManageTarget, SessionsKeyAction,
};

static NEXT_DIR: AtomicU64 = AtomicU64::new(0);

fn temp_root(tag: &str) -> PathBuf {
    let unique = NEXT_DIR.fetch_add(1, Ordering::Relaxed);
    let root = std::env::temp_dir().join(format!(
        "pir-tui-session-manage-{}-{tag}-{unique}",
        std::process::id()
    ));
    std::fs::create_dir_all(&root).expect("create temp root");
    root
}

/// A valid persisted session file (header + one message + a seed name).
fn session_fixture(tag: &str) -> (PathBuf, PathBuf, PathBuf) {
    let root = temp_root(tag);
    let session_dir = root.join("sessions");
    let mut manager =
        SessionManager::create(&root.to_string_lossy(), &session_dir).expect("create session");
    manager
        .append_message(Message::User(UserMessage {
            role: UserRole::User,
            content: vec![ContentBlock::Text(TextContent {
                text: "hello".to_string(),
                text_signature: None,
            })],
            tools_added: None,
            tools_removed: None,
            timestamp: pir::ai::util::now_ms(),
            sections: None,
        }))
        .expect("materialize the session file");
    manager
        .append_session_info("seed")
        .expect("seed the display name");
    let path = manager
        .session_file()
        .expect("persisted session has a file")
        .to_path_buf();
    drop(manager);
    (root, session_dir, path)
}

#[test]
fn picker_rename_appends_a_name_and_keeps_the_session_valid() {
    let (root, session_dir, path) = session_fixture("rename");
    let before = std::fs::read(&path).expect("read session");

    rename_session(&path, "  my session  ").expect("rename appends a session_info entry");

    assert_eq!(
        stored_session_name(&path).as_deref(),
        Some("my session"),
        "the picker prefill/readback sees the trimmed name"
    );
    let manager = SessionManager::open(&path).expect("renamed session still opens");
    assert_eq!(manager.session_name().as_deref(), Some("my session"));
    let summaries = SessionManager::list_sessions(&session_dir, None);
    assert!(
        summaries
            .iter()
            .any(|summary| summary.path == path && summary.name.as_deref() == Some("my session")),
        "the session list reflects the rename"
    );
    let after = std::fs::read(&path).expect("read session after rename");
    assert!(
        after.starts_with(&before),
        "a rename is append-only and must never rewrite the stored transcript"
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn picker_rename_blank_is_the_upstream_no_op() {
    let (root, _session_dir, path) = session_fixture("rename-blank");
    let before = std::fs::read(&path).expect("read session");

    rename_session(&path, "   ").expect("a blank rename is a no-op");

    assert_eq!(std::fs::read(&path).expect("read session"), before);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn picker_rename_reports_missing_and_non_session_targets() {
    let (root, _session_dir, _path) = session_fixture("rename-errors");

    let missing = root.join("missing.jsonl");
    let error = rename_session(&missing, "name").expect_err("missing files cannot be renamed");
    assert!(error.contains("not found"), "{error}");

    let plain = root.join("plain.jsonl");
    std::fs::write(&plain, "this is not a pi session\n").expect("write plain file");
    let error = rename_session(&plain, "name").expect_err("non-session files are refused");
    assert!(error.contains("cannot read session file"), "{error}");
    assert_eq!(
        std::fs::read_to_string(&plain).expect("plain file survives"),
        "this is not a pi session\n"
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn picker_delete_removes_a_validated_session_file() {
    let (root, session_dir, path) = session_fixture("delete");

    let method = delete_session(&path).expect("delete a valid session");
    assert!(matches!(
        method,
        SessionDeleteMethod::Trash | SessionDeleteMethod::Unlink
    ));
    assert!(!path.exists(), "the session file is gone");
    assert!(
        !SessionManager::list_sessions(&session_dir, None)
            .iter()
            .any(|summary| summary.path == path),
        "a deleted session no longer appears in the picker source"
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn picker_delete_refuses_missing_and_non_session_files() {
    let (root, _session_dir, _path) = session_fixture("delete-errors");

    let missing = root.join("missing.jsonl");
    let error = delete_session(&missing).expect_err("missing files are refused");
    assert!(error.contains("not found"), "{error}");

    let plain = root.join("plain.jsonl");
    std::fs::write(&plain, "this is not a pi session\n").expect("write plain file");
    let error = delete_session(&plain).expect_err("non-session files are refused");
    assert!(error.contains("not a pi session file"), "{error}");
    assert!(plain.exists(), "the refused file is left untouched");
    assert_eq!(
        std::fs::read_to_string(&plain).expect("plain file survives"),
        "this is not a pi session\n"
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn session_manage_decision_dispatches_rename_delete_and_noninvasive() {
    let path = PathBuf::from("/sessions/a.jsonl");
    let file = SessionManageTarget::File(path.clone());

    // Rename / delete on a persisted row dispatch their flows.
    assert_eq!(
        session_manage_decision(SessionsKeyAction::Rename, Some(&file), None, true),
        SessionManageDecision::Rename(path.clone())
    );
    assert_eq!(
        session_manage_decision(SessionsKeyAction::Delete, Some(&file), None, true),
        SessionManageDecision::Delete(path.clone())
    );
    // ctrl+backspace deletes only with an empty query; otherwise it is
    // ordinary filter deletion.
    assert_eq!(
        session_manage_decision(
            SessionsKeyAction::DeleteNoninvasive,
            Some(&file),
            None,
            true
        ),
        SessionManageDecision::Delete(path.clone())
    );
    assert_eq!(
        session_manage_decision(
            SessionsKeyAction::DeleteNoninvasive,
            Some(&file),
            None,
            false
        ),
        SessionManageDecision::ClearQuery
    );

    // The `+ New session` action row is not a session file.
    let new_row = SessionManageTarget::NewSession;
    let rename_error =
        session_manage_decision(SessionsKeyAction::Rename, Some(&new_row), None, true);
    assert!(
        matches!(
            rename_error,
            SessionManageDecision::Unavailable(message) if message.contains("cannot be renamed")
        ),
        "{rename_error:?}"
    );
    let delete_error =
        session_manage_decision(SessionsKeyAction::Delete, Some(&new_row), None, true);
    assert!(
        matches!(
            delete_error,
            SessionManageDecision::Unavailable(message) if message.contains("cannot be deleted")
        ),
        "{delete_error:?}"
    );

    // The active session's file is open by the live writer: deletion is
    // refused with an actionable message (rename stays allowed).
    assert_eq!(
        session_manage_decision(
            SessionsKeyAction::Delete,
            Some(&file),
            Some("/sessions/a.jsonl"),
            true
        ),
        SessionManageDecision::Unavailable(
            "The active session cannot be deleted; switch to another tab first"
        )
    );
    assert_eq!(
        session_manage_decision(
            SessionsKeyAction::Rename,
            Some(&file),
            Some("/sessions/a.jsonl"),
            true
        ),
        SessionManageDecision::Rename(path)
    );

    // No row selected.
    assert_eq!(
        session_manage_decision(SessionsKeyAction::Delete, None, None, true),
        SessionManageDecision::Unavailable("No session selected")
    );

    // Display toggles never enter the mutation path.
    for action in [
        SessionsKeyAction::ToggleSort,
        SessionsKeyAction::ToggleNamedFilter,
        SessionsKeyAction::TogglePath,
    ] {
        assert_eq!(
            session_manage_decision(action, Some(&file), None, true),
            SessionManageDecision::None
        );
    }
}

#[test]
fn session_delete_method_status_messages_match_upstream() {
    assert_eq!(
        SessionDeleteMethod::Trash.status_message(),
        "Session moved to trash"
    );
    assert_eq!(
        SessionDeleteMethod::Unlink.status_message(),
        "Session deleted"
    );
}
