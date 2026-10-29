//! W17-B: the on-disk size of one large `bash` tool result.
//!
//! A real ConPTY run (`scripts/tui_conpty/out/w16k-final/
//! s49_long_output_spill_expand`, written up in the W16 ConPTY lane logs under `logs/w18-help/`)
//! measured one `toolResult` JSONL line at 260 559 B, storing the same 5 000-row
//! body three times:
//!
//! | field | bytes | what it is |
//! |---|---|---|
//! | `content` | 51 337 | retained tail (1 828 rows) + `[Showing lines …]` footer |
//! | `details.truncation.content` | 51 183 | the same tail, footer excluded |
//! | `structuredContent.output` | 140 000 | the FULL stream the model never sees |
//!
//! This suite pins the two invariants the fix rests on:
//!
//! 1. **The session record keeps exactly two of them.** `content` (the
//!    model-facing body every reader uses) and `details.truncation` (upstream
//!    `BashToolDetails` parity, whose counters drive the TUI truncation
//!    warning) survive the write; `structuredContent` — the largest copy, read
//!    only by live programmatic callers and absent from upstream's persisted
//!    `ToolResultMessage` — is dropped by
//!    `session_manager::types::session_tool_result`.
//! 2. **Every consumer still sees what it needs**: the model text is
//!    byte-identical to the live result, the TUI truncation notice renders the
//!    same wording, `ctrl+o` expansion has the whole retained tail, session
//!    replay and `/export` produce the same body, and compaction summarizes
//!    from the same text.
//!
//! A test fails if the full body disappears from where something needs it:
//! `live_result_keeps_the_structured_body_for_programmatic_callers` asserts the
//! live (pre-write) value still carries `structuredContent`, so deleting the
//! copy at the RESULT-construction site instead of at the session writer is
//! caught.

use pir::agent::tool_batch::ToolResult;
use pir::ai::types::{
    ContentBlock, Message, TextContent, ToolResultMessage, ToolResultRole, UserMessage, UserRole,
};
#[cfg(feature = "tui")]
use pir::coding_agent::truncate::format_size;
use pir::coding_agent::truncate::{
    format_shell_output, truncate_tail, TruncationResult, DEFAULT_MAX_BYTES, DEFAULT_MAX_LINES,
};
use pir::session_manager::types::StoredMessage;
use pir::session_manager::{FileEntry, MessageEntry, SessionHeader, SessionManager};

// ---------------------------------------------------------------------------
// The measured shape, reproduced without a shell
// ---------------------------------------------------------------------------

/// Rows in the real run's body.
const ROWS: usize = 5_000;
/// The measured retained-row count (the 50 KB byte cap fired first).
const MEASURED_RETAINED_ROWS: usize = 1_828;
/// The run's first retained row (5 000 - 1 828 + 1).
const MEASURED_FIRST_RETAINED_ROW: usize = 3_173;
/// The measured sizes from the run's session file, byte for byte.
const MEASURED_TRUNCATION_CONTENT_BYTES: usize = 51_183;
const MEASURED_STRUCTURED_OUTPUT_BYTES: usize = 140_000;
const MEASURED_LINE_BYTES: usize = 260_559;

/// One payload row: `s49 row NNNN: payload line` — 26 bytes + CRLF, the same
/// shape the scenario's python subprocess printed on Windows.
fn row(index: usize) -> String {
    format!("s49 row {index:04}: payload line")
}

/// The full stream the command printed (rows 1..=5000), CRLF-separated like
/// the real run: 5 000 x 28 B = 140 000 B.
fn full_body() -> String {
    let mut text = String::with_capacity(MEASURED_STRUCTURED_OUTPUT_BYTES);
    for index in 1..=ROWS {
        text.push_str(&row(index));
        text.push_str("\r\n");
    }
    text
}

/// The result the bash tool builds for that run, shaped exactly like
/// `finalize_shell_result` (src/coding_agent/tools/bash.rs): `content` is
/// `format_shell_output` over the tail truncation, `details` is
/// `{truncation, fullOutputPath}`, and `structuredContent` is the full body
/// under the 1 MiB programmatic cap.
fn measured_result(full: &str) -> ToolResult {
    let truncation = truncate_tail(full, DEFAULT_MAX_LINES, DEFAULT_MAX_BYTES);
    assert!(truncation.truncated, "the fixture must trip the caps");
    // The run's capture evicted a prefix into a spill file, so the footer
    // advertises one; the exact path is irrelevant to the size shape.
    let full_path = Some("/tmp/pir-shell-32364-0-abc".to_owned());
    let rendered = format_shell_output(&truncation, full_path.as_deref(), "(no output)");
    let mut details = serde_json::Map::new();
    details.insert("truncation".to_owned(), truncation.to_details_json());
    if let Some(path) = &full_path {
        details.insert("fullOutputPath".to_owned(), serde_json::json!(path));
    }
    let mut result = ToolResult::ok(rendered);
    result.details = Some(serde_json::Value::Object(details));
    result.structured_content = Some(serde_json::json!({
        "output": full,
        "truncated": false,
        "exit_code": 0,
        "wall_time_seconds": 0.4,
    }));
    result
}

/// The tool-result message the agent loop builds from that result
/// (`agent_loop::to_tool_result_message`).
fn tool_result_message(result: &ToolResult) -> ToolResultMessage {
    ToolResultMessage {
        role: ToolResultRole::ToolResult,
        tool_call_id: "call_bash_1".into(),
        tool_name: "bash".into(),
        content: vec![ContentBlock::Text(TextContent {
            text: result.content.clone(),
            text_signature: None,
        })],
        details: result.details.clone(),
        usage: None,
        added_tool_names: Vec::new(),
        structured_content: result.structured_content.clone(),
        nested_calls: None,
        is_error: result.is_error,
        duration_ms: Some(410),
        timestamp: 1_760_000_000_000,
    }
}

fn text_of(message: &ToolResultMessage) -> String {
    message
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text(text) => Some(text.text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn user_message(text: &str) -> Message {
    Message::User(UserMessage {
        role: UserRole::User,
        content: vec![ContentBlock::Text(TextContent {
            text: text.to_owned(),
            text_signature: None,
        })],
        tools_added: None,
        tools_removed: None,
        sections: None,
        timestamp: 1_760_000_000_000,
    })
}

/// The assistant message that requested the bash call, so `/export` renders
/// the result inside a tool block (`render_tool_call_html` needs the call).
fn assistant_tool_call(call_id: &str) -> Message {
    Message::Assistant(pir::ai::types::AssistantMessage {
        content: vec![ContentBlock::ToolCall(pir::ai::types::ToolCall {
            id: call_id.to_owned(),
            name: "bash".into(),
            arguments: [(
                "command".to_owned(),
                serde_json::json!("python -c 'for i in range(1, 5001): print(...)'"),
            )]
            .into_iter()
            .collect(),
            thought_signature: None,
            namespace: None,
            arguments_error: None,
        })],
        provider: "anthropic".into(),
        model: "claude-sonnet-4".into(),
        stop_reason: pir::ai::types::StopReason::ToolUse,
        timestamp: 1_760_000_000_000,
        ..Default::default()
    })
}

fn temp_dir(label: &str) -> std::path::PathBuf {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or(0);
    let dir = std::env::temp_dir().join(format!(
        "pir-w17b-tool-result-size-{label}-{}-{stamp}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

/// Write the tool result through the real session writer and return the
/// manager, its file path, and the single `toolResult` JSONL line.
fn session_line(result: &ToolResult) -> (SessionManager, std::path::PathBuf, String) {
    let dir = temp_dir("session");
    let mut manager = SessionManager::create(&dir.to_string_lossy(), &dir).expect("create session");
    manager
        .append_message(user_message("print the payload"))
        .expect("append user");
    manager
        .append_message(assistant_tool_call("call_bash_1"))
        .expect("append assistant");
    manager
        .append_message(Message::ToolResult(tool_result_message(result)))
        .expect("append tool result");
    let path = manager.path().to_path_buf();
    let text = std::fs::read_to_string(&path).expect("read session file");
    let line = text
        .lines()
        .find(|line| line.contains("\"role\":\"toolResult\""))
        .expect("the session file must carry the tool result line")
        .to_owned();
    (manager, path, line)
}

// ---------------------------------------------------------------------------
// 1. The measured baseline: three copies live, two on disk
// ---------------------------------------------------------------------------

/// Pin the measured shape AND the fix: the live result carries all three
/// copies, the session line keeps two.
#[test]
fn session_line_drops_the_structured_copy_and_keeps_the_other_two() {
    let full = full_body();
    assert_eq!(
        full.len(),
        MEASURED_STRUCTURED_OUTPUT_BYTES,
        "the fixture reproduces the run's 140 000 B full body"
    );
    let result = measured_result(&full);
    let live = tool_result_message(&result);

    // --- live result: all three copies ---
    let live_text = text_of(&live);
    let live_details = live.details.as_ref().expect("live details");
    let live_truncation_content = live_details["truncation"]["content"]
        .as_str()
        .expect("live details.truncation.content");
    let live_structured = live
        .structured_content
        .as_ref()
        .and_then(|structured| structured.get("output"))
        .and_then(serde_json::Value::as_str)
        .expect("live structuredContent.output");

    assert_eq!(live_structured.len(), MEASURED_STRUCTURED_OUTPUT_BYTES);
    assert_eq!(
        live_truncation_content.len(),
        MEASURED_TRUNCATION_CONTENT_BYTES,
        "the fixture reproduces the run's 1 828-row / 51 183 B retained tail"
    );
    assert_eq!(
        live_details["truncation"]["outputLines"],
        MEASURED_RETAINED_ROWS
    );
    assert_eq!(
        live_text.find("\n\n["),
        Some(MEASURED_TRUNCATION_CONTENT_BYTES),
        "the model-facing text is the retained tail plus the footer"
    );
    assert_eq!(
        live_text.matches(row(4_500).as_str()).count(),
        1,
        "a retained row appears once in the model-facing text"
    );

    // --- session line: the structured copy is gone, the other two remain ---
    let (_manager, _path, line) = session_line(&result);
    let message: serde_json::Value =
        serde_json::from_str(&line).expect("the session line must be JSON");
    let stored = message
        .get("message")
        .expect("the session line wraps the message");
    assert!(
        stored.get("structuredContent").is_none(),
        "the session record must not carry the full-body structuredContent \
         copy: {}",
        &line[..line.len().min(400)]
    );
    assert_eq!(
        stored["content"][0]["text"].as_str().map(str::len),
        Some(live_text.len()),
        "the model-facing content survives the write unchanged"
    );
    assert_eq!(
        stored["details"]["truncation"]["content"]
            .as_str()
            .map(str::len),
        Some(MEASURED_TRUNCATION_CONTENT_BYTES),
        "details.truncation (upstream BashToolDetails parity) survives the write"
    );
    assert!(
        stored["details"]["fullOutputPath"].is_string(),
        "the advertised full-output path survives the write"
    );

    // The measured line was 260 559 B; dropping the 140 000 B copy leaves the
    // ~120 KB shape. Assert the saving, not an exact byte count (escaping and
    // the footer path also contribute).
    assert!(
        MEASURED_LINE_BYTES - line.len() >= MEASURED_STRUCTURED_OUTPUT_BYTES - 2_000,
        "the session line must shrink by roughly the structured copy \
         (line {} B, measured {} B)",
        line.len(),
        MEASURED_LINE_BYTES
    );
    assert!(
        line.len() < 160_000,
        "the stored line stays in the ~120 KB shape: {} B",
        line.len()
    );
    // The retained tail appears exactly twice: model text and details.
    assert_eq!(
        line.matches(row(4_500).as_str()).count(),
        2,
        "the retained tail appears once as content and once as \
         details.truncation"
    );
    assert_eq!(
        line.matches(row(MEASURED_FIRST_RETAINED_ROW - 1).as_str())
            .count(),
        0,
        "the truncated-away head must not be stored in the session line"
    );
}

/// The retention is a SESSION-WRITE rule, not a result-shape change: a live
/// `ToolResult` (and the live message the agent loop builds from it) still
/// carry `structuredContent`, because codemode scripts and `tool_result`
/// handlers read it there.
///
/// This is the guard the task asks for: it fails if the copy is deleted at the
/// result-construction site instead of at the session writer.
#[test]
fn live_result_keeps_the_structured_body_for_programmatic_callers() {
    let full = full_body();
    let result = measured_result(&full);
    let structured = result
        .structured_content
        .as_ref()
        .expect("a live bash result must carry structuredContent");
    assert_eq!(
        structured["output"].as_str().map(str::len),
        Some(MEASURED_STRUCTURED_OUTPUT_BYTES),
        "programmatic callers get the full body"
    );
    let live = tool_result_message(&result);
    assert!(
        live.structured_content.is_some(),
        "the live message keeps structuredContent; only the session writer \
         clears it"
    );
}

// ---------------------------------------------------------------------------
// 2. Consumer invariants
// ---------------------------------------------------------------------------

/// The model's next request sees exactly the live text: adapters serialize
/// only `content` blocks, and the stored message's content is byte-identical.
#[test]
fn model_facing_text_is_identical_after_the_write() {
    let full = full_body();
    let result = measured_result(&full);
    let live = tool_result_message(&result);
    let stored = StoredMessage::from(Message::ToolResult(live.clone()));
    let StoredMessage::ToolResult(stored) = &stored else {
        panic!("a tool-result message must store as a tool result");
    };

    // What a provider adapter sends: the text blocks of `content`.
    let sent: String = live
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text(text) => Some(text.text.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(
        text_of(stored),
        sent,
        "the replayed model-facing text must be byte-identical"
    );
    // ... and the round trip through the stored message reproduces the live
    // message for the model: same content, same details, minus the structured
    // copy.
    let replayed: Message = Message::from(StoredMessage::ToolResult(stored.clone()));
    let Message::ToolResult(replayed) = replayed else {
        panic!("round trip must keep the role");
    };
    assert_eq!(replayed.content, live.content);
    assert_eq!(replayed.details, live.details);
    assert_eq!(replayed.structured_content, None);
    assert_eq!(replayed.tool_call_id, live.tool_call_id);
    assert_eq!(replayed.is_error, live.is_error);
}

/// The TUI truncation notice and `ctrl+o` body read `content` and the
/// `details.truncation` COUNTERS — never `details.truncation.content` — so the
/// stored shape renders exactly like the live one.
#[cfg(feature = "tui")]
#[test]
fn tui_reads_content_and_counters_not_the_duplicated_tail() {
    use pir::tui::messages::{shell_truncation_warning, strip_shell_footer};

    let full = full_body();
    let result = measured_result(&full);
    let live = tool_result_message(&result);
    let stored = StoredMessage::from(Message::ToolResult(live.clone()));
    let StoredMessage::ToolResult(stored) = &stored else {
        panic!("stored tool result");
    };
    let live_details = live.details.as_ref().expect("live details");
    let stored_details = stored.details.as_ref().expect("stored details");

    // The collapsed/expanded body: the footer is stripped from `content`, and
    // the remaining rows match the live render exactly.
    let live_text = text_of(&live);
    let stored_text = text_of(stored);
    let live_body = strip_shell_footer(live_text.trim(), Some(live_details));
    let stored_body = strip_shell_footer(stored_text.trim(), Some(stored_details));
    assert_eq!(stored_body, live_body);
    assert!(
        stored_body.contains(&row(5_000)),
        "the last retained row is in the expandable body (ctrl+o)"
    );
    assert!(
        !stored_body.contains(&row(MEASURED_FIRST_RETAINED_ROW - 1)),
        "the truncated-away head never renders"
    );

    // The truncation notice wording (`Truncated: 1828 lines shown (50.0KB
    // limit)`) comes from the counters and is identical.
    let live_warning = shell_truncation_warning(Some(live_details));
    let stored_warning = shell_truncation_warning(Some(stored_details));
    assert_eq!(stored_warning, live_warning);
    assert!(
        stored_warning.as_deref().is_some_and(|text| {
            text.contains(&format!("{MEASURED_RETAINED_ROWS} lines shown"))
                && text.contains(&format_size(DEFAULT_MAX_BYTES))
                && text.contains("Full output:")
        }),
        "the measured notice must survive the write: {stored_warning:?}"
    );

    // Prove the counters are what the renderer needs: removing the
    // duplicated tail from details leaves the warning unchanged.
    let mut counters_only = stored_details.clone();
    if let Some(truncation) = counters_only
        .get_mut("truncation")
        .and_then(serde_json::Value::as_object_mut)
    {
        truncation.remove("content");
    }
    assert_eq!(
        shell_truncation_warning(Some(&counters_only)),
        stored_warning,
        "no TUI path reads details.truncation.content"
    );
}

/// Session replay and `/export`: the replayed message text and the HTML
/// export's tool block carry the same retained tail, and the full body is not
/// exported (the spill path is, as the upstream footer says).
#[test]
fn replay_and_export_keep_the_retained_tail() {
    let full = full_body();
    let result = measured_result(&full);
    let (_manager, path, _line) = session_line(&result);
    let dir = path.parent().expect("session dir").to_path_buf();

    // Replay through a fresh open: the tool result comes back exactly as the
    // live message's content.
    let reopened = SessionManager::open(&path).expect("reopen session");
    let replayed: Vec<Message> = reopened
        .build_session_projection()
        .expect("projection")
        .messages;
    let tool_result = replayed
        .iter()
        .find_map(|message| match message {
            Message::ToolResult(result) => Some(result),
            _ => None,
        })
        .expect("the replayed transcript carries the tool result");
    assert_eq!(
        text_of(tool_result),
        text_of(&tool_result_message(&result)),
        "replay reproduces the model-facing body byte for byte"
    );
    assert_eq!(tool_result.structured_content, None);
    let replayed_details = tool_result.details.as_ref().expect("replayed details");
    assert_eq!(
        replayed_details["truncation"]["content"]
            .as_str()
            .map(str::len),
        Some(MEASURED_TRUNCATION_CONTENT_BYTES),
        "details.truncation survives replay (offline readers get the tail)"
    );

    // `/export`: the HTML page renders the retained tail, and never the
    // dropped head.
    let html_path = dir.join("export.html");
    pir::coding_agent::export_html::export_session_to_html(&path, Some(&html_path))
        .expect("export session");
    let html = std::fs::read_to_string(&html_path).expect("read export");
    assert!(
        html.contains(&row(5_000)),
        "the export shows the retained tail"
    );
    assert!(
        !html.contains(&row(MEASURED_FIRST_RETAINED_ROW - 1)),
        "the export must not resurrect the truncated-away head"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Compaction summarizes tool results from their `content` text
/// (`compaction::serialize_conversation` -> `[Tool result]: …`, truncated by
/// `truncate_for_summary` to `TOOL_RESULT_MAX_CHARS` from the START). The
/// stored shape serializes identically — same prefix, same truncation marker.
#[test]
fn compaction_serializes_the_same_tool_result_text() {
    let full = full_body();
    let result = measured_result(&full);
    let live = tool_result_message(&result);
    let stored = StoredMessage::from(Message::ToolResult(live.clone()));
    let StoredMessage::ToolResult(stored) = &stored else {
        panic!("stored tool result");
    };
    let replayed: Message = Message::from(StoredMessage::ToolResult(stored.clone()));

    let live_serialized = pir::compaction::serialize_conversation(&[Message::ToolResult(live)]);
    let replayed_serialized = pir::compaction::serialize_conversation(&[replayed]);
    assert_eq!(
        replayed_serialized, live_serialized,
        "compaction must summarize the same tool-result text before and after \
         the session write"
    );
    // The summarizer sees the head of the retained tail (the first
    // `TOOL_RESULT_MAX_CHARS` characters) plus the standard marker.
    assert!(
        replayed_serialized.contains(&row(MEASURED_FIRST_RETAINED_ROW)),
        "the summarized text starts at the first retained row"
    );
    assert!(
        replayed_serialized.contains("more characters truncated"),
        "the summarizer's own cap still marks the cut"
    );
    assert!(
        !replayed_serialized.contains(&row(MEASURED_FIRST_RETAINED_ROW - 1)),
        "compaction never feeds the dropped head into the summarizer"
    );
}

// ---------------------------------------------------------------------------
// 3. The kept copy and the untruncated case
// ---------------------------------------------------------------------------

/// `TruncationResult::to_details_json` keeps `content` (upstream
/// `TruncationResult` parity). This test pins the decision in BOTH directions:
/// a future change that drops the tail from the details must update it
/// deliberately, because offline readers of a pi session file get the tail
/// from there.
#[test]
fn details_truncation_keeps_the_tail_for_offline_readers() {
    let full = full_body();
    let truncation: TruncationResult = truncate_tail(&full, DEFAULT_MAX_LINES, DEFAULT_MAX_BYTES);
    let details = truncation.to_details_json();
    assert_eq!(
        details["content"].as_str().map(str::len),
        Some(MEASURED_TRUNCATION_CONTENT_BYTES),
        "details.truncation.content is the upstream-parity copy of the tail"
    );
    // The counters the TUI + model budget read are all present.
    for key in [
        "truncated",
        "truncatedBy",
        "totalLines",
        "totalBytes",
        "outputLines",
        "outputBytes",
        "lastLinePartial",
        "firstLineExceedsLimit",
        "maxLines",
        "maxBytes",
    ] {
        assert!(
            details.get(key).is_some(),
            "details.truncation.{key} must stay in the upstream shape"
        );
    }
    assert_eq!(details["totalLines"], ROWS);
    assert_eq!(details["outputLines"], MEASURED_RETAINED_ROWS);
    assert_eq!(details["truncatedBy"], "bytes");
    // The same tail, footer excluded: the message text is the details copy
    // plus `\n\n[Showing lines …]`.
    let rendered = format_shell_output(&truncation, Some("/tmp/spill"), "(no output)");
    let footer_at = rendered.find("\n\n[").expect("footer");
    assert_eq!(
        &rendered[..footer_at],
        details["content"].as_str().expect("details content")
    );
    let footer = rendered[footer_at..]
        .strip_prefix("\n\n")
        .expect("the footer is separated from the tail by a blank line");
    assert!(
        footer.starts_with(&format!("[Showing lines {MEASURED_FIRST_RETAINED_ROW}-")),
        "the footer names the first retained row: {footer:?}"
    );
}

/// A small (untruncated) shell result stores no details at all, so the fix
/// changes nothing for the common case.
#[test]
fn untruncated_shell_result_stores_no_duplicate() {
    let small = "hello\nworld\n";
    let result = {
        let mut result = ToolResult::ok(small);
        result.structured_content = Some(serde_json::json!({
            "output": small,
            "truncated": false,
            "exit_code": 0,
            "wall_time_seconds": 0.1,
        }));
        result
    };
    let live = tool_result_message(&result);
    let stored = StoredMessage::from(Message::ToolResult(live));
    let StoredMessage::ToolResult(stored) = &stored else {
        panic!("stored tool result");
    };
    assert!(stored.details.is_none(), "no details below the caps");
    assert!(stored.structured_content.is_none());
    assert_eq!(text_of(stored), "hello\nworld\n");

    // A hand-built entry serializes the same way through a real file.
    let dir = temp_dir("small");
    let header = FileEntry::Session(SessionHeader::new(
        "w17b-small",
        dir.to_string_lossy(),
        "2026-10-22T00:00:00.000Z".to_owned(),
    ));
    let entry = FileEntry::Message(MessageEntry {
        id: "aaaa0001".to_owned(),
        parent_id: None,
        timestamp: "2026-10-22T00:00:01.000Z".to_owned(),
        message: StoredMessage::ToolResult(stored.clone()),
    });
    let path = dir.join("session.jsonl");
    std::fs::write(
        &path,
        format!(
            "{}\n{}\n",
            serde_json::to_string(&header).expect("header json"),
            serde_json::to_string(&entry).expect("entry json")
        ),
    )
    .expect("write session");
    let line = std::fs::read_to_string(&path).expect("read session");
    assert!(!line.contains("structuredContent"));
    let _ = std::fs::remove_dir_all(&dir);
}
