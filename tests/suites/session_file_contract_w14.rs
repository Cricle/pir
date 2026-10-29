//! W14 session-file contract: the upstream v2 shapes the v2→v3 migration must
//! be able to READ without losing the transcript, and the invariants it must
//! preserve when it rewrites the file.
//!
//! W13 pinned the root cause (upstream stores `message.content` as a JSON
//! string where pir stores a block array) and the atomic publish. This suite
//! closes the remaining gaps in that contract:
//!
//! 1. the string form on EVERY role the normalizer claims to repair
//!    (`assistant`, `developer`, `toolResult`, not only `user`);
//! 2. a file that MIXES the string form and the block-array form, where both
//!    lines must load;
//! 3. an unknown message shape that must land in `unparsed_raw` and therefore
//!    make the migration REFUSE to rewrite — the raw line is preserved
//!    verbatim, never silently dropped;
//! 4. the post-migration round trip: reopening the rewritten file yields the
//!    same number of entries and the same first/last ids;
//! 5. a concurrent reader that opens the path mid-rewrite: a Windows sharing
//!    violation is a legitimate transient, `NotFound` is a hard failure.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use pir::session_manager::{FileEntry, SessionManager, StoredMessage, CURRENT_SESSION_VERSION};

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

fn temp_dir(name: &str) -> PathBuf {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    let dir = std::env::temp_dir().join(format!("pir-w14-session-file-{name}-{stamp}"));
    std::fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

/// A v2 session header (version 2 is what forces the migration).
fn v2_header() -> Value {
    json!({
        "type": "session",
        "version": 2,
        "id": "22222222-3333-4444-5555-666666666666",
        "timestamp": "2025-01-01T00:00:00.000Z",
        "cwd": "C:\\repo",
    })
}

/// One v2 message line with an explicit id/parent chain.
fn v2_message(id: &str, parent: Option<&str>, message: Value) -> Value {
    json!({
        "type": "message",
        "id": id,
        "parentId": parent,
        "timestamp": "2025-01-01T00:00:01.000Z",
        "message": message,
    })
}

/// Serialize lines as the session file's JSONL body (trailing newline
/// included, so the loader's torn-tail repair never fires).
fn write_lines(path: &Path, lines: &[Value]) -> String {
    let mut body = String::new();
    for line in lines {
        body.push_str(&line.to_string());
        body.push('\n');
    }
    std::fs::write(path, &body).expect("write fixture");
    body
}

fn text_of_message(inner: &pir::session_manager::MessageEntry) -> String {
    let blocks = match &inner.message {
        StoredMessage::User(user) => user.content.clone(),
        StoredMessage::Assistant(assistant) => assistant.content.clone(),
        StoredMessage::ToolResult(result) => result.content.clone(),
        other => panic!("unexpected stored message: {other:?}"),
    };
    blocks
        .iter()
        .filter_map(|block| match block {
            pir::ai::types::ContentBlock::Text(text) => Some(text.text.clone()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn entry_ids(entries: &[FileEntry]) -> Vec<String> {
    entries
        .iter()
        .map(|entry| entry.entry_id().to_owned())
        .collect()
}

// ---------------------------------------------------------------------------
// 1. The string form on every role
// ---------------------------------------------------------------------------

/// Upstream types `message.content` as `string | Content[]`, so a v2 file may
/// carry the string form on `assistant`, `developer` AND `toolResult`, not
/// just `user`. Every one of them must become a real entry with its text
/// preserved; a single unparsed role would read as a hole in the transcript.
#[test]
fn string_content_loads_for_assistant_developer_and_tool_result() {
    let dir = temp_dir("string-all-roles");
    let path = dir.join("v2-all-roles.jsonl");

    let assistant = json!({
        "role": "assistant",
        "content": "answer text",
        "api": "test-api",
        "provider": "test",
        "model": "test-model",
        "usage": {
            "input": 1, "output": 2, "cacheRead": 0, "cacheWrite": 0,
            "totalTokens": 3,
            "cost": {"input": 0.0, "output": 0.0, "cacheRead": 0.0, "cacheWrite": 0.0, "total": 0.0},
        },
        "stopReason": "stop",
        "timestamp": 2,
    });
    let developer = json!({
        "role": "developer",
        "content": "developer guidance",
        "timestamp": 3,
    });
    let tool_result = json!({
        "role": "toolResult",
        "toolCallId": "call-1",
        "toolName": "read",
        "content": "tool output",
        "isError": false,
        "timestamp": 4,
    });

    let mut lines = vec![v2_header()];
    lines.push(v2_message(
        "m0000000",
        None,
        json!({"role":"user","content":"prompt","timestamp":1}),
    ));
    lines.push(v2_message("m0000001", Some("m0000000"), assistant));
    lines.push(v2_message("m0000002", Some("m0000001"), developer));
    lines.push(v2_message("m0000003", Some("m0000002"), tool_result));
    write_lines(&path, &lines);

    let session = SessionManager::open(&path).expect("v2 open");
    let entries = session.entries();
    assert_eq!(
        entries.len(),
        4,
        "every string-content role must load: {:?}",
        entry_ids(&entries)
    );

    let roles: Vec<&'static str> = entries
        .iter()
        .map(|entry| match entry {
            FileEntry::Message(inner) => inner.message.role(),
            other => panic!("expected a message entry, got {other:?}"),
        })
        .collect();
    assert_eq!(roles, vec!["user", "assistant", "developer", "toolResult"]);

    let texts: Vec<String> = entries
        .iter()
        .map(|entry| match entry {
            FileEntry::Message(inner) => text_of_message(inner),
            other => panic!("expected a message entry, got {other:?}"),
        })
        .collect();
    assert_eq!(
        texts,
        vec![
            "prompt".to_owned(),
            "answer text".to_owned(),
            "developer guidance".to_owned(),
            "tool output".to_owned(),
        ],
        "the string content survives as one text block per role"
    );

    assert_eq!(
        session.header().and_then(|header| header.version),
        Some(CURRENT_SESSION_VERSION),
        "the migrated header is at the current version"
    );

    // The rewrite published the block form for every role.
    let published = std::fs::read_to_string(&path).expect("rewritten file");
    let published: Vec<Value> = published
        .lines()
        .map(|line| serde_json::from_str(line).expect("parseable published line"))
        .collect();
    assert_eq!(published.len(), 5, "header + 4 entries");
    assert_eq!(published[0]["version"], CURRENT_SESSION_VERSION);
    for (index, expected) in [
        (1, "prompt"),
        (2, "answer text"),
        (3, "developer guidance"),
        (4, "tool output"),
    ] {
        assert_eq!(
            published[index]["message"]["content"][0]["type"], "text",
            "line {index} content was normalized to a block array"
        );
        assert_eq!(published[index]["message"]["content"][0]["text"], expected);
    }

    std::fs::remove_dir_all(&dir).ok();
}

// ---------------------------------------------------------------------------
// 2. Mixed string / block-array content
// ---------------------------------------------------------------------------

/// A real transcript can hold both wire shapes on different lines (the string
/// form is older/compact; the array form is what newer writers persist). Both
/// must load, and the migration must rewrite the file rather than bail.
#[test]
fn mixed_string_and_block_array_content_both_load() {
    let dir = temp_dir("mixed-shapes");
    let path = dir.join("v2-mixed.jsonl");

    let mut lines = vec![v2_header()];
    let mut parent: Option<String> = None;
    for index in 0..8usize {
        let id = format!("m{index:07}");
        let message = if index % 2 == 0 {
            json!({"role": "user", "content": format!("string {index}"), "timestamp": index})
        } else {
            json!({
                "role": "user",
                "content": [{"type": "text", "text": format!("blocks {index}")}],
                "timestamp": index,
            })
        };
        lines.push(v2_message(&id, parent.as_deref(), message));
        parent = Some(id);
    }
    write_lines(&path, &lines);

    let session = SessionManager::open(&path).expect("v2 mixed-shape open");
    let entries = session.entries();
    assert_eq!(entries.len(), 8, "no line may be dropped: {entries:?}");

    let texts: Vec<String> = entries
        .iter()
        .map(|entry| match entry {
            FileEntry::Message(inner) => text_of_message(inner),
            other => panic!("expected a message entry, got {other:?}"),
        })
        .collect();
    let expected: Vec<String> = (0..8usize)
        .map(|index| {
            if index % 2 == 0 {
                format!("string {index}")
            } else {
                format!("blocks {index}")
            }
        })
        .collect();
    assert_eq!(texts, expected, "both shapes carry the same text");

    // The mixed file parsed completely, so the migration REWROTE it.
    let published = std::fs::read_to_string(&path).expect("rewritten file");
    let published: Vec<Value> = published
        .lines()
        .map(|line| serde_json::from_str(line).expect("parseable published line"))
        .collect();
    assert_eq!(published.len(), 9, "header + 8 entries");
    assert_eq!(published[0]["version"], CURRENT_SESSION_VERSION);
    for line in &published[1..] {
        assert!(
            line["message"]["content"].is_array(),
            "every published line uses the block form: {line}"
        );
    }

    std::fs::remove_dir_all(&dir).ok();
}

// ---------------------------------------------------------------------------
// 3. Unknown message shape: preserved, never silently dropped
// ---------------------------------------------------------------------------

/// An unknown message shape must NOT be coerced into an empty message (that
/// would silently erase content). It stays raw in `unparsed_raw`, and because
/// the loaded subset cannot reproduce the file exactly, the migration must
/// refuse its rewrite: the file on disk stays byte-identical, still holding
/// the unknown line verbatim.
#[test]
fn an_unknown_message_shape_is_preserved_and_blocks_the_rewrite() {
    let dir = temp_dir("unknown-shape");
    let path = dir.join("v2-unknown.jsonl");

    // `content` is a JSON object — neither the string form nor a block array.
    // The normalizer deliberately leaves it alone so the line fails loudly.
    let unknown_line = v2_message(
        "m0000001",
        Some("m0000000"),
        json!({"role": "user", "content": {"unexpected": true}, "timestamp": 2}),
    );
    let lines = vec![
        v2_header(),
        v2_message(
            "m0000000",
            None,
            json!({"role": "user", "content": "before", "timestamp": 1}),
        ),
        unknown_line,
        v2_message(
            "m0000002",
            Some("m0000001"),
            json!({"role": "user", "content": "after", "timestamp": 3}),
        ),
    ];
    let original = write_lines(&path, &lines);
    let unknown_text = lines[2].to_string();

    let session = SessionManager::open(&path).expect("open a file with an unknown line");
    let entries = session.entries();
    assert_eq!(
        entries.len(),
        2,
        "only the parseable lines become entries: {:?}",
        entry_ids(&entries)
    );
    // The migration was APPLIED in memory (the header is at the current
    // version) even though the file was not rewritten.
    assert_eq!(
        session.header().and_then(|header| header.version),
        Some(CURRENT_SESSION_VERSION),
        "the in-memory migration still runs"
    );

    // The raw line was preserved: the migration REFUSED to rewrite, so the
    // file is byte-identical and the unknown line is still there verbatim.
    let on_disk = std::fs::read_to_string(&path).expect("the file still exists");
    assert_eq!(
        on_disk, original,
        "the migration must not rewrite a file holding undeserializable lines"
    );
    assert!(
        on_disk.contains(&unknown_text),
        "the unknown line survives verbatim: {on_disk}"
    );
    let disk_header: Value =
        serde_json::from_str(on_disk.lines().next().expect("header line")).expect("header json");
    assert_eq!(
        disk_header["version"], 2,
        "the on-disk header is still v2: the rewrite was refused"
    );

    // A second open sees the same thing: nothing was dropped by the first.
    let reopened = SessionManager::open(&path).expect("reopen");
    assert_eq!(reopened.entries().len(), 2);
    assert_eq!(std::fs::read_to_string(&path).unwrap(), original);

    std::fs::remove_dir_all(&dir).ok();
}

// ---------------------------------------------------------------------------
// 4. Round trip after migration
// ---------------------------------------------------------------------------

/// After the v2→v3 rewrite, reopening the SAME path must reproduce the entry
/// list exactly: the same count and the same first/last ids. This is the
/// user-visible half of the data-loss bug — the transcript must still be
/// there on the next open.
#[test]
fn migration_round_trips_entry_count_and_ids_through_disk() {
    let dir = temp_dir("round-trip");
    let path = dir.join("v2-round-trip.jsonl");

    let count = 25usize;
    let mut lines = vec![v2_header()];
    let mut parent: Option<String> = None;
    for index in 0..count {
        let id = format!("m{index:07}");
        // Alternate the two upstream wire shapes so the round trip covers both.
        let content = if index % 3 == 0 {
            json!(format!("prompt {index}"))
        } else {
            json!([{"type": "text", "text": format!("prompt {index}")}])
        };
        lines.push(v2_message(
            &id,
            parent.as_deref(),
            json!({"role": "user", "content": content, "timestamp": index}),
        ));
        parent = Some(id);
    }
    write_lines(&path, &lines);

    let migrated = SessionManager::open(&path).expect("migrating open");
    let migrated_entries = migrated.entries();
    assert_eq!(migrated_entries.len(), count);
    let migrated_ids = entry_ids(&migrated_entries);
    assert_eq!(migrated_ids.first().map(String::as_str), Some("m0000000"));
    assert_eq!(
        migrated_ids.last().map(String::as_str),
        Some("m0000024"),
        "the last id is the v2 fixture's"
    );
    drop(migrated);

    let reopened = SessionManager::open(&path).expect("reopen the rewritten file");
    let reopened_entries = reopened.entries();
    assert_eq!(
        reopened_entries.len(),
        count,
        "the rewritten file still holds every entry"
    );
    let reopened_ids = entry_ids(&reopened_entries);
    assert_eq!(
        reopened_ids, migrated_ids,
        "every entry id survives the rewrite in order"
    );
    assert_eq!(
        reopened_entries, migrated_entries,
        "the entry payloads survive the rewrite"
    );

    std::fs::remove_dir_all(&dir).ok();
}

// ---------------------------------------------------------------------------
// 5. Concurrent reader that OPENS the file mid-rewrite
// ---------------------------------------------------------------------------

/// A reader that OPENS the session path while the migration is publishing must
/// never be told the file is missing. On Windows the open can transiently fail
/// with a sharing violation while the rewrite swaps the file; that is
/// legitimate and tolerated. `NotFound` is not: it means the publish left a
/// window in which the user's transcript did not exist.
#[test]
fn opening_the_path_mid_rewrite_only_ever_tolerates_sharing_violations() {
    let dir = temp_dir("mid-rewrite-open");
    let path = dir.join("v2-mid-rewrite.jsonl");

    // A large fixture widens the rewrite window so the reader actually races
    // the publish instead of only seeing the finished file.
    let count = 1_500usize;
    let mut lines = vec![v2_header()];
    let mut parent: Option<String> = None;
    for index in 0..count {
        let id = format!("m{index:07}");
        lines.push(v2_message(
            &id,
            parent.as_deref(),
            json!({"role": "user", "content": format!("prompt {index}"), "timestamp": index}),
        ));
        parent = Some(id);
    }
    write_lines(&path, &lines);
    let expected_lines = count + 1;

    let stop = Arc::new(AtomicBool::new(false));
    let successful_opens = Arc::new(AtomicUsize::new(0));
    let transient_errors = Arc::new(AtomicUsize::new(0));
    let reader = {
        let path = path.clone();
        let stop = Arc::clone(&stop);
        let successful_opens = Arc::clone(&successful_opens);
        let transient_errors = Arc::clone(&transient_errors);
        std::thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                match std::fs::File::open(&path) {
                    Ok(mut file) => {
                        use std::io::Read as _;
                        let mut content = String::new();
                        file.read_to_string(&mut content)
                            .expect("a held handle of a session is always readable");
                        assert!(
                            !content.is_empty(),
                            "an opened session is never empty mid-rewrite"
                        );
                        let lines: Vec<&str> = content.lines().collect();
                        assert_eq!(
                            lines.len(),
                            expected_lines,
                            "an opened session is always a complete transcript"
                        );
                        for line in lines {
                            serde_json::from_str::<Value>(line)
                                .unwrap_or_else(|error| panic!("torn line {line:?}: {error}"));
                        }
                        successful_opens.fetch_add(1, Ordering::Relaxed);
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                        panic!("the session file was MISSING during the rewrite: {error}")
                    }
                    Err(error) => {
                        // Windows rename-while-open surfaces as a sharing
                        // violation (32) or access denied (5). Anything else
                        // on the open path is not a documented transient.
                        let raw = error.raw_os_error();
                        if cfg!(windows) {
                            assert!(
                                matches!(raw, Some(32) | Some(5)),
                                "unexpected concurrent-open error: {error} (raw {raw:?})"
                            );
                        }
                        transient_errors.fetch_add(1, Ordering::Relaxed);
                    }
                }
            }
        })
    };

    // Let the reader actually get onto the file before the rewrite starts, so
    // the loop is guaranteed to observe the pre-migration transcript and the
    // race is not left to thread scheduling.
    let start_deadline = Instant::now() + Duration::from_secs(5);
    while successful_opens.load(Ordering::Relaxed) == 0 {
        assert!(
            Instant::now() < start_deadline,
            "the reader never managed to open the session"
        );
        std::thread::sleep(Duration::from_millis(1));
    }

    // The migrating open retries Windows sharing violations against the
    // reader's handles; the live file stays intact throughout.
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut session = None;
    let mut last_error = None;
    while Instant::now() < deadline {
        match SessionManager::open(&path) {
            Ok(opened) => {
                session = Some(opened);
                break;
            }
            Err(error) => {
                last_error = Some(error);
                std::thread::sleep(Duration::from_millis(5));
            }
        }
    }
    let session = session.unwrap_or_else(|| {
        panic!("the migrating open eventually succeeds: {last_error:?}");
    });
    assert_eq!(session.entries().len(), count, "the transcript is complete");

    stop.store(true, Ordering::Relaxed);
    reader.join().expect("reader thread");

    assert!(
        successful_opens.load(Ordering::Relaxed) > 0,
        "the reader opened a complete transcript at least once"
    );
    // Transients are allowed (and likely on Windows); the invariant is that
    // none of them was `NotFound`, which would already have panicked.

    // After both sides settle, the published file is one complete v3
    // transcript.
    let published = std::fs::read_to_string(&path).expect("the session file survives");
    let published: Vec<Value> = published
        .lines()
        .map(|line| serde_json::from_str(line).expect("parseable published line"))
        .collect();
    assert_eq!(published.len(), expected_lines);
    assert_eq!(published[0]["version"], CURRENT_SESSION_VERSION);

    std::fs::remove_dir_all(&dir).ok();
}
