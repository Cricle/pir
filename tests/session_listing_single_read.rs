//! CPU5: session listing reads each file once and filters by cwd in memory.
//!
//! The pre-fix listing opened every session file twice: a bounded header scan
//! (up to 1 MiB) and then a second open/stream for the discovery peek. The
//! `#[doc(hidden)]` `session_list_file_reads` counter is the test seam: one
//! open per `.jsonl` file per listing. The filtered listing must agree with
//! filtering the full listing in memory (the `/resume` path lists once and
//! filters `SessionSummary.cwd`; see `SessionManager::filter_sessions_by_cwd`).

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use pir::ai::types::{ContentBlock, Message, TextContent, UserMessage, UserRole};
use pir::session_manager::{
    reset_session_list_file_reads, session_list_file_reads, SessionManager, SessionSummary,
};

struct TempDir(PathBuf);

impl TempDir {
    fn new(label: &str) -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or(0);
        let path = std::env::temp_dir().join(format!(
            "pir-session-listing-{label}-{}-{nanos}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("create session fixture dir");
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
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

fn create_session(dir: &Path, cwd: &str, text: &str) -> String {
    let mut session = SessionManager::create(cwd, dir).expect("create session");
    session
        .append_message(user_message(text))
        .expect("append user message");
    session.id().to_owned()
}

fn ids(sessions: &[SessionSummary]) -> Vec<String> {
    sessions.iter().map(|summary| summary.id.clone()).collect()
}

#[test]
fn listing_opens_each_file_once_and_filtered_agrees_with_in_memory_filter() {
    let dir = TempDir::new("single-read");
    let first = create_session(dir.path(), "C:\\proj", "first");
    std::thread::sleep(std::time::Duration::from_millis(20));
    let second = create_session(dir.path(), "C:\\proj", "second");
    std::thread::sleep(std::time::Duration::from_millis(20));
    let other = create_session(dir.path(), "C:\\other", "other");
    // Non-session / non-jsonl files: the garbage file is opened (and skipped
    // as headerless), the .txt is not a candidate at all.
    std::fs::write(dir.path().join("garbage.jsonl"), "not json at all\n")
        .expect("write garbage session");
    std::fs::write(dir.path().join("notes.txt"), "ignored").expect("write non-session file");

    reset_session_list_file_reads();
    let all = SessionManager::list_sessions(dir.path(), None);
    assert_eq!(all.len(), 3, "the garbage file is skipped: {:?}", ids(&all));
    assert_eq!(
        session_list_file_reads(),
        4,
        "one open per .jsonl file (three sessions + the garbage file)"
    );

    let filtered = SessionManager::list_sessions(dir.path(), Some("C:\\proj"));
    let in_memory = SessionManager::filter_sessions_by_cwd(&all, "C:\\proj");
    assert_eq!(
        filtered, in_memory,
        "the filtered listing must equal the in-memory filter of the full listing"
    );
    assert_eq!(
        ids(&filtered),
        vec![second.clone(), first],
        "most recently modified first"
    );
    assert!(!ids(&filtered).contains(&other));

    // The filtered pass shares the single-read property: it still opens each
    // candidate once (the header decides the cwd, then the same reader peeks).
    reset_session_list_file_reads();
    let _ = SessionManager::list_sessions(dir.path(), Some("C:\\proj"));
    assert_eq!(
        session_list_file_reads(),
        4,
        "a filtered listing must not add a second open per file"
    );
}
