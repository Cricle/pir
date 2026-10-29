//! W8-B durable lifecycle/storage residuals.
//!
//! Covers the observable contracts of the closed residuals:
//!
//! * committed watches queue frames without invoking consumer code (the
//!   primitive the runtime watch delivery task drains off the mutation line);
//! * `beforeClose` + `writeAtClose` run after the admission seal, land their
//!   batch, and run exactly once;
//! * SQLite request keys are unique, newest-wins and deterministic across a
//!   reopen, including databases that still carry pre-migration duplicates;
//! * pi-format round trips keep an opaque payload verbatim, keep non-text
//!   `custom_message` content, and remap `branch_summary.fromId` together with
//!   the imported provisional ids.
//!
//! This file is not registered in `tests/g_durable.rs`; the foreman owns suite
//! registration.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use serde_json::{json, Value};

use pir::durable::ids::{EntryId, ROOT_CONVERSATION_ID};
use pir::durable::pi_format::export::stable_pi_id;
use pir::durable::pi_format::{import_session_text, render_session, ExportInput, ImportedSession};
use pir::durable::session::transaction::HeadTarget;
use pir::durable::types::EntryRecord;
use pir::durable::Context;

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

struct TempDir(PathBuf);

impl TempDir {
    fn new(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "pir-w8b-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|duration| duration.as_nanos())
                .unwrap_or(0)
        ));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("create temp dir");
        Self(path)
    }

    fn path(&self) -> &std::path::Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        for _ in 0..20 {
            match std::fs::remove_dir_all(&self.0) {
                Ok(()) => return,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => return,
                Err(_) => std::thread::sleep(std::time::Duration::from_millis(10)),
            }
        }
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn header(id: &str) -> String {
    format!(
        r#"{{"type":"session","version":3,"id":"{id}","timestamp":"1970-01-01T00:00:00.000Z","cwd":"C:\\repo"}}"#
    )
}

fn join_lines(lines: &[&str]) -> String {
    let mut text = lines.join("\n");
    text.push('\n');
    text
}

fn parse_lines(rendered: &str) -> Vec<Value> {
    rendered
        .lines()
        .map(|line| serde_json::from_str(line).expect("rendered line is JSON"))
        .collect()
}

/// The documented caller-side conversion: a draft's position is its
/// provisional id and `SelfEntry` resolves to the entry's own id.
fn as_records(imported: &ImportedSession) -> Vec<EntryRecord> {
    as_records_with_ids(imported, None)
}

fn as_records_with_ids(imported: &ImportedSession, minted: Option<&[EntryId]>) -> Vec<EntryRecord> {
    imported
        .entries
        .iter()
        .enumerate()
        .map(|(index, draft)| {
            let id = minted
                .and_then(|ids| ids.get(index).copied())
                .unwrap_or_else(|| EntryId::from_raw(index as i64 + 1));
            EntryRecord {
                id,
                conversation_id: ROOT_CONVERSATION_ID,
                kind: draft.kind.clone(),
                model: draft.model.clone(),
                data: draft.data.clone(),
                head: draft.head.map(|head| match head {
                    HeadTarget::SelfEntry => id,
                    HeadTarget::Entry(target) => target,
                }),
                edits: draft.edits.clone(),
                by_task_id: None,
            }
        })
        .collect()
}

// ---------------------------------------------------------------------------
// (a) committed watches queue; only the explicit drain runs consumer code
// ---------------------------------------------------------------------------

#[test]
fn committed_watch_queues_frames_without_invoking_the_listener() {
    use pir::durable::session::observation::CommittedWatch;

    let watch = CommittedWatch::new(Some(json!({"n": 0}).as_object().cloned().unwrap()), 1);
    let seen: Arc<Mutex<Vec<i64>>> = Arc::new(Mutex::new(Vec::new()));

    // `advance` is what a commit listener runs on the line: it must only append
    // to the bounded frame queue.
    {
        let core_handle = watch.core();
        let mut core = core_handle
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for n in 1..=3 {
            core.advance(
                Some(json!({"n": n}).as_object().cloned().unwrap()),
                Vec::new(),
                1,
                Context::background(),
            );
        }
    }
    assert!(
        seen.lock().unwrap().is_empty(),
        "advancing from the commit line never invokes the listener"
    );

    let captured = seen.clone();
    watch.start(move |frame| {
        let n = frame
            .value
            .as_ref()
            .and_then(|value| value.get("n"))
            .and_then(Value::as_i64)
            .unwrap_or(-1);
        captured.lock().unwrap().push(n);
        true
    });
    assert_eq!(
        *seen.lock().unwrap(),
        vec![1, 2, 3],
        "the delivery pass drains the queued frames FIFO"
    );
}

// ---------------------------------------------------------------------------
// (a) beforeClose + writeAtClose: sealed admission, one close-time batch
// ---------------------------------------------------------------------------

#[tokio::test]
async fn before_close_write_at_close_lands_once_after_the_seal() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use pir::durable::session::facade::{ConversationOwnership, Session};
    use pir::durable::session::line::SessionOptions;
    use pir::durable::storage::jsonl::JsonlStorage;
    use pir::durable::types::StorageWrite;
    use pir::durable::Storage;

    let dir = TempDir::new("write-at-close");
    let storage = JsonlStorage::open(dir.path()).await.expect("open jsonl");
    let session = Session::open(Arc::new(storage), SessionOptions::default());
    let conversation = session
        .create_conversation(
            ConversationOwnership::Ownerless,
            None,
            Context::background(),
        )
        .await
        .expect("create conversation")
        .id;

    let runs = Arc::new(AtomicUsize::new(0));
    let hook_runs = runs.clone();
    let hook_session = session.clone();
    session
        .handle()
        .register_before_close(Box::new(move || {
            Box::pin(async move {
                hook_runs.fetch_add(1, Ordering::SeqCst);
                // The write is computed from the final line state after the
                // seal; it must still reach storage.
                hook_session
                    .write_at_close(move |_state| {
                        vec![StorageWrite::Entry {
                            value: EntryRecord {
                                id: EntryId::from_raw(50),
                                conversation_id: conversation,
                                kind: "pi.user".to_owned(),
                                model: None,
                                data: Some(json!({ "close": true })),
                                head: None,
                                edits: None,
                                by_task_id: None,
                            },
                        }]
                    })
                    .await
                    .expect("write at close");
            })
        }))
        .expect("register before close");

    session.close().await.expect("close");
    assert_eq!(runs.load(Ordering::SeqCst), 1);

    // A second close must not rerun the hook.
    session.close().await.expect("second close");
    assert_eq!(runs.load(Ordering::SeqCst), 1, "beforeClose runs once");

    // Reopen the log: the close-time batch was committed.
    let reopened = JsonlStorage::open(dir.path()).await.expect("reopen jsonl");
    let entry = reopened
        .entry(EntryId::from_raw(50))
        .await
        .expect("read entry")
        .expect("the close-time write survived");
    assert_eq!(entry.entry.data, Some(json!({ "close": true })));
    reopened.close().await.expect("close reopened");
}

// ---------------------------------------------------------------------------
// (c) SQLite submission request keys
// ---------------------------------------------------------------------------

#[cfg(any(feature = "extensions", feature = "context-mode"))]
mod sqlite_request_keys {
    use super::*;
    use pir::durable::ids::SubmissionId;
    use pir::durable::storage::sqlite::{
        apply_sqlite_migrations, apply_sqlite_migrations_with, create_sqlite_migrations,
        SqliteStorage,
    };
    use pir::durable::types::{
        Storage, StorageWrite, SubmissionRecord, SubmissionStatus, SubmissionType,
    };
    use pir::durable::{ConversationId, SubmissionQuery};

    fn submission_record(
        id: i64,
        conversation_id: i64,
        request_id: Option<&str>,
        status: SubmissionStatus,
    ) -> SubmissionRecord {
        SubmissionRecord {
            trace: None,
            id: SubmissionId::from_raw(id),
            conversation_id: ConversationId::from_raw(conversation_id),
            request_id: request_id.map(str::to_owned),
            submission_type: SubmissionType::Input,
            status,
            entry: None,
            answer: None,
            reason: None,
            detail: None,
        }
    }

    fn write(record: SubmissionRecord) -> StorageWrite {
        StorageWrite::Submission { value: record }
    }

    #[tokio::test]
    async fn request_key_is_unique_newest_wins_and_stable_across_reopen() {
        let dir = TempDir::new("sqlite-request-key");
        let path = dir.path().join("durable.sqlite");
        let storage = SqliteStorage::open(&path).await.expect("open sqlite");

        storage
            .commit(vec![write(submission_record(
                10,
                1,
                Some("req"),
                SubmissionStatus::Queued,
            ))])
            .await
            .expect("first write");
        storage
            .commit(vec![write(submission_record(
                20,
                1,
                Some("req"),
                SubmissionStatus::Placed,
            ))])
            .await
            .expect("replacement write");

        let by_request = storage
            .submission_by_request(ConversationId::from_raw(1), "req")
            .await
            .expect("lookup")
            .expect("the request key resolves");
        assert_eq!(
            by_request.id,
            SubmissionId::from_raw(20),
            "the newest holder wins"
        );
        assert!(
            storage
                .submission(SubmissionId::from_raw(10))
                .await
                .expect("lookup old id")
                .is_none(),
            "the unique key leaves one holder for the request"
        );

        // Two writes to the same key in one batch: the later write wins and
        // the batch keeps exactly one holder.
        storage
            .commit(vec![
                write(submission_record(
                    30,
                    1,
                    Some("batch"),
                    SubmissionStatus::Queued,
                )),
                write(submission_record(
                    40,
                    1,
                    Some("batch"),
                    SubmissionStatus::Queued,
                )),
            ])
            .await
            .expect("same-batch key");
        assert_eq!(
            storage
                .submission_by_request(ConversationId::from_raw(1), "batch")
                .await
                .expect("lookup batch")
                .expect("batch key")
                .id,
            SubmissionId::from_raw(40)
        );

        // The index is a real unique (partial) index, not just a query order.
        let connection = rusqlite::Connection::open(&path).expect("raw connection");
        let sql: String = connection
            .query_row(
                "SELECT sql FROM sqlite_master WHERE type = 'index' AND name = 'submissions_by_request'",
                [],
                |row| row.get(0),
            )
            .expect("index exists");
        assert!(
            sql.to_ascii_uppercase().contains("UNIQUE"),
            "submissions_by_request must be unique: {sql}"
        );
        assert!(
            sql.to_ascii_uppercase().contains("WHERE"),
            "the index is partial on non-null request ids: {sql}"
        );
        drop(connection);
        storage.close().await.expect("close sqlite");

        let reopened = SqliteStorage::open(&path).await.expect("reopen sqlite");
        assert_eq!(
            reopened
                .submission_by_request(ConversationId::from_raw(1), "req")
                .await
                .expect("lookup after reopen")
                .expect("key survives")
                .id,
            SubmissionId::from_raw(20)
        );
        let page = reopened
            .scan_submissions(
                SubmissionQuery {
                    conversation_id: Some(ConversationId::from_raw(1)),
                    ..SubmissionQuery::default()
                },
                100,
                None,
            )
            .await
            .expect("scan");
        let keys: Vec<Option<&str>> = page
            .items
            .iter()
            .map(|item| item.request_id.as_deref())
            .collect();
        assert_eq!(
            keys.iter().filter(|key| **key == Some("req")).count(),
            1,
            "one record per request key: {keys:?}"
        );
        reopened.close().await.expect("close reopened");
    }

    #[test]
    fn migration_three_dedupes_legacy_request_keys_and_installs_the_unique_index() {
        let dir = TempDir::new("sqlite-migration-three");
        let path = dir.path().join("legacy.sqlite");
        let migrations = create_sqlite_migrations();

        {
            let mut connection = rusqlite::Connection::open(&path).expect("open raw");
            // A database created before the unique-key migration.
            apply_sqlite_migrations_with(&mut connection, &migrations[..2])
                .expect("apply v1 and v2");
            let request = serde_json::to_string("dup").expect("encode key");
            for (id, request_id) in [
                (41i64, Some(request.clone())),
                (42, Some(request.clone())),
                (43, Some(serde_json::to_string("other").unwrap())),
                (44, None),
            ] {
                connection
                    .execute(
                        "INSERT INTO submissions (id, conversation_id, request_id, status, record)
                         VALUES (?1, 1, ?2, 'queued', '{\"id\": 1}')",
                        rusqlite::params![id, request_id],
                    )
                    .expect("insert legacy row");
            }
        }

        {
            let mut connection = rusqlite::Connection::open(&path).expect("reopen raw");
            apply_sqlite_migrations(&mut connection).expect("apply migration 3");
            let request = serde_json::to_string("dup").expect("encode key");
            let count: i64 = connection
                .query_row(
                    "SELECT COUNT(*) FROM submissions WHERE request_id = ?1",
                    rusqlite::params![request.clone()],
                    |row| row.get(0),
                )
                .expect("count dup rows");
            assert_eq!(count, 1, "duplicates collapse to the newest row");
            let kept: i64 = connection
                .query_row(
                    "SELECT id FROM submissions WHERE request_id = ?1",
                    rusqlite::params![request],
                    |row| row.get(0),
                )
                .expect("kept row");
            assert_eq!(kept, 42, "MAX(id) survives, matching the lookup order");
            let null_rows: i64 = connection
                .query_row(
                    "SELECT COUNT(*) FROM submissions WHERE request_id IS NULL",
                    [],
                    |row| row.get(0),
                )
                .expect("count null rows");
            assert_eq!(null_rows, 1, "the partial index ignores NULL keys");
        }
    }
}

// ---------------------------------------------------------------------------
// (d) pi_format round trips
// ---------------------------------------------------------------------------

#[test]
fn custom_entry_keeps_its_opaque_payload_across_export() {
    let payload = json!({
        "nested": { "arr": [1, 2, { "deep": true }], "nil": null },
        "scalar": 7,
        "text": "héllo \"quoted\"\nline",
        "unicode": "雪"
    });
    let text = join_lines(&[
        header("sess-custom-payload").as_str(),
        &format!(
            r#"{{"type":"custom","id":"aaaaaaaa","parentId":null,"timestamp":"1970-01-01T00:00:01.000Z","customType":"opaque_blob","data":{payload}}}"#
        ),
    ]);
    let imported = import_session_text(&text).expect("import custom");

    assert_eq!(imported.entries.len(), 1);
    let data = imported.entries[0].data.as_ref().expect("custom data");
    assert_eq!(data["source"], "custom");
    assert_eq!(data["customType"], "opaque_blob");
    assert_eq!(data["data"], payload, "the payload is kept verbatim");

    let records = as_records(&imported);
    let labels = BTreeMap::new();
    let input = ExportInput {
        header: imported.header.clone(),
        entries: &records,
        agent: None,
        usage: None,
        session_info: None,
        labels: &labels,
        opaque: &imported.opaque,
    };
    let rendered = render_session(&input).expect("render custom");
    let lines = parse_lines(&rendered);
    let custom = lines
        .iter()
        .find(|line| line["type"] == "custom")
        .expect("custom line");
    assert_eq!(custom["customType"], "opaque_blob");
    assert_eq!(
        custom["data"], payload,
        "export re-emits the opaque payload"
    );

    // And a second import sees exactly the same payload.
    let reimported = import_session_text(&rendered).expect("re-import custom");
    assert_eq!(
        reimported.entries[0].data.as_ref().expect("data")["data"],
        payload
    );
}

#[test]
fn custom_message_keeps_non_text_content_across_export() {
    let content = json!([
        { "type": "text", "text": "look at this" },
        {
            "type": "image",
            "data": "aGVsbG8=",
            "mimeType": "image/png"
        }
    ]);
    let text = join_lines(&[
        header("sess-custom-message").as_str(),
        &format!(
            r#"{{"type":"custom_message","id":"bbbbbbbb","parentId":null,"timestamp":"1970-01-01T00:00:02.000Z","customType":"inject","content":{content},"display":false,"details":{{"k":"v"}}}}"#
        ),
    ]);
    let imported = import_session_text(&text).expect("import custom_message");

    assert!(imported.opaque.is_empty(), "custom_message is mapped");
    let draft = &imported.entries[0];
    let data = draft.data.as_ref().expect("custom_message data");
    assert_eq!(data["content"], content, "non-text blocks stay in data");
    assert_eq!(
        draft.model.as_ref().unwrap()[0]["content"],
        content,
        "the model projection keeps every block type"
    );

    let records = as_records(&imported);
    let labels = BTreeMap::new();
    let input = ExportInput {
        header: imported.header.clone(),
        entries: &records,
        agent: None,
        usage: None,
        session_info: None,
        labels: &labels,
        opaque: &[],
    };
    let rendered = render_session(&input).expect("render custom_message");
    let lines = parse_lines(&rendered);
    let custom = lines
        .iter()
        .find(|line| line["type"] == "custom_message")
        .expect("custom_message line");
    assert_eq!(
        custom["content"], content,
        "export writes the original block array, not flattened text"
    );
    assert_eq!(custom["display"], false);
    assert_eq!(custom["details"]["k"], "v");

    let reimported = import_session_text(&rendered).expect("re-import custom_message");
    assert_eq!(
        reimported.entries[0].data.as_ref().expect("data")["content"],
        content
    );
}

#[test]
fn opaque_lines_are_re_emitted_with_their_payload() {
    let unknown = r#"{"type":"mystery_future","id":"77777777","parentId":null,"timestamp":"1970-01-01T00:00:03.000Z","payload":{"x":1},"nested":{"deep":[1,2]}}"#;
    let text = join_lines(&[
        header("sess-opaque").as_str(),
        unknown,
        r#"{"type":"message","id":"aaaaaaaa","parentId":"77777777","timestamp":"1970-01-01T00:00:04.000Z","message":{"role":"user","content":[{"type":"text","text":"still imported"}],"timestamp":1}}"#,
    ]);
    let imported = import_session_text(&text).expect("import opaque");
    assert_eq!(imported.opaque.len(), 1);

    let records = as_records(&imported);
    let labels = BTreeMap::new();
    let input = ExportInput {
        header: imported.header.clone(),
        entries: &records,
        agent: None,
        usage: None,
        session_info: None,
        labels: &labels,
        opaque: &imported.opaque,
    };
    let rendered = render_session(&input).expect("render opaque");
    let lines = parse_lines(&rendered);
    let opaque = lines
        .iter()
        .find(|line| line["type"] == "mystery_future")
        .expect("opaque line is re-emitted");
    assert_eq!(opaque["id"], "77777777");
    assert_eq!(opaque["payload"], json!({ "x": 1 }));
    assert_eq!(opaque["nested"], json!({ "deep": [1, 2] }));
    assert_eq!(
        lines
            .iter()
            .position(|line| line["type"] == "mystery_future"),
        Some(1),
        "the opaque line keeps its position before the mapped entry"
    );
}

#[test]
fn remap_updates_branch_summary_from_entry_and_from_id() {
    let text = join_lines(&[
        header("sess-branch-remap").as_str(),
        r#"{"type":"message","id":"aaaaaaaa","parentId":null,"timestamp":"1970-01-01T00:00:01.000Z","message":{"role":"user","content":[{"type":"text","text":"hi"}],"timestamp":1}}"#,
        r#"{"type":"message","id":"bbbbbbbb","parentId":"aaaaaaaa","timestamp":"1970-01-01T00:00:02.000Z","message":{"role":"assistant","api":"test","provider":"test","model":"m","content":[{"type":"text","text":"hello"}],"stopReason":"stop","usage":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0,"totalTokens":0,"cost":{"input":0.0,"output":0.0,"cacheRead":0.0,"cacheWrite":0.0,"total":0.0}},"timestamp":2}}"#,
        r#"{"type":"branch_summary","id":"cccccccc","parentId":"bbbbbbbb","timestamp":"1970-01-01T00:00:03.000Z","fromId":"bbbbbbbb","summary":"branch"}"#,
    ]);
    let mut imported = import_session_text(&text).expect("import branch");
    let branch = imported
        .entries
        .iter()
        .find(|draft| {
            draft
                .data
                .as_ref()
                .is_some_and(|data| data["kind"] == "branch")
        })
        .expect("branch summary draft");
    assert_eq!(branch.data.as_ref().unwrap()["fromEntry"], 2);
    assert_eq!(branch.data.as_ref().unwrap()["fromId"], "bbbbbbbb");

    let minted = [
        EntryId::from_raw(100),
        EntryId::from_raw(101),
        EntryId::from_raw(102),
    ];
    imported.remap_entry_ids(&minted).expect("remap");
    let branch = imported
        .entries
        .iter()
        .find(|draft| {
            draft
                .data
                .as_ref()
                .is_some_and(|data| data["kind"] == "branch")
        })
        .expect("branch summary draft");
    assert_eq!(
        branch.data.as_ref().unwrap()["fromEntry"],
        101,
        "the durable target follows the minted id"
    );
    assert_eq!(
        branch.data.as_ref().unwrap()["fromId"],
        stable_pi_id(101),
        "the raw pi id is kept consistent with the exported target"
    );
    assert_eq!(imported.entry_ids["bbbbbbbb"], EntryId::from_raw(101));

    let records = as_records_with_ids(&imported, Some(&minted[..]));
    let labels = BTreeMap::new();
    let input = ExportInput {
        header: imported.header.clone(),
        entries: &records,
        agent: None,
        usage: None,
        session_info: None,
        labels: &labels,
        opaque: &[],
    };
    let rendered = render_session(&input).expect("render branch");
    let lines = parse_lines(&rendered);
    let branch = lines
        .iter()
        .find(|line| line["type"] == "branch_summary")
        .expect("branch_summary line");
    assert_eq!(
        branch["fromId"],
        stable_pi_id(101),
        "the exported reference resolves to the minted target's pi id"
    );
    assert!(
        lines
            .iter()
            .any(|line| { line["type"] == "message" && line["id"] == stable_pi_id(101) }),
        "the referenced pi id exists in the exported chain"
    );
}

#[test]
fn custom_role_message_round_trips_as_a_message() {
    let text = join_lines(&[
        header("sess-custom-role").as_str(),
        r#"{"type":"message","id":"dddddddd","parentId":null,"timestamp":"1970-01-01T00:00:05.000Z","message":{"role":"custom","customType":"hook","content":[{"type":"text","text":"note"}],"display":true,"timestamp":5000}}"#,
    ]);
    let imported = import_session_text(&text).expect("import custom role");
    assert!(imported.opaque.is_empty());
    assert_eq!(imported.entries[0].kind, "pi.custom");
    assert_eq!(
        imported.entries[0].data.as_ref().expect("marker")["source"],
        "message"
    );

    let records = as_records(&imported);
    let labels: BTreeMap<EntryId, String> = BTreeMap::new();
    let input = ExportInput {
        header: imported.header.clone(),
        entries: &records,
        agent: None,
        usage: None,
        session_info: None,
        labels: &labels,
        opaque: &[],
    };
    let rendered = render_session(&input).expect("render custom role");
    let lines = parse_lines(&rendered);
    let message = lines
        .iter()
        .find(|line| line["type"] == "message")
        .expect("message line");
    assert_eq!(message["message"]["role"], "custom");
    assert_eq!(message["message"]["customType"], "hook");
    assert_eq!(
        message["message"]["content"],
        json!([{ "type": "text", "text": "note" }])
    );
    assert_eq!(message["message"]["display"], true);
}
