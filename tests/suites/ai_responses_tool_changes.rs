//! D4: OpenAI Responses / Codex in-place tool changes (audit #7) and the
//! OpenAI Completions index-after-id tool block reuse (audit #9).
//!
//! Upstream semantics under test:
//! - `getResponsesToolSupport` picks `changes` (mid-conversation system
//!   messages + `additional_tools`), `additions` (tool search), or `none`;
//! - `resolveTranscriptTools` decides which declarations stay in the top-level
//!   `tools` field and which are anchored in place;
//! - anchored `toolsAdded` become `additional_tools` / synthetic tool-search
//!   items even when the declaring system message has no text, and anchored
//!   `toolsRemoved` become text removal notices;
//! - empty-body declaration messages are never dropped wholesale.

use std::collections::BTreeMap;

use pir::ai::api::openai::parse_openai_sse;
use pir::ai::api::openai_responses::shared::{
    ConvertResponsesMessagesOptions, OPENAI_TOOL_CALL_PROVIDERS,
};
use pir::ai::api::openai_responses::{
    build_codex_body, convert_responses_messages, get_responses_tool_support,
    resolve_responses_request_tools, ResponsesToolSupport,
};
use pir::ai::types::{
    AssistantMessage, AssistantMessageEvent, ContentBlock, Context, Message, Model, ModelCost,
    ModelCostRates, OpenAiCompat, StreamOptions, TextContent, Tool, ToolReference, UserMessage,
    UserRole,
};
use serde_json::{json, Value};

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

fn model(api: &str, compat: Option<OpenAiCompat>) -> Model {
    Model {
        id: "gpt-5.6-sol".to_string(),
        name: "gpt-5.6-sol".to_string(),
        api: api.to_string(),
        provider: "openai".to_string(),
        base_url: "https://api.openai.com/v1".to_string(),
        model_type: None,
        reasoning: false,
        thinking_level_map: None,
        input: vec!["text".to_string()],
        input_limits: None,
        cost: ModelCost {
            rates: ModelCostRates {
                input: 0.0,
                output: 0.0,
                cache_read: 0.0,
                cache_write: 0.0,
            },
            tiers: Vec::new(),
        },
        context_window: 200_000,
        max_tokens: 16_384,
        sampling_params: None,
        sampling_params_by_thinking_level: None,
        headers: None,
        prompt_cache: None,
        thinking_token_budget_field: None,
        compat,
    }
}

fn compat(mid_convo: bool, additional: bool, search: bool) -> OpenAiCompat {
    OpenAiCompat {
        supports_mid_convo_system_messages: Some(mid_convo),
        supports_additional_tools: Some(additional),
        supports_tool_search: Some(search),
        ..OpenAiCompat::default()
    }
}

fn tool(name: &str) -> Tool {
    Tool {
        name: name.to_string(),
        description: format!("{name} tool"),
        parameters: json!({"type": "object", "properties": {}}),
        constrained_sampling: None,
    }
}

fn declaration(added: &[&str], removed: &[&str]) -> Message {
    Message::System(UserMessage {
        role: UserRole::System,
        content: Vec::new(),
        tools_added: if added.is_empty() {
            None
        } else {
            Some(added.iter().map(|name| tool(name)).collect())
        },
        tools_removed: if removed.is_empty() {
            None
        } else {
            Some(
                removed
                    .iter()
                    .map(|name| ToolReference {
                        name: (*name).to_string(),
                    })
                    .collect(),
            )
        },
        sections: None,
        timestamp: 0,
    })
}

fn user(text: &str) -> Message {
    Message::User(UserMessage {
        role: UserRole::User,
        content: vec![ContentBlock::Text(TextContent {
            text: text.to_string(),
            text_signature: None,
        })],
        tools_added: None,
        tools_removed: None,
        sections: None,
        timestamp: 1,
    })
}

fn options(mid_convo: bool, additional: bool, search: bool) -> ConvertResponsesMessagesOptions {
    ConvertResponsesMessagesOptions {
        supports_mid_convo_system_messages: mid_convo,
        supports_additional_tools: additional,
        supports_tool_search: search,
        ..Default::default()
    }
}

fn items_of_type<'a>(items: &'a [Value], kind: &str) -> Vec<&'a Value> {
    items
        .iter()
        .filter(|item| item.get("type").and_then(Value::as_str) == Some(kind))
        .collect()
}

fn tool_names(tools: &[Tool]) -> Vec<String> {
    tools.iter().map(|tool| tool.name.clone()).collect()
}

fn additional_tool_names(item: &Value) -> Vec<String> {
    item.get("tools")
        .and_then(Value::as_array)
        .map(|tools| {
            tools
                .iter()
                .filter_map(|tool| tool.get("name").and_then(Value::as_str))
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------
// getResponsesToolSupport (upstream openai-responses-shared.ts:167-174)
// ---------------------------------------------------------------------------

#[test]
fn responses_tool_support_picks_the_upstream_three_modes() {
    assert_eq!(
        get_responses_tool_support(true, true, true),
        ResponsesToolSupport::Changes
    );
    assert_eq!(
        get_responses_tool_support(true, true, false),
        ResponsesToolSupport::Changes
    );
    assert_eq!(
        get_responses_tool_support(true, false, true),
        ResponsesToolSupport::Additions
    );
    assert_eq!(
        get_responses_tool_support(false, false, true),
        ResponsesToolSupport::Additions
    );
    // `additional_tools` without mid-conversation system messages cannot
    // anchor the items, and without tool search nothing can be added in place.
    assert_eq!(
        get_responses_tool_support(false, true, false),
        ResponsesToolSupport::None
    );
    assert_eq!(
        get_responses_tool_support(false, false, false),
        ResponsesToolSupport::None
    );
}

// ---------------------------------------------------------------------------
// changes mode: every tool rides additional_tools and the prefix is stable
// ---------------------------------------------------------------------------

#[test]
fn responses_changes_mode_keeps_the_prefix_when_tools_are_added() {
    let model = model("openai-responses", Some(compat(true, true, true)));

    let context_a = Context {
        system_prompt: Some("System prompt.".to_string()),
        messages: vec![declaration(&["A"], &[]), user("first")],
        tools: Some(vec![tool("A")]),
    };
    let items_a = convert_responses_messages(
        &model,
        &context_a,
        OPENAI_TOOL_CALL_PROVIDERS,
        &options(true, true, true),
    );
    // The top-level list stays empty in changes mode; the first declaration
    // becomes the head's additional_tools item even though its body is empty.
    assert_eq!(
        resolve_responses_request_tools(&context_a, true, true, true),
        Vec::<Tool>::new()
    );
    assert_eq!(items_a.len(), 3, "{items_a:?}");
    assert_eq!(items_a[0]["type"], "additional_tools");
    assert_eq!(items_a[0]["role"], "developer");
    assert_eq!(additional_tool_names(&items_a[0]), vec!["A"]);
    assert_eq!(items_a[1]["role"], "system");
    assert_eq!(items_a[1]["content"], "System prompt.");
    assert_eq!(items_a[2]["role"], "user");

    let context_ab = Context {
        system_prompt: Some("System prompt.".to_string()),
        messages: vec![
            declaration(&["A"], &[]),
            user("first"),
            declaration(&["B"], &[]),
        ],
        tools: Some(vec![tool("A"), tool("B")]),
    };
    let items_ab = convert_responses_messages(
        &model,
        &context_ab,
        OPENAI_TOOL_CALL_PROVIDERS,
        &options(true, true, true),
    );
    assert_eq!(
        resolve_responses_request_tools(&context_ab, true, true, true),
        Vec::<Tool>::new()
    );
    assert_eq!(
        &items_ab[..items_a.len()],
        &items_a[..],
        "the existing input prefix must not change when B is declared"
    );
    let additional = items_of_type(&items_ab, "additional_tools");
    assert_eq!(additional.len(), 2, "{items_ab:?}");
    assert_eq!(additional_tool_names(additional[1]), vec!["B"]);
    // The new declaration is preceded by the incremental-tools notice.
    let notice = &items_ab[items_a.len()];
    assert_eq!(notice["role"], "system");
    assert!(
        notice["content"]
            .as_str()
            .is_some_and(|text| text.contains("incremental tools update")),
        "{notice}"
    );
}

// ---------------------------------------------------------------------------
// additions mode: top-level list freezes, later tools use synthetic search
// ---------------------------------------------------------------------------

#[test]
fn responses_additions_mode_freezes_top_level_tools_and_anchors_additions() {
    let model = model("openai-responses", Some(compat(true, false, true)));

    let context_a = Context {
        system_prompt: Some("System prompt.".to_string()),
        messages: vec![declaration(&["A"], &[]), user("first")],
        tools: Some(vec![tool("A")]),
    };
    let request_a = resolve_responses_request_tools(&context_a, true, false, true);
    assert_eq!(tool_names(&request_a), vec!["A"]);
    let items_a = convert_responses_messages(
        &model,
        &context_a,
        OPENAI_TOOL_CALL_PROVIDERS,
        &options(true, false, true),
    );
    // The leading declaration is folded into the request tools.
    assert_eq!(items_a.len(), 2, "{items_a:?}");
    assert_eq!(items_a[0]["role"], "system");
    assert_eq!(items_a[1]["role"], "user");
    assert!(items_of_type(&items_a, "tool_search_output").is_empty());

    let context_ab = Context {
        system_prompt: Some("System prompt.".to_string()),
        messages: vec![
            declaration(&["A"], &[]),
            user("first"),
            declaration(&["B"], &[]),
        ],
        tools: Some(vec![tool("A"), tool("B")]),
    };
    let request_ab = resolve_responses_request_tools(&context_ab, true, false, true);
    assert_eq!(
        tool_names(&request_ab),
        vec!["A"],
        "the top-level tools prefix must stay frozen"
    );
    let items_ab = convert_responses_messages(
        &model,
        &context_ab,
        OPENAI_TOOL_CALL_PROVIDERS,
        &options(true, false, true),
    );
    assert_eq!(&items_ab[..items_a.len()], &items_a[..]);
    let outputs = items_of_type(&items_ab, "tool_search_output");
    assert_eq!(outputs.len(), 1, "{items_ab:?}");
    let declared = outputs[0]["tools"].as_array().expect("tools array");
    assert_eq!(declared.len(), 1);
    assert_eq!(declared[0]["name"], "B");
    assert_eq!(declared[0]["defer_loading"], true);
    let calls = items_of_type(&items_ab, "tool_search_call");
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0]["call_id"], outputs[0]["call_id"]);
    assert!(items_of_type(&items_ab, "additional_tools").is_empty());
}

// ---------------------------------------------------------------------------
// none mode: the top-level list is rebuilt from the current tool state
// ---------------------------------------------------------------------------

#[test]
fn responses_none_mode_rebuilds_the_full_tool_list() {
    let model = model("openai-responses", Some(compat(false, false, false)));
    let context = Context {
        system_prompt: Some("System prompt.".to_string()),
        messages: vec![
            declaration(&["A"], &[]),
            user("first"),
            declaration(&["B"], &[]),
        ],
        tools: Some(vec![tool("A"), tool("B")]),
    };
    let request = resolve_responses_request_tools(&context, false, false, false);
    assert_eq!(tool_names(&request), vec!["A", "B"]);
    let items = convert_responses_messages(
        &model,
        &context,
        OPENAI_TOOL_CALL_PROVIDERS,
        &options(false, false, false),
    );
    assert_eq!(items.len(), 2, "{items:?}");
    assert_eq!(items[0]["role"], "system");
    assert_eq!(items[1]["role"], "user");
    assert!(items_of_type(&items, "additional_tools").is_empty());
    assert!(items_of_type(&items, "tool_search_output").is_empty());
}

// ---------------------------------------------------------------------------
// removal notices (upstream renderToolRemovalNotice / unavailableTools)
// ---------------------------------------------------------------------------

#[test]
fn responses_changes_mode_announces_removed_tools_as_text() {
    let model = model("openai-responses", Some(compat(true, true, true)));
    let context = Context {
        system_prompt: Some("System prompt.".to_string()),
        messages: vec![
            declaration(&["A"], &[]),
            user("first"),
            declaration(&[], &["A"]),
            // B is both re-declared and removed: only true removals are
            // announced, and this one is a same-name redefinition.
            declaration(&["B"], &["B"]),
            declaration(&["C"], &[]),
            user("second"),
        ],
        tools: Some(vec![tool("B"), tool("C")]),
    };
    let items = convert_responses_messages(
        &model,
        &context,
        OPENAI_TOOL_CALL_PROVIDERS,
        &options(true, true, true),
    );
    let notices: Vec<&str> = items
        .iter()
        .filter_map(|item| item.get("content").and_then(Value::as_str))
        .filter(|content| content.contains("no longer available"))
        .collect();
    assert_eq!(notices.len(), 1, "{items:?}");
    assert!(notices[0].contains("- A"), "{}", notices[0]);
    assert!(
        !notices[0].contains("- B"),
        "a same-name redefinition is not a removal: {}",
        notices[0]
    );
    // After the removal, the next incremental notice restates what is still
    // unavailable (upstream keeps removed tools declared but announced).
    let incremental: Vec<&str> = items
        .iter()
        .filter_map(|item| item.get("content").and_then(Value::as_str))
        .filter(|content| content.contains("Still unavailable"))
        .collect();
    assert_eq!(incremental.len(), 2, "{items:?}");
    for notice in &incremental {
        assert!(notice.contains("Still unavailable: A."), "{}", notice);
    }
    let additional = items_of_type(&items, "additional_tools");
    let names: Vec<String> = additional
        .iter()
        .flat_map(|item| additional_tool_names(item))
        .collect();
    assert_eq!(names, vec!["A", "B", "C"]);
}

#[test]
fn responses_empty_body_declarations_still_emit_tool_items() {
    // The declared system messages carry no text at all. Both in-place modes
    // must still send their tool payloads.
    let changes = model("openai-responses", Some(compat(true, true, true)));
    let context = Context {
        system_prompt: None,
        messages: vec![declaration(&["A"], &[]), declaration(&["B"], &[])],
        tools: Some(vec![tool("A"), tool("B")]),
    };
    let items = convert_responses_messages(
        &changes,
        &context,
        OPENAI_TOOL_CALL_PROVIDERS,
        &options(true, true, true),
    );
    let additional = items_of_type(&items, "additional_tools");
    assert_eq!(additional.len(), 2, "{items:?}");
    assert_eq!(additional_tool_names(additional[1]), vec!["B"]);
}

#[test]
fn responses_system_message_body_and_tool_changes_are_both_sent() {
    let model = model("openai-responses", Some(compat(true, true, true)));
    let mut update = declaration(&["B"], &["A"]);
    if let Message::System(system) = &mut update {
        system.content = vec![ContentBlock::Text(TextContent {
            text: "Loadout changed.".to_string(),
            text_signature: None,
        })];
    }
    let context = Context {
        system_prompt: Some("System prompt.".to_string()),
        messages: vec![declaration(&["A"], &[]), user("first"), update],
        tools: Some(vec![tool("B")]),
    };
    let items = convert_responses_messages(
        &model,
        &context,
        OPENAI_TOOL_CALL_PROVIDERS,
        &options(true, true, true),
    );
    // The message body survives alongside its tool changes; the removal notice
    // is appended after the content.
    let combined = items
        .iter()
        .filter_map(|item| item.get("content").and_then(Value::as_str))
        .find(|content| content.starts_with("Loadout changed."))
        .expect("message body item");
    assert!(combined.contains("no longer available"), "{combined}");
    assert!(combined.contains("- A"), "{combined}");
    // The addition is still declared in place.
    let additional = items_of_type(&items, "additional_tools");
    assert_eq!(additional.len(), 2, "{items:?}");
    assert_eq!(additional_tool_names(additional[1]), vec!["B"]);
}

// ---------------------------------------------------------------------------
// Codex body: same tool support modes
// ---------------------------------------------------------------------------

#[test]
fn codex_body_uses_additional_tools_in_changes_mode() {
    let model = model("openai-codex-responses", Some(compat(true, true, true)));
    let context_a = Context {
        system_prompt: Some("Codex prompt.".to_string()),
        messages: vec![declaration(&["A"], &[]), user("first")],
        tools: Some(vec![tool("A")]),
    };
    let body_a = build_codex_body(&model, &context_a, &StreamOptions::default()).expect("body");
    assert!(body_a.get("tools").is_none(), "{body_a}");
    assert_eq!(body_a["instructions"], "Codex prompt.");
    let input_a = body_a["input"].as_array().expect("input");
    let additional_a = items_of_type(input_a, "additional_tools");
    assert_eq!(additional_a.len(), 1, "{body_a}");
    assert_eq!(additional_tool_names(additional_a[0]), vec!["A"]);

    let context_ab = Context {
        system_prompt: Some("Codex prompt.".to_string()),
        messages: vec![
            declaration(&["A"], &[]),
            user("first"),
            declaration(&["B"], &[]),
        ],
        tools: Some(vec![tool("A"), tool("B")]),
    };
    let body_ab = build_codex_body(&model, &context_ab, &StreamOptions::default()).expect("body");
    assert!(body_ab.get("tools").is_none(), "{body_ab}");
    let input_ab = body_ab["input"].as_array().expect("input");
    assert_eq!(
        &input_ab[..input_a.len()],
        input_a,
        "the codex input prefix must not change when B is declared"
    );
    let additional_ab = items_of_type(input_ab, "additional_tools");
    assert_eq!(additional_ab.len(), 2, "{body_ab}");
    assert_eq!(additional_tool_names(additional_ab[1]), vec!["B"]);
}

#[test]
fn codex_body_freezes_top_level_tools_in_additions_mode() {
    let model = model("openai-codex-responses", Some(compat(true, false, true)));
    let context = Context {
        system_prompt: Some("Codex prompt.".to_string()),
        messages: vec![
            declaration(&["A"], &[]),
            user("first"),
            declaration(&["B"], &[]),
        ],
        tools: Some(vec![tool("A"), tool("B")]),
    };
    let body = build_codex_body(&model, &context, &StreamOptions::default()).expect("body");
    let top_level = body["tools"].as_array().expect("top-level tools");
    assert_eq!(top_level.len(), 1, "{body}");
    assert_eq!(top_level[0]["name"], "A");
    let input = body["input"].as_array().expect("input");
    let outputs = items_of_type(input, "tool_search_output");
    assert_eq!(outputs.len(), 1, "{body}");
    assert_eq!(outputs[0]["tools"][0]["name"], "B");
    assert_eq!(outputs[0]["tools"][0]["defer_loading"], true);
}

// ---------------------------------------------------------------------------
// #9: OpenAI Completions tool block reuse when the index arrives after the id
// ---------------------------------------------------------------------------

fn event_names(events: &[AssistantMessageEvent]) -> Vec<&'static str> {
    events
        .iter()
        .map(|event| match event {
            AssistantMessageEvent::Start { .. } => "start",
            AssistantMessageEvent::TextStart { .. } => "text_start",
            AssistantMessageEvent::TextDelta { .. } => "text_delta",
            AssistantMessageEvent::TextEnd { .. } => "text_end",
            AssistantMessageEvent::ThinkingStart { .. } => "thinking_start",
            AssistantMessageEvent::ThinkingDelta { .. } => "thinking_delta",
            AssistantMessageEvent::ThinkingEnd { .. } => "thinking_end",
            AssistantMessageEvent::ToolCallStart { .. } => "toolcall_start",
            AssistantMessageEvent::ToolCallDelta { .. } => "toolcall_delta",
            AssistantMessageEvent::ToolCallEnd { .. } => "toolcall_end",
            AssistantMessageEvent::Done { .. } => "done",
            AssistantMessageEvent::Error { .. } => "error",
        })
        .collect()
}

fn done_message(events: &[AssistantMessageEvent]) -> AssistantMessage {
    events
        .iter()
        .rev()
        .find_map(|event| match event {
            AssistantMessageEvent::Done { message, .. } => Some(message.clone()),
            _ => None,
        })
        .expect("done event")
}

#[test]
fn openai_completions_id_then_index_reuses_the_same_tool_block() {
    let model = model("openai-chat", None);
    let body = concat!(
        "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"id\":\"call_1\",\"type\":\"function\",\"function\":{\"name\":\"lookup\",\"arguments\":\"{\\\"q\\\":\"}}]}}]}\n\n",
        "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"function\":{\"arguments\":\"\\\"x\\\"}\"}}]}}]}\n\n",
        "data: {\"choices\":[{\"finish_reason\":\"tool_calls\"}]}\n\n",
        "data: [DONE]\n\n"
    );
    let events = parse_openai_sse(body.as_bytes(), &model).expect("parses");
    assert_eq!(
        event_names(&events),
        vec![
            "start",
            "toolcall_start",
            "toolcall_delta",
            "toolcall_delta",
            "toolcall_end",
            "done"
        ],
        "the index-bearing chunk must not start a second block"
    );
    let message = done_message(&events);
    assert_eq!(message.content.len(), 1, "{:?}", message.content);
    let ContentBlock::ToolCall(call) = &message.content[0] else {
        panic!("expected a tool call block: {:?}", message.content);
    };
    assert_eq!(call.id, "call_1");
    assert_eq!(call.name, "lookup");
    assert_eq!(
        call.arguments,
        BTreeMap::from([("q".to_string(), json!("x"))])
    );
}

#[test]
fn openai_completions_index_then_id_updates_the_same_tool_block() {
    let model = model("openai-chat", None);
    let body = concat!(
        "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_2\",\"type\":\"function\",\"function\":{\"name\":\"lookup\",\"arguments\":\"{\\\"q\\\":\"}}]}}]}\n\n",
        "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"id\":\"call_2\",\"function\":{\"arguments\":\"1}\"}}]}}]}\n\n",
        "data: {\"choices\":[{\"finish_reason\":\"tool_calls\"}]}\n\n"
    );
    let events = parse_openai_sse(body.as_bytes(), &model).expect("parses");
    assert_eq!(
        event_names(&events),
        vec![
            "start",
            "toolcall_start",
            "toolcall_delta",
            "toolcall_delta",
            "toolcall_end",
            "done"
        ]
    );
    let message = done_message(&events);
    assert_eq!(message.content.len(), 1, "{:?}", message.content);
    let ContentBlock::ToolCall(call) = &message.content[0] else {
        panic!("expected a tool call block: {:?}", message.content);
    };
    assert_eq!(call.id, "call_2");
    assert_eq!(
        call.arguments,
        BTreeMap::from([("q".to_string(), json!(1))])
    );
}
