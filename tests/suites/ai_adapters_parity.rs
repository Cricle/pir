//! Golden parity tests for the remaining API adapters
//! (openai-responses / openai-codex-responses / azure-openai-responses /
//! bedrock-converse-stream / google-vertex / mistral-conversations).
//!
//! Request bodies, SSE / event-stream fixtures, usage and stop-reason mappings,
//! the item-id prefix rules, and the AWS SigV4 published test vector mirror the
//! upstream `packages/ai` tests (ground truth at HEAD ce950d78f). No live
//! network: HTTP-level contract tests live in `ai_remaining_adapter_contracts.rs`.

use std::collections::BTreeMap;

use pir::ai::api::bedrock::{
    build_additional_model_request_fields, build_bedrock_body, convert_bedrock_messages,
    format_amz_date, map_bedrock_stop_reason, parse_bedrock_event_stream, resolve_bedrock_region,
    sigv4_sign_at, sigv4_signing_key,
};
use pir::ai::api::google_vertex::resolve_vertex_url;
use pir::ai::api::mistral::{
    build_mistral_payload, map_mistral_stop_reason, parse_mistral_sse, to_mistral_wire_payload,
    MistralToolCallIdNormalizer, MISTRAL_TOOL_CALL_ID_LENGTH,
};
use pir::ai::api::openai_responses::azure::{
    azure_responses_url, normalize_azure_base_url, resolve_deployment_name,
};
use pir::ai::api::openai_responses::codex::{
    build_codex_body, extract_account_id, resolve_codex_url,
};
use pir::ai::api::openai_responses::shared::{
    convert_responses_messages, convert_responses_tools, parse_responses_sse,
};
use pir::ai::api::openai_responses::{build_responses_params, ResponsesCompat};
use pir::ai::api::transform::{normalize_id_part, short_hash, transform_messages};
use pir::ai::providers::catalog;
use pir::ai::types::*;
use serde_json::{json, Value};

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

fn model(api: &str, provider: &str, id: &str) -> Model {
    model_with(api, provider, id, Vec::new())
}

fn model_with(api: &str, provider: &str, id: &str, input: Vec<&str>) -> Model {
    Model {
        id: id.into(),
        name: id.into(),
        api: api.into(),
        provider: provider.into(),
        base_url: "https://example.test/v1".into(),
        reasoning: false,
        thinking_level_map: None,
        input: {
            let mut all = vec!["text".to_string()];
            all.extend(input.into_iter().map(str::to_string));
            all
        },
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

fn text_block(text: &str) -> ContentBlock {
    ContentBlock::Text(TextContent {
        text: text.into(),
        text_signature: None,
    })
}

fn user_message(text: &str) -> Message {
    Message::User(UserMessage {
        role: UserRole::User,
        content: vec![text_block(text)],
        tools_added: None,
        tools_removed: None,
        timestamp: 1,
        sections: None,
    })
}

fn assistant_message(content: Vec<ContentBlock>, stop: StopReason) -> Message {
    let mut message = AssistantMessage {
        api: "openai-responses".into(),
        provider: "openai".into(),
        model: "gpt-5.5".into(),
        timestamp: 2,
        ..AssistantMessage::default()
    };
    message.content = content;
    message.stop_reason = stop;
    Message::Assistant(message)
}

fn tool_call_block(id: &str, name: &str, arguments: Value) -> ContentBlock {
    ContentBlock::ToolCall(ToolCall {
        id: id.into(),
        name: name.into(),
        arguments: serde_json::from_value(arguments).unwrap_or_default(),
        thought_signature: None,
        namespace: None,
        arguments_error: None,
    })
}

fn tool_result_message(call_id: &str, name: &str, text: &str) -> Message {
    Message::ToolResult(ToolResultMessage {
        role: ToolResultRole::ToolResult,
        tool_call_id: call_id.into(),
        tool_name: name.into(),
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

fn simple_context(text: &str) -> Context {
    Context {
        system_prompt: Some("You are helpful.".into()),
        messages: vec![user_message(text)],
        tools: None,
    }
}

fn text_event_message(events: &[AssistantMessageEvent]) -> AssistantMessage {
    events
        .iter()
        .find_map(|event| match event {
            AssistantMessageEvent::Done { message, .. } => Some(message.clone()),
            _ => None,
        })
        .expect("done event")
}

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

// ---------------------------------------------------------------------------
// transform shared helpers
// ---------------------------------------------------------------------------

#[test]
fn short_hash_is_deterministic_and_ascii() {
    let a = short_hash("call_abc|item_1");
    let b = short_hash("call_abc|item_1");
    assert_eq!(a, b);
    assert!(!a.is_empty());
    assert!(a.chars().all(|c| c.is_ascii_alphanumeric()));
}

#[test]
fn normalize_id_part_strips_and_truncates() {
    assert_eq!(normalize_id_part("call_abc-1"), "call_abc-1");
    assert_eq!(normalize_id_part("a b/c.d"), "a_b_c_d");
    let long = "x".repeat(80);
    assert_eq!(normalize_id_part(&long).chars().count(), 64);
}

#[test]
fn transform_messages_synthesizes_missing_tool_results() {
    let model = model("openai-responses", "openai", "gpt-5.5");
    let context = Context {
        messages: vec![
            assistant_message(
                vec![tool_call_block("call_1|fc_1", "search", json!({"q": "x"}))],
                StopReason::ToolUse,
            ),
            user_message("and then?"),
        ],
        ..Context::default()
    };
    let transformed = transform_messages(&context.messages, &model, None);
    let synthetic = transformed.iter().find_map(|message| match message {
        Message::ToolResult(result) if result.tool_call_id == "call_1|fc_1" => Some(result),
        _ => None,
    });
    let synthetic = synthetic.expect("synthetic tool result");
    assert!(synthetic.is_error);
    assert_eq!(
        synthetic.content[0],
        text_block("No result provided"),
        "synthetic result text"
    );
}

// ---------------------------------------------------------------------------
// openai-responses: message conversion
// ---------------------------------------------------------------------------

#[test]
fn responses_messages_golden_body() {
    let model = model("openai-responses", "openai", "gpt-5.5");
    let context = Context {
        system_prompt: Some("You are helpful.".into()),
        messages: vec![
            user_message("Hello"),
            assistant_message(
                vec![
                    text_block("Hi"),
                    tool_call_block("call_1|fc_item_1", "search", json!({"q": "x"})),
                ],
                StopReason::ToolUse,
            ),
            tool_result_message("call_1", "search", "found it"),
        ],
        tools: None,
    };
    let items = convert_responses_messages(
        &model,
        &context,
        pir::ai::api::openai_responses::shared::OPENAI_TOOL_CALL_PROVIDERS,
        &Default::default(),
    );
    // Leading system prompt as developer role (non-reasoning model -> system).
    assert_eq!(items[0]["role"], "system");
    assert_eq!(items[0]["content"], "You are helpful.");
    assert_eq!(
        items[1],
        json!({"role": "user", "content": [{"type": "input_text", "text": "Hello"}]})
    );
    let message = &items[2];
    assert_eq!(message["type"], "message");
    assert_eq!(message["id"], "msg_pi_1");
    assert_eq!(message["content"][0]["type"], "output_text");
    let call = &items[3];
    assert_eq!(call["type"], "function_call");
    assert_eq!(call["call_id"], "call_1");
    assert_eq!(call["id"], "fc_item_1");
    assert_eq!(call["arguments"], r#"{"q":"x"}"#);
    let output = &items[4];
    assert_eq!(output["type"], "function_call_output");
    assert_eq!(output["call_id"], "call_1");
    assert_eq!(output["output"], "found it");
}

#[test]
fn responses_foreign_tool_call_ids_are_hashed_with_fc_prefix() {
    let model = model("openai-responses", "openai", "gpt-5.5");
    let mut foreign = assistant_message(
        vec![tool_call_block("call\x01|xitem", "search", json!({}))],
        StopReason::ToolUse,
    );
    if let Message::Assistant(assistant) = &mut foreign {
        assistant.provider = "anthropic".into();
        assistant.api = "anthropic-messages".into();
    }
    let context = Context {
        messages: vec![foreign],
        ..Context::default()
    };
    let items = convert_responses_messages(
        &model,
        &context,
        pir::ai::api::openai_responses::shared::OPENAI_TOOL_CALL_PROVIDERS,
        &Default::default(),
    );
    let call = &items[0];
    assert_eq!(call["type"], "function_call");
    let item_id = call["id"].as_str().expect("item id");
    assert!(
        item_id.starts_with("fc_"),
        "foreign item ids get fc_ prefix: {item_id}"
    );
    assert!(item_id.chars().count() <= 64);
}

#[test]
fn responses_different_model_drops_item_id() {
    let model = model("openai-responses", "openai", "gpt-5.5");
    let mut other_model = assistant_message(
        vec![tool_call_block("call_1|fc_item_1", "search", json!({}))],
        StopReason::ToolUse,
    );
    if let Message::Assistant(assistant) = &mut other_model {
        assistant.model = "gpt-5.4".into();
    }
    let context = Context {
        messages: vec![other_model],
        ..Context::default()
    };
    let items = convert_responses_messages(
        &model,
        &context,
        pir::ai::api::openai_responses::shared::OPENAI_TOOL_CALL_PROVIDERS,
        &Default::default(),
    );
    assert!(
        items[0].get("id").is_none(),
        "different-model calls omit the item id"
    );
}

#[test]
fn responses_tools_golden() {
    let tools = vec![Tool {
        name: "search".into(),
        description: "Search".into(),
        parameters: json!({"type": "object", "properties": {"query": {"type": "string"}}}),
        constrained_sampling: None,
    }];
    // Default (non-strict) omits the strict flag.
    let converted = convert_responses_tools(&tools, false).expect("tools");
    assert_eq!(converted[0]["type"], "function");
    assert_eq!(converted[0]["name"], "search");
    assert!(converted[0].get("strict").is_none());
    // Strict providers carry the flag.
    let converted = convert_responses_tools(&tools, true).expect("tools");
    assert_eq!(converted[0]["strict"], false);
}

#[test]
fn responses_params_golden_with_reasoning_and_cache() {
    let mut model = model("openai-responses", "openai", "gpt-5.5");
    model.reasoning = true;
    let options = StreamOptions {
        session_id: Some("sess-1".into()),
        reasoning: Some(ThinkingLevel::High),
        max_tokens: Some(8),
        cache_retention: Some(CacheRetention::Long),
        ..StreamOptions::default()
    };
    let params = build_responses_params(
        &model,
        &simple_context("Hello"),
        &options,
        &ResponsesCompat::default(),
        "gpt-5.5",
        pir::ai::api::openai_responses::shared::OPENAI_TOOL_CALL_PROVIDERS,
        false,
        None,
    )
    .expect("params");
    assert_eq!(params["model"], "gpt-5.5");
    assert_eq!(params["stream"], true);
    assert_eq!(params["store"], false);
    assert_eq!(params["prompt_cache_key"], "sess-1");
    assert_eq!(params["prompt_cache_retention"], "24h");
    // max_output_tokens clamps to the 16 minimum.
    assert_eq!(params["max_output_tokens"], 16);
    assert_eq!(
        params["reasoning"],
        json!({"effort": "high", "summary": "auto"})
    );
    assert_eq!(params["include"], json!(["reasoning.encrypted_content"]));
}

#[test]
fn responses_chatgpt_sign_in_omits_unsupported_fields() {
    let mut model = model("openai-responses", "openai", "gpt-5.5");
    model.base_url = "https://api.openai.com/v1".into();
    let options = StreamOptions {
        api_key: Some("chatgpt-token".into()),
        max_tokens: Some(1024),
        temperature: Some(0.5),
        ..StreamOptions::default()
    };
    let params = build_responses_params(
        &model,
        &simple_context("Hello"),
        &options,
        &ResponsesCompat::default(),
        "gpt-5.5",
        pir::ai::api::openai_responses::shared::OPENAI_TOOL_CALL_PROVIDERS,
        pir::ai::api::openai_responses::shared::is_chatgpt_sign_in(&model, Some("chatgpt-token")),
        None,
    )
    .expect("params");
    assert!(params.get("max_output_tokens").is_none());
    assert!(params.get("temperature").is_none());
}

// ---------------------------------------------------------------------------
// openai-responses: SSE stream parsing
// ---------------------------------------------------------------------------

#[test]
fn responses_sse_text_stream_golden() {
    let sse = concat!(
        "data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_1\"}}\n\n",
        "data: {\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{\"type\":\"message\",\"id\":\"msg_1\",\"role\":\"assistant\",\"status\":\"in_progress\",\"content\":[]}}\n\n",
        "data: {\"type\":\"response.output_text.delta\",\"output_index\":0,\"delta\":\"Hello\"}\n\n",
        "data: {\"type\":\"response.output_text.delta\",\"output_index\":0,\"delta\":\" world\"}\n\n",
        "data: {\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{\"type\":\"message\",\"id\":\"msg_1\",\"role\":\"assistant\",\"status\":\"completed\",\"content\":[{\"type\":\"output_text\",\"text\":\"Hello world\"}]}}\n\n",
        "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_1\",\"status\":\"completed\",\"usage\":{\"input_tokens\":10,\"output_tokens\":3,\"total_tokens\":13,\"input_tokens_details\":{\"cached_tokens\":4,\"cache_write_tokens\":2},\"output_tokens_details\":{\"reasoning_tokens\":1}}}}\n\n",
    );
    let model = model("openai-responses", "openai", "gpt-5.5");
    let events = parse_responses_sse(sse.as_bytes(), &model).expect("events");
    assert_eq!(
        event_names(&events),
        vec![
            "start",
            "text_start",
            "text_delta",
            "text_delta",
            "text_end",
            "done"
        ]
    );
    let final_message = text_event_message(&events);
    assert_eq!(final_message.response_id.as_deref(), Some("resp_1"));
    assert_eq!(final_message.stop_reason, StopReason::Stop);
    assert_eq!(final_message.raw_stop_reason.as_deref(), Some("completed"));
    // OpenAI includes cached and cache-write tokens in input_tokens.
    assert_eq!(final_message.usage.input, 4);
    assert_eq!(final_message.usage.cache_read, 4);
    assert_eq!(final_message.usage.cache_write, 2);
    assert_eq!(final_message.usage.output, 3);
    assert_eq!(final_message.usage.reasoning, Some(1));
    assert_eq!(final_message.usage.total_tokens, 13);
    // Text blocks persist the pir text signature with the item id.
    assert!(
        matches!(&final_message.content[0], ContentBlock::Text(text) if text.text == "Hello world")
    );
}

#[test]
fn responses_sse_text_signature_carries_item_id() {
    let sse = concat!(
        "data: {\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{\"type\":\"message\",\"id\":\"msg_9\"}}\n\n",
        "data: {\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{\"type\":\"message\",\"id\":\"msg_9\",\"content\":[{\"type\":\"output_text\",\"text\":\"done\"}]}}\n\n",
        "data: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\"}}\n\n",
    );
    let model = model("openai-responses", "openai", "gpt-5.5");
    let events = parse_responses_sse(sse.as_bytes(), &model).expect("events");
    let final_message = text_event_message(&events);
    match &final_message.content[0] {
        ContentBlock::Text(text) => {
            let signature = text.text_signature.as_deref().expect("text signature");
            let parsed: Value = serde_json::from_str(signature).expect("signature json");
            assert_eq!(parsed["v"], 1);
            assert_eq!(parsed["id"], "msg_9");
        }
        other => panic!("expected text block, got {other:?}"),
    }
}

#[test]
fn responses_sse_tool_call_and_incomplete_mapping() {
    let sse = concat!(
        "data: {\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{\"type\":\"function_call\",\"call_id\":\"call_1\",\"id\":\"fc_1\",\"name\":\"search\",\"arguments\":\"\"}}\n\n",
        "data: {\"type\":\"response.function_call_arguments.delta\",\"output_index\":0,\"delta\":\"{\\\"q\\\":\\\"ru\"}\n\n",
        "data: {\"type\":\"response.function_call_arguments.delta\",\"output_index\":0,\"delta\":\"st\\\"}\"}\n\n",
        "data: {\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{\"type\":\"function_call\",\"call_id\":\"call_1\",\"id\":\"fc_1\",\"name\":\"search\",\"arguments\":\"{\\\"q\\\":\\\"rust\\\"}\"}}\n\n",
        "data: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\"}}\n\n",
    );
    let model = model("openai-responses", "openai", "gpt-5.5");
    let events = parse_responses_sse(sse.as_bytes(), &model).expect("events");
    let final_message = text_event_message(&events);
    assert_eq!(final_message.stop_reason, StopReason::ToolUse);
    match &final_message.content[0] {
        ContentBlock::ToolCall(call) => {
            assert_eq!(call.id, "call_1|fc_1");
            assert_eq!(call.name, "search");
            assert_eq!(
                call.arguments.get("q").and_then(Value::as_str),
                Some("rust")
            );
        }
        other => panic!("expected tool call, got {other:?}"),
    }
    // Incomplete max_output_tokens maps to length with the raw reason kept.
    let sse = "data: {\"type\":\"response.completed\",\"response\":{\"status\":\"incomplete\",\"incomplete_details\":{\"reason\":\"max_output_tokens\"}}}\n\n";
    let events = parse_responses_sse(sse.as_bytes(), &model).expect("events");
    let final_message = text_event_message(&events);
    assert_eq!(final_message.stop_reason, StopReason::Length);
    assert_eq!(
        final_message.raw_stop_reason.as_deref(),
        Some("incomplete.max_output_tokens")
    );
}

#[test]
fn responses_sse_rejects_unfinished_tool_calls() {
    let sse = concat!(
        "data: {\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{\"type\":\"function_call\",\"call_id\":\"call_1\",\"id\":\"fc_1\",\"name\":\"search\",\"arguments\":\"\"}}\n\n",
        "data: {\"type\":\"response.function_call_arguments.delta\",\"output_index\":0,\"delta\":\"{\\\"q\\\":\"}\n\n",
        "data: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\"}}\n\n",
    );
    let model = model("openai-responses", "openai", "gpt-5.5");
    let error = parse_responses_sse(sse.as_bytes(), &model).expect_err("unfinished call rejected");
    assert!(
        error.to_string().contains("unfinished tool call"),
        "unexpected error: {error}"
    );
}

#[test]
fn responses_sse_requires_terminal_event_and_maps_errors() {
    let model = model("openai-responses", "openai", "gpt-5.5");
    let error = parse_responses_sse(b"data: {}\n\n", &model).expect_err("no terminal event");
    assert!(error.to_string().contains("terminal response event"));

    let error = parse_responses_sse(
        b"data: {\"type\":\"error\",\"code\":\"overflow\",\"message\":\"too long\"}\n\n",
        &model,
    )
    .expect_err("error event");
    assert!(error.to_string().contains("Error Code overflow"));

    let error = parse_responses_sse(
        b"data: {\"type\":\"response.failed\",\"response\":{\"error\":{\"code\":\"boom\",\"message\":\"bad\"}}}\n\n",
        &model,
    )
    .expect_err("failed response");
    assert!(error.to_string().contains("boom: bad"));
}

// ---------------------------------------------------------------------------
// openai-codex-responses
// ---------------------------------------------------------------------------

#[test]
fn codex_url_resolution() {
    assert_eq!(
        resolve_codex_url("https://chatgpt.com/backend-api"),
        "https://chatgpt.com/backend-api/codex/responses"
    );
    assert_eq!(
        resolve_codex_url("https://chatgpt.com/backend-api/codex"),
        "https://chatgpt.com/backend-api/codex/responses"
    );
    assert_eq!(
        resolve_codex_url("https://chatgpt.com/backend-api/codex/responses"),
        "https://chatgpt.com/backend-api/codex/responses"
    );
    assert_eq!(
        resolve_codex_url(""),
        "https://chatgpt.com/backend-api/codex/responses"
    );
}

#[test]
fn codex_account_id_extraction() {
    use base64::Engine;
    let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode(br#"{"https://api.openai.com/auth":{"chatgpt_account_id":"acc_1"}}"#);
    let token = format!("aaa.{payload}.bbb");
    assert_eq!(extract_account_id(&token).expect("account id"), "acc_1");
    assert!(extract_account_id("not-a-jwt").is_err());
}

#[test]
fn codex_body_golden() {
    let mut model = model("openai-codex-responses", "openai-codex", "gpt-5.3-codex");
    model.reasoning = true;
    model.base_url = "https://chatgpt.com/backend-api".into();
    let options = StreamOptions {
        session_id: Some("sess-1".into()),
        reasoning: Some(ThinkingLevel::Medium),
        ..StreamOptions::default()
    };
    let body = build_codex_body(&model, &simple_context("Hello"), &options).expect("body");
    assert_eq!(body["model"], "gpt-5.3-codex");
    assert_eq!(body["store"], false);
    assert_eq!(body["stream"], true);
    // Codex sends the system prompt via `instructions`, not an input item.
    assert_eq!(body["instructions"], "You are helpful.");
    assert!(
        !body["input"]
            .as_array()
            .expect("input")
            .iter()
            .any(|item| item.get("role").and_then(Value::as_str) == Some("system")),
        "no leading system input item"
    );
    assert_eq!(body["text"], json!({"verbosity": "low"}));
    assert_eq!(body["include"], json!(["reasoning.encrypted_content"]));
    assert_eq!(body["prompt_cache_key"], "sess-1");
    assert_eq!(body["tool_choice"], "auto");
    assert_eq!(body["parallel_tool_calls"], true);
    assert_eq!(
        body["reasoning"],
        json!({"effort": "medium", "summary": "auto"})
    );
}

#[test]
fn codex_sse_maps_done_and_end_turn() {
    let sse = concat!(
        "data: {\"type\":\"response.output_item.added\",\"item\":{\"type\":\"message\",\"id\":\"msg_1\",\"role\":\"assistant\",\"status\":\"in_progress\",\"content\":[]}}\n\n",
        "data: {\"type\":\"response.output_text.delta\",\"delta\":\"Hello\"}\n\n",
        "data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"message\",\"id\":\"msg_1\",\"role\":\"assistant\",\"status\":\"completed\",\"content\":[{\"type\":\"output_text\",\"text\":\"Hello\"}]}}\n\n",
        "data: {\"type\":\"response.done\",\"response\":{\"status\":\"completed\",\"end_turn\":true,\"usage\":{\"input_tokens\":5,\"output_tokens\":3,\"total_tokens\":8,\"input_tokens_details\":{\"cached_tokens\":0}}}}\n\n",
    );
    let model = model("openai-codex-responses", "openai-codex", "gpt-5.1-codex");
    // Build the codex parser via the shared parser + codex event mapping: the
    // codex-mode flag lives on ResponsesSseParser.
    let mut parser = pir::ai::api::openai_responses::ResponsesSseParser::new(&model);
    parser.codex_mode = true;
    let mut events = vec![AssistantMessageEvent::Start {
        partial: parser.snapshot(),
    }];
    for line in sse.lines() {
        parser.feed_line(line, &mut events).expect("feed");
    }
    parser.feed_line("", &mut events).expect("flush");
    parser.finish(&mut events).expect("finish");
    let final_message = text_event_message(&events);
    assert_eq!(final_message.stop_reason, StopReason::Stop);
    assert_eq!(final_message.end_turn, Some(true));
    assert_eq!(final_message.usage.input, 5);
    assert_eq!(
        final_message.raw_stop_reason.as_deref(),
        Some("completed"),
        "codex response.done normalizes to response.completed"
    );
}

// ---------------------------------------------------------------------------
// azure-openai-responses
// ---------------------------------------------------------------------------

#[test]
fn azure_deployment_map_and_url() {
    let model = model("azure-openai-responses", "azure", "gpt-5.5");
    // No map: the model id is the deployment.
    assert_eq!(resolve_deployment_name(&model, None), "gpt-5.5");
    let mut env: ProviderEnv = ProviderEnv::new();
    env.insert(
        "AZURE_OPENAI_DEPLOYMENT_NAME_MAP".into(),
        "gpt-5.5=gpt55-deploy, gpt-5.4=gpt54".into(),
    );
    assert_eq!(resolve_deployment_name(&model, Some(&env)), "gpt55-deploy");
    let mut env = ProviderEnv::new();
    env.insert("AZURE_OPENAI_RESOURCE_NAME".into(), "my-resource".into());
    let (url, api_version) = azure_url_parts(&model, Some(&env));
    assert_eq!(
        url,
        "https://my-resource.openai.azure.com/openai/v1/deployments/gpt-5.5/responses"
    );
    assert_eq!(api_version, "v1");
}

fn azure_url_parts(model: &Model, env: Option<&ProviderEnv>) -> (String, String) {
    let url = azure_responses_url(model, env).expect("url");
    let (without_query, query) = url.split_once("?api-version=").expect("api-version query");
    (without_query.to_string(), query.to_string())
}

#[test]
fn azure_base_url_normalization() {
    assert_eq!(
        normalize_azure_base_url("https://res.openai.azure.com").expect("normalized"),
        "https://res.openai.azure.com/openai/v1"
    );
    assert_eq!(
        normalize_azure_base_url("https://res.openai.azure.com/").expect("normalized"),
        "https://res.openai.azure.com/openai/v1"
    );
    assert_eq!(
        normalize_azure_base_url("https://res.openai.azure.com/openai/v1/responses")
            .expect("normalized"),
        "https://res.openai.azure.com/openai/v1"
    );
    // Custom (non-Azure) endpoints keep their path.
    assert_eq!(
        normalize_azure_base_url("https://proxy.example.com/azure").expect("normalized"),
        "https://proxy.example.com/azure"
    );
    assert!(normalize_azure_base_url("not a url").is_err());
}

#[test]
fn azure_params_use_deployment_as_model() {
    let model = model("azure-openai-responses", "azure", "gpt-5.5");
    let params = pir::ai::api::openai_responses::azure::build_azure_params(
        &model,
        &simple_context("Hello"),
        &StreamOptions::default(),
    )
    .expect("params");
    assert_eq!(params["model"], "gpt-5.5");
    assert_eq!(params["stream"], true);
    assert_eq!(params["store"], false);
}

// ---------------------------------------------------------------------------
// bedrock-converse-stream
// ---------------------------------------------------------------------------

fn claude_model() -> Model {
    model(
        "bedrock-converse-stream",
        "amazon-bedrock",
        "anthropic.claude-sonnet-4-6",
    )
}

#[test]
fn bedrock_body_golden_with_cache_points_and_merged_tool_results() {
    let model = claude_model();
    let context = Context {
        system_prompt: Some("You are helpful.".into()),
        messages: vec![
            user_message("Hello"),
            assistant_message(
                vec![
                    text_block("Let me look."),
                    tool_call_block("toolu_1", "search", json!({"q": "x"})),
                ],
                StopReason::ToolUse,
            ),
            tool_result_message("toolu_1", "search", "one"),
            tool_result_message("toolu_2", "read", "two"),
        ],
        tools: None,
    };
    let options = StreamOptions {
        max_tokens: Some(1024),
        temperature: Some(0.2),
        cache_retention: Some(CacheRetention::Short),
        ..StreamOptions::default()
    };
    let body = build_bedrock_body(&model, &context, &options).expect("body");
    assert_eq!(body["modelId"], "anthropic.claude-sonnet-4-6");
    assert_eq!(
        body["inferenceConfig"],
        json!({"maxTokens": 1024, "temperature": 0.2})
    );
    assert_eq!(
        body["system"],
        json!([{"text": "You are helpful."}, {"cachePoint": {"type": "default"}}])
    );
    let messages = body["messages"].as_array().expect("messages");
    assert_eq!(messages.len(), 3, "user, assistant, merged tool results");
    assert_eq!(messages[0]["role"], "user");
    assert_eq!(messages[1]["role"], "assistant");
    assert_eq!(messages[1]["content"][0]["text"], "Let me look.");
    assert_eq!(messages[1]["content"][1]["toolUse"]["toolUseId"], "toolu_1");
    // Consecutive tool results merge into a single user message.
    assert_eq!(messages[2]["role"], "user");
    let contents = messages[2]["content"].as_array().expect("contents");
    assert_eq!(contents.len(), 3, "two tool results + cache point");
    assert_eq!(contents[0]["toolResult"]["toolUseId"], "toolu_1");
    assert_eq!(contents[1]["toolResult"]["toolUseId"], "toolu_2");
    assert_eq!(contents[1]["toolResult"]["status"], "success");
    assert_eq!(contents[2]["cachePoint"], json!({"type": "default"}));
}

#[test]
fn bedrock_thinking_replay_and_redacted_content() {
    use base64::Engine;
    let model = claude_model();
    let encrypted = b"secret-bytes".to_vec();
    let redacted_signature = base64::engine::general_purpose::STANDARD.encode(&encrypted);
    let signature_for_block = redacted_signature.clone();
    let mut assistant = AssistantMessage {
        api: "bedrock-converse-stream".into(),
        provider: "amazon-bedrock".into(),
        model: "anthropic.claude-sonnet-4-6".into(),
        timestamp: 2,
        ..AssistantMessage::default()
    };
    assistant.content = vec![
        ContentBlock::Thinking(ThinkingContent {
            thinking: "deep thought".into(),
            thinking_signature: Some("sig-abc".into()),
            redacted: false,
        }),
        ContentBlock::Thinking(ThinkingContent {
            thinking: String::new(),
            thinking_signature: Some(signature_for_block),
            redacted: true,
        }),
    ];
    assistant.stop_reason = StopReason::Stop;
    let context = Context {
        messages: vec![Message::Assistant(assistant)],
        ..Context::default()
    };
    let messages = convert_bedrock_messages(&context, &model, false, false, false);
    let content = messages[0]["content"].as_array().expect("content");
    // Signed thinking replays as reasoningContent with the signature;
    // redacted thinking replays as redactedContent bytes.
    assert_eq!(
        content[0],
        json!({"reasoningContent": {"reasoningText": {"text": "deep thought", "signature": "sig-abc"}}})
    );
    assert_eq!(
        content[1]["reasoningContent"]["redactedContent"].as_str(),
        Some(redacted_signature.as_str()),
        "redacted payloads travel as base64 bytes"
    );
}

#[test]
fn bedrock_additional_model_request_fields() {
    // Adaptive thinking on Opus 4.7+ with block binding + beta header.
    let mut opus = model(
        "bedrock-converse-stream",
        "amazon-bedrock",
        "anthropic.claude-opus-4-7",
    );
    opus.reasoning = true;
    let fields = build_additional_model_request_fields(&opus, Some(ThinkingLevel::High))
        .expect("fields")
        .expect("fields set");
    assert_eq!(
        fields,
        json!({
            "thinking": {
                "type": "adaptive",
                "display": "summarized",
                "block_binding": {"prefix_mismatch_behavior": "drop_block"},
            },
            "output_config": {"effort": "high"},
            "anthropic_beta": ["thinking-binding-controls-2026-08-01"],
        })
    );
    // Adaptive (Sonnet 4.6) without block binding (Opus 4.6/Sonnet 4.6 reject
    // thinking.block_binding).
    let mut sonnet46 = claude_model();
    sonnet46.reasoning = true;
    let fields = build_additional_model_request_fields(&sonnet46, Some(ThinkingLevel::Low))
        .expect("fields")
        .expect("fields set");
    assert_eq!(
        fields,
        json!({
            "thinking": {"type": "adaptive", "display": "summarized"},
            "output_config": {"effort": "low"},
        })
    );
    // Budget-based thinking on Claude Sonnet 4.5 with the interleaved beta.
    let mut sonnet45 = model(
        "bedrock-converse-stream",
        "amazon-bedrock",
        "anthropic.claude-sonnet-4-5",
    );
    sonnet45.reasoning = true;
    let fields = build_additional_model_request_fields(&sonnet45, Some(ThinkingLevel::Low))
        .expect("fields")
        .expect("fields set");
    assert_eq!(
        fields,
        json!({
            "thinking": {"type": "enabled", "budget_tokens": 2048, "display": "summarized"},
            "anthropic_beta": ["interleaved-thinking-2025-05-14"],
        })
    );
    // GPT-OSS models take a flat reasoning_effort clamped to low/medium/high.
    let mut gpt_oss = model(
        "bedrock-converse-stream",
        "amazon-bedrock",
        "openai.gpt-oss-120b",
    );
    gpt_oss.reasoning = true;
    let fields = build_additional_model_request_fields(&gpt_oss, Some(ThinkingLevel::Max))
        .expect("fields")
        .expect("fields set");
    assert_eq!(fields, json!({"reasoning_effort": "high"}));
}

#[test]
fn bedrock_stop_reason_mapping() {
    assert_eq!(
        map_bedrock_stop_reason(Some("end_turn")).0,
        StopReason::Stop
    );
    assert_eq!(
        map_bedrock_stop_reason(Some("stop_sequence")).0,
        StopReason::Stop
    );
    assert_eq!(
        map_bedrock_stop_reason(Some("max_tokens")).0,
        StopReason::Length
    );
    assert_eq!(
        map_bedrock_stop_reason(Some("model_context_window_exceeded")).0,
        StopReason::Length
    );
    assert_eq!(
        map_bedrock_stop_reason(Some("tool_use")).0,
        StopReason::ToolUse
    );
    let (stop, message) = map_bedrock_stop_reason(Some("guardrail_intervened"));
    assert_eq!(stop, StopReason::Error);
    assert_eq!(
        message.as_deref(),
        Some("Provider stopped with: guardrail_intervened")
    );
}

#[test]
fn bedrock_region_resolution() {
    let mut model = claude_model();
    model.base_url = "https://bedrock-runtime.eu-west-1.amazonaws.com".into();
    assert_eq!(resolve_bedrock_region(&model, None), "eu-west-1");
    let mut env = ProviderEnv::new();
    env.insert("AWS_REGION".into(), "us-west-2".into());
    assert_eq!(resolve_bedrock_region(&model, Some(&env)), "us-west-2");
    let mut model = claude_model();
    model.id = "arn:aws:bedrock:ap-south-1::inference-profile/x".into();
    model.base_url = String::new();
    assert_eq!(resolve_bedrock_region(&model, None), "ap-south-1");
    assert_eq!(resolve_bedrock_region(&claude_model(), None), "us-east-1");
}

// --- AWS SigV4 published test vector --------------------------------------

#[test]
fn sigv4_matches_aws_published_test_vector() {
    // AWS documentation example ("Example: GET", IAM ListUsers).
    // https://docs.aws.amazon.com/general/latest/gr/sigv4-signed-request-examples.html
    let url = "https://iam.amazonaws.com/?Action=ListUsers&Version=2010-05-08";
    let mut headers: BTreeMap<String, String> = BTreeMap::new();
    headers.insert(
        "Content-Type".into(),
        "application/x-www-form-urlencoded; charset=utf-8".into(),
    );
    let payload = b"";
    let signed = sigv4_sign_at(
        "GET",
        url,
        &headers,
        payload,
        "AKIDEXAMPLE",
        "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY",
        None,
        "us-east-1",
        "iam",
        "20150830T123600Z",
    )
    .expect("signature");
    let authorization = signed
        .iter()
        .find(|(name, _)| name == "authorization")
        .map(|(_, value)| value.clone())
        .expect("authorization header");
    assert_eq!(
        authorization,
        "AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/20150830/us-east-1/iam/aws4_request, \
         SignedHeaders=content-type;host;x-amz-date, \
         Signature=5d672d79c15b13162d9279b0855cfba6789a8edb4c82c400e06b5924a6f2b5d7"
    );
    // The derived signing key for this vector is also published.
    let key = sigv4_signing_key(
        "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY",
        "20150830",
        "us-east-1",
        "iam",
    )
    .expect("signing key");
    let hex = key
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    assert_eq!(
        hex,
        "c4afb1cc5771d871763a393e44b703571b55cc28424d1a5e86da6ed3c154a4b9"
    );
}

#[test]
fn amz_date_formatting() {
    assert_eq!(format_amz_date(0), "19700101T000000Z");
    // 2025-08-30T12:36:00Z == 1756557360
    assert_eq!(format_amz_date(1_756_557_360), "20250830T123600Z");
}

// --- Bedrock event-stream framing -----------------------------------------

/// Encode one AWS event-stream frame (v0): prelude + headers + payload + CRC.
fn encode_event_frame(message_type: &str, event_type: &str, payload_json: &str) -> Vec<u8> {
    let mut headers: Vec<u8> = Vec::new();
    for (name, value) in [(":message-type", message_type), (":event-type", event_type)] {
        headers.push(name.len() as u8);
        headers.extend_from_slice(name.as_bytes());
        headers.push(7); // string
        headers.extend_from_slice(&(value.len() as u16).to_be_bytes());
        headers.extend_from_slice(value.as_bytes());
    }
    let total = 12 + headers.len() + payload_json.len() + 4;
    let mut frame = Vec::with_capacity(total);
    frame.extend_from_slice(&(total as u32).to_be_bytes());
    frame.extend_from_slice(&(headers.len() as u32).to_be_bytes());
    frame.extend_from_slice(&[0, 0, 0, 0]); // prelude CRC (not validated)
    frame.extend_from_slice(&headers);
    frame.extend_from_slice(payload_json.as_bytes());
    frame.extend_from_slice(&[0, 0, 0, 0]); // message CRC (not validated)
    frame
}

#[test]
fn bedrock_event_stream_full_session() {
    let mut body = Vec::new();
    body.extend_from_slice(&encode_event_frame(
        "event",
        "messageStart",
        r#"{"messageStart":{"role":"assistant"}}"#,
    ));
    body.extend_from_slice(&encode_event_frame(
        "event",
        "contentBlockDelta",
        r#"{"contentBlockIndex":0,"delta":{"reasoningContent":{"text":"thinking hard"}}}"#,
    ));
    body.extend_from_slice(&encode_event_frame(
        "event",
        "contentBlockStop",
        r#"{"contentBlockIndex":0}"#,
    ));
    body.extend_from_slice(&encode_event_frame(
        "event",
        "contentBlockDelta",
        r#"{"contentBlockIndex":1,"delta":{"text":"Hello"}}"#,
    ));
    body.extend_from_slice(&encode_event_frame(
        "event",
        "contentBlockStart",
        r#"{"contentBlockIndex":2,"start":{"toolUse":{"toolUseId":"toolu_1","name":"search"}}}"#,
    ));
    body.extend_from_slice(&encode_event_frame(
        "event",
        "contentBlockDelta",
        r#"{"contentBlockIndex":2,"delta":{"toolUse":{"input":"{\"q\":"}}}"#,
    ));
    body.extend_from_slice(&encode_event_frame(
        "event",
        "contentBlockDelta",
        r#"{"contentBlockIndex":2,"delta":{"toolUse":{"input":"\"rust\"}"}}}"#,
    ));
    body.extend_from_slice(&encode_event_frame(
        "event",
        "contentBlockStop",
        r#"{"contentBlockIndex":2}"#,
    ));
    body.extend_from_slice(&encode_event_frame(
        "event",
        "messageStop",
        r#"{"messageStop":{"stopReason":"tool_use"}}"#,
    ));
    body.extend_from_slice(&encode_event_frame("event", "metadata", r#"{"metadata":{"usage":{"inputTokens":11,"outputTokens":7,"cacheReadInputTokens":3,"cacheWriteInputTokens":4,"totalTokens":18}}}"#));

    let model = claude_model();
    let events = parse_bedrock_event_stream(&body, &model).expect("events");
    assert_eq!(
        event_names(&events),
        vec![
            "start",
            "thinking_start",
            "thinking_delta",
            "thinking_end",
            "text_start",
            "text_delta",
            "toolcall_start",
            "toolcall_delta",
            "toolcall_delta",
            "toolcall_end",
            "done"
        ]
    );
    let final_message = text_event_message(&events);
    assert_eq!(final_message.stop_reason, StopReason::ToolUse);
    assert_eq!(final_message.raw_stop_reason.as_deref(), Some("tool_use"));
    assert_eq!(final_message.usage.input, 11);
    assert_eq!(final_message.usage.output, 7);
    assert_eq!(final_message.usage.cache_read, 3);
    assert_eq!(final_message.usage.cache_write, 4);
    assert_eq!(final_message.usage.total_tokens, 18);
    assert!(
        matches!(&final_message.content[0], ContentBlock::Thinking(t) if t.thinking == "thinking hard")
    );
    assert!(matches!(&final_message.content[1], ContentBlock::Text(t) if t.text == "Hello"));
    match &final_message.content[2] {
        ContentBlock::ToolCall(call) => {
            assert_eq!(call.name, "search");
            assert_eq!(
                call.arguments.get("q").and_then(Value::as_str),
                Some("rust")
            );
        }
        other => panic!("expected tool call, got {other:?}"),
    }
}

#[test]
fn bedrock_event_stream_exception_and_incomplete_frame_rejection() {
    let model = claude_model();
    let mut body = encode_event_frame(
        "exception",
        "ThrottlingException",
        r#"{"Message":"Rate exceeded"}"#,
    );
    let error = parse_bedrock_event_stream(&body, &model).expect_err("exception");
    assert!(error
        .to_string()
        .contains("Throttling error: Rate exceeded"));

    // A truncated frame buffers until complete; an absurd prelude fails fast.
    body = vec![0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0];
    let error = parse_bedrock_event_stream(&body, &model).expect_err("bad prelude");
    assert!(error.to_string().contains("malformed frame prelude"));
}

// ---------------------------------------------------------------------------
// google-vertex
// ---------------------------------------------------------------------------

#[test]
fn vertex_express_url() {
    let mut model = model("google-vertex", "google-vertex", "gemini-3.5-pro");
    // Catalog placeholder base URL falls back to the express endpoint.
    model.base_url = "https://{location}-aiplatform.googleapis.com".into();
    assert_eq!(
        resolve_vertex_url(&model),
        "https://aiplatform.googleapis.com/v1/publishers/google/models/gemini-3.5-pro:streamGenerateContent?alt=sse"
    );
    // A concrete custom base URL wins.
    model.base_url = "https://my-vpc-endpoint.example.com".into();
    assert_eq!(
        resolve_vertex_url(&model),
        "https://my-vpc-endpoint.example.com/v1/publishers/google/models/gemini-3.5-pro:streamGenerateContent?alt=sse"
    );
}

#[test]
fn vertex_body_matches_gemini_shape() {
    let mut model = model("google-vertex", "google-vertex", "gemini-2.5-pro");
    model.reasoning = true;
    let context = Context {
        system_prompt: Some("You are helpful.".into()),
        messages: vec![user_message("Hello")],
        tools: None,
    };
    let options = StreamOptions {
        reasoning: Some(ThinkingLevel::High),
        temperature: Some(0.3),
        max_tokens: Some(512),
        ..StreamOptions::default()
    };
    let body =
        pir::ai::api::google_vertex::build_vertex_body(&model, &context, &options).expect("body");
    assert_eq!(
        body["systemInstruction"],
        json!({"parts": [{"text": "You are helpful."}]})
    );
    assert_eq!(
        body["generationConfig"]["thinkingConfig"],
        json!({"includeThoughts": true, "thinkingBudget": 32768})
    );
    assert_eq!(body["generationConfig"]["temperature"], 0.3);
    assert_eq!(body["generationConfig"]["maxOutputTokens"], 512);
}

// ---------------------------------------------------------------------------
// mistral-conversations
// ---------------------------------------------------------------------------

#[test]
fn mistral_payload_and_wire_remap() {
    let mut model = model("mistral-conversations", "mistral", "mistral-large-latest");
    model.reasoning = true;
    let context = Context {
        system_prompt: Some("You are helpful.".into()),
        messages: vec![user_message("Hello")],
        tools: None,
    };
    let mut normalizer = MistralToolCallIdNormalizer::new();
    let options = StreamOptions {
        session_id: Some("sess-1".into()),
        reasoning: Some(ThinkingLevel::High),
        max_tokens: Some(512),
        temperature: Some(0.4),
        ..StreamOptions::default()
    };
    let payload =
        build_mistral_payload(&model, &context, &options, &mut normalizer).expect("payload");
    assert_eq!(payload["model"], "mistral-large-latest");
    assert_eq!(payload["stream"], true);
    assert_eq!(payload["maxTokens"], 512);
    assert_eq!(payload["promptCacheKey"], "sess-1");
    assert_eq!(
        payload["messages"][0],
        json!({"role": "system", "content": "You are helpful."})
    );
    // Thinking-level-map-less reasoning models use prompt_mode + reasoning_effort.
    assert_eq!(payload["promptMode"], "reasoning");
    assert_eq!(payload["reasoningEffort"], "high");

    let wire = to_mistral_wire_payload(&payload);
    assert_eq!(wire["max_tokens"], 512);
    assert_eq!(wire["prompt_cache_key"], "sess-1");
    assert_eq!(wire["prompt_mode"], "reasoning");
    assert_eq!(wire["reasoning_effort"], "high");
    assert!(wire.get("maxTokens").is_none());
}

#[test]
fn mistral_tool_call_ids_are_nine_alphanumerics() {
    let mut normalizer = MistralToolCallIdNormalizer::new();
    let a = normalizer.normalize("call_abc|fc_item_1");
    let b = normalizer.normalize("call_abc|fc_item_1");
    assert_eq!(a, b, "stable per original id");
    assert_eq!(a.chars().count(), MISTRAL_TOOL_CALL_ID_LENGTH);
    assert!(a.chars().all(|c| c.is_ascii_alphanumeric()));
    // Distinct ids never collide; the map is reversible.
    let c = normalizer.normalize("totally different id");
    assert_ne!(a, c);
}

#[test]
fn mistral_sse_text_and_usage() {
    let sse = concat!(
        "data: {\"id\":\"cmpl-1\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"Hello\"}}]}\n\n",
        "data: {\"id\":\"cmpl-1\",\"choices\":[{\"index\":0,\"delta\":{\"content\":[{\"type\":\"thinking\",\"thinking\":[{\"type\":\"text\",\"text\":\"pondering\"}]}]}}]}\n\n",
        "data: {\"id\":\"cmpl-1\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\" again\"},\"finish_reason\":null}]}\n\n",
        "data: {\"id\":\"cmpl-1\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":20,\"completion_tokens\":5,\"total_tokens\":25,\"prompt_tokens_details\":{\"cached_tokens\":6}}}\n\n",
        "data: [DONE]\n\n",
    );
    let model = model("mistral-conversations", "mistral", "mistral-large-latest");
    let events = parse_mistral_sse(sse.as_bytes(), &model).expect("events");
    assert_eq!(
        event_names(&events),
        vec![
            "start",
            "text_start",
            "text_delta",
            "text_end",
            "thinking_start",
            "thinking_delta",
            "thinking_end",
            "text_start",
            "text_delta",
            "text_end",
            "done"
        ]
    );
    let final_message = text_event_message(&events);
    assert_eq!(final_message.response_id.as_deref(), Some("cmpl-1"));
    assert_eq!(final_message.stop_reason, StopReason::Stop);
    // Cached prompt tokens subtract from input.
    assert_eq!(final_message.usage.input, 14);
    assert_eq!(final_message.usage.cache_read, 6);
    assert_eq!(final_message.usage.output, 5);
    assert_eq!(final_message.usage.total_tokens, 25);
}

#[test]
fn mistral_sse_tool_calls_with_partial_json() {
    let sse = concat!(
        "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"\"}}]}\n\n",
        "data: {\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"abc123def\",\"function\":{\"name\":\"search\",\"arguments\":\"\"}}]}}]}\n\n",
        "data: {\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"{\\\"q\\\":\\\"ru\"}}]}}]}\n\n",
        "data: {\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"st\\\"}\"}}]}}]}\n\n",
        "data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n",
        "data: [DONE]\n\n",
    );
    let model = model("mistral-conversations", "mistral", "mistral-large-latest");
    let events = parse_mistral_sse(sse.as_bytes(), &model).expect("events");
    let final_message = text_event_message(&events);
    assert_eq!(final_message.stop_reason, StopReason::ToolUse);
    assert_eq!(final_message.raw_stop_reason.as_deref(), Some("tool_calls"));
    match &final_message.content[0] {
        ContentBlock::ToolCall(call) => {
            assert_eq!(call.id, "abc123def");
            assert_eq!(call.name, "search");
            assert_eq!(
                call.arguments.get("q").and_then(Value::as_str),
                Some("rust")
            );
        }
        other => panic!("expected tool call, got {other:?}"),
    }
}

#[test]
fn mistral_stop_reason_mapping() {
    assert_eq!(map_mistral_stop_reason("stop").0, StopReason::Stop);
    assert_eq!(map_mistral_stop_reason("length").0, StopReason::Length);
    assert_eq!(
        map_mistral_stop_reason("model_length").0,
        StopReason::Length
    );
    assert_eq!(map_mistral_stop_reason("tool_calls").0, StopReason::ToolUse);
    let (stop, message) = map_mistral_stop_reason("error");
    assert_eq!(stop, StopReason::Error);
    assert_eq!(
        message.as_deref(),
        Some("Provider stopped with: error (server error)")
    );
}

// ---------------------------------------------------------------------------
// Catalog un-gating
// ---------------------------------------------------------------------------

#[test]
fn catalog_registers_the_new_api_ids_as_supported() {
    for api in [
        "openai-responses",
        "openai-codex-responses",
        "azure-openai-responses",
        "bedrock-converse-stream",
        "google-vertex",
        "mistral-conversations",
    ] {
        assert!(
            catalog::supported_apis().contains(&api),
            "{api} should be supported"
        );
    }
}

#[test]
fn catalog_entries_build_runtime_providers_for_new_apis() {
    for provider_id in [
        "openai",
        "openai-codex",
        "azure",
        "amazon-bedrock",
        "google-vertex",
        "mistral",
    ] {
        let entry = catalog::load_provider(provider_id)
            .expect("catalog load")
            .unwrap_or_else(|| panic!("{provider_id} in catalog"));
        let provider = pir::ai::providers::construction::CatalogProviderEntry::from_catalog(&entry)
            .unwrap_or_else(|error| panic!("{provider_id} builds: {error}"));
        assert_eq!(provider.id(), provider_id);
        assert!(!provider.get_models().is_empty());
    }
}

// ---------------------------------------------------------------------------
// Bedrock request-level thinking configuration (upstream `thinkingDisplay` /
// `interleavedThinking`)
// ---------------------------------------------------------------------------

#[test]
fn bedrock_request_options_control_thinking_display_and_interleaved_beta() {
    let mut model = model(
        "bedrock-converse-stream",
        "amazon-bedrock",
        "anthropic.claude-sonnet-4-5",
    );
    model.reasoning = true;
    let options = StreamOptions {
        reasoning: Some(ThinkingLevel::Low),
        max_tokens: Some(8192),
        thinking_display: Some(ThinkingDisplay::Omitted),
        interleaved_thinking: Some(false),
        ..StreamOptions::default()
    };
    let fields = pir::ai::api::bedrock::build_additional_model_request_fields_with_options(
        &model,
        Some(ThinkingLevel::Low),
        &options,
    )
    .expect("fields")
    .expect("fields set");
    assert_eq!(fields["thinking"]["display"], "omitted");
    assert!(
        fields.get("anthropic_beta").is_none(),
        "interleavedThinking=false omits the beta: {fields}"
    );
    assert_eq!(fields["thinking"]["budget_tokens"], 2048);

    // GovCloud rejects the Claude `thinking.display` field entirely.
    let mut gov = model.clone();
    gov.id = "us-gov.anthropic.claude-sonnet-4-5".into();
    let fields = pir::ai::api::bedrock::build_additional_model_request_fields_with_options(
        &gov,
        Some(ThinkingLevel::Low),
        &options,
    )
    .expect("fields")
    .expect("fields set");
    assert!(
        fields["thinking"].get("display").is_none(),
        "GovCloud omits display: {fields}"
    );
}
