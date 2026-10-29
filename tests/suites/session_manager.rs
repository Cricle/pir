//! Tests for the upstream-interoperable session manager (pi JSONL v3).
//!
//! Covers: golden record serialization (exact upstream line shapes), the
//! lazy file-creation rule, projection semantics (context edits, newest
//! compaction only), self-retaining compactions + branch copy semantics,
//! directory layout, and listing (old-format files are skipped).

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use pir::ai::types::{
    AssistantMessage, ContentBlock, Message, Model, ModelCost, ModelCostRates, StopReason,
    TextContent, ToolCall, ToolResultMessage, ToolResultRole, Usage, UserMessage, UserRole,
};
use pir::session_manager::{
    AppendFailure, CompactionEntry, ContextEditEntry, FileEntry, LabelEntry, MessageEntry,
    SessionInfoEntry, SessionManager, StoredMessage, UsageEntry, CURRENT_SESSION_VERSION,
};

fn temp_dir(name: &str) -> PathBuf {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("pir-session-manager-{name}-{stamp}"));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn missing_session_cwd_issue_matches_upstream_shape() {
    let dir = temp_dir("missing-cwd");
    let missing = dir.join("gone-project");
    let mut session = SessionManager::create(missing.to_string_lossy().as_ref(), &dir).unwrap();
    session.append_message(user_message("hello")).unwrap();

    let issue = session
        .missing_session_cwd()
        .expect("a deleted stored cwd must be reported");
    assert_eq!(issue.session_cwd, missing.to_string_lossy());
    assert_eq!(issue.session_file.as_path(), session.path());
    let text = issue.error_message("C:/current");
    assert!(
        text.contains("Stored session working directory does not exist"),
        "{text}"
    );
    assert!(text.contains("Session file:"), "{text}");
    assert!(
        text.contains("Current working directory: C:/current"),
        "{text}"
    );

    // A cwd that still exists is not an issue.
    let present = SessionManager::create(dir.to_string_lossy().as_ref(), &dir).unwrap();
    assert!(present.missing_session_cwd().is_none());
}

fn user_message(text: &str) -> Message {
    Message::User(UserMessage {
        role: UserRole::User,
        content: vec![ContentBlock::Text(TextContent {
            text: text.into(),
            text_signature: None,
        })],
        tools_added: None,
        tools_removed: None,
        timestamp: pir::ai::util::now_ms(),
        sections: None,
    })
}

fn assistant_message(text: &str, stop_reason: StopReason) -> Message {
    Message::Assistant(AssistantMessage {
        content: vec![ContentBlock::Text(TextContent {
            text: text.into(),
            text_signature: None,
        })],
        stop_reason,
        timestamp: pir::ai::util::now_ms(),
        ..Default::default()
    })
}

fn text_of(message: &Message) -> String {
    match message {
        Message::User(inner) => match inner.content.first() {
            Some(ContentBlock::Text(text)) => text.text.clone(),
            _ => String::new(),
        },
        Message::Assistant(inner) => match inner.content.first() {
            Some(ContentBlock::Text(text)) => text.text.clone(),
            _ => String::new(),
        },
        _ => String::new(),
    }
}

fn lines_of(path: &std::path::Path) -> Vec<serde_json::Value> {
    let content = std::fs::read_to_string(path).unwrap();
    content
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

// ---------------------------------------------------------------------------
// Golden serialization: upstream record shapes
// ---------------------------------------------------------------------------

#[test]
#[cfg_attr(target_os = "linux", ignore = "asserts Windows path encoding")]
fn golden_record_serialization_matches_upstream_shapes() {
    let dir = temp_dir("golden");
    let mut session = SessionManager::create("C:/repo", &dir).unwrap();

    let user_id = session.append_message(user_message("hello")).unwrap();
    let assistant_id = session
        .append_message(assistant_message("hi", StopReason::Stop))
        .unwrap();
    session.append_thinking_level_change("medium").unwrap();
    session
        .append_model_change("anthropic", "claude-sonnet-4-5")
        .unwrap();
    session
        .append_usage(
            "cache_warm",
            "anthropic",
            "claude-sonnet-4-5",
            Usage {
                input: 3,
                output: 4,
                cache_read: 5,
                cache_write: 0,
                total_tokens: 12,
                ..Usage::default()
            },
            Some("warmed"),
        )
        .unwrap();
    let compaction_id = session
        .append_compaction("summary so far", Some(&user_id), 1234, None)
        .unwrap();
    session
        .append_custom_entry("my_data", Some(serde_json::json!({"foo": "bar"})))
        .unwrap();
    session.set_session_name(Some("demo")).unwrap();
    session.set_label(&user_id, Some("start")).unwrap();
    session
        .append_context_edit(&user_id, Some(serde_json::json!("edited prompt")))
        .unwrap();

    let lines = lines_of(session.path());
    // Line 1: header.
    let header = &lines[0];
    assert_eq!(header["type"], "session");
    assert_eq!(header["version"], CURRENT_SESSION_VERSION);
    assert_eq!(header["id"], session.id());
    assert_eq!(header["cwd"], "C:\\repo");
    assert!(header["timestamp"].as_str().unwrap().ends_with('Z'));
    assert!(header.get("parentSession").is_none());

    // Message entry: {type, id, parentId, timestamp, message}.
    let user_entry = &lines[1];
    assert_eq!(user_entry["type"], "message");
    assert_eq!(user_entry["id"], user_id.as_str());
    assert!(user_entry["parentId"].is_null());
    assert_eq!(user_entry["message"]["role"], "user");
    assert_eq!(user_entry["message"]["content"][0]["type"], "text");
    assert_eq!(user_entry["message"]["content"][0]["text"], "hello");

    let assistant_entry = &lines[2];
    assert_eq!(assistant_entry["id"], assistant_id.as_str());
    assert_eq!(assistant_entry["parentId"], user_id.as_str());
    assert_eq!(assistant_entry["message"]["role"], "assistant");
    assert_eq!(assistant_entry["message"]["stopReason"], "stop");

    // thinking_level_change / model_change.
    assert_eq!(lines[3]["type"], "thinking_level_change");
    assert_eq!(lines[3]["thinkingLevel"], "medium");
    assert_eq!(lines[4]["type"], "model_change");
    assert_eq!(lines[4]["provider"], "anthropic");
    assert_eq!(lines[4]["modelId"], "claude-sonnet-4-5");

    // usage entry: kind/provider/model/usage/note (camelCase usage fields).
    let usage_entry = &lines[5];
    assert_eq!(usage_entry["type"], "usage");
    assert_eq!(usage_entry["kind"], "cache_warm");
    assert_eq!(usage_entry["provider"], "anthropic");
    assert_eq!(usage_entry["model"], "claude-sonnet-4-5");
    assert_eq!(usage_entry["usage"]["input"], 3);
    assert_eq!(usage_entry["usage"]["cacheRead"], 5);
    assert_eq!(usage_entry["usage"]["totalTokens"], 12);
    assert_eq!(usage_entry["note"], "warmed");

    // compaction entry: firstKeptEntryId always a string; optional fields
    // (details/usage/fromHook) omitted when absent.
    let compaction_entry = &lines[6];
    assert_eq!(compaction_entry["type"], "compaction");
    assert_eq!(compaction_entry["id"], compaction_id.as_str());
    assert_eq!(compaction_entry["summary"], "summary so far");
    assert_eq!(compaction_entry["firstKeptEntryId"], user_id.as_str());
    assert_eq!(compaction_entry["tokensBefore"], 1234);
    assert!(compaction_entry.get("details").is_none());
    assert!(compaction_entry.get("usage").is_none());
    assert!(compaction_entry.get("fromHook").is_none());
    assert!(compaction_entry.get("systemMessage").is_none());

    // custom entry: customType/data precede id/parentId/timestamp.
    let custom_entry = &lines[7];
    assert_eq!(custom_entry["type"], "custom");
    assert_eq!(custom_entry["customType"], "my_data");
    assert_eq!(custom_entry["data"]["foo"], "bar");

    // session_info entry.
    assert_eq!(lines[8]["type"], "session_info");
    assert_eq!(lines[8]["name"], "demo");

    // label entry.
    assert_eq!(lines[9]["type"], "label");
    assert_eq!(lines[9]["targetId"], user_id.as_str());
    assert_eq!(lines[9]["label"], "start");

    // context_edit entry: replacement {content} (string normalized to a text
    // block for user targets? no 鈥?user keeps the string).
    let edit_entry = &lines[10];
    assert_eq!(edit_entry["type"], "context_edit");
    assert_eq!(edit_entry["targetId"], user_id.as_str());
    assert_eq!(edit_entry["replacement"]["content"], "edited prompt");

    // Entry ids are 8 hex chars (upstream generateId).
    for line in lines.iter().skip(1) {
        let id = line["id"].as_str().unwrap();
        assert_eq!(id.len(), 8, "id {id} is not 8 hex chars");
        assert!(id.chars().all(|c| c.is_ascii_hexdigit()));
    }

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn self_retaining_compaction_stores_its_own_id() {
    let dir = temp_dir("self-retain");
    let mut session = SessionManager::create("C:/repo", &dir).unwrap();
    session.append_message(user_message("hello")).unwrap();
    let compaction_id = session
        .append_compaction("only summary", None, 100, None)
        .unwrap();

    let lines = lines_of(session.path());
    let entry = lines.last().unwrap();
    assert_eq!(entry["type"], "compaction");
    assert_eq!(entry["id"], compaction_id.as_str());
    // null firstKeptEntryId 鈫?the entry's own id (self-retaining).
    assert_eq!(entry["firstKeptEntryId"], compaction_id.as_str());

    // Projection: the summary is the only context contribution; nothing is
    // retained from before.
    let context = session.build_context_entries().unwrap();
    assert_eq!(context.len(), 1);
    assert!(text_of(&context[0]).contains("only summary"));

    std::fs::remove_dir_all(&dir).unwrap();
}

// ---------------------------------------------------------------------------
// File-creation rule
// ---------------------------------------------------------------------------

#[test]
fn file_is_created_on_first_user_or_assistant_message() {
    let dir = temp_dir("file-rule");
    let mut session = SessionManager::create("C:/repo", &dir).unwrap();
    let path = session.path().to_path_buf();

    // Setup entries alone stay in memory: no file yet.
    assert!(!path.exists());
    session.append_thinking_level_change("low").unwrap();
    session.append_model_change("anthropic", "claude").unwrap();
    session.append_custom_entry("note", None).unwrap();
    assert!(!path.exists(), "no file may exist before a conversation");

    // The first user message creates the file with ALL buffered entries.
    session
        .append_message(user_message("first prompt"))
        .unwrap();
    assert!(path.exists());
    let lines = lines_of(&path);
    assert_eq!(lines.len(), 5);
    assert_eq!(lines[0]["type"], "session");
    assert_eq!(lines[1]["type"], "thinking_level_change");
    assert_eq!(lines[2]["type"], "model_change");
    assert_eq!(lines[3]["type"], "custom");
    assert_eq!(lines[4]["type"], "message");
    assert_eq!(lines[4]["message"]["content"][0]["text"], "first prompt");

    // Aborting before any message leaves no file at all.
    let dir2 = temp_dir("file-rule-abort");
    let aborted = SessionManager::create("C:/repo", &dir2).unwrap();
    assert!(!aborted.path().exists());
    std::fs::remove_dir_all(&dir).unwrap();
    std::fs::remove_dir_all(&dir2).unwrap();
}

// ---------------------------------------------------------------------------
// Round trip
// ---------------------------------------------------------------------------

#[test]
fn round_trip_create_append_reopen() {
    let dir = temp_dir("round-trip");
    let mut session = SessionManager::create("C:/repo", &dir).unwrap();
    let user_id = session.append_message(user_message("hello")).unwrap();
    session
        .append_message(assistant_message("hi", StopReason::Stop))
        .unwrap();
    session.set_session_name(Some("demo")).unwrap();
    session.set_label(&user_id, Some("start")).unwrap();
    session
        .append_usage("cache_warm", "anthropic", "claude", Usage::default(), None)
        .unwrap();

    let reopened = SessionManager::open(session.path()).unwrap();
    assert_eq!(reopened.id(), session.id());
    assert_eq!(reopened.session_name().as_deref(), Some("demo"));
    assert_eq!(reopened.get_label(&user_id).as_deref(), Some("start"));
    let entries = reopened.entries();
    assert_eq!(entries.len(), 5);
    assert_eq!(entries[0].id(), Some(user_id.as_str()));
    assert_eq!(entries[1].parent_id(), Some(user_id.as_str()));
    // usage entries are kept (state entries, not context).
    assert!(matches!(entries[4], FileEntry::Usage(_)));

    // Messages survive the round trip.
    let context = reopened.build_context_entries().unwrap();
    let texts: Vec<String> = context.iter().map(text_of).collect();
    assert_eq!(texts, vec!["hello", "hi"]);

    // Reopening again stays stable.
    let again = SessionManager::open(session.path()).unwrap();
    assert_eq!(again.entries().len(), 5);

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn torn_tail_is_repaired_and_non_sessions_are_rejected() {
    let dir = temp_dir("torn-tail");
    let mut session = SessionManager::create("C:/repo", &dir).unwrap();
    session.append_message(user_message("before")).unwrap();
    let path = session.path().to_path_buf();

    // Missing trailing newline is repaired on load (upstream behavior).
    {
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        file.write_all(b"{\"type\":\"message\",\"id\":\"deadbeef\"")
            .unwrap();
    }
    // The malformed final line is skipped, the file is repaired.
    let reopened = SessionManager::open(&path).unwrap();
    assert_eq!(reopened.entries().len(), 1);
    let content = std::fs::read_to_string(&path).unwrap();
    assert!(content.ends_with('\n'));

    // A non-empty file that does not parse as a pi session fails without
    // being modified (upstream `_setSessionFile`).
    let v4_path = dir.join("legacy.jsonl");
    std::fs::write(
        &v4_path,
        "{\"v\":4,\"kind\":\"header\",\"id\":\"x\",\"storageVersion\":1,\"createdAt\":1,\"cwd\":\".\"}\n",
    )
    .unwrap();
    let error = SessionManager::open(&v4_path).unwrap_err();
    assert!(
        error.to_string().contains("not a valid pi session"),
        "unexpected error: {error}"
    );
    assert_eq!(
        std::fs::read_to_string(&v4_path).unwrap(),
        "{\"v\":4,\"kind\":\"header\",\"id\":\"x\",\"storageVersion\":1,\"createdAt\":1,\"cwd\":\".\"}\n",
        "the invalid file must not be modified"
    );

    // Empty file: initialized with a header.
    let empty_path = dir.join("empty.jsonl");
    std::fs::write(&empty_path, "").unwrap();
    let initialized = SessionManager::open(&empty_path).unwrap();
    assert_eq!(initialized.id().len(), 36);
    let header_line = std::fs::read_to_string(&empty_path).unwrap();
    let header: serde_json::Value = serde_json::from_str(header_line.trim()).unwrap();
    assert_eq!(header["type"], "session");
    assert_eq!(header["version"], CURRENT_SESSION_VERSION);
    drop(initialized);

    std::fs::remove_dir_all(&dir).unwrap();
}

// ---------------------------------------------------------------------------
// Projection: context edits and compaction
// ---------------------------------------------------------------------------

#[test]
fn context_edit_replaces_and_omits_targets() {
    let dir = temp_dir("context-edit");
    let mut session = SessionManager::create("C:/repo", &dir).unwrap();
    let user_id = session
        .append_message(user_message("original question"))
        .unwrap();
    let tool_call_id = session
        .append_message(assistant_message("working", StopReason::ToolUse))
        .unwrap();
    session.append_message(user_message("follow up")).unwrap();

    // Replace the user message content.
    session
        .append_context_edit(&user_id, Some(serde_json::json!("redacted question")))
        .unwrap();
    let context = session.build_context_entries().unwrap();
    let texts: Vec<String> = context.iter().map(text_of).collect();
    assert_eq!(texts, vec!["redacted question", "working", "follow up"]);

    // Omit the tool-call assistant message entirely.
    session.append_context_edit(&tool_call_id, None).unwrap();
    let context = session.build_context_entries().unwrap();
    let texts: Vec<String> = context.iter().map(text_of).collect();
    assert_eq!(texts, vec!["redacted question", "follow up"]);

    // Assistant string replacements become text blocks (upstream rule).
    session
        .append_context_edit(&tool_call_id, Some(serde_json::json!("stand-in")))
        .unwrap();
    let context = session.build_context_entries().unwrap();
    assert_eq!(context.len(), 3);
    assert!(matches!(&context[1], Message::Assistant(a)
        if matches!(&a.content[..], [ContentBlock::Text(t)] if t.text == "stand-in")));

    // Editing an entry that is not on the active branch fails.
    let stray = session.append_message(user_message("stray")).unwrap();
    session.branch(&tool_call_id).unwrap();
    assert!(session.append_context_edit(&stray, None).is_err());
    assert!(session.append_context_edit("missing-id", None).is_err());

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn compaction_projection_replays_summary_plus_kept_range() {
    let dir = temp_dir("projection");
    let mut session = SessionManager::create("C:/repo", &dir).unwrap();
    session.append_message(user_message("q1")).unwrap();
    session
        .append_message(assistant_message("a1", StopReason::Stop))
        .unwrap();
    let u2 = session.append_message(user_message("q2")).unwrap();
    session
        .append_message(assistant_message("a2", StopReason::Stop))
        .unwrap();
    // Keep from q2 onwards.
    session
        .append_compaction("earlier summarized", Some(&u2), 999, None)
        .unwrap();
    session.append_message(user_message("q3")).unwrap();

    let context = session.build_context_entries().unwrap();
    let texts: Vec<String> = context.iter().map(text_of).collect();
    // summary (framed like upstream) + kept (q2, a2) + after (q3).
    assert_eq!(texts.len(), 4);
    assert!(texts[0].contains("earlier summarized"), "{texts:?}");
    assert_eq!(
        &texts[1..],
        &["q2".to_owned(), "a2".to_owned(), "q3".to_owned()]
    );

    // A second compaction replaces the first (newest-compaction-only): its
    // own kept range is authoritative and older compactions contribute
    // nothing. Self-retaining (None) keeps only the summary + later entries.
    session
        .append_compaction("second summary", None, 100, None)
        .unwrap();
    session.append_message(user_message("q4")).unwrap();
    let context = session.build_context_entries().unwrap();
    let texts: Vec<String> = context.iter().map(text_of).collect();
    assert_eq!(texts.len(), 2, "{texts:?}");
    assert!(texts[0].contains("second summary"));
    assert_eq!(texts[1], "q4");
}

#[test]
fn model_and_thinking_settings_are_tracked_from_the_branch() {
    let mut session = SessionManager::in_memory("C:\\repo", None).unwrap();
    session.append_thinking_level_change("high").unwrap();
    session
        .append_model_change("anthropic", "claude-opus-4-5")
        .unwrap();
    session.append_message(user_message("hi")).unwrap();
    let projection = session.build_session_projection().unwrap();
    assert_eq!(projection.thinking_level, "high");
    assert_eq!(
        projection.model,
        Some(("anthropic".to_owned(), "claude-opus-4-5".to_owned()))
    );
    // Assistant messages update the tracked model.
    session
        .append_message(assistant_message("hey", StopReason::Stop))
        .unwrap();
    let projection = session.build_session_projection().unwrap();
    assert_eq!(projection.model, Some((String::new(), String::new())));
}

#[test]
fn state_entries_contribute_no_context() {
    let dir = temp_dir("state-entries");
    let mut session = SessionManager::create("C:/repo", &dir).unwrap();
    session.append_message(user_message("hello")).unwrap();
    session.append_custom_entry("note", None).unwrap();
    session
        .append_usage("cache_warm", "anthropic", "claude", Usage::default(), None)
        .unwrap();
    let leaf = session.leaf_id().unwrap().to_owned();
    session.set_label(&leaf, Some("x")).unwrap();
    let context = session.build_context_entries().unwrap();
    assert_eq!(context.len(), 1);

    std::fs::remove_dir_all(&dir).unwrap();
}

// ---------------------------------------------------------------------------
// Branch / fork
// ---------------------------------------------------------------------------

#[test]
fn branch_moves_the_leaf_and_appends_form_a_new_child() {
    let dir = temp_dir("branch");
    let mut session = SessionManager::create("C:/repo", &dir).unwrap();
    let root = session.append_message(user_message("root")).unwrap();
    let tail = session.append_message(user_message("tail")).unwrap();

    session.branch(&root).unwrap();
    let branched = session.append_message(user_message("branched")).unwrap();
    let context = session.build_context_entries().unwrap();
    let texts: Vec<String> = context.iter().map(text_of).collect();
    assert_eq!(texts, vec!["root", "branched"]);
    assert_eq!(session.leaf_id(), Some(branched.as_str()));

    // Unknown branch target fails.
    assert!(session.branch("missing-id").is_err());
    let _ = tail;

    // resetLeaf: the next append becomes a new root.
    session.reset_leaf();
    let new_root = session.append_message(user_message("new root")).unwrap();
    let entry = session.entry(&new_root).unwrap();
    assert!(entry.parent_id().is_none());

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn branched_session_copies_path_and_preserves_self_retaining_compaction() {
    let dir = temp_dir("branched-file");
    let mut session = SessionManager::create("C:/repo", &dir).unwrap();
    session.append_message(user_message("dropped")).unwrap();
    let kept = session.append_message(user_message("kept")).unwrap();
    // A self-retaining compaction (firstKeptEntryId === own id).
    let compaction_id = session
        .append_compaction("checkpoint", None, 42, None)
        .unwrap();
    session.append_message(user_message("after")).unwrap();
    session.set_label(&kept, Some("keep-me")).unwrap();
    let previous_path = session.path().to_path_buf();

    // Branch at the compaction entry: the copied path is
    // root..compaction, so the self-retaining cut must survive the copy.
    let new_path = session
        .create_branched_session(&compaction_id)
        .unwrap()
        .unwrap();

    // New header with parentSession pointing at the source file.
    let lines = lines_of(&new_path);
    let header = &lines[0];
    assert_eq!(header["type"], "session");
    assert_eq!(header["version"], CURRENT_SESSION_VERSION);
    assert_eq!(
        header["parentSession"],
        previous_path.to_string_lossy().as_ref()
    );

    // The path contains kept..compaction..after, labels re-appended at the
    // end, and the compaction stays self-retaining.
    let entries: Vec<&serde_json::Value> = lines[1..].iter().collect();
    let compaction = entries
        .iter()
        .find(|entry| entry["type"] == "compaction")
        .unwrap();
    assert_eq!(compaction["id"], compaction_id.as_str());
    assert_eq!(compaction["firstKeptEntryId"], compaction_id.as_str());

    // The new manager replays the copied path.
    let branched = SessionManager::open(&new_path).unwrap();
    let context = branched.build_context_entries().unwrap();
    let texts: Vec<String> = context.iter().map(text_of).collect();
    // Self-retaining compaction replays ONLY its summary (nothing retained
    // before it, and nothing after it was copied).
    assert_eq!(texts.len(), 1, "{texts:?}");
    assert!(texts[0].contains("checkpoint"));
    assert_eq!(branched.get_label(&kept).as_deref(), Some("keep-me"));

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
#[cfg_attr(target_os = "linux", ignore = "asserts Windows path encoding")]
fn fork_from_source_copies_history_with_parent_session() {
    let dir = temp_dir("fork-src");
    let target_dir = temp_dir("fork-dst");
    let mut source = SessionManager::create("C:/repo", &dir).unwrap();
    source.append_message(user_message("one")).unwrap();
    source.append_message(user_message("two")).unwrap();

    let forked =
        SessionManager::fork_from_source(source.path(), "C:\\target-repo", &target_dir, None)
            .unwrap();
    assert_ne!(forked.id(), source.id());
    assert_eq!(
        forked.parent_session(),
        Some(source.path().to_string_lossy().as_ref())
    );
    assert_eq!(forked.cwd(), "C:\\target-repo");
    let context = forked.build_context_entries().unwrap();
    let texts: Vec<String> = context.iter().map(text_of).collect();
    assert_eq!(texts, vec!["one", "two"]);

    // Forks are appendable.
    let mut forked = forked;
    forked.append_message(user_message("three")).unwrap();

    std::fs::remove_dir_all(&dir).unwrap();
    std::fs::remove_dir_all(&target_dir).unwrap();
}

// ---------------------------------------------------------------------------
// Directory layout + listing
// ---------------------------------------------------------------------------

#[test]
#[cfg_attr(target_os = "linux", ignore = "asserts Windows path encoding")]
fn session_directory_layout_matches_upstream_encoding() {
    // Upstream `getDefaultSessionDirPath`: `--` + resolved cwd with / \ :
    // replaced by '-' + `--`, under <agent-dir>/sessions.
    assert_eq!(
        pir::session_manager::encode_cwd_dir_name("C:\\Users\\x\\repo"),
        "--C--Users-x-repo--"
    );
    assert_eq!(
        pir::session_manager::encode_cwd_dir_name("/home/user/repo"),
        "--home-user-repo--"
    );
    let dir = pir::session_manager::default_session_dir_path("C:\\Users\\x\\repo");
    let encoded = dir.file_name().unwrap().to_str().unwrap();
    assert_eq!(encoded, "--C--Users-x-repo--");
    assert_eq!(
        dir.parent().unwrap().file_name().unwrap().to_str(),
        Some("sessions")
    );
}

#[test]
fn session_files_use_upstream_names() {
    let dir = temp_dir("file-name");
    let session = SessionManager::create("C:/repo", &dir).unwrap();
    let name = session.path().file_name().unwrap().to_str().unwrap();
    // <ISO with : and . replaced by ->_<session id>.jsonl
    let (stamp, rest) = name.split_once('_').unwrap();
    assert!(stamp.ends_with('Z') || stamp.contains('T'), "{stamp}");
    assert!(!stamp.contains(':') && !stamp.contains('.'));
    let (id, ext) = rest.split_once('.').unwrap();
    assert_eq!(ext, "jsonl");
    assert_eq!(id.len(), 36, "uuid v7 session id: {id}");

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
#[cfg_attr(target_os = "linux", ignore = "asserts Windows path encoding")]
fn list_sessions_walks_layout_and_skips_old_format_files() {
    let dir = temp_dir("listing");
    // Two upstream sessions for different cwds.
    let mut first = SessionManager::create("C:\\proj", &dir).unwrap();
    let first_id = first.id().to_owned();
    first.append_message(user_message("one")).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(20));
    let mut second = SessionManager::create("C:\\proj", &dir).unwrap();
    let second_id = second.id().to_owned();
    second.append_message(user_message("two")).unwrap();
    // A session for another cwd.
    let mut other = SessionManager::create("C:\\other", &dir).unwrap();
    other.append_message(user_message("other")).unwrap();
    // An old pir v4 file: must be skipped, not crash.
    std::fs::write(
        dir.join("legacy-v4.jsonl"),
        "{\"v\":4,\"kind\":\"header\",\"id\":\"legacy\",\"storageVersion\":1,\"createdAt\":1,\"cwd\":\"C:\\\\proj\"}\n",
    )
    .unwrap();
    // A garbage file: skipped.
    std::fs::write(dir.join("garbage.jsonl"), "not json at all\n").unwrap();
    // Non-jsonl files are ignored.
    std::fs::write(dir.join("notes.txt"), "ignore").unwrap();

    let all = SessionManager::list_sessions(&dir, None);
    let ids: Vec<&str> = all.iter().map(|summary| summary.id.as_str()).collect();
    assert_eq!(ids.len(), 3, "legacy/garbage files are skipped: {ids:?}");
    // Most recently modified first: `other` was appended last.
    assert_eq!(ids[0], other.id());
    assert_eq!(ids[2], first_id);

    let filtered = SessionManager::list_sessions(&dir, Some("C:\\proj"));
    assert_eq!(filtered.len(), 2);
    assert!(filtered.iter().all(|summary| summary.cwd == "C:\\proj"));

    // find_session_by_id matches exact header ids.
    assert_eq!(
        SessionManager::find_session_by_id(&dir, &first_id).unwrap(),
        first.path()
    );
    assert!(SessionManager::find_session_by_id(&dir, "missing").is_none());

    // continue_recent resumes the most recent matching session.
    let resumed = SessionManager::continue_recent(&dir, "C:\\proj").unwrap();
    assert_eq!(resumed.id(), second_id);

    std::fs::remove_dir_all(&dir).unwrap();
}

// ---------------------------------------------------------------------------
// Custom ids + usage/compaction typed access
// ---------------------------------------------------------------------------

#[test]
fn custom_session_ids_are_validated() {
    let dir = temp_dir("custom-id");
    let session = SessionManager::create_with_id("C:\\repo", &dir, "my-session.id").unwrap();
    assert_eq!(session.id(), "my-session.id");
    assert_eq!(session.header().unwrap().id, "my-session.id");
    assert!(SessionManager::create_with_id("C:\\repo", &dir, "bad id/").is_err());
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn typed_entry_accessors_read_back_what_was_written() {
    let dir = temp_dir("typed");
    let mut session = SessionManager::create("C:/repo", &dir).unwrap();
    let user_id = session.append_message(user_message("hello")).unwrap();
    let compaction_id = session
        .append_compaction(
            "sum",
            Some(&user_id),
            10,
            Some(serde_json::json!({"readFiles": ["a"]})),
        )
        .unwrap();

    let reopened = SessionManager::open(session.path()).unwrap();
    match reopened.entry(&compaction_id).unwrap() {
        FileEntry::Compaction(CompactionEntry {
            summary,
            first_kept_entry_id,
            tokens_before,
            details,
            ..
        }) => {
            assert_eq!(summary, "sum");
            assert_eq!(first_kept_entry_id, &user_id);
            assert_eq!(*tokens_before, 10);
            assert_eq!(details.as_ref().unwrap()["readFiles"][0], "a");
        }
        other => panic!("expected compaction entry, got {other:?}"),
    }
    match reopened.entry(&user_id).unwrap() {
        FileEntry::Message(MessageEntry { message, .. }) => {
            assert!(text_of(&message.clone().into()).contains("hello"));
        }
        other => panic!("expected message entry, got {other:?}"),
    }

    // Unknown entry ids and duplicate compaction target validation.
    assert!(reopened.entry("ffffffff").is_none());

    // Label + session_info typed access (append order: label, then name).
    session.set_label(&user_id, Some("mark")).unwrap();
    session.set_session_name(Some("named")).unwrap();
    match reopened_settle(&session).as_slice() {
        [.., FileEntry::Label(LabelEntry {
            label, target_id, ..
        }), FileEntry::SessionInfo(SessionInfoEntry { name, .. })] => {
            assert_eq!(name.as_deref(), Some("named"));
            assert_eq!(label.as_deref(), Some("mark"));
            assert_eq!(target_id, &user_id);
        }
        other => panic!("unexpected tail entries: {other:?}"),
    }

    // Usage entries deserialize into UsageEntry.
    session
        .append_usage("cache_warm", "p", "m", Usage::default(), Some("n"))
        .unwrap();
    let reopened2 = SessionManager::open(session.path()).unwrap();
    assert!(matches!(
        reopened2.entries().last().unwrap(),
        FileEntry::Usage(UsageEntry { kind, note, .. })
            if kind == "cache_warm" && note.as_deref() == Some("n")
    ));

    std::fs::remove_dir_all(&dir).unwrap();
}

fn reopened_settle(session: &SessionManager) -> Vec<FileEntry> {
    SessionManager::open(session.path()).unwrap().entries()
}

// ---------------------------------------------------------------------------
// P1-3: v2 hookMessage migration + undeserializable-line preservation
// ---------------------------------------------------------------------------

fn write_v2_hook_session(path: &std::path::Path, extra_line: Option<&str>) {
    let mut content = String::from(
        "{\"type\":\"session\",\"version\":2,\"id\":\"11111111-2222-3333-4444-555555555555\",\
         \"timestamp\":\"2025-01-01T00:00:00.000Z\",\"cwd\":\"C:\\\\repo\"}\n",
    );
    content.push_str(
        "{\"type\":\"message\",\"id\":\"aa11bb22\",\"parentId\":null,\
         \"timestamp\":\"2025-01-01T00:00:01.000Z\",\
         \"message\":{\"role\":\"hookMessage\",\"content\":\"hook text\",\"timestamp\":1735689601000}}\n",
    );
    if let Some(line) = extra_line {
        content.push_str(line);
        if !line.ends_with('\n') {
            content.push('\n');
        }
    }
    std::fs::write(path, content).unwrap();
}

#[test]
fn v2_hook_message_migrates_to_custom_preserving_data() {
    let dir = temp_dir("hook-migrate");
    let path = dir.join("v2-hook.jsonl");
    write_v2_hook_session(&path, None);

    let mut reopened = SessionManager::open(&path).unwrap();
    let entries = reopened.entries();
    assert_eq!(entries.len(), 1);
    match &entries[0] {
        FileEntry::Message(MessageEntry { message, .. }) => {
            assert!(matches!(
                message,
                pir::session_manager::StoredMessage::Custom(_)
            ));
            assert_eq!(message.role(), "custom");
        }
        other => panic!("expected message entry, got {other:?}"),
    }

    // The migrated message contributes to LLM context like upstream.
    let context = reopened.build_context_entries().unwrap();
    let texts: Vec<String> = context.iter().map(text_of).collect();
    assert_eq!(texts, vec!["hook text"]);

    // The file was rewritten at v3 with the role renamed; the remaining
    // message fields (content, timestamp) are preserved verbatim.
    let lines = lines_of(&path);
    assert_eq!(lines[0]["type"], "session");
    assert_eq!(lines[0]["version"], CURRENT_SESSION_VERSION);
    assert_eq!(lines[1]["message"]["role"], "custom");
    assert_eq!(lines[1]["message"]["content"], "hook text");
    assert_eq!(lines[1]["message"]["timestamp"], 1_735_689_601_000u64);

    // The session stays appendable after the migration.
    reopened.append_message(user_message("after")).unwrap();
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn undeserializable_lines_survive_migration_without_rewrite() {
    let dir = temp_dir("refuse-rewrite");
    let path = dir.join("v2-garbage.jsonl");
    let garbage = "this line is not json at all";
    write_v2_hook_session(&path, Some(garbage));
    let original = std::fs::read_to_string(&path).unwrap();

    // Loading succeeds; the parseable entries (with the hookMessage rename)
    // are usable in memory.
    let mut reopened = SessionManager::open(&path).unwrap();
    assert_eq!(reopened.entries().len(), 1);
    let context = reopened.build_context_entries().unwrap();
    let texts: Vec<String> = context.iter().map(text_of).collect();
    assert_eq!(texts, vec!["hook text"]);

    // The migration rewrite must be REFUSED: the on-disk file still holds
    // the v2 header, the original hookMessage line, and the garbage line.
    let after = std::fs::read_to_string(&path).unwrap();
    assert_eq!(after, original, "file must not be rewritten");
    assert!(after.contains("\"version\":2"));
    assert!(after.contains("hookMessage"));
    assert!(after.contains(garbage));

    // Appends still work and never erase the preserved lines.
    reopened.append_message(user_message("next")).unwrap();
    let content = std::fs::read_to_string(&path).unwrap();
    assert!(content.contains(garbage), "garbage line was erased");

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn fork_from_source_carries_raw_text_for_unparseable_lines() {
    let dir = temp_dir("fork-raw-src");
    let target_dir = temp_dir("fork-raw-dst");
    let mut source = SessionManager::create("C:/repo", &dir).unwrap();
    source.append_message(user_message("kept")).unwrap();
    let source_path = source.path().to_path_buf();
    let garbage = "{\"type\":\"mystery\",\"payload\":{\"x\":[1,2,3]}}";
    {
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&source_path)
            .unwrap();
        writeln!(file, "{garbage}").unwrap();
    }

    let forked =
        SessionManager::fork_from_source(&source_path, "C:\\dst", &target_dir, None).unwrap();
    assert_eq!(forked.entries().len(), 1, "only parsed entries replay");
    let forked_content = std::fs::read_to_string(forked.path()).unwrap();
    assert!(
        forked_content.contains(garbage),
        "raw unparseable line must be carried verbatim: {forked_content}"
    );
    // The forked session itself tolerates (and remembers) the raw line.
    let reopened = SessionManager::open(forked.path()).unwrap();
    assert_eq!(reopened.entries().len(), 1);

    std::fs::remove_dir_all(&dir).unwrap();
    std::fs::remove_dir_all(&target_dir).unwrap();
}

// ---------------------------------------------------------------------------
// P1-4: streaming reads (no mid-line truncation, lossy UTF-8 tolerated)
// ---------------------------------------------------------------------------

#[test]
fn large_unterminated_final_line_parses_whole_and_is_repaired_once() {
    let dir = temp_dir("big-torn-line");
    let mut session = SessionManager::create("C:/repo", &dir).unwrap();
    session.append_message(user_message("first")).unwrap();
    let path = session.path().to_path_buf();

    // A final line larger than the 1 MiB read buffer with no trailing
    // newline: it must be read as ONE entry (never split) and repaired.
    let big_text = "x".repeat(2 * 1024 * 1024);
    {
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        let line = serde_json::json!({
            "type": "message",
            "id": "feedface",
            "parentId": null,
            "timestamp": "2025-01-01T00:00:00.000Z",
            "message": {"role": "user", "content": [{"type": "text", "text": big_text}],
                        "timestamp": 1735689600000u64}
        });
        write!(file, "{line}").unwrap();
    }
    let reopened = SessionManager::open(&path).unwrap();
    assert_eq!(reopened.entries().len(), 2);
    let content = std::fs::read_to_string(&path).unwrap();
    assert!(content.ends_with('\n'));
    // Repairing twice must not happen (the repaired file ends in a newline).
    let reopened2 = SessionManager::open(&path).unwrap();
    assert_eq!(reopened2.entries().len(), 2);
    assert_eq!(std::fs::read_to_string(&path).unwrap(), content);

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn invalid_utf8_is_tolerated_lossily_without_losing_entries() {
    let dir = temp_dir("lossy-utf8");
    let mut session = SessionManager::create("C:/repo", &dir).unwrap();
    session.append_message(user_message("before")).unwrap();
    let path = session.path().to_path_buf();
    {
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        // Torn tail with invalid UTF-8 and no trailing newline.
        file.write_all(b"\xff\xfe not utf8").unwrap();
    }
    let reopened = SessionManager::open(&path).unwrap();
    assert_eq!(reopened.entries().len(), 1, "lossy line is not an entry");
    // Repair ran; the lossy line's raw text is preserved in the file.
    let bytes = std::fs::read(&path).unwrap();
    let content = String::from_utf8_lossy(&bytes).into_owned();
    assert!(content.ends_with('\n'));
    assert!(content.contains("not utf8"));
    std::fs::remove_dir_all(&dir).unwrap();
}

// ---------------------------------------------------------------------------
// P2-7: UNC paths resolve and encode like upstream
// ---------------------------------------------------------------------------

#[test]
#[cfg_attr(target_os = "linux", ignore = "asserts Windows path encoding")]
fn unc_paths_resolve_and_encode_like_upstream() {
    use pir::session_manager::{encode_cwd_dir_name, resolve_path_str_with};
    use std::path::Path;
    // Explicit literal home/cwd: resolution must not consult the process
    // environment (every input below is absolute, so neither is joined).
    let cwd = Path::new("C:\\base");
    // UNC prefixes survive resolution (Node win32 path.resolve semantics).
    assert_eq!(
        resolve_path_str_with("\\\\server\\share\\dir\\sub", None, cwd),
        "\\\\server\\share\\dir\\sub"
    );
    assert_eq!(
        resolve_path_str_with("\\\\server\\share\\..\\other", None, cwd),
        "\\\\server\\share\\other"
    );
    // A root-relative path resolves onto the current drive's root, like
    // Node's win32 `path.resolve("/a/b/../c")` 鈫?`<cwd-drive>:\a\c`.
    assert_eq!(
        resolve_path_str_with("C:\\a\\b\\..\\c", None, cwd),
        "C:\\a\\c"
    );
    assert_eq!(resolve_path_str_with("C:\\a\\..\\b", None, cwd), "C:\\b");

    // Upstream getDefaultSessionDirPath strips exactly ONE leading separator
    // and maps the remaining / \ : to -, so `\\server\share\dir` keeps its
    // second leading separator only as a dash.
    assert_eq!(
        encode_cwd_dir_name("\\\\server\\share\\dir"),
        "---server-share-dir--"
    );
    assert_eq!(encode_cwd_dir_name("/home/u/repo"), "--home-u-repo--");
    assert_eq!(encode_cwd_dir_name("C:\\Users\\x"), "--C--Users-x--");
}

// ---------------------------------------------------------------------------
// P2-7b: the explicit-parameter core never reads the process environment
// ---------------------------------------------------------------------------

#[test]
fn resolve_path_str_with_uses_explicit_home_and_cwd_only() {
    use pir::session_manager::resolve_path_str_with;
    use std::path::Path;
    // `~` expands against the EXPLICIT home; no ambient `$HOME` /
    // `$USERPROFILE` can leak into the result.
    #[cfg(windows)]
    assert_eq!(
        resolve_path_str_with(
            "~/repo",
            Some(Path::new("C:\\home\\u")),
            Path::new("C:\\base")
        ),
        "C:\\home\\u\\repo"
    );
    #[cfg(not(windows))]
    assert_eq!(
        resolve_path_str_with("~/repo", Some(Path::new("/home/u")), Path::new("/base")),
        "/home/u/repo"
    );
    // `home = None` leaves the tilde verbatim (the path then lands on the
    // explicit cwd) instead of reading the process environment.
    assert_eq!(resolve_path_str_with("~", None, Path::new(".")), "~");
    // A relative path resolves onto the explicit cwd, not the process cwd.
    assert_eq!(
        resolve_path_str_with("sub/./file", None, Path::new("/base")),
        "/base/sub/file"
    );
}

// ---------------------------------------------------------------------------
// P2-8: open() of an empty file falls back to process cwd
// ---------------------------------------------------------------------------

#[test]
fn open_empty_file_uses_process_cwd_not_file_dir() {
    let dir = temp_dir("empty-cwd");
    let empty_path = dir.join("empty.jsonl");
    std::fs::write(&empty_path, "").unwrap();
    let manager = SessionManager::open(&empty_path).unwrap();
    // The expected value is the explicit-form resolve of the process cwd:
    // `None` home and a `.` base keep it immune to ambient `$HOME` /
    // `$USERPROFILE` (the path is absolute, so neither is consulted), while
    // the production `open()` boundary remains the subject under test.
    let process_cwd = std::env::current_dir().unwrap();
    let expected = pir::session_manager::resolve_path_str_with(
        &process_cwd.to_string_lossy(),
        None,
        std::path::Path::new("."),
    );
    assert_eq!(manager.cwd(), expected, "cwd must be process.cwd()");
    // The initialized header carries the same cwd.
    let header_line = std::fs::read_to_string(&empty_path).unwrap();
    let header: serde_json::Value = serde_json::from_str(header_line.trim()).unwrap();
    assert_eq!(header["cwd"], expected);
    std::fs::remove_dir_all(&dir).unwrap();
}

// ---------------------------------------------------------------------------
// P2-9: header detection searches the whole entry list
// ---------------------------------------------------------------------------

#[test]
#[cfg_attr(target_os = "linux", ignore = "asserts Windows path encoding")]
fn header_detection_searches_the_whole_entry_list() {
    let dir = temp_dir("header-search");
    let path = dir.join("header-late.jsonl");
    std::fs::write(&path, "").unwrap();
    let header = pir::session_manager::SessionHeader::new(
        "cafe1234",
        "C:\\late-cwd",
        "2025-01-01T00:00:00.000Z".to_owned(),
    );
    let message = FileEntry::Message(MessageEntry {
        id: "deadbeef".to_owned(),
        parent_id: None,
        timestamp: "2025-01-01T00:00:01.000Z".to_owned(),
        message: user_message("orphan").into(),
    });
    // The header is NOT the first entry; detection must still find it.
    let mut manager = SessionManager::open(&path).unwrap();
    manager
        .set_session_file(&path, Some(vec![message, FileEntry::Session(header)]))
        .unwrap();
    assert_eq!(manager.id(), "cafe1234");
    assert_eq!(manager.cwd(), "C:\\late-cwd");
    assert_eq!(manager.entries().len(), 1);
    std::fs::remove_dir_all(&dir).unwrap();
}

// ---------------------------------------------------------------------------
// P2-11: clear-value record byte shapes match upstream
// ---------------------------------------------------------------------------

#[test]
fn clear_value_records_match_upstream_byte_shapes() {
    let dir = temp_dir("clear-shapes");
    let mut session = SessionManager::create("C:/repo", &dir).unwrap();
    let target = session.append_message(user_message("hi")).unwrap();
    let custom_id = session
        .append_custom_message_entry("note", serde_json::json!("ctx"), true, None)
        .unwrap();

    // Upstream appendLabelChange("") serializes `label: ""` and clears.
    session.set_label(&target, Some("mark")).unwrap();
    assert_eq!(session.get_label(&target).as_deref(), Some("mark"));
    session.set_label(&target, Some("")).unwrap();
    assert_eq!(session.get_label(&target), None, "empty label clears");
    // Upstream appendSessionInfo("") serializes `name: ""` and clears.
    session.set_session_name(Some("named")).unwrap();
    session.set_session_name(None).unwrap();
    // Upstream stores a string replacement on a custom_message target raw
    // ({content: "str"}), without normalizing it into a text block.
    session
        .append_context_edit(&custom_id, Some(serde_json::json!("raw string")))
        .unwrap();

    let lines = lines_of(session.path());
    let label = lines
        .iter()
        .rev()
        .find(|line| line["type"] == "label" && line["targetId"] == target.as_str())
        .unwrap();
    assert_eq!(
        label["label"], "",
        "clearing must serialize label:\"\" (not omit it)"
    );
    assert!(label.as_object().unwrap().contains_key("label"));
    let info = lines
        .iter()
        .rev()
        .find(|line| line["type"] == "session_info" && line.get("name").is_some())
        .unwrap();
    assert_eq!(info["name"], "", "clearing must serialize name:\"\"");
    let edit = lines
        .iter()
        .find(|line| line["type"] == "context_edit" && line["targetId"] == custom_id.as_str())
        .unwrap();
    assert_eq!(
        edit["replacement"]["content"], "raw string",
        "custom-target string replacements stay raw strings"
    );

    // Session name cleared by the empty string.
    assert_eq!(session.session_name(), None);
    std::fs::remove_dir_all(&dir).unwrap();
}

// ---------------------------------------------------------------------------
// P2-6: timestamp parser strictness + short fractions
// ---------------------------------------------------------------------------

#[test]
fn ms_from_iso_rejects_invalid_clock_fields_and_parses_short_fractions() {
    use pir::session_manager::{iso_from_ms, ms_from_iso};

    // Round trip of pir's own writer output.
    assert_eq!(
        ms_from_iso("2025-06-15T12:34:56.789Z"),
        Some(1_749_990_896_789)
    );
    assert_eq!(
        ms_from_iso(&iso_from_ms(1_749_990_896_789)),
        Some(1_749_990_896_789)
    );

    // No fraction and no fraction-with-Z.
    assert_eq!(ms_from_iso("2025-06-15T12:34:56Z"), Some(1_749_990_896_000));

    // Short fractions weight like Date.parse: .5 鈫?500 ms, .05 鈫?50 ms.
    assert_eq!(
        ms_from_iso("2025-06-15T12:34:56.5Z"),
        Some(1_749_990_896_500)
    );
    assert_eq!(
        ms_from_iso("2025-06-15T12:34:56.05Z"),
        Some(1_749_990_896_050)
    );
    // Digits beyond milliseconds are truncated.
    assert_eq!(
        ms_from_iso("2025-06-15T12:34:56.123456Z"),
        Some(1_749_990_896_123)
    );
    // A lone dot with no digits is invalid.
    assert_eq!(ms_from_iso("2025-06-15T12:34:56.Z"), None);

    // Invalid clock fields are rejected (previously T99:99:99 parsed).
    assert_eq!(ms_from_iso("2025-06-15T99:00:00.000Z"), None);
    assert_eq!(ms_from_iso("2025-06-15T12:99:00.000Z"), None);
    assert_eq!(ms_from_iso("2025-06-15T12:34:99.000Z"), None);

    // Calendar validity: per-month day lengths and leap years.
    assert_eq!(ms_from_iso("2025-04-31T00:00:00.000Z"), None);
    assert!(ms_from_iso("2024-02-29T00:00:00.000Z").is_some());
    assert_eq!(ms_from_iso("2023-02-29T00:00:00.000Z"), None);
    assert_eq!(ms_from_iso("2025-13-01T00:00:00.000Z"), None);
    assert_eq!(ms_from_iso("2025-00-01T00:00:00.000Z"), None);

    // Malformed separators and garbage.
    assert_eq!(ms_from_iso("2025/06/15T12:34:56Z"), None);
    assert_eq!(ms_from_iso("not a timestamp"), None);
    assert_eq!(ms_from_iso(""), None);
}

#[test]
fn context_edit_typed_shape_round_trips() {
    let dir = temp_dir("edit-typed");
    let mut session = SessionManager::create("C:/repo", &dir).unwrap();
    let user_id = session.append_message(user_message("hi")).unwrap();
    session
        .append_context_edit(
            &user_id,
            Some(serde_json::json!([{"type": "text", "text": "x"}])),
        )
        .unwrap();

    let reopened = SessionManager::open(session.path()).unwrap();
    match reopened.entries().last().unwrap() {
        FileEntry::ContextEdit(ContextEditEntry {
            target_id,
            replacement,
            ..
        }) => {
            assert_eq!(target_id, &user_id);
            let replacement = replacement.as_ref().unwrap();
            assert_eq!(replacement.content[0]["text"], "x");
        }
        other => panic!("expected context_edit entry, got {other:?}"),
    }

    std::fs::remove_dir_all(&dir).unwrap();
}

// ---------------------------------------------------------------------------
// append_compaction usage/from_hook + branchWithSummary (upstream
// session-manager.ts:1261-1287, 1600-1625)
// ---------------------------------------------------------------------------

#[test]
fn append_compaction_round_trips_usage_and_from_hook() {
    let dir = temp_dir("compaction-usage");
    let mut session = SessionManager::create("C:/repo", &dir).unwrap();
    let user_id = session.append_message(user_message("hello")).unwrap();

    let usage = Usage {
        input: 10,
        output: 20,
        cache_read: 5,
        cache_write: 2,
        total_tokens: 37,
        ..Usage::default()
    };
    let extension_id = session
        .append_compaction_with_usage(
            "extension summary",
            Some(&user_id),
            100,
            None,
            Some(usage),
            true,
        )
        .unwrap();
    let pi_id = session
        .append_compaction_with_usage("pi summary", Some(&extension_id), 200, None, None, false)
        .unwrap();
    // The plain 4-arg method stays the pi-generated default.
    let plain_id = session
        .append_compaction("plain", Some(&pi_id), 300, None)
        .unwrap();

    let lines = lines_of(session.path());
    let extension_line = &lines
        .iter()
        .find(|line| line["id"] == extension_id.as_str())
        .unwrap();
    assert_eq!(extension_line["fromHook"], serde_json::json!(true));
    assert_eq!(extension_line["usage"]["input"], 10);
    assert_eq!(extension_line["usage"]["totalTokens"], 37);
    let pi_line = &lines
        .iter()
        .find(|line| line["id"] == pi_id.as_str())
        .unwrap();
    // pi-generated compactions omit fromHook on the wire (upstream parity).
    assert!(pi_line.get("fromHook").is_none());
    assert!(pi_line.get("usage").is_none());
    let plain_line = &lines
        .iter()
        .find(|line| line["id"] == plain_id.as_str())
        .unwrap();
    assert!(plain_line.get("fromHook").is_none());
    assert!(plain_line.get("usage").is_none());

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn branch_with_summary_appends_a_branch_summary_entry() {
    let dir = temp_dir("branch-with-summary");
    let mut session = SessionManager::create("C:/repo", &dir).unwrap();
    let user_id = session.append_message(user_message("start")).unwrap();
    let assistant_id = session
        .append_message(assistant_message("reply", StopReason::Stop))
        .unwrap();

    let usage = Usage {
        input: 1,
        output: 2,
        total_tokens: 3,
        ..Usage::default()
    };
    let summary_id = session
        .branch_with_summary(
            Some(&user_id),
            "the abandoned path discussed X",
            None,
            Some(false),
            Some(usage),
        )
        .unwrap();

    // The leaf moved to the branch point; the summary is a child of it.
    assert_eq!(session.leaf_id(), Some(summary_id.as_str()));
    match session.entry(&summary_id).unwrap() {
        FileEntry::BranchSummary(entry) => {
            assert_eq!(
                entry.from_id, assistant_id,
                "fromId is the leaf before branching"
            );
            assert_eq!(entry.parent_id.as_deref(), Some(user_id.as_str()));
            assert_eq!(entry.summary, "the abandoned path discussed X");
            assert_eq!(entry.usage.as_ref().unwrap().total_tokens, 3);
            assert_eq!(entry.from_hook, Some(false));
        }
        other => panic!("expected branch_summary entry, got {other:?}"),
    }

    // `None` entry id branches from before the first entry (upstream "root"
    // applies only when the leaf is empty); here the leaf is the first
    // summary, so fromId is that entry (upstream `fromId = leafId ?? "root"`).
    let root_summary = session
        .branch_with_summary(None, "before everything", None, None, None)
        .unwrap();
    match session.entry(&root_summary).unwrap() {
        FileEntry::BranchSummary(entry) => {
            assert_eq!(entry.from_id, summary_id.as_str());
            assert_eq!(entry.parent_id, None);
        }
        other => panic!("expected branch_summary entry, got {other:?}"),
    }

    std::fs::remove_dir_all(&dir).unwrap();
}

// ---------------------------------------------------------------------------
// SessionSummary fields (upstream SessionInfo, session-manager.ts:229-243)
// ---------------------------------------------------------------------------

#[test]
fn list_sessions_populates_name_first_message_count_and_parent() {
    let dir = temp_dir("summary-fields");
    let mut parent = SessionManager::create("C:\\proj", &dir).unwrap();
    parent.append_message(user_message("parent text")).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(20));

    let mut child = SessionManager::create("C:\\proj", &dir).unwrap();
    child
        .append_message(user_message("first user words"))
        .unwrap();
    child
        .append_message(assistant_message("answer", StopReason::Stop))
        .unwrap();
    child
        .append_message(user_message("second user message"))
        .unwrap();
    child.set_session_name(Some("my session")).unwrap();
    // Capture the child path BEFORE branching: after the branch the manager
    // points at the branched file.
    let child_path = child.path().to_path_buf();
    // Branch: the branched session's header gains parentSession = child path.
    let leaf_id = child.leaf_id().unwrap().to_string();
    let branched = child.create_branched_session(&leaf_id).unwrap().unwrap();

    let summaries = SessionManager::list_sessions(&dir, Some("C:\\proj"));
    assert_eq!(summaries.len(), 3);
    let branched_summary = summaries
        .iter()
        .find(|summary| summary.id == SessionManager::open(&branched).unwrap().id())
        .unwrap();
    assert_eq!(branched_summary.name.as_deref(), Some("my session"));
    assert_eq!(branched_summary.first_message, "first user words");
    assert_eq!(branched_summary.message_count, 3);
    assert_eq!(
        branched_summary.parent_session_path.as_deref(),
        Some(child_path.to_string_lossy().as_ref())
    );

    // A session without session_info carries no name (upstream `undefined`).
    let parent_summary = summaries
        .iter()
        .find(|summary| summary.id != branched_summary.id && summary.first_message == "parent text")
        .unwrap();
    assert_eq!(parent_summary.name, None);
    assert_eq!(parent_summary.message_count, 1);
    assert_eq!(parent_summary.parent_session_path, None);

    std::fs::remove_dir_all(&dir).unwrap();
}

// ---------------------------------------------------------------------------
// Session selector search (upstream `session-selector-search.ts`)
// ---------------------------------------------------------------------------

/// The listing collects EVERY user/assistant message as the search corpus
/// (upstream `allMessagesText`), and the search implements the upstream
/// selector semantics: case-insensitive tokens matched with AND across
/// id/name/cwd/messages, quoted phrases with whitespace normalization,
/// `re:` regex mode, tiered scoring, and no result for a miss.
#[test]
fn session_search_matches_every_message_and_upstream_semantics() {
    use pir::session_manager::search::{match_session_search, SessionSearchQuery};

    let dir = temp_dir("selector-search");
    let mut session = SessionManager::create("C:\\proj", &dir).unwrap();
    session
        .append_message(user_message("Alpha first question"))
        .unwrap();
    session
        .append_message(assistant_message(
            "reply about Beta topic",
            StopReason::Stop,
        ))
        .unwrap();
    session
        .append_message(user_message("second user Gamma"))
        .unwrap();
    session.set_session_name(Some("Release Planning")).unwrap();

    let summary = SessionManager::list_sessions(&dir, None)
        .into_iter()
        .next()
        .expect("one session");
    assert_eq!(
        summary.all_messages_text, "Alpha first question reply about Beta topic second user Gamma",
        "every user/assistant message feeds the search corpus"
    );
    assert_eq!(summary.first_message, "Alpha first question");

    let fields = summary.search_fields();
    let matched =
        |query: &str| match_session_search(&fields, &SessionSearchQuery::parse(query)).is_some();

    // Empty query matches everything with a neutral score (upstream returns
    // the unmodified list before parsing).
    assert_eq!(
        match_session_search(&fields, &SessionSearchQuery::parse("   ")),
        Some(0.0)
    );

    // Case-insensitivity and a hit on a NON-first message.
    assert!(matched("beta"));
    assert!(matched("GAMMA"));

    // Multi-word queries are AND over tokens; tokens may hit different
    // messages/fields.
    assert!(matched("alpha gamma"));
    assert!(matched("first beta gamma"));
    assert!(!matched("alpha delta"));

    // Quoted phrase: whitespace is normalized, and a phrase cannot span a
    // field boundary.
    assert!(matched("\"beta   topic\""));
    assert!(!matched("\"beta gamma\""));

    // Regex mode is case-insensitive; an invalid pattern matches nothing.
    assert!(matched("re:^alpha"));
    assert!(matched("re:planning$"));
    assert!(!matched("re:["));

    // No result.
    assert!(!matched("nonexistent"));

    // Tiered scoring: a name hit beats a message-text hit, which beats a
    // scattered fuzzy match in the name/cwd.
    let name_score = match_session_search(&fields, &SessionSearchQuery::parse("release")).unwrap();
    let text_score = match_session_search(&fields, &SessionSearchQuery::parse("beta")).unwrap();
    assert!(
        name_score < text_score,
        "name hit {name_score} must outrank text hit {text_score}"
    );

    std::fs::remove_dir_all(&dir).unwrap();
}

/// The selector's `filter_rows` consumes the same search semantics: empty
/// query = every row, multi-word AND, case-insensitive, hits on ANY message,
/// no result on a miss, and relevance order is score-first (name hits beat
/// message hits even when the message hit is newer).
#[cfg(feature = "tui")]
#[test]
fn session_selector_filter_rows_uses_upstream_search_semantics() {
    use pir::coding_agent::session_pick::{filter_rows, SessionRow, SortMode};
    use pir::session_manager::SessionSummary;

    let summary =
        |id: &str, name: Option<&str>, all_messages_text: &str, modified_ms: u64| SessionSummary {
            path: PathBuf::from(format!("C:\\sessions\\{id}.jsonl")),
            id: id.to_owned(),
            cwd: "C:\\proj".to_owned(),
            modified: std::time::UNIX_EPOCH + std::time::Duration::from_millis(modified_ms),
            name: name.map(str::to_owned),
            first_message: all_messages_text.to_owned(),
            all_messages_text: all_messages_text.to_owned(),
            message_count: 2,
            parent_session_path: None,
        };
    let rows: Vec<SessionRow> = [
        summary("id-a", Some("Release notes"), "alpha", 2_000),
        summary("id-b", None, "release alpha", 3_000),
        summary("id-c", None, "first question second answer", 1_000),
    ]
    .iter()
    .map(SessionRow::from)
    .collect();

    // Empty query returns every row, newest first for Recent.
    assert_eq!(
        filter_rows(&rows, "   ", SortMode::Recent, false),
        vec![1, 0, 2]
    );

    // Multi-word AND, case-insensitive; a token may hit a later message.
    assert_eq!(
        filter_rows(&rows, "FIRST answer", SortMode::Recent, false),
        vec![2]
    );
    assert_eq!(
        filter_rows(&rows, "release ALPHA", SortMode::Recent, false),
        vec![1, 0]
    );

    // No result.
    assert!(filter_rows(&rows, "definitely-absent", SortMode::Recent, false).is_empty());

    // Relevance: the name hit (score tier 0) outranks the message-text hit
    // (tier 1) even though the message hit is newer.
    assert_eq!(
        filter_rows(&rows, "release", SortMode::Relevance, false),
        vec![0, 1]
    );

    // Named-only still composes with the query.
    assert_eq!(
        filter_rows(&rows, "alpha", SortMode::Relevance, true),
        vec![0]
    );
}

// ---------------------------------------------------------------------------
// Background tool results: resume/replay keeps BOTH entries
// (TOOL-BACKGROUND-SCOPE.md §2.4.1 / §7, foreman decision 2026-10-15)
// ---------------------------------------------------------------------------

/// RAII cleanup for one of the suite's [`temp_dir`] paths: the directory is
/// removed on drop, including when an assertion panics mid-test.
struct TempDirGuard(PathBuf);

impl Drop for TempDirGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn guard_temp_dir(name: &str) -> TempDirGuard {
    TempDirGuard(temp_dir(name))
}

/// Provider call id of the backgrounded call in these fixtures.
const BACKGROUND_CALL_ID: &str = "call_bg_1";
/// Tool name of the backgrounded call.
const BACKGROUND_TOOL_NAME: &str = "bash";

/// The placeholder text the batch answers an adopted background call with
/// (`src/agent/tool_scheduler/execute.rs::background_placeholder`).
fn background_placeholder_text() -> String {
    format!(
        "Tool '{BACKGROUND_TOOL_NAME}' is running in the background (callId: {BACKGROUND_CALL_ID}). Its result will arrive as a separate message; do not wait for it."
    )
}

/// The framed model-facing completion text
/// (`src/agent_session/background.rs::background_result_frame`, §2.4.6).
fn background_result_frame() -> String {
    format!(
        "<background-tool-result tool=\"{BACKGROUND_TOOL_NAME}\" callId=\"{BACKGROUND_CALL_ID}\" status=\"ok\" durationMs=\"42\">\nstored output\n</background-tool-result>"
    )
}

/// The `details` object `background.rs::finish` persists with the custom entry
/// (§2.4.1): call metadata plus the tool result's own details.
fn background_result_details() -> serde_json::Value {
    serde_json::json!({
        "callId": BACKGROUND_CALL_ID,
        "toolName": BACKGROUND_TOOL_NAME,
        "status": "ok",
        "durationMs": 42,
        "details": {"exitCode": 0, "command": "echo hi"},
    })
}

fn background_tool_call_assistant() -> Message {
    Message::Assistant(AssistantMessage {
        content: vec![ContentBlock::ToolCall(ToolCall {
            id: BACKGROUND_CALL_ID.to_owned(),
            name: BACKGROUND_TOOL_NAME.to_owned(),
            arguments: [("command".to_owned(), serde_json::json!("sleep 5"))]
                .into_iter()
                .collect(),
            thought_signature: None,
            namespace: None,
            arguments_error: None,
        })],
        api: "anthropic-messages".into(),
        provider: "anthropic".into(),
        model: "claude-bg".into(),
        stop_reason: StopReason::ToolUse,
        timestamp: pir::ai::util::now_ms(),
        ..Default::default()
    })
}

fn background_placeholder_result_message() -> Message {
    Message::ToolResult(ToolResultMessage {
        role: ToolResultRole::ToolResult,
        tool_call_id: BACKGROUND_CALL_ID.to_owned(),
        tool_name: BACKGROUND_TOOL_NAME.to_owned(),
        content: vec![ContentBlock::Text(TextContent {
            text: background_placeholder_text(),
            text_signature: None,
        })],
        details: Some(serde_json::json!({
            "background": true,
            "callId": BACKGROUND_CALL_ID,
            "toolName": BACKGROUND_TOOL_NAME,
        })),
        usage: None,
        added_tool_names: Vec::new(),
        structured_content: None,
        nested_calls: None,
        is_error: false,
        duration_ms: None,
        timestamp: pir::ai::util::now_ms(),
    })
}

/// Append the four-entry fixture the resume pin works on: user, assistant
/// (tool_use = the backgrounded call), the placeholder tool_result the batch
/// answered with, and the later `backgroundToolResult` custom message.
fn append_background_exchange(session: &mut SessionManager) -> (String, String, String, String) {
    let user_id = session
        .append_message(user_message("run it in the background"))
        .unwrap();
    let assistant_id = session
        .append_message(background_tool_call_assistant())
        .unwrap();
    let result_id = session
        .append_message(background_placeholder_result_message())
        .unwrap();
    let custom_id = session
        .append_custom_message_entry(
            "backgroundToolResult",
            serde_json::Value::String(background_result_frame()),
            false,
            Some(background_result_details()),
        )
        .unwrap();
    (user_id, assistant_id, result_id, custom_id)
}

/// First text block of any message role (`text_of` covers only
/// user/assistant).
fn first_text_of(message: &Message) -> String {
    let content = match message {
        Message::User(inner) => &inner.content,
        Message::System(inner) => &inner.content,
        Message::Developer(inner) => &inner.content,
        Message::Assistant(inner) => &inner.content,
        Message::ToolResult(inner) => &inner.content,
    };
    content
        .iter()
        .find_map(|block| match block {
            ContentBlock::Text(text) => Some(text.text.clone()),
            _ => None,
        })
        .unwrap_or_default()
}

/// A model matching [`background_tool_call_assistant`], so the replay
/// transform takes its same-model path.
fn background_test_model() -> Model {
    Model {
        id: "claude-bg".into(),
        name: "claude-bg".into(),
        api: "anthropic-messages".into(),
        provider: "anthropic".into(),
        base_url: "https://example.test".into(),
        reasoning: false,
        thinking_level_map: None,
        input: vec!["text".to_owned()],
        cost: ModelCost {
            rates: ModelCostRates {
                input: 1.0,
                output: 2.0,
                cache_read: 0.1,
                cache_write: 0.2,
            },
            tiers: Vec::new(),
        },
        context_window: 128_000,
        max_tokens: 16_384,
        sampling_params: None,
        headers: None,
        thinking_token_budget_field: None,
        input_limits: None,
        prompt_cache: None,
        sampling_params_by_thinking_level: None,
        model_type: None,
        compat: None,
    }
}

/// Round-trip pin for the persisted `backgroundToolResult` entry
/// (TOOL-BACKGROUND-SCOPE.md §2.4.1): `[user, assistant(tool_use),
/// tool_result(placeholder), custom_message]` saves and reloads through the
/// real `SessionManager` with the exact on-disk shape — entry order, parent
/// chain, `customType`, the explicit `display: false` flag, the framed content
/// bytes, and the whole `details` object all survive.
#[test]
fn background_tool_result_round_trips_with_exact_on_disk_shape() {
    let dir = guard_temp_dir("background-round-trip");
    let mut session = SessionManager::create("C:/repo", &dir.0).unwrap();
    let (user_id, assistant_id, result_id, custom_id) = append_background_exchange(&mut session);

    let placeholder = background_placeholder_text();
    let framed = background_result_frame();
    let details = background_result_details();

    let lines = lines_of(session.path());
    assert_eq!(lines.len(), 5, "header + 4 entries");

    // The assistant entry carries the provider tool_use of the detached call.
    let assistant_line = &lines[2];
    assert_eq!(assistant_line["id"], assistant_id.as_str());
    assert_eq!(assistant_line["parentId"], user_id.as_str());
    assert_eq!(assistant_line["message"]["role"], "assistant");
    assert_eq!(assistant_line["message"]["stopReason"], "toolUse");
    assert_eq!(assistant_line["message"]["content"][0]["type"], "toolCall");
    assert_eq!(
        assistant_line["message"]["content"][0]["id"],
        BACKGROUND_CALL_ID
    );
    assert_eq!(
        assistant_line["message"]["content"][0]["name"],
        BACKGROUND_TOOL_NAME
    );

    // The placeholder tool_result answers that same call id.
    let result_line = &lines[3];
    assert_eq!(result_line["id"], result_id.as_str());
    assert_eq!(result_line["parentId"], assistant_id.as_str());
    assert_eq!(result_line["message"]["role"], "toolResult");
    assert_eq!(result_line["message"]["toolCallId"], BACKGROUND_CALL_ID);
    assert_eq!(result_line["message"]["toolName"], BACKGROUND_TOOL_NAME);
    assert_eq!(result_line["message"]["isError"], false);
    assert_eq!(result_line["message"]["content"][0]["type"], "text");
    assert_eq!(
        result_line["message"]["content"][0]["text"],
        placeholder.as_str()
    );
    assert_eq!(result_line["message"]["details"]["background"], true);
    assert_eq!(
        result_line["message"]["details"]["callId"],
        BACKGROUND_CALL_ID
    );

    // The custom entry: upstream line shape and upstream construction order
    // (`appendCustomMessageEntry`, session-manager.ts:1341-1351).
    let custom_line = &lines[4];
    assert_eq!(custom_line["type"], "custom_message");
    assert_eq!(custom_line["id"], custom_id.as_str());
    assert_eq!(custom_line["parentId"], result_id.as_str());
    assert_eq!(custom_line["customType"], "backgroundToolResult");
    assert_eq!(
        custom_line["content"],
        serde_json::Value::String(framed.clone())
    );
    assert_eq!(
        custom_line["display"], false,
        "display:false must be written, never omitted"
    );
    assert_eq!(custom_line["details"], details);
    let keys: Vec<&str> = custom_line
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(
        keys,
        [
            "type",
            "customType",
            "content",
            "display",
            "details",
            "id",
            "parentId",
            "timestamp"
        ]
    );

    // Reload through the real manager: order, parent chain and every field
    // survive.
    let mut reopened = SessionManager::open(session.path()).unwrap();
    let entries = reopened.entries();
    assert_eq!(entries.len(), 4);
    assert_eq!(entries[0].id(), Some(user_id.as_str()));
    assert_eq!(entries[1].id(), Some(assistant_id.as_str()));
    assert_eq!(entries[2].id(), Some(result_id.as_str()));
    assert_eq!(entries[3].id(), Some(custom_id.as_str()));
    assert_eq!(entries[1].parent_id(), Some(user_id.as_str()));
    assert_eq!(entries[2].parent_id(), Some(assistant_id.as_str()));
    assert_eq!(entries[3].parent_id(), Some(result_id.as_str()));
    assert_eq!(reopened.leaf_id(), Some(custom_id.as_str()));

    match &entries[1] {
        FileEntry::Message(MessageEntry {
            message: StoredMessage::Assistant(assistant),
            ..
        }) => {
            let call = assistant
                .content
                .iter()
                .find_map(|block| match block {
                    ContentBlock::ToolCall(call) => Some(call),
                    _ => None,
                })
                .expect("assistant tool_use survives the reload");
            assert_eq!(call.id, BACKGROUND_CALL_ID);
            assert_eq!(assistant.stop_reason, StopReason::ToolUse);
        }
        other => panic!("expected assistant message entry, got {other:?}"),
    }
    match &entries[2] {
        FileEntry::Message(MessageEntry {
            message: StoredMessage::ToolResult(result),
            ..
        }) => {
            assert_eq!(result.tool_call_id, BACKGROUND_CALL_ID);
            assert_eq!(result.tool_name, BACKGROUND_TOOL_NAME);
            assert!(!result.is_error);
            assert_eq!(
                result.content,
                vec![ContentBlock::Text(TextContent {
                    text: placeholder,
                    text_signature: None,
                })]
            );
            assert_eq!(result.details.as_ref().unwrap()["background"], true);
        }
        other => panic!("expected tool_result message entry, got {other:?}"),
    }
    match &entries[3] {
        FileEntry::CustomMessage(entry) => {
            assert_eq!(entry.custom_type, "backgroundToolResult");
            assert!(!entry.display);
            assert_eq!(entry.content, serde_json::Value::String(framed));
            assert_eq!(entry.details.as_ref(), Some(&details));
        }
        other => panic!("expected custom_message entry, got {other:?}"),
    }

    // A resumed turn appends from the custom entry (the reloaded leaf), so
    // the background result stays on the continued branch.
    let next = reopened
        .append_message(user_message("continue from here"))
        .unwrap();
    assert_eq!(
        reopened.entry(&next).unwrap().parent_id(),
        Some(custom_id.as_str())
    );
}

/// The foreman decision, pinned (2026-10-15): a resumed session that saw a
/// background call keeps BOTH entries — the placeholder tool_result the model
/// originally received AND the later `backgroundToolResult` custom message.
/// Nothing is suppressed; the custom message is honest history, exactly like
/// the `bashExecution` custom-message precedent
/// (`AgentSession::record_bash_result`).
///
/// The projection asserted here is the transcript `pir resume` / the TUI
/// consumes (`AgentSession::transcript` → `SessionManager::
/// build_session_projection`). `display: false` is a UI hint only, never a
/// context gate (upstream `sessionEntryToContextMessages` returns custom
/// messages unconditionally).
#[test]
fn resumed_projection_keeps_placeholder_and_background_result_both() {
    let dir = guard_temp_dir("background-projection");
    let mut session = SessionManager::create("C:/repo", &dir.0).unwrap();
    let (_user_id, _assistant_id, _result_id, custom_id) = append_background_exchange(&mut session);
    let path = session.path().to_path_buf();

    let reopened = SessionManager::open(&path).unwrap();
    let projection = reopened.build_session_projection().unwrap();

    // Four contributions, append order preserved: user, assistant(tool_use),
    // placeholder tool_result, then the custom result rendered as a user-role
    // text message (the same lowering `convertToLlm` applies upstream).
    assert_eq!(projection.messages.len(), 4, "{:?}", projection.messages);
    assert!(matches!(projection.messages[0], Message::User(_)));
    let assistant = match &projection.messages[1] {
        Message::Assistant(assistant) => assistant,
        other => panic!("expected assistant, got {other:?}"),
    };
    assert!(assistant.content.iter().any(|block| matches!(
        block,
        ContentBlock::ToolCall(call) if call.id == BACKGROUND_CALL_ID
    )));
    let result = match &projection.messages[2] {
        Message::ToolResult(result) => result,
        other => panic!("expected toolResult, got {other:?}"),
    };
    assert_eq!(result.tool_call_id, BACKGROUND_CALL_ID);
    assert_eq!(
        first_text_of(&projection.messages[2]),
        background_placeholder_text(),
        "the placeholder the model already saw is NOT suppressed"
    );
    assert_eq!(result.details.as_ref().unwrap()["background"], true);
    assert_eq!(
        first_text_of(&projection.messages[3]),
        background_result_frame(),
        "the later custom message is surfaced too"
    );

    // Source entries and per-entry contributions keep the append order; the
    // display:false custom entry still contributes exactly one message.
    assert!(matches!(
        *projection.entries[1].source_entry,
        FileEntry::Message(_)
    ));
    assert!(matches!(
        *projection.entries[2].source_entry,
        FileEntry::Message(_)
    ));
    match projection.entries[3].source_entry.as_ref() {
        FileEntry::CustomMessage(entry) => {
            assert_eq!(entry.custom_type, "backgroundToolResult");
            assert!(!entry.display, "display:false is a UI hint");
        }
        other => panic!("expected custom_message source entry, got {other:?}"),
    }
    assert_eq!(
        projection.entries[3].message_count(),
        1,
        "display:false still contributes context"
    );
    assert_eq!(reopened.leaf_id(), Some(custom_id.as_str()));

    // Both public context entry points agree (they share the projection).
    assert_eq!(
        reopened.build_context_entries().unwrap(),
        projection.messages
    );
}

/// Provider pairing is the load-bearing constraint
/// (TOOL-BACKGROUND-SCOPE.md §7, `src/ai/api/transform.rs`): the placeholder
/// is the ONLY tool_result for the backgrounded call, before and after the
/// provider replay transform — no duplicate, and no synthesized
/// `No result provided` — while the custom message rides along as an ordinary
/// context message.
#[test]
fn resumed_background_transcript_pairs_the_placeholder_exactly_once() {
    let dir = guard_temp_dir("background-pairing");
    let mut session = SessionManager::create("C:/repo", &dir.0).unwrap();
    append_background_exchange(&mut session);
    let path = session.path().to_path_buf();

    let reopened = SessionManager::open(&path).unwrap();
    let projection = reopened.build_session_projection().unwrap();

    let all_results = projection
        .messages
        .iter()
        .filter(|message| matches!(message, Message::ToolResult(_)))
        .count();
    assert_eq!(
        all_results, 1,
        "one tool_result total: {:?}",
        projection.messages
    );
    assert_eq!(
        first_text_of(&projection.messages[2]),
        background_placeholder_text()
    );

    // Replay through the real provider transform: it synthesizes tool results
    // only for ORPHANED tool_use blocks, so the placeholder must come out
    // unchanged (and alone).
    let model = background_test_model();
    let transformed =
        pir::ai::api::transform::transform_messages(&projection.messages, &model, None);
    let paired = transformed
        .iter()
        .filter_map(|message| match message {
            Message::ToolResult(result) if result.tool_call_id == BACKGROUND_CALL_ID => {
                Some(result)
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(paired.len(), 1, "exactly one tool_result after replay");
    assert_eq!(
        first_text_of(&Message::ToolResult(paired[0].clone())),
        background_placeholder_text(),
        "the replayed result is the placeholder, not a synthetic one"
    );
    assert!(
        !transformed
            .iter()
            .any(|message| first_text_of(message).contains("No result provided")),
        "no orphan synthesis in the replayed transcript"
    );
    // No reordering: the assistant tool_use is immediately followed by its
    // result, and the custom message survives as the last context message.
    let assistant_index = transformed
        .iter()
        .position(|message| matches!(message, Message::Assistant(_)))
        .unwrap();
    assert!(matches!(
        transformed[assistant_index + 1],
        Message::ToolResult(_)
    ));
    assert_eq!(
        first_text_of(transformed.last().unwrap()),
        background_result_frame()
    );
}

/// INVARIANT (borrowed projection): `SessionManager::build_session_projection`
/// now walks the live `Arc<FileEntry>` list (header included) instead of a
/// deep-cloned [`SessionManager::entries`] snapshot. The two shapes must
/// produce the IDENTICAL projection — same retained entry list, same messages,
/// same settings — or the shared walk would silently change what the model
/// sees. A compaction (retained range + stale-compaction skip) and a
/// `context_edit` on a retained entry are both in the fixture.
#[test]
fn manager_projection_matches_the_owned_entry_view() {
    let mut session = SessionManager::in_memory("C:/repo", None).unwrap();
    let u1 = session.append_message(user_message("q1")).unwrap();
    session
        .append_message(assistant_message("a1", StopReason::Stop))
        .unwrap();
    let u2 = session.append_message(user_message("q2")).unwrap();
    session
        .append_message(assistant_message("a2", StopReason::Stop))
        .unwrap();
    session
        .append_compaction("earlier summarized", Some(&u2), 999, None)
        .unwrap();
    let q3 = session.append_message(user_message("q3")).unwrap();
    session
        .append_context_edit(&q3, Some(serde_json::json!("edited q3")))
        .unwrap();
    session.append_thinking_level_change("high").unwrap();
    // An edit on an entry the compaction did NOT retain: it must stay out of
    // the projection in both shapes.
    session
        .append_context_edit(&u1, Some(serde_json::json!("edited q1")))
        .unwrap();

    let borrowed = session.build_session_projection().unwrap();
    let owned = pir::session_manager::projection::build_session_projection(
        &session.entries(),
        session.leaf_id(),
    );
    assert_eq!(
        borrowed, owned,
        "the Arc-walk projection must equal the owned-entries projection"
    );
    assert_eq!(borrowed.messages, session.build_context_entries().unwrap());
    assert_eq!(borrowed.thinking_level, "high");
    // The fixture's kept range is (q2, a2) plus the edit-applied q3.
    let texts: Vec<String> = borrowed.messages.iter().map(text_of).collect();
    assert_eq!(
        texts,
        vec![
            "The conversation history before this point was compacted into the following summary:\n\n<summary>\nearlier summarized\n</summary>".to_owned(),
            "q2".to_owned(),
            "a2".to_owned(),
            "edited q3".to_owned(),
        ],
        "the borrowed walk keeps the exact retained range: {texts:?}"
    );
}

/// INVARIANT (header-transparent path walk): `build_session_path` is generic
/// over the entry container, so the manager can pass its raw `Arc<FileEntry>`
/// list (header first) where callers used to pass the header-filtered
/// `entries()` view. Both must walk identical paths — including the
/// stale-leaf fallback, which picks the last NON-header entry — and a
/// header-only list must still yield an empty path.
#[test]
fn path_walk_is_header_transparent_for_both_containers() {
    let mut session = SessionManager::in_memory("C:/repo", None).unwrap();
    session.append_message(user_message("only")).unwrap();
    let owned = session.entries();
    let header = session.header().expect("session header").clone();
    let mut arcs: Vec<std::sync::Arc<FileEntry>> =
        vec![std::sync::Arc::new(FileEntry::Session(header.clone()))];
    arcs.extend(owned.iter().cloned().map(std::sync::Arc::new));

    for leaf in [session.leaf_id(), Some("missing-entry-id")] {
        let via_arcs = pir::session_manager::projection::build_session_path(&arcs, leaf);
        let via_owned = pir::session_manager::projection::build_session_path(&owned, leaf);
        let arc_ids: Vec<&str> = via_arcs.iter().map(|entry| entry.entry_id()).collect();
        let owned_ids: Vec<&str> = via_owned.iter().map(|entry| entry.entry_id()).collect();
        assert_eq!(
            arc_ids, owned_ids,
            "leaf {leaf:?}: the Arc walk (header included) must match the owned view"
        );
        assert_eq!(
            arc_ids.len(),
            1,
            "leaf {leaf:?}: the single stored entry is always on the path"
        );
    }

    // Header-only: no id, no non-header fallback candidate, empty path.
    let header_only = vec![std::sync::Arc::new(FileEntry::Session(header))];
    assert!(pir::session_manager::projection::build_session_path(
        &header_only,
        Some("missing-entry-id")
    )
    .is_empty());
}

/// INVARIANT (single-copy branch walk): `branch_entries` returns exactly what
/// the old whole-snapshot walk returned — the leaf→root path, oldest first —
/// while cloning only the path members instead of the entire session.
#[test]
fn branch_entries_walks_the_live_arc_list() {
    let mut session = SessionManager::in_memory("C:/repo", None).unwrap();
    let u1 = session.append_message(user_message("q1")).unwrap();
    session
        .append_message(assistant_message("a1", StopReason::Stop))
        .unwrap();
    let u2 = session.append_message(user_message("q2")).unwrap();

    let branch = session.branch_entries(None).unwrap();
    let ids: Vec<&str> = branch.iter().map(|entry| entry.entry_id()).collect();
    // The leaf IS the tip: the path is every stored entry, oldest first — the
    // same list the old whole-snapshot walk produced.
    let expected: Vec<String> = session
        .entries()
        .iter()
        .map(|entry| entry.entry_id().to_owned())
        .collect();
    assert_eq!(
        ids.len(),
        expected.len(),
        "the tip-leaf path covers every entry"
    );
    for (id, expected_id) in ids.iter().zip(expected.iter()) {
        assert_eq!(id, expected_id);
    }
    assert_eq!(ids.first().copied(), Some(u1.as_str()));
    assert_eq!(ids.last().copied(), Some(u2.as_str()));
    // Explicit from_id: the path stops at that entry (no descendants).
    let partial = session.branch_entries(Some(&u2)).unwrap();
    assert_eq!(partial.len(), 3);
    assert_eq!(partial.last().unwrap().entry_id(), u2);
    assert_eq!(
        branch.last().unwrap().entry_id(),
        session.leaf_id().unwrap()
    );
}

// ---------------------------------------------------------------------------
// Branching a PAGED manager
// ---------------------------------------------------------------------------

/// `create_branched_session` copies the leaf path — on a paged manager
/// `branch_entries` walks the RESIDENT WINDOW only, so the "path" is a
/// truncated transcript (the window's oldest parent is in the skipped
/// prefix). The call must be refused BEFORE it replaces
/// entries/id/session_file; the old code bailed at the final `rewrite_file`
/// AFTER the mutation and left the manager pointing at a never-written file.
#[test]
fn branched_session_on_a_paged_manager_is_refused_before_any_state_changes() {
    let dir = temp_dir("paged-branch");
    let mut source = SessionManager::create("C:/repo", &dir).unwrap();
    let path = source.path().to_path_buf();
    for index in 0..80 {
        source
            .append_message(user_message(&format!(
                "question {index}: {}",
                "x".repeat(600)
            )))
            .unwrap();
        source
            .append_message(assistant_message("answer", StopReason::Stop))
            .unwrap();
    }
    drop(source);

    let mut paged = SessionManager::open_paged(&path, 8 * 1024, 16).expect("paged open");
    assert!(paged.is_paged(), "the fixture must exceed the page caps");
    let leaf_id = paged.leaf_id().expect("leaf").to_owned();
    // The window holds conversation, so the OLD flow reached the rewrite
    // guard — but only after replacing the manager's state.
    assert!(paged
        .entries()
        .iter()
        .any(|entry| matches!(entry, FileEntry::Message(_))));

    let before_id = paged.id().to_owned();
    let before_file = paged.session_file().map(std::path::Path::to_path_buf);
    let before_entries = paged.entries();
    let before_leaf = paged.leaf_id().map(str::to_owned);
    let before_window = paged.paged_window();

    let error = paged
        .create_branched_session(&leaf_id)
        .expect_err("a paged manager must refuse a branch rewrite");
    assert!(
        error.to_string().contains("tail page"),
        "the refusal names the paged mode: {error}"
    );

    assert_eq!(paged.id(), before_id, "the session id must be untouched");
    assert_eq!(
        paged.session_file(),
        before_file.as_deref(),
        "the session file must be untouched (never point at a never-written path)"
    );
    assert_eq!(
        paged.entries(),
        before_entries,
        "the entries must be untouched (no truncated transcript)"
    );
    assert_eq!(paged.leaf_id(), before_leaf.as_deref());
    assert_eq!(paged.paged_window(), before_window);
    assert!(paged.is_paged(), "the manager stays a tail page");
    std::fs::remove_dir_all(&dir).ok();
}

/// The silent case the guard could not catch: a branch at a
/// label/session_info id whose truncated path holds no user/assistant
/// message. The old flow returned `Ok(new_path)` and switched the manager to
/// the truncated transcript; it must be refused like every other paged
/// rewrite.
#[test]
fn branched_session_on_a_paged_manager_without_conversation_is_refused_too() {
    let dir = temp_dir("paged-branch-noconv");
    let mut source = SessionManager::create("C:/repo", &dir).unwrap();
    let path = source.path().to_path_buf();
    for index in 0..40 {
        source
            .append_message(user_message(&format!(
                "question {index}: {}",
                "x".repeat(600)
            )))
            .unwrap();
        source
            .append_message(assistant_message("answer", StopReason::Stop))
            .unwrap();
    }
    // Push every message out of a 16-entry tail window: the resident window
    // is a chain of session_info entries, so a branch at its tip has no
    // conversation and used to skip the rewrite guard entirely.
    for index in 0..20 {
        let name = format!("name {index}");
        source.set_session_name(Some(name.as_str())).unwrap();
    }
    drop(source);

    let mut paged = SessionManager::open_paged(&path, 8 * 1024, 16).expect("paged open");
    assert!(paged.is_paged(), "the fixture must exceed the page caps");
    assert!(
        !paged
            .entries()
            .iter()
            .any(|entry| matches!(entry, FileEntry::Message(_))),
        "the resident window must hold no conversation"
    );
    let leaf_id = paged.leaf_id().expect("leaf").to_owned();

    let before_id = paged.id().to_owned();
    let before_file = paged.session_file().map(std::path::Path::to_path_buf);
    let before_entries = paged.entries();
    let before_leaf = paged.leaf_id().map(str::to_owned);
    let before_window = paged.paged_window();

    let error = paged
        .create_branched_session(&leaf_id)
        .expect_err("a paged manager must refuse a branch rewrite without conversation too");
    assert!(
        error.to_string().contains("tail page"),
        "the refusal names the paged mode: {error}"
    );

    assert_eq!(paged.id(), before_id, "the session id must be untouched");
    assert_eq!(paged.session_file(), before_file.as_deref());
    assert_eq!(paged.entries(), before_entries);
    assert_eq!(paged.leaf_id(), before_leaf.as_deref());
    assert_eq!(paged.paged_window(), before_window);
    assert!(paged.is_paged());
    std::fs::remove_dir_all(&dir).ok();
}

// ---------------------------------------------------------------------------
// `bashExecution` message entries (upstream core/messages.ts:29-44)
// ---------------------------------------------------------------------------

#[test]
fn bash_execution_entries_round_trip_as_upstream_role() {
    let dir = temp_dir("bash-execution");
    let mut session = SessionManager::create("C:/repo", &dir).unwrap();
    session.append_message(user_message("hi")).unwrap();
    let included_id = session
        .append_bash_execution(
            "echo hi",
            "hi\n",
            Some(0),
            false,
            false,
            None,
            false,
            1_700_000_000_000,
        )
        .unwrap();
    let excluded_id = session
        .append_bash_execution(
            "echo secret",
            "secret",
            Some(0),
            false,
            false,
            None,
            true,
            1_700_000_000_001,
        )
        .unwrap();

    // Wire shape: a normal message entry whose message role is bashExecution
    // (upstream `recordBashResult` -> `appendMessage`).
    let lines = lines_of(session.path());
    let included = lines
        .iter()
        .find(|line| {
            line["type"] == "message"
                && line["message"]["role"] == "bashExecution"
                && line["message"]["command"] == "echo hi"
        })
        .expect("bashExecution message line");
    assert_eq!(included["message"]["output"], "hi\n");
    assert_eq!(included["message"]["exitCode"], 0);
    assert_eq!(included["message"]["cancelled"], false);
    assert_eq!(included["message"]["truncated"], false);
    assert!(
        included["message"].get("excludeFromContext").is_none(),
        "false is omitted from the wire like upstream's undefined"
    );
    let excluded = lines
        .iter()
        .find(|line| {
            line["type"] == "message"
                && line["message"]["role"] == "bashExecution"
                && line["message"]["command"] == "echo secret"
        })
        .expect("excluded bashExecution line");
    assert_eq!(excluded["message"]["excludeFromContext"], true);

    // A reopened manager parses the entry as the typed variant (it must not
    // land in `unparsed_raw_lines`), and the projection renders the upstream
    // `bashExecutionToText` user text.
    let reopened = SessionManager::open(session.path()).unwrap();
    let entries = reopened.entries();
    let entry = entries
        .iter()
        .find(|entry| entry.id() == Some(included_id.as_str()))
        .expect("included entry");
    let FileEntry::Message(message) = entry else {
        panic!("expected a message entry");
    };
    let StoredMessage::BashExecution(bash) = &message.message else {
        panic!("expected a stored bashExecution message");
    };
    assert_eq!(bash.command, "echo hi");
    let projected = pir::session_manager::projection::session_entry_to_context_messages(entry);
    assert_eq!(projected.len(), 1);
    let Message::User(user) = &projected[0] else {
        panic!("bashExecution projects as a user message");
    };
    let ContentBlock::Text(text) = &user.content[0] else {
        panic!("text block");
    };
    // Upstream `bashExecutionToText` keeps the captured output verbatim, so
    // the trailing newline stays inside the fence (a blank line before the
    // closing fence).
    assert_eq!(text.text, "Ran `echo hi`\n```\nhi\n\n```");

    // `!!` records stay in the transcript but are skipped by the model
    // context (upstream `convertToLlm` honors `excludeFromContext`).
    let excluded_entry = entries
        .iter()
        .find(|entry| entry.id() == Some(excluded_id.as_str()))
        .expect("excluded entry");
    assert!(
        pir::session_manager::projection::session_entry_to_context_messages(excluded_entry)
            .is_empty()
    );
    assert_eq!(
        pir::session_manager::projection::projected_message_count(excluded_entry, None),
        0,
        "the metadata twin must agree with the projection"
    );

    std::fs::remove_dir_all(&dir).ok();
}

// ---------------------------------------------------------------------------
// Listing progress / cancellation (upstream
// `listSessions({ onProgress, signal })`, session-manager.ts:912-973)
// ---------------------------------------------------------------------------

fn seed_listed_sessions(dir: &std::path::Path, count: usize) {
    for index in 0..count {
        let mut session = SessionManager::create("C:\\repo", dir).unwrap();
        session
            .append_message(user_message(&format!("listed session {index}")))
            .unwrap();
    }
}

#[test]
fn list_sessions_reports_progress_and_keeps_the_old_api() {
    let dir = temp_dir("list-progress");
    seed_listed_sessions(&dir, 3);

    let mut calls: Vec<(usize, usize, Option<Vec<String>>)> = Vec::new();
    let mut progress =
        |loaded: usize, total: usize, partial: Option<&[pir::session_manager::SessionSummary]>| {
            calls.push((
                loaded,
                total,
                partial.map(|sessions| sessions.iter().map(|session| session.id.clone()).collect()),
            ));
        };
    let outcome = SessionManager::list_sessions_cancellable(&dir, None, Some(&mut progress), None);
    drop(progress);

    assert!(!outcome.cancelled, "no cancel signal means a full scan");
    assert_eq!(outcome.sessions.len(), 3);
    assert_eq!(calls.len(), 3, "one progress call per candidate file");
    assert_eq!((calls[0].0, calls[0].1), (1, 3));
    assert!(
        calls[0].2.as_ref().is_some_and(|ids| ids.len() == 1),
        "the first file publishes a partial listing"
    );
    assert!(
        calls[1].2.is_none(),
        "intermediate files only advance the counters"
    );
    assert!(
        calls[2].2.as_ref().is_some_and(|ids| ids.len() == 3),
        "the last file publishes the complete listing"
    );
    assert_eq!((calls[2].0, calls[2].1), (3, 3));

    // The two-argument API is the same scan without callbacks.
    let plain = SessionManager::list_sessions(&dir, None);
    assert_eq!(plain.len(), 3);
    let mut sorted_ids: Vec<String> = plain.iter().map(|session| session.id.clone()).collect();
    sorted_ids.sort();
    let mut outcome_ids: Vec<String> = outcome
        .sessions
        .iter()
        .map(|session| session.id.clone())
        .collect();
    outcome_ids.sort();
    assert_eq!(sorted_ids, outcome_ids);

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn list_sessions_cancellation_returns_the_partial_list() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    let dir = temp_dir("list-cancel");
    seed_listed_sessions(&dir, 3);

    let processed = AtomicUsize::new(0);
    let mut progress = |_: usize, _: usize, _: Option<&[pir::session_manager::SessionSummary]>| {
        processed.fetch_add(1, Ordering::SeqCst);
    };
    // Cancelled between the first and second file: the scan stops with the
    // first loaded session instead of discarding everything.
    let cancel = || processed.load(Ordering::SeqCst) >= 1;
    let outcome =
        SessionManager::list_sessions_cancellable(&dir, None, Some(&mut progress), Some(&cancel));

    assert!(outcome.cancelled, "the cancel signal must be reported");
    assert_eq!(outcome.sessions.len(), 1);
    assert_eq!(processed.load(Ordering::SeqCst), 1);

    // Cancelled before the first file: an empty partial result, still flagged.
    let cancel_immediately = || true;
    let mut never = |_: usize, _: usize, _: Option<&[pir::session_manager::SessionSummary]>| {
        panic!("progress must not run for a cancelled scan")
    };
    let outcome = SessionManager::list_sessions_cancellable(
        &dir,
        None,
        Some(&mut never),
        Some(&cancel_immediately),
    );
    assert!(outcome.cancelled);
    assert!(outcome.sessions.is_empty());

    std::fs::remove_dir_all(&dir).ok();
}

// ---------------------------------------------------------------------------
// Off-thread session appends (blocking file I/O off the async runtime)
// ---------------------------------------------------------------------------

/// The user-message texts of a persisted session file, in file order.
fn session_user_texts(path: &std::path::Path) -> Vec<String> {
    std::fs::read_to_string(path)
        .expect("session file is readable")
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .filter(|entry| entry.get("type").and_then(serde_json::Value::as_str) == Some("message"))
        .filter_map(|entry| {
            entry
                .get("message")?
                .get("content")?
                .get(0)?
                .get("text")?
                .as_str()
                .map(str::to_owned)
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn off_thread_appends_are_serialized_and_keep_per_task_order() {
    const TASKS: usize = 6;
    const PER_TASK: usize = 20;
    let dir = temp_dir("off-thread-order");
    let session = SessionManager::create("C:/repo", &dir).unwrap();
    let path = session.path().to_path_buf();
    let session = Arc::new(tokio::sync::Mutex::new(session));

    let mut handles = Vec::new();
    for task in 0..TASKS {
        let session = Arc::clone(&session);
        handles.push(tokio::spawn(async move {
            let messages = (0..PER_TASK)
                .map(|index| user_message(&format!("task-{task}-{index}")))
                .collect::<Vec<_>>();
            SessionManager::append_messages_off_thread(&session, messages).await
        }));
    }
    for handle in handles {
        let ids = handle
            .await
            .expect("append task joins")
            .expect("append batch succeeds");
        assert_eq!(ids.len(), PER_TASK);
    }

    let texts = session_user_texts(&path);
    assert_eq!(texts.len(), TASKS * PER_TASK, "every append is persisted");
    for task in 0..TASKS {
        let prefix = format!("task-{task}-");
        let ordered: Vec<usize> = texts
            .iter()
            .filter_map(|text| text.strip_prefix(&prefix))
            .map(|index| index.parse().expect("index suffix"))
            .collect();
        assert_eq!(
            ordered,
            (0..PER_TASK).collect::<Vec<_>>(),
            "task {task}'s appends keep their order"
        );
    }

    // The mutex also serialized the in-memory index: one branch holding every
    // appended message, with no lost or duplicated leaf.
    let guard = session.lock().await;
    assert_eq!(guard.branch_entries(None).unwrap().len(), TASKS * PER_TASK);

    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn off_thread_append_failure_rolls_back_and_keeps_the_leaf() {
    let dir = temp_dir("off-thread-failure");
    let mut session = SessionManager::create("C:/repo", &dir).unwrap();
    let first_id = session.append_message(user_message("first")).unwrap();
    let path = session.path().to_path_buf();
    let session = Arc::new(tokio::sync::Mutex::new(session));
    let before = std::fs::read(&path).unwrap();

    {
        let mut guard = session.lock().await;
        guard.fail_next_append_for_test(AppendFailure::PartialWrite(10));
    }
    let error = SessionManager::append_message_off_thread(&session, user_message("second"))
        .await
        .expect_err("the injected failure propagates through the blocking task");
    assert!(
        error
            .to_string()
            .contains("injected session append failure"),
        "the original error is preserved: {error}"
    );
    assert_eq!(
        std::fs::read(&path).unwrap(),
        before,
        "a failed partial write is truncated back to the previous length"
    );
    {
        let guard = session.lock().await;
        assert_eq!(
            guard.leaf_id(),
            Some(first_id.as_str()),
            "a failed append never commits the in-memory leaf"
        );
    }

    // The failed append is retryable and does not duplicate the transcript.
    let third_id = SessionManager::append_message_off_thread(&session, user_message("third"))
        .await
        .expect("a later append succeeds");
    assert_ne!(first_id, third_id);
    assert_eq!(session_user_texts(&path), vec!["first", "third"]);

    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn off_thread_append_leaves_the_async_runtime_responsive() {
    // The default `#[tokio::test]` runtime is single-threaded: a synchronous
    // sleep in the append path would starve this task entirely.
    let dir = temp_dir("off-thread-responsive");
    let mut session = SessionManager::create("C:/repo", &dir).unwrap();
    session.stall_next_append_for_test(Duration::from_millis(200));
    let path = session.path().to_path_buf();
    let session = Arc::new(tokio::sync::Mutex::new(session));

    let append = tokio::spawn({
        let session = Arc::clone(&session);
        async move { SessionManager::append_message_off_thread(&session, user_message("slow")).await }
    });
    // Let the spawned task submit its blocking work, then count how often the
    // async task gets polled while the blocking write is stalled.
    tokio::task::yield_now().await;
    let mut ticks = 0usize;
    while !append.is_finished() {
        ticks += 1;
        tokio::task::yield_now().await;
    }
    let id = append
        .await
        .expect("append task joins")
        .expect("append succeeds");
    assert!(!id.is_empty());
    assert!(
        ticks > 10,
        "the async runtime kept running while the blocking write was stalled (ticks={ticks})"
    );
    assert_eq!(session_user_texts(&path), vec!["slow"]);

    std::fs::remove_dir_all(&dir).ok();
}

/// A PAGED open must repair a torn trailing line before appending: a crash can
/// leave the last entry's bytes without their newline, and appending raw JSON
/// onto that line joins the two records — both then become unparseable on
/// every later load (the full-open path repairs the tail; the paged path used
/// to skip the repair it never ran).
#[test]
fn paged_open_repairs_a_torn_trailing_line_before_appending() {
    let dir = temp_dir("paged-torn-tail");
    let mut session = SessionManager::create("C:/repo", &dir).unwrap();
    for index in 0..60 {
        session
            .append_message(user_message(&format!(
                "message {index} {}",
                "x".repeat(2000)
            )))
            .unwrap();
    }
    let path = session.path().to_path_buf();
    drop(session);
    // Simulate the crash between the entry's line and its newline.
    let length = std::fs::metadata(&path).unwrap().len();
    assert!(length > 0);
    std::fs::OpenOptions::new()
        .write(true)
        .open(&path)
        .unwrap()
        .set_len(length - 1)
        .unwrap();
    assert_eq!(
        std::fs::read(&path).unwrap().last(),
        Some(&b'}'),
        "the fixture must end on a complete record without its newline"
    );

    let mut paged = SessionManager::open_paged(&path, 4 * 1024, 8).expect("paged open");
    assert!(paged.is_paged(), "the fixture must exceed the page caps");
    let leaf = paged.leaf_id().expect("resident leaf").to_owned();
    paged
        .append_message(user_message("appended-after-torn-tail"))
        .expect("append after a paged open");
    drop(paged);

    let full = SessionManager::open(&path).expect("full open");
    assert!(
        full.entry(&leaf).is_some(),
        "the torn entry must stay loadable after the tail repair"
    );
    let texts: Vec<String> = full
        .build_context_entries()
        .expect("context")
        .iter()
        .map(text_of)
        .collect();
    assert!(
        texts.iter().any(|text| text == "appended-after-torn-tail"),
        "the appended entry must parse instead of joining the torn line: {texts:?}"
    );
    std::fs::remove_dir_all(&dir).ok();
}

// ---------------------------------------------------------------------------
// D10: directory disk usage is metadata-only
// ---------------------------------------------------------------------------

/// `disk_usage` counts the `.jsonl` regular files and sums their lengths from
/// directory metadata. A file whose CONTENT is not a valid session still
/// counts (a header/peek scanner would reject it), which pins "never reads
/// file contents"; directories with a `.jsonl` extension are ignored.
#[test]
fn session_disk_usage_never_reads_file_contents() {
    let dir = temp_dir("disk-usage");
    let mut first = SessionManager::create("C:/repo", &dir).unwrap();
    first.append_message(user_message("first session")).unwrap();
    let mut second = SessionManager::create("C:/repo", &dir).unwrap();
    second
        .append_message(user_message("second session"))
        .unwrap();

    // Content-invalid session file: only a metadata-only scan can count it.
    let bogus = dir.join("bogus.jsonl");
    std::fs::write(&bogus, b"this is not a session file").unwrap();
    // A directory with the `.jsonl` extension is not a session file.
    std::fs::create_dir_all(dir.join("nested.jsonl")).unwrap();
    // Non-session extension is ignored.
    std::fs::write(dir.join("notes.txt"), b"ignored").unwrap();

    let usage = SessionManager::disk_usage(&dir);
    assert_eq!(usage.sessions, 3, "two sessions plus the bogus .jsonl file");
    let expected_bytes: u64 = [
        first.path().to_path_buf(),
        second.path().to_path_buf(),
        bogus.clone(),
    ]
    .iter()
    .map(|path| std::fs::metadata(path).unwrap().len())
    .sum();
    assert_eq!(usage.bytes, expected_bytes);

    let missing = SessionManager::disk_usage(&dir.join("does-not-exist"));
    assert_eq!((missing.sessions, missing.bytes), (0, 0));
    std::fs::remove_dir_all(&dir).ok();
}

/// `append_compaction` stores the same replayed system checkpoint whether it
/// walks the projection or the system-only entry view — WITHOUT materializing
/// the conversation.
///
/// The old implementation built a full `SessionProjection` (one clone of every
/// user/assistant/toolResult message) just to replay the system messages into
/// the compaction entry. This pins both halves: the checkpoint bytes are
/// unchanged, and `projection_build_count` does not move across the append.
#[test]
fn compaction_checkpoint_replays_system_entries_without_building_the_projection() {
    fn system_message(text: &str, timestamp: u64) -> Message {
        Message::System(UserMessage {
            role: UserRole::System,
            content: vec![ContentBlock::Text(TextContent {
                text: text.to_owned(),
                text_signature: None,
            })],
            tools_added: None,
            tools_removed: None,
            sections: None,
            timestamp,
        })
    }

    let mut session = SessionManager::in_memory("C:/repo", None).unwrap();
    session
        .append_message(system_message("base prompt", 1))
        .unwrap();
    session.append_message(user_message("hi")).unwrap();
    session
        .append_message(assistant_message("hello", StopReason::Stop))
        .unwrap();
    // A mid-conversation prompt update: the checkpoint is the REPLAY of both
    // system messages, not just the newest one.
    session
        .append_message(system_message("later instructions", 2))
        .unwrap();

    let expected = {
        let projection = session.build_session_projection().unwrap();
        pir::session_manager::projection::current_system_message(&projection.messages)
            .expect("the fixture carries system messages")
    };
    let builds_before = session.projection_build_count();

    session
        .append_compaction("summary", None, 100, None)
        .unwrap();

    assert_eq!(
        session.projection_build_count(),
        builds_before,
        "append_compaction must not materialize the transcript projection"
    );
    let entries = session.entries();
    let compaction = entries
        .iter()
        .rev()
        .find_map(|entry| match entry {
            FileEntry::Compaction(inner) => Some(inner),
            _ => None,
        })
        .expect("the compaction entry was appended");
    // The checkpoint's timestamp is deliberately the compaction's own stamp
    // (`ms_from_iso(&timestamp)`), not the replayed message's first timestamp;
    // every other field is the same replay the projection would produce.
    let stored = compaction
        .system_message
        .as_ref()
        .expect("the compaction stored a checkpoint");
    assert_eq!(
        stored.content, expected.content,
        "same replayed prompt text"
    );
    assert_eq!(stored.sections, expected.sections, "same sections");
    assert_eq!(stored.tools_added, expected.tools_added, "same tools");
    assert_eq!(
        stored.tools_removed, expected.tools_removed,
        "same removed tools"
    );
    assert!(stored.timestamp > 0, "the checkpoint carries a stamp");
    let text = stored.content.as_str();
    assert!(text.contains("base prompt"), "{text}");
    assert!(text.contains("later instructions"), "{text}");
}

/// `last_assistant_text` reads the same answer as the projection, including a
/// context-edit replacement, and does not materialize the transcript.
#[test]
fn last_assistant_text_matches_the_projection_without_building_it() {
    let mut session = SessionManager::in_memory("C:/repo", None).unwrap();
    session.append_message(user_message("q1")).unwrap();
    session
        .append_message(assistant_message("first answer", StopReason::Stop))
        .unwrap();
    session.append_message(user_message("q2")).unwrap();
    let second = session
        .append_message(assistant_message("second answer", StopReason::Stop))
        .unwrap();
    session
        .append_context_edit(&second, Some(serde_json::json!("rewritten answer")))
        .unwrap();
    session.append_message(user_message("q3")).unwrap();

    let expected = {
        let projection = session.build_session_projection().unwrap();
        projection
            .messages
            .iter()
            .rev()
            .find_map(|message| match message {
                Message::Assistant(assistant) => Some(
                    assistant
                        .content
                        .iter()
                        .filter_map(|block| match block {
                            ContentBlock::Text(text) => Some(text.text.as_str()),
                            _ => None,
                        })
                        .collect::<String>(),
                ),
                _ => None,
            })
            .expect("an assistant message")
    };
    assert_eq!(expected, "rewritten answer");

    let builds_before = session.projection_build_count();
    assert_eq!(
        session.last_assistant_text().as_deref(),
        Some("rewritten answer")
    );
    assert_eq!(
        session.projection_build_count(),
        builds_before,
        "last_assistant_text must not build the projection"
    );

    // `replacement: null` removes the newest assistant from context, so the
    // read falls back to the earlier answer instead of returning the omitted
    // one or None.
    let mut omitted = SessionManager::in_memory("C:/repo", None).unwrap();
    omitted.append_message(user_message("q1")).unwrap();
    omitted
        .append_message(assistant_message("kept answer", StopReason::Stop))
        .unwrap();
    let dropped = omitted
        .append_message(assistant_message("dropped answer", StopReason::Stop))
        .unwrap();
    omitted.append_context_edit(&dropped, None).unwrap();
    assert_eq!(
        omitted.last_assistant_text().as_deref(),
        Some("kept answer")
    );
}
