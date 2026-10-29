//! W10-A: malformed final tool-call arguments, matching upstream pi v1.3.0
//! `parseToolCallArguments` (#10897/#10944).
//!
//! Upstream keeps the tool call (`arguments = {}`) and records
//! `argumentsError` ("<parse error>\n\nReceived arguments:\n<raw>"); the
//! stream completes normally so the agent loop runs the call, validation
//! turns the error into a model-visible tool result, and the model can
//! self-correct on the next turn.

use std::collections::BTreeMap;

use pir::ai::api::anthropic::parse_anthropic_sse;
use pir::ai::api::bedrock::parse_bedrock_event_stream;
use pir::ai::api::google::parse_google_sse;
use pir::ai::api::mistral::parse_mistral_sse;
use pir::ai::api::openai::{build_openai_body, parse_openai_sse};
use pir::ai::api::openai_responses::shared::parse_responses_sse;
use pir::ai::api::pi_messages::parse_pi_messages_sse;
use pir::ai::api::tool_args::parse_final_tool_arguments;
use pir::ai::types::*;
use serde_json::{json, Value};

// ---------------------------------------------------------------------------
// Fixtures
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

fn done_of(events: &[AssistantMessageEvent]) -> AssistantMessage {
    match events.last() {
        Some(AssistantMessageEvent::Done { message, .. }) => message.clone(),
        other => panic!("expected terminal Done event, got {other:?}"),
    }
}

fn done_reason(events: &[AssistantMessageEvent]) -> TerminalReason {
    match events.last() {
        Some(AssistantMessageEvent::Done { reason, .. }) => *reason,
        other => panic!("expected terminal Done event, got {other:?}"),
    }
}

fn tool_call_of(message: &AssistantMessage) -> ToolCall {
    message
        .content
        .iter()
        .find_map(|block| match block {
            ContentBlock::ToolCall(call) => Some(call.clone()),
            _ => None,
        })
        .unwrap_or_else(|| panic!("expected a tool call, got {:?}", message.content))
}

fn tool_call_end_of(events: &[AssistantMessageEvent]) -> ToolCall {
    events
        .iter()
        .find_map(|event| match event {
            AssistantMessageEvent::ToolCallEnd { tool_call, .. } => Some(tool_call.clone()),
            _ => None,
        })
        .expect("ToolCallEnd event")
}

fn openai_chunk(delta: Value, finish_reason: Value) -> String {
    format!(
        "data: {}\n\n",
        json!({"id": "c1", "choices": [{"index": 0, "delta": delta, "finish_reason": finish_reason}]})
    )
}

fn openai_tool_body(arguments: &str) -> String {
    format!(
        "{}{}",
        openai_chunk(
            json!({"tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"search","arguments":arguments}}]}),
            json!("tool_calls")
        ),
        "data: [DONE]\n\n"
    )
}

// ---------------------------------------------------------------------------
// Shared parser: upstream `parseToolCallArguments` semantics
// ---------------------------------------------------------------------------

#[test]
fn shared_parser_keeps_valid_object_without_error() {
    let parsed = parse_final_tool_arguments(r#"{"query":"rust","limit":3}"#);
    assert_eq!(
        parsed.arguments.get("query").and_then(Value::as_str),
        Some("rust")
    );
    assert_eq!(
        parsed.arguments.get("limit").and_then(Value::as_i64),
        Some(3)
    );
    assert!(parsed.error.is_none());
}

#[test]
fn shared_parser_repairs_invalid_string_escapes_without_error() {
    // Upstream `parseJsonWithRepair`: only malformed string escapes are
    // repaired; the call succeeds with no argumentsError.
    let parsed = parse_final_tool_arguments(r#"{"note":"\url{x}"}"#);
    assert_eq!(
        parsed.arguments.get("note").and_then(Value::as_str),
        Some("\\url{x}")
    );
    assert!(parsed.error.is_none());
}

#[test]
fn shared_parser_empty_text_is_empty_object_without_error() {
    let parsed = parse_final_tool_arguments("   ");
    assert!(parsed.arguments.is_empty());
    assert!(parsed.error.is_none());
}

#[test]
fn shared_parser_truncated_json_keeps_no_prefix_and_records_received_text() {
    let raw = r#"{"kind":"seek","clipId":"clip-fl"#;
    let parsed = parse_final_tool_arguments(raw);
    assert!(parsed.arguments.is_empty(), "no parsed prefix is kept");
    let error = parsed.error.expect("argumentsError");
    assert!(error.contains("Received arguments:"));
    assert!(error.ends_with(raw), "got: {error}");
}

#[test]
fn shared_parser_non_object_is_an_error_like_upstream() {
    let parsed = parse_final_tool_arguments("[1,2]");
    assert!(parsed.arguments.is_empty());
    assert_eq!(
        parsed.error.as_deref(),
        Some("Tool arguments must be a JSON object\n\nReceived arguments:\n[1,2]")
    );

    // JSON `null` is not a valid argument object upstream either.
    let parsed = parse_final_tool_arguments("null");
    assert!(parsed.arguments.is_empty());
    assert!(parsed
        .error
        .as_deref()
        .is_some_and(|error| error.starts_with("Tool arguments must be a JSON object")));
}

// ---------------------------------------------------------------------------
// OpenAI chat completions (and every openai-compatible provider)
// ---------------------------------------------------------------------------

#[test]
fn openai_valid_final_args_keep_call_without_error() {
    let model = base_model("openai-completions", "test", "gpt");
    let body = openai_tool_body(r#"{"query":"rust"}"#);
    let events = parse_openai_sse(body.as_bytes(), &model).expect("stream completes");
    assert_eq!(done_reason(&events), TerminalReason::ToolUse);
    let done = done_of(&events);
    assert_eq!(done.stop_reason, StopReason::ToolUse);
    let call = tool_call_of(&done);
    assert_eq!(
        call.arguments.get("query").and_then(Value::as_str),
        Some("rust")
    );
    assert!(call.arguments_error.is_none());
    assert!(tool_call_end_of(&events).arguments_error.is_none());
}

#[test]
fn openai_truncated_final_args_keep_call_with_error_and_complete() {
    // #10897: a cut-off document must not keep the parsed prefix and must
    // not terminate the stream as an error.
    let model = base_model("openai-completions", "test", "gpt");
    let raw = r#"{"query": "ru"#;
    let body = openai_tool_body(raw);
    let events = parse_openai_sse(body.as_bytes(), &model).expect("stream completes");

    assert_eq!(done_reason(&events), TerminalReason::ToolUse);
    let done = done_of(&events);
    let call = tool_call_of(&done);
    assert!(call.arguments.is_empty(), "arguments must be {{}}");
    let error = call.arguments_error.clone().expect("argumentsError is set");
    assert!(error.contains("Received arguments:"));
    assert!(error.ends_with(raw), "got: {error}");

    // The finalized event and the terminal message agree.
    assert_eq!(tool_call_end_of(&events), call);
}

#[test]
fn openai_garbage_final_args_keep_call_with_error_and_complete() {
    let model = base_model("openai-completions", "test", "gpt");
    let raw = r#"{"query": "x"} garbage"#;
    let body = openai_tool_body(raw);
    let events = parse_openai_sse(body.as_bytes(), &model).expect("stream completes");
    let call = tool_call_of(&done_of(&events));
    assert!(call.arguments.is_empty());
    let error = call.arguments_error.clone().expect("argumentsError is set");
    assert!(error.ends_with(raw), "got: {error}");
}

// ---------------------------------------------------------------------------
// OpenAI Responses / Codex (shared codec)
// ---------------------------------------------------------------------------

#[test]
fn responses_malformed_final_args_keep_call_with_error_and_complete() {
    let model = base_model("openai-responses", "openai", "gpt-5.5");
    let raw = r#"{"q":"ru"#;
    let sse = format!(
        concat!(
            "data: {{\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{{\"type\":\"function_call\",\"call_id\":\"call_1\",\"id\":\"fc_1\",\"name\":\"search\",\"arguments\":\"\"}}}}\n\n",
            "data: {{\"type\":\"response.function_call_arguments.delta\",\"output_index\":0,\"delta\":{delta}}}\n\n",
            "data: {{\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{{\"type\":\"function_call\",\"call_id\":\"call_1\",\"id\":\"fc_1\",\"name\":\"search\",\"arguments\":{arguments}}}}}\n\n",
            "data: {{\"type\":\"response.completed\",\"response\":{{\"status\":\"completed\"}}}}\n\n"
        ),
        delta = json!(raw),
        arguments = json!(raw),
    );
    let events = parse_responses_sse(sse.as_bytes(), &model).expect("stream completes");
    assert_eq!(done_reason(&events), TerminalReason::ToolUse);
    let call = tool_call_of(&done_of(&events));
    assert_eq!(call.id, "call_1|fc_1");
    assert!(call.arguments.is_empty());
    let error = call.arguments_error.clone().expect("argumentsError is set");
    assert!(error.ends_with(raw), "got: {error}");
}

#[test]
fn responses_valid_final_args_have_no_error() {
    let model = base_model("openai-responses", "openai", "gpt-5.5");
    let sse = concat!(
        "data: {\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{\"type\":\"function_call\",\"call_id\":\"call_1\",\"id\":\"fc_1\",\"name\":\"search\",\"arguments\":\"\"}}\n\n",
        "data: {\"type\":\"response.function_call_arguments.delta\",\"output_index\":0,\"delta\":\"{\\\"q\\\":\\\"ru\"}\n\n",
        "data: {\"type\":\"response.function_call_arguments.delta\",\"output_index\":0,\"delta\":\"st\\\"}\"}\n\n",
        "data: {\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{\"type\":\"function_call\",\"call_id\":\"call_1\",\"id\":\"fc_1\",\"name\":\"search\",\"arguments\":\"{\\\"q\\\":\\\"rust\\\"}\"}}\n\n",
        "data: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\"}}\n\n",
    );
    let events = parse_responses_sse(sse.as_bytes(), &model).expect("stream completes");
    let call = tool_call_of(&done_of(&events));
    assert_eq!(
        call.arguments.get("q").and_then(Value::as_str),
        Some("rust")
    );
    assert!(call.arguments_error.is_none());
}

// ---------------------------------------------------------------------------
// Anthropic Messages
// ---------------------------------------------------------------------------

#[test]
fn anthropic_malformed_final_args_keep_call_with_error_and_complete() {
    let model = base_model("anthropic-messages", "anthropic", "claude");
    let raw = r#"{"q": "ru"#;
    let body = format!(
        concat!(
            "data: {{\"type\":\"message_start\",\"message\":{{\"id\":\"msg_1\",\"usage\":{{\"input_tokens\":1,\"output_tokens\":1}}}}}}\n\n",
            "data: {{\"type\":\"content_block_start\",\"index\":0,\"content_block\":{{\"type\":\"tool_use\",\"id\":\"toolu_1\",\"name\":\"search\"}}}}\n\n",
            "data: {{\"type\":\"content_block_delta\",\"index\":0,\"delta\":{{\"type\":\"input_json_delta\",\"partial_json\":{delta}}}}}\n\n",
            "data: {{\"type\":\"content_block_stop\",\"index\":0}}\n\n",
            "data: {{\"type\":\"message_delta\",\"delta\":{{\"stop_reason\":\"tool_use\"}},\"usage\":{{\"output_tokens\":5}}}}\n\n",
            "data: {{\"type\":\"message_stop\"}}\n\n"
        ),
        delta = json!(raw),
    );
    let events = parse_anthropic_sse(body.as_bytes(), &model).expect("stream completes");
    assert_eq!(done_reason(&events), TerminalReason::ToolUse);
    let call = tool_call_of(&done_of(&events));
    assert!(call.arguments.is_empty());
    let error = call.arguments_error.clone().expect("argumentsError is set");
    assert!(error.ends_with(raw), "got: {error}");
    assert_eq!(tool_call_end_of(&events), call);
}

// ---------------------------------------------------------------------------
// Mistral Conversations
// ---------------------------------------------------------------------------

#[test]
fn mistral_malformed_final_args_keep_call_with_error_and_complete() {
    let model = base_model("mistral-conversations", "mistral", "mistral-large-latest");
    let raw = r#"{"q": "ru"#;
    let body = format!(
        concat!(
            "data: {{\"choices\":[{{\"index\":0,\"delta\":{{\"tool_calls\":[{{\"index\":0,\"id\":\"abc123def\",\"function\":{{\"name\":\"search\",\"arguments\":\"\"}}}}]}}}}]}}\n\n",
            "data: {{\"choices\":[{{\"index\":0,\"delta\":{{\"tool_calls\":[{{\"index\":0,\"function\":{{\"arguments\":{delta}}}}}]}}}}]}}\n\n",
            "data: {{\"choices\":[{{\"index\":0,\"delta\":{{}},\"finish_reason\":\"tool_calls\"}}]}}\n\n",
            "data: [DONE]\n\n"
        ),
        delta = json!(raw),
    );
    let events = parse_mistral_sse(body.as_bytes(), &model).expect("stream completes");
    assert_eq!(done_reason(&events), TerminalReason::ToolUse);
    let call = tool_call_of(&done_of(&events));
    assert!(call.arguments.is_empty());
    let error = call.arguments_error.clone().expect("argumentsError is set");
    assert!(error.ends_with(raw), "got: {error}");
}

// ---------------------------------------------------------------------------
// Bedrock Converse Stream
// ---------------------------------------------------------------------------

fn es_header_string(name: &str, value: &str) -> Vec<u8> {
    let mut out = vec![name.len() as u8];
    out.extend_from_slice(name.as_bytes());
    out.push(7);
    out.extend_from_slice(&(value.len() as u16).to_be_bytes());
    out.extend_from_slice(value.as_bytes());
    out
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
    frame.extend_from_slice(&[0, 0, 0, 0]); // prelude CRC (unused)
    frame.extend_from_slice(&header_block);
    frame.extend_from_slice(payload);
    frame.extend_from_slice(&[0, 0, 0, 0]); // message CRC (unused)
    frame
}

fn es_event_frame(event_type: &str, payload: &Value) -> Vec<u8> {
    es_frame(
        vec![
            es_header_string(":message-type", "event"),
            es_header_string(":event-type", event_type),
        ],
        payload.to_string().as_bytes(),
    )
}

#[test]
fn bedrock_malformed_final_args_keep_call_with_error_and_complete() {
    let model = base_model("bedrock-converse-stream", "bedrock", "claude");
    let raw = r#"{"query": "ru"#;
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
            "delta": { "toolUse": { "input": raw } },
        }),
    ));
    body.extend_from_slice(&es_event_frame(
        "contentBlockStop",
        &json!({ "contentBlockIndex": 0 }),
    ));
    body.extend_from_slice(&es_event_frame(
        "messageStop",
        &json!({ "messageStop": { "stopReason": "tool_use" } }),
    ));

    let events = parse_bedrock_event_stream(&body, &model).expect("stream completes");
    assert_eq!(done_reason(&events), TerminalReason::ToolUse);
    let call = tool_call_of(&done_of(&events));
    assert!(call.arguments.is_empty());
    let error = call.arguments_error.clone().expect("argumentsError is set");
    assert!(error.ends_with(raw), "got: {error}");
    assert_eq!(tool_call_end_of(&events), call);
}

// ---------------------------------------------------------------------------
// Google Generative AI / Vertex (args arrive already decoded)
// ---------------------------------------------------------------------------

#[test]
fn google_valid_args_have_no_error() {
    let model = base_model("google-generative-ai", "google", "gemini");
    let body =
        "data: {\"candidates\":[{\"content\":{\"parts\":[{\"functionCall\":{\"name\":\"search\",\"args\":{\"q\":\"rust\"}}}]},\"finishReason\":\"STOP\"}]}\n\n";
    let events = parse_google_sse(body.as_bytes(), &model).expect("stream completes");
    let call = tool_call_of(&done_of(&events));
    assert_eq!(
        call.arguments.get("q").and_then(Value::as_str),
        Some("rust")
    );
    assert!(call.arguments_error.is_none());
}

#[test]
fn google_non_object_args_keep_call_with_error_and_complete() {
    let model = base_model("google-generative-ai", "google", "gemini");
    let body =
        "data: {\"candidates\":[{\"content\":{\"parts\":[{\"functionCall\":{\"name\":\"search\",\"args\":\"not-an-object\"}}]},\"finishReason\":\"STOP\"}]}\n\n";
    let events = parse_google_sse(body.as_bytes(), &model).expect("stream completes");
    let call = tool_call_of(&done_of(&events));
    assert!(call.arguments.is_empty());
    let error = call.arguments_error.clone().expect("argumentsError is set");
    assert!(error.contains("Tool arguments must be a JSON object"));
    assert!(error.ends_with("\"not-an-object\""), "got: {error}");
}

// ---------------------------------------------------------------------------
// pi-messages (wire events already parsed by the backend)
// ---------------------------------------------------------------------------

fn pi_usage() -> Value {
    json!({
        "input": 12,
        "output": 3,
        "cacheRead": 0,
        "cacheWrite": 0,
        "totalTokens": 15,
        "cost": {"input": 0.0, "output": 0.0, "cacheRead": 0.0, "cacheWrite": 0.0, "total": 0.0},
    })
}

#[test]
fn pi_messages_malformed_fragments_keep_call_with_error_and_complete() {
    let model = base_model("pi-messages", "pi", "model");
    let raw = r#"{"q": "ru"#;
    let body = [
        format!(
            "data: {}\n\n",
            json!({"type": "toolcall_start", "contentIndex": 0, "id": "call_1", "toolName": "search"})
        ),
        format!(
            "data: {}\n\n",
            json!({"type": "toolcall_delta", "contentIndex": 0, "delta": raw})
        ),
        format!(
            "data: {}\n\n",
            json!({"type": "toolcall_end", "contentIndex": 0, "toolCall": {"id": "call_1", "name": "search", "arguments": {}}})
        ),
        format!(
            "data: {}\n\n",
            json!({"type": "done", "reason": "toolUse", "usage": pi_usage()})
        ),
    ]
    .concat();
    let events = parse_pi_messages_sse(body.as_bytes(), &model).expect("stream completes");
    assert_eq!(done_reason(&events), TerminalReason::ToolUse);
    let call = tool_call_of(&done_of(&events));
    assert!(call.arguments.is_empty());
    let error = call.arguments_error.clone().expect("argumentsError is set");
    assert!(error.ends_with(raw), "got: {error}");
}

// ---------------------------------------------------------------------------
// Continuation: the finalized message replays into the next request
// ---------------------------------------------------------------------------

#[test]
fn malformed_call_replays_for_continuation() {
    let model = base_model("openai-completions", "test", "gpt");
    let raw = r#"{"query": "ru"#;
    let body = openai_tool_body(raw);
    let events = parse_openai_sse(body.as_bytes(), &model).expect("stream completes");
    let message = done_of(&events);
    assert_eq!(message.stop_reason, StopReason::ToolUse);

    // Persisted/session round-trip keeps the empty arguments and the error.
    let wire = serde_json::to_value(Message::Assistant(message.clone())).expect("serializes");
    let round: Message = serde_json::from_value(wire).expect("deserializes");
    let Message::Assistant(round) = round else {
        panic!("expected assistant message");
    };
    let call = tool_call_of(&round);
    assert!(call.arguments.is_empty());
    assert!(call.arguments_error.is_some());

    // The next request can be built and replays `arguments: "{}"`; the parse
    // error is local metadata (upstream does not send `argumentsError`).
    let context = Context {
        system_prompt: None,
        messages: vec![Message::Assistant(round)],
        ..Context::default()
    };
    let next = build_openai_body(
        &model,
        &context,
        &StreamOptions::default(),
        &OpenAiCompat::default(),
    )
    .expect("next request body");
    let tool_calls = next["messages"][0]["tool_calls"]
        .as_array()
        .expect("tool_calls replay");
    assert_eq!(tool_calls[0]["function"]["arguments"], json!("{}"));
}

// ---------------------------------------------------------------------------
// Model-visible tool result (agent scheduler path)
// ---------------------------------------------------------------------------

struct EchoTool;

impl pir::agent::tool_execution::AgentTool for EchoTool {
    fn name(&self) -> &str {
        "echo"
    }
    fn description(&self) -> &str {
        "echoes text"
    }
    fn parameters(&self) -> Option<Value> {
        Some(json!({
            "type": "object",
            "required": ["text"],
            "properties": {"text": {"type": "string"}},
        }))
    }
    fn run(
        &self,
        _args: &Value,
        _ctx: &pir::agent::tool_execution::ToolContext,
    ) -> pir::agent::tool_execution::BoxFuture<'static, pir::agent::tool_batch::ToolResult> {
        Box::pin(async { pir::agent::tool_batch::ToolResult::ok("ran") })
    }
}

#[test]
fn scheduler_reports_arguments_error_as_model_visible_tool_result() {
    // Upstream `validateToolArguments`: if `argumentsError` is set, the call is
    // rejected before schema validation with
    // `Invalid arguments for tool "<name>": <argumentsError>`, and the agent
    // loop turns that into an `isError` tool result for the model.
    //
    // NOTE (W10-A): this is the required post-fix contract of
    // `src/agent/tool_execution.rs::tool_calls_from_message` +
    // `src/agent/tool_scheduler/prepare.rs::prepare_calls`; this lane owns only
    // src/ai/**, so the foreman must land the scheduler change.
    use pir::agent::tool_execution::{tool_calls_from_message, ToolContext, ToolRegistry};
    use pir::agent::tool_scheduler::{ToolScheduler, ToolSchedulerConfig};

    let raw = r#"{"text": "hi"#;
    let parsed = parse_final_tool_arguments(raw);
    let error = parsed.error.expect("argumentsError");
    let message = AssistantMessage {
        content: vec![ContentBlock::ToolCall(ToolCall {
            id: "call_1".into(),
            name: "echo".into(),
            arguments: BTreeMap::new(),
            thought_signature: None,
            namespace: None,
            arguments_error: Some(error.clone()),
        })],
        ..AssistantMessage::default()
    };

    let calls = tool_calls_from_message(&message);
    assert_eq!(calls.len(), 1);
    assert!(calls[0].arguments.is_object());

    let runtime = tokio::runtime::Runtime::new().expect("runtime");
    runtime.block_on(async {
        let mut registry = ToolRegistry::new();
        registry.register(std::sync::Arc::new(EchoTool));
        let context = ToolContext::new(".");
        let scheduler = ToolScheduler::new(ToolSchedulerConfig::default());
        let outcome = scheduler
            .execute_batch(calls, &registry, &context, None)
            .await;
        assert_eq!(outcome.results.len(), 1);
        let result = &outcome.results[0];
        assert!(result.is_error(), "validation must fail the call");
        assert!(
            result
                .result
                .content
                .contains("Invalid arguments for tool \"echo\""),
            "model-visible result must name the invalid arguments: {}",
            result.result.content
        );
        assert!(
            result.result.content.contains("Received arguments:"),
            "model-visible result must carry the parse error text: {}",
            result.result.content
        );
    });
}
