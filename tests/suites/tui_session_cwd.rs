//! §6 #16: the TUI's missing-session-cwd prompt.
//!
//! Upstream offers "Continue in current cwd" / "Cancel" when a resumed
//! session's stored cwd no longer exists (`session-cwd.ts` +
//! `interactive-mode.ts:promptForMissingSessionCwd`). These tests pin the
//! prompt text, the key resolution, and the two outcomes: Continue reopens the
//! session with the current cwd as the override without rewriting the file;
//! Cancel leaves the file byte-for-byte untouched; an existing cwd never
//! prompts.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crossterm::event::{KeyCode, KeyModifiers};

use pir::ai::types::{ContentBlock, Message, TextContent, UserMessage, UserRole};
use pir::session_manager::{MissingSessionCwd, SessionManager};
use pir::tui::session_cwd::{
    detect_missing_session_cwd, missing_cwd_override, missing_cwd_prompt, missing_cwd_prompt_step,
    open_session_in_current_cwd, MissingCwdChoice, MissingCwdPromptStep,
};

static NEXT_DIR: AtomicU64 = AtomicU64::new(0);

fn temp_root(tag: &str) -> PathBuf {
    let unique = NEXT_DIR.fetch_add(1, Ordering::Relaxed);
    let root = std::env::temp_dir().join(format!(
        "pir-tui-session-cwd-{}-{tag}-{unique}",
        std::process::id()
    ));
    std::fs::create_dir_all(&root).expect("create temp root");
    root
}

/// Create a persisted session whose header cwd is `cwd` and return its path
/// plus the exact bytes written. A user message materializes the file (the
/// manager writes lazily on the first conversation entry).
fn create_session(cwd: &Path, session_dir: &Path) -> (PathBuf, Vec<u8>) {
    let mut manager = SessionManager::create(&cwd.to_string_lossy(), session_dir)
        .expect("create session manager");
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
    let path = manager
        .session_file()
        .expect("a persisted session has a file")
        .to_path_buf();
    drop(manager);
    let bytes = std::fs::read(&path).expect("read session file");
    (path, bytes)
}

#[test]
fn missing_cwd_prompt_matches_the_upstream_continue_cancel_choice() {
    let issue = MissingSessionCwd {
        session_file: PathBuf::from("/sessions/x.jsonl"),
        session_cwd: "/project/gone".to_string(),
    };
    let prompt = missing_cwd_prompt(&issue, "/current");
    assert_eq!(prompt.title, "Session cwd not found");
    assert_eq!(prompt.options, vec!["Continue", "Cancel"]);
    assert_eq!(prompt.default_selection(), 0);
    // Upstream `formatMissingSessionCwdPrompt` wording.
    assert!(prompt
        .message
        .contains("cwd from session file does not exist"));
    assert!(prompt.message.contains("/project/gone"));
    assert!(prompt.message.contains("continue in current cwd"));
    assert!(prompt.message.contains("/current"));

    // Enter on the preselected row continues in the current cwd.
    assert_eq!(
        missing_cwd_prompt_step(KeyCode::Enter, KeyModifiers::NONE, 0, 2),
        MissingCwdPromptStep::Choose(MissingCwdChoice::Continue)
    );
    assert_eq!(
        missing_cwd_override(MissingCwdChoice::Continue, "/current"),
        Some("/current".to_string())
    );

    // Down highlights Cancel; Enter cancels.
    assert_eq!(
        missing_cwd_prompt_step(KeyCode::Down, KeyModifiers::NONE, 0, 2),
        MissingCwdPromptStep::Move(1)
    );
    assert_eq!(
        missing_cwd_prompt_step(KeyCode::Enter, KeyModifiers::NONE, 1, 2),
        MissingCwdPromptStep::Choose(MissingCwdChoice::Cancel)
    );
    assert_eq!(
        missing_cwd_override(MissingCwdChoice::Cancel, "/current"),
        None
    );

    // Esc and Ctrl+C cancel from any highlighted row.
    assert_eq!(
        missing_cwd_prompt_step(KeyCode::Esc, KeyModifiers::NONE, 0, 2),
        MissingCwdPromptStep::Choose(MissingCwdChoice::Cancel)
    );
    assert_eq!(
        missing_cwd_prompt_step(KeyCode::Char('c'), KeyModifiers::CONTROL, 0, 2),
        MissingCwdPromptStep::Choose(MissingCwdChoice::Cancel)
    );
    assert_eq!(
        missing_cwd_prompt_step(KeyCode::Char('x'), KeyModifiers::NONE, 0, 2),
        MissingCwdPromptStep::Ignore
    );
}

#[test]
fn existing_session_cwd_needs_no_prompt() {
    let root = temp_root("existing");
    let session_dir = root.join("sessions");
    let work = root.join("work");
    std::fs::create_dir_all(&work).expect("create work dir");
    let (path, _) = create_session(&work, &session_dir);

    assert_eq!(
        detect_missing_session_cwd(&path),
        None,
        "an existing stored cwd must not prompt"
    );
    let manager = SessionManager::open(&path).expect("open session");
    assert!(manager.missing_session_cwd().is_none());

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn vanished_session_cwd_is_detected_and_cancel_keeps_the_file_untouched() {
    let root = temp_root("vanished");
    let session_dir = root.join("sessions");
    let gone = root.join("gone");
    let (path, before) = create_session(&gone, &session_dir);

    let issue = detect_missing_session_cwd(&path).expect("detect the vanished cwd");
    assert_eq!(issue.session_file, path);
    assert_eq!(Path::new(&issue.session_cwd), gone.as_path());

    // Cancel: no cwd override, and the picker never opens/writes the file.
    let step = missing_cwd_prompt_step(KeyCode::Esc, KeyModifiers::NONE, 0, 2);
    let MissingCwdPromptStep::Choose(choice) = step else {
        panic!("Esc must cancel the missing-cwd prompt: {step:?}");
    };
    assert_eq!(missing_cwd_override(choice, "/current"), None);
    assert_eq!(
        std::fs::read(&path).expect("read session file"),
        before,
        "a cancelled resume must not rewrite the session"
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn continue_reopens_in_current_cwd_without_rewriting_the_session() {
    let root = temp_root("continue");
    let session_dir = root.join("sessions");
    let gone = root.join("gone");
    let current = root.join("current");
    std::fs::create_dir_all(&current).expect("create current cwd");
    let (path, before) = create_session(&gone, &session_dir);

    let mut manager = open_session_in_current_cwd(&path, &current.to_string_lossy())
        .expect("reopen with the current cwd override");
    assert!(
        manager.missing_session_cwd().is_none(),
        "the override cwd exists, so the resumed manager is healthy"
    );
    assert_eq!(Path::new(manager.cwd()), current.as_path());
    // The append-only log still accepts entries after the recovery.
    manager
        .append_session_info("after continue")
        .expect("append after a cwd recovery");
    drop(manager);

    let summaries = SessionManager::list_sessions(&session_dir, None);
    let summary = summaries
        .iter()
        .find(|summary| summary.path == path)
        .expect("the resumed session is still listed");
    assert_eq!(summary.name.as_deref(), Some("after continue"));

    // The stored header stays the same: only the manager's cwd override moved.
    let after = std::fs::read(&path).expect("read session file");
    assert!(
        after.starts_with(&before),
        "the recovery must append, never rewrite the stored header"
    );

    let _ = std::fs::remove_dir_all(&root);
}
