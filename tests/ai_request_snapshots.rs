//! Golden request-shape snapshots for the OpenAI / Anthropic / Google adapters
//! (insta). Each snapshot freezes the request body one adapter builds for a
//! representative conversation — plain messages, thinking, and tool calls with
//! tool-result replay — so wire-shape regressions surface as diffs.
//!
//! Bodies are serialized through a key-sorting canonicalizer before
//! `assert_json_snapshot!`, so snapshot files show keys alphabetically and are
//! stable regardless of map implementation details. Formatting is insta's
//! pretty JSON (2-space indent). Snapshots live in `tests/snapshots/`
//! (`<test-binary>__<name>.snap`, insta's default layout for integration
//! tests). The Anthropic builder also returns headers; only the body is
//! snapshotted (headers carry the platform-dependent user-agent).

use insta::assert_json_snapshot;
use pir::ai::api::anthropic::build_anthropic_request;
use pir::ai::api::google::build_google_body;
use pir::ai::api::openai::{build_openai_body, build_openai_body_streamed, resolve_compat};
use pir::ai::types::*;
use serde_json::{json, Map, Value};
use std::collections::BTreeMap;

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

/// Shared tool schema (a fn because `json!` is not const-evaluable).
fn weather_schema() -> Value {
    json!({
        "type": "object",
        "properties": { "city": { "type": "string" } },
        "required": ["city"],
    })
}

/// Recursively sort object keys so snapshot files are canonical (and immune to
/// serde_json map-order details).
fn canonical(value: &Value) -> Value {
    match value {
        Value::Object(map) => {
            let mut keys: Vec<&str> = map.keys().map(String::as_str).collect();
            keys.sort_unstable();
            let mut out = Map::new();
            for key in keys {
                out.insert(key.to_string(), canonical(&map[key]));
            }
            Value::Object(out)
        }
        Value::Array(items) => Value::Array(items.iter().map(canonical).collect()),
        other => other.clone(),
    }
}

fn model(id: &str, api: &str, provider: &str, reasoning: bool, max_tokens: u64) -> Model {
    Model {
        id: id.into(),
        name: id.into(),
        api: api.into(),
        provider: provider.into(),
        base_url: "http://localhost:1".into(),
        reasoning,
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
        context_window: 200_000,
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

fn openai_model(reasoning: bool) -> Model {
    model("test", "openai-chat", "test", reasoning, 8192)
}

fn anthropic_model(reasoning: bool) -> Model {
    model(
        "claude-test",
        "anthropic-messages",
        "anthropic",
        reasoning,
        64_000,
    )
}

fn google_model(reasoning: bool) -> Model {
    model(
        "gemini-2.5-flash",
        "google-generative-ai",
        "google",
        reasoning,
        65_536,
    )
}

fn text_block(text: &str) -> ContentBlock {
    ContentBlock::Text(TextContent {
        text: text.into(),
        text_signature: None,
    })
}

fn thinking_block(text: &str, signature: Option<&str>) -> ContentBlock {
    ContentBlock::Thinking(ThinkingContent {
        thinking: text.into(),
        thinking_signature: signature.map(Into::into),
        redacted: false,
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

/// An assistant message that replays as same-model history for the given
/// adapter (thinking blocks and thought signatures survive).
fn assistant_msg(api: &str, provider: &str, model_id: &str, content: Vec<ContentBlock>) -> Message {
    Message::Assistant(AssistantMessage {
        content,
        api: api.into(),
        provider: provider.into(),
        model: model_id.into(),
        stop_reason: StopReason::ToolUse,
        ..AssistantMessage::default()
    })
}

fn tool_call(id: &str, name: &str, city: &str) -> ContentBlock {
    ContentBlock::ToolCall(ToolCall {
        id: id.into(),
        name: name.into(),
        arguments: BTreeMap::from([("city".to_string(), json!(city))]),
        thought_signature: None,
        namespace: None,
        arguments_error: None,
    })
}

fn signed_tool_call(
    id: &str,
    name: &str,
    city: &str,
    thought_signature: Option<&str>,
) -> ContentBlock {
    let ContentBlock::ToolCall(mut call) = tool_call(id, name, city) else {
        unreachable!()
    };
    call.thought_signature = thought_signature.map(Into::into);
    ContentBlock::ToolCall(call)
}

fn tool_result_msg(call_id: &str, tool_name: &str, text: &str) -> Message {
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

fn weather_tool() -> Tool {
    Tool {
        name: "get_weather".into(),
        description: "Get the current weather for a city.".into(),
        parameters: weather_schema(),
        constrained_sampling: None,
    }
}

fn options(
    temperature: Option<f64>,
    max_tokens: Option<u64>,
    reasoning: Option<ThinkingLevel>,
) -> StreamOptions {
    StreamOptions {
        temperature,
        max_tokens,
        reasoning,
        ..StreamOptions::default()
    }
}

// ---------------------------------------------------------------------------
// OpenAI chat/completions
// ---------------------------------------------------------------------------

/// Plain non-reasoning request: system + user message, sampling parameters.
/// Documents the standard OpenAI-compatible body: `store: false`,
/// `stream_options.include_usage`, `max_completion_tokens`.
#[test]
fn openai_plain_message_snapshot() {
    let model = openai_model(false);
    let context = Context {
        system_prompt: Some("You are helpful.".into()),
        messages: vec![user_msg("Hello")],
        ..Context::default()
    };
    let body = build_openai_body(
        &model,
        &context,
        &options(Some(0.7), Some(512), None),
        &resolve_compat(&model),
    )
    .unwrap();
    assert_json_snapshot!("openai_plain_message", canonical(&body));
}

/// Reasoning request: `reasoning_effort` from the thinking level, and the
/// system prompt replayed with the `developer` role (detected compat). The
/// assistant thinking block is not replayed (OpenAI-format thinking has no
/// replay channel without `reasoning_details` signatures).
#[test]
fn openai_thinking_level_snapshot() {
    let model = openai_model(true);
    let context = Context {
        system_prompt: Some("You are helpful.".into()),
        messages: vec![
            user_msg("Weather in Paris?"),
            assistant_msg(
                "openai-chat",
                "test",
                "test",
                vec![
                    thinking_block("Pondering the forecast.", None),
                    text_block("Let me check the forecast."),
                ],
            ),
            user_msg("Thanks"),
        ],
        ..Context::default()
    };
    let body = build_openai_body(
        &model,
        &context,
        &options(None, Some(2048), Some(ThinkingLevel::Medium)),
        &resolve_compat(&model),
    )
    .unwrap();
    assert_json_snapshot!("openai_thinking_level", canonical(&body));
}

/// Tool round-trip: declared function tool, assistant tool-call replay
/// (stringified arguments, `content: null`), and the `tool` result message.
#[test]
fn openai_tool_replay_snapshot() {
    let model = openai_model(false);
    let context = Context {
        messages: vec![
            user_msg("Weather in Paris?"),
            assistant_msg(
                "openai-chat",
                "test",
                "test",
                vec![tool_call("call_1", "get_weather", "Paris")],
            ),
            tool_result_msg("call_1", "get_weather", "Sunny, 22C"),
            user_msg("Thanks"),
        ],
        tools: Some(vec![weather_tool()]),
        ..Context::default()
    };
    let body = build_openai_body(
        &model,
        &context,
        &options(None, None, None),
        &resolve_compat(&model),
    )
    .unwrap();
    assert_json_snapshot!("openai_tool_replay", canonical(&body));
}

// ---------------------------------------------------------------------------
// Anthropic messages
// ---------------------------------------------------------------------------

/// Plain non-reasoning request: short cache retention marks the system prompt
/// and the last message block, temperature is sent while thinking is off.
#[test]
fn anthropic_plain_message_snapshot() {
    let model = anthropic_model(false);
    let context = Context {
        system_prompt: Some("You are helpful.".into()),
        messages: vec![user_msg("Hello")],
        ..Context::default()
    };
    let (body, _headers) =
        build_anthropic_request(&model, &context, &options(Some(0.7), Some(512), None)).unwrap();
    assert_json_snapshot!("anthropic_plain_message", canonical(&body));
}

/// Extended-thinking request: `thinking.budget_tokens` from the level, the
/// interleaved-thinking beta, `max_tokens` raised by the budget, and the
/// temperature dropped (incompatible with thinking).
#[test]
fn anthropic_thinking_budget_snapshot() {
    let model = anthropic_model(true);
    let context = Context {
        system_prompt: Some("You are helpful.".into()),
        messages: vec![user_msg("Hello")],
        ..Context::default()
    };
    let (body, _headers) = build_anthropic_request(
        &model,
        &context,
        &options(Some(0.2), Some(8192), Some(ThinkingLevel::Medium)),
    )
    .unwrap();
    assert_json_snapshot!("anthropic_thinking_budget", canonical(&body));
}

/// Tool round-trip: function tools with the legacy `input_schema` shape and
/// `eager_input_streaming`, signed thinking replay, `tool_use` / `tool_result`
/// blocks, and the cache breakpoint on the last conversation block.
#[test]
fn anthropic_tool_replay_snapshot() {
    let model = anthropic_model(false);
    let read_file = Tool {
        name: "read_file".into(),
        description: "Read a file from disk.".into(),
        parameters: json!({
            "type": "object",
            "properties": { "path": { "type": "string" } },
            "required": ["path"],
        }),
        constrained_sampling: None,
    };
    let context = Context {
        system_prompt: Some("You are helpful.".into()),
        messages: vec![
            user_msg("Check the weather in Paris."),
            assistant_msg(
                "anthropic-messages",
                "anthropic",
                "claude-test",
                vec![
                    thinking_block("The user wants weather data.", Some("sig-abc123")),
                    tool_call("call_1", "get_weather", "Paris"),
                ],
            ),
            tool_result_msg("call_1", "get_weather", "Sunny, 22C"),
            user_msg("Now write a note about it."),
        ],
        tools: Some(vec![weather_tool(), read_file]),
    };
    let (body, _headers) =
        build_anthropic_request(&model, &context, &options(None, Some(1024), None)).unwrap();
    assert_json_snapshot!("anthropic_tool_replay", canonical(&body));
}

// ---------------------------------------------------------------------------
// Google generative AI
// ---------------------------------------------------------------------------

/// Plain non-reasoning request: `systemInstruction`, `generationConfig` with
/// temperature / maxOutputTokens, no `thinkingConfig`.
#[test]
fn google_plain_message_snapshot() {
    let model = google_model(false);
    let context = Context {
        system_prompt: Some("You are helpful.".into()),
        messages: vec![user_msg("Hello")],
        ..Context::default()
    };
    let body = build_google_body(&model, &context, &options(Some(0.7), Some(512), None)).unwrap();
    assert_json_snapshot!("google_plain_message", canonical(&body));
}

/// Thinking request: `thinkingConfig` budget for the level, same-model
/// thinking replayed as a `thought` part carrying its `thoughtSignature`.
#[test]
fn google_thinking_budget_snapshot() {
    let model = google_model(true);
    let context = Context {
        system_prompt: Some("You are helpful.".into()),
        messages: vec![
            user_msg("Hello"),
            assistant_msg(
                "google-generative-ai",
                "google",
                "gemini-2.5-flash",
                vec![
                    thinking_block("Pondering the question.", Some("AAAAAAAAAAAAAAAAAAAAAA==")),
                    text_block("Here you go."),
                ],
            ),
            user_msg("Thanks"),
        ],
        ..Context::default()
    };
    let body = build_google_body(
        &model,
        &context,
        &options(None, Some(512), Some(ThinkingLevel::High)),
    )
    .unwrap();
    assert_json_snapshot!("google_thinking_budget", canonical(&body));
}

/// Tool round-trip: `functionDeclarations` with `parametersJsonSchema`,
/// `functionCall` replay carrying its `thoughtSignature`, and the
/// `functionResponse` with the `output` envelope merged into a user turn.
#[test]
fn google_tool_replay_snapshot() {
    let model = google_model(false);
    let context = Context {
        messages: vec![
            user_msg("Weather in Paris?"),
            assistant_msg(
                "google-generative-ai",
                "google",
                "gemini-2.5-flash",
                vec![signed_tool_call(
                    "call_1",
                    "get_weather",
                    "Paris",
                    Some("BBBBBBBBBBBBBBBBBBBBAA=="),
                )],
            ),
            tool_result_msg("call_1", "get_weather", "Sunny, 22C"),
            user_msg("Thanks"),
        ],
        tools: Some(vec![weather_tool()]),
        ..Context::default()
    };
    let body = build_google_body(&model, &context, &options(None, None, None)).unwrap();
    assert_json_snapshot!("google_tool_replay", canonical(&body));
}

/// Gemini ignores `const` in `parametersJsonSchema`, so the adapter sends
/// single-value `enum`s (upstream `rewriteConstAsEnum`). String and number
/// constants rewrite; an existing `enum` is left alone.
#[test]
fn google_const_as_enum_tool_snapshot() {
    let model = google_model(false);
    let context = Context {
        tools: Some(vec![Tool {
            name: "choose_mode".into(),
            description: "Choose a mode and retry count.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "mode": { "type": "string", "const": "fast" },
                    "retries": { "type": "number", "const": 2 },
                    "kept": { "type": "string", "const": "kept", "enum": ["kept", "other"] },
                    "literal": { "type": "string", "default": { "const": "not-a-schema" } },
                },
                "required": ["mode"],
            }),
            constrained_sampling: None,
        }]),
        ..Context::default()
    };
    let body = build_google_body(&model, &context, &options(None, None, None)).unwrap();
    assert_json_snapshot!("google_const_as_enum_tool", canonical(&body));
}

// ---------------------------------------------------------------------------
// Streamed body == value body (byte identity, memory lane)
// ---------------------------------------------------------------------------

/// INVARIANT (byte identity): the STREAMED body and the VALUE-based body must
/// serialize to the same bytes for every message shape the converter handles.
///
/// Why it needs a test: the streamed body emits a single-text message (a user
/// prompt, the system prompt, every summarizer prompt) from the BORROWED text
/// (`WireItem::Text`) instead of building a part `Value` plus an item `Value` —
/// two full copies of the text per request. The lazy item must produce the same
/// two-key object, so this test walks every branch of
/// `OpenAiMessageConverter` through both paths and compares the serialized
/// bytes, including the `requiresAssistantAfterToolResult` insertion and the
/// developer instruction role.
#[test]
fn streamed_and_value_bodies_are_byte_identical_for_every_shape() {
    let model = openai_model(false);
    let reasoning_model = openai_model(true);
    let context = Context {
        system_prompt: Some("lead system prompt".into()),
        messages: vec![
            // Single text block: the lazy fast path.
            user_msg("plain single-text prompt"),
            // Text + image: the parts-array path.
            Message::User(UserMessage {
                role: UserRole::User,
                content: vec![
                    text_block("with an image"),
                    ContentBlock::Image(ImageContent {
                        data: "aGVsbG8=".into(),
                        mime_type: "image/png".into(),
                    }),
                ],
                tools_added: None,
                tools_removed: None,
                timestamp: 1,
                sections: None,
            }),
            // Assistant text + tool call, then its result.
            assistant_msg(
                "openai-chat",
                "test",
                "test",
                vec![
                    text_block("calling the tool"),
                    tool_call("call_1", "get_weather", "Paris"),
                ],
            ),
            tool_result_msg("call_1", "get_weather", "Sunny, 22C"),
            // Thinking replay + text (the `requiresThinkingAsText` branch is
            // exercised through the compat below as well).
            assistant_msg(
                "openai-chat",
                "test",
                "test",
                vec![thinking_block("reasoning", None), text_block("the answer")],
            ),
            // A mid-conversation system message and a developer-role message.
            Message::System(UserMessage {
                role: UserRole::System,
                content: vec![text_block("mid-conversation instruction")],
                tools_added: None,
                tools_removed: None,
                timestamp: 2,
                sections: None,
            }),
            Message::Developer(UserMessage {
                role: UserRole::Developer,
                content: vec![text_block("developer instruction")],
                tools_added: None,
                tools_removed: None,
                timestamp: 3,
                sections: None,
            }),
            // An empty-content user message (skipped by both paths).
            Message::User(UserMessage {
                role: UserRole::User,
                content: vec![text_block("")],
                tools_added: None,
                tools_removed: None,
                timestamp: 4,
                sections: None,
            }),
        ],
        tools: Some(vec![weather_tool()]),
    };
    let options = options(None, None, None);

    // The resolved compat plus a forced-flag variant covering the remaining
    // converter branches: the `requiresAssistantAfterToolResult` insertion, the
    // tool-result `name` field, thinking-as-text, and the developer instruction
    // role (which needs a reasoning model, see `ReplayPlan::prepare`).
    let mut with_flags = resolve_compat(&reasoning_model);
    with_flags.requires_assistant_after_tool_result = Some(true);
    with_flags.requires_tool_result_name = Some(true);
    with_flags.requires_thinking_as_text = Some(true);
    with_flags.supports_developer_role = Some(true);

    for (model, compat) in [
        (&model, resolve_compat(&model)),
        (&reasoning_model, with_flags),
    ] {
        let value_body = build_openai_body(model, &context, &options, &compat).expect("value body");
        let streamed =
            build_openai_body_streamed(model, &context, &options, &compat).expect("streamed body");
        assert_eq!(
            serde_json::to_vec(&value_body).expect("serialize value"),
            serde_json::to_vec(&streamed).expect("serialize streamed"),
            "the streamed body must be byte-identical to the value body"
        );
    }
}
