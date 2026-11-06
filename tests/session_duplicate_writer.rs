//! Regression: two TUIs using the same explicit `--session-id` must never
//! create two transcripts with the same session UUID.
//!
//! The transcript file name embeds a creation timestamp, and the per-file
//! writer lock (`.<timestamp>_<id>.jsonl.lock`) is keyed on that name. Two
//! creators that both see an empty session directory therefore lock two
//! different paths and each `create_new`s its own file. The fix claims the
//! session ID itself (`.session-<id>.lock`) from the `create_with_id` lookup
//! until the first transcript write materializes the header, so exactly one
//! creator wins and the other fails with an explicit "already in use" error.

use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::sync::{Arc, Barrier};
use std::time::{Duration, SystemTime};

use pir::ai::types::*;
use pir::session_manager::SessionManager;

fn temp_dir(label: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "pir-duplicate-writer-{label}-{}-{}",
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

fn jsonl_names(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .expect("read session dir")
        .flatten()
        .filter(|entry| entry.path().extension().and_then(|ext| ext.to_str()) == Some("jsonl"))
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

/// Two threads race `create_with_id` for one id from an empty directory.
/// Exactly one may obtain a manager; the other must report "already in use".
#[test]
fn two_threads_creating_the_same_id_produce_exactly_one_writer() {
    let dir = temp_dir("same-id-race");
    let id = "race-session-id";
    let start = Arc::new(Barrier::new(3));
    let done = Arc::new(Barrier::new(3));
    let (tx, rx) = mpsc::channel();

    let mut threads = Vec::new();
    for _ in 0..2 {
        let dir = dir.clone();
        let start = Arc::clone(&start);
        let done = Arc::clone(&done);
        let tx = tx.clone();
        threads.push(std::thread::spawn(move || {
            start.wait();
            let result = SessionManager::create_with_id(".", &dir, id)
                .map(|manager| manager.id().to_owned())
                .map_err(|error| error.to_string());
            tx.send(result).expect("send attempt result");
            // Keep a winning manager alive until BOTH attempts have run.
            done.wait();
        }));
    }
    drop(tx);

    start.wait();
    let results: Vec<_> = (0..2)
        .map(|_| rx.recv().expect("attempt result"))
        .collect();
    done.wait();
    for thread in threads {
        thread.join().expect("creator thread");
    }

    let winners = results.iter().filter(|result| result.is_ok()).count();
    assert_eq!(
        winners, 1,
        "exactly one creator may claim a session id: {results:?}"
    );
    let error = results
        .iter()
        .find_map(|result| result.as_ref().err())
        .expect("one attempt must fail");
    assert!(
        error.contains("already in use"),
        "the loser must report the session as occupied: {error}"
    );
    assert!(
        jsonl_names(&dir).is_empty(),
        "creation is lazy until the first conversation entry: {:?}",
        jsonl_names(&dir)
    );
}

/// Once the winner has written its header, the id is discoverable and a
/// second `create_with_id` must refuse to add another file (it must NOT make
/// `<new-timestamp>_<same-id>.jsonl`).
#[test]
fn a_flushed_session_id_cannot_be_created_again() {
    let dir = temp_dir("flushed-id");
    let id = "flushed-session-id";

    let mut first = SessionManager::create_with_id(".", &dir, id).expect("first creator");
    first
        .append_message(user_message("first turn"))
        .expect("materialize session file");
    assert_eq!(jsonl_names(&dir).len(), 1, "one file after the first turn");

    let error = SessionManager::create_with_id(".", &dir, id)
        .expect_err("a second creator must be rejected")
        .to_string();
    assert!(
        error.contains("already exists"),
        "the duplicate must be named explicitly: {error}"
    );
    assert_eq!(
        jsonl_names(&dir).len(),
        1,
        "no second same-id transcript may appear: {:?}",
        jsonl_names(&dir)
    );

    // The original writer still owns its file.
    let reopened = SessionManager::find_session_by_id(&dir, id).expect("discover session by id");
    assert_eq!(first.path(), reopened);
}

/// Child-process half of [`another_process_cannot_create_the_same_session_id`].
/// Ignored in the normal suite; the parent re-executes this test binary with
/// `--ignored --exact` and points it at the directory/id to hold.
#[test]
#[ignore = "helper process for the cross-process duplicate-writer test"]
fn cross_process_duplicate_writer_holder() {
    let Some(dir) = std::env::var_os("PIR_TEST_DUP_DIR") else {
        return;
    };
    let Some(id) = std::env::var_os("PIR_TEST_DUP_ID") else {
        return;
    };
    let Some(marker) = std::env::var_os("PIR_TEST_DUP_MARKER") else {
        return;
    };
    let _manager = SessionManager::create_with_id(".", Path::new(&dir), &id.to_string_lossy())
        .expect("child claims the session id");
    std::fs::write(marker, b"ready").expect("write holder-ready marker");
    std::thread::sleep(Duration::from_secs(60));
}

/// A real second process holding the id claim blocks this process with the
/// same clear error (no second file exists yet, so the file lookup cannot be
/// what rejects it).
#[test]
fn another_process_cannot_create_the_same_session_id() {
    let dir = temp_dir("cross-process");
    let id = "cross-process-session-id";
    let marker = dir.join("holder-ready");
    let mut child = std::process::Command::new(std::env::current_exe().expect("test binary"))
        .args([
            "cross_process_duplicate_writer_holder",
            "--ignored",
            "--test-threads=1",
        ])
        .env("PIR_TEST_DUP_DIR", &dir)
        .env("PIR_TEST_DUP_ID", id)
        .env("PIR_TEST_DUP_MARKER", &marker)
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

    let error = SessionManager::create_with_id(".", &dir, id)
        .expect_err("the second process must lose the id claim")
        .to_string();
    assert!(
        error.contains("already in use"),
        "the loser must report the session as occupied: {error}"
    );
    assert!(
        error.contains(&format!("pid {}", child.id())),
        "the error should name the holding process: {error}"
    );
    assert!(
        jsonl_names(&dir).is_empty(),
        "no transcript may be created by either side yet: {:?}",
        jsonl_names(&dir)
    );

    let _ = child.kill();
    let _ = child.wait();
}
