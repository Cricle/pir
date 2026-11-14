//! Unit tests for `src/coding_agent/session_resolve.rs`, moved out of the
//! inline `#[cfg(test)] mod tests` block (source hygiene: no inline test
//! modules in `src/`).

use std::path::{Path, PathBuf};

use pir::ai::types::{ContentBlock, TextContent, UserMessage, UserRole};
use pir::coding_agent::session_resolve::{resolve_session_plan, SessionFlags, SessionPlan};
use pir::session_manager::{FileEntry, MessageEntry, SessionHeader, SessionManager, StoredMessage};

fn temp_dir(label: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "pir-session-resolve-{label}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or_default()
    ));
    std::fs::create_dir_all(&dir).expect("temp dir");
    dir
}

/// One session file: header plus `messages` user entries (so a listing peek
/// would have real work to do if it ran). `mtime_secs` orders the files.
fn write_session(
    dir: &Path,
    name: &str,
    id: &str,
    cwd: &str,
    messages: usize,
    mtime_secs: u64,
) -> PathBuf {
    let path = dir.join(name);
    let mut text = String::new();
    text.push_str(
        &serde_json::to_string(&FileEntry::Session(SessionHeader::new(
            id,
            cwd,
            "2026-01-01T00:00:00.000Z".to_string(),
        )))
        .expect("header json"),
    );
    text.push('\n');
    for index in 0..messages {
        let entry = FileEntry::Message(MessageEntry {
            id: format!("m{index:04}"),
            parent_id: None,
            timestamp: "2026-01-01T00:00:00.000Z".to_string(),
            message: StoredMessage::User(UserMessage {
                role: UserRole::User,
                content: vec![ContentBlock::Text(TextContent {
                    text: format!("turn {index}"),
                    text_signature: None,
                })],
                tools_added: None,
                tools_removed: None,
                timestamp: 0,
                sections: None,
            }),
        });
        text.push_str(&serde_json::to_string(&entry).expect("entry json"));
        text.push('\n');
    }
    std::fs::write(&path, text).expect("write session");
    let mtime = std::time::UNIX_EPOCH + std::time::Duration::from_secs(mtime_secs);
    std::fs::File::options()
        .write(true)
        .open(&path)
        .expect("open session for mtime")
        .set_modified(mtime)
        .expect("set mtime");
    path
}

/// `--continue` (and headless `-r`) must pick the picker's default row —
/// the newest session whose header cwd matches — WITHOUT materializing a
/// listing. The listing peeks every file in the directory (message count,
/// first message, the search corpus marker); the resolution needs one path,
/// so `session_list_file_reads()` must stay 0 and the answer must equal
/// `list_sessions(..).first()`.
#[test]
fn continue_takes_newest_matching_without_listing() {
    let dir = temp_dir("continue");
    let cwd = dir.join("project");
    std::fs::create_dir_all(&cwd).unwrap();
    let cwd_text = cwd.to_string_lossy().into_owned();
    let other = dir.join("other-project");
    std::fs::create_dir_all(&other).unwrap();

    // Oldest first; the NEWEST file belongs to another project, so the
    // cwd filter has to skip a real candidate.
    write_session(&dir, "a.jsonl", "id-a", &cwd_text, 3, 1_000);
    write_session(&dir, "b.jsonl", "id-b", &cwd_text, 1, 2_000);
    let expected = write_session(&dir, "c.jsonl", "id-c", &cwd_text, 7, 3_000);
    write_session(&dir, "z.jsonl", "id-z", &other.to_string_lossy(), 5, 4_000);

    pir::session_manager::reset_session_list_file_reads();
    let plan = resolve_session_plan(
        &SessionFlags {
            continue_recent: true,
            ..SessionFlags::default()
        },
        &cwd,
        &dir,
        false,
    )
    .expect("plan");
    assert_eq!(plan, SessionPlan::Open(expected.clone()));
    assert_eq!(
        pir::session_manager::session_list_file_reads(),
        0,
        "`--continue` must not materialize a full listing"
    );

    // The listing answer is the same path (order parity), and it does read
    // the files — the counter proves the cheap path was taken above.
    let listed = SessionManager::list_sessions(&dir, Some(cwd_text.as_str()));
    assert_eq!(
        listed.first().map(|summary| summary.path.clone()),
        Some(expected)
    );
    assert!(pir::session_manager::session_list_file_reads() > 0);
}

/// Headless `-r` resolves to the same default row as `--continue`.
#[test]
fn headless_resume_takes_newest_matching_without_listing() {
    let dir = temp_dir("headless-resume");
    let cwd = dir.join("project");
    std::fs::create_dir_all(&cwd).unwrap();
    let cwd_text = cwd.to_string_lossy().into_owned();
    write_session(&dir, "a.jsonl", "id-a", &cwd_text, 2, 1_000);
    let expected = write_session(&dir, "b.jsonl", "id-b", &cwd_text, 2, 2_000);

    pir::session_manager::reset_session_list_file_reads();
    let plan = resolve_session_plan(
        &SessionFlags {
            resume: true,
            ..SessionFlags::default()
        },
        &cwd,
        &dir,
        false,
    )
    .expect("plan");
    assert_eq!(plan, SessionPlan::Open(expected));
    assert_eq!(pir::session_manager::session_list_file_reads(), 0);
}

/// No matching session: `--continue` starts a new one (unchanged).
#[test]
fn continue_without_sessions_starts_new() {
    let dir = temp_dir("continue-empty");
    let cwd = dir.join("project");
    std::fs::create_dir_all(&cwd).unwrap();
    let plan = resolve_session_plan(
        &SessionFlags {
            continue_recent: true,
            ..SessionFlags::default()
        },
        &cwd,
        &dir.join("sessions"),
        false,
    )
    .expect("plan");
    assert_eq!(plan, SessionPlan::New { id: None });
}
