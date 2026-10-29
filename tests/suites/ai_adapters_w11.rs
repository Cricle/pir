//! W12-G: adapter drift closure for `src/ai/api/**`.
//!
//! Each test pins one behavior that was aligned with upstream
//! `packages/ai/src/api/**` in this lane. No network: every case exercises a
//! pure request builder / parser or a local loopback server.
//!
//! Registration note (foreman): this suite must be listed in `tests/g_ai.rs`
//! as
//! ```text
//! #[path = "suites/ai_adapters_w11.rs"]
//! mod ai_adapters_w11;
//! ```
//! (the lane scope excludes `tests/g_*.rs`).

use std::collections::BTreeMap;

use pir::ai::api::anthropic::{build_anthropic_request, AnthropicSseParser, CLAUDE_CODE_VERSION};
use pir::ai::api::openai::{build_openai_body, parse_openai_sse, resolve_compat, OpenAiSseParser};
use pir::ai::api::openai_responses::codex::{
    build_codex_body, codex_ws, enhance_codex_error, extract_account_id, parse_error_response,
    resolve_codex_url,
};
use pir::ai::api::openai_responses::shared::{
    build_responses_params, convert_responses_messages, parse_responses_sse,
    ConvertResponsesMessagesOptions, ResponsesSseParser, OPENAI_TOOL_CALL_PROVIDERS,
};
use pir::ai::api::openai_responses::ResponsesCompat;
use pir::ai::api::tool_args::parse_final_tool_arguments;
use pir::ai::event_stream::AssistantMessageEventStream;
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
        base_url: "https://example.test/v1".into(),
        reasoning: false,
        thinking_level_map: None,
        input: vec!["text".into()],
        cost: ModelCost {
            rates: ModelCostRates {
                input: 1.0,
                output: 2.0,
                cache_read: 0.1,
                cache_write: 0.2,
            },
            tiers: Vec::new(),
        },
        context_window: 200_000,
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

fn anthropic_model() -> Model {
    let mut model = base_model("anthropic-messages", "anthropic", "claude-test");
    model.base_url = "https://api.anthropic.com".into();
    model.reasoning = true;
    model
}

fn anthropic_context() -> Context {
    Context {
        system_prompt: Some("You are helpful.".into()),
        messages: vec![user_message("Hello")],
        tools: None,
    }
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
        timestamp: 1,
        sections: None,
    })
}

fn assistant_message(
    api: &str,
    provider: &str,
    model: &str,
    content: Vec<ContentBlock>,
) -> Message {
    let mut message = AssistantMessage {
        api: api.into(),
        provider: provider.into(),
        model: model.into(),
        timestamp: 2,
        ..AssistantMessage::default()
    };
    message.content = content;
    message.stop_reason = StopReason::Stop;
    Message::Assistant(message)
}

fn anthropic_options() -> StreamOptions {
    StreamOptions {
        api_key: Some("sk-ant-api-test".into()),
        ..StreamOptions::default()
    }
}

// ---------------------------------------------------------------------------
// Anthropic
// ---------------------------------------------------------------------------

#[test]
fn anthropic_sends_sdk_default_headers() {
    // Upstream `createClient` merges `accept: application/json` and
    // `anthropic-dangerous-direct-browser-access: true` into every request.
    let (_, headers) = build_anthropic_request(
        &anthropic_model(),
        &anthropic_context(),
        &anthropic_options(),
    )
    .expect("request");
    assert_eq!(
        headers.get("accept").map(String::as_str),
        Some("application/json")
    );
    assert_eq!(
        headers
            .get("anthropic-dangerous-direct-browser-access")
            .map(String::as_str),
        Some("true")
    );
    assert_eq!(
        headers.get("anthropic-version").map(String::as_str),
        Some("2023-06-01")
    );
    assert_eq!(
        headers.get("x-api-key").map(String::as_str),
        Some("sk-ant-api-test")
    );
}

#[test]
fn anthropic_oauth_sends_claude_code_identity_headers() {
    let mut options = anthropic_options();
    options.api_key = Some("sk-ant-oat-token".into());
    let (_, headers) = build_anthropic_request(&anthropic_model(), &anthropic_context(), &options)
        .expect("request");
    assert_eq!(
        headers.get("user-agent").map(String::as_str),
        Some(format!("claude-cli/{CLAUDE_CODE_VERSION}").as_str())
    );
    assert_eq!(headers.get("x-app").map(String::as_str), Some("cli"));
    assert!(headers
        .get("authorization")
        .is_some_and(|value| value == "Bearer sk-ant-oat-token"));
    assert!(!headers.contains_key("x-api-key"));
}

#[test]
fn anthropic_pi_cache_retention_env_promotes_long_ttl() {
    // Upstream `resolveCacheRetention`: no explicit request value falls back to
    // `PI_CACHE_RETENTION=long`, then "short".
    let mut env = BTreeMap::new();
    env.insert("PI_CACHE_RETENTION".to_string(), "long".to_string());
    let options = StreamOptions {
        api_key: Some("sk-ant-api-test".into()),
        env: Some(env),
        ..StreamOptions::default()
    };
    let (body, _) = build_anthropic_request(&anthropic_model(), &anthropic_context(), &options)
        .expect("request");
    assert_eq!(
        body["system"][0]["cache_control"],
        json!({"type": "ephemeral", "ttl": "1h"}),
        "PI_CACHE_RETENTION=long must reach the Anthropic cache marker"
    );
}

#[test]
fn anthropic_long_retention_degrades_without_compat_support() {
    // Upstream `getCacheControl`: the `1h` ttl is gated on
    // `supportsLongCacheRetention` (default true).
    let mut model = anthropic_model();
    let compat = OpenAiCompat {
        supports_long_cache_retention: Some(false),
        ..OpenAiCompat::default()
    };
    model.compat = Some(compat);
    let options = StreamOptions {
        api_key: Some("sk-ant-api-test".into()),
        cache_retention: Some(CacheRetention::Long),
        ..StreamOptions::default()
    };
    let (body, _) =
        build_anthropic_request(&model, &anthropic_context(), &options).expect("request");
    assert_eq!(
        body["system"][0]["cache_control"],
        json!({"type": "ephemeral"})
    );
}

#[test]
fn anthropic_sends_server_side_fallback_models() {
    // Upstream `buildParams`: `params.fallbacks = allowedFallbackModels.map(
    // (fallback) => ({ model: fallback.model }))`; the beta header alone is
    // inert without the list.
    let mut model = anthropic_model();
    let compat = OpenAiCompat {
        allowed_fallback_models: Some(vec![
            json!({"provider": "anthropic", "model": "claude-fallback-a", "cost": {"input": 1}}),
            json!({"provider": "anthropic", "model": "claude-fallback-b"}),
            json!({"provider": "anthropic"}),
        ]),
        ..OpenAiCompat::default()
    };
    model.compat = Some(compat);
    let (body, headers) =
        build_anthropic_request(&model, &anthropic_context(), &anthropic_options())
            .expect("request");
    assert_eq!(
        body["fallbacks"],
        json!([{"model": "claude-fallback-a"}, {"model": "claude-fallback-b"}]),
        "only entries with a model id are sent, each as {{model}}"
    );
    assert!(headers
        .get("anthropic-beta")
        .is_some_and(|value| value.contains("server-side-fallback-2026-07-01")));
}

#[test]
fn anthropic_openrouter_sends_session_affinity_header() {
    // Upstream `getAnthropicCompat` + `createClient`: OpenRouter defaults
    // session affinity on with `x-session-id`.
    let mut model = anthropic_model();
    model.provider = "openrouter".into();
    model.base_url = "https://openrouter.ai/api/v1".into();
    let options = StreamOptions {
        api_key: Some("sk-ant-api-test".into()),
        session_id: Some("sess-123".into()),
        ..StreamOptions::default()
    };
    let (_, headers) =
        build_anthropic_request(&model, &anthropic_context(), &options).expect("request");
    assert_eq!(
        headers.get("x-session-id").map(String::as_str),
        Some("sess-123")
    );
    assert!(!headers.contains_key("x-session-affinity"));
}

#[test]
fn anthropic_session_affinity_gated_by_cache_retention_none() {
    let mut model = anthropic_model();
    model.provider = "openrouter".into();
    let options = StreamOptions {
        api_key: Some("sk-ant-api-test".into()),
        session_id: Some("sess-123".into()),
        cache_retention: Some(CacheRetention::None),
        ..StreamOptions::default()
    };
    let (_, headers) =
        build_anthropic_request(&model, &anthropic_context(), &options).expect("request");
    assert!(
        !headers.contains_key("x-session-id"),
        "retention \"none\" drops the cache session id and its affinity header"
    );
}

#[test]
fn anthropic_non_openrouter_has_no_affinity_header_by_default() {
    let options = StreamOptions {
        api_key: Some("sk-ant-api-test".into()),
        session_id: Some("sess-123".into()),
        ..StreamOptions::default()
    };
    let (_, headers) = build_anthropic_request(&anthropic_model(), &anthropic_context(), &options)
        .expect("request");
    assert!(!headers.contains_key("x-session-id"));
    assert!(!headers.contains_key("x-session-affinity"));
}

#[test]
fn anthropic_thinking_budgets_override_the_default_ladder() {
    // Upstream `adjustMaxTokensForThinking` reads `thinkingBudgets[level]`
    // before `DEFAULT_THINKING_BUDGETS`.
    let mut model = anthropic_model();
    model.max_tokens = 40_000;
    let options = StreamOptions {
        api_key: Some("sk-ant-api-test".into()),
        reasoning: Some(ThinkingLevel::Medium),
        thinking_budgets: Some(ThinkingBudgets {
            medium: Some(3_072),
            ..ThinkingBudgets::default()
        }),
        ..StreamOptions::default()
    };
    let (body, _) =
        build_anthropic_request(&model, &anthropic_context(), &options).expect("request");
    assert_eq!(body["thinking"]["budget_tokens"], 3_072);
}

#[test]
fn anthropic_thinking_budget_defaults_to_the_model_cap() {
    // Upstream: `maxTokens === undefined` keeps the model cap instead of
    // adding the budget on top of it.
    let mut model = anthropic_model();
    model.max_tokens = 12_000;
    let options = StreamOptions {
        api_key: Some("sk-ant-api-test".into()),
        reasoning: Some(ThinkingLevel::Medium),
        ..StreamOptions::default()
    };
    let (body, _) =
        build_anthropic_request(&model, &anthropic_context(), &options).expect("request");
    assert_eq!(body["max_tokens"], 12_000);
    assert_eq!(body["thinking"]["budget_tokens"], 8_192);
}

#[test]
fn anthropic_managed_effort_replays_per_message_effort_markers() {
    // Upstream `insertThinkingLevelMessages`: managed-effort models replay the
    // recorded effort before each assistant reply and pin the active effort in
    // a trailing effort-only system message.
    let mut model = anthropic_model();
    let compat = OpenAiCompat {
        supports_mid_convo_effort: Some(true),
        ..OpenAiCompat::default()
    };
    model.compat = Some(compat);
    let mut assistant = AssistantMessage {
        api: "anthropic-messages".into(),
        provider: "anthropic".into(),
        model: "claude-test".into(),
        timestamp: 2,
        ..AssistantMessage::default()
    };
    assistant.content = vec![ContentBlock::Text(TextContent {
        text: "hi".into(),
        text_signature: None,
    })];
    assistant.stop_reason = StopReason::Stop;
    assistant.provider_thinking_level = Some("low".into());
    let context = Context {
        system_prompt: None,
        messages: vec![
            user_message("go"),
            Message::Assistant(assistant),
            user_message("again"),
        ],
        tools: None,
    };
    let options = StreamOptions {
        api_key: Some("sk-ant-api-test".into()),
        reasoning: Some(ThinkingLevel::High),
        ..StreamOptions::default()
    };
    let (body, headers) = build_anthropic_request(&model, &context, &options).expect("request");
    let messages = body["messages"].as_array().expect("messages");
    let marker = json!({"role": "system", "content": [], "output_config": {"effort": "low"}});
    assert!(
        messages.contains(&marker),
        "the recorded effort must replay before its assistant message: {messages:?}"
    );
    assert_eq!(
        messages.last().cloned(),
        Some(json!({"role": "system", "content": [], "output_config": {"effort": "high"}})),
        "the active effort is pinned in the trailing marker"
    );
    // The managed-effort thinking branch is independent of the level.
    assert_eq!(
        body["thinking"],
        json!({"type": "adaptive", "display": "summarized",
               "block_binding": {"prefix_mismatch_behavior": "drop_block"}})
    );
    assert!(headers
        .get("anthropic-beta")
        .is_some_and(|value| value.contains("mid-conversation-output-config-2026-07-01")));
}

#[test]
fn anthropic_managed_effort_markers_skipped_when_thinking_disabled() {
    // Upstream `effortMessages = supportsMidConvoEffort && !thinkingDisabled`.
    let mut model = anthropic_model();
    let compat = OpenAiCompat {
        supports_mid_convo_effort: Some(true),
        ..OpenAiCompat::default()
    };
    model.compat = Some(compat);
    let options = StreamOptions {
        api_key: Some("sk-ant-api-test".into()),
        reasoning: Some(ThinkingLevel::Off),
        ..StreamOptions::default()
    };
    let (body, _) =
        build_anthropic_request(&model, &anthropic_context(), &options).expect("request");
    let messages = body["messages"].as_array().expect("messages");
    assert!(
        !messages
            .iter()
            .any(|message| message.get("output_config").is_some()),
        "no effort markers while thinking is disabled: {messages:?}"
    );
    assert_eq!(body["thinking"], json!({"type": "disabled"}));
}

#[test]
fn anthropic_stream_records_provider_thinking_level() {
    // Upstream records `output.providerThinkingLevel` on the reply so the next
    // request can replay it.
    let mut model = anthropic_model();
    let compat = OpenAiCompat {
        supports_mid_convo_effort: Some(true),
        ..OpenAiCompat::default()
    };
    model.compat = Some(compat);
    let mut parser = AnthropicSseParser::new(&model);
    parser = parser.with_provider_thinking_level("high");
    assert_eq!(
        parser.snapshot().provider_thinking_level.as_deref(),
        Some("high")
    );
}

// ---------------------------------------------------------------------------
// OpenAI Completions
// ---------------------------------------------------------------------------

#[test]
fn openai_completions_uses_max_completion_tokens_by_default() {
    let model = base_model("openai-completions", "openai", "gpt-5");
    let options = StreamOptions {
        max_tokens: Some(1_024),
        ..StreamOptions::default()
    };
    let body = build_openai_body(
        &model,
        &Context {
            messages: vec![user_message("hi")],
            ..Context::default()
        },
        &options,
        &resolve_compat(&model),
    )
    .expect("body");
    assert_eq!(body["max_completion_tokens"], 1_024);
    assert!(body.get("max_tokens").is_none());
}

#[test]
fn openai_completions_max_tokens_field_compat_override() {
    let mut model = base_model("openai-completions", "deepseek", "deepseek-chat");
    model.base_url = "https://api.deepseek.com/v1".into();
    let options = StreamOptions {
        max_tokens: Some(512),
        ..StreamOptions::default()
    };
    let body = build_openai_body(
        &model,
        &Context {
            messages: vec![user_message("hi")],
            ..Context::default()
        },
        &options,
        &resolve_compat(&model),
    )
    .expect("body");
    assert_eq!(body["max_tokens"], 512, "detected compat picks max_tokens");
    assert!(body.get("max_completion_tokens").is_none());
}

#[test]
fn openai_completions_omits_whitespace_only_assistant_text() {
    // Upstream `convertMessages`: assistant text parts are filtered to
    // non-blank ones before they are joined.
    let model = base_model("openai-completions", "openai", "gpt-5");
    let context = Context {
        messages: vec![assistant_message(
            "openai-completions",
            "openai",
            "gpt-5",
            vec![ContentBlock::Text(TextContent {
                text: "   ".into(),
                text_signature: None,
            })],
        )],
        ..Context::default()
    };
    let body = build_openai_body(
        &model,
        &context,
        &StreamOptions::default(),
        &resolve_compat(&model),
    )
    .expect("body");
    assert_eq!(
        body["messages"].as_array().map(Vec::len),
        Some(0),
        "an all-blank assistant message is dropped: {body}"
    );
}

#[test]
fn openai_completions_tool_call_keeps_empty_arguments_on_parse_failure() {
    // Upstream `parseToolCallArguments`: a malformed finished call keeps the
    // call with `arguments: {}` plus `argumentsError`.
    let model = base_model("openai-completions", "openai", "gpt-5");
    let body = concat!(
        "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",",
        "\"function\":{\"name\":\"search\",\"arguments\":\"{\\\"q\\\":\"}}]}}]}\n\n",
        "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n",
        "data: [DONE]\n\n"
    );
    let events = parse_openai_sse(body.as_bytes(), &model).expect("parses");
    let Some(AssistantMessageEvent::Done { message, .. }) = events.last() else {
        panic!("stream must end with Done");
    };
    let Some(ContentBlock::ToolCall(call)) = message.content.first() else {
        panic!("tool call block expected: {:?}", message.content);
    };
    assert_eq!(call.id, "call_1");
    assert!(call.arguments.is_empty(), "no partial prefix is kept");
    let error = call.arguments_error.as_deref().expect("argumentsError set");
    assert!(
        error.contains("Received arguments:"),
        "error carries the raw text: {error}"
    );
}

#[test]
fn openai_completions_thinking_precedes_text_in_one_delta() {
    // Upstream orders the thinking block before the text block, even when a
    // proxy aggregates both into a single delta.
    let model = base_model("openai-completions", "openai", "gpt-5");
    let body = concat!(
        "data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"think\",\"content\":\"answer\"}}]}\n\n",
        "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
        "data: [DONE]\n\n"
    );
    let events = parse_openai_sse(body.as_bytes(), &model).expect("parses");
    let Some(AssistantMessageEvent::Done { message, .. }) = events.last() else {
        panic!("stream must end with Done");
    };
    assert!(
        matches!(
            message.content.first(),
            Some(ContentBlock::Thinking(thinking)) if thinking.thinking == "think"
        ),
        "thinking block first: {:?}",
        message.content
    );
    assert!(
        matches!(message.content.get(1), Some(ContentBlock::Text(text)) if text.text == "answer"),
        "text block second: {:?}",
        message.content
    );
}

#[test]
fn openai_reasoning_detail_signature_round_trips() {
    // `reasoning_details` are replay metadata: serialized into the thinking
    // block's signature slot at block end and replayed as `reasoning_details`.
    let model = base_model("openai-completions", "openrouter", "openrouter/auto");
    let body = concat!(
        "data: {\"choices\":[{\"delta\":{\"reasoning_details\":[",
        "{\"type\":\"reasoning.text\",\"text\":\"step\"}]}}]}\n\n",
        "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
        "data: [DONE]\n\n"
    );
    let events = parse_openai_sse(body.as_bytes(), &model).expect("parses");
    let Some(AssistantMessageEvent::Done { message, .. }) = events.last() else {
        panic!("stream must end with Done");
    };
    let Some(ContentBlock::Thinking(thinking)) = message.content.first() else {
        panic!("thinking block expected: {:?}", message.content);
    };
    let signature = thinking
        .thinking_signature
        .as_deref()
        .expect("signature carries the details");
    let details: Value = serde_json::from_str(signature).expect("signature is JSON");
    assert_eq!(details[0]["type"], "reasoning.text");
    assert_eq!(details[0]["text"], "step");

    // Replay: the same model sends them back verbatim.
    let context = Context {
        messages: vec![Message::Assistant(message.clone())],
        ..Context::default()
    };
    let replay = build_openai_body(
        &model,
        &context,
        &StreamOptions::default(),
        &resolve_compat(&model),
    )
    .expect("body");
    assert_eq!(
        replay["messages"][0]["reasoning_details"],
        json!([{"type": "reasoning.text", "text": "step"}])
    );
}

#[test]
fn openai_parser_streams_usage_from_choice_fallback() {
    // Moonshot-style `choice.usage` is honored when the chunk has no usage.
    let model = base_model("openai-completions", "moonshotai", "kimi");
    let body = concat!(
        "data: {\"choices\":[{\"delta\":{\"content\":\"hi\"},",
        "\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":2}}]}\n\n",
        "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
        "data: [DONE]\n\n"
    );
    let events = parse_openai_sse(body.as_bytes(), &model).expect("parses");
    let Some(AssistantMessageEvent::Done { message, .. }) = events.last() else {
        panic!("stream must end with Done");
    };
    assert_eq!(message.usage.input, 10);
    assert_eq!(message.usage.output, 2);
}

// ---------------------------------------------------------------------------
// OpenAI Responses (shared codec)
// ---------------------------------------------------------------------------

#[test]
fn responses_min_output_tokens_floor() {
    // Upstream pins `max_output_tokens` at 16 so a tiny cap is not rejected.
    let model = base_model("openai-responses", "openai", "gpt-5.5");
    let compat = ResponsesCompat::default();
    let options = StreamOptions {
        max_tokens: Some(4),
        ..StreamOptions::default()
    };
    let params = build_responses_params(
        &model,
        &Context {
            messages: vec![user_message("hi")],
            ..Context::default()
        },
        &options,
        &compat,
        "gpt-5.5",
        OPENAI_TOOL_CALL_PROVIDERS,
        false,
        None,
    )
    .expect("params");
    assert_eq!(params["max_output_tokens"], 16);
}

#[test]
fn responses_long_cache_retention_marker() {
    let model = base_model("openai-responses", "openai", "gpt-5.5");
    let compat = ResponsesCompat::default();
    let options = StreamOptions {
        cache_retention: Some(CacheRetention::Long),
        session_id: Some("sess-1".into()),
        ..StreamOptions::default()
    };
    let params = build_responses_params(
        &model,
        &Context {
            messages: vec![user_message("hi")],
            ..Context::default()
        },
        &options,
        &compat,
        "gpt-5.5",
        OPENAI_TOOL_CALL_PROVIDERS,
        false,
        None,
    )
    .expect("params");
    assert_eq!(params["prompt_cache_retention"], "24h");
    assert_eq!(params["prompt_cache_key"], "sess-1");
}

#[test]
fn responses_configuration_update_carries_effort_items() {
    // Upstream `pushConfigurationUpdate`: the requested level rides in the
    // input items while the request effort is pinned to the fixed
    // `CONFIGURATION_UPDATE_REQUEST_EFFORT` so the cached prefix survives
    // level changes. Mid-conversation effort requires a reasoning model and a
    // requested (non-off) level.
    let mut model = base_model("openai-responses", "openai", "gpt-5.5");
    model.reasoning = true;
    let compat = ResponsesCompat::default();
    let options = StreamOptions {
        reasoning: Some(ThinkingLevel::High),
        ..StreamOptions::default()
    };
    let params = build_responses_params(
        &model,
        &Context {
            messages: vec![user_message("hi")],
            ..Context::default()
        },
        &options,
        &compat,
        "gpt-5.5",
        OPENAI_TOOL_CALL_PROVIDERS,
        false,
        Some("high"),
    )
    .expect("params");
    let input = params["input"].as_array().expect("input");
    let last = input.last().expect("input item");
    assert_eq!(last["type"], "configuration_update");
    assert_eq!(last["reasoning"]["effort"], "high");
    assert_eq!(params["reasoning"]["effort"], "medium");
}

#[test]
fn responses_malformed_tool_arguments_keep_call_with_error() {
    let model = base_model("openai-responses", "openai", "gpt-5.5");
    let body = concat!(
        "data: {\"type\":\"response.output_item.added\",\"output_index\":0,",
        "\"item\":{\"type\":\"function_call\",\"id\":\"fc_1\",\"call_id\":\"call_1\",\"name\":\"search\"}}\n\n",
        "data: {\"type\":\"response.function_call_arguments.delta\",\"output_index\":0,",
        "\"delta\":\"{\\\"q\\\":\"}\n\n",
        "data: {\"type\":\"response.output_item.done\",\"output_index\":0,",
        "\"item\":{\"type\":\"function_call\",\"id\":\"fc_1\",\"call_id\":\"call_1\",\"name\":\"search\",",
        "\"arguments\":\"{\\\"q\\\":\"}}\n\n",
        "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_1\",\"status\":\"completed\",",
        "\"output\":[]}}\n\n"
    );
    let events = parse_responses_sse(body.as_bytes(), &model).expect("parses");
    let Some(AssistantMessageEvent::Done { message, .. }) = events.last() else {
        panic!("stream must end with Done");
    };
    let Some(ContentBlock::ToolCall(call)) = message.content.first() else {
        panic!("tool call expected: {:?}", message.content);
    };
    assert_eq!(call.name, "search");
    assert!(call.arguments.is_empty());
    assert!(call.arguments_error.is_some());
}

// ---------------------------------------------------------------------------
// Codex WebSocket transport
// ---------------------------------------------------------------------------

#[test]
fn codex_websocket_url_maps_scheme() {
    assert_eq!(
        codex_ws::resolve_codex_websocket_url("https://chatgpt.com/backend-api"),
        "wss://chatgpt.com/backend-api/codex/responses"
    );
    assert_eq!(
        codex_ws::resolve_codex_websocket_url("http://127.0.0.1:8080"),
        "ws://127.0.0.1:8080/codex/responses"
    );
    assert_eq!(
        resolve_codex_url("https://chatgpt.com/backend-api/codex"),
        "https://chatgpt.com/backend-api/codex/responses"
    );
}

#[test]
fn codex_websocket_headers_replace_sse_specifics() {
    let mut model_headers = BTreeMap::new();
    model_headers.insert("x-model-header".to_string(), "1".to_string());
    let headers = codex_ws::build_codex_websocket_headers(
        Some(&model_headers),
        None,
        "acct_1",
        "token",
        "sess-1",
    );
    assert_eq!(
        headers.get("openai-beta").map(String::as_str),
        Some(codex_ws::OPENAI_BETA_RESPONSES_WEBSOCKETS)
    );
    assert_eq!(
        headers.get("session-id").map(String::as_str),
        Some("sess-1")
    );
    assert_eq!(
        headers.get("x-client-request-id").map(String::as_str),
        Some("sess-1")
    );
    assert!(!headers.contains_key("accept"));
    assert!(!headers.contains_key("content-type"));
    assert_eq!(headers.get("originator").map(String::as_str), Some("pi"));
    assert_eq!(
        headers.get("chatgpt-account-id").map(String::as_str),
        Some("acct_1")
    );
    assert_eq!(headers.get("x-model-header").map(String::as_str), Some("1"));
}

#[test]
fn codex_websocket_continuation_key_follows_cache_retention() {
    let options = StreamOptions {
        session_id: Some("sess-1".into()),
        ..StreamOptions::default()
    };
    assert_eq!(
        codex_ws::continuation_key(&options).as_deref(),
        Some("sess-1")
    );
    let options = StreamOptions {
        session_id: Some("sess-1".into()),
        cache_retention: Some(CacheRetention::None),
        ..StreamOptions::default()
    };
    assert!(codex_ws::continuation_key(&options).is_none());
    let options = StreamOptions::default();
    assert!(codex_ws::continuation_key(&options).is_none());
    let options = StreamOptions {
        session_id: Some("x".repeat(80)),
        ..StreamOptions::default()
    };
    assert_eq!(
        codex_ws::continuation_key(&options).map(|key| key.len()),
        Some(64),
        "the key is clamped exactly like prompt_cache_key"
    );
}

#[test]
fn codex_zstd_codec_report_matches_the_build() {
    // Upstream compresses the SSE body whenever node:zlib exposes zstd; pir
    // does so whenever the `zstd` codec is compiled in (`extensions`).
    assert_eq!(
        codex_ws::zstd_request_compression_available(),
        cfg!(feature = "extensions")
    );
    #[cfg(feature = "extensions")]
    {
        let payload = br#"{"model":"gpt-5.5","input":[]}"#;
        let compressed =
            codex_ws::compress_request_body_zstd(payload).expect("zstd codec compresses");
        let decoded = zstd::stream::decode_all(&compressed[..]).expect("round-trips");
        assert_eq!(decoded, payload);
    }
}

#[tokio::test]
async fn codex_websocket_sse_fallback_marker_skips_later_attempts() {
    // The `None` closes below are GLOBAL: they retire every pooled connection,
    // including other suites' in the merged `g_ai` binary. Take the shared
    // pool lock so no pool-continuity test is running a multi-turn reuse
    // check at the same time (the reset at the end wipes the pool too).
    let _pool = crate::common::codex_ws_pool_guard().await;
    codex_ws::close_codex_websocket_sessions(None).await;
    let model = base_model("openai-codex-responses", "openai-codex", "gpt-5.5-codex");
    let options = StreamOptions {
        session_id: Some("sess-fallback-marker".into()),
        ..StreamOptions::default()
    };
    let parser_for = || {
        let mut parser = ResponsesSseParser::new(&model);
        parser.codex_mode = true;
        parser
    };
    let sink = AssistantMessageEventStream::new();
    let body = json!({"model": "gpt-5.5-codex", "input": []});
    // Unreachable port: the transport fails, which records the fallback.
    let outcome = codex_ws::try_stream_codex_websocket(
        "ws://127.0.0.1:9/codex/responses",
        &body,
        "acct_1",
        "token",
        &model,
        &options,
        &mut parser_for(),
        &sink,
    )
    .await
    .expect("a connect failure is a fallback, not an error");
    assert!(matches!(outcome, codex_ws::CodexWsOutcome::Fallback));

    // The second attempt must not even try to connect: a bogus URL would
    // surface as a transport failure but the recorded marker short-circuits.
    let outcome = codex_ws::try_stream_codex_websocket(
        "ws://127.0.0.1:9/codex/responses",
        &body,
        "acct_1",
        "token",
        &model,
        &options,
        &mut parser_for(),
        &sink,
    )
    .await
    .expect("marker short-circuits");
    assert!(matches!(outcome, codex_ws::CodexWsOutcome::Fallback));

    // Resetting lets the transport be attempted again.
    codex_ws::reset_codex_websocket_state(Some("sess-fallback-marker")).await;
    codex_ws::close_codex_websocket_sessions(None).await;
}

#[test]
fn codex_account_id_extraction() {
    let token = format!(
        "eyJhbGciOiJub25lIn0.{}.sig",
        base64_url(
            serde_json::to_string(&json!({
                "https://api.openai.com/auth": {"chatgpt_account_id": "acct_42"}
            }))
            .expect("json")
            .as_bytes()
        )
    );
    assert_eq!(extract_account_id(&token).as_deref(), Ok("acct_42"));
    assert!(extract_account_id("not-a-jwt").is_err());
}

fn base64_url(bytes: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

#[test]
fn codex_error_body_maps_to_friendly_usage_limit() {
    let body = json!({
        "error": {
            "code": "usage_limit_reached",
            "message": "limit",
            "plan_type": "pro",
            "resets_at": 1_700_000_060u64
        }
    })
    .to_string();
    let parsed = parse_error_response(&body, 429, 1_700_000_000_000);
    let friendly = parsed.friendly_message.expect("friendly message");
    assert!(friendly.contains("ChatGPT usage limit"), "{friendly}");
    assert!(friendly.contains("pro plan"), "{friendly}");

    let error = enhance_codex_error(
        pir::ai::error::AiError::NonRetryable(format!("HTTP 429: {body}")),
        1_700_000_000_000,
    );
    let text = error.to_string();
    assert!(text.contains("ChatGPT usage limit"), "{text}");
}

#[test]
fn codex_request_body_shape() {
    let mut model = base_model("openai-codex-responses", "openai-codex", "gpt-5.5-codex");
    model.reasoning = true;
    let options = StreamOptions {
        session_id: Some("sess-1".into()),
        ..StreamOptions::default()
    };
    let body = build_codex_body(
        &model,
        &Context {
            system_prompt: Some("sys".into()),
            messages: vec![user_message("hi")],
            tools: None,
        },
        &options,
    )
    .expect("body");
    assert_eq!(body["store"], false);
    assert_eq!(body["stream"], true);
    assert_eq!(body["instructions"], "sys");
    assert_eq!(body["text"]["verbosity"], "low");
    assert_eq!(body["include"], json!(["reasoning.encrypted_content"]));
    assert_eq!(body["tool_choice"], "auto");
    assert_eq!(body["parallel_tool_calls"], true);
    assert_eq!(body["prompt_cache_key"], "sess-1");
}

// ---------------------------------------------------------------------------
// Google
// ---------------------------------------------------------------------------

#[test]
fn google_strict_prefer_tool_without_support_is_sent_non_strict() {
    // Upstream `resolveJsonSchemaStrictSampling`: a "prefer" tool on a model
    // without strict support falls back to its original schema and does not
    // select VALIDATED.
    let model = base_model("google-generative-ai", "google", "gemini-2.5-flash");
    let context = Context {
        tools: Some(vec![Tool {
            name: "search".into(),
            description: "Search".into(),
            parameters: json!({"type": "object", "properties": {"q": {"type": "string"}}}),
            constrained_sampling: Some(ConstrainedSamplingConfig::JsonSchema {
                strict: StrictPreference::Prefer,
            }),
        }]),
        ..Context::default()
    };
    let body = pir::ai::api::google::build_google_body(&model, &context, &StreamOptions::default())
        .expect("body");
    assert!(
        body.get("toolConfig").is_none(),
        "no functionCallingConfig without strict tools"
    );
    assert_eq!(
        body["tools"][0]["functionDeclarations"][0]["parametersJsonSchema"],
        json!({"type": "object", "properties": {"q": {"type": "string"}}})
    );
}

#[test]
fn google_strict_require_on_unsupported_model_is_an_error() {
    // Upstream: `strict === "require"` without strict support throws.
    let model = base_model("google-generative-ai", "google", "gemini-2.5-flash");
    let context = Context {
        tools: Some(vec![Tool {
            name: "search".into(),
            description: "Search".into(),
            parameters: json!({"type": "object", "properties": {}}),
            constrained_sampling: Some(ConstrainedSamplingConfig::JsonSchema {
                strict: StrictPreference::Require,
            }),
        }]),
        ..Context::default()
    };
    let error =
        pir::ai::api::google::build_google_body(&model, &context, &StreamOptions::default())
            .expect_err("must fail");
    assert!(
        error
            .to_string()
            .contains("requires JSON-schema constrained sampling"),
        "{error}"
    );
}

#[test]
fn google_strict_schema_expands_required_and_optional_nullability() {
    // Upstream `makeStrictJsonSchema`: every property becomes required,
    // `additionalProperties` is forced false, and optional properties are
    // wrapped in `anyOf` with null.
    let model = base_model("google-generative-ai", "google", "gemini-3-flash-preview");
    let context = Context {
        tools: Some(vec![Tool {
            name: "search".into(),
            description: "Search".into(),
            parameters: json!({
                "type": "object",
                "properties": {"q": {"type": "string"}},
            }),
            constrained_sampling: Some(ConstrainedSamplingConfig::JsonSchema {
                strict: StrictPreference::Require,
            }),
        }]),
        ..Context::default()
    };
    let body = pir::ai::api::google::build_google_body(&model, &context, &StreamOptions::default())
        .expect("body");
    assert_eq!(
        body["toolConfig"]["functionCallingConfig"]["mode"],
        "VALIDATED"
    );
    let schema = &body["tools"][0]["functionDeclarations"][0]["parametersJsonSchema"];
    assert_eq!(schema["additionalProperties"], false);
    assert_eq!(schema["required"], json!(["q"]));
    assert_eq!(
        schema["properties"]["q"],
        json!({"anyOf": [{"type": "string"}, {"type": "null"}]})
    );
}

#[test]
fn google_thinking_budgets_override_the_table() {
    // Upstream `getGoogleBudget(model, level, options.thinkingBudgets)`.
    let mut model = base_model("google-generative-ai", "google", "gemini-2.5-pro");
    model.reasoning = true;
    let options = StreamOptions {
        reasoning: Some(ThinkingLevel::Medium),
        thinking_budgets: Some(ThinkingBudgets {
            medium: Some(4_096),
            ..ThinkingBudgets::default()
        }),
        ..StreamOptions::default()
    };
    let body = pir::ai::api::google::build_google_body(&model, &Context::default(), &options)
        .expect("body");
    assert_eq!(
        body["generationConfig"]["thinkingConfig"]["thinkingBudget"],
        4_096
    );
}

#[test]
fn google_vertex_flash_lite_uses_the_vertex_table() {
    // Upstream google-vertex `getGoogleBudget` has no `2.5-flash-lite` row, so
    // that id falls into the `2.5-flash` row (minimal 128, not 512).
    assert_eq!(
        pir::ai::api::google::google_thinking_budget_with(
            "gemini-2.5-flash-lite",
            pir::ai::api::google::ResolvedGoogleLevel::Minimal,
            None,
            true,
        ),
        128
    );
    assert_eq!(
        pir::ai::api::google::google_thinking_budget_with(
            "gemini-2.5-flash-lite",
            pir::ai::api::google::ResolvedGoogleLevel::Minimal,
            None,
            false,
        ),
        512
    );
}

// ---------------------------------------------------------------------------
// Mistral
// ---------------------------------------------------------------------------

#[test]
fn mistral_wire_remap_covers_every_upstream_field() {
    let payload = json!({
        "model": "mistral-large-latest",
        "topP": 0.9,
        "maxTokens": 512,
        "randomSeed": 7,
        "toolChoice": "auto",
        "presencePenalty": 0.1,
        "frequencyPenalty": 0.2,
        "parallelToolCalls": true,
        "promptMode": "reasoning",
        "reasoningEffort": "high",
        "promptCacheKey": "sess-1",
        "safePrompt": true,
        "responseFormat": {
            "type": "json_schema",
            "jsonSchema": {"name": "x", "schemaDefinition": {"type": "object"}},
        },
        "messages": [
            {
                "role": "assistant",
                "toolCalls": [{"id": "a", "function": {"name": "f", "arguments": "{}"}}],
                "toolCallId": "a",
                "content": [
                    {"type": "image_url", "imageUrl": "https://x/y.png"},
                    {"type": "document_url", "documentUrl": "https://x/y.pdf"},
                    {"type": "input_audio", "inputAudio": "abc"},
                ],
            }
        ],
    });
    let wire = pir::ai::api::mistral::to_mistral_wire_payload(&payload);
    for field in [
        "top_p",
        "max_tokens",
        "random_seed",
        "tool_choice",
        "presence_penalty",
        "frequency_penalty",
        "parallel_tool_calls",
        "prompt_mode",
        "reasoning_effort",
        "prompt_cache_key",
        "safe_prompt",
    ] {
        assert!(wire.get(field).is_some(), "missing {field}: {wire}");
    }
    for field in [
        "topP",
        "maxTokens",
        "randomSeed",
        "toolChoice",
        "presencePenalty",
        "frequencyPenalty",
        "parallelToolCalls",
        "promptMode",
        "reasoningEffort",
        "promptCacheKey",
        "safePrompt",
    ] {
        assert!(wire.get(field).is_none(), "leaked {field}: {wire}");
    }
    assert_eq!(wire["response_format"]["json_schema"]["name"], "x");
    assert_eq!(
        wire["response_format"]["json_schema"]["schema"],
        json!({"type": "object"})
    );
    let message = &wire["messages"][0];
    assert!(message.get("tool_calls").is_some());
    assert!(message.get("tool_call_id").is_some());
    assert!(message.get("toolCalls").is_none());
    assert_eq!(message["content"][0]["image_url"], "https://x/y.png");
    assert_eq!(message["content"][1]["document_url"], "https://x/y.pdf");
    assert_eq!(message["content"][2]["input_audio"], "abc");
}

#[test]
fn openai_replays_legacy_encrypted_reasoning_from_tool_calls() {
    // Upstream `legacyReasoningDetails`: encrypted reasoning persisted on a
    // tool call's `thoughtSignature` is replayed as `reasoning_details` when no
    // thinking block carries signed details.
    let model = base_model("openai-completions", "openai", "gpt-5");
    let encrypted = json!({
        "type": "reasoning.encrypted",
        "id": "rs_1",
        "data": "opaque",
    })
    .to_string();
    let context = Context {
        messages: vec![assistant_message(
            "openai-completions",
            "openai",
            "gpt-5",
            vec![ContentBlock::ToolCall(ToolCall {
                id: "call_1".into(),
                name: "search".into(),
                arguments: [("q".to_string(), json!("x"))].into_iter().collect(),
                thought_signature: Some(encrypted),
                namespace: None,
                arguments_error: None,
            })],
        )],
        ..Context::default()
    };
    let body = build_openai_body(
        &model,
        &context,
        &StreamOptions::default(),
        &resolve_compat(&model),
    )
    .expect("body");
    assert_eq!(
        body["messages"][0]["reasoning_details"],
        json!([{"type": "reasoning.encrypted", "id": "rs_1", "data": "opaque"}])
    );
}

#[test]
fn openai_opencode_go_reasoning_signature_renamed_on_replay() {
    // Upstream: `opencode-go` streams reasoning under `reasoning` and expects
    // it back as `reasoning_content`.
    let mut model = base_model("openai-completions", "opencode-go", "gpt-oss-120b");
    model.reasoning = true;
    let context = Context {
        messages: vec![assistant_message(
            "openai-completions",
            "opencode-go",
            "gpt-oss-120b",
            vec![ContentBlock::Thinking(ThinkingContent {
                thinking: "step".into(),
                thinking_signature: Some("reasoning".into()),
                redacted: false,
            })],
        )],
        ..Context::default()
    };
    let body = build_openai_body(
        &model,
        &context,
        &StreamOptions::default(),
        &resolve_compat(&model),
    )
    .expect("body");
    let message = &body["messages"][0];
    assert_eq!(message["reasoning_content"], "step");
    assert!(
        message.get("reasoning").is_none(),
        "the raw `reasoning` field must not be replayed: {message}"
    );
}

#[test]
fn openai_llamacpp_reasoning_signature_replayed_verbatim() {
    // llama.cpp + gpt-oss: the recorded field name is replayed as-is.
    let mut model = base_model("openai-completions", "llamacpp", "gpt-oss-20b");
    model.reasoning = true;
    let context = Context {
        messages: vec![assistant_message(
            "openai-completions",
            "llamacpp",
            "gpt-oss-20b",
            vec![ContentBlock::Thinking(ThinkingContent {
                thinking: "step".into(),
                thinking_signature: Some("reasoning_content".into()),
                redacted: false,
            })],
        )],
        ..Context::default()
    };
    let body = build_openai_body(
        &model,
        &context,
        &StreamOptions::default(),
        &resolve_compat(&model),
    )
    .expect("body");
    assert_eq!(body["messages"][0]["reasoning_content"], "step");
}

// ---------------------------------------------------------------------------
// Shared tool-argument parsing
// ---------------------------------------------------------------------------

#[test]
fn tool_arg_parse_rejects_non_object_json() {
    let parsed = parse_final_tool_arguments("[1,2]");
    assert!(parsed.arguments.is_empty());
    assert_eq!(
        parsed.error.as_deref(),
        Some("Tool arguments must be a JSON object\n\nReceived arguments:\n[1,2]")
    );
}

#[test]
fn tool_arg_parse_repairs_invalid_string_escapes() {
    let parsed = parse_final_tool_arguments("{\"path\":\"C:\\\\tmp\\p\"}");
    assert_eq!(parsed.error, None, "invalid escapes are doubled, not fatal");
    assert_eq!(parsed.arguments["path"], "C:\\tmp\\p");
}

#[test]
fn tool_arg_parse_empty_input_is_an_empty_object() {
    let parsed = parse_final_tool_arguments("   ");
    assert!(parsed.arguments.is_empty());
    assert_eq!(parsed.error, None);
}

// ---------------------------------------------------------------------------
// Responses message conversion (codex call-provider set)
// ---------------------------------------------------------------------------

#[test]
fn responses_replays_tool_calls_and_outputs() {
    let model = base_model("openai-responses", "openai", "gpt-5.5");
    let call = ContentBlock::ToolCall(ToolCall {
        id: "call_1".into(),
        name: "search".into(),
        arguments: [("q".to_string(), json!("x"))].into_iter().collect(),
        thought_signature: None,
        namespace: None,
        arguments_error: None,
    });
    let context = Context {
        messages: vec![
            assistant_message("openai-responses", "openai", "gpt-5.5", vec![call]),
            Message::ToolResult(ToolResultMessage {
                role: ToolResultRole::ToolResult,
                tool_call_id: "call_1".into(),
                tool_name: "search".into(),
                content: vec![ContentBlock::Text(TextContent {
                    text: "ok".into(),
                    text_signature: None,
                })],
                details: None,
                usage: None,
                added_tool_names: Vec::new(),
                structured_content: None,
                nested_calls: None,
                is_error: false,
                duration_ms: None,
                timestamp: 3,
            }),
        ],
        ..Context::default()
    };
    let items = convert_responses_messages(
        &model,
        &context,
        OPENAI_TOOL_CALL_PROVIDERS,
        &ConvertResponsesMessagesOptions {
            include_system_prompt: false,
            ..Default::default()
        },
    );
    assert_eq!(items[0]["type"], "function_call");
    assert_eq!(items[0]["call_id"], "call_1");
    assert_eq!(items[0]["arguments"], "{\"q\":\"x\"}");
    assert_eq!(items[1]["type"], "function_call_output");
    assert_eq!(items[1]["call_id"], "call_1");
}

// ---------------------------------------------------------------------------
// OpenAI completions parser construction
// ---------------------------------------------------------------------------

#[test]
fn openai_parser_rejects_stream_without_finish_reason_by_default() {
    let model = base_model("openai-completions", "openai", "gpt-5");
    let body = concat!(
        "data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\n",
        "data: [DONE]\n\n"
    );
    let mut parser = OpenAiSseParser::new(&model);
    let mut events = Vec::new();
    parser.feed(body.as_bytes(), &mut events).expect("feeds");
    let error = parser.finish(&mut events).expect_err("must fail");
    assert!(
        error.to_string().contains("without finish_reason"),
        "{error}"
    );
}

// ---------------------------------------------------------------------------
// Request-level thinking configuration (upstream `thinkingDisplay`,
// `interleavedThinking`, `reasoningSummary`, `textVerbosity`)
// ---------------------------------------------------------------------------

#[test]
fn anthropic_request_options_control_thinking_display_and_interleaved_beta() {
    let model = anthropic_model();
    let options = StreamOptions {
        api_key: Some("sk-ant-api-test".into()),
        reasoning: Some(ThinkingLevel::Medium),
        thinking_display: Some(ThinkingDisplay::Omitted),
        interleaved_thinking: Some(false),
        ..StreamOptions::default()
    };
    let (body, headers) =
        build_anthropic_request(&model, &anthropic_context(), &options).expect("request");
    assert_eq!(body["thinking"]["type"], "enabled");
    assert_eq!(body["thinking"]["display"], "omitted");
    assert!(
        headers
            .get("anthropic-beta")
            .is_none_or(|value| !value.contains("interleaved-thinking")),
        "interleavedThinking=false drops the beta: {headers:?}"
    );

    // The omitted display is a wire value, not an omission.
    let options = StreamOptions {
        reasoning: Some(ThinkingLevel::Medium),
        thinking_display: Some(ThinkingDisplay::Summarized),
        interleaved_thinking: Some(true),
        ..options
    };
    let (body, headers) =
        build_anthropic_request(&model, &anthropic_context(), &options).expect("request");
    assert_eq!(body["thinking"]["display"], "summarized");
    assert!(
        headers
            .get("anthropic-beta")
            .is_some_and(|value| value.contains("interleaved-thinking-2025-05-14")),
        "interleavedThinking=true keeps the beta: {headers:?}"
    );
}

#[test]
fn codex_request_options_set_reasoning_summary_and_text_verbosity() {
    let mut model = base_model("openai-codex-responses", "openai-codex", "gpt-5.3-codex");
    model.reasoning = true;
    model.base_url = "https://chatgpt.com/backend-api".into();
    let context = Context {
        system_prompt: Some("You are helpful.".into()),
        messages: vec![user_message("Hello")],
        tools: None,
    };
    let options = StreamOptions {
        reasoning: Some(ThinkingLevel::Medium),
        reasoning_summary: Some(ReasoningSummary::Concise),
        text_verbosity: Some(TextVerbosity::High),
        ..StreamOptions::default()
    };
    let body = build_codex_body(&model, &context, &options).expect("body");
    assert_eq!(body["text"], json!({"verbosity": "high"}));
    assert_eq!(
        body["reasoning"],
        json!({"effort": "medium", "summary": "concise"})
    );
}

#[test]
fn responses_request_options_override_the_reasoning_summary() {
    let mut model = base_model("openai-responses", "openai", "gpt-5.5");
    model.reasoning = true;
    let context = Context {
        system_prompt: Some("You are helpful.".into()),
        messages: vec![user_message("Hello")],
        tools: None,
    };
    let options = StreamOptions {
        reasoning: Some(ThinkingLevel::High),
        reasoning_summary: Some(ReasoningSummary::Detailed),
        ..StreamOptions::default()
    };
    let params = build_responses_params(
        &model,
        &context,
        &options,
        &ResponsesCompat::default(),
        "gpt-5.5",
        OPENAI_TOOL_CALL_PROVIDERS,
        false,
        None,
    )
    .expect("params");
    assert_eq!(
        params["reasoning"],
        json!({"effort": "high", "summary": "detailed"})
    );
}
