//! durable records → pi session format (the export direction).
//!
//! The acceptance contract is the golden shape from
//! `session_manager::golden_record_serialization_matches_upstream_shapes`:
//! header first, one compact JSON object per line, `\n`-terminated, 8-hex ids,
//! a linear `parentId` chain — and a file that `SessionManager::open` reads
//! back into the same model-visible projection.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde_json::{Map, Value};

use pir::durable::ids::{EntryId, ROOT_CONVERSATION_ID};
use pir::durable::pi_format::{read_session_file, render_session, write_session, ExportInput};
use pir::durable::session::transaction::HeadTarget;
use pir::durable::types::EntryRecord;
use pir::session_manager::SessionManager;

struct TempDir(PathBuf);

impl TempDir {
    fn new(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "pir-pi-export-{label}-{}-{}",
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
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn source_fixture() -> String {
    [
        r#"{"type":"session","version":3,"id":"sess-export","timestamp":"2026-10-20T00:00:00.000Z","cwd":"C:\\repo"}"#,
        r#"{"type":"message","id":"aaaaaaaa","parentId":null,"timestamp":"2026-10-20T00:00:01.000Z","message":{"role":"user","content":[{"type":"text","text":"hi"}],"timestamp":1}}"#,
        r#"{"type":"message","id":"bbbbbbbb","parentId":"aaaaaaaa","timestamp":"2026-10-20T00:00:02.000Z","message":{"role":"assistant","api":"test","provider":"test","model":"m","content":[{"type":"text","text":"hello"}],"stopReason":"stop","usage":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0,"totalTokens":0,"cost":{"input":0.0,"output":0.0,"cacheRead":0.0,"cacheWrite":0.0,"total":0.0}},"timestamp":2}}"#,
        r#"{"type":"model_change","id":"cccccccc","parentId":"bbbbbbbb","timestamp":"2026-10-20T00:00:03.000Z","provider":"anthropic","modelId":"claude-sonnet-4-5"}"#,
        r#"{"type":"compaction","id":"ffffffff","parentId":"cccccccc","timestamp":"2026-10-20T00:00:06.000Z","summary":"summary so far","firstKeptEntryId":"aaaaaaaa","tokensBefore":1234}"#,
        r#"{"type":"label","id":"33333333","parentId":"ffffffff","timestamp":"2026-10-20T00:00:09.000Z","targetId":"aaaaaaaa","label":"start"}"#,
        r#"{"type":"session_info","id":"44444444","parentId":"33333333","timestamp":"2026-10-20T00:00:10.000Z","name":"demo"}"#,
    ]
    .join("\n")
        + "\n"
}

fn records(imported: &pir::durable::pi_format::ImportedSession) -> Vec<EntryRecord> {
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

fn session_info_map(name: &str) -> Map<String, Value> {
    let mut map = Map::new();
    map.insert("name".to_owned(), Value::String(name.to_owned()));
    map
}

#[test]
fn export_writes_the_pi_wire_shape_and_reopens() {
    let dir = TempDir::new("roundtrip");
    let source_path = dir.0.join("source.jsonl");
    std::fs::write(&source_path, source_fixture()).expect("write source");

    let imported = read_session_file(&source_path).expect("import");
    let entries = records(&imported);
    let session_info = session_info_map(imported.session_info.as_deref().unwrap_or(""));
    let input = ExportInput {
        header: imported.header.clone(),
        entries: &entries,
        agent: Some(&imported.agent),
        usage: Some(&imported.usage),
        session_info: Some(&session_info),
        labels: &imported.labels,
        opaque: &[],
    };

    let rendered = render_session(&input).expect("render");
    // Byte-stable across renders.
    assert_eq!(rendered, render_session(&input).expect("render again"));

    let lines: Vec<&str> = rendered.lines().collect();
    assert!(!lines.is_empty());
    assert!(rendered.ends_with('\n'), "every line is newline-terminated");

    // Line 1 is the v3 header.
    let header: Value = serde_json::from_str(lines[0]).expect("header json");
    assert_eq!(header["type"], "session");
    assert_eq!(header["version"], 3);
    assert_eq!(header["id"], "sess-export");
    assert_eq!(header["cwd"], "C:\\repo");

    // Every line is compact (serde_json's compact form round-trips byte-wise
    // because `preserve_order` keeps the key order).
    for line in &lines {
        let value: Value = serde_json::from_str(line).expect("line json");
        assert_eq!(*line, value.to_string(), "line is not compact: {line}");
    }

    // Ids are 8 lowercase hex and the chain is linear.
    let mut previous: Option<String> = None;
    for (index, line) in lines.iter().enumerate().skip(1) {
        let value: Value = serde_json::from_str(line).expect("line json");
        let id = value["id"].as_str().expect("entry id");
        assert_eq!(id.len(), 8, "id {id} is not 8 chars");
        assert!(
            id.chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()),
            "id {id} is not lowercase hex"
        );
        if index == 1 {
            assert!(value["parentId"].is_null(), "first entry has parentId null");
        } else {
            assert_eq!(
                value["parentId"].as_str(),
                previous.as_deref(),
                "line {index} parentId must be the previous line's id"
            );
        }
        previous = Some(id.to_owned());
    }

    // Write and reopen through the real session manager.
    let exported_path = dir.0.join("exported.jsonl");
    write_session(&exported_path, &input).expect("write");
    let reopened = SessionManager::open(&exported_path).expect("reopen exported session");
    assert_eq!(reopened.id(), "sess-export");

    let source = SessionManager::open(&source_path).expect("open source session");
    let source_projection = source
        .build_session_projection()
        .expect("source projection");
    let exported_projection = reopened
        .build_session_projection()
        .expect("exported projection");
    assert_eq!(
        exported_projection.messages, source_projection.messages,
        "export must reproduce the model-visible transcript"
    );
    assert_eq!(
        exported_projection.thinking_level,
        source_projection.thinking_level
    );
    assert_eq!(exported_projection.model, source_projection.model);
}

#[test]
fn export_keeps_durable_only_kinds_as_custom_entries() {
    let dir = TempDir::new("unknown-kind");
    let header = pir::session_manager::types::SessionHeader::new(
        "sess-unknown",
        "C:\\repo",
        "2026-10-20T00:00:00.000Z".to_owned(),
    );
    let entries = vec![EntryRecord {
        id: EntryId::from_raw(1),
        conversation_id: ROOT_CONVERSATION_ID,
        kind: "pi.something-new".to_owned(),
        model: None,
        data: Some(serde_json::json!({ "payload": 7 })),
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
    let rendered = render_session(&input).expect("render");
    let line: Value =
        serde_json::from_str(rendered.lines().nth(1).expect("entry line")).expect("entry json");
    assert_eq!(line["type"], "custom");
    assert_eq!(line["customType"], "pi.something-new");
    assert_eq!(line["data"]["data"]["payload"], 7);

    // And the file is still openable (a `custom` entry is a state line).
    let path = dir.0.join("unknown.jsonl");
    write_session(&path, &input).expect("write");
    SessionManager::open(&path).expect("reopen");
}

#[test]
fn stable_pi_id_is_deterministic_and_separates_high_bits() {
    let low = pir::durable::pi_format::export::stable_pi_id(1);
    assert_eq!(low, "00000001");
    assert_eq!(low, pir::durable::pi_format::export::stable_pi_id(1));
    assert_ne!(
        pir::durable::pi_format::export::stable_pi_id(1),
        pir::durable::pi_format::export::stable_pi_id(1 + (1_i64 << 32))
    );
}
