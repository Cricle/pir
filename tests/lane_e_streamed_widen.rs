//! Byte-equality tests for the file-backed streamed widen transcript.
//!
//! The print widen fallback can serialize the session's model context straight
//! from the JSONL file (`SessionMessageSource`) instead of materializing
//! `widen_to_full` + `build_session_projection`. These tests pin the exact
//! projection semantics the streaming source must reproduce:
//!
//! * every entry kind (message roles, custom message, branch summary,
//!   compaction checkpoint, context edit, excluded/kept bash records, state
//!   entries),
//! * the compaction retained range and its system-message skip,
//! * a branched leaf path where file order differs from path order,
//! * the serialized request body (streamed source vs resident slice vs value
//!   body) byte for byte.

use std::sync::Arc;

use pir::ai::api::openai::{build_openai_body, build_openai_body_streamed, SessionMessageSource};
use pir::ai::types::MessageSource;
use pir::ai::types::{
    AssistantMessage, ContentBlock, Context, Message, Model, ModelCost, ModelCostRates,
    OpenAiCompat, StreamOptions, TextContent, Tool, ToolCall, ToolResultMessage, UserMessage,
    UserRole,
};
use pir::session_manager::{BashExecutionRecord, SessionManager};

fn temp_dir(label: &str) -> std::path::PathBuf {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    let dir = std::env::temp_dir().join(format!(
        "pir-lane-e-streamed-{label}-{}-{stamp}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("fixture dir");
    dir
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
        timestamp: 0,
    })
}

fn system_message(text: &str) -> Message {
    Message::System(UserMessage {
        role: UserRole::System,
        content: vec![ContentBlock::Text(TextContent {
            text: text.to_owned(),
            text_signature: None,
        })],
        tools_added: Some(vec![Tool {
            name: "read".to_owned(),
            description: "read a file".to_owned(),
            parameters: serde_json::json!({"type": "object", "properties": {}}),
            constrained_sampling: None,
        }]),
        tools_removed: None,
        sections: None,
        timestamp: 0,
    })
}

fn assistant_message(text: &str) -> Message {
    Message::Assistant(AssistantMessage {
        content: vec![ContentBlock::Text(TextContent {
            text: text.to_owned(),
            text_signature: None,
        })],
        ..Default::default()
    })
}

fn assistant_with_tool_call(text: &str, call_id: &str) -> Message {
    Message::Assistant(AssistantMessage {
        content: vec![
            ContentBlock::Text(TextContent {
                text: text.to_owned(),
                text_signature: None,
            }),
            ContentBlock::ToolCall(ToolCall {
                id: call_id.to_owned(),
                name: "read".to_owned(),
                arguments: [("path".to_owned(), serde_json::json!("a.txt"))]
                    .into_iter()
                    .collect(),
                thought_signature: None,
                namespace: None,
                arguments_error: None,
            }),
        ],
        ..Default::default()
    })
}

fn tool_result(tool_call_id: &str, text: &str) -> Message {
    Message::ToolResult(ToolResultMessage {
        role: pir::ai::types::ToolResultRole::ToolResult,
        tool_call_id: tool_call_id.to_owned(),
        tool_name: "read".to_owned(),
        content: vec![ContentBlock::Text(TextContent {
            text: text.to_owned(),
            text_signature: None,
        })],
        details: None,
        usage: None,
        added_tool_names: Vec::new(),
        structured_content: None,
        nested_calls: None,
        is_error: false,
        duration_ms: None,
        timestamp: 0,
    })
}

fn mock_model() -> Model {
    Model {
        id: "mock-model".into(),
        name: "Mock model".into(),
        api: "openai-completions".into(),
        provider: "mock".into(),
        base_url: "http://127.0.0.1:1/v1".into(),
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
        context_window: 100_000_000,
        max_tokens: 4096,
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

/// Compaction + context edit + custom message + bash records + mid-convo
/// system message: the retention rules and every entry kind on one linear
/// leaf.
fn write_compacted_fixture() -> (std::path::PathBuf, String) {
    let dir = temp_dir("compacted");
    let mut session = SessionManager::create("C:/repo", &dir).expect("create");
    session
        .append_message(system_message("session prompt"))
        .expect("system");
    let u1 = session
        .append_message(user_message("kept question"))
        .expect("u1");
    session
        .append_message(assistant_message("kept answer"))
        .expect("a1");
    // Dropped by the compaction (before firstKeptEntryId).
    session
        .append_message(user_message("summarized question"))
        .expect("u2");
    session
        .append_message(assistant_message("summarized answer"))
        .expect("a2");
    session
        .append_compaction("earlier summarized", Some(&u1), 999, None)
        .expect("compaction");
    session
        .append_bash_execution(BashExecutionRecord {
            command: "echo hidden",
            output: "hidden output",
            exit_code: Some(0),
            cancelled: false,
            truncated: false,
            full_output_path: None,
            exclude_from_context: true,
            timestamp: 0,
        })
        .expect("excluded bash");
    session
        .append_bash_execution(BashExecutionRecord {
            command: "echo shown",
            output: "shown output",
            exit_code: Some(0),
            cancelled: false,
            truncated: false,
            full_output_path: None,
            exclude_from_context: false,
            timestamp: 0,
        })
        .expect("kept bash");
    let u3 = session
        .append_message(user_message("edited question"))
        .expect("u3");
    session
        .append_context_edit(&u3, Some(serde_json::json!("edited prompt")))
        .expect("edit");
    session
        .append_custom_message_entry(
            "test.custom",
            serde_json::json!([{ "type": "text", "text": "custom payload" }]),
            false,
            None,
        )
        .expect("custom");
    session
        .append_message(system_message("mid-convo declaration"))
        .expect("mid system");
    session
        .append_message(assistant_with_tool_call("calling", "call-1"))
        .expect("tool call");
    session
        .append_message(tool_result("call-1", "tool output"))
        .expect("tool result");
    let leaf = session.leaf_id().expect("leaf").to_owned();
    let path = session.session_file().expect("file").to_path_buf();
    drop(session);
    (path, leaf)
}

/// A branch summary on the leaf path: file order (u1, a1, u2, a2) differs
/// from path order (u1, branch summary, ...).
fn write_branched_fixture() -> (std::path::PathBuf, String) {
    let dir = temp_dir("branched");
    let mut session = SessionManager::create("C:/repo", &dir).expect("create");
    let u1 = session
        .append_message(user_message("root question"))
        .expect("u1");
    session
        .append_message(assistant_message("root answer"))
        .expect("a1");
    session
        .append_message(user_message("abandoned question"))
        .expect("u2");
    session
        .append_message(assistant_message("abandoned answer"))
        .expect("a2");
    session.branch(&u1).expect("branch");
    session
        .branch_with_summary(Some(&u1), "branched away", None, None, None)
        .expect("branch summary");
    session
        .append_message(user_message("branch question"))
        .expect("branch user");
    session
        .append_message(assistant_message("branch answer"))
        .expect("branch assistant");
    let leaf = session.leaf_id().expect("leaf").to_owned();
    let path = session.session_file().expect("file").to_path_buf();
    drop(session);
    (path, leaf)
}

fn assert_streamed_matches_projection(label: &str, path: &std::path::Path, leaf: &str) {
    let model = mock_model();
    let compat = OpenAiCompat::default();

    // Baseline: the full manager's projection (the production fallback).
    let manager = SessionManager::open(path).expect("open full");
    let projection = manager.build_session_projection().expect("projection");
    assert_eq!(manager.leaf_id(), Some(leaf), "{label}: fixture leaf");

    // Collect the streamed source and compare message-for-message.
    let source = SessionMessageSource::build_from_file(path, Some(leaf)).expect("source");
    let collected: Vec<Message> = source
        .messages()
        .map(|message| match message.expect("source message") {
            pir::ai::types::SourceMessage::Borrowed(message) => message.clone(),
            pir::ai::types::SourceMessage::Owned(message) => message,
        })
        .collect();
    assert_eq!(
        collected, projection.messages,
        "{label}: streamed source must equal the projection"
    );

    // Baseline body: the resident slice (the pre-streaming builder input).
    let baseline_context = Context {
        system_prompt: Some("base system prompt".to_owned()),
        messages: projection.messages,
        tools: None,
    };
    let options = StreamOptions::default();
    let baseline = build_openai_body_streamed(&model, &baseline_context, &options, &compat)
        .expect("baseline streamed body");
    let baseline_bytes = serde_json::to_vec(&baseline).expect("baseline bytes");

    // Value body is the same bytes on this fixture (no mid-convo collapse
    // differences are hidden by the streamed path).
    let value =
        build_openai_body(&model, &baseline_context, &options, &compat).expect("value body");
    let value_bytes = serde_json::to_vec(&value).expect("value bytes");
    assert_eq!(
        baseline_bytes, value_bytes,
        "{label}: streamed slice body must match the value body"
    );

    // Streamed body: session file as the historical prefix, empty resident
    // tail. Byte-for-byte identical to the baseline.
    let streamed_context = Context {
        system_prompt: Some("base system prompt".to_owned()),
        messages: Vec::new(),
        tools: None,
    };
    let streamed_options = StreamOptions {
        message_source: Some(Arc::new(
            SessionMessageSource::build_from_file(path, Some(leaf)).expect("streamed source"),
        )),
        ..Default::default()
    };
    let streamed =
        build_openai_body_streamed(&model, &streamed_context, &streamed_options, &compat)
            .expect("streamed body");
    assert_eq!(
        streamed.estimated_json_bytes(),
        baseline.estimated_json_bytes(),
        "{label}: estimate must match the slice path"
    );
    let streamed_bytes = serde_json::to_vec(&streamed).expect("streamed bytes");
    assert_eq!(
        streamed_bytes, baseline_bytes,
        "{label}: streamed source body must be byte-identical to the slice builder"
    );
}

#[test]
fn streamed_source_matches_projection_and_body_on_compacted_session() {
    let (path, leaf) = write_compacted_fixture();
    assert_streamed_matches_projection("compacted", &path, &leaf);
    let _ = std::fs::remove_dir_all(path.parent().expect("parent"));
}

#[test]
fn streamed_source_matches_projection_and_body_on_branched_session() {
    let (path, leaf) = write_branched_fixture();
    assert_streamed_matches_projection("branched", &path, &leaf);
    let _ = std::fs::remove_dir_all(path.parent().expect("parent"));
}

#[test]
fn streamed_source_matches_projection_on_an_empty_session() {
    let dir = temp_dir("empty");
    let mut session = SessionManager::create("C:/repo", &dir).expect("create");
    session
        .append_message(user_message("only message"))
        .expect("message");
    let leaf = session.leaf_id().expect("leaf").to_owned();
    let path = session.session_file().expect("file").to_path_buf();
    drop(session);
    assert_streamed_matches_projection("single", &path, &leaf);
    let _ = std::fs::remove_dir_all(dir);
}
