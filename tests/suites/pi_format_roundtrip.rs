//! pi session format (v3 JSONL) <-> durable records: full round trips, the six
//! documented lossy mappings, opaque preservation, and import error paths.
//!
//! The acceptance shape is `tests/suites/session_manager.rs`'s
//! `golden_record_serialization_matches_upstream_shapes`: a handwritten v3
//! file is imported into entry drafts, the caller converts the drafts to
//! `EntryRecord`s (ids by 1-based position, `SelfEntry` heads to the entry's
//! own id), `render_session` exports them, and `SessionManager::open` reads the
//! result back into the same model-visible projection (timestamps included).

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde_json::{Map, Value};

use pir::ai::types::{ContentBlock, Message};
use pir::durable::ids::{EntryId, ROOT_CONVERSATION_ID};
use pir::durable::pi_format::{
    read_session_file, render_session, write_session, ExportInput, ImportError, ImportedSession,
};
use pir::durable::session::transaction::HeadTarget;
use pir::durable::types::{ContextEditAction, EntryRecord};
use pir::session_manager::{SessionHeader, SessionManager};

// ---------------------------------------------------------------------------
// Fixtures and conversion helpers
// ---------------------------------------------------------------------------

struct TempDir(PathBuf);

impl TempDir {
    fn new(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "pir-pi-roundtrip-{label}-{}-{}",
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

    fn file(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A v3 session header with a fixed epoch timestamp so entry timestamps can be
/// asserted as plain milliseconds.
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

fn write_fixture(dir: &TempDir, name: &str, text: &str) -> PathBuf {
    let path = dir.file(name);
    std::fs::write(&path, text).expect("write fixture");
    path
}

fn import_fixture(label: &str, text: &str) -> ImportedSession {
    let dir = TempDir::new(label);
    let path = write_fixture(&dir, "session.jsonl", text);
    read_session_file(&path).expect("import fixture")
}

fn parse_lines(rendered: &str) -> Vec<Value> {
    rendered
        .lines()
        .map(|line| serde_json::from_str(line).expect("rendered line is JSON"))
        .collect()
}

/// The caller-side conversion documented by lossy mapping #2: draft position
/// gives the provisional id, and a `SelfEntry` head resolves to the entry's own
/// id.
fn as_records(imported: &ImportedSession) -> Vec<EntryRecord> {
    imported
        .entries
        .iter()
        .enumerate()
        .map(|(index, draft)| {
            let id = EntryId::from_raw(index as i64 + 1);
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

fn session_info_map(name: Option<&str>) -> Map<String, Value> {
    let mut map = Map::new();
    if let Some(name) = name {
        map.insert("name".to_owned(), Value::String(name.to_owned()));
    }
    map
}

/// The text and timestamp of a projected message (all projected messages in
/// these fixtures are user-role: user text, compaction summary, custom message).
fn user_text_and_timestamp(message: &Message) -> (String, u64) {
    match message {
        Message::User(user) => {
            let text = user
                .content
                .iter()
                .filter_map(|block| match block {
                    ContentBlock::Text(text) => Some(text.text.as_str()),
                    _ => None,
                })
                .collect::<String>();
            (text, user.timestamp)
        }
        other => panic!("expected a user message, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Full round trip
// ---------------------------------------------------------------------------

/// Every requested entry family, in file order; all parents form one chain.
fn full_session_fixture() -> String {
    let header = header("sess-roundtrip");
    join_lines(&[
        header.as_str(),
        r#"{"type":"message","id":"aaaaaaaa","parentId":null,"timestamp":"1970-01-01T00:00:01.000Z","message":{"role":"user","content":[{"type":"text","text":"hi"}],"timestamp":1000}}"#,
        r#"{"type":"message","id":"bbbbbbbb","parentId":"aaaaaaaa","timestamp":"1970-01-01T00:00:02.000Z","message":{"role":"assistant","api":"test","provider":"test","model":"m","content":[{"type":"text","text":"hello"}],"stopReason":"stop","usage":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0,"totalTokens":0,"cost":{"input":0.0,"output":0.0,"cacheRead":0.0,"cacheWrite":0.0,"total":0.0}},"timestamp":2000}}"#,
        r#"{"type":"compaction","id":"cccccccc","parentId":"bbbbbbbb","timestamp":"1970-01-01T00:00:03.000Z","summary":"summary so far","firstKeptEntryId":"aaaaaaaa","tokensBefore":1234}"#,
        r#"{"type":"context_edit","id":"dddddddd","parentId":"cccccccc","timestamp":"1970-01-01T00:00:04.000Z","targetId":"bbbbbbbb","replacement":null}"#,
        r#"{"type":"context_edit","id":"eeeeeeee","parentId":"dddddddd","timestamp":"1970-01-01T00:00:05.000Z","targetId":"aaaaaaaa","replacement":{"content":[{"type":"text","text":"edited prompt"}]}}"#,
        r#"{"type":"label","id":"ffffffff","parentId":"eeeeeeee","timestamp":"1970-01-01T00:00:06.000Z","targetId":"aaaaaaaa","label":"start"}"#,
        r#"{"type":"session_info","id":"11111111","parentId":"ffffffff","timestamp":"1970-01-01T00:00:07.000Z","name":"demo"}"#,
        r#"{"type":"model_change","id":"22222222","parentId":"11111111","timestamp":"1970-01-01T00:00:08.000Z","provider":"anthropic","modelId":"claude-sonnet-4-5"}"#,
        r#"{"type":"thinking_level_change","id":"33333333","parentId":"22222222","timestamp":"1970-01-01T00:00:09.000Z","thinkingLevel":"high"}"#,
        r#"{"type":"usage","id":"44444444","parentId":"33333333","timestamp":"1970-01-01T00:00:10.000Z","kind":"cache_warm","provider":"anthropic","model":"claude-sonnet-4-5","usage":{"input":3,"output":4,"cacheRead":5,"cacheWrite":0,"totalTokens":12,"cost":{"input":0.0,"output":0.0,"cacheRead":0.0,"cacheWrite":0.0,"total":0.0}},"note":"warmed"}"#,
        r#"{"type":"custom","id":"55555555","parentId":"44444444","timestamp":"1970-01-01T00:00:11.000Z","customType":"my_data","data":{"foo":"bar"}}"#,
        r#"{"type":"custom_message","id":"66666666","parentId":"55555555","timestamp":"1970-01-01T00:00:12.000Z","customType":"inject","content":"extra context","display":true}"#,
    ])
}

#[test]
fn full_v3_session_roundtrips_to_the_same_projection() {
    let dir = TempDir::new("full");
    let source_path = write_fixture(&dir, "source.jsonl", &full_session_fixture());

    let imported = read_session_file(&source_path).expect("import full fixture");

    // Every requested entry family mapped, in file order.
    let kinds: Vec<&str> = imported
        .entries
        .iter()
        .map(|draft| draft.kind.as_str())
        .collect();
    assert_eq!(
        kinds,
        [
            "pi.user",
            "pi.assistant",
            "pi.compaction",
            "pi.context-edit",
            "pi.context-edit",
            "pi.custom",
            "pi.custom",
        ],
        "labels/session_info/model_change/thinking_level_change/usage produce no drafts"
    );
    assert_eq!(imported.agent["model"]["provider"], "anthropic");
    assert_eq!(imported.agent["model"]["modelId"], "claude-sonnet-4-5");
    assert_eq!(imported.agent["thinkingLevel"], "high");
    assert_eq!(imported.session_info.as_deref(), Some("demo"));
    assert_eq!(
        imported
            .labels
            .get(&EntryId::from_raw(1))
            .map(String::as_str),
        Some("start")
    );
    assert!(imported.opaque.is_empty());

    let entries = as_records(&imported);
    let session_info = session_info_map(imported.session_info.as_deref());
    let input = ExportInput {
        header: imported.header.clone(),
        entries: &entries,
        agent: Some(&imported.agent),
        usage: Some(&imported.usage),
        session_info: Some(&session_info),
        labels: &imported.labels,
        opaque: &[],
    };

    // Same input, same bytes (render and write).
    let rendered = render_session(&input).expect("render");
    assert_eq!(
        rendered,
        render_session(&input).expect("render again"),
        "render_session is byte-stable"
    );
    assert!(rendered.ends_with('\n'), "every line is newline-terminated");

    let first_path = dir.file("exported-a.jsonl");
    let second_path = dir.file("exported-b.jsonl");
    write_session(&first_path, &input).expect("write a");
    write_session(&second_path, &input).expect("write b");
    assert_eq!(
        std::fs::read(&first_path).expect("read a"),
        std::fs::read(&second_path).expect("read b"),
        "write_session is byte-stable"
    );
    assert_eq!(
        std::fs::read_to_string(&first_path).expect("read a as text"),
        rendered,
        "write_session writes exactly render_session's bytes"
    );

    // Reopen both files through the real session manager.
    let source = SessionManager::open(&source_path).expect("open source");
    let reopened = SessionManager::open(&first_path).expect("open exported");
    let source_projection = source
        .build_session_projection()
        .expect("source projection");
    let reopened_projection = reopened
        .build_session_projection()
        .expect("exported projection");

    assert_eq!(
        reopened_projection.messages, source_projection.messages,
        "export must reproduce the model-visible transcript, timestamps included"
    );
    assert_eq!(
        reopened_projection.thinking_level,
        source_projection.thinking_level
    );
    assert_eq!(reopened_projection.model, source_projection.model);

    // Pin what the fixture actually exercised: the compaction summary (the
    // assistant was omitted by the first edit), the edited user content and the
    // custom_message text, each with its original timestamp.
    assert_eq!(source_projection.messages.len(), 3);
    let expected = [
        (
            "The conversation history before this point was compacted into the following summary:\n\n<summary>\nsummary so far\n</summary>".to_owned(),
            3_000,
        ),
        ("edited prompt".to_owned(), 1_000),
        ("extra context".to_owned(), 12_000),
    ];
    for (message, (text, timestamp)) in source_projection.messages.iter().zip(expected) {
        assert_eq!(user_text_and_timestamp(message), (text, timestamp));
    }
}

// ---------------------------------------------------------------------------
// Lossy mapping 1: the `pi.session` document is caller-seeded
// ---------------------------------------------------------------------------

#[test]
fn lossy_1_pi_session_document_is_caller_seeded() {
    let text = join_lines(&[
        header("sess-seed").as_str(),
        r#"{"type":"session_info","id":"11111111","parentId":null,"timestamp":"1970-01-01T00:00:01.000Z","name":"first"}"#,
        r#"{"type":"session_info","id":"22222222","parentId":"11111111","timestamp":"1970-01-01T00:00:02.000Z","name":"second"}"#,
    ]);
    let imported = import_fixture("lossy-1", &text);

    // Import yields the header and the last name, never a durable document.
    assert_eq!(imported.header.id, "sess-seed");
    assert_eq!(imported.header.version, Some(3));
    assert_eq!(imported.header.cwd, "C:\\repo");
    assert_eq!(imported.conversation.id, ROOT_CONVERSATION_ID);
    assert_eq!(
        imported.session_info.as_deref(),
        Some("second"),
        "last session_info name wins"
    );
    assert!(
        imported.entries.is_empty(),
        "the header and session_info lines are not entry drafts"
    );
    assert!(imported.entry_ids.is_empty());
    assert!(
        imported.opaque.is_empty(),
        "session_info is mapped, not opaque"
    );

    // The caller seeds `pi.session` from the header + mapped name; the export
    // then emits it as a session_info line.
    let labels = BTreeMap::new();
    let info = session_info_map(Some("second"));
    let entries: Vec<EntryRecord> = Vec::new();
    let input = ExportInput {
        header: imported.header.clone(),
        entries: &entries,
        agent: None,
        usage: None,
        session_info: Some(&info),
        labels: &labels,
        opaque: &[],
    };
    let lines = parse_lines(&render_session(&input).expect("render seeded pi.session"));
    assert_eq!(lines.len(), 2);
    assert_eq!(lines[0]["type"], "session");
    assert_eq!(lines[0]["version"], 3);
    assert_eq!(lines[0]["id"], "sess-seed");
    assert_eq!(lines[1]["type"], "session_info");
    assert_eq!(lines[1]["name"], "second");
}

// ---------------------------------------------------------------------------
// Lossy mapping 2: 1-based provisional ids; heads use them
// ---------------------------------------------------------------------------

#[test]
fn lossy_2_entry_ids_are_one_based_provisional_ids() {
    let text = join_lines(&[
        header("sess-ids").as_str(),
        r#"{"type":"message","id":"aaaaaaaa","parentId":null,"timestamp":"1970-01-01T00:00:01.000Z","message":{"role":"user","content":[{"type":"text","text":"hi"}],"timestamp":1}}"#,
        r#"{"type":"message","id":"bbbbbbbb","parentId":"aaaaaaaa","timestamp":"1970-01-01T00:00:02.000Z","message":{"role":"assistant","api":"test","provider":"test","model":"m","content":[{"type":"text","text":"hello"}],"stopReason":"stop","usage":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0,"totalTokens":0,"cost":{"input":0.0,"output":0.0,"cacheRead":0.0,"cacheWrite":0.0,"total":0.0}},"timestamp":2}}"#,
        r#"{"type":"compaction","id":"cccccccc","parentId":"bbbbbbbb","timestamp":"1970-01-01T00:00:03.000Z","summary":"cut","firstKeptEntryId":"aaaaaaaa","tokensBefore":10}"#,
        r#"{"type":"compaction","id":"dddddddd","parentId":"cccccccc","timestamp":"1970-01-01T00:00:04.000Z","summary":"self cut","firstKeptEntryId":"dddddddd","tokensBefore":20}"#,
        r#"{"type":"label","id":"eeeeeeee","parentId":"dddddddd","timestamp":"1970-01-01T00:00:05.000Z","targetId":"aaaaaaaa","label":"start"}"#,
        r#"{"type":"usage","id":"ffffffff","parentId":"eeeeeeee","timestamp":"1970-01-01T00:00:06.000Z","kind":"turn","provider":"p","model":"m","usage":{"input":1,"output":2,"cacheRead":0,"cacheWrite":0,"totalTokens":3,"cost":{"input":0.0,"output":0.0,"cacheRead":0.0,"cacheWrite":0.0,"total":0.0}}}"#,
        r#"{"type":"custom","id":"11111111","parentId":"ffffffff","timestamp":"1970-01-01T00:00:07.000Z","customType":"state","data":{"n":1}}"#,
    ]);
    let imported = import_fixture("lossy-2", &text);

    let expected: BTreeMap<String, EntryId> = BTreeMap::from([
        ("aaaaaaaa".to_owned(), EntryId::from_raw(1)),
        ("bbbbbbbb".to_owned(), EntryId::from_raw(2)),
        ("cccccccc".to_owned(), EntryId::from_raw(3)),
        ("dddddddd".to_owned(), EntryId::from_raw(4)),
        ("11111111".to_owned(), EntryId::from_raw(5)),
    ]);
    assert_eq!(
        imported.entry_ids, expected,
        "provisional ids are the 1-based draft position"
    );
    assert!(
        !imported.entry_ids.contains_key("eeeeeeee"),
        "label lines are not entries"
    );
    assert!(
        !imported.entry_ids.contains_key("ffffffff"),
        "usage lines are not entries"
    );

    // Heads point at those provisional ids; a self-retaining compaction is
    // `SelfEntry` on import.
    assert_eq!(
        imported.entries[2].head,
        Some(HeadTarget::Entry(EntryId::from_raw(1)))
    );
    assert_eq!(imported.entries[3].head, Some(HeadTarget::SelfEntry));

    // The caller maps `SelfEntry` to the entry's own id.
    let records = as_records(&imported);
    for (index, record) in records.iter().enumerate() {
        assert_eq!(record.id, EntryId::from_raw(index as i64 + 1));
    }
    assert_eq!(records[2].head, Some(EntryId::from_raw(1)));
    assert_eq!(
        records[3].head,
        Some(EntryId::from_raw(4)),
        "SelfEntry resolves to the entry's own id"
    );
}

// ---------------------------------------------------------------------------
// Lossy mapping 3: labels keep only the final target -> label map
// ---------------------------------------------------------------------------

#[test]
fn lossy_3_labels_keep_only_the_final_target_map() {
    let text = join_lines(&[
        header("sess-labels").as_str(),
        r#"{"type":"message","id":"aaaaaaaa","parentId":null,"timestamp":"1970-01-01T00:00:01.000Z","message":{"role":"user","content":[{"type":"text","text":"hi"}],"timestamp":1}}"#,
        r#"{"type":"label","id":"bbbbbbbb","parentId":"aaaaaaaa","timestamp":"1970-01-01T00:00:02.000Z","targetId":"aaaaaaaa","label":"old"}"#,
        r#"{"type":"label","id":"cccccccc","parentId":"bbbbbbbb","timestamp":"1970-01-01T00:00:03.000Z","targetId":"aaaaaaaa","label":"start"}"#,
    ]);
    let imported = import_fixture("lossy-3", &text);

    assert_eq!(imported.entries.len(), 1, "only the message is an entry");
    assert_eq!(
        imported.labels.len(),
        1,
        "the final target -> label map survives, position and timestamps do not"
    );
    assert_eq!(
        imported
            .labels
            .get(&EntryId::from_raw(1))
            .map(String::as_str),
        Some("start"),
        "the later label replaces the earlier one"
    );
    assert!(!imported.entry_ids.contains_key("bbbbbbbb"));
    assert!(!imported.entry_ids.contains_key("cccccccc"));

    // Export re-emits the surviving map at the tail, timestamped at the header
    // (the original label timestamps are gone).
    let entries = as_records(&imported);
    let input = ExportInput {
        header: imported.header.clone(),
        entries: &entries,
        agent: None,
        usage: None,
        session_info: None,
        labels: &imported.labels,
        opaque: &[],
    };
    let lines = parse_lines(&render_session(&input).expect("render labels"));
    let labels: Vec<&Value> = lines
        .iter()
        .filter(|line| line["type"].as_str() == Some("label"))
        .collect();
    assert_eq!(labels.len(), 1);
    assert_eq!(labels[0]["targetId"], "00000001");
    assert_eq!(labels[0]["label"], "start");
    assert_eq!(
        labels[0]["timestamp"], "1970-01-01T00:00:00.000Z",
        "the original label timestamps are not preserved"
    );
    assert_ne!(labels[0]["id"], "bbbbbbbb");
    assert_ne!(labels[0]["id"], "cccccccc");
    assert_eq!(
        lines.last().expect("a last line")["type"],
        "label",
        "labels are re-emitted at the tail, not at their original position"
    );
}

// ---------------------------------------------------------------------------
// Lossy mapping 4: one durable edit per context_edit line
// ---------------------------------------------------------------------------

#[test]
fn lossy_4_context_edit_emits_one_edit_per_line() {
    let text = join_lines(&[
        header("sess-edits").as_str(),
        r#"{"type":"message","id":"aaaaaaaa","parentId":null,"timestamp":"1970-01-01T00:00:01.000Z","message":{"role":"user","content":[{"type":"text","text":"hi"}],"timestamp":1}}"#,
        r#"{"type":"message","id":"bbbbbbbb","parentId":"aaaaaaaa","timestamp":"1970-01-01T00:00:02.000Z","message":{"role":"assistant","api":"test","provider":"test","model":"m","content":[{"type":"text","text":"hello"}],"stopReason":"stop","usage":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0,"totalTokens":0,"cost":{"input":0.0,"output":0.0,"cacheRead":0.0,"cacheWrite":0.0,"total":0.0}},"timestamp":2}}"#,
        r#"{"type":"context_edit","id":"cccccccc","parentId":"bbbbbbbb","timestamp":"1970-01-01T00:00:03.000Z","targetId":"aaaaaaaa","replacement":null}"#,
        r#"{"type":"context_edit","id":"dddddddd","parentId":"cccccccc","timestamp":"1970-01-01T00:00:04.000Z","targetId":"aaaaaaaa","replacement":{"content":[{"type":"text","text":"changed"}]}}"#,
    ]);
    let imported = import_fixture("lossy-4", &text);

    let kinds: Vec<&str> = imported
        .entries
        .iter()
        .map(|draft| draft.kind.as_str())
        .collect();
    assert_eq!(
        kinds,
        [
            "pi.user",
            "pi.assistant",
            "pi.context-edit",
            "pi.context-edit"
        ]
    );

    let omit = imported.entries[2].edits.as_ref().expect("omit edits");
    assert_eq!(omit.len(), 1, "one pi context_edit line is one edit");
    assert_eq!(omit[0].target, EntryId::from_raw(1));
    assert_eq!(omit[0].action, ContextEditAction::Omit);
    assert_eq!(
        imported.entries[2]
            .data
            .as_ref()
            .expect("omit timestamp")
            .get("timestamp")
            .and_then(Value::as_str),
        Some("1970-01-01T00:00:03.000Z")
    );

    let replace = imported.entries[3].edits.as_ref().expect("replace edits");
    assert_eq!(replace.len(), 1, "one pi context_edit line is one edit");
    assert_eq!(replace[0].target, EntryId::from_raw(1));
    assert_eq!(
        replace[0].action,
        ContextEditAction::Replace {
            messages: vec![serde_json::json!([{"type": "text", "text": "changed"}])],
        }
    );
    assert_eq!(
        imported.entries[3]
            .data
            .as_ref()
            .expect("replace timestamp")
            .get("timestamp")
            .and_then(Value::as_str),
        Some("1970-01-01T00:00:04.000Z")
    );

    // Export writes one context_edit line per durable edit, in order.
    let entries = as_records(&imported);
    let labels = BTreeMap::new();
    let input = ExportInput {
        header: imported.header.clone(),
        entries: &entries,
        agent: None,
        usage: None,
        session_info: None,
        labels: &labels,
        opaque: &[],
    };
    let lines = parse_lines(&render_session(&input).expect("render edits"));
    let edits: Vec<&Value> = lines
        .iter()
        .filter(|line| line["type"].as_str() == Some("context_edit"))
        .collect();
    assert_eq!(edits.len(), 2, "one line per durable edit");
    assert_eq!(edits[0]["targetId"], "00000001");
    assert!(edits[0]["replacement"].is_null(), "Omit re-emits null");
    assert_eq!(edits[1]["targetId"], "00000001");
    assert_eq!(
        edits[1]["replacement"]["content"],
        serde_json::json!([{"type": "text", "text": "changed"}])
    );
}

// ---------------------------------------------------------------------------
// Lossy mapping 5: usage accumulates numbers only
// ---------------------------------------------------------------------------

#[test]
fn lossy_5_usage_accumulates_numbers_without_attribution() {
    let text = join_lines(&[
        header("sess-usage").as_str(),
        r#"{"type":"usage","id":"aaaaaaaa","parentId":null,"timestamp":"1970-01-01T00:00:01.000Z","kind":"cache_warm","provider":"anthropic","model":"m1","usage":{"input":3,"output":4,"cacheRead":5,"cacheWrite":6,"totalTokens":12,"cost":{"input":0.5,"output":0.25,"cacheRead":0.0,"cacheWrite":0.0,"total":0.75}},"note":"first"}"#,
        r#"{"type":"usage","id":"bbbbbbbb","parentId":"aaaaaaaa","timestamp":"1970-01-01T00:00:02.000Z","kind":"turn","provider":"openai","model":"m2","usage":{"input":7,"output":8,"cacheRead":9,"cacheWrite":10,"totalTokens":20,"cost":{"input":1.5,"output":2.25,"cacheRead":0.0,"cacheWrite":0.0,"total":3.75}},"note":"second"}"#,
    ]);
    let imported = import_fixture("lossy-5", &text);

    assert!(imported.entries.is_empty(), "usage lines are not entries");
    assert!(imported.entry_ids.is_empty());

    let number = |key: &str| imported.usage.get(key).and_then(Value::as_f64);
    assert_eq!(number("input"), Some(10.0));
    assert_eq!(number("output"), Some(12.0));
    assert_eq!(number("cacheRead"), Some(14.0));
    assert_eq!(number("cacheWrite"), Some(16.0));
    assert_eq!(number("totalTokens"), Some(32.0));
    let cost = imported.usage.get("cost").and_then(Value::as_object);
    assert_eq!(
        cost.and_then(|cost| cost.get("input"))
            .and_then(Value::as_f64),
        Some(2.0)
    );
    assert_eq!(
        cost.and_then(|cost| cost.get("output"))
            .and_then(Value::as_f64),
        Some(2.5)
    );
    assert_eq!(
        cost.and_then(|cost| cost.get("total"))
            .and_then(Value::as_f64),
        Some(4.5)
    );

    for key in ["kind", "provider", "model", "note", "timestamp"] {
        assert!(
            imported.usage.get(key).is_none(),
            "per-line attribution `{key}` is not preserved"
        );
    }

    // Export emits one aggregate usage entry: the attribution became blank.
    let entries: Vec<EntryRecord> = Vec::new();
    let labels = BTreeMap::new();
    let input = ExportInput {
        header: imported.header.clone(),
        entries: &entries,
        agent: None,
        usage: Some(&imported.usage),
        session_info: None,
        labels: &labels,
        opaque: &[],
    };
    let lines = parse_lines(&render_session(&input).expect("render usage"));
    assert_eq!(lines.len(), 2);
    let usage = &lines[1];
    assert_eq!(usage["type"], "usage");
    assert_eq!(usage["kind"], "aggregate");
    assert_eq!(usage["provider"], "");
    assert_eq!(usage["model"], "");
    assert!(usage.get("note").is_none());
    assert_eq!(
        usage["usage"]["input"].as_f64(),
        Some(10.0),
        "the accumulated totals survive"
    );
    assert_eq!(usage["usage"]["totalTokens"].as_f64(), Some(32.0));
}

// ---------------------------------------------------------------------------
// Lossy mapping 6: custom_message is mapped, content stays in `model`
// ---------------------------------------------------------------------------

#[test]
fn lossy_6_custom_message_is_mapped_not_opaque() {
    let text = join_lines(&[
        header("sess-custom-message").as_str(),
        r#"{"type":"custom_message","id":"aaaaaaaa","parentId":null,"timestamp":"1970-01-01T00:00:12.000Z","customType":"inject","content":"extra context","display":true,"details":{"k":1}}"#,
    ]);
    let imported = import_fixture("lossy-6", &text);

    assert!(
        imported.opaque.is_empty(),
        "custom_message must be mapped, never opaque"
    );
    assert_eq!(imported.entries.len(), 1);
    let draft = &imported.entries[0];
    assert_eq!(draft.kind, "pi.custom");
    let data = draft.data.as_ref().expect("custom_message data");
    assert_eq!(data["source"], "custom_message");
    assert_eq!(data["customType"], "inject");
    assert_eq!(data["display"], true);
    assert_eq!(data["details"]["k"], 1);
    assert_eq!(
        draft.model,
        Some(vec![serde_json::json!({
            "role": "user",
            "content": [{ "type": "text", "text": "extra context" }],
            "timestamp": 12_000,
        })]),
        "the model-visible content stays in `model`"
    );

    // Export maps it back to a custom_message line (not to `custom`/opaque).
    let entries = as_records(&imported);
    let labels = BTreeMap::new();
    let input = ExportInput {
        header: imported.header.clone(),
        entries: &entries,
        agent: None,
        usage: None,
        session_info: None,
        labels: &labels,
        opaque: &[],
    };
    let lines = parse_lines(&render_session(&input).expect("render custom_message"));
    assert_eq!(lines.len(), 2);
    assert_eq!(lines[1]["type"], "custom_message");
    assert_eq!(lines[1]["customType"], "inject");
    assert_eq!(lines[1]["content"], "extra context");
    assert_eq!(lines[1]["display"], true);
    assert_eq!(lines[1]["details"]["k"], 1);
}

// ---------------------------------------------------------------------------
// Unknown `type` preservation
// ---------------------------------------------------------------------------

#[test]
fn unknown_type_is_opaque_on_import_and_custom_on_export() {
    let unknown_line = r#"{"type":"mystery_future","id":"77777777","parentId":null,"timestamp":"1970-01-01T00:00:03.000Z","payload":{"x":1},"nested":{"deep":[1,2]}}"#;
    let text = join_lines(&[
        header("sess-unknown").as_str(),
        unknown_line,
        r#"{"type":"message","id":"aaaaaaaa","parentId":"77777777","timestamp":"1970-01-01T00:00:04.000Z","message":{"role":"user","content":[{"type":"text","text":"still imported"}],"timestamp":1}}"#,
    ]);
    let imported = import_fixture("opaque", &text);

    // Import: the verbatim line is in `opaque`; the rest still imports.
    assert_eq!(imported.opaque.len(), 1);
    let opaque = &imported.opaque[0];
    assert_eq!(opaque.id, "77777777");
    assert_eq!(opaque.kind, "mystery_future");
    assert_eq!(opaque.parent_id, None);
    assert_eq!(opaque.timestamp, "1970-01-01T00:00:03.000Z");
    assert_eq!(
        opaque.raw,
        serde_json::from_str::<Value>(unknown_line).expect("fixture JSON")
    );
    assert_eq!(imported.entries.len(), 1);
    assert_eq!(imported.entries[0].kind, "pi.user");
    assert!(!imported.entry_ids.contains_key("77777777"));

    // Export: a durable-only kind becomes a `custom` entry instead of being
    // dropped, carrying its model/data payload.
    let dir = TempDir::new("opaque-export");
    let header = SessionHeader::new(
        "sess-durable-only",
        "C:\\repo",
        "1970-01-01T00:00:00.000Z".to_owned(),
    );
    let entries = vec![EntryRecord {
        id: EntryId::from_raw(1),
        conversation_id: ROOT_CONVERSATION_ID,
        kind: "mystery_future".to_owned(),
        model: Some(vec![serde_json::json!("model payload")]),
        data: Some(serde_json::json!({ "payload": { "x": 1 } })),
        head: None,
        edits: None,
        by_task_id: None,
    }];
    let labels = BTreeMap::new();
    let input = ExportInput {
        header,
        entries: &entries,
        agent: None,
        usage: None,
        session_info: None,
        labels: &labels,
        opaque: &[],
    };
    let rendered = render_session(&input).expect("render unknown kind");
    let lines = parse_lines(&rendered);
    assert_eq!(lines.len(), 2);
    assert_eq!(lines[1]["type"], "custom");
    assert_eq!(lines[1]["customType"], "mystery_future");
    assert_eq!(
        lines[1]["data"]["model"],
        serde_json::json!(["model payload"])
    );
    assert_eq!(lines[1]["data"]["data"]["payload"]["x"], 1);

    // The exported file is still a valid pi session.
    let path = write_fixture(&dir, "unknown.jsonl", &rendered);
    SessionManager::open(&path).expect("open exported unknown kind");
}

// ---------------------------------------------------------------------------
// Import error paths
// ---------------------------------------------------------------------------

#[test]
fn import_error_paths_are_reported() {
    let dir = TempDir::new("errors");
    let path = dir.file("session.jsonl");

    // No header: empty file.
    std::fs::write(&path, "").expect("write empty");
    assert!(
        matches!(read_session_file(&path), Err(ImportError::MissingHeader)),
        "an empty file has no header"
    );

    // No header: the first line is valid JSON but not a `session` header.
    std::fs::write(
        &path,
        r#"{"type":"custom","id":"aaaaaaaa","customType":"x"}
"#,
    )
    .expect("write non-header");
    assert!(
        matches!(read_session_file(&path), Err(ImportError::MissingHeader)),
        "the first line must be a session header"
    );

    // Non-v3 files (v2 and a v1-style header without `version`).
    for (line, expected) in [
        (
            r#"{"type":"session","version":2,"id":"s","timestamp":"1970-01-01T00:00:00.000Z","cwd":"C:\\repo"}"#,
            Some(2),
        ),
        (
            r#"{"type":"session","id":"s","timestamp":"1970-01-01T00:00:00.000Z","cwd":"C:\\repo"}"#,
            None,
        ),
    ] {
        std::fs::write(&path, format!("{line}\n")).expect("write version");
        match read_session_file(&path) {
            Err(ImportError::UnsupportedVersion { found, supported }) => {
                assert_eq!(found, expected);
                assert_eq!(supported, 3);
            }
            other => panic!("expected UnsupportedVersion, got {other:?}"),
        }
    }

    // Invalid JSON on the header line.
    std::fs::write(&path, "not json\n").expect("write bad header");
    match read_session_file(&path) {
        Err(ImportError::Json { line, .. }) => assert_eq!(line, 1),
        other => panic!("expected Json line 1, got {other:?}"),
    }

    // Invalid JSON on a body line: the 1-based ORIGINAL line number is
    // reported, blank lines included.
    let text = join_lines(&[
        header("sess-bad-json").as_str(),
        "",
        r#"{"type":"message","#,
    ]);
    std::fs::write(&path, text).expect("write bad body");
    match read_session_file(&path) {
        Err(ImportError::Json { line, .. }) => {
            assert_eq!(line, 3, "the reported line is the original file position")
        }
        other => panic!("expected Json line 3, got {other:?}"),
    }
}
