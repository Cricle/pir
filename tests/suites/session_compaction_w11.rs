//! W11 interop suite: session format v3 round-trips, paged-vs-full parity, and
//! the compaction checkpoint/cut-point contract.
//!
//! The three blocks below pin behaviour the older suites leave open:
//!
//! 1. **Format v3 round-trips** — every entry type (`usage` kinds, `label`,
//!    `session_info`, `context_edit`, `branch_summary`, `custom`,
//!    `custom_message`, and the checkpoint `systemMessage` with its sections
//!    and tool declarations) survives write→read→project byte-faithfully, ids
//!    stay 8 hex chars, and a fork/branch preserves a self-retaining
//!    compaction's cut.
//! 2. **Paged vs full projections** — the same fixture opened tail-first and
//!    opened fully must agree on the resident window's projection until the
//!    pages are stitched, and must agree EXACTLY once every page is read or
//!    the session is widened: usage, cost and compaction boundaries included.
//! 3. **Compaction** — projected cut points, split turns, thresholds, overflow
//!    recovery, the history/turn-prefix/update prompt shapes, branch
//!    summarization, and `firstKeptEntryId` on the persisted entry.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use pir::ai::types::{
    AssistantMessage, ContentBlock, Context, Message, Model, ModelCost, ModelCostRates, StopReason,
    TextContent, Tool, ToolCall, ToolResultMessage, ToolResultRole, Usage, UserMessage, UserRole,
};
use pir::compaction::prompts::COMPACTION_SUMMARY_PREFIX;
use pir::compaction::{
    compact_context, estimate_context_tokens, find_projected_cut_point, prepare_compaction,
    prepare_compaction_refs, recover_from_overflow, should_compact, should_compact_tokens,
    CompactionConfig, CompleteFn, CutEntry, PathEntryMeta, ProjectionEntryMeta,
};
use pir::session_manager::{FileEntry, SessionManager, StoredMessage, CURRENT_SESSION_VERSION};

// ---------------------------------------------------------------------------
// Shared fixtures
// ---------------------------------------------------------------------------

fn temp_dir(name: &str) -> PathBuf {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("pir-w11i-{name}-{stamp}"));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn text_block(text: &str) -> ContentBlock {
    ContentBlock::Text(TextContent {
        text: text.into(),
        text_signature: None,
    })
}

fn user_msg(text: &str) -> Message {
    Message::User(UserMessage {
        role: UserRole::User,
        content: vec![text_block(text)],
        tools_added: None,
        tools_removed: None,
        timestamp: 1,
        sections: None,
    })
}

fn system_msg(text: &str, timestamp: u64) -> Message {
    Message::System(UserMessage {
        role: UserRole::System,
        content: vec![text_block(text)],
        tools_added: None,
        tools_removed: None,
        sections: None,
        timestamp,
    })
}

fn assistant_msg(content: Vec<ContentBlock>, stop: StopReason, usage: Usage) -> Message {
    Message::Assistant(AssistantMessage {
        api: "test-api".into(),
        provider: "test".into(),
        model: "test-model".into(),
        content,
        stop_reason: stop,
        timestamp: 2,
        usage,
        ..Default::default()
    })
}

fn tool_result_msg(call_id: &str, text: &str) -> Message {
    Message::ToolResult(ToolResultMessage {
        role: ToolResultRole::ToolResult,
        tool_call_id: call_id.into(),
        tool_name: "read".into(),
        content: vec![text_block(text)],
        details: None,
        usage: None,
        added_tool_names: Vec::new(),
        structured_content: None,
        nested_calls: None,
        is_error: false,
        duration_ms: None,
        timestamp: 3,
    })
}

fn test_model(context_window: u64, max_tokens: u64) -> Model {
    Model {
        id: "test-model".into(),
        name: "Test".into(),
        api: "openai-completions".into(),
        provider: "test".into(),
        base_url: "http://localhost".into(),
        reasoning: false,
        thinking_level_map: None,
        input: vec!["text".into()],
        cost: ModelCost {
            rates: ModelCostRates {
                input: 0.0,
                output: 0.0,
                cache_read: 0.0,
                cache_write: 0.0,
            },
            tiers: Vec::new(),
        },
        context_window,
        max_tokens,
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

fn complete_with(text: &'static str, captured: Arc<Mutex<Vec<Context>>>) -> Arc<CompleteFn> {
    Arc::new(Box::new(move |context: Context| {
        captured.lock().unwrap().push(context);
        Box::pin(async move {
            Ok(AssistantMessage {
                api: "test-api".into(),
                provider: "test".into(),
                model: "test-model".into(),
                content: vec![text_block(text)],
                stop_reason: StopReason::Stop,
                usage: Usage {
                    total_tokens: 500,
                    ..Usage::default()
                },
                timestamp: 4,
                ..Default::default()
            })
        })
    }))
}

fn prompt_text_of(context: &Context) -> String {
    context
        .messages
        .iter()
        .flat_map(|message| match message {
            Message::User(user) => user
                .content
                .iter()
                .filter_map(|block| match block {
                    ContentBlock::Text(text) => Some(text.text.clone()),
                    _ => None,
                })
                .collect::<Vec<_>>(),
            _ => Vec::new(),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

// ---------------------------------------------------------------------------
// 1. Format v3 round-trips: every entry type
// ---------------------------------------------------------------------------

/// Every persisted entry type must come back out of a reopen with the same
/// wire shape — including the shapes older tests never exercised: a usage
/// entry with a non-`cache_warm` kind and a `note`, a `session_info` CLEAR, a
/// `label` CLEAR, a `context_edit` with `replacement: null` (omission), a
/// `branch_summary` with details/usage/fromHook, a `custom` entry with nested
/// data, and a `custom_message` whose content is a block ARRAY.
#[test]
fn every_entry_type_round_trips_through_disk() {
    let dir = temp_dir("roundtrip-all");
    let mut session = SessionManager::create("C:/repo", &dir).unwrap();

    let user_id = session.append_message(user_msg("prompt")).unwrap();
    let assistant_id = session
        .append_message(assistant_msg(
            vec![text_block("answer")],
            StopReason::Stop,
            Usage {
                total_tokens: 7,
                ..Usage::default()
            },
        ))
        .unwrap();

    // usage: an arbitrary kind plus a note (upstream `kind` is any string).
    session
        .append_usage(
            "provider_retry",
            "test",
            "test-model",
            Usage {
                input: 11,
                output: 3,
                cache_read: 2,
                cache_write: 1,
                total_tokens: 17,
                ..Usage::default()
            },
            Some("one retry"),
        )
        .unwrap();

    // thinking + model state.
    session.append_thinking_level_change("high").unwrap();
    session.append_model_change("test", "test-model-2").unwrap();

    // label: set then CLEAR (upstream `label: ""`).
    session.set_label(&user_id, Some("bookmark")).unwrap();
    session.set_label(&assistant_id, Some("other")).unwrap();
    session.set_label(&assistant_id, Some("")).unwrap();

    // session_info: name then CLEAR.
    session.set_session_name(Some("first name")).unwrap();
    session.set_session_name(None).unwrap();
    session.set_session_name(Some("final name")).unwrap();

    // custom entry with nested data.
    session
        .append_custom_entry(
            "artifact_index",
            Some(serde_json::json!({"items": [{"path": "a.rs", "rev": 3}]})),
        )
        .unwrap();

    // custom_message with a BLOCK array (not a string).
    let custom_message_id = session
        .append_custom_message_entry(
            "phone-inline",
            serde_json::json!([
                {"type": "text", "text": "inline"},
                {"type": "image", "data": "AAAA", "mimeType": "image/png"}
            ]),
            false,
            Some(serde_json::json!({"hidden": true})),
        )
        .unwrap();

    // branch_summary carrying details + usage + fromHook; the leaf moves back
    // to the assistant and the summary becomes its child.
    session
        .branch_with_summary(
            Some(&assistant_id),
            "abandoned path",
            Some(serde_json::json!({"readFiles": ["x.rs"], "modifiedFiles": ["y.rs"]})),
            Some(false),
            Some(Usage {
                total_tokens: 9,
                ..Usage::default()
            }),
        )
        .unwrap();

    // context_edit replacement (a string; pir keeps user-target strings raw)
    // on the branch, then an OMISSION (`replacement: null`) on the assistant.
    session
        .append_context_edit(&user_id, Some(serde_json::json!("edited prompt")))
        .unwrap();
    session.append_context_edit(&assistant_id, None).unwrap();

    // Reopen: the entry list and each typed payload must be identical.
    let reopened = SessionManager::open(session.path()).unwrap();
    assert_eq!(reopened.id(), session.id());
    assert_eq!(
        reopened.entries(),
        session.entries(),
        "every entry must survive write→read unchanged"
    );

    // Spot-check the typed shapes the projection depends on.
    let entries = reopened.entries();
    let usage = entries
        .iter()
        .find_map(|entry| match entry {
            FileEntry::Usage(inner) => Some(inner.clone()),
            _ => None,
        })
        .expect("usage entry");
    assert_eq!(usage.kind, "provider_retry");
    assert_eq!(usage.note.as_deref(), Some("one retry"));
    assert_eq!(usage.usage.total_tokens, 17);

    let branch_summary = entries
        .iter()
        .find_map(|entry| match entry {
            FileEntry::BranchSummary(inner) => Some(inner.clone()),
            _ => None,
        })
        .expect("branch summary");
    assert_eq!(branch_summary.summary, "abandoned path");
    assert_eq!(branch_summary.usage.as_ref().unwrap().total_tokens, 9);
    assert_eq!(branch_summary.from_hook, Some(false));
    assert_eq!(
        branch_summary.details.as_ref().unwrap()["modifiedFiles"][0],
        "y.rs"
    );

    let custom_message = entries
        .iter()
        .find_map(|entry| match entry {
            FileEntry::CustomMessage(inner) => Some(inner.clone()),
            _ => None,
        })
        .expect("custom message");
    assert_eq!(custom_message.id, custom_message_id);
    assert_eq!(custom_message.custom_type, "phone-inline");
    assert!(!custom_message.display);
    assert!(
        custom_message.content.is_array(),
        "block arrays stay arrays"
    );
    assert_eq!(custom_message.content[1]["type"], "image");

    // A cleared label and a cleared session_info name are the ABSENT values.
    assert_eq!(reopened.get_label(&assistant_id), None);
    assert_eq!(
        reopened.get_label(&user_id).as_deref(),
        Some("bookmark"),
        "clearing one target must not clear another"
    );
    assert_eq!(reopened.session_name().as_deref(), Some("final name"));

    // Every non-header entry id is 8 hex chars (upstream `generateId`).
    for entry in &entries {
        let id = entry.entry_id();
        assert_eq!(id.len(), 8, "id {id:?} is not 8 chars");
        assert!(id.chars().all(|c| c.is_ascii_hexdigit()), "id {id:?}");
    }

    // The context_edit omission removes the assistant from context while the
    // replacement rewrites the user prompt.
    let context = reopened.build_context_entries().unwrap();
    let all_text = context
        .iter()
        .flat_map(|message| match message {
            Message::User(user) => user.content.clone(),
            Message::Assistant(assistant) => assistant.content.clone(),
            _ => Vec::new(),
        })
        .filter_map(|block| match block {
            ContentBlock::Text(text) => Some(text.text),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        !all_text.contains("answer"),
        "the omitted assistant must not replay: {all_text:?}"
    );
    assert!(
        all_text.contains("edited prompt"),
        "the replaced user prompt replays: {all_text:?}"
    );

    std::fs::remove_dir_all(&dir).ok();
}

/// The checkpoint `systemMessage` (the field upstream stores on every
/// compaction entry) must keep its named sections and tool declarations
/// through disk, and the projection must replay them — a resumed session
/// restores its prompt sections and active tool list from it.
#[allow(clippy::assertions_on_constants)]
#[test]
fn compaction_system_message_keeps_sections_and_tools() {
    let dir = temp_dir("checkpoint-system");
    let mut session = SessionManager::create("C:/repo", &dir).unwrap();

    // A declaration system entry: base prompt + a named section + one tool.
    let tool = Tool {
        name: "read".into(),
        description: "read a file".into(),
        parameters: serde_json::json!({"type": "object"}),
        constrained_sampling: None,
    };
    let mut declaration = UserMessage {
        role: UserRole::System,
        content: vec![text_block("base prompt")],
        tools_added: Some(vec![tool.clone()]),
        tools_removed: None,
        sections: None,
        timestamp: 1_000,
    };
    let mut sections = std::collections::BTreeMap::new();
    sections.insert("guidelines".to_owned(), Some("be terse".to_owned()));
    sections.insert("obsolete".to_owned(), None);
    declaration.sections = Some(sections);
    session
        .append_message(Message::System(declaration))
        .unwrap();
    session.append_message(user_msg("hello")).unwrap();
    let compaction_id = session
        .append_compaction("checkpoint", None, 1_000, None)
        .unwrap();

    let reopened = SessionManager::open(session.path()).unwrap();
    let stored = match reopened.entry(&compaction_id).unwrap() {
        FileEntry::Compaction(inner) => inner.system_message.clone().expect("checkpoint system"),
        other => panic!("expected compaction entry, got {other:?}"),
    };
    assert_eq!(stored.content, "base prompt");
    assert_eq!(
        stored.sections.as_ref().unwrap()["guidelines"],
        "be terse",
        "the named section survives the disk round trip"
    );
    assert_eq!(stored.tools_added.as_ref().unwrap()[0]["name"], "read");

    // The projection replays the checkpoint system message with BOTH its
    // sections and its tool declaration (not just its text).
    let context = reopened.build_context_entries().unwrap();
    let replayed = match context.first() {
        Some(Message::System(system)) => system.clone(),
        other => panic!("expected a replayed system message, got {other:?}"),
    };
    assert_eq!(
        replayed.sections.as_ref().unwrap()["guidelines"].as_deref(),
        Some("be terse")
    );
    assert_eq!(replayed.tools_added.as_ref().unwrap()[0], tool);
    // The summary follows the checkpoint.
    assert!(matches!(context.get(1), Some(Message::User(_))));
    assert!(CURRENT_SESSION_VERSION >= 3);

    std::fs::remove_dir_all(&dir).ok();
}

/// A stored system message written by UPSTREAM may carry `content` as a
/// `TextContent[]` array. It must load (joined text), not disappear.
#[test]
fn block_array_system_content_loads_lossily() {
    let dir = temp_dir("system-blocks");
    let path = dir.join("blocks.jsonl");
    std::fs::write(
        &path,
        "{\"type\":\"session\",\"version\":3,\"id\":\"abc\",\"timestamp\":\"2026-01-01T00:00:00.000Z\",\"cwd\":\"C:\\\\repo\"}\n\
         {\"type\":\"message\",\"id\":\"aa11bb22\",\"parentId\":null,\"timestamp\":\"2026-01-01T00:00:01.000Z\",\
         \"message\":{\"role\":\"system\",\"content\":[{\"type\":\"text\",\"text\":\"first\"},{\"type\":\"text\",\"text\":\"second\"}],\"timestamp\":1000}}\n",
    )
    .unwrap();

    let session = SessionManager::open(&path).unwrap();
    let entries = session.entries();
    assert_eq!(entries.len(), 1, "the entry must not be dropped");
    match &entries[0] {
        FileEntry::Message(inner) => match &inner.message {
            StoredMessage::System(system) => {
                assert_eq!(system.content, "first\nsecond");
            }
            other => panic!("expected a system message, got {other:?}"),
        },
        other => panic!("expected a message entry, got {other:?}"),
    }
    std::fs::remove_dir_all(&dir).ok();
}

/// A fork and a branch both carry a SELF-RETAINING compaction (one whose
/// `firstKeptEntryId` is its own id) unchanged: remapping must never re-point
/// it at another entry, or the retained range would silently grow.
#[test]
fn fork_and_branch_preserve_self_retaining_compactions() {
    let source_dir = temp_dir("selfretain-src");
    let target_dir = temp_dir("selfretain-dst");
    let mut source = SessionManager::create("C:/repo", &source_dir).unwrap();
    source.append_message(user_msg("one")).unwrap();
    let compaction_id = source
        .append_compaction("only summary", None, 42, None)
        .unwrap();
    source.append_message(user_msg("two")).unwrap();
    let source_path = source.path().to_path_buf();

    // Fork: the compaction line is copied verbatim.
    let forked =
        SessionManager::fork_from_source(&source_path, "C:\\dst", &target_dir, None).unwrap();
    let forked_compaction = match forked.entry(&compaction_id).unwrap() {
        FileEntry::Compaction(inner) => inner.clone(),
        other => panic!("expected compaction entry, got {other:?}"),
    };
    assert_eq!(
        forked_compaction.first_kept_entry_id, compaction_id,
        "a fork must keep the self-retaining cut"
    );

    // Branch onto a NEW file: the path is re-chained, the self-retention is not.
    let mut source = SessionManager::open(&source_path).unwrap();
    let leaf = source.leaf_id().unwrap().to_owned();
    let branched_path = source
        .create_branched_session(&leaf)
        .unwrap()
        .expect("persisted branch");
    let branched = SessionManager::open(&branched_path).unwrap();
    let branched_compaction = match branched.entry(&compaction_id).unwrap() {
        FileEntry::Compaction(inner) => inner.clone(),
        other => panic!("expected compaction entry, got {other:?}"),
    };
    assert_eq!(
        branched_compaction.first_kept_entry_id, compaction_id,
        "a branch must keep the self-retaining cut"
    );
    // The re-chained branch keeps the compaction's parent (the first message):
    // removing/normalizing the path must not orphan the checkpoint.
    let first_message_id = branched
        .entries()
        .iter()
        .find(|entry| matches!(entry, FileEntry::Message(_)))
        .unwrap()
        .entry_id()
        .to_owned();
    assert_eq!(
        branched_compaction.parent_id.as_deref(),
        Some(first_message_id.as_str())
    );

    std::fs::remove_dir_all(&source_dir).ok();
    std::fs::remove_dir_all(&target_dir).ok();
}

// ---------------------------------------------------------------------------
// 2. Paged vs full projections
// ---------------------------------------------------------------------------

/// Write a v3 session with `turns` user/assistant pairs, a usage entry per
/// turn, and an optional compaction at the midpoint. Returns the path.
fn write_paged_fixture(path: &Path, turns: usize, compaction_at: Option<usize>) {
    use std::io::Write as _;
    let mut file = std::fs::File::create(path).unwrap();
    writeln!(
        file,
        "{}",
        serde_json::json!({
            "type": "session", "version": 3, "id": "pagedfixture",
            "timestamp": "2026-01-01T00:00:00.000Z", "cwd": "C:\\repo",
        })
    )
    .unwrap();
    let mut parent: Option<String> = None;
    let mut compaction_written = false;
    for index in 0..turns {
        if !compaction_written && compaction_at == Some(index) {
            compaction_written = true;
            let id = format!("c{index:06}");
            // A REAL cut: the compaction is appended at the end of the
            // previous turn and keeps that turn's user message onward.
            let first_kept = format!("u{:06}", index.saturating_sub(1));
            writeln!(
                file,
                "{}",
                serde_json::json!({
                    "type": "compaction", "id": id, "parentId": parent,
                    "timestamp": "2026-01-01T00:00:00.000Z",
                    "summary": "earlier work",
                    "firstKeptEntryId": first_kept,
                    "tokensBefore": 5000,
                })
            )
            .unwrap();
            parent = Some(id);
        }
        let user_id = format!("u{index:06}");
        writeln!(
            file,
            "{}",
            serde_json::json!({
                "type": "message", "id": user_id, "parentId": parent,
                "timestamp": "2026-01-01T00:00:00.000Z",
                "message": {
                    "role": "user", "timestamp": index as u64 * 10,
                    "content": [{"type": "text", "text": format!("question {index}: {}", "x".repeat(400))}],
                },
            })
        )
        .unwrap();
        let assistant_id = format!("a{index:06}");
        writeln!(
            file,
            "{}",
            serde_json::json!({
                "type": "message", "id": assistant_id, "parentId": user_id,
                "timestamp": "2026-01-01T00:00:00.000Z",
                "message": {
                    "role": "assistant", "timestamp": index as u64 * 10 + 5,
                    "api": "test-api", "provider": "test", "model": "test-model",
                    "stopReason": "stop",
                    "usage": {
                        "input": 100 + index as u64, "output": 10,
                        "cacheRead": 0, "cacheWrite": 0,
                        "totalTokens": 110 + index as u64,
                        "cost": {"input": 0.001, "output": 0.002, "cacheRead": 0.0, "cacheWrite": 0.0, "total": 0.003},
                    },
                    "content": [{"type": "text", "text": format!("answer {index}")}],
                },
            })
        )
        .unwrap();
        writeln!(
            file,
            "{}",
            serde_json::json!({
                "type": "usage", "id": format!("g{index:06}"), "parentId": assistant_id,
                "timestamp": "2026-01-01T00:00:00.000Z",
                "kind": "cache_warm", "provider": "test", "model": "test-model",
                "usage": {
                    "input": 5, "output": 1, "cacheRead": 0, "cacheWrite": 0,
                    "totalTokens": 6,
                    "cost": {"input": 0.0, "output": 0.0, "cacheRead": 0.0, "cacheWrite": 0.0, "total": 0.0},
                },
            })
        )
        .unwrap();
        parent = Some(format!("g{index:06}"));
    }
}

fn total_usage(entries: &[FileEntry]) -> Usage {
    let mut total = Usage::default();
    for entry in entries {
        let usage = match entry {
            FileEntry::Message(inner) => match &inner.message {
                StoredMessage::Assistant(assistant) => Some(&assistant.usage),
                StoredMessage::ToolResult(result) => result.usage.as_ref(),
                _ => None,
            },
            FileEntry::Usage(inner) => Some(&inner.usage),
            FileEntry::Compaction(inner) => inner.usage.as_ref(),
            FileEntry::BranchSummary(inner) => inner.usage.as_ref(),
            _ => None,
        };
        if let Some(usage) = usage {
            total.input += usage.input;
            total.output += usage.output;
            total.cache_read += usage.cache_read;
            total.cache_write += usage.cache_write;
            total.total_tokens += usage.total_tokens;
            total.cost.input += usage.cost.input;
            total.cost.output += usage.cost.output;
            total.cost.total += usage.cost.total;
        }
    }
    total
}

/// The pages stitched back together must equal the FULL open on the fixture —
/// entries, usage/cost totals, and the compaction boundary. The resident
/// window alone must NOT pretend to be the whole session.
#[test]
fn paged_pages_stitch_back_to_the_full_projection() {
    let dir = temp_dir("paged-parity");
    let path = dir.join("session.jsonl");
    write_paged_fixture(&path, 60, Some(20));

    let full = SessionManager::open(&path).expect("full open");
    let full_entries = full.entries();
    let paged = SessionManager::open_paged(&path, 8 * 1024, 12).expect("paged open");
    assert!(paged.is_paged(), "the fixture must exceed the caps");
    assert!(
        paged.entries().len() < full_entries.len(),
        "the resident window is a strict subset"
    );

    // The resident window is a SUFFIX of the file's entries.
    let resident = paged.entries();
    let resident_start = full_entries.len() - resident.len();
    assert_eq!(
        resident,
        full_entries[resident_start..].to_vec(),
        "the resident window must be a suffix of the full entry list"
    );

    // Walk every older page and prepend; the stitched list equals the full one.
    let mut stitched = resident.clone();
    let mut cursor = paged.prefix_cursor().expect("cursor");
    while let Some(page) = paged
        .read_older_page(cursor, 8 * 1024, 12)
        .expect("older page")
    {
        let mut older: Vec<FileEntry> = page
            .entries()
            .iter()
            .filter(|entry| !entry.is_header())
            .map(|entry| entry.as_ref().clone())
            .collect();
        older.extend(stitched);
        stitched = older;
        cursor = page.page_start();
    }
    assert_eq!(cursor, 0, "the walk reaches the file start");
    assert_eq!(
        stitched, full_entries,
        "the stitched pages must equal the FULL open entry for entry"
    );

    // Usage/cost totals over the stitched pages equal the full open's.
    let full_usage = total_usage(&full_entries);
    let stitched_usage = total_usage(&stitched);
    assert_eq!(stitched_usage.input, full_usage.input);
    assert_eq!(stitched_usage.output, full_usage.output);
    assert_eq!(stitched_usage.total_tokens, full_usage.total_tokens);
    assert_eq!(stitched_usage.cost.total, full_usage.cost.total);

    // Compaction boundaries agree (the fixture holds one cut).
    let full_cuts: Vec<(String, String)> = full
        .entries()
        .into_iter()
        .filter_map(|entry| match entry {
            FileEntry::Compaction(inner) => Some((inner.id, inner.first_kept_entry_id)),
            _ => None,
        })
        .collect();
    let stitched_cuts: Vec<(String, String)> = stitched
        .iter()
        .filter_map(|entry| match entry {
            FileEntry::Compaction(inner) => {
                Some((inner.id.clone(), inner.first_kept_entry_id.clone()))
            }
            _ => None,
        })
        .collect();
    assert_eq!(full_cuts, stitched_cuts);
    assert_eq!(full_cuts.len(), 1);

    // Widening gives the byte-identical entry list and projection.
    let mut widened = SessionManager::open_paged(&path, 8 * 1024, 12).expect("paged open");
    widened.widen_to_full().expect("widen");
    assert!(!widened.is_paged());
    assert_eq!(widened.entries(), full_entries);
    assert_eq!(
        widened.build_session_projection().unwrap().messages,
        full.build_session_projection().unwrap().messages,
        "paged-then-widened must project identically to a full open"
    );

    std::fs::remove_dir_all(&dir).ok();
}

/// The streamed tree scan of a paged manager must cover the WHOLE session
/// (rows and counter), not the resident window — and its rows must equal the
/// full manager's rows exactly.
#[test]
fn paged_tree_scan_matches_the_full_scan() {
    let dir = temp_dir("paged-tree");
    let path = dir.join("tree.jsonl");
    write_paged_fixture(&path, 40, None);

    let full = SessionManager::open(&path).expect("full open");
    let paged = SessionManager::open_paged(&path, 8 * 1024, 12).expect("paged open");
    assert!(paged.is_paged());

    let mut full_rows: Vec<(String, Option<String>, &'static str)> = Vec::new();
    let full_stats = full
        .for_each_tree_row(|row| {
            full_rows.push((row.id.clone(), row.parent_id.clone(), row.kind_hint));
        })
        .unwrap();
    let mut paged_rows: Vec<(String, Option<String>, &'static str)> = Vec::new();
    let paged_stats = paged
        .for_each_tree_row(|row| {
            paged_rows.push((row.id.clone(), row.parent_id.clone(), row.kind_hint));
        })
        .unwrap();

    assert_eq!(full_rows, paged_rows, "rows must match the full scan");
    assert_eq!(full_stats.rows, paged_stats.rows);
    assert_eq!(full_stats.roots, paged_stats.roots);
    assert_eq!(full_stats.max_depth, paged_stats.max_depth);
    assert_eq!(full_stats.entries, paged_stats.entries);
    assert!(
        paged_stats.entries > paged.entries().len(),
        "the scan counted the streamed prefix too"
    );
    assert!(paged.is_paged(), "the scan never widens the session");

    std::fs::remove_dir_all(&dir).ok();
}

/// Listing a paged session must agree with listing the full one: id, cwd,
/// name, message count, and parent path all come from the same bytes.
#[test]
fn paged_listing_agrees_with_the_full_listing() {
    let dir = temp_dir("paged-listing");
    let path = dir.join("listing.jsonl");
    write_paged_fixture(&path, 30, None);
    // Add a name so the peek has something to find.
    let mut session = SessionManager::open(&path).unwrap();
    session.set_session_name(Some("paged name")).unwrap();
    session.append_message(user_msg("last")).unwrap();
    drop(session);

    let summaries = SessionManager::list_sessions(&dir, None);
    assert_eq!(summaries.len(), 1);
    let summary = &summaries[0];
    assert_eq!(summary.id, "pagedfixture");
    assert_eq!(summary.name.as_deref(), Some("paged name"));
    assert_eq!(summary.message_count, 61, "30 pairs + the last prompt");

    std::fs::remove_dir_all(&dir).ok();
}

/// A window is self-contained only when the LEAF PATH's checkpoint (and its
/// retained range) is resident: an off-path compaction in the window must not
/// make a truncated context look complete, and a checkpoint whose kept range
/// fell into the skipped prefix must widen too.
#[test]
fn paged_widening_requires_an_on_path_resident_checkpoint() {
    use pir::compaction::paged_requires_widening;

    // (a) Off-path compaction: build a long abandoned branch with a
    // checkpoint on it, then branch back to the root and continue; the
    // newest leaf-path entry is no compaction, but the off-path checkpoint
    // sits inside the resident window.
    let dir = temp_dir("paged-widen-branch");
    let mut session = SessionManager::create("C:/repo", &dir).unwrap();
    let root = session.append_message(user_msg("root")).unwrap();
    for index in 0..30 {
        session
            .append_message(user_msg(&format!("abandoned {index} {}", "a".repeat(300))))
            .unwrap();
    }
    session
        .append_compaction("abandoned checkpoint", Some(&root), 500, None)
        .unwrap();
    session.branch(&root).unwrap();
    for index in 0..10 {
        session
            .append_message(user_msg(&format!("current {index} {}", "c".repeat(300))))
            .unwrap();
    }
    let path_for_manager = session.path().to_path_buf();
    drop(session);
    let paged = SessionManager::open_paged(&path_for_manager, 64 * 1024, 16).expect("paged open");
    assert!(paged.is_paged(), "the fixture must exceed the caps");
    assert!(
        paged
            .entries()
            .iter()
            .any(|entry| matches!(entry, FileEntry::Compaction(_))),
        "the abandoned checkpoint is inside the resident window"
    );
    let model = test_model(10_000_000, 8_192);
    let config = CompactionConfig::default();
    assert!(
        paged_requires_widening(&paged, &model, &config),
        "an OFF-PATH compaction must not make the window look complete"
    );
    drop(paged);
    std::fs::remove_dir_all(&dir).ok();

    // (b) On-path checkpoint with its retained range resident: self-contained,
    // so the window alone is the complete model context.
    let dir = temp_dir("paged-widen-self");
    let path = dir.join("self.jsonl");
    // The checkpoint sits just before the LAST turn, so a 16-entry tail window
    // holds it, its kept range, and the turn after it.
    write_paged_fixture(&path, 31, Some(30));
    let paged = SessionManager::open_paged(&path, 64 * 1024, 16).expect("paged open");
    assert!(paged.is_paged());
    let projection = paged.build_session_projection().unwrap();
    assert!(
        matches!(
            projection
                .entries
                .first()
                .map(|entry| entry.source_entry.as_ref()),
            Some(FileEntry::Compaction(_))
        ),
        "the window's projection starts at the on-path checkpoint"
    );
    let first_kept = match projection
        .entries
        .first()
        .map(|entry| entry.source_entry.as_ref())
    {
        Some(FileEntry::Compaction(compaction)) => compaction.first_kept_entry_id.clone(),
        _ => unreachable!(),
    };
    assert!(
        paged.entry(&first_kept).is_some(),
        "the checkpoint's kept range is resident: {first_kept}"
    );
    assert!(
        !paged_requires_widening(&paged, &model, &config),
        "an on-path checkpoint with its kept range resident needs no widening"
    );

    std::fs::remove_dir_all(&dir).ok();
}

// ---------------------------------------------------------------------------
// 3. Compaction: cut points, threshold, overflow, prompts, branch, cut ids
// ---------------------------------------------------------------------------

fn cut_entries(messages: &[Message]) -> Vec<CutEntry> {
    messages
        .iter()
        .cloned()
        .map(CutEntry::from_message)
        .collect()
}

/// A projected cut walks BACKWARDS and prefers a late cut point; a split turn
/// reports the turn's user message. Mirrors upstream `findProjectedCutPoint`.
#[test]
fn projected_cut_point_keeps_the_newest_rows() {
    let messages = vec![
        user_msg("turn one"),
        assistant_msg(
            vec![text_block("reply one")],
            StopReason::Stop,
            Usage::default(),
        ),
        user_msg("turn two"),
        assistant_msg(
            vec![text_block("reply two")],
            StopReason::Stop,
            Usage::default(),
        ),
    ];
    let entries = cut_entries(&messages);

    // A budget that the last turn alone satisfies cuts at its user message.
    let turn_tokens = pir::compaction::estimate_message_tokens(&messages[2])
        + pir::compaction::estimate_message_tokens(&messages[3]);
    let result = find_projected_cut_point(&entries, 0, entries.len(), turn_tokens);
    assert_eq!(result.first_kept_entry_index, 2);
    assert!(!result.is_split_turn, "the cut lands on a turn start");
    assert_eq!(result.turn_start_index, -1);

    // A budget crossed at the assistant reply splits the turn and reports its
    // user message.
    let split_budget = pir::compaction::estimate_message_tokens(&messages[3]);
    let result = find_projected_cut_point(&entries, 0, entries.len(), split_budget);
    assert_eq!(result.first_kept_entry_index, 3);
    assert!(result.is_split_turn);
    assert_eq!(result.turn_start_index, 2);
}

/// Tool results are never cut points, even when they dominate the budget.
#[test]
fn projected_cut_point_never_opens_on_a_tool_result() {
    let messages = vec![
        user_msg("use the tool"),
        assistant_msg(
            vec![ContentBlock::ToolCall(ToolCall {
                id: "call-1".into(),
                name: "read".into(),
                arguments: Default::default(),
                thought_signature: None,
                namespace: None,
                arguments_error: None,
            })],
            StopReason::ToolUse,
            Usage::default(),
        ),
        tool_result_msg("call-1", &"z".repeat(40_000)),
    ];
    let entries = cut_entries(&messages);
    let result = find_projected_cut_point(&entries, 0, entries.len(), 1);
    assert_eq!(
        result.first_kept_entry_index, 1,
        "the cut opens on the assistant tool call, never the result"
    );
    assert!(!entries[2].is_cut_point());
}

/// The threshold is `contextTokens > contextWindow - reserveTokens`, strictly,
/// and a disabled config never compacts — pinned directly on the token form so
/// a future estimator change cannot move the boundary.
#[test]
fn threshold_is_strict_and_respects_the_enabled_flag() {
    let config = CompactionConfig {
        enabled: true,
        reserve_tokens: 1_000,
        keep_recent_tokens: 100,
    };
    assert!(!should_compact_tokens(9_000, 10_000, &config));
    assert!(should_compact_tokens(9_001, 10_000, &config));
    assert!(!should_compact_tokens(
        9_001,
        10_000,
        &CompactionConfig {
            enabled: false,
            ..config.clone()
        }
    ));

    // The message-level wrapper agrees with the token form.
    let messages = vec![
        user_msg("hi"),
        assistant_msg(
            vec![text_block("there")],
            StopReason::Stop,
            Usage {
                total_tokens: 9_001,
                ..Usage::default()
            },
        ),
    ];
    let context = Context {
        system_prompt: None,
        messages: messages.clone(),
        tools: None,
    };
    let model = test_model(10_000, 1_000);
    assert_eq!(
        estimate_context_tokens(&messages).tokens,
        9_001,
        "the last usage carries the count"
    );
    assert!(should_compact(&context, &model, &config));
}

/// A system message's sections and tool declarations count toward the estimate
/// (upstream `estimateTokens`'s system case), so a prompt-heavy session
/// compacts when its REAL context is full.
#[test]
fn token_estimate_counts_sections_and_tool_declarations() {
    let bare = system_msg("type a task", 1);
    let bare_tokens = pir::compaction::estimate_message_tokens(&bare);

    let mut rich = UserMessage {
        role: UserRole::System,
        content: vec![text_block("type a task")],
        tools_added: Some(vec![Tool {
            name: "read".into(),
            description: "read a file from disk".into(),
            parameters: serde_json::json!({"type": "object", "properties": {"path": {"type": "string"}}}),
            constrained_sampling: None,
        }]),
        tools_removed: None,
        sections: None,
        timestamp: 1,
    };
    let mut sections = std::collections::BTreeMap::new();
    sections.insert("guidelines".to_owned(), Some("x".repeat(4_000)));
    rich.sections = Some(sections);
    let rich_tokens = pir::compaction::estimate_message_tokens(&Message::System(rich.clone()));

    assert!(
        rich_tokens > bare_tokens + 1_000,
        "the 4000-char section must count: {rich_tokens} vs {bare_tokens}"
    );
    // The removed-tool record is NOT counted upstream.
    rich.tools_removed = Some(vec![pir::ai::types::ToolReference {
        name: "bash".into(),
    }]);
    assert_eq!(
        pir::compaction::estimate_message_tokens(&Message::System(rich)),
        rich_tokens,
        "toolsRemoved carries no weight (upstream parity)"
    );
}

/// `prepare_compaction` cuts, summarizes and KEEPS the tail; the persisted
/// compaction entry records the cut as `firstKeptEntryId`, and replaying the
/// entry tree yields summary + kept range.
#[tokio::test]
async fn prepared_compaction_records_first_kept_entry_id() {
    let dir = temp_dir("prepare-cut");
    let mut session = SessionManager::create("C:/repo", &dir).unwrap();
    // Three turns of ~1k tokens each; keep_recent_tokens keeps the last one.
    for index in 0..3 {
        session
            .append_message(user_msg(&format!("turn {index} {}", "q".repeat(2_000))))
            .unwrap();
        session
            .append_message(assistant_msg(
                vec![text_block(&format!("reply {index} {}", "r".repeat(2_000)))],
                StopReason::Stop,
                Usage {
                    total_tokens: 1_000,
                    ..Usage::default()
                },
            ))
            .unwrap();
    }

    let projection = session.build_session_projection().unwrap();
    let metas = ProjectionEntryMeta::from_projection(&projection.entries);
    let refs = ProjectionEntryMeta::cut_refs(&metas, &projection.messages);
    let branch = session.branch_path_meta(None);
    let config = CompactionConfig {
        enabled: true,
        reserve_tokens: 1_000,
        keep_recent_tokens: 1_200,
    };
    let preparation = prepare_compaction_refs(&refs, &branch, &config).expect("a plan");
    let first_kept = preparation
        .first_kept_entry_id
        .clone()
        .expect("an entry id");
    assert!(!preparation.messages_to_summarize.is_empty());
    assert!(!preparation.kept_messages.is_empty());
    assert!(
        preparation.is_split_turn,
        "the budget is crossed at an assistant reply, so the cut splits its turn"
    );
    let preparation_is_split = preparation.is_split_turn;

    let captured: Arc<Mutex<Vec<Context>>> = Arc::new(Mutex::new(Vec::new()));
    let result = pir::compaction::compact_with_preparation_refs(
        preparation,
        &test_model(36_384, 8_192),
        complete_with("checkpoint", captured.clone()),
        None,
    )
    .await
    .expect("compaction");
    assert_eq!(
        result.first_kept_entry_id.as_deref(),
        Some(first_kept.as_str())
    );

    // Persist the checkpoint exactly as the host does and replay it.
    session
        .append_compaction(
            result.summary.clone(),
            result.first_kept_entry_id.as_deref(),
            result.tokens_before,
            None,
        )
        .unwrap();
    let reopened = SessionManager::open(session.path()).unwrap();
    let context = reopened.build_context_entries().unwrap();
    let first_text = match &context[0] {
        Message::User(user) => match &user.content[0] {
            ContentBlock::Text(text) => text.text.clone(),
            other => panic!("expected text, got {other:?}"),
        },
        other => panic!("expected the summary user message, got {other:?}"),
    };
    assert!(first_text.starts_with(COMPACTION_SUMMARY_PREFIX));
    assert!(
        first_text.contains("checkpoint"),
        "the checkpoint summary leads the replay"
    );

    // The kept range starts at the recorded entry: the projector's second
    // message is the first kept message (the cut may land on an assistant
    // reply when it splits a turn).
    let kept_entry = reopened.entry(&first_kept).expect("kept entry exists");
    assert!(matches!(kept_entry, FileEntry::Message(_)));
    let kept_text = entry_text(kept_entry);
    assert!(!kept_text.is_empty(), "the kept entry carries text");
    assert_eq!(
        message_text(&context[1]),
        kept_text,
        "context[1] must be the recorded first kept entry"
    );

    // The borrowed and owned preparations agree on the plan (same projection,
    // no session mutation between the two).
    let owned = prepare_compaction(refs.as_slice(), &branch, &config).expect("owned plan");
    assert_eq!(owned.first_kept_entry_id, result.first_kept_entry_id);
    assert_eq!(owned.tokens_before, result.tokens_before);
    assert_eq!(owned.is_split_turn, preparation_is_split);

    std::fs::remove_dir_all(&dir).ok();
}

/// The text of a stored entry's message, whichever role it carries.
fn entry_text(entry: &FileEntry) -> String {
    match entry {
        FileEntry::Message(inner) => message_text(&inner.message.clone().into()),
        _ => String::new(),
    }
}

/// The text blocks of a message joined with "\n".
fn message_text(message: &Message) -> String {
    let blocks = match message {
        Message::User(user) | Message::System(user) | Message::Developer(user) => {
            user.content.clone()
        }
        Message::Assistant(assistant) => assistant.content.clone(),
        Message::ToolResult(result) => result.content.clone(),
    };
    blocks
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text(text) => Some(text.text.clone()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// A compaction that already ends the branch is refused ("Already compacted"),
/// and a session too small to cut yields no plan.
#[test]
fn prepare_compaction_refuses_a_compaction_tail_and_a_small_session() {
    let mut session = SessionManager::in_memory("C:/repo", None).unwrap();
    session.append_message(user_msg("only turn")).unwrap();
    session.append_compaction("done", None, 10, None).unwrap();

    let projection = session.build_session_projection().unwrap();
    let metas = ProjectionEntryMeta::from_projection(&projection.entries);
    let refs = ProjectionEntryMeta::cut_refs(&metas, &projection.messages);
    let branch = session.branch_path_meta(None);
    assert!(prepare_compaction_refs(&refs, &branch, &CompactionConfig::default()).is_none());

    // One tiny message: no cut point other than the start, and nothing to
    // summarize once the boundary is dropped.
    let small = cut_entries(&[user_msg("hi")]);
    let path: Vec<PathEntryMeta> = vec![PathEntryMeta {
        id: Some("u1".into()),
        is_compaction: false,
        is_context_edit: false,
    }];
    assert!(prepare_compaction(small.as_slice(), &path, &CompactionConfig::default()).is_none());
}

/// Overflow recovery drops the failed trailing assistant, compacts, and hands
/// back the rebuilt context; without a trailing assistant it returns `None`.
#[tokio::test]
async fn overflow_recovery_drops_the_failed_attempt_and_compacts() {
    let captured: Arc<Mutex<Vec<Context>>> = Arc::new(Mutex::new(Vec::new()));
    let model = test_model(36_384, 8_192);
    let config = CompactionConfig {
        enabled: true,
        reserve_tokens: 1_000,
        keep_recent_tokens: 100,
    };
    let context = Context {
        system_prompt: Some("system".into()),
        messages: vec![
            user_msg("first question"),
            assistant_msg(
                vec![text_block("first reply")],
                StopReason::Stop,
                Usage::default(),
            ),
            user_msg(&format!("long prompt {}", "x".repeat(8_000))),
            assistant_msg(
                vec![text_block("partial")],
                StopReason::Error,
                Usage::default(),
            ),
        ],
        tools: None,
    };
    let recovered = recover_from_overflow(
        context,
        "prompt is too long",
        &model,
        complete_with("recovered", captured.clone()),
        &config,
    )
    .await
    .expect("recovery");
    assert_eq!(recovered.system_prompt.as_deref(), Some("system"));
    assert!(matches!(recovered.messages.first(), Some(Message::User(_))));
    let prompt = prompt_text_of(&captured.lock().unwrap()[0]);
    assert!(prompt
        .contains("Additional focus: Recover from a context-overflow error: prompt is too long"));

    // No trailing assistant: nothing to recover from.
    let no_recovery = recover_from_overflow(
        Context {
            system_prompt: None,
            messages: vec![user_msg("hi")],
            tools: None,
        },
        "overflow",
        &model,
        complete_with("unused", captured),
        &config,
    )
    .await;
    assert!(no_recovery.is_none());
}

/// The three prompt shapes are byte-pinned: history (opening), update
/// (previous summary) and turn-prefix (`# Conversation` framing).
#[tokio::test]
async fn summarizer_prompts_match_the_upstream_framings() {
    let captured: Arc<Mutex<Vec<Context>>> = Arc::new(Mutex::new(Vec::new()));
    let model = test_model(36_384, 8_192);
    let config = CompactionConfig {
        enabled: true,
        reserve_tokens: 16_384,
        keep_recent_tokens: 500,
    };
    // Four messages of 1000 chars (250 estimated tokens each): the backward
    // budget walk crosses 500 at the second turn's user message, keeping one
    // summary span and one call.
    let history_messages = || {
        vec![
            user_msg(&"a".repeat(1_000)),
            assistant_msg(
                vec![text_block(&"b".repeat(1_000))],
                StopReason::Stop,
                Usage::default(),
            ),
            user_msg(&"c".repeat(1_000)),
            assistant_msg(
                vec![text_block(&"d".repeat(1_000))],
                StopReason::Stop,
                Usage::default(),
            ),
        ]
    };

    // History (no previous summary).
    compact_context(
        Context {
            system_prompt: None,
            messages: history_messages(),
            tools: None,
        },
        &model,
        complete_with("history", captured.clone()),
        &config,
        None,
        None,
    )
    .await
    .expect("history compaction");
    let history = prompt_text_of(&captured.lock().unwrap()[0]);
    assert!(history.starts_with("<conversation>\n"));
    assert!(!history.contains("<previous-summary>"));
    assert!(history.contains("Create a structured context checkpoint summary"));

    // Update (previous summary present).
    compact_context(
        Context {
            system_prompt: None,
            messages: history_messages(),
            tools: None,
        },
        &model,
        complete_with("updated", captured.clone()),
        &config,
        Some("prior checkpoint"),
        None,
    )
    .await
    .expect("update compaction");
    let update = prompt_text_of(&captured.lock().unwrap()[1]);
    assert!(update.contains("<previous-summary>\nprior checkpoint\n</previous-summary>"));
    assert!(update.contains("Update the existing structured summary with new information."));

    // Turn prefix (split turn): its own framing.
    let big = "y".repeat(4_000);
    let split_config = CompactionConfig {
        enabled: true,
        reserve_tokens: 1_000,
        keep_recent_tokens: 100,
    };
    pir::compaction::compact_context(
        Context {
            system_prompt: None,
            messages: vec![
                user_msg(&big),
                assistant_msg(vec![text_block(&big)], StopReason::Stop, Usage::default()),
                user_msg("later"),
                assistant_msg(
                    vec![text_block("later reply")],
                    StopReason::Stop,
                    Usage::default(),
                ),
            ],
            tools: None,
        },
        &model,
        complete_with("merged", captured.clone()),
        &split_config,
        None,
        None,
    )
    .await
    .expect("split compaction");
    let prompts = captured.lock().unwrap();
    assert!(
        prompts
            .iter()
            .skip(2)
            .any(|context| prompt_text_of(context).starts_with("# Conversation\n")),
        "the split-turn prefix call uses its own framing"
    );
}

/// Branch summarization over a real branch: the abandoned path is summarized
/// (never the shared prefix), and the preamble + file sections are appended.
#[tokio::test]
async fn branch_summarization_covers_only_the_abandoned_path() {
    let dir = temp_dir("branch-summary");
    let mut session = SessionManager::create("C:/repo", &dir).unwrap();
    let first = session.append_message(user_msg("shared root")).unwrap();
    session
        .append_message(user_msg("abandoned branch"))
        .unwrap();
    let abandoned_leaf = session.leaf_id().unwrap().to_owned();
    // Branch back to the root and continue on a new path.
    session.branch(&first).unwrap();
    session.append_message(user_msg("current branch")).unwrap();

    let collected = pir::compaction::collect_entries_for_branch_summary(
        &session,
        Some(&abandoned_leaf),
        session.leaf_id().unwrap(),
    );
    assert_eq!(
        collected.common_ancestor_id.as_deref(),
        Some(first.as_str())
    );
    let texts: Vec<String> = collected
        .entries
        .iter()
        .filter_map(|entry| match entry {
            FileEntry::Message(inner) => match &inner.message {
                StoredMessage::User(user) => match &user.content[0] {
                    ContentBlock::Text(text) => Some(text.text.clone()),
                    _ => None,
                },
                _ => None,
            },
            _ => None,
        })
        .collect();
    assert_eq!(
        texts,
        vec!["abandoned branch".to_owned()],
        "only the abandoned path is summarized"
    );

    let captured: Arc<Mutex<Vec<Context>>> = Arc::new(Mutex::new(Vec::new()));
    let result = pir::compaction::generate_branch_summary(
        &collected.entries,
        &test_model(36_384, 8_192),
        complete_with("branch checkpoint", captured.clone()),
        None,
        false,
        16_384,
    )
    .await;
    assert!(result.error.is_none(), "{result:?}");
    let summary = result.summary.expect("summary");
    assert!(summary.starts_with("The user explored a different conversation branch"));
    assert!(summary.contains("branch checkpoint"));
    let prompt = prompt_text_of(&captured.lock().unwrap()[0]);
    assert!(prompt.contains("[User]: abandoned branch"));
    assert!(
        !prompt.contains("shared root"),
        "the shared root is not summarized"
    );

    std::fs::remove_dir_all(&dir).ok();
}

// ---------------------------------------------------------------------------
// 4. SessionManager lock scopes: append handle, rewrites, fsync
// ---------------------------------------------------------------------------

/// Write a v2 (pre-tree-id) session: a header plus `entries` message lines
/// WITHOUT ids/parentIds — exactly the shape `migrateV1ToV2` repairs.
fn write_v2_session(path: &Path, entries: usize) {
    use std::io::Write as _;
    let mut file = std::fs::File::create(path).unwrap();
    writeln!(
        file,
        "{{\"type\":\"session\",\"version\":2,\"id\":\"22222222-3333-4444-5555-666666666666\",\"timestamp\":\"2025-01-01T00:00:00.000Z\",\"cwd\":\"C:\\repo\"}}"
    )
    .unwrap();
    for index in 0..entries {
        writeln!(
            file,
            "{{\"type\":\"message\",\"id\":\"m{index:07}\",\"parentId\":{},\"timestamp\":\"2025-01-01T00:00:01.000Z\",\"message\":{{\"role\":\"user\",\"content\":\"prompt {index}\",\"timestamp\":1}}}}",
            if index == 0 {
                "null".to_owned()
            } else {
                format!("\"m{:07}\"", index - 1)
            }
        )
        .unwrap();
    }
}

/// A v2→v3 migration REWRITES the same path while a reader polls it: the
/// published file must always be one COMPLETE parseable transcript — never
/// missing (the removed `remove_file` fallback left that window) and never
/// half-written.
///
/// Windows sharing semantics make a rewrite legitimately fail while a reader
/// holds its own read handle, so the migrating open is retried; what is
/// asserted unconditionally is that the file EXISTS at every observable
/// moment and that whatever it holds is a complete transcript.
#[test]
fn rewrite_publishes_atomically_while_a_reader_polls() {
    let dir = temp_dir("rewrite-atomic");
    let path = dir.join("v2.jsonl");
    write_v2_session(&path, 300);
    let line_count = 301;

    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let saw_published = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let reader = {
        let path = path.clone();
        let stop = Arc::clone(&stop);
        let saw_published = Arc::clone(&saw_published);
        std::thread::spawn(move || {
            while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                match std::fs::read_to_string(&path) {
                    Ok(content) => {
                        assert!(
                            !content.is_empty(),
                            "a concurrent reader must never see an empty session"
                        );
                        // Every line is complete JSON: no partially written
                        // transcript is ever visible.
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
                        saw_published.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                        panic!("the session file vanished during a rewrite: {error}")
                    }
                    // Sharing violations are expected on Windows while the
                    // writer holds the file; they are not a torn read.
                    Err(_) => {}
                }
            }
        })
    };

    // The v2 open migrates and rewrites the same path. A Windows sharing
    // violation against the reader's handle is a legitimate transient
    // failure (the live file stays intact); retry until it gets through.
    let mut session = None;
    for _ in 0..200 {
        match SessionManager::open(&path) {
            Ok(opened) => {
                session = Some(opened);
                break;
            }
            Err(_) => std::thread::sleep(std::time::Duration::from_millis(5)),
        }
    }
    let session = session.expect("the migrating open eventually succeeds");
    assert_eq!(session.entries().len(), 300);
    assert_eq!(
        session.header().and_then(|header| header.version),
        Some(CURRENT_SESSION_VERSION)
    );
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    reader.join().expect("reader thread");
    assert!(
        saw_published.load(std::sync::atomic::Ordering::Relaxed) > 0,
        "the reader observed complete transcripts"
    );

    // No temp file survives a successful rewrite.
    let leftovers = temp_leftovers(&dir);
    assert!(leftovers.is_empty(), "leftover temp files: {leftovers:?}");

    // The published file is the migrated v3 transcript.
    let published = std::fs::read_to_string(&path).unwrap();
    let lines: Vec<serde_json::Value> = published
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(lines.len(), line_count);
    assert_eq!(lines[0]["version"], CURRENT_SESSION_VERSION);

    std::fs::remove_dir_all(&dir).ok();
}

/// Two managers migrating the SAME v2 file concurrently must never publish a
/// mixture of their transcripts, and their temp files must never collide.
///
/// This is the same-path rewrite race: the pre-fix fixed `.tmp` name was
/// shared, so the two writers' bytes could interleave in the temp file and the
/// last rename published a mixed transcript. A writer may lose the race to a
/// Windows sharing violation (the live file stays intact and complete); the
/// assertions are on what survives, and on the temp files never mixing.
#[test]
fn concurrent_rewrites_of_one_path_never_mix_temp_contents() {
    let dir = temp_dir("rewrite-concurrent");
    let path = dir.join("v2-concurrent.jsonl");
    write_v2_session(&path, 60);

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

    // The surviving file is ONE complete, parseable transcript — never a
    // mixture of two writers' bytes, and never a missing file.
    let published = std::fs::read_to_string(&path).expect("the session file survives");
    let lines: Vec<serde_json::Value> = published
        .lines()
        .map(|line| {
            serde_json::from_str(line).unwrap_or_else(|error| panic!("torn line {line:?}: {error}"))
        })
        .collect();
    assert_eq!(lines.len(), 61, "header + 60 entries");
    assert_eq!(lines[0]["version"], CURRENT_SESSION_VERSION);
    let ids: Vec<&str> = lines[1..]
        .iter()
        .map(|line| line["id"].as_str().unwrap())
        .collect();
    let mut unique: Vec<&str> = ids.clone();
    unique.sort_unstable();
    unique.dedup();
    assert_eq!(unique.len(), ids.len(), "no duplicated entry line");

    // No temp file survives either rewrite.
    let leftovers = temp_leftovers(&dir);
    assert!(leftovers.is_empty(), "leftover temp files: {leftovers:?}");

    std::fs::remove_dir_all(&dir).ok();
}

/// The rewrite temp files left in `dir` (a successful rewrite removes its own).
fn temp_leftovers(dir: &Path) -> Vec<String> {
    std::fs::read_dir(dir)
        .unwrap()
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| name.contains(".tmp."))
        .collect()
}

/// Appends through the cached handle land at the CURRENT end of the file even
/// when another manager appended to the same path in between: the length probe
/// reopens by path, so no write is lost or overwritten.
///
/// This is the append-handle half of the lock scope: two handles on one path
/// (the host's session manager plus a resumed/second reader) must never
/// truncate each other's bytes.
#[test]
fn interleaved_appends_from_two_managers_are_all_preserved() {
    let dir = temp_dir("append-interleaved");
    let mut writer = SessionManager::create("C:/repo", &dir).unwrap();
    writer.append_message(user_msg("seed")).unwrap();
    let path = writer.path().to_path_buf();

    // A second manager opens the same path and appends (an external writer).
    let mut other = SessionManager::open(&path).unwrap();
    other.append_message(user_msg("from other")).unwrap();
    other.sync_now().unwrap();
    drop(other);

    // The first manager's cached handle was opened at the OLD length; the
    // append must not write over the other writer's line.
    writer.append_message(user_msg("after other")).unwrap();
    writer.sync_now().unwrap();

    let reopened = SessionManager::open(&path).unwrap();
    let texts: Vec<String> = reopened
        .entries()
        .iter()
        .filter_map(|entry| match entry {
            FileEntry::Message(inner) => match &inner.message {
                StoredMessage::User(user) => user.content.first().and_then(|block| match block {
                    ContentBlock::Text(text) => Some(text.text.clone()),
                    _ => None,
                }),
                _ => None,
            },
            _ => None,
        })
        .collect();
    assert_eq!(
        texts,
        vec![
            "seed".to_owned(),
            "from other".to_owned(),
            "after other".to_owned()
        ],
        "neither writer's line may be lost"
    );

    std::fs::remove_dir_all(&dir).ok();
}

// ---------------------------------------------------------------------------
// D5: compaction appends run off the runtime thread
// ---------------------------------------------------------------------------

/// The compaction append builds the current system message from the whole
/// projection and fsyncs the first write; `append_compaction_off_thread` must
/// run both on the blocking pool so a slow disk cannot park a runtime worker.
#[tokio::test]
async fn compaction_append_runs_off_the_runtime_thread() {
    let dir = temp_dir("compaction-append-off-thread");
    let mut manager = SessionManager::create(".", &dir).expect("create session");
    manager
        .append_message(user_msg("seed"))
        .expect("seed message");
    manager.stall_next_append_for_test(Duration::from_millis(800));
    let storage = Arc::new(tokio::sync::Mutex::new(manager));

    let appending = tokio::spawn({
        let storage = Arc::clone(&storage);
        async move {
            SessionManager::append_compaction_off_thread(
                &storage, "summary", None, 10, None, None, false,
            )
            .await
        }
    });
    // Current-thread runtime: a synchronous append would keep this timer from
    // firing for the whole stall.
    let probe = tokio::time::timeout(
        Duration::from_millis(200),
        tokio::time::sleep(Duration::from_millis(40)),
    )
    .await;
    assert!(
        probe.is_ok(),
        "the runtime must stay responsive while the compaction append is stalled"
    );
    assert!(
        !appending.is_finished(),
        "the append should still be inside the stalled blocking task"
    );

    let entry_id = appending
        .await
        .expect("append task join")
        .expect("compaction append");
    assert!(!entry_id.is_empty());
    std::fs::remove_dir_all(&dir).ok();
}
