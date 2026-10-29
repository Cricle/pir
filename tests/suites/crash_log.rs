//! Crash log (upstream `core/crash-log.ts`): panics append a bounded,
//! timestamped record to `<agentDir>/crashes.json` and the TUI restores the
//! terminal before the panic message prints.

use std::path::PathBuf;

use pir::coding_agent::crash_log::{
    clear_crash_log, crash_log_path, read_crash_log, record_crash, take_unnotified_crash,
    CrashKind, CrashRecord, MAX_CRASH_RECORDS,
};

fn temp_dir(name: &str) -> PathBuf {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or(0);
    let dir = std::env::temp_dir().join(format!("pir-crash-log-{name}-{stamp}"));
    let _ = std::fs::create_dir_all(&dir);
    dir
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or(0)
}

/// Append `count` distinct records to `path`.
fn append_records(path: &std::path::Path, count: usize) -> Vec<CrashRecord> {
    (0..count)
        .map(|index| {
            record_crash(
                CrashKind::UncaughtException,
                &format!("boom {index}"),
                Some("at src/main.rs:1:1"),
                None,
                "C:\\work",
                path,
            )
            .expect("record written")
        })
        .collect()
}

#[test]
fn crash_log_path_lives_under_the_agent_dir() {
    let dir = temp_dir("path");
    assert_eq!(crash_log_path(&dir), dir.join("crashes.json"));
}

#[test]
fn record_crash_appends_a_bounded_entry() {
    let dir = temp_dir("append");
    let path = crash_log_path(&dir);

    let record = record_crash(
        CrashKind::UncaughtException,
        "unexpected panic",
        Some("stack line"),
        Some("session.jsonl"),
        "C:\\project",
        &path,
    )
    .expect("record written");

    // Upstream record shape: timestamp/version/kind/message/stack/
    // sessionFile/cwd, with `notified` absent until announced.
    assert_eq!(record.kind, CrashKind::UncaughtException);
    assert_eq!(record.message, "unexpected panic");
    assert_eq!(record.stack.as_deref(), Some("stack line"));
    assert_eq!(record.session_file.as_deref(), Some("session.jsonl"));
    assert_eq!(record.cwd, "C:\\project");
    assert_eq!(record.version, env!("CARGO_PKG_VERSION"));
    assert!(record.notified.is_none());
    assert!(!record.timestamp.is_empty());

    let on_disk = read_crash_log(&path);
    assert_eq!(on_disk.len(), 1);
    assert_eq!(on_disk[0].message, "unexpected panic");

    // The file is pretty-printed JSON with a trailing newline (upstream
    // `JSON.stringify(records, null, 2) + "\n"`).
    let raw = std::fs::read_to_string(&path).expect("read crash log");
    assert!(raw.starts_with("[\n"), "expected pretty JSON: {raw}");
    assert!(raw.ends_with("]\n"), "expected trailing newline: {raw}");

    // camelCase keys match the upstream JSON.
    assert!(
        raw.contains("\"sessionFile\""),
        "camelCase keys expected: {raw}"
    );
    assert!(
        raw.contains("\"uncaught_exception\""),
        "kind rename missing: {raw}"
    );
}

#[test]
fn record_crash_enforces_the_newest_first_cap() {
    let dir = temp_dir("cap");
    let path = crash_log_path(&dir);

    // Write well past the cap; only the newest MAX_CRASH_RECORDS survive.
    append_records(&path, MAX_CRASH_RECORDS + 4);

    let records = read_crash_log(&path);
    assert_eq!(records.len(), MAX_CRASH_RECORDS, "cap not enforced");
    // The oldest entries were dropped, newest kept (upstream `slice(-MAX)`).
    assert_eq!(
        records.last().map(|record| record.message.as_str()),
        Some("boom 8"),
        "newest record must be retained"
    );
    assert_eq!(
        records.first().map(|record| record.message.as_str()),
        Some("boom 4"),
        "only the newest window must remain"
    );
}

#[test]
fn record_crash_replaces_malformed_files() {
    let dir = temp_dir("malformed");
    let path = crash_log_path(&dir);
    std::fs::write(&path, "not json at all").expect("write");

    // A malformed file reads as empty, so the append starts fresh.
    assert!(read_crash_log(&path).is_empty());
    let record = record_crash(CrashKind::FatalError, "fatal", None, None, "C:\\x", &path)
        .expect("record written");
    assert_eq!(record.kind, CrashKind::FatalError);

    let records = read_crash_log(&path);
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].message, "fatal");
}

#[test]
fn record_crash_creates_a_missing_agent_dir() {
    let dir = temp_dir("mkdir").join("nested").join("agent");
    let path = crash_log_path(&dir);
    assert!(!path.exists());

    record_crash(
        CrashKind::UncaughtException,
        "x",
        None,
        None,
        "C:\\x",
        &path,
    )
    .expect("record written");
    assert!(path.exists(), "the agent dir must be created");
}

#[test]
fn read_crash_log_drops_entries_missing_required_fields() {
    let dir = temp_dir("filter");
    let path = crash_log_path(&dir);
    std::fs::write(
        &path,
        r#"[
            {"timestamp":"2026-01-01T00:00:00.000Z","version":"1","kind":"uncaught_exception","message":"keep","stack":null,"sessionFile":null,"cwd":"/x"},
            {"version":"1","kind":"uncaught_exception","message":"no timestamp","stack":null,"sessionFile":null,"cwd":"/x"},
            {"timestamp":"2026-01-01T00:00:00.000Z","version":"1","kind":"uncaught_exception","stack":null,"sessionFile":null,"cwd":"/x"}
        ]"#,
    )
    .expect("write");

    // The entry without a message and the one without a timestamp are dropped.
    let records = read_crash_log(&path);
    assert_eq!(records.len(), 1, "only the well-formed record must remain");
    assert_eq!(records[0].message, "keep");
}

#[test]
fn take_unnotified_crash_marks_the_record_announced() {
    let dir = temp_dir("take");
    let path = crash_log_path(&dir);
    record_crash(
        CrashKind::UncaughtException,
        "report me",
        None,
        None,
        "C:\\x",
        &path,
    )
    .expect("record written");

    let taken = take_unnotified_crash(&path, now_ms()).expect("fresh crash reported");
    assert_eq!(taken.message, "report me");

    // The record is now marked notified: a second take reports nothing.
    assert!(take_unnotified_crash(&path, now_ms()).is_none());
    assert_eq!(read_crash_log(&path)[0].notified, Some(true));
}

#[test]
fn take_unnotified_crash_ignores_stale_records() {
    let dir = temp_dir("stale");
    let path = crash_log_path(&dir);
    // A record timestamped 8 days ago is past the 7-day report window.
    let stale = "2020-01-01T00:00:00.000Z";
    std::fs::write(
        &path,
        format!(
            r#"[{{"timestamp":"{stale}","version":"1","kind":"uncaught_exception","message":"old","stack":null,"sessionFile":null,"cwd":"/x"}}]"#
        ),
    )
    .expect("write");

    assert!(take_unnotified_crash(&path, now_ms()).is_none());
}

#[test]
fn clear_crash_log_removes_the_file() {
    let dir = temp_dir("clear");
    let path = crash_log_path(&dir);
    record_crash(
        CrashKind::UncaughtException,
        "x",
        None,
        None,
        "C:\\x",
        &path,
    )
    .expect("record written");
    assert!(path.exists());

    clear_crash_log(&path);
    assert!(!path.exists(), "the crash log must be removed");
    // Clearing a missing file is a no-op, not an error.
    clear_crash_log(&path);
    assert!(read_crash_log(&path).is_empty());
}
