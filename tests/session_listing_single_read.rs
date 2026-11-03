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
    peek_session_info_for_test, peek_session_listing_for_test, peek_session_name_for_test,
    read_bounded_line_for_test, reset_session_list_file_reads, session_list_file_reads, FileEntry,
    MessageEntry, SessionManager, SessionSummary, StoredMessage,
};

/// Serializes the tests in this file: `session_list_file_reads` is a
/// PROCESS-GLOBAL counter, so libtest's default parallel threads let another
/// test's listing bump it between `reset` and the exact assertion. A poisoned
/// lock (one failed assertion) still lets the remaining tests run.
static TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn test_lock() -> std::sync::MutexGuard<'static, ()> {
    TEST_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

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
    let _serial = test_lock();
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

/// Run `operation` on a detached thread and fail instead of hanging when it
/// blocks (the pre-fix listing opens a `*.jsonl` FIFO and blocks forever).
#[cfg(unix)]
fn run_bounded<T: Send + 'static>(what: &str, operation: impl FnOnce() -> T + Send + 'static) -> T {
    let (sender, receiver) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = sender.send(operation());
    });
    receiver
        .recv_timeout(std::time::Duration::from_secs(10))
        .unwrap_or_else(|_| {
            panic!("{what} did not finish within 10s (a FIFO candidate was opened)")
        })
}

#[cfg(unix)]
fn make_fifo(path: &Path) -> bool {
    std::process::Command::new("mkfifo")
        .arg(path)
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

#[cfg(unix)]
#[test]
fn listing_recent_resume_and_find_by_id_skip_a_fifo_session_candidate() {
    let _serial = test_lock();
    let dir = TempDir::new("fifo");
    let id = create_session(dir.path(), "C:\\proj", "real");
    let fifo = dir.path().join("zzz-fifo.jsonl");
    if !make_fifo(&fifo) {
        eprintln!("mkfifo unavailable; skipping FIFO regression");
        return;
    }

    let dir_path = dir.path().to_path_buf();
    let listed = run_bounded("list_sessions", move || {
        SessionManager::list_sessions(&dir_path, None)
            .iter()
            .map(|summary| summary.id.clone())
            .collect::<Vec<_>>()
    });
    assert_eq!(listed, vec![id.clone()], "the FIFO is not a session");

    let dir_path = dir.path().to_path_buf();
    let expected = id.clone();
    let resumed = run_bounded("continue_recent", move || {
        SessionManager::continue_recent(&dir_path, "C:\\proj")
            .expect("continue the real session")
            .id()
            .to_owned()
    });
    assert_eq!(
        resumed, expected,
        "the newest candidate is still the real file"
    );

    // A directory whose ONLY `.jsonl` is a FIFO: `find_session_by_id` must
    // return None instead of blocking on the open.
    let only_fifo = TempDir::new("fifo-only");
    if !make_fifo(&only_fifo.path().join("only.jsonl")) {
        eprintln!("mkfifo unavailable; skipping find_session_by_id FIFO check");
        return;
    }
    let fifo_dir = only_fifo.path().to_path_buf();
    let found = run_bounded("find_session_by_id", move || {
        SessionManager::find_session_by_id(&fifo_dir, "missing-id")
    });
    assert_eq!(found, None);
}

#[cfg(unix)]
#[test]
fn a_symlink_to_a_regular_session_stays_discoverable() {
    let _serial = test_lock();
    let dir = TempDir::new("symlink");
    let id = create_session(dir.path(), "C:\\proj", "linked");
    let original = std::fs::read_dir(dir.path())
        .expect("session dir")
        .flatten()
        .map(|entry| entry.path())
        .find(|path| path.extension().and_then(|value| value.to_str()) == Some("jsonl"))
        .expect("session file");
    let target = dir.path().join("hidden-target.bin");
    std::fs::rename(&original, &target).expect("move session file aside");
    let link = dir.path().join("linked.jsonl");
    std::os::unix::fs::symlink(&target, &link).expect("create symlink");

    let sessions = SessionManager::list_sessions(dir.path(), None);
    assert_eq!(
        ids(&sessions),
        vec![id.clone()],
        "Path::metadata follows the link, so a symlinked session is still listed"
    );
    assert_eq!(
        SessionManager::find_session_by_id(dir.path(), &id),
        Some(link),
        "find_session_by_id accepts a symlink to a regular session"
    );
}

#[cfg(unix)]
#[test]
fn symlinked_session_sorts_by_its_target_mtime_like_find_most_recent() {
    let _serial = test_lock();
    let dir = TempDir::new("symlink-mtime");
    // The link is created FIRST, so its own (lstat) mtime is older than every
    // session file; the regular file it points at is written LAST.
    let link = dir.path().join("zzz-linked.jsonl");
    std::os::unix::fs::symlink(dir.path().join("linked-target.bin"), &link)
        .expect("create dangling symlink");
    std::thread::sleep(std::time::Duration::from_millis(30));
    let older = create_session(dir.path(), "C:\\proj", "older");
    std::thread::sleep(std::time::Duration::from_millis(30));

    let mut newest = SessionManager::create("C:\\proj", dir.path()).expect("create newest session");
    newest
        .append_message(user_message("newest"))
        .expect("append newest");
    let newest_id = newest.id().to_owned();
    let newest_path = newest
        .session_file()
        .expect("newest session file")
        .to_path_buf();
    drop(newest);
    std::fs::rename(&newest_path, dir.path().join("linked-target.bin"))
        .expect("move newest session file to the symlink target");

    // `Path::metadata` follows the link, so the linked entry carries the
    // TARGET file's (newest) mtime. `DirEntry::metadata` (lstat) would sort it
    // by the link's older mtime and put `older` first.
    let listed = SessionManager::list_sessions(dir.path(), None);
    assert_eq!(
        ids(&listed),
        vec![newest_id.clone(), older.clone()],
        "the symlink sorts by its target's mtime, not the link's own"
    );
    let headers = SessionManager::list_session_headers(dir.path(), None);
    assert_eq!(
        headers
            .iter()
            .map(|header| header.id.clone())
            .collect::<Vec<_>>(),
        vec![newest_id.clone(), older],
        "the header-only listing uses the same symlink mtime rule"
    );
    assert_eq!(
        SessionManager::continue_recent(dir.path(), "C:\\proj")
            .expect("continue recent")
            .id()
            .to_owned(),
        newest_id,
        "find_most_recent_session agrees with the listing"
    );
}

#[test]
fn read_bounded_line_skips_overlong_lines_and_keeps_the_next_line() {
    let _serial = test_lock();
    let input = b"0123456789abcdefXYZ\nnext\n";
    // Exercise the `fill_buf`/`consume` boundary with 1-byte, odd-sized and
    // large buffers.
    for capacity in [1usize, 7, 64] {
        let mut reader = std::io::BufReader::with_capacity(capacity, &input[..]);
        let mut line = Vec::new();
        assert!(
            read_bounded_line_for_test(&mut reader, &mut line, 16).expect("first line"),
            "cap={capacity}"
        );
        assert!(
            line.is_empty(),
            "cap={capacity}: the 19-byte line over the 16-byte cap is discarded, not truncated"
        );
        assert!(
            read_bounded_line_for_test(&mut reader, &mut line, 16).expect("second line"),
            "cap={capacity}: scanning continues after an overlong line"
        );
        assert_eq!(line, b"next\n", "cap={capacity}");
        assert!(
            !read_bounded_line_for_test(&mut reader, &mut line, 16).expect("eof"),
            "cap={capacity}"
        );
    }
}

#[test]
fn read_bounded_line_admits_lines_up_to_the_cap_plus_newline() {
    let _serial = test_lock();
    let input = b"abcd\nab\n";
    let mut reader = std::io::BufReader::with_capacity(2, &input[..]);
    let mut line = Vec::new();
    assert!(read_bounded_line_for_test(&mut reader, &mut line, 4).expect("max-length line"));
    assert_eq!(line, b"abcd\n", "a line of exactly `max` bytes is accepted");
    assert!(read_bounded_line_for_test(&mut reader, &mut line, 4).expect("short line"));
    assert_eq!(line, b"ab\n");
    assert!(!read_bounded_line_for_test(&mut reader, &mut line, 4).expect("eof"));
}

#[test]
fn read_bounded_line_rejects_an_unterminated_tail_of_cap_plus_one() {
    let _serial = test_lock();
    // `max + 1` content bytes, no trailing newline: over-long, dropped.
    let overlong = [b'x'; 17];
    let mut reader = std::io::BufReader::with_capacity(5, &overlong[..]);
    let mut line = Vec::new();
    assert!(
        !read_bounded_line_for_test(&mut reader, &mut line, 16).expect("overlong tail"),
        "an unterminated max+1 tail is over the cap and reports no line"
    );
    assert!(
        line.is_empty(),
        "the over-long tail is discarded, not returned"
    );

    // Exactly `max` content bytes with no trailing newline is a valid tail.
    let exact = vec![b'y'; 16];
    let mut reader = std::io::BufReader::with_capacity(5, &exact[..]);
    assert!(read_bounded_line_for_test(&mut reader, &mut line, 16).expect("exact tail"));
    assert_eq!(
        line, exact,
        "an unterminated tail of exactly `max` bytes is accepted"
    );
    assert!(!read_bounded_line_for_test(&mut reader, &mut line, 16).expect("eof"));
}

#[test]
fn the_small_cap_peeks_discard_an_over_cap_line_and_keep_scanning() {
    let _serial = test_lock();
    // Small maxima exercise the over-cap discard path without a 256 MiB
    // fixture; both entry points share `read_bounded_line`.
    const MAX: usize = 64;
    let oversize = format!(
        "{{\"type\":\"session_info\",\"name\":\"{}\"}}",
        "o".repeat(MAX)
    );
    assert!(oversize.len() > MAX);

    for name_only in [false, true] {
        let peek = |input: &str| -> Option<String> {
            let mut reader = std::io::BufReader::with_capacity(7, input.as_bytes());
            if name_only {
                peek_session_name_for_test(&mut reader, MAX)
            } else {
                peek_session_info_for_test(&mut reader, MAX).0
            }
        };

        // The over-cap line is LAST: were it buffered instead of discarded,
        // its name would win; the earlier small name must survive.
        let input = format!("{{\"type\":\"session_info\",\"name\":\"kept\"}}\n{oversize}\n");
        assert_eq!(
            peek(&input).as_deref(),
            Some("kept"),
            "name_only={name_only}: the over-cap session_info line was discarded, not parsed"
        );

        // A small line AFTER the over-cap one must still be read (scanning
        // continues past the discarded line).
        let input = format!("{oversize}\n{{\"type\":\"session_info\",\"name\":\"after\"}}\n");
        assert_eq!(
            peek(&input).as_deref(),
            Some("after"),
            "name_only={name_only}: scanning continues after an over-cap line"
        );
    }
}

#[test]
fn listing_peek_counts_an_over_cap_message_from_its_retained_prefix() {
    let _serial = test_lock();
    // Small cap: the retained prefix still carries `"type":"message"` and the
    // role, so an oversized message is counted without buffering the body.
    const MAX: usize = 64;
    let long_text = "x".repeat(256);
    let message = FileEntry::Message(MessageEntry {
        id: "m-overlong".to_owned(),
        parent_id: None,
        timestamp: "2026-01-01T00:00:00.000Z".to_owned(),
        message: StoredMessage::from(user_message(&long_text)),
    });
    let message_line = serde_json::to_string(&message).expect("serialize message");
    assert!(message_line.len() > MAX);
    let input = format!("{message_line}\n{{\"type\":\"session_info\",\"name\":\"after\"}}\n");
    let mut reader = std::io::BufReader::with_capacity(7, input.as_bytes());
    let (name, first_message, message_count) = peek_session_listing_for_test(&mut reader, MAX);
    assert_eq!(message_count, 1, "the over-cap message is still counted");
    assert_eq!(
        name.as_deref(),
        Some("after"),
        "the peek keeps scanning after an over-cap line"
    );
    assert_eq!(
        first_message, "(no messages)",
        "a truncated JSON prefix is never parsed as a message"
    );
}

#[test]
fn list_sessions_counts_a_message_line_over_the_listing_cap() {
    let _serial = test_lock();
    let dir = TempDir::new("listing-over-cap");
    let mut session = SessionManager::create("C:\\proj", dir.path()).expect("create session");
    session
        .append_message(user_message("first"))
        .expect("append first message");
    let big = "x".repeat(2 * 1024 * 1024);
    session
        .append_message(user_message(&big))
        .expect("append oversized message");
    session
        .append_session_info("named")
        .expect("append session info");

    let summaries = SessionManager::list_sessions(dir.path(), None);
    assert_eq!(summaries.len(), 1);
    assert_eq!(
        summaries[0].message_count, 2,
        "the over-cap message line is still counted by the listing"
    );
    assert_eq!(
        summaries[0].first_message, "first",
        "the first small user message remains the preview"
    );
    assert_eq!(summaries[0].name.as_deref(), Some("named"));
}

#[test]
fn the_listing_peek_walks_past_a_multi_mib_under_cap_line() {
    let _serial = test_lock();
    let dir = TempDir::new("long-peek-line");
    let mut session = SessionManager::create("C:\\proj", dir.path()).expect("create session");
    session
        .append_message(user_message("first"))
        .expect("append message");
    let path = session.session_file().expect("session file").to_path_buf();

    // A 4 MiB junk line between the message and the session_info entry. It is
    // UNDER `MAX_SESSION_LINE_BYTES` (256 MiB), so the peek DOES buffer it in
    // full and simply skips it as a non-entry line; this test only proves the
    // scanner recovers after a long-but-under-cap line. The over-cap discard
    // path is covered by the small-cap tests above.
    {
        use std::io::Write as _;
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .expect("open session for junk append");
        file.write_all(&vec![b'x'; 4 * 1024 * 1024])
            .expect("write junk line");
        file.write_all(b"\n").expect("terminate junk line");
    }
    // The manager notices the external length change and appends after it.
    session
        .append_session_info("named")
        .expect("append session info");

    let summaries = SessionManager::list_sessions(dir.path(), None);
    assert_eq!(summaries.len(), 1);
    assert_eq!(summaries[0].message_count, 1);
    assert_eq!(
        summaries[0].name.as_deref(),
        Some("named"),
        "the peek must reach the session_info line after the 4 MiB line"
    );
    let headers = SessionManager::list_session_headers(dir.path(), None);
    assert_eq!(headers.len(), 1);
    assert_eq!(
        headers[0].name.as_deref(),
        Some("named"),
        "the name-only peek must also walk past the 4 MiB line"
    );
}
