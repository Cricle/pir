//! W13 hermetic regressions for the session REWRITE path (atomic publish) and
//! the reader's tolerance of a transient zero-byte session file.
//!
//! Both cases below were measured as a DATA-INTEGRITY failure: a v2 session
//! opened with ZERO entries (`left: 0, right: 300`). The two independent
//! causes pinned here are
//!
//! 1. **Upstream's `content: string` message shape.** A v2 line such as
//!    `{"role":"user","content":"prompt 0"}` does not deserialize into pir's
//!    `Vec<ContentBlock>`, so the whole message line was classified
//!    undeserializable: it never became an entry, and the migration's
//!    preserve-unknown-lines rule then refused the rewrite. The session
//!    opened header-only — an empty transcript — even though the file on disk
//!    was complete. The loader now wraps the string as one text block.
//! 2. **A zero-byte read mistaken for a new session.** `SessionManager::open`
//!    treats "no entries" as a fresh session and WRITES a header into the
//!    path. Believing a transient zero-length observation therefore replaced
//!    another writer's transcript. The read now retries, bounded, while the
//!    file is observed at zero bytes.
//!
//! What the rewrite must ALWAYS guarantee (and what these tests assert):
//! the published path names a COMPLETE, parseable transcript at every
//! observable moment; it is never missing, never zero-length, never torn. A
//! Windows sharing violation is a legitimate transient on both sides, so the
//! migrating open is retried and read errors other than `NotFound` are
//! ignored; a `NotFound` is a hard failure.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use pir::session_manager::{FileEntry, SessionManager, StoredMessage, CURRENT_SESSION_VERSION};

/// The upstream v2 fixture: a header plus `entries` message lines whose
/// `content` is a plain STRING (upstream's `string | TextContent[]` shape),
/// chained by id/parentId.
fn write_v2_string_content_session(path: &Path, entries: usize) {
    let mut file = std::fs::File::create(path).expect("create v2 fixture");
    writeln!(
        file,
        "{{\"type\":\"session\",\"version\":2,\"id\":\"22222222-3333-4444-5555-666666666666\",\
         \"timestamp\":\"2025-01-01T00:00:00.000Z\",\"cwd\":\"C:\\\\repo\"}}"
    )
    .expect("write header");
    for index in 0..entries {
        writeln!(
            file,
            "{{\"type\":\"message\",\"id\":\"m{index:07}\",\"parentId\":{},\
             \"timestamp\":\"2025-01-01T00:00:01.000Z\",\
             \"message\":{{\"role\":\"user\",\"content\":\"prompt {index}\",\"timestamp\":1}}}}",
            if index == 0 {
                "null".to_owned()
            } else {
                format!("\"m{:07}\"", index - 1)
            }
        )
        .expect("write entry");
    }
    file.sync_all().expect("sync fixture");
}

fn temp_dir(name: &str) -> PathBuf {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    let dir = std::env::temp_dir().join(format!("pir-w13-rewrite-{name}-{stamp}"));
    std::fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

/// Rewrite temp files left in `dir` (a successful rewrite removes its own).
fn temp_leftovers(dir: &Path) -> Vec<String> {
    std::fs::read_dir(dir)
        .expect("read dir")
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| name.contains(".tmp."))
        .collect()
}

/// Every line of the published file, parsed (a torn or missing line panics).
fn published_lines(path: &Path) -> Vec<serde_json::Value> {
    let content = std::fs::read_to_string(path).expect("the session file survives");
    content
        .lines()
        .map(|line| {
            serde_json::from_str(line).unwrap_or_else(|error| panic!("torn line {line:?}: {error}"))
        })
        .collect()
}

/// A `content: "string"` v2 message line becomes a REAL entry, not an
/// undeserializable one, so the open is not header-only and the migration
/// publishes the full v3 transcript.
#[test]
fn string_content_v2_messages_load_and_migrate() {
    let dir = temp_dir("string-content");
    let path = dir.join("v2-string.jsonl");
    write_v2_string_content_session(&path, 12);

    let session = SessionManager::open(&path).expect("v2 string-content open");
    assert_eq!(
        session.entries().len(),
        12,
        "a string-content message line must not be dropped"
    );
    assert_eq!(
        session.header().and_then(|header| header.version),
        Some(CURRENT_SESSION_VERSION),
        "the migrated header is at the current version"
    );
    // The text is preserved (the reduction is lossy in shape only).
    let first = session.entries().into_iter().next().expect("first entry");
    match first {
        FileEntry::Message(inner) => {
            assert!(
                matches!(inner.message, StoredMessage::User(_)),
                "the string content stays a user message"
            );
            let context = session.build_context_entries().expect("context");
            let rendered = format!("{:?}", context.first().expect("one context message"));
            assert!(
                rendered.contains("prompt 0"),
                "the string content survives as text: {rendered}"
            );
        }
        other => panic!("expected a message entry, got {other:?}"),
    }

    // The migration REWROTE the file at v3 with every line parseable.
    let lines = published_lines(&path);
    assert_eq!(lines.len(), 13, "header + 12 entries");
    assert_eq!(lines[0]["version"], CURRENT_SESSION_VERSION);
    assert_eq!(lines[1]["message"]["content"][0]["type"], "text");
    assert_eq!(lines[1]["message"]["content"][0]["text"], "prompt 0");
    assert!(
        temp_leftovers(&dir).is_empty(),
        "a successful rewrite leaves no temp file"
    );

    std::fs::remove_dir_all(&dir).ok();
}

/// A reader polling the path through a full v2→v3 rewrite must never observe
/// a missing, empty or torn transcript: the published name always holds a
/// complete parseable file. This is the reader-side half of the measured
/// `0 lines published` failure.
#[test]
fn rewrite_publishes_a_complete_transcript_to_a_polling_reader() {
    let dir = temp_dir("atomic");
    let path = dir.join("v2.jsonl");
    write_v2_string_content_session(&path, 300);
    let line_count = 301;

    let stop = Arc::new(AtomicBool::new(false));
    let published_reads = Arc::new(AtomicUsize::new(0));
    let reader = {
        let path = path.clone();
        let stop = Arc::clone(&stop);
        let published_reads = Arc::clone(&published_reads);
        std::thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                match std::fs::read_to_string(&path) {
                    Ok(content) => {
                        assert!(
                            !content.is_empty(),
                            "a concurrent reader must never see an empty session"
                        );
                        let lines: Vec<&str> = content.lines().collect();
                        assert_eq!(
                            lines.len(),
                            line_count,
                            "the published transcript is complete"
                        );
                        for line in lines {
                            serde_json::from_str::<serde_json::Value>(line)
                                .unwrap_or_else(|error| panic!("torn line {line:?}: {error}"));
                        }
                        published_reads.fetch_add(1, Ordering::Relaxed);
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                        panic!("the session file vanished during a rewrite: {error}")
                    }
                    // A Windows sharing violation while the writer holds the
                    // file is not a torn read.
                    Err(_) => {}
                }
            }
        })
    };

    // The migrating open can lose to a Windows sharing violation against the
    // reader's handle; that transient is legitimate and retried. The live file
    // stays intact throughout.
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut session = None;
    while Instant::now() < deadline {
        match SessionManager::open(&path) {
            Ok(opened) => {
                session = Some(opened);
                break;
            }
            Err(_) => std::thread::sleep(Duration::from_millis(5)),
        }
    }
    let session = session.expect("the migrating open eventually succeeds");
    assert_eq!(session.entries().len(), 300);
    assert_eq!(
        session.header().and_then(|header| header.version),
        Some(CURRENT_SESSION_VERSION)
    );

    stop.store(true, Ordering::Relaxed);
    reader.join().expect("reader thread");
    assert!(
        published_reads.load(Ordering::Relaxed) > 0,
        "the reader observed complete transcripts"
    );
    assert!(
        temp_leftovers(&dir).is_empty(),
        "no temp file survives a successful rewrite: {:?}",
        temp_leftovers(&dir)
    );

    let lines = published_lines(&path);
    assert_eq!(lines.len(), line_count);
    assert_eq!(lines[0]["version"], CURRENT_SESSION_VERSION);

    std::fs::remove_dir_all(&dir).ok();
}

/// Two managers migrating the SAME v2 file concurrently publish ONE complete
/// transcript: never a mixture of both writers' bytes, never a missing file,
/// never a duplicated entry line, and never a shared temp name.
#[test]
fn concurrent_rewrites_of_one_path_publish_one_transcript() {
    let dir = temp_dir("concurrent");
    let path = dir.join("v2-concurrent.jsonl");
    write_v2_string_content_session(&path, 60);

    let barrier = Arc::new(std::sync::Barrier::new(2));
    let handles: Vec<_> = (0..2)
        .map(|_| {
            let barrier = Arc::clone(&barrier);
            let path = path.clone();
            std::thread::spawn(move || {
                barrier.wait();
                SessionManager::open(&path)
            })
        })
        .collect();
    let outcomes: Vec<_> = handles
        .into_iter()
        .map(|handle| handle.join().expect("open thread"))
        .collect();
    assert!(
        outcomes.iter().any(|outcome| outcome.is_ok()),
        "at least one migrating open must succeed: {outcomes:?}"
    );
    for outcome in outcomes.iter().flatten() {
        assert_eq!(outcome.entries().len(), 60);
    }
    drop(outcomes);

    let lines = published_lines(&path);
    assert_eq!(lines.len(), 61, "header + 60 entries");
    assert_eq!(lines[0]["version"], CURRENT_SESSION_VERSION);
    let ids: Vec<&str> = lines[1..]
        .iter()
        .map(|line| line["id"].as_str().expect("entry id"))
        .collect();
    let mut unique = ids.clone();
    unique.sort_unstable();
    unique.dedup();
    assert_eq!(unique.len(), ids.len(), "no duplicated entry line");
    assert!(
        temp_leftovers(&dir).is_empty(),
        "no temp file survives either rewrite: {:?}",
        temp_leftovers(&dir)
    );

    std::fs::remove_dir_all(&dir).ok();
}

/// A leftover temp file from a crashed writer must never be truncated by a
/// later rewrite (the old `File::create` opened the temp by name and would
/// have erased it in place, publishing another writer's partial bytes).
///
/// The temp names carry the process id and a process-global counter, so the
/// test squats on the NEXT few names the rewrite would draw. Every squatted
/// file must still hold its own bytes afterwards, and the rewrite must publish
/// a complete transcript by skipping to a free name.
#[test]
fn a_stale_temp_file_is_never_truncated_or_published() {
    let dir = temp_dir("stale-temp");
    let path = dir.join("v2.jsonl");
    write_v2_string_content_session(&path, 40);

    let first_sequence = pir::session_manager::next_rewrite_temp_sequence();
    // Fewer squatted names than the rewrite's attempt budget: the rewrite must
    // skip them and still publish, not exhaust its attempts. The name is built
    // exactly the way the rewrite builds it (the session path's own bytes plus
    // `.tmp.<pid>.<sequence>`).
    let stale: Vec<(u64, PathBuf)> = (first_sequence..first_sequence + 3)
        .map(|sequence| {
            let mut name = path.as_os_str().to_os_string();
            name.push(format!(".tmp.{}.{sequence}", std::process::id()));
            let stale = PathBuf::from(name);
            let body = format!("STALE WRITER BYTES {sequence}\n");
            std::fs::write(&stale, &body).expect("write stale temp");
            (sequence, stale)
        })
        .collect();

    let session = SessionManager::open(&path).expect("migrating open with stale temps present");
    assert_eq!(session.entries().len(), 40, "the transcript is complete");
    assert_eq!(
        session.header().and_then(|header| header.version),
        Some(CURRENT_SESSION_VERSION)
    );

    for (sequence, stale) in &stale {
        assert_eq!(
            std::fs::read_to_string(stale).expect("stale temp survives"),
            format!("STALE WRITER BYTES {sequence}\n"),
            "a leftover temp must never be truncated by another writer"
        );
    }
    let lines = published_lines(&path);
    assert_eq!(lines.len(), 41, "header + 40 entries");
    assert_eq!(lines[0]["type"], "session");

    std::fs::remove_dir_all(&dir).ok();
}

/// A zero-byte session file that is about to be filled by another writer must
/// not be mistaken for a new session: the open retries while the path reads as
/// zero bytes and lands on the real transcript instead of initializing (and
/// writing) an empty session over it.
#[test]
fn open_retries_a_transient_zero_byte_file_instead_of_reinitializing() {
    let dir = temp_dir("zero-byte");
    let path = dir.join("v2.jsonl");
    let payload_dir = temp_dir("zero-byte-payload");
    let payload_path = payload_dir.join("payload.jsonl");
    write_v2_string_content_session(&payload_path, 300);

    // The observable transient: the name exists and reads as ZERO bytes.
    std::fs::write(&path, "").expect("publish zero bytes");

    let (go, wait) = std::sync::mpsc::channel::<()>();
    let writer = {
        let path = path.clone();
        let payload_path = payload_path.clone();
        std::thread::spawn(move || {
            // Publish only AFTER the reader has started, and publish by RENAME
            // so the path never holds a partially written transcript (the
            // subject here is the zero-byte window, not torn writes).
            let _ = wait.recv();
            std::thread::sleep(Duration::from_millis(10));
            std::fs::rename(&payload_path, &path).expect("publish transcript");
        })
    };
    go.send(()).expect("signal writer");

    let session = SessionManager::open(&path).expect("open over a zero-byte transient");
    writer.join().expect("writer thread");

    assert!(
        !session.entries().is_empty(),
        "a zero-byte transient must not be read as a new empty session"
    );
    assert_eq!(session.entries().len(), 300);
    // The retry never wrote a fresh empty header over the transcript: the id
    // is the fixture's, not a newly generated session id.
    assert_eq!(session.id(), "22222222-3333-4444-5555-666666666666");

    std::fs::remove_dir_all(&dir).ok();
    std::fs::remove_dir_all(&payload_dir).ok();
}
