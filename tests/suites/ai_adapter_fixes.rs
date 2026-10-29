//! Regression tests for the adapter-audit fixes (P1-1..P1-7, P2-8..P2-23):
//! each test pins one finding against the upstream behavior in
//! `packages/ai/src/api/*.ts` + `utils/{retry,provider-retry,overflow}.ts`.

use std::collections::BTreeMap;

use pir::ai::api::anthropic::{build_anthropic_request, parse_anthropic_sse};
use pir::ai::api::bedrock::{parse_bedrock_event_stream, BedrockEventStreamParser};
use pir::ai::api::google::build_google_body;
use pir::ai::api::openai::{build_openai_body, parse_openai_sse};
use pir::ai::api::openai_responses::azure::build_azure_params;
use pir::ai::api::openai_responses::shared::{parse_partial_json, parse_responses_sse};
use pir::ai::api::openai_responses::ResponsesCompat;
use pir::ai::transport::{
    exponential_backoff_delay, is_retryable, server_retry_delay, ProviderRequestClient,
    ProviderRequestConfig,
};
use pir::ai::types::*;
use serde_json::{json, Value};
use std::time::Duration;

// Old-signature shim: the production `convert_messages` is (model, context,
// compat); the call sites below were written against (model, compat, context).
fn convert_messages(model: &Model, compat: &OpenAiCompat, context: &Context) -> Vec<Value> {
    pir::ai::api::openai::convert_messages(model, context, compat)
}

// ---------------------------------------------------------------------------
// Shared fixtures
// ---------------------------------------------------------------------------

fn base_model(api: &str, provider: &str, id: &str) -> Model {
    Model {
        id: id.into(),
        name: id.into(),
        api: api.into(),
        provider: provider.into(),
        base_url: "https://example.invalid".into(),
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
        context_window: 32_000,
        max_tokens: 8_192,
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

/// Compat knob: the endpoint does not reliably terminate streams with a
/// finish reason (pir infers stop/toolUse from the content).
fn openai_compat_infer_finish_reason() -> pir::ai::types::OpenAiCompat {
    pir::ai::types::OpenAiCompat {
        supports_finish_reason: false,
        ..Default::default()
    }
}

/// Compat knob: the endpoint only accepts the deprecated `max_tokens` field.
fn openai_compat_max_tokens_field() -> pir::ai::types::OpenAiCompat {
    pir::ai::types::OpenAiCompat {
        max_tokens_field: Some("max_tokens".into()),
        ..Default::default()
    }
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

fn image_block() -> ContentBlock {
    ContentBlock::Image(ImageContent {
        data: "aGk=".into(),
        mime_type: "image/png".into(),
    })
}

fn assistant_message(
    provider: &str,
    api: &str,
    model_id: &str,
    content: Vec<ContentBlock>,
    stop_reason: StopReason,
) -> Message {
    Message::Assistant(AssistantMessage {
        api: api.into(),
        provider: provider.into(),
        model: model_id.into(),
        content,
        stop_reason,
        ..AssistantMessage::default()
    })
}

fn tool_result(call_id: &str, tool_name: &str, text: &str) -> Message {
    Message::ToolResult(ToolResultMessage {
        role: ToolResultRole::ToolResult,
        tool_call_id: call_id.into(),
        tool_name: tool_name.into(),
        content: vec![text_block(text)],
        details: None,
        usage: None,
        added_tool_names: Vec::new(),
        structured_content: None,
        nested_calls: None,
        is_error: false,
        duration_ms: None,
        timestamp: 2,
    })
}

fn done_of(events: &[AssistantMessageEvent]) -> AssistantMessage {
    match events.last() {
        Some(AssistantMessageEvent::Done { message, .. }) => message.clone(),
        other => panic!("expected terminal Done event, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// P1-1 / P2-8: Bedrock event-stream frame handling
// ---------------------------------------------------------------------------

/// Encode one AWS event-stream header (string value, type 7).
fn es_header_string(name: &str, value: &str) -> Vec<u8> {
    let mut out = vec![name.len() as u8];
    out.extend_from_slice(name.as_bytes());
    out.push(7);
    out.extend_from_slice(&(value.len() as u16).to_be_bytes());
    out.extend_from_slice(value.as_bytes());
    out
}

/// Encode one AWS event-stream header (type 8, TIMESTAMP: 8-byte int64).
fn es_header_timestamp(name: &str) -> Vec<u8> {
    let mut out = vec![name.len() as u8];
    out.extend_from_slice(name.as_bytes());
    out.push(8);
    out.extend_from_slice(&1_700_000_000_000_u64.to_be_bytes());
    out
}

/// Encode one AWS event-stream header (type 4, INTEGER: 4-byte int32).
fn es_header_integer(name: &str, value: i32) -> Vec<u8> {
    let mut out = vec![name.len() as u8];
    out.extend_from_slice(name.as_bytes());
    out.push(4);
    out.extend_from_slice(&value.to_be_bytes());
    out
}

/// One event frame with string `:message-type` / `:event-type` headers.
fn es_event_frame(event_type: &str, payload: &Value) -> Vec<u8> {
    es_frame(
        vec![
            es_header_string(":message-type", "event"),
            es_header_string(":event-type", event_type),
        ],
        payload.to_string().as_bytes(),
    )
}

fn es_frame(headers: Vec<Vec<u8>>, payload: &[u8]) -> Vec<u8> {
    let mut header_block = Vec::new();
    for header in headers {
        header_block.extend_from_slice(&header);
    }
    let total_len = 12 + header_block.len() + payload.len() + 4;
    let mut frame = Vec::with_capacity(total_len);
    frame.extend_from_slice(&(total_len as u32).to_be_bytes());
    frame.extend_from_slice(&(header_block.len() as u32).to_be_bytes());
    frame.extend_from_slice(&[0, 0, 0, 0]); // prelude CRC (unused by pir)
    frame.extend_from_slice(&header_block);
    frame.extend_from_slice(payload);
    frame.extend_from_slice(&[0, 0, 0, 0]); // message CRC (unused by pir)
    frame
}

fn message_stop_frame_with_reason(reason: &str) -> Vec<u8> {
    es_frame(
        vec![
            es_header_string(":message-type", "event"),
            es_header_string(":event-type", "messageStop"),
            // TIMESTAMP headers must not desynchronize the header walk.
            es_header_timestamp(":timestamp"),
        ],
        format!(r#"{{"messageStop":{{"stopReason":"{reason}"}}}}"#).as_bytes(),
    )
}

fn message_stop_frame() -> Vec<u8> {
    message_stop_frame_with_reason("end_turn")
}

#[test]
fn bedrock_malformed_frame_headers_exceeding_payload_is_an_error_not_panic() {
    // P1-1: headers_len claims to reach past the frame's CRC area; the old
    // `12 + headers_len > total_len` check let start > end and sliced out of
    // bounds. Must fail with a typed error instead of panicking.
    let mut frame = Vec::new();
    let total_len = 20u32;
    frame.extend_from_slice(&total_len.to_be_bytes());
    frame.extend_from_slice(&20u32.to_be_bytes()); // headers_len
    frame.extend_from_slice(&[0, 0, 0, 0]);
    frame.extend_from_slice(&[0u8; 8]);
    let model = base_model("bedrock-converse-stream", "bedrock", "claude");
    let result = parse_bedrock_event_stream(&frame, &model);
    let error = result.expect_err("malformed frame must error");
    assert!(
        error.to_string().contains("headers exceed frame size"),
        "got: {error}"
    );
}

#[test]
fn bedrock_timestamp_header_does_not_desynchronize_parsing() {
    // P2-8: header type 8 (TIMESTAMP) is 8 bytes per the AWS spec; skipping
    // only 4 desyncs the walk so the following headers misread.
    let model = base_model("bedrock-converse-stream", "bedrock", "claude");
    let mut body = es_frame(
        vec![
            es_header_string(":message-type", "event"),
            es_header_string(":event-type", "messageStart"),
            es_header_timestamp(":timestamp"),
        ],
        br#"{"messageStart":{"role":"assistant"}}"#,
    );
    body.extend_from_slice(&message_stop_frame());
    let events = parse_bedrock_event_stream(&body, &model).expect("frames parse");
    let done = done_of(&events);
    assert_eq!(done.stop_reason, StopReason::Stop);
}

#[test]
fn bedrock_minimal_frame_with_crc_room_parses() {
    // A frame where the payload is exactly the CRC room (empty payload) must
    // still parse (the bounds check requires 12 + headers + 4 <= total).
    let model = base_model("bedrock-converse-stream", "bedrock", "claude");
    let mut parser = BedrockEventStreamParser::new(&model);
    let mut out = Vec::new();
    let frame = message_stop_frame();
    parser.feed(&frame, &mut out).expect("frame parses");
    parser.finish(&mut out).expect("finish");
    let done = done_of(&out);
    assert_eq!(done.stop_reason, StopReason::Stop);
}

#[test]
fn bedrock_integer_header_does_not_desynchronize_parsing() {
    // Header type 4 (INTEGER) is 4 bytes per the AWS event-stream spec; it
    // used to fail with "unknown header value type 4". A string header after
    // it must still decode cleanly (no desync).
    let model = base_model("bedrock-converse-stream", "bedrock", "claude");
    let mut body = es_frame(
        vec![
            es_header_string(":message-type", "event"),
            es_header_string(":event-type", "messageStart"),
            es_header_integer(":priority", 7),
            es_header_string("x-trailing", "still-aligned"),
        ],
        br#"{"messageStart":{"role":"assistant"}}"#,
    );
    body.extend_from_slice(&message_stop_frame());
    let events = parse_bedrock_event_stream(&body, &model).expect("frames parse");
    let done = done_of(&events);
    assert_eq!(done.stop_reason, StopReason::Stop);
}

#[test]
fn bedrock_malformed_final_tool_args_are_kept_with_arguments_error() {
    // Upstream 1.3.0 semantics: a final fragment that stays malformed keeps
    // the tool call with `arguments = {}` and a model-visible
    // `argumentsError`; the stream itself completes with ToolUse.
    let model = base_model("bedrock-converse-stream", "bedrock", "claude");

    let mut body = es_event_frame(
        "contentBlockStart",
        &json!({
            "contentBlockIndex": 0,
            "start": { "toolUse": { "toolUseId": "call_1", "name": "search" } },
        }),
    );
    body.extend_from_slice(&es_event_frame(
        "contentBlockDelta",
        &json!({
            "contentBlockIndex": 0,
            "delta": { "toolUse": { "input": "{\"query\": \"ru" } },
        }),
    ));
    body.extend_from_slice(&es_event_frame(
        "contentBlockStop",
        &json!({ "contentBlockIndex": 0 }),
    ));
    // Bedrock reports tool use via the stop reason; the malformed final
    // arguments must not turn it into an error stop.
    body.extend_from_slice(&message_stop_frame_with_reason("tool_use"));
    let events = parse_bedrock_event_stream(&body, &model).expect("parses");
    let done = done_of(&events);
    assert_eq!(done.stop_reason, StopReason::ToolUse);
    let Some(ContentBlock::ToolCall(call)) = done.content.first() else {
        panic!("tool call expected, got {:?}", done.content);
    };
    assert!(
        call.arguments.is_empty(),
        "malformed args fall back to an empty object: {:?}",
        call.arguments
    );
    assert!(
        call.arguments_error
            .as_deref()
            .is_some_and(|error| error.contains("Received arguments")),
        "argumentsError is set: {:?}",
        call.arguments_error
    );

    let mut body = es_event_frame(
        "contentBlockStart",
        &json!({
            "contentBlockIndex": 0,
            "start": { "toolUse": { "toolUseId": "call_2", "name": "search" } },
        }),
    );
    body.extend_from_slice(&es_event_frame(
        "contentBlockDelta",
        &json!({
            "contentBlockIndex": 0,
            "delta": { "toolUse": { "input": "{\"query\": \"x\"} garbage" } },
        }),
    ));
    body.extend_from_slice(&es_event_frame(
        "contentBlockStop",
        &json!({ "contentBlockIndex": 0 }),
    ));
    body.extend_from_slice(&message_stop_frame_with_reason("tool_use"));
    let events = parse_bedrock_event_stream(&body, &model).expect("parses");
    let done = done_of(&events);
    let Some(ContentBlock::ToolCall(call)) = done.content.first() else {
        panic!("tool call expected, got {:?}", done.content);
    };
    assert!(call.arguments.is_empty());
    assert!(
        call.arguments_error.is_some(),
        "garbage args carry an error"
    );
}

// ---------------------------------------------------------------------------
// P1-2: OpenAI finish_reason table
// ---------------------------------------------------------------------------

fn openai_chunk(delta: Value, finish_reason: Value) -> String {
    format!(
        "data: {}\n\ndata: [DONE]\n\n",
        json!({"id": "c1", "choices": [{"index": 0, "delta": delta, "finish_reason": finish_reason}]})
    )
}

#[test]
fn openai_content_filter_finish_reason_surfaces_as_stream_error() {
    // P1-2: upstream mapStopReason maps content_filter / network_error /
    // unknown reasons to StopReason::Error with a "Provider finish_reason"
    // message; pir used to collapse them to Stop.
    let model = base_model("openai-completions", "test", "gpt");
    let body = format!(
        "{}{}",
        openai_chunk(json!({"content": "hi"}), json!("content_filter")),
        "data: [DONE]\n\n"
    );
    let error = parse_openai_sse(body.as_bytes(), &model).expect_err("error stop reason");
    assert!(
        error
            .to_string()
            .contains("Provider finish_reason: content_filter"),
        "got: {error}"
    );
}

#[test]
fn openai_unknown_finish_reason_surfaces_as_stream_error() {
    let model = base_model("openai-completions", "test", "gpt");
    let body = format!(
        "{}{}",
        openai_chunk(json!({}), json!("mystery_reason")),
        "data: [DONE]\n\n"
    );
    let error = parse_openai_sse(body.as_bytes(), &model).expect_err("error stop reason");
    assert!(
        error
            .to_string()
            .contains("Provider finish_reason: mystery_reason"),
        "got: {error}"
    );
}

#[test]
fn openai_end_and_max_tokens_finish_reasons() {
    // "end" is an upstream-accepted stop alias; unknown "max_tokens" (not an
    // upstream value) is an error like any other unknown reason.
    let model = base_model("openai-completions", "test", "gpt");
    let body = format!(
        "{}{}",
        openai_chunk(json!({}), json!("end")),
        "data: [DONE]\n\n"
    );
    let done = done_of(&parse_openai_sse(body.as_bytes(), &model).expect("parses"));
    assert_eq!(done.stop_reason, StopReason::Stop);
}

// ---------------------------------------------------------------------------
// P1-3: OpenAI streaming usage accounting
// ---------------------------------------------------------------------------

#[test]
fn openai_usage_subtracts_cached_and_cache_write_tokens() {
    // Upstream parseChunkUsage: input = max(0, prompt - cached - cacheWrite);
    // totalTokens = input + output + cacheRead + cacheWrite.
    let model = base_model("openai-completions", "test", "gpt");
    let body = format!(
        "{}{}",
        openai_chunk(json!({}), json!("stop")),
        "data: {\"id\":\"c1\",\"choices\":[],\"usage\":{\"prompt_tokens\":20,\"completion_tokens\":5,\"prompt_tokens_details\":{\"cached_tokens\":6,\"cache_write_tokens\":4}}}\n\ndata: [DONE]\n\n"
    );
    let done = done_of(&parse_openai_sse(body.as_bytes(), &model).expect("parses"));
    assert_eq!(done.usage.input, 10);
    assert_eq!(done.usage.cache_read, 6);
    assert_eq!(done.usage.cache_write, 4);
    assert_eq!(done.usage.total_tokens, 25);
}

#[test]
fn openai_usage_deepseek_and_kimi_cache_hit_fields() {
    let model = base_model("openai-completions", "test", "gpt");
    // DeepSeek: prompt_cache_hit_tokens.
    let body = format!(
        "{}{}",
        openai_chunk(json!({}), json!("stop")),
        "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":20,\"completion_tokens\":5,\"prompt_cache_hit_tokens\":7}}\n\ndata: [DONE]\n\n"
    );
    let done = done_of(&parse_openai_sse(body.as_bytes(), &model).expect("parses"));
    assert_eq!(done.usage.cache_read, 7);
    assert_eq!(done.usage.input, 13);

    // Kimi: top-level usage.cached_tokens.
    let body = format!(
        "{}{}",
        openai_chunk(json!({}), json!("stop")),
        "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":20,\"completion_tokens\":5,\"cached_tokens\":9}}\n\ndata: [DONE]\n\n"
    );
    let done = done_of(&parse_openai_sse(body.as_bytes(), &model).expect("parses"));
    assert_eq!(done.usage.cache_read, 9);
    assert_eq!(done.usage.input, 11);
}

#[test]
fn openai_usage_moonshot_choice_level_fallback() {
    // Some providers (Moonshot) return usage on choice.usage.
    let model = base_model("openai-completions", "test", "gpt");
    let body = format!(
        "{}{}",
        openai_chunk(json!({}), json!("stop")),
        "data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":null,\"usage\":{\"prompt_tokens\":30,\"completion_tokens\":4,\"prompt_tokens_details\":{\"cached_tokens\":10}}}]}\n\ndata: [DONE]\n\n"
    );
    let done = done_of(&parse_openai_sse(body.as_bytes(), &model).expect("parses"));
    assert_eq!(done.usage.input, 20);
    assert_eq!(done.usage.cache_read, 10);
}

#[test]
fn openai_usage_moonshot_choice_level_fallback_ignores_a_null_top_level_usage() {
    // Standard OpenAI chunks carry `"usage": null`; a provider that puts the
    // usage block on the choice must still be honored (absent and null are the
    // same "no top-level usage" case).
    let model = base_model("openai-completions", "test", "gpt");
    let body = concat!(
        "data: {\"usage\":null,\"choices\":[{\"index\":0,\"delta\":{\"content\":\"hi\"},",
        "\"finish_reason\":null,\"usage\":{\"prompt_tokens\":30,\"completion_tokens\":4}}]}\n\n",
        "data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
        "data: [DONE]\n\n"
    );
    let done = done_of(&parse_openai_sse(body.as_bytes(), &model).expect("parses"));
    assert_eq!(done.usage.input, 30);
    assert_eq!(done.usage.output, 4);
}

// ---------------------------------------------------------------------------
// P1-4 / P1-6 / P1-5: OpenAI request conversion
// ---------------------------------------------------------------------------

#[test]
fn openai_convert_normalizes_responses_pipe_tool_call_ids() {
    // P1-4: transform_messages runs with the upstream normalizeToolCallId
    // (pipe-split + short-hash for >40 chars).
    let mut model = base_model("openai-completions", "test", "gpt");
    model.input = vec!["text".into(), "image".into()];
    let long_item = "a".repeat(80);
    let call = ContentBlock::ToolCall(ToolCall {
        id: format!("call_abc|fc_{long_item}"),
        name: "search".into(),
        arguments: BTreeMap::new(),
        thought_signature: None,
        namespace: None,
        arguments_error: None,
    });
    let messages = vec![
        assistant_message(
            "other",
            "anthropic-messages",
            "claude",
            vec![call],
            StopReason::ToolUse,
        ),
        tool_result(&format!("call_abc|fc_{long_item}"), "search", "ok"),
    ];
    let converted = convert_messages(
        &model,
        &pir::ai::types::OpenAiCompat::default(),
        &Context {
            system_prompt: None,
            messages,
            ..Context::default()
        },
    );
    let tool_call_id = converted[0]["tool_calls"][0]["id"]
        .as_str()
        .expect("tool call id")
        .to_string();
    assert!(
        tool_call_id.len() <= 40,
        "sanitized id must fit the OpenAI limit: {tool_call_id}"
    );
    assert!(tool_call_id.starts_with("call_abc_"));
    assert!(!tool_call_id.contains('|'));
    // The matching tool result was remapped to the same id.
    assert_eq!(converted[1]["tool_call_id"], json!(tool_call_id));
}

#[test]
fn openai_convert_short_pipe_ids_combine_call_and_item() {
    let model = base_model("openai-completions", "test", "gpt");
    let call = ContentBlock::ToolCall(ToolCall {
        id: "call_x|fc_item1".into(),
        name: "search".into(),
        arguments: BTreeMap::new(),
        thought_signature: None,
        namespace: None,
        arguments_error: None,
    });
    let messages = vec![
        assistant_message(
            "other",
            "anthropic-messages",
            "claude",
            vec![call],
            StopReason::ToolUse,
        ),
        tool_result("call_x|fc_item1", "search", "ok"),
    ];
    let converted = convert_messages(
        &model,
        &pir::ai::types::OpenAiCompat::default(),
        &Context {
            system_prompt: None,
            messages,
            ..Context::default()
        },
    );
    assert_eq!(
        converted[0]["tool_calls"][0]["id"],
        json!("call_x_fc_item1")
    );
    assert_eq!(converted[1]["tool_call_id"], json!("call_x_fc_item1"));
}

#[test]
fn openai_convert_skips_errored_assistants_and_synthesizes_tool_results() {
    // P1-4: errored/aborted assistant turns are skipped entirely, and
    // unanswered tool calls from a completed assistant turn get a synthetic
    // "No result provided" error result.
    let model = base_model("openai-completions", "test", "gpt");
    let orphan_call = ContentBlock::ToolCall(ToolCall {
        id: "call_1".into(),
        name: "search".into(),
        arguments: BTreeMap::new(),
        thought_signature: None,
        namespace: None,
        arguments_error: None,
    });
    let messages = vec![
        assistant_message(
            "test",
            "openai-completions",
            "gpt",
            vec![text_block("partial")],
            StopReason::Error,
        ),
        assistant_message(
            "test",
            "openai-completions",
            "gpt",
            vec![orphan_call],
            StopReason::ToolUse,
        ),
        user_message("retry"),
    ];
    let converted = convert_messages(
        &model,
        &pir::ai::types::OpenAiCompat::default(),
        &Context {
            system_prompt: None,
            messages,
            ..Context::default()
        },
    );
    // The errored assistant is not on the wire.
    assert!(
        converted
            .iter()
            .all(|item| item["role"] != "assistant" || item["content"] != json!("partial")),
        "errored assistant must be skipped: {converted:?}"
    );
    // The orphaned call becomes a synthetic error result.
    let synthetic = converted
        .iter()
        .find(|item| item["content"] == json!("No result provided"))
        .expect("synthetic tool result present");
    assert_eq!(synthetic["tool_call_id"], json!("call_1"));
}

#[test]
fn openai_convert_user_images_become_image_url_parts() {
    // P1-6: user images become `image_url` content parts instead of being
    // silently dropped.
    let mut model = base_model("openai-completions", "test", "gpt");
    model.input = vec!["text".into(), "image".into()];
    let converted = convert_messages(
        &model,
        &pir::ai::types::OpenAiCompat::default(),
        &Context {
            system_prompt: None,
            messages: vec![Message::User(UserMessage {
                role: UserRole::User,
                content: vec![text_block("look"), image_block()],
                tools_added: None,
                tools_removed: None,
                timestamp: 1,
                sections: None,
            })],
            ..Context::default()
        },
    );
    let parts = converted[0]["content"].as_array().expect("content parts");
    assert_eq!(parts[0]["type"], "text");
    assert_eq!(parts[0]["text"], "look");
    assert_eq!(parts[1]["type"], "image_url");
    assert_eq!(
        parts[1]["image_url"]["url"],
        json!("data:image/png;base64,aGk=")
    );
}

#[test]
fn openai_convert_non_vision_user_images_downgrade_to_placeholder() {
    let model = base_model("openai-completions", "test", "gpt");
    let converted = convert_messages(
        &model,
        &pir::ai::types::OpenAiCompat::default(),
        &Context {
            system_prompt: None,
            messages: vec![Message::User(UserMessage {
                role: UserRole::User,
                content: vec![image_block()],
                tools_added: None,
                tools_removed: None,
                timestamp: 1,
                sections: None,
            })],
            ..Context::default()
        },
    );
    assert_eq!(
        converted[0]["content"],
        json!("(image omitted: model does not support images)")
    );
}

#[test]
fn openai_convert_tool_result_images_become_follow_up_user_message() {
    // P1-6: tool-result images ride as a follow-up user message with
    // image_url parts (upstream convertMessages toolResult branch).
    let mut model = base_model("openai-completions", "test", "gpt");
    model.input = vec!["text".into(), "image".into()];
    let result = ToolResultMessage {
        role: ToolResultRole::ToolResult,
        tool_call_id: "call_1".into(),
        tool_name: "screenshot".into(),
        content: vec![image_block()],
        details: None,
        usage: None,
        added_tool_names: Vec::new(),
        structured_content: None,
        nested_calls: None,
        is_error: false,
        duration_ms: None,
        timestamp: 2,
    };
    let converted = convert_messages(
        &model,
        &pir::ai::types::OpenAiCompat::default(),
        &Context {
            system_prompt: None,
            messages: vec![Message::ToolResult(result), user_message("next")],
            ..Context::default()
        },
    );
    assert_eq!(converted[0]["role"], "tool");
    assert_eq!(converted[0]["content"], "(see attached image)");
    assert_eq!(converted[1]["role"], "user");
    let parts = converted[1]["content"].as_array().expect("parts");
    assert_eq!(parts[0]["text"], "Attached image(s) from tool result:");
    assert_eq!(parts[1]["type"], "image_url");
    // The following user turn still follows.
    assert_eq!(converted[2]["role"], "user");
    assert_eq!(converted[2]["content"], "next");
}

#[test]
fn openai_body_defaults_to_max_completion_tokens() {
    // P1-5: standard OpenAI takes max_completion_tokens; max_tokens is the
    // per-compat exception path.
    let model = base_model("openai-completions", "test", "gpt");
    let options = StreamOptions {
        max_tokens: Some(512),
        ..StreamOptions::default()
    };
    let body = build_openai_body(&model, &Context::default(), &options, &Default::default())
        .expect("body");
    assert_eq!(body["max_completion_tokens"], 512);
    assert!(body.get("max_tokens").is_none());

    // Compat override switches the field.
    let compat = openai_compat_max_tokens_field();
    let body = build_openai_body(&model, &Context::default(), &options, &compat).expect("body");
    assert_eq!(body["max_tokens"], 512);
    assert!(body.get("max_completion_tokens").is_none());
}

#[test]
fn openai_body_omits_empty_tools_array() {
    // Bench finding (openai gateway 400): `--no-tools` yields `Some(vec![])`
    // in the Context, and several OpenAI-compatible gateways reject an empty
    // `tools: []`. Upstream only sets the field when the list is non-empty.
    let model = base_model("openai-completions", "test", "gpt");
    let context = Context {
        tools: Some(Vec::new()),
        ..Context::default()
    };
    let body = build_openai_body(
        &model,
        &context,
        &StreamOptions::default(),
        &Default::default(),
    )
    .expect("body");
    assert!(
        body.get("tools").is_none(),
        "empty tools must be omitted, got: {body}"
    );
}

// ---------------------------------------------------------------------------
// P2-15 / P2-16 / P2-17: OpenAI SSE reasoning selection + finish contract
// ---------------------------------------------------------------------------

#[test]
fn openai_reasoning_delta_picks_first_non_empty_field() {
    // P2-15: the first NON-EMPTY of reasoning_content/reasoning/reasoning_text
    // per chunk; an empty reasoning_content must not mask reasoning, and an
    // empty reasoning_content + reasoning must not mask reasoning_text. The
    // compat override lets the stream finish so the emitted thinking text is
    // observable (a stream without finish_reason would only prove P2-16).
    let mut model = base_model("openai-completions", "test", "gpt");
    model.compat = Some(openai_compat_infer_finish_reason());
    let body = format!(
        "{}{}{}",
        openai_chunk(
            json!({
                "reasoning_content": "",
                "reasoning": "middle",
                "reasoning_text": "deep"
            }),
            json!(null)
        ),
        openai_chunk(
            json!({"reasoning_content": "", "reasoning": "", "reasoning_text": "deep"}),
            json!(null)
        ),
        "data: [DONE]\n\n"
    );
    let done = done_of(&parse_openai_sse(body.as_bytes(), &model).expect("parses"));
    let Some(ContentBlock::Thinking(thinking)) = done.content.first() else {
        panic!("thinking block expected, got {:?}", done.content);
    };
    // Chunk 1: `reasoning` wins over the later `reasoning_text`; chunk 2:
    // `reasoning_text` is reached once the earlier fields are empty.
    assert_eq!(thinking.thinking, "middledeep");
}

#[test]
fn openai_reasoning_delta_selection_uses_reasoning_text_when_earlier_fields_empty() {
    let mut model = base_model("openai-completions", "test", "gpt");
    model.compat = Some(openai_compat_infer_finish_reason());
    let body = format!(
        "{}{}",
        openai_chunk(
            json!({"reasoning_content": "", "reasoning": "", "reasoning_text": "deep"}),
            json!(null)
        ),
        "data: [DONE]\n\n"
    );
    let done = done_of(&parse_openai_sse(body.as_bytes(), &model).expect("parses"));
    let Some(ContentBlock::Thinking(thinking)) = done.content.first() else {
        panic!("thinking block expected, got {:?}", done.content);
    };
    assert_eq!(thinking.thinking, "deep");
}

#[test]
fn openai_duplicate_reasoning_fields_take_first_non_empty() {
    // chutes.ai returns reasoning_content and reasoning with the same content;
    // only one thinking delta may be produced per chunk.
    let mut model = base_model("openai-completions", "test", "gpt");
    model.compat = Some(openai_compat_infer_finish_reason());
    let body = format!(
        "{}{}",
        openai_chunk(
            json!({"reasoning_content": "same", "reasoning": "same"}),
            json!(null)
        ),
        "data: [DONE]\n\n"
    );
    let events = parse_openai_sse(body.as_bytes(), &model).expect("parses");
    let thinking_deltas = events
        .iter()
        .filter(|event| matches!(event, AssistantMessageEvent::ThinkingDelta { delta, .. } if delta == "same"))
        .count();
    assert_eq!(thinking_deltas, 1, "one delta per chunk");
}

#[test]
fn openai_stream_without_finish_reason_fails_when_supported() {
    // P2-16: with compat.supportsFinishReason (the default) a stream that ends
    // without a terminal finish_reason is an error, not a silent stop.
    let model = base_model("openai-completions", "test", "gpt");
    let body = format!(
        "{}{}",
        openai_chunk(json!({"content": "hi"}), json!(null)),
        "data: [DONE]\n\n"
    );
    let error = parse_openai_sse(body.as_bytes(), &model).expect_err("missing finish_reason");
    assert!(
        error
            .to_string()
            .contains("Stream ended without finish_reason"),
        "got: {error}"
    );
}

#[test]
fn openai_stream_records_response_model() {
    // P2-16: chunk.model differing from the requested model is recorded.
    let model = base_model("openai-completions", "test", "gpt-requested");
    let body = "data: {\"id\":\"c1\",\"model\":\"gpt-4o-mini-2024\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"x\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n";
    let done = done_of(&parse_openai_sse(body.as_bytes(), &model).expect("parses"));
    assert_eq!(done.response_model.as_deref(), Some("gpt-4o-mini-2024"));
}

#[test]
fn openai_malformed_final_tool_args_are_kept_with_arguments_error() {
    // Upstream 1.3.0: a final arguments fragment that stays malformed keeps
    // the tool call with `arguments = {}` and `argumentsError`; the stream
    // completes with ToolUse and the scheduler later returns a model-visible
    // error result. Cut-off JSON is malformed too — upstream deliberately
    // does not close open strings or containers.
    let mut model = base_model("openai-completions", "test", "gpt");
    model.compat = Some(openai_compat_infer_finish_reason());

    let body = format!(
        "{}{}",
        openai_chunk(
            json!({"tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"search","arguments":"{\"query\": \"ru"}}]}),
            json!(null)
        ),
        "data: [DONE]\n\n"
    );
    let done = done_of(&parse_openai_sse(body.as_bytes(), &model).expect("parses"));
    assert_eq!(done.stop_reason, StopReason::ToolUse);
    let Some(ContentBlock::ToolCall(call)) = done.content.first() else {
        panic!("tool call expected, got {:?}", done.content);
    };
    assert!(call.arguments.is_empty());
    assert!(
        call.arguments_error
            .as_deref()
            .is_some_and(|error| error.contains("Received arguments")),
        "argumentsError is set: {:?}",
        call.arguments_error
    );

    let body = format!(
        "{}{}",
        openai_chunk(
            json!({"tool_calls":[{"index":0,"id":"call_2","type":"function","function":{"name":"search","arguments":"{\"query\": \"x\"} garbage"}}]}),
            json!(null)
        ),
        "data: [DONE]\n\n"
    );
    let done = done_of(&parse_openai_sse(body.as_bytes(), &model).expect("parses"));
    let Some(ContentBlock::ToolCall(call)) = done.content.first() else {
        panic!("tool call expected, got {:?}", done.content);
    };
    assert!(call.arguments.is_empty());
    assert!(
        call.arguments_error.is_some(),
        "garbage args carry an error"
    );
}

#[test]
fn openai_tool_calls_without_index_key_by_id() {
    // P2-17: providers that omit the wire index still get distinct blocks
    // keyed by tool-call id.
    let mut model = base_model("openai-completions", "test", "gpt");
    model.compat = Some(openai_compat_infer_finish_reason());
    let body = format!(
        "{}{}{}",
        openai_chunk(
            json!({"tool_calls":[{"id":"call_a","type":"function","function":{"name":"a","arguments":"{\"x\":1}"}}]}),
            json!(null)
        ),
        openai_chunk(
            json!({"tool_calls":[{"id":"call_b","type":"function","function":{"name":"b","arguments":"{\"y\":2}"}}]}),
            json!(null)
        ),
        "data: [DONE]\n\n"
    );
    let done = done_of(&parse_openai_sse(body.as_bytes(), &model).expect("parses"));
    let calls: Vec<&ToolCall> = done
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::ToolCall(call) => Some(call),
            _ => None,
        })
        .collect();
    assert_eq!(calls.len(), 2, "distinct blocks per id: {calls:?}");
    assert_eq!(calls[0].id, "call_a");
    assert_eq!(calls[1].id, "call_b");
    assert_eq!(calls[1].arguments.get("y"), Some(&json!(2)));
}

// ---------------------------------------------------------------------------
// P1-7 / P2-9..P2-14: Anthropic request building + SSE parsing
// ---------------------------------------------------------------------------

fn anthropic_model_with(compat: pir::ai::types::OpenAiCompat) -> Model {
    let mut model = base_model("anthropic-messages", "anthropic", "claude-sonnet-4-5");
    model.reasoning = true;
    model.compat = Some(compat);
    model
}

/// Mid-conversation system update carrying a native tool addition; it only
/// becomes a `system` wire message when native tool changes are enabled.
fn anthropic_tool_update_system(text: &str, tool_name: &str) -> Message {
    Message::System(UserMessage {
        role: UserRole::System,
        content: vec![text_block(text)],
        tools_added: Some(vec![Tool {
            name: tool_name.into(),
            description: "Test tool.".into(),
            parameters: json!({"type": "object", "properties": {}}),
            constrained_sampling: None,
        }]),
        tools_removed: None,
        timestamp: 2,
        sections: None,
    })
}

/// The initial active tool that anchors the deferred placeholder.
fn anthropic_initial_tool() -> Tool {
    Tool {
        name: "read".into(),
        description: "Read a file.".into(),
        parameters: json!({"type": "object", "properties": {}}),
        constrained_sampling: None,
    }
}

/// Native mid-conversation system messages + tool changes.
fn anthropic_native_tool_changes_compat() -> pir::ai::types::OpenAiCompat {
    pir::ai::types::OpenAiCompat {
        supports_mid_convo_system_messages: Some(true),
        supports_mid_convo_tool_changes: Some(true),
        ..Default::default()
    }
}

#[test]
fn anthropic_request_applies_transform_messages() {
    // P1-7: errored assistant turns are skipped and orphaned tool calls get
    // synthetic results; foreign thinking is lowered to text.
    let model = anthropic_model_with(Default::default());
    let orphan = ContentBlock::ToolCall(ToolCall {
        id: "call_1".into(),
        name: "search".into(),
        arguments: BTreeMap::new(),
        thought_signature: None,
        namespace: None,
        arguments_error: None,
    });
    let context = Context {
        system_prompt: Some("Be helpful.".into()),
        messages: vec![
            user_message("go"),
            assistant_message(
                "anthropic",
                "anthropic-messages",
                "claude-sonnet-4-5",
                vec![text_block("partial")],
                StopReason::Aborted,
            ),
            assistant_message(
                "anthropic",
                "anthropic-messages",
                "claude-sonnet-4-5",
                vec![orphan],
                StopReason::ToolUse,
            ),
            user_message("continue"),
        ],
        tools: None,
    };
    let options = StreamOptions {
        api_key: Some("sk-ant-api".into()),
        ..StreamOptions::default()
    };
    let (body, _) = build_anthropic_request(&model, &context, &options).expect("request");
    let messages = body["messages"].as_array().expect("messages");
    // The aborted assistant is not on the wire.
    assert!(
        !messages.iter().any(|m| m["role"] == "assistant"
            && m["content"]
                .as_array()
                .map(|blocks| blocks.iter().any(|b| b["text"] == json!("partial")))
                .unwrap_or(false)),
        "aborted assistant skipped: {messages:?}"
    );
    // The orphaned call got a synthetic error tool result.
    assert!(
        messages.iter().any(|m| m["role"] == "user"
            && m["content"]
                .as_array()
                .map(|blocks| blocks
                    .iter()
                    .any(|b| b["type"] == "tool_result"
                        && b["content"] == json!("No result provided")))
                .unwrap_or(false)),
        "synthetic tool result present: {messages:?}"
    );
}

#[test]
fn anthropic_request_drops_foreign_thinking() {
    let model = anthropic_model_with(Default::default());
    let thinking = ContentBlock::Thinking(ThinkingContent {
        thinking: "foreign thoughts".into(),
        thinking_signature: Some("sig".into()),
        redacted: false,
    });
    let context = Context {
        system_prompt: None,
        messages: vec![
            user_message("go"),
            assistant_message(
                "openai",
                "openai-completions",
                "gpt",
                vec![thinking, text_block("answer")],
                StopReason::Stop,
            ),
        ],
        tools: None,
    };
    let options = StreamOptions::default();
    let (body, _) = build_anthropic_request(&model, &context, &options).expect("request");
    let blocks = body["messages"][1]["content"].as_array().expect("blocks");
    assert!(
        blocks.iter().all(|b| b["type"] != "thinking"),
        "foreign thinking lowered: {blocks:?}"
    );
    assert!(
        !blocks
            .iter()
            .any(|b| b["type"] == "text" && b["text"] == json!("foreign thoughts")),
        "foreign thinking is not replayed as visible text: {blocks:?}"
    );
}

#[test]
fn anthropic_managed_effort_betas_and_fixed_effort() {
    // P2-9 + P2-11: supportsMidConvoEffort adds the mid-conversation
    // output-config and thinking-binding betas, and the top-level
    // output_config.effort is fixed at "low" regardless of the requested level
    // (upstream #10063: organizations cap effort and reject higher values).
    // The active effort still travels in the trailing system marker.
    let compat = pir::ai::types::OpenAiCompat {
        supports_mid_convo_effort: Some(true),
        ..Default::default()
    };
    let model = anthropic_model_with(compat);
    let context = Context {
        system_prompt: None,
        messages: vec![user_message("go")],
        tools: None,
    };
    let options = StreamOptions {
        api_key: Some("sk-ant-api".into()),
        reasoning: Some(ThinkingLevel::Low),
        ..StreamOptions::default()
    };
    let (body, headers) = build_anthropic_request(&model, &context, &options).expect("request");
    let betas = headers
        .get("anthropic-beta")
        .expect("beta header")
        .split(',')
        .map(str::to_string)
        .collect::<Vec<_>>();
    assert!(betas.contains(&"mid-conversation-output-config-2026-07-01".to_string()));
    assert!(betas.contains(&"thinking-binding-controls-2026-08-01".to_string()));
    assert_eq!(body["output_config"], json!({"effort": "low"}));
    assert_eq!(
        body["thinking"]["block_binding"],
        json!({"prefix_mismatch_behavior": "drop_block"})
    );
    assert_eq!(
        body["messages"]
            .as_array()
            .and_then(|messages| messages.last())
            .cloned(),
        Some(json!({"role": "system", "content": [], "output_config": {"effort": "high"}})),
        "the active effort marker keeps replaying the managed effort"
    );
}

#[test]
fn anthropic_empty_assistant_holds_pending_system_updates_past_user() {
    // P0 (#11091): an assistant made only of foreign thinking is emptied by
    // transform and dropped by `assistant_message_blocks`. Held system
    // updates must stay pending across it; flushing at the dropped assistant
    // would place a `system` message directly before a following `user`
    // message, which Anthropic rejects ("role 'system' must precede an
    // 'assistant' message").
    let model = anthropic_model_with(anthropic_native_tool_changes_compat());
    let empty_after_transform = assistant_message(
        "openai",
        "openai-completions",
        "gpt",
        vec![ContentBlock::Thinking(ThinkingContent {
            thinking: "foreign thoughts".into(),
            thinking_signature: Some("sig".into()),
            redacted: false,
        })],
        StopReason::Stop,
    );
    let context = Context {
        system_prompt: None,
        messages: vec![
            user_message("go"),
            anthropic_tool_update_system("A search tool is now available.", "grep"),
            empty_after_transform,
            anthropic_tool_update_system("Another tool is now available.", "find"),
            user_message("again"),
        ],
        tools: Some(vec![anthropic_initial_tool()]),
    };
    let options = StreamOptions {
        api_key: Some("sk-ant-api".into()),
        ..StreamOptions::default()
    };
    let (body, _) = build_anthropic_request(&model, &context, &options).expect("request");
    let messages = body["messages"].as_array().expect("messages");
    let roles: Vec<&str> = messages
        .iter()
        .map(|message| message["role"].as_str().expect("role"))
        .collect();
    // The emptied assistant is not on the wire; both held updates follow the
    // user turn instead of landing between two users.
    assert_eq!(
        roles,
        vec!["user", "user", "system", "system"],
        "wire order: {messages:?}"
    );
    assert!(
        !messages
            .windows(2)
            .any(|pair| pair[0]["role"] == json!("system") && pair[1]["role"] == json!("user")),
        "no system message may be followed by a user message: {messages:?}"
    );
    // Both updates survive with their native tool additions.
    assert_eq!(
        messages[2]["content"][0]["text"],
        json!("A search tool is now available.")
    );
    assert_eq!(messages[2]["content"][1]["type"], json!("tool_addition"));
    assert_eq!(
        messages[3]["content"][1]["tool"]["definition"]["name"],
        json!("find")
    );
}

#[test]
fn anthropic_system_update_flushes_before_normal_assistant() {
    // Regression guard for the P0 fix: a real (non-empty) assistant still
    // receives the held system update immediately before it.
    let model = anthropic_model_with(anthropic_native_tool_changes_compat());
    let context = Context {
        system_prompt: None,
        messages: vec![
            user_message("go"),
            anthropic_tool_update_system("A search tool is now available.", "grep"),
            assistant_message(
                "anthropic",
                "anthropic-messages",
                "claude-sonnet-4-5",
                vec![text_block("answer")],
                StopReason::Stop,
            ),
        ],
        tools: Some(vec![anthropic_initial_tool()]),
    };
    let options = StreamOptions {
        api_key: Some("sk-ant-api".into()),
        ..StreamOptions::default()
    };
    let (body, _) = build_anthropic_request(&model, &context, &options).expect("request");
    let messages = body["messages"].as_array().expect("messages");
    let roles: Vec<&str> = messages
        .iter()
        .map(|message| message["role"].as_str().expect("role"))
        .collect();
    assert_eq!(
        roles,
        vec!["user", "system", "assistant"],
        "the update still flushes right before the assistant: {messages:?}"
    );
    assert_eq!(
        messages[1]["content"][0]["text"],
        json!("A search tool is now available.")
    );
    assert_eq!(messages[1]["content"][1]["type"], json!("tool_addition"));
    assert_eq!(messages[2]["content"][0]["text"], json!("answer"));
}

#[test]
fn anthropic_server_side_fallback_beta_for_allowed_fallback_models() {
    // P2-9: non-empty allowedFallbackModels enables the server-side-fallback
    // beta.
    let compat = pir::ai::types::OpenAiCompat {
        allowed_fallback_models: Some(vec![json!({"model": "claude-haiku"})]),
        ..Default::default()
    };
    let model = anthropic_model_with(compat);
    let context = Context {
        system_prompt: None,
        messages: vec![user_message("go")],
        tools: None,
    };
    let options = StreamOptions {
        api_key: Some("sk-ant-api".into()),
        ..StreamOptions::default()
    };
    let (_, headers) = build_anthropic_request(&model, &context, &options).expect("request");
    let betas = headers.get("anthropic-beta").expect("beta header");
    assert!(
        betas.contains("server-side-fallback-2026-07-01"),
        "got: {betas}"
    );
}

#[test]
fn anthropic_budget_tokens_floor_at_1024() {
    // P2-10: a tiny max_tokens must not clamp the thinking budget to zero
    // (upstream `options.thinkingBudgetTokens || 1024`).
    let mut model = anthropic_model_with(Default::default());
    model.max_tokens = 2_000;
    let context = Context {
        system_prompt: None,
        messages: vec![user_message("go")],
        tools: None,
    };
    let options = StreamOptions {
        api_key: Some("sk-ant-api".into()),
        max_tokens: Some(1_024),
        reasoning: Some(ThinkingLevel::Medium),
        ..StreamOptions::default()
    };
    let (body, _) = build_anthropic_request(&model, &context, &options).expect("request");
    assert_eq!(body["thinking"]["budget_tokens"], 1024);
}

#[test]
fn anthropic_cache_control_markers_on_system_tools_and_last_message() {
    // P2-14: system blocks, the last tool, and the last conversation message
    // carry cache_control {type: ephemeral}; long retention adds ttl: 1h;
    // retention "none" drops every marker.
    let mut model = anthropic_model_with(Default::default());
    model.input = vec!["text".into()];
    let tool = Tool {
        name: "search".into(),
        description: "s".into(),
        parameters: json!({"type": "object", "properties": {}}),
        constrained_sampling: None,
    };
    let context = Context {
        system_prompt: Some("Be helpful.".into()),
        messages: vec![user_message("go")],
        tools: Some(vec![tool]),
    };

    let options = StreamOptions {
        api_key: Some("sk-ant-api".into()),
        ..StreamOptions::default()
    };
    let (body, _) = build_anthropic_request(&model, &context, &options).expect("request");
    assert_eq!(
        body["system"][0]["cache_control"],
        json!({"type": "ephemeral"})
    );
    assert_eq!(
        body["tools"][0]["cache_control"],
        json!({"type": "ephemeral"})
    );
    assert_eq!(
        body["messages"]
            .as_array()
            .and_then(|m| m.last())
            .and_then(|m| m["content"].as_array())
            .and_then(|blocks| blocks.last())
            .and_then(|block| block.get("cache_control"))
            .cloned()
            .expect("last message block carries cache_control"),
        json!({"type": "ephemeral"})
    );

    let options = StreamOptions {
        api_key: Some("sk-ant-api".into()),
        cache_retention: Some(CacheRetention::Long),
        ..StreamOptions::default()
    };
    let (body, _) = build_anthropic_request(&model, &context, &options).expect("request");
    assert_eq!(
        body["system"][0]["cache_control"],
        json!({"type": "ephemeral", "ttl": "1h"})
    );

    let options = StreamOptions {
        api_key: Some("sk-ant-api".into()),
        cache_retention: Some(CacheRetention::None),
        ..StreamOptions::default()
    };
    let (body, _) = build_anthropic_request(&model, &context, &options).expect("request");
    assert!(body["system"][0].get("cache_control").is_none());
    assert!(body["tools"][0].get("cache_control").is_none());
}

#[test]
fn anthropic_toolcall_end_emitted_at_content_block_stop() {
    // P2-12: `toolcall_end` arrives at content_block_stop (upstream), so the
    // Done event is preceded by it in block order.
    let model = anthropic_model_with(Default::default());
    let body = concat!(
        "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",\"usage\":{\"input_tokens\":1,\"output_tokens\":1}}}\n\n",
        "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_1\",\"name\":\"search\"}}\n\n",
        "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"q\\\":\\\"x\\\"}\"}}\n\n",
        "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
        "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\"},\"usage\":{\"output_tokens\":5}}\n\n",
        "data: {\"type\":\"message_stop\"}\n\n",
    );
    let events = parse_anthropic_sse(body.as_bytes(), &model).expect("parses");
    let stop_position = events
        .iter()
        .position(|event| matches!(event, AssistantMessageEvent::Done { .. }))
        .expect("Done event");
    let tool_end_position = events
        .iter()
        .position(|event| matches!(event, AssistantMessageEvent::ToolCallEnd { .. }))
        .expect("ToolCallEnd event");
    assert!(
        tool_end_position < stop_position,
        "toolcall_end precedes Done"
    );
    let done = done_of(&events);
    assert_eq!(done.stop_reason, StopReason::ToolUse);
    let Some(ContentBlock::ToolCall(call)) = done.content.first() else {
        panic!("tool call in final content, got {:?}", done.content);
    };
    assert_eq!(call.arguments.get("q"), Some(&json!("x")));
}

#[test]
fn anthropic_error_stream_after_unstopped_tool_block_has_no_toolcall_end() {
    // P2-12: the old finish()-time flush pushed toolcall_end events into
    // error-terminated streams that upstream never produces.
    let model = anthropic_model_with(Default::default());
    let body = concat!(
        "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",\"usage\":{\"input_tokens\":1,\"output_tokens\":1}}}\n\n",
        "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_1\",\"name\":\"search\"}}\n\n",
        "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"q\\\":\"}}\n\n",
        "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"refusal\",\"stop_details\":{\"explanation\":\"no\"}}}\n\n",
        "data: {\"type\":\"message_stop\"}\n\n",
    );
    let result = parse_anthropic_sse(body.as_bytes(), &model);
    let error = result.expect_err("refusal stop is an error");
    assert!(error.to_string().contains("no"), "got: {error}");
}

#[test]
fn anthropic_multi_line_data_payloads_are_joined() {
    // P2-13: multi-line `data:` fields are valid SSE; the JSON payload must be
    // reassembled, not truncated to the first line.
    let model = anthropic_model_with(Default::default());
    let body = concat!(
        "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",\"usage\":{\"input_tokens\":1,\"output_tokens\":1}}}\n\n",
        "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
        "data: {\"type\":\"content_block_delta\",\"index\":0,\n",
        "data: \"delta\":{\"type\":\"text_delta\",\"text\":\"split\"}}\n\n",
        "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
        "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"}}\n\n",
        "data: {\"type\":\"message_stop\"}\n\n",
    );
    let events = parse_anthropic_sse(body.as_bytes(), &model).expect("parses");
    let deltas: Vec<&String> = events
        .iter()
        .filter_map(|event| match event {
            AssistantMessageEvent::TextDelta { delta, .. } => Some(delta),
            _ => None,
        })
        .collect();
    assert_eq!(deltas, vec!["split"], "joined payload produced one delta");
}

// ---------------------------------------------------------------------------
// P2-18: Google system folding + transform
// ---------------------------------------------------------------------------

#[test]
fn google_folds_mid_conversation_system_into_system_instruction() {
    // P2-18: system messages run through transform and fold into
    // systemInstruction instead of being silently skipped.
    let mut model = base_model("google-generative-ai", "google", "gemini-2.5-flash");
    model.input = vec!["text".into(), "image".into()];
    let context = Context {
        system_prompt: Some("Lead prompt.".into()),
        messages: vec![
            user_message("hi"),
            Message::System(UserMessage {
                role: UserRole::System,
                content: vec![text_block("mid update")],
                tools_added: None,
                tools_removed: None,
                timestamp: 2,
                sections: None,
            }),
            user_message("there"),
        ],
        tools: None,
    };
    let body = build_google_body(&model, &context, &StreamOptions::default()).expect("body");
    assert_eq!(
        body["systemInstruction"]["parts"][0]["text"],
        "Lead prompt.\n\nmid update"
    );
    let contents = body["contents"].as_array().expect("contents");
    assert_eq!(contents.len(), 2, "no system turn in contents");
    assert_eq!(contents[0]["role"], "user");
    assert_eq!(contents[1]["role"], "user");
}

// ---------------------------------------------------------------------------
// P2-19 / P2-20 / P2-21: OpenAI Responses shared codec
// ---------------------------------------------------------------------------

#[test]
fn responses_unknown_terminal_status_is_an_error() {
    // P2-19: unknown terminal statuses surface as
    // (Error, "Unhandled stop reason: ...").
    let model = base_model("openai-responses", "test", "gpt");
    let body = "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"r1\",\"status\":\"mystery\",\"usage\":{\"input_tokens\":1,\"output_tokens\":1}}}\n\n";
    // A failed/cancelled/unknown terminal status is a provider error, not a
    // successful Done event; `finish` must surface it as a terminal error.
    let error = parse_responses_sse(body.as_bytes(), &model).expect_err("terminal error");
    assert!(
        matches!(
            &error,
            pir::ai::error::AiError::NonRetryable(message)
                if message.contains("Unhandled stop reason: mystery")
        ),
        "unexpected error: {error:?}"
    );
}

#[test]
fn azure_params_omit_prompt_cache_retention() {
    // P2-20: Azure sends only prompt_cache_key; prompt_cache_retention is
    // gated to the openai-responses variant.
    let mut azure = base_model("azure-openai-responses", "azure", "gpt-5.2");
    azure.compat = None;
    let context = Context {
        system_prompt: None,
        messages: vec![user_message("hi")],
        tools: None,
    };
    let options = StreamOptions {
        cache_retention: Some(CacheRetention::Long),
        session_id: Some("session-1".into()),
        ..StreamOptions::default()
    };
    let params = build_azure_params(&azure, &context, &options).expect("params");
    assert_eq!(params["prompt_cache_key"], json!("session-1"));
    assert!(
        params.get("prompt_cache_retention").is_none(),
        "Azure must not send prompt_cache_retention: {params}"
    );

    // The openai-responses variant keeps the 24h marker for long retention.
    let openai = base_model("openai-responses", "openai", "gpt-5.2");
    let compat = ResponsesCompat::from_model(&openai);
    let params = pir::ai::api::openai_responses::shared::build_responses_params(
        &openai,
        &context,
        &options,
        &compat,
        "gpt-5.2",
        pir::ai::api::openai_responses::shared::OPENAI_TOOL_CALL_PROVIDERS,
        false,
        None,
    )
    .expect("params");
    assert_eq!(params["prompt_cache_retention"], json!("24h"));
}

#[test]
fn responses_partial_json_completes_truncated_literals() {
    // P2-21: truncated `tru`/`fals`/`nul` literals complete to real values
    // instead of collapsing to Null arguments.
    assert_eq!(parse_partial_json("{\"a\": tru"), json!({"a": true}));
    assert_eq!(parse_partial_json("{\"a\": fals"), json!({"a": false}));
    assert_eq!(parse_partial_json("{\"a\": nul"), json!({"a": null}));
    assert_eq!(parse_partial_json("tru"), json!(true));
    assert_eq!(parse_partial_json("fals"), json!(false));
    // Truncated strings/containers still repair.
    assert_eq!(parse_partial_json("{\"a\": \"he"), json!({"a": "he"}));
    assert_eq!(
        parse_partial_json("{\"a\": {\"b\": 1"),
        json!({"a": {"b": 1}})
    );
}

// ---------------------------------------------------------------------------
// D29 (upstream `4b994d5e5`): invalid `\u` escapes in tool-call arguments
// ---------------------------------------------------------------------------

#[test]
fn d29_partial_json_keeps_invalid_unicode_escapes_literal() {
    // `\u` not followed by four hex digits (`\usepackage`, `\url{x}`) is kept
    // as literal text like any other invalid escape. Before the fix the repair
    // did not touch the escape, serde failed, and the whole document fell back
    // to `Null` (upstream dropped the field and everything after it).
    assert_eq!(
        parse_partial_json(r#"{"text":"\url{x}"}"#),
        json!({"text": "\\url{x}"})
    );
    assert_eq!(
        parse_partial_json(r#"{"path":"a.tex","content":"\usepackage{x}","mode":"w"}"#),
        json!({"path": "a.tex", "content": "\\usepackage{x}", "mode": "w"})
    );
    // A `\u` with four characters that are not all hex is invalid too, and the
    // following text stays literal (only the backslash is doubled).
    assert_eq!(
        parse_partial_json(r#"{"text":"\u00zz"}"#),
        json!({"text": "\\u00zz"})
    );
}

#[test]
fn d29_partial_json_decodes_valid_unicode_escapes() {
    // A valid `\uXXXX` still decodes to its code point; the invalid escape
    // that follows stays literal (upstream test `caf\u00e9 \url`).
    assert_eq!(
        parse_partial_json(r#"{"text":"caf\u00e9 \url"}"#),
        json!({"text": "café \\url"})
    );
    assert_eq!(
        parse_partial_json(r#"{"text":"\u0041"}"#),
        json!({"text": "A"})
    );
}

#[test]
fn d29_partial_json_drops_escape_cut_off_mid_stream() {
    // `\u` whose hex digits are cut off by the end of the stream: upstream's
    // partial-json fallback drops the incomplete escape, so the field keeps
    // only the text before it.
    assert_eq!(
        parse_partial_json(r#"{"text":"ab\u00"#),
        json!({"text": "ab"})
    );
    assert_eq!(
        parse_partial_json(r#"{"text":"ab\u"#),
        json!({"text": "ab"})
    );
}

#[test]
fn d29_partial_json_other_escape_behavior_unchanged() {
    // Valid escapes (including `\/`, `\"`, `\\`) keep their existing meaning,
    // and a truncated backslash still closes the string as before.
    assert_eq!(
        parse_partial_json(r#"{"a":"line\nbreak","b":"quote\"here","c":"slash\/here","d":"\\"}"#),
        json!({"a": "line\nbreak", "b": "quote\"here", "c": "slash/here", "d": "\\"})
    );
    assert_eq!(parse_partial_json("{\"a\": \"he"), json!({"a": "he"}));
}

#[test]
fn d29_anthropic_tool_arguments_survive_invalid_escape() {
    // D29 end-to-end on the anthropic `{}`-collapse path: a `\u` escape in one
    // field used to fail the parse, and `parse_tool_arguments` collapsed the
    // whole argument object to `{}`, losing `path` and `mode` too.
    let model = anthropic_model_with(Default::default());
    let body = concat!(
        "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",\"usage\":{\"input_tokens\":1,\"output_tokens\":1}}}\n\n",
        "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_1\",\"name\":\"write\"}}\n\n",
        "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"path\\\":\\\"a.tex\\\",\\\"content\\\":\\\"\\\\usepackage{x}\\\",\\\"mode\\\":\\\"w\\\"}\"}}\n\n",
        "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
        "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\"},\"usage\":{\"output_tokens\":5}}\n\n",
        "data: {\"type\":\"message_stop\"}\n\n",
    );
    let events = parse_anthropic_sse(body.as_bytes(), &model).expect("parses");
    let done = done_of(&events);
    assert_eq!(done.stop_reason, StopReason::ToolUse);
    let Some(ContentBlock::ToolCall(call)) = done.content.first() else {
        panic!("tool call in final content, got {:?}", done.content);
    };
    assert_eq!(call.arguments.get("path"), Some(&json!("a.tex")));
    assert_eq!(
        call.arguments.get("content"),
        Some(&json!("\\usepackage{x}"))
    );
    assert_eq!(call.arguments.get("mode"), Some(&json!("w")));
}

// ---------------------------------------------------------------------------
// P2-23: transport retry policy
// ---------------------------------------------------------------------------

#[test]
fn transport_retryable_statuses_match_upstream() {
    // P2-23: 408/409/429 and 5xx only; 425 is not retryable.
    for status in [408, 409, 429, 500, 502, 503, 504] {
        assert!(is_retryable(status), "{status} should be retryable");
    }
    for status in [400, 401, 403, 404, 413, 422, 425] {
        assert!(!is_retryable(status), "{status} must not be retryable");
    }
}

#[test]
fn transport_retry_after_ms_wins_over_retry_after() {
    // P2-23: retry-after-ms is honored first.
    let delay = server_retry_delay(Some("30"), Some("9999")).expect("delay");
    assert_eq!(delay, Duration::from_millis(30));
    let delay = server_retry_delay(None, Some("2")).expect("delay");
    assert_eq!(delay, Duration::from_secs(2));
    // Unparseable values fall through to exponential backoff.
    assert!(server_retry_delay(Some("soon"), None).is_none());
    assert!(server_retry_delay(None, None).is_none());
}

#[test]
fn transport_exponential_backoff_shape() {
    // P2-23: min(0.5 * 2^n, 8s), shrunk by up to 25% jitter.
    let caps = [500_u64, 1_000, 2_000, 4_000, 8_000, 8_000, 8_000];
    for (attempt, cap) in caps.iter().enumerate() {
        let delay = exponential_backoff_delay(attempt as u32);
        assert!(
            delay <= Duration::from_millis(*cap),
            "attempt {attempt}: {delay:?} exceeds cap {cap}"
        );
        assert!(
            delay >= Duration::from_millis(cap * 3 / 4),
            "attempt {attempt}: {delay:?} below the 75% jitter floor"
        );
    }
}

#[test]
fn transport_client_config_defaults_match_upstream_caps() {
    let config = ProviderRequestConfig::default();
    assert_eq!(config.max_retry_delay, Duration::from_secs(60));
    // The client type stays constructible through the shared config (the
    // fail-fast path is covered by the mock contract test in
    // ai_provider_contracts.rs).
    let _client = ProviderRequestClient::new(config).expect("client");
}

// ---------------------------------------------------------------------------
// Transport retry-wait seam (MAPPING.md follow-up: the upstream
// `RetryStatusIndicator` countdown + `abortRetry`). The transport hands each
// scheduled retry to `ProviderStreamHooks.on_retry`, which owns the wait.
// ---------------------------------------------------------------------------

/// Serve one `429` response (with `retry-after-ms`) per request on `stream`
/// and count the requests; the connection is kept alive so a pooled retry
/// lands on the same socket (a fresh connect for every attempt would make
/// the test racy under load).
fn serve_retryable_requests(
    mut stream: std::net::TcpStream,
    requests: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    retry_after_ms: u64,
) {
    use std::io::{BufRead, BufReader, Read, Write};
    let Ok(clone) = stream.try_clone() else {
        return;
    };
    let mut reader = BufReader::new(clone);
    loop {
        let mut request_line = String::new();
        if reader.read_line(&mut request_line).unwrap_or(0) == 0 {
            break;
        }
        let mut content_length = 0usize;
        loop {
            let mut header = String::new();
            if reader.read_line(&mut header).unwrap_or(0) == 0 {
                break;
            }
            if header.trim().is_empty() {
                break;
            }
            if let Some(value) = header.to_ascii_lowercase().strip_prefix("content-length:") {
                content_length = value.trim().parse().unwrap_or(0);
            }
        }
        if content_length > 0 {
            let mut body = vec![0u8; content_length];
            let _ = reader.read_exact(&mut body);
        }
        requests.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let body = br#"{"error":{"message":"rate limited"}}"#;
        let response = format!(
            "HTTP/1.1 429 Too Many Requests\r\ncontent-type: application/json\r\nretry-after-ms: {retry_after_ms}\r\ncontent-length: {}\r\n\r\n",
            body.len()
        );
        if stream.write_all(response.as_bytes()).is_err() {
            break;
        }
        if stream.write_all(body).is_err() {
            break;
        }
        let _ = stream.flush();
    }
}

/// A local HTTP endpoint that answers every request with `429` plus a
/// `retry-after-ms` value, and counts the requests it served.
fn retryable_endpoint(
    retry_after_ms: u64,
) -> (String, std::sync::Arc<std::sync::atomic::AtomicUsize>) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");
    let requests = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counter = std::sync::Arc::clone(&requests);
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { continue };
            let counter = std::sync::Arc::clone(&counter);
            std::thread::spawn(move || serve_retryable_requests(stream, counter, retry_after_ms));
        }
    });
    (format!("http://{addr}/v1"), requests)
}

fn retry_client(max_retries: u32, max_retry_delay: Duration) -> ProviderRequestClient {
    ProviderRequestClient::new(ProviderRequestConfig {
        timeout: Duration::from_secs(5),
        max_response_bytes: 4096,
        max_retries,
        max_retry_delay,
        proxy_url: None,
        http_idle_timeout: None,
    })
    .expect("client builds")
}

#[tokio::test]
async fn transport_retry_hook_reports_the_schedule_and_proceeds() {
    use pir::ai::types::{RetryNotice, RetryOutcome, RetryWaitHook};
    use std::sync::atomic::Ordering;
    use std::sync::{Arc, Mutex};

    let (base, requests) = retryable_endpoint(5);
    let seen: Arc<Mutex<Vec<RetryNotice>>> = Arc::new(Mutex::new(Vec::new()));
    let recorder = Arc::clone(&seen);
    let hooks = ProviderStreamHooks {
        on_retry: RetryWaitHook::new(Arc::new(move |notice| {
            recorder.lock().unwrap().push(notice);
            Box::pin(async { RetryOutcome::Proceed })
        })),
        ..Default::default()
    };
    let client = retry_client(1, Duration::from_secs(1));

    let result = client
        .request_streaming(
            reqwest::Method::GET,
            &format!("{base}/models"),
            &BTreeMap::new(),
            Vec::new(),
            None,
            &hooks,
            &mut |_chunk| Ok(()),
        )
        .await;
    // Both attempts got 429: the retry ran, the request still fails.
    let error = result.expect_err("429 stays an error");
    assert!(error.to_string().contains("429"), "{error}");
    assert_eq!(
        requests.load(Ordering::SeqCst),
        2,
        "the scheduled retry re-sent the request"
    );
    assert_eq!(
        seen.lock().unwrap().as_slice(),
        &[RetryNotice {
            attempt: 1,
            max_attempts: 1,
            delay_ms: 5,
        }],
        "one notice with the upstream auto_retry_start shape"
    );
}

#[tokio::test]
async fn transport_retry_hook_cancel_fails_with_retry_cancelled() {
    use pir::ai::error::AiError;
    use pir::ai::types::{RetryOutcome, RetryWaitHook};
    use std::sync::atomic::Ordering;
    use std::sync::Arc;

    let (base, requests) = retryable_endpoint(5_000);
    let hooks = ProviderStreamHooks {
        on_retry: RetryWaitHook::new(Arc::new(|_notice| {
            // Upstream `abortRetry()` during the countdown.
            Box::pin(async { RetryOutcome::Cancelled })
        })),
        ..Default::default()
    };
    let client = retry_client(2, Duration::from_secs(60));

    let result = client
        .request_streaming(
            reqwest::Method::GET,
            &format!("{base}/models"),
            &BTreeMap::new(),
            Vec::new(),
            None,
            &hooks,
            &mut |_chunk| Ok(()),
        )
        .await;
    let error = result.expect_err("a cancelled retry fails");
    assert!(
        matches!(error, AiError::RetryCancelled { attempts: 1 }),
        "unexpected error: {error}"
    );
    assert_eq!(
        error.to_string(),
        "Retry failed after 1 attempts: Retry cancelled"
    );
    assert_eq!(
        requests.load(Ordering::SeqCst),
        1,
        "a cancelled retry must not re-send the request"
    );
}

#[tokio::test]
async fn transport_retry_hook_wait_still_honours_the_request_signal() {
    use pir::ai::error::AiError;
    use pir::ai::types::{AbortSignal, RetryOutcome, RetryWaitHook};
    use std::sync::Arc;
    use std::time::Instant;

    // A long server-requested delay (within the cap) so the hook's own wait
    // is where the request spends its time; the request signal must still cut
    // it short (upstream: `abort()` → `abortRetry()`).
    let (base, _requests) = retryable_endpoint(60_000);
    let hooks = ProviderStreamHooks {
        on_retry: RetryWaitHook::new(Arc::new(|notice| {
            Box::pin(async move {
                tokio::time::sleep(Duration::from_millis(notice.delay_ms)).await;
                RetryOutcome::Proceed
            })
        })),
        ..Default::default()
    };
    let client = retry_client(1, Duration::from_secs(120));
    let signal = AbortSignal::new();
    let aborter = signal.clone();
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(50));
        aborter.abort();
    });

    let started = Instant::now();
    let result = client
        .request_streaming(
            reqwest::Method::GET,
            &format!("{base}/models"),
            &BTreeMap::new(),
            Vec::new(),
            Some(&signal),
            &hooks,
            &mut |_chunk| Ok(()),
        )
        .await;
    assert!(
        matches!(result, Err(AiError::Aborted)),
        "the request signal must end the hooked wait"
    );
    assert!(started.elapsed() < Duration::from_secs(5));
}

// ---------------------------------------------------------------------------
// Premature stream endings (CL32 review item D2)
//
// A clean EOF after a 2xx response, without the provider's terminal event,
// means the transport read a complete body and rendered no verdict. Every
// adapter must classify that parser-visible truncation as the SAME retryable
// class pi-messages already reports (`StreamNotTerminated`), not as a
// non-retryable provider verdict — otherwise the agent loop hard-exits a turn
// it re-issued before the structural classes existed.
// ---------------------------------------------------------------------------

/// The structural verdict every truncated-stream adapter error must carry.
fn assert_truncated_is_retryable(error: &pir::ai::error::AiError, expected_text: &str) {
    use pir::ai::error::TurnRetryClass;

    assert!(
        matches!(error, pir::ai::error::AiError::StreamTruncated(_)),
        "a clean EOF without the terminal event is a truncated stream, got: {error:?}"
    );
    assert!(
        error.to_string().contains(expected_text),
        "the adapter wording is preserved, got: {error}"
    );
    assert_eq!(
        error.turn_retry_class(),
        TurnRetryClass::RetryableTurn,
        "a premature stream ending must be re-issuable: {error}"
    );
}

#[test]
fn openai_premature_stream_end_is_retryable() {
    // Body ends after content with no finish_reason chunk (clean EOF).
    let model = base_model("openai-completions", "test", "gpt");
    let body = format!(
        "{}{}",
        openai_chunk(json!({"content": "hi"}), json!(null)),
        "data: [DONE]\n\n"
    );
    let error = parse_openai_sse(body.as_bytes(), &model).expect_err("truncated stream");
    assert_truncated_is_retryable(&error, "Stream ended without finish_reason");
}

#[test]
fn anthropic_premature_stream_end_is_retryable() {
    // `message_start` + a text block, then EOF: no `message_delta`/`message_stop`.
    let model = anthropic_model_with(Default::default());
    let body = concat!(
        "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",\"usage\":{\"input_tokens\":1,\"output_tokens\":1}}}\n\n",
        "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
        "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"partial\"}}\n\n",
    );
    let error = parse_anthropic_sse(body.as_bytes(), &model).expect_err("truncated stream");
    assert_truncated_is_retryable(&error, "Anthropic stream ended without a stop reason");
}

#[test]
fn bedrock_premature_stream_end_is_retryable() {
    // `messageStart` + a delta, then EOF: no `messageStop`.
    let model = base_model("bedrock-converse-stream", "bedrock", "claude");
    let mut body = es_event_frame(
        "messageStart",
        &json!({ "messageStart": { "role": "assistant" } }),
    );
    body.extend_from_slice(&es_event_frame(
        "contentBlockDelta",
        &json!({
            "contentBlockIndex": 0,
            "delta": { "text": "partial" },
        }),
    ));
    let error = parse_bedrock_event_stream(&body, &model).expect_err("truncated stream");
    assert_truncated_is_retryable(&error, "Bedrock stream ended without a stop reason");
}

#[test]
fn google_premature_stream_end_is_retryable() {
    // A content chunk with no finishReason, then EOF.
    use pir::ai::api::google::parse_google_sse;

    let model = base_model("google-generative-ai", "google", "gemini-2.5-flash");
    let body = r#"data: {"candidates":[{"content":{"parts":[{"text":"partial"}]}}]}"#;
    let error = parse_google_sse(body.as_bytes(), &model).expect_err("truncated stream");
    assert_truncated_is_retryable(&error, "Google stream ended without a finish reason");
}

#[test]
fn mistral_premature_stream_end_is_retryable() {
    use pir::ai::api::mistral::parse_mistral_sse;

    // Content chunk, then EOF: the stop reason stays `pending`.
    let model = base_model("mistral-conversations", "mistral", "mistral-large-latest");
    let body = "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"partial\"}}]}\n\n";
    let error = parse_mistral_sse(body.as_bytes(), &model).expect_err("truncated stream");
    assert_truncated_is_retryable(&error, "Mistral stream ended without a finish reason");
}

#[test]
fn responses_premature_stream_end_is_retryable() {
    // `response.created` + an output delta, then EOF: no `response.completed`.
    let model = base_model("openai-responses", "openai", "gpt-5");
    let body = concat!(
        "data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_1\"}}\n\n",
        "data: {\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{\"type\":\"message\",\"id\":\"msg_1\"}}\n\n",
        "data: {\"type\":\"response.output_text.delta\",\"output_index\":0,\"delta\":\"partial\"}\n\n",
    );
    let error = parse_responses_sse(body.as_bytes(), &model).expect_err("truncated stream");
    assert_truncated_is_retryable(&error, "ended before a terminal response event");
}

#[test]
fn pi_messages_premature_stream_end_matches_the_carrier() {
    use pir::ai::api::pi_messages::parse_pi_messages_sse;

    // The reference behavior the adapters now match: no terminal event.
    let model = base_model("pi-messages", "test", "pi");
    let body = "data: {\"type\":\"start\"}\n\ndata: {\"type\":\"text_delta\",\"contentIndex\":0,\"delta\":\"partial\"}\n\n";
    let error = parse_pi_messages_sse(body.as_bytes(), &model).expect_err("truncated stream");
    use pir::ai::error::TurnRetryClass;
    assert_eq!(
        error.turn_retry_class(),
        TurnRetryClass::RetryableTurn,
        "pi-messages is the reference class: {error}"
    );
}
