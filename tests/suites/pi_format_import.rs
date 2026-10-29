//! pi session format → durable records (the import direction).
//!
//! Covers every mapped entry type, the provisional-id contract, the
//! `pi.agent`/`pi.usage`/labels/session_info outputs, and the rule that an
//! unknown `type` is preserved instead of failing the import.

use std::path::PathBuf;

use serde_json::Value;

use pir::durable::ids::EntryId;
use pir::durable::pi_format::{read_session_file, OpaqueEntry};
use pir::durable::types::ContextEditAction;

struct TempDir(PathBuf);

impl TempDir {
    fn new(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "pir-pi-format-{label}-{}-{}",
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

/// A version-3 session exercising every mapped entry type plus one unknown one.
fn fixture() -> String {
    [
        r#"{"type":"session","version":3,"id":"sess-1","timestamp":"2026-10-20T00:00:00.000Z","cwd":"C:\\repo"}"#,
        r#"{"type":"message","id":"aaaaaaaa","parentId":null,"timestamp":"2026-10-20T00:00:01.000Z","message":{"role":"user","content":[{"type":"text","text":"hi"}],"timestamp":1}}"#,
        r#"{"type":"message","id":"bbbbbbbb","parentId":"aaaaaaaa","timestamp":"2026-10-20T00:00:02.000Z","message":{"role":"assistant","api":"test","provider":"test","model":"m","content":[{"type":"text","text":"hello"}],"stopReason":"stop","usage":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0,"totalTokens":0,"cost":{"input":0.0,"output":0.0,"cacheRead":0.0,"cacheWrite":0.0,"total":0.0}},"timestamp":2}}"#,
        r#"{"type":"model_change","id":"cccccccc","parentId":"bbbbbbbb","timestamp":"2026-10-20T00:00:03.000Z","provider":"anthropic","modelId":"claude-sonnet-4-5"}"#,
        r#"{"type":"thinking_level_change","id":"dddddddd","parentId":"cccccccc","timestamp":"2026-10-20T00:00:04.000Z","thinkingLevel":"high"}"#,
        r#"{"type":"usage","id":"eeeeeeee","parentId":"dddddddd","timestamp":"2026-10-20T00:00:05.000Z","kind":"cache_warm","provider":"anthropic","model":"claude-sonnet-4-5","usage":{"input":3,"output":4,"cacheRead":5,"cacheWrite":0,"totalTokens":12,"cost":{"input":0.0,"output":0.0,"cacheRead":0.0,"cacheWrite":0.0,"total":0.0}},"note":"warmed"}"#,
        r#"{"type":"compaction","id":"ffffffff","parentId":"eeeeeeee","timestamp":"2026-10-20T00:00:06.000Z","summary":"summary so far","firstKeptEntryId":"aaaaaaaa","tokensBefore":1234}"#,
        r#"{"type":"branch_summary","id":"11111111","parentId":"ffffffff","timestamp":"2026-10-20T00:00:07.000Z","fromId":"bbbbbbbb","summary":"branch summary"}"#,
        r#"{"type":"context_edit","id":"22222222","parentId":"11111111","timestamp":"2026-10-20T00:00:08.000Z","targetId":"bbbbbbbb","replacement":null}"#,
        r#"{"type":"label","id":"33333333","parentId":"22222222","timestamp":"2026-10-20T00:00:09.000Z","targetId":"aaaaaaaa","label":"start"}"#,
        r#"{"type":"session_info","id":"44444444","parentId":"33333333","timestamp":"2026-10-20T00:00:10.000Z","name":"demo"}"#,
        r#"{"type":"custom","id":"55555555","parentId":"44444444","timestamp":"2026-10-20T00:00:11.000Z","customType":"my_data","data":{"foo":"bar"}}"#,
        r#"{"type":"custom_message","id":"66666666","parentId":"55555555","timestamp":"2026-10-20T00:00:12.000Z","customType":"inject","content":"extra context","display":true}"#,
        r#"{"type":"mystery_future","id":"77777777","parentId":"66666666","timestamp":"2026-10-20T00:00:13.000Z","payload":{"x":1}}"#,
    ]
    .join("\n")
        + "\n"
}

#[test]
fn import_maps_every_entry_type_in_file_order() {
    let dir = TempDir::new("import-all");
    let path = dir.0.join("session.jsonl");
    std::fs::write(&path, fixture()).expect("write fixture");

    let imported = read_session_file(&path).expect("import");

    assert_eq!(imported.header.id, "sess-1");
    assert_eq!(imported.header.cwd, "C:\\repo");
    assert_eq!(imported.header.version, Some(3));
    assert_eq!(imported.conversation.id.as_i64(), 1);
    assert_eq!(imported.conversation.parent, None);

    let kinds: Vec<&str> = imported
        .entries
        .iter()
        .map(|draft| draft.kind.as_str())
        .collect();
    assert_eq!(
        kinds,
        vec![
            "pi.user",
            "pi.assistant",
            "pi.compaction", // compaction
            "pi.compaction", // branch summary, distinguished by data.kind
            "pi.context-edit",
            "pi.custom", // custom entry
            "pi.custom", // custom_message
        ],
        "labels/session_info/model_change/thinking_level_change/usage produce no entry"
    );

    // Provisional ids: 1-based draft order.
    assert_eq!(imported.entry_ids["aaaaaaaa"], EntryId::from_raw(1));
    assert_eq!(imported.entry_ids["bbbbbbbb"], EntryId::from_raw(2));
    assert_eq!(imported.entry_ids["ffffffff"], EntryId::from_raw(3));
    assert_eq!(imported.entry_ids["11111111"], EntryId::from_raw(4));
    assert_eq!(imported.entry_ids["22222222"], EntryId::from_raw(5));
    assert_eq!(imported.entry_ids["55555555"], EntryId::from_raw(6));
    assert_eq!(imported.entry_ids["66666666"], EntryId::from_raw(7));
    assert!(
        !imported.entry_ids.contains_key("33333333"),
        "label lines are not entries"
    );

    // The compaction head points at the first kept entry.
    let compaction = &imported.entries[2];
    assert_eq!(
        compaction.head,
        Some(pir::durable::session::transaction::HeadTarget::Entry(
            EntryId::from_raw(1)
        ))
    );
    let data = compaction.data.as_ref().expect("compaction data");
    assert_eq!(data["kind"], "compaction");
    assert_eq!(data["summary"], "summary so far");
    assert_eq!(data["tokensBefore"], 1234);
    assert_eq!(data["timestamp"], "2026-10-20T00:00:06.000Z");

    // The branch summary shares the kind but is marked in data.
    let branch = &imported.entries[3];
    assert_eq!(branch.head, None);
    assert_eq!(branch.data.as_ref().expect("branch data")["kind"], "branch");
    assert_eq!(
        branch.data.as_ref().expect("branch data")["fromId"],
        "bbbbbbbb"
    );

    // context_edit with `replacement: null` becomes an omission of the target.
    let edit = &imported.entries[4];
    let edits = edit.edits.as_ref().expect("context edit");
    assert_eq!(edits.len(), 1);
    assert_eq!(edits[0].target, EntryId::from_raw(2));
    assert_eq!(edits[0].action, ContextEditAction::Omit);

    // custom vs custom_message are distinguished in data.
    let custom = &imported.entries[5];
    assert_eq!(
        custom.data.as_ref().expect("custom data")["source"],
        "custom"
    );
    assert_eq!(
        custom.data.as_ref().expect("custom data")["customType"],
        "my_data"
    );
    let custom_message = &imported.entries[6];
    assert_eq!(
        custom_message.data.as_ref().expect("custom message")["source"],
        "custom_message"
    );
    assert_eq!(
        custom_message
            .model
            .as_ref()
            .and_then(|model| model.first()),
        Some(&serde_json::json!({
            "role": "user",
            "content": [{ "type": "text", "text": "extra context" }],
            "timestamp": 1792454412000u64,
        })),
        "custom_message content is projected as user text"
    );
}

#[test]
fn import_collects_agent_usage_labels_and_session_info() {
    let dir = TempDir::new("import-state");
    let path = dir.0.join("session.jsonl");
    std::fs::write(&path, fixture()).expect("write fixture");

    let imported = read_session_file(&path).expect("import");

    assert_eq!(imported.agent["model"]["provider"], "anthropic");
    assert_eq!(imported.agent["model"]["modelId"], "claude-sonnet-4-5");
    assert_eq!(imported.agent["thinkingLevel"], "high");

    // Usage accumulates numerically (f64 storage).
    let input = imported.usage["input"].as_f64().expect("input usage");
    assert!((input - 3.0).abs() < f64::EPSILON);
    let cache_read = imported.usage["cacheRead"].as_f64().expect("cache read");
    assert!((cache_read - 5.0).abs() < f64::EPSILON);
    assert!(
        imported.usage.get("kind").is_none(),
        "non-numeric fields are not usage totals"
    );

    assert_eq!(
        imported
            .labels
            .get(&EntryId::from_raw(1))
            .map(String::as_str),
        Some("start")
    );
    assert_eq!(imported.session_info.as_deref(), Some("demo"));
}

#[test]
fn import_preserves_unknown_types_as_opaque() {
    let dir = TempDir::new("import-opaque");
    let path = dir.0.join("session.jsonl");
    std::fs::write(
        &path,
        [
            r#"{"type":"session","version":3,"id":"s","timestamp":"2026-10-20T00:00:00.000Z","cwd":"C:\\repo"}"#,
            r#"{"type":"mystery_future","id":"77777777","parentId":null,"timestamp":"2026-10-20T00:00:13.000Z","payload":{"x":1}}"#,
            r#"{"type":"message","id":"aaaaaaaa","parentId":"77777777","timestamp":"2026-10-20T00:00:14.000Z","message":{"role":"user","content":[{"type":"text","text":"still imported"}],"timestamp":1}}"#,
        ]
        .join("\n"),
    )
    .expect("write fixture");

    let imported = read_session_file(&path).expect("import never fails on an unknown type");
    assert_eq!(imported.opaque.len(), 1);
    let OpaqueEntry {
        id,
        kind,
        parent_id,
        raw,
        ..
    } = &imported.opaque[0];
    assert_eq!(id, "77777777");
    assert_eq!(kind, "mystery_future");
    assert_eq!(parent_id.as_deref(), None);
    assert_eq!(raw["payload"]["x"], Value::from(1));
    // The known line after it still imports.
    assert_eq!(imported.entries.len(), 1);
    assert_eq!(imported.entries[0].kind, "pi.user");
}

#[test]
fn import_rejects_non_v3_files() {
    let dir = TempDir::new("import-version");
    let path = dir.0.join("session.jsonl");
    std::fs::write(
        &path,
        r#"{"type":"session","id":"s","timestamp":"2026-10-20T00:00:00.000Z","cwd":"C:\\repo"}"#,
    )
    .expect("write fixture");
    assert!(matches!(
        read_session_file(&path),
        Err(pir::durable::pi_format::ImportError::UnsupportedVersion { found: None, .. })
    ));

    std::fs::write(
        &path,
        r#"{"type":"session","version":2,"id":"s","timestamp":"2026-10-20T00:00:00.000Z","cwd":"C:\\repo"}"#,
    )
    .expect("write fixture");
    assert!(matches!(
        read_session_file(&path),
        Err(pir::durable::pi_format::ImportError::UnsupportedVersion { found: Some(2), .. })
    ));
}
