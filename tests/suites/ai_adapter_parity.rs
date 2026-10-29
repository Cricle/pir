//! Adapter-parity tests against upstream HEAD behavior (ce950d78f):
//! Anthropic thinking/replay/inline-tools/strict fallback golden payloads,
//! OpenAI strict default + empty-part omission + sampling merge chain, and
//! retry/overflow classification from upstream `overflow.ts` / `retry.ts`.

use std::collections::BTreeMap;

use pir::ai::api::anthropic::{build_anthropic_request, parse_anthropic_sse, CLAUDE_CODE_VERSION};
use pir::ai::api::openai::{
    build_openai_body, convert_messages, convert_tool, resolve_sampling_params,
};
use pir::ai::types::*;
use serde_json::{json, Value};

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

fn anthropic_model() -> Model {
    Model {
        id: "claude-sonnet-4-5".into(),
        name: "Claude Sonnet 4.5".into(),
        api: "anthropic-messages".into(),
        provider: "anthropic".into(),
        base_url: "https://api.anthropic.com".into(),
        reasoning: true,
        thinking_level_map: None,
        input: vec!["text".into()],
        cost: ModelCost {
            rates: ModelCostRates {
                input: 0.0,
                output: 0.0,
                cache_read: 0.0,
                cache_write: 0.0,
            },
            tiers: vec![],
        },
        context_window: 200_000,
        max_tokens: 32_768,
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

fn openai_model() -> Model {
    Model {
        id: "gpt-test".into(),
        name: "GPT Test".into(),
        api: "openai-completions".into(),
        provider: "test".into(),
        base_url: "https://example.invalid/v1".into(),
        reasoning: true,
        thinking_level_map: None,
        input: vec!["text".into()],
        cost: ModelCost {
            rates: ModelCostRates {
                input: 0.0,
                output: 0.0,
                cache_read: 0.0,
                cache_write: 0.0,
            },
            tiers: vec![],
        },
        context_window: 16_000,
        max_tokens: 2_000,
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

fn error_assistant_message(provider: &str, message: &str) -> AssistantMessage {
    AssistantMessage {
        api: "openai-completions".into(),
        provider: provider.into(),
        model: "test-model".into(),
        stop_reason: StopReason::Error,
        error_message: Some(message.to_string()),
        timestamp: 0,
        ..AssistantMessage::default()
    }
}

fn tool(name: &str, parameters: Value, strict: Option<StrictPreference>) -> Tool {
    Tool {
        name: name.into(),
        description: "test tool".into(),
        parameters,
        constrained_sampling: strict.map(|strict| ConstrainedSamplingConfig::JsonSchema { strict }),
    }
}

fn context_with(messages: Vec<Message>) -> Context {
    Context {
        system_prompt: None,
        messages,
        tools: None,
    }
}

// ---------------------------------------------------------------------------
// Anthropic thinking config golden payloads
// ---------------------------------------------------------------------------

#[test]
fn anthropic_thinking_enabled_uses_budget_table() {
    // Upstream DEFAULT_THINKING_BUDGETS: minimal 1024, low 2048, medium 8192,
    // high 16384 (xhigh/max clamp to high); budget_tokens = min(budget,
    // max(0, maxTokens - 1024)); xhigh/max clamp to the high budget.
    let model = anthropic_model();
    let context = context_with(vec![user_message("Hello")]);
    for (level, expected_budget) in [
        (ThinkingLevel::Minimal, 1024),
        (ThinkingLevel::Low, 2048),
        (ThinkingLevel::Medium, 8192),
        (ThinkingLevel::High, 16384),
        (ThinkingLevel::XHigh, 16384),
        (ThinkingLevel::Max, 16384),
    ] {
        let options = StreamOptions {
            reasoning: Some(level),
            ..StreamOptions::default()
        };
        let (body, _) = build_anthropic_request(&model, &context, &options).unwrap();
        assert_eq!(
            body["thinking"],
            json!({"type": "enabled", "budget_tokens": expected_budget, "display": "summarized"}),
            "level {level:?}"
        );
        assert_eq!(body["max_tokens"], json!(32_768));
        assert!(body.get("output_config").is_none());
    }
}

#[test]
fn anthropic_thinking_off_sends_disabled_and_temperature_rules() {
    let model = anthropic_model();
    let context = context_with(vec![user_message("Hello")]);
    // Off: thinking disabled marker; temperature still allowed.
    let options = StreamOptions {
        reasoning: Some(ThinkingLevel::Off),
        temperature: Some(0.7),
        ..StreamOptions::default()
    };
    let (body, _) = build_anthropic_request(&model, &context, &options).unwrap();
    assert_eq!(body["thinking"], json!({"type": "disabled"}));
    assert_eq!(body["temperature"], json!(0.7));

    // Enabled thinking suppresses temperature (incompatible with extended
    // thinking, upstream buildParams condition).
    let options = StreamOptions {
        reasoning: Some(ThinkingLevel::High),
        temperature: Some(0.7),
        ..StreamOptions::default()
    };
    let (body, _) = build_anthropic_request(&model, &context, &options).unwrap();
    assert!(body.get("temperature").is_none());

    // Model with an explicit null "off" map entry omits the disabled marker.
    let mut mapped = anthropic_model();
    mapped.thinking_level_map = Some([("off".to_owned(), None)].into_iter().collect());
    let options = StreamOptions {
        reasoning: Some(ThinkingLevel::Off),
        ..StreamOptions::default()
    };
    let (body, _) = build_anthropic_request(&mapped, &context, &options).unwrap();
    assert!(body.get("thinking").is_none());
}

#[test]
fn anthropic_adaptive_thinking_maps_effort_levels() {
    // Upstream mapThinkingLevelToEffort: explicit thinkingLevelMap string
    // entries win, else minimal/low->low, medium->medium, else high.
    let mut model = anthropic_model();
    model.compat = Some(OpenAiCompat {
        force_adaptive_thinking: Some(true),
        ..OpenAiCompat::default()
    });
    model.thinking_level_map = Some(
        [
            ("off".to_owned(), None),
            ("low".to_owned(), Some("low".to_owned())),
            ("xhigh".to_owned(), Some("xhigh".to_owned())),
        ]
        .into_iter()
        .collect(),
    );
    let context = context_with(vec![user_message("Hello")]);
    let options = StreamOptions {
        reasoning: Some(ThinkingLevel::XHigh),
        ..StreamOptions::default()
    };
    let (body, _) = build_anthropic_request(&model, &context, &options).unwrap();
    assert_eq!(
        body["thinking"],
        json!({"type": "adaptive", "display": "summarized"})
    );
    assert_eq!(body["output_config"], json!({"effort": "xhigh"}));

    // No explicit map entry for high: falls back to the static table.
    let options = StreamOptions {
        reasoning: Some(ThinkingLevel::High),
        ..StreamOptions::default()
    };
    let (body, _) = build_anthropic_request(&model, &context, &options).unwrap();
    assert_eq!(body["output_config"], json!({"effort": "high"}));
}

#[test]
fn anthropic_thinking_replay_rules() {
    let model = anthropic_model();
    let assistant = Message::Assistant(AssistantMessage {
        api: "anthropic-messages".into(),
        provider: "anthropic".into(),
        model: "claude-sonnet-4-5".into(),
        content: vec![
            ContentBlock::Thinking(ThinkingContent {
                thinking: "thinking hard".into(),
                thinking_signature: Some("sig-1".into()),
                redacted: false,
            }),
            ContentBlock::Thinking(ThinkingContent {
                thinking: "[Reasoning redacted]".into(),
                thinking_signature: Some("opaque-payload".into()),
                redacted: true,
            }),
            ContentBlock::Thinking(ThinkingContent {
                thinking: "no signature".into(),
                thinking_signature: None,
                redacted: false,
            }),
            ContentBlock::Text(TextContent {
                text: "answer".into(),
                text_signature: None,
            }),
        ],
        stop_reason: StopReason::Stop,
        ..AssistantMessage::default()
    });
    let context = context_with(vec![user_message("Hi"), assistant]);
    let options = StreamOptions {
        reasoning: Some(ThinkingLevel::Off),
        api_key: Some("sk-ant-api".into()),
        ..StreamOptions::default()
    };
    let (body, _) = build_anthropic_request(&model, &context, &options).unwrap();
    let blocks = body["messages"][1]["content"].as_array().unwrap();
    assert_eq!(
        blocks[0],
        json!({"type": "thinking", "thinking": "thinking hard", "signature": "sig-1"})
    );
    assert_eq!(
        blocks[1],
        json!({"type": "redacted_thinking", "data": "opaque-payload"})
    );
    // Missing signature converts to plain text (upstream default).
    assert_eq!(blocks[2], json!({"type": "text", "text": "no signature"}));
    assert_eq!(blocks[3], json!({"type": "text", "text": "answer"}));

    // allowEmptySignature compat keeps empty-signature thinking blocks.
    let mut model = anthropic_model();
    model.compat = Some(OpenAiCompat {
        allow_empty_signature: Some(true),
        ..OpenAiCompat::default()
    });
    let (body, _) = build_anthropic_request(&model, &context, &options).unwrap();
    let blocks = body["messages"][1]["content"].as_array().unwrap();
    assert_eq!(
        blocks[2],
        json!({"type": "thinking", "thinking": "no signature", "signature": ""})
    );
}

#[test]
fn anthropic_inline_tools_beta_and_placeholder() {
    let mut model = anthropic_model();
    model.compat = Some(OpenAiCompat {
        supports_mid_convo_system_messages: Some(true),
        supports_mid_convo_tool_changes: Some(true),
        ..OpenAiCompat::default()
    });
    let read_tool = tool(
        "read",
        json!({"type": "object", "properties": {"path": {"type": "string"}}}),
        None,
    );
    let grep_tool = tool(
        "grep",
        json!({"type": "object", "properties": {"pattern": {"type": "string"}}}),
        None,
    );
    let later_system = Message::System(UserMessage {
        role: UserRole::System,
        content: vec![ContentBlock::Text(TextContent {
            text: "A search tool is now available.".into(),
            text_signature: None,
        })],
        tools_added: Some(vec![grep_tool.clone()]),
        tools_removed: Some(vec![ToolReference {
            name: "read".into(),
        }]),
        timestamp: 2,
        sections: None,
    });
    let context = Context {
        system_prompt: Some("base prompt".into()),
        messages: vec![
            user_message("Hi"),
            later_system,
            Message::Assistant(AssistantMessage {
                api: "anthropic-messages".into(),
                provider: "anthropic".into(),
                model: "claude-sonnet-4-5".into(),
                content: vec![ContentBlock::Text(TextContent {
                    text: "ok".into(),
                    text_signature: None,
                })],
                stop_reason: StopReason::Stop,
                ..AssistantMessage::default()
            }),
        ],
        tools: Some(vec![read_tool.clone()]),
    };
    let options = StreamOptions {
        api_key: Some("sk-ant-api".into()),
        ..StreamOptions::default()
    };
    let (body, headers) = build_anthropic_request(&model, &context, &options).unwrap();

    // Beta header carries the inline-tools feature.
    let beta = headers.get("anthropic-beta").unwrap();
    assert!(beta.contains("inline-tools-2026-09-15"), "betas: {beta}");

    // Request-level tool list stays fixed: initial tools + placeholder.
    let tools = body["tools"].as_array().unwrap();
    assert_eq!(tools.len(), 2);
    assert_eq!(tools[0]["name"], json!("read"));
    assert_eq!(tools[1]["name"], json!("__pi_deferred_placeholder__"));
    assert_eq!(tools[1]["defer_loading"], json!(true));

    // The later system turn becomes a system-role message with a
    // tool_addition block defining the tool by value, followed by a
    // tool_removal for the withdrawn tool.
    let system_turns: Vec<&Value> = body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|m| m["role"] == json!("system"))
        .collect();
    assert_eq!(system_turns.len(), 1);
    let blocks = system_turns[0]["content"].as_array().unwrap();
    assert_eq!(blocks[0]["type"], json!("text"));
    assert_eq!(
        blocks[1],
        json!({"type": "tool_removal", "tool": {"type": "tool_reference", "name": "read"}})
    );
    assert_eq!(blocks[2]["type"], json!("tool_addition"));
    assert_eq!(blocks[2]["tool"]["type"], json!("tool_definition"));
    assert_eq!(blocks[2]["tool"]["definition"]["name"], json!("grep"));
}

#[test]
fn anthropic_oauth_identity_and_stealth_version() {
    let model = anthropic_model();
    let read_tool = tool("read", json!({"type": "object", "properties": {}}), None);
    let context = Context {
        system_prompt: Some("base prompt".into()),
        messages: vec![Message::Assistant(AssistantMessage {
            api: "anthropic-messages".into(),
            provider: "anthropic".into(),
            model: "claude-sonnet-4-5".into(),
            content: vec![ContentBlock::ToolCall(ToolCall {
                id: "call_1".into(),
                name: "read".into(),
                arguments: BTreeMap::new(),
                thought_signature: None,
                namespace: None,
                arguments_error: None,
            })],
            stop_reason: StopReason::ToolUse,
            ..AssistantMessage::default()
        })],
        tools: Some(vec![read_tool]),
    };
    let options = StreamOptions {
        api_key: Some("sk-ant-oat01-test".into()),
        ..StreamOptions::default()
    };
    let (body, headers) = build_anthropic_request(&model, &context, &options).unwrap();
    // Claude Code identity headers with the 2.1.280 stealth version.
    assert_eq!(
        headers.get("user-agent").map(String::as_str),
        Some(CLAUDE_CODE_VERSION)
            .map(|v| format!("claude-cli/{v}"))
            .as_deref()
    );
    assert_eq!(CLAUDE_CODE_VERSION, "2.1.280");
    assert_eq!(headers.get("x-app").unwrap(), "cli");
    assert!(headers
        .get("authorization")
        .unwrap()
        .starts_with("Bearer sk-ant-oat"));
    // Identity system block first, then the base prompt.
    let system = body["system"].as_array().unwrap();
    assert_eq!(
        system[0]["text"],
        json!("You are Claude Code, Anthropic's official CLI for Claude.")
    );
    assert_eq!(system[1]["text"], json!("base prompt"));
    // Tool names map to Claude Code canonical casing.
    assert_eq!(body["tools"][0]["name"], json!("Read"));
    let tool_use = &body["messages"][0]["content"][0];
    assert_eq!(tool_use["name"], json!("Read"));
    // OAuth beta features.
    let beta = headers.get("anthropic-beta").unwrap();
    assert!(beta.contains("claude-code-20250219"));
    assert!(beta.contains("oauth-2025-04-20"));
}

// ---------------------------------------------------------------------------
// Anthropic strict tool schemas
// ---------------------------------------------------------------------------

#[test]
fn anthropic_strict_fallback_on_unsupported_keywords() {
    let mut model = anthropic_model();
    model.compat = Some(OpenAiCompat {
        supports_strict_tools: Some(true),
        ..OpenAiCompat::default()
    });
    let context = context_with(vec![user_message("Hi")]);
    let options = StreamOptions::default();

    // A "prefer" tool whose schema contains an unsupported keyword (maximum)
    // falls back to non-strict: legacy input_schema, no strict marker.
    let loose = tool(
        "loose",
        json!({"type": "object", "properties": {"n": {"type": "number", "maximum": 10}}}),
        Some(StrictPreference::Prefer),
    );
    let (body, _) =
        build_anthropic_request(&model, &context_with2(&context, vec![loose]), &options).unwrap();
    let converted = &body["tools"][0];
    assert!(converted.get("strict").is_none());
    assert_eq!(converted["input_schema"]["type"], json!("object"));

    // A clean "prefer" tool becomes strict: required expanded to every
    // property, additionalProperties false.
    let clean = tool(
        "clean",
        json!({"type": "object", "properties": {"a": {"type": "string"}, "b": {"type": "integer"}}}),
        Some(StrictPreference::Prefer),
    );
    let (body, _) =
        build_anthropic_request(&model, &context_with2(&context, vec![clean]), &options).unwrap();
    let converted = &body["tools"][0];
    assert_eq!(converted["strict"], json!(true));
    let required: Vec<&str> = converted["input_schema"]["required"]
        .as_array()
        .unwrap()
        .iter()
        .map(|value| value.as_str().expect("required entries are strings"))
        .collect();
    assert_eq!(required, vec!["a", "b"]);
    // Optional non-nullable properties are wrapped in anyOf with null.
    assert_eq!(
        converted["input_schema"]["properties"]["b"]["anyOf"][1],
        json!({"type": "null"})
    );
    assert_eq!(
        converted["input_schema"]["additionalProperties"],
        json!(false)
    );
}

fn context_with2(base: &Context, tools: Vec<Tool>) -> Context {
    Context {
        system_prompt: base.system_prompt.clone(),
        messages: base.messages.clone(),
        tools: Some(tools),
    }
}

#[test]
fn anthropic_strict_require_tool_fails_closed() {
    let mut model = anthropic_model();
    model.compat = Some(OpenAiCompat {
        supports_strict_tools: Some(true),
        ..OpenAiCompat::default()
    });
    let context = Context {
        system_prompt: None,
        messages: vec![user_message("Hi")],
        tools: Some(vec![tool(
            "strict",
            json!({"type": "object", "properties": {"n": {"type": "number", "minimum": 1}}}),
            Some(StrictPreference::Require),
        )]),
    };
    let error = build_anthropic_request(&model, &context, &StreamOptions::default()).unwrap_err();
    assert!(
        error
            .to_string()
            .contains("requires JSON-schema constrained sampling"),
        "error: {error}"
    );

    // Without strict support, a "require" tool fails too; a "prefer" tool
    // silently falls back.
    let mut model = anthropic_model();
    model.compat = None;
    let context = Context {
        system_prompt: None,
        messages: vec![user_message("Hi")],
        tools: Some(vec![tool(
            "strict",
            json!({"type": "object", "properties": {}}),
            Some(StrictPreference::Require),
        )]),
    };
    let error = build_anthropic_request(&model, &context, &StreamOptions::default()).unwrap_err();
    assert!(error.to_string().contains("strict tools are unsupported"));
}

// ---------------------------------------------------------------------------
// Anthropic SSE: thinking / redacted / refusal / error events
// ---------------------------------------------------------------------------

const THINKING_FIXTURE: &str = r#"data: {"type":"message_start","message":{"id":"msg_t","usage":{"input_tokens":5,"output_tokens":0}}}

data: {"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}}

data: {"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"step one "}}

data: {"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"sig#abc"}}

data: {"type":"content_block_stop","index":0}

data: {"type":"content_block_start","index":1,"content_block":{"type":"redacted_thinking","data":"encrypted"}}

data: {"type":"content_block_stop","index":1}

data: {"type":"content_block_start","index":2,"content_block":{"type":"text","text":""}}

data: {"type":"content_block_delta","index":2,"delta":{"type":"text_delta","text":"done"}}

data: {"type":"content_block_stop","index":2}

data: {"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":9}}

data: {"type":"message_stop"}

"#;

#[test]
fn anthropic_sse_thinking_and_redacted_blocks() {
    let model = anthropic_model();
    let events = parse_anthropic_sse(THINKING_FIXTURE.as_bytes(), &model).unwrap();
    assert!(events.iter().any(|event| matches!(
        event,
        AssistantMessageEvent::ThinkingStart {
            content_index: 0,
            ..
        }
    )));
    assert!(events.iter().any(|event| matches!(
        event,
        AssistantMessageEvent::ThinkingStart {
            content_index: 1,
            ..
        }
    )));
    let Some(AssistantMessageEvent::Done { message, .. }) = events.last() else {
        panic!("stream must end with Done");
    };
    assert_eq!(message.stop_reason, StopReason::Stop);
    // Thinking blocks are preserved with signature; redacted keeps the
    // encrypted payload in the signature slot (upstream replay contract).
    let thinking: Vec<&ThinkingContent> = message
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Thinking(thinking) => Some(thinking),
            _ => None,
        })
        .collect();
    assert_eq!(thinking.len(), 2);
    assert_eq!(thinking[0].thinking, "step one ");
    assert_eq!(thinking[0].thinking_signature.as_deref(), Some("sig#abc"));
    assert!(!thinking[0].redacted);
    assert_eq!(thinking[1].thinking, "[Reasoning redacted]");
    assert_eq!(thinking[1].thinking_signature.as_deref(), Some("encrypted"));
    assert!(thinking[1].redacted);
}

#[test]
fn anthropic_sse_refusal_maps_to_error() {
    let model = anthropic_model();
    let body = r#"data: {"type":"message_start","message":{"id":"m","usage":{}}}

data: {"type":"message_delta","delta":{"stop_reason":"refusal","stop_details":{"explanation":"cannot help with that"}}}

"#;
    let error = parse_anthropic_sse(body.as_bytes(), &model).unwrap_err();
    assert!(
        error.to_string().contains("cannot help with that"),
        "{error}"
    );
}

#[test]
fn anthropic_sse_error_event_fails() {
    let model = anthropic_model();
    let body = "event: error\ndata: {\"type\":\"error\",\"message\":\"overloaded\"}\n\n";
    let error = parse_anthropic_sse(body.as_bytes(), &model).unwrap_err();
    assert!(error.to_string().contains("overloaded"), "{error}");
}

// ---------------------------------------------------------------------------
// OpenAI: strict default, empty parts, sampling merge
// ---------------------------------------------------------------------------

#[test]
fn openai_strict_mode_defaults_to_false() {
    // Compat absent: no strict marker is sent at all.
    let model = openai_model();
    let plain = tool("f", json!({"type": "object", "properties": {}}), None);
    let converted = convert_tool(
        &plain,
        model.compat.as_ref().unwrap_or(&OpenAiCompat::default()),
    )
    .unwrap();
    assert!(converted["function"].get("strict").is_none());

    // supportsStrictMode true: strict marker present (false for plain tools).
    let mut model = openai_model();
    model.compat = Some(OpenAiCompat {
        supports_strict_mode: Some(true),
        ..OpenAiCompat::default()
    });
    let compat = model.compat.as_ref().unwrap();
    let converted = convert_tool(&plain, compat).unwrap();
    assert_eq!(converted["function"]["strict"], json!(false));

    // A "prefer" tool goes strict whenever the strict transform succeeds; the
    // unsupported-keyword fallback is Anthropic-specific, so numeric bounds
    // survive verbatim on the OpenAI-compatible path (upstream openai
    // convertTools passes no keyword check).
    let loose = tool(
        "loose",
        json!({"type": "object", "properties": {"n": {"type": "number", "minimum": 1}}}),
        Some(StrictPreference::Prefer),
    );
    let converted = convert_tool(&loose, compat).unwrap();
    assert_eq!(converted["function"]["strict"], json!(true));
    // Optional properties are null-wrapped; the bound survives under anyOf[0]
    // (upstream makeJsonSchemaNodeStrict).
    assert_eq!(
        converted["function"]["parameters"]["properties"]["n"]["anyOf"][0]["minimum"],
        json!(1)
    );

    // A clean "prefer" tool goes strict with the transformed schema.
    let clean = tool(
        "clean",
        json!({"type": "object", "properties": {"a": {"type": "string"}}}),
        Some(StrictPreference::Prefer),
    );
    let converted = convert_tool(&clean, compat).unwrap();
    assert_eq!(converted["function"]["strict"], json!(true));
    assert_eq!(
        converted["function"]["parameters"]["required"],
        json!(["a"])
    );
    assert_eq!(
        converted["function"]["parameters"]["additionalProperties"],
        json!(false)
    );

    // A "require" tool whose schema cannot be expressed in the strict subset
    // errors out.
    let strict_required = tool(
        "strict",
        json!({"type": "object", "oneOf": [{"type": "object"}]}),
        Some(StrictPreference::Require),
    );
    assert!(convert_tool(&strict_required, compat).is_err());
}

/// Recursively assert every object schema in a strict-transformed tree closes
/// its property set (upstream `makeJsonSchemaNodeStrict` sets
/// `additionalProperties: false` on every object node it visits).
fn assert_strict_object_closure(schema: &Value) {
    match schema {
        Value::Object(map) => {
            if map.get("type").and_then(Value::as_str) == Some("object") {
                assert_eq!(
                    map.get("additionalProperties"),
                    Some(&Value::Bool(false)),
                    "object schema missing additionalProperties:false: {schema}"
                );
            }
            for value in map.values() {
                assert_strict_object_closure(value);
            }
        }
        Value::Array(items) => {
            for item in items {
                assert_strict_object_closure(item);
            }
        }
        _ => {}
    }
}

#[test]
#[cfg(feature = "tool-edit")]
fn edit_tool_strict_schema_closes_nested_object_array_items() {
    use pir::agent::tool_execution::AgentTool;
    use pir::ai::api::constrained_sampling::make_strict_json_schema;

    // The real `edit` tool (not a hand-written fixture) is what OpenAI /
    // Copilot receive; `edits.items` is an array-element object and used to
    // lose the recursion's `additionalProperties: false` write-back.
    let parameters = pir::coding_agent::tools::EditTool::new()
        .parameters()
        .expect("edit tool declares parameters");
    let strict =
        make_strict_json_schema(&parameters, None).expect("edit schema is strict-compatible");
    assert_strict_object_closure(&strict);
    assert_eq!(strict["additionalProperties"], json!(false));
    let items = &strict["properties"]["edits"]["items"];
    assert_eq!(items["type"], json!("object"));
    assert_eq!(items["additionalProperties"], json!(false));
    assert_eq!(items["required"], json!(["oldText", "newText"]));

    // Same shape on the OpenAI wire path (`convert_tool`).
    let mut model = openai_model();
    model.compat = Some(OpenAiCompat {
        supports_strict_mode: Some(true),
        ..OpenAiCompat::default()
    });
    let compat = model.compat.as_ref().unwrap();
    let converted = convert_tool(
        &tool("edit", parameters, Some(StrictPreference::Prefer)),
        compat,
    )
    .expect("strict conversion succeeds");
    assert_eq!(converted["function"]["strict"], json!(true));
    let wire = &converted["function"]["parameters"];
    assert_strict_object_closure(wire);
    assert_eq!(
        wire["properties"]["edits"]["items"]["additionalProperties"],
        json!(false)
    );
}

#[test]
fn strict_transform_writes_back_nested_array_and_union_recursion() {
    use pir::ai::api::constrained_sampling::make_strict_json_schema;

    let schema = json!({
        "type": "object",
        "properties": {
            "groups": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {
                        "name": {"type": "string"},
                        "members": {
                            "type": "array",
                            "items": {
                                "type": "object",
                                "properties": {"id": {"type": "number"}},
                                "required": ["id"],
                            },
                        },
                    },
                    "required": ["name", "members"],
                },
            },
            "choice": {
                "anyOf": [{"type": "string", "const": "a"}, {"type": "integer"}],
            },
        },
        "required": ["groups"],
    });
    let strict = make_strict_json_schema(&schema, None).expect("strict");
    assert_strict_object_closure(&strict);

    let group = &strict["properties"]["groups"]["items"];
    assert_eq!(group["additionalProperties"], json!(false));
    assert_eq!(group["required"], json!(["name", "members"]));
    let member = &group["properties"]["members"]["items"];
    assert_eq!(member["additionalProperties"], json!(false));
    assert_eq!(member["required"], json!(["id"]));

    // `anyOf` branches are validated in place; the optional non-nullable
    // property is null-wrapped only after its branches were processed.
    assert_eq!(
        strict["properties"]["choice"],
        json!({
            "anyOf": [
                {"anyOf": [{"type": "string", "const": "a"}, {"type": "integer"}]},
                {"type": "null"}
            ]
        })
    );

    // `oneOf`/`allOf` stay unsupported by the strict subset (upstream
    // `UNSUPPORTED_STRICT_SCHEMA_KEYS`), including inside array elements.
    for keyword in ["oneOf", "allOf"] {
        let error = make_strict_json_schema(
            &json!({
                "type": "object",
                "properties": {
                    "groups": {
                        "type": "array",
                        "items": {"type": "object", (keyword): [{"type": "string"}]},
                    },
                },
            }),
            None,
        )
        .unwrap_err();
        assert!(
            error.to_string().contains("schemas are unsupported"),
            "{keyword}: {error}"
        );
    }

    // The same validation runs inside `anyOf` branches.
    let error = make_strict_json_schema(
        &json!({
            "type": "object",
            "properties": {
                "x": {"anyOf": [{"type": "string", "patternProperties": {}}]},
            },
        }),
        None,
    )
    .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("patternProperties schemas are unsupported"),
        "{error}"
    );
}

#[test]
fn openai_empty_text_parts_omitted() {
    let empty_text = |text: &str| {
        ContentBlock::Text(TextContent {
            text: text.into(),
            text_signature: None,
        })
    };
    // User messages drop empty text parts; a message with only empty parts is
    // skipped entirely.
    let messages = vec![
        Message::User(UserMessage {
            role: UserRole::User,
            content: vec![empty_text(""), empty_text("real")],
            tools_added: None,
            tools_removed: None,
            timestamp: 1,
            sections: None,
        }),
        Message::User(UserMessage {
            role: UserRole::User,
            content: vec![empty_text("")],
            tools_added: None,
            tools_removed: None,
            timestamp: 2,
            sections: None,
        }),
    ];
    let converted = convert_messages(
        &openai_model(),
        &context_with(messages),
        &OpenAiCompat::default(),
    );
    assert_eq!(converted.len(), 1);
    assert_eq!(converted[0]["content"], json!("real"));

    // Assistant messages with no content and no tool calls are skipped.
    let messages = vec![
        user_message("hi"),
        Message::Assistant(AssistantMessage {
            api: "openai-completions".into(),
            provider: "test".into(),
            model: "gpt-test".into(),
            content: vec![empty_text("")],
            stop_reason: StopReason::Stop,
            ..AssistantMessage::default()
        }),
    ];
    let converted = convert_messages(
        &openai_model(),
        &context_with(messages),
        &OpenAiCompat::default(),
    );
    assert_eq!(converted.len(), 1);

    // Empty tool results carry the upstream placeholder.
    let messages = vec![Message::ToolResult(ToolResultMessage {
        role: ToolResultRole::ToolResult,
        tool_call_id: "call_1".into(),
        tool_name: "search".into(),
        content: vec![],
        details: None,
        usage: None,
        added_tool_names: Vec::new(),
        structured_content: None,
        nested_calls: None,
        is_error: false,
        duration_ms: None,
        timestamp: 3,
    })];
    let converted = convert_messages(
        &openai_model(),
        &context_with(messages),
        &OpenAiCompat::default(),
    );
    assert_eq!(converted[0]["content"], json!("(no tool output)"));
}

#[test]
fn openai_sampling_params_merge_chain() {
    // Upstream resolveSamplingParams: model.samplingParams ->
    // samplingParamsByThinkingLevel[level] -> request params.
    let mut model = openai_model();
    model.sampling_params = Some(
        [
            ("top_p".to_owned(), json!(0.9)),
            ("top_k".to_owned(), json!(40)),
        ]
        .into_iter()
        .collect(),
    );
    model.compat = Some(OpenAiCompat {
        sampling_params_by_thinking_level: Some(
            [(
                "high".to_owned(),
                [("top_k".to_owned(), json!(5))].into_iter().collect(),
            )]
            .into_iter()
            .collect(),
        ),
        ..OpenAiCompat::default()
    });

    // At "high": per-level map overrides top_k, model default still supplies
    // top_p; request params override both.
    let merged = resolve_sampling_params(
        &model,
        Some(ThinkingLevel::High),
        Some(&[("top_p".to_owned(), json!(0.7))].into_iter().collect()),
    )
    .unwrap();
    assert_eq!(merged.get("top_p"), Some(&json!(0.7)));
    assert_eq!(merged.get("top_k"), Some(&json!(5)));

    // At other levels the per-level map does not apply.
    let merged = resolve_sampling_params(&model, Some(ThinkingLevel::Low), None).unwrap();
    assert_eq!(merged.get("top_k"), Some(&json!(40)));

    // End-to-end: body carries merged params after the named fields.
    let context = context_with(vec![user_message("hi")]);
    let options = StreamOptions {
        reasoning: Some(ThinkingLevel::High),
        sampling_params: Some(
            [("presence_penalty".to_owned(), json!(0.1))]
                .into_iter()
                .collect(),
        ),
        ..StreamOptions::default()
    };
    let body =
        build_openai_body(&model, &context, &options, model.compat.as_ref().unwrap()).unwrap();
    assert_eq!(body["top_p"], json!(0.9));
    assert_eq!(body["top_k"], json!(5));
    assert_eq!(body["presence_penalty"], json!(0.1));
}

#[test]
fn openai_stream_options_gated_by_compat() {
    let model = openai_model();
    let context = context_with(vec![user_message("hi")]);
    // Default: stream_options.include_usage present.
    let body = build_openai_body(
        &model,
        &context,
        &StreamOptions::default(),
        &OpenAiCompat::default(),
    )
    .unwrap();
    assert_eq!(body["stream_options"]["include_usage"], json!(true));
    // supportsUsageInStreaming false: omitted entirely.
    let compat = OpenAiCompat {
        supports_usage_in_streaming: false,
        ..OpenAiCompat::default()
    };
    let body = build_openai_body(&model, &context, &StreamOptions::default(), &compat).unwrap();
    assert!(body.get("stream_options").is_none());
}

// ---------------------------------------------------------------------------
// Retry / overflow classification (upstream overflow.ts + retry.ts)
// ---------------------------------------------------------------------------

#[test]
fn context_overflow_matches_upstream_cases() {
    // z.ai CN endpoint code-1261 patterns (#9805, #10208).
    for message_text in [
        "400 {\"code\":\"1261\",\"message\":\"Prompt too long\"}",
        "400 {\"code\":\"1261\",\"message\":\"Prompt exceeds max length\"}",
    ] {
        let message = error_assistant_message("zai", message_text);
        assert!(
            pir::ai::util::is_context_overflow(&message, Some(1_048_576)),
            "{message_text}"
        );
    }
    // Ollama explicit overflow.
    let message = error_assistant_message(
        "ollama",
        "400 `prompt too long; exceeded max context length by 100918 tokens`",
    );
    assert!(pir::ai::util::is_context_overflow(&message, Some(32_768)));
    // LiteLLM-wrapped OpenAI maximum context length.
    let message = error_assistant_message(
        "openai",
        "Error: 503 litellm.ServiceUnavailableError: litellm.MidStreamFallbackError: litellm.APIConnectionError: APIConnectionError: OpenAIException - Requested token count exceeds the model's maximum context length of 131072 tokens.",
    );
    assert!(pir::ai::util::is_context_overflow(&message, Some(131_072)));
    // DS4 with comma-separated counts.
    let message = error_assistant_message(
        "ds4",
        "Prompt has 5,958,968 tokens, but the configured context size is 256,000 tokens",
    );
    assert!(pir::ai::util::is_context_overflow(&message, Some(256_000)));
    // Cerebras bodyless 400 is scoped to that provider; a bodyless 413 is a
    // provider-independent request-size overflow (#11016).
    let cerebras = error_assistant_message("cerebras", "400 status code (no body)");
    assert!(pir::ai::util::is_context_overflow(&cerebras, Some(131_072)));
    let other = error_assistant_message("opencode-go", "400 status code (no body)");
    assert!(!pir::ai::util::is_context_overflow(&other, Some(1_000_000)));
    let other = error_assistant_message("opencode-go", "413 status code (no body)");
    assert!(pir::ai::util::is_context_overflow(&other, Some(1_000_000)));
    // Bedrock throttling "too many tokens" is NOT overflow.
    let message = error_assistant_message(
        "amazon-bedrock",
        "Throttling error: Too many tokens, please wait before trying again.",
    );
    assert!(!pir::ai::util::is_context_overflow(&message, Some(200_000)));
    // Generic rate limits are not overflow.
    let message = error_assistant_message(
        "openai",
        "Rate limit exceeded, please retry after 30 seconds.",
    );
    assert!(!pir::ai::util::is_context_overflow(&message, Some(200_000)));

    // Silent overflow: successful stop with input usage over the window.
    let mut message = error_assistant_message("zai", "");
    message.stop_reason = StopReason::Stop;
    message.error_message = None;
    message.usage.input = 1_100_000;
    assert!(pir::ai::util::is_context_overflow(
        &message,
        Some(1_048_576)
    ));

    // Xiaomi-style: length stop, zero output, filled window.
    let mut message = error_assistant_message("xiaomi", "");
    message.stop_reason = StopReason::Length;
    message.error_message = None;
    message.usage.input = 58;
    message.usage.cache_read = 1_048_512;
    message.usage.output = 0;
    assert!(pir::ai::util::is_context_overflow(
        &message,
        Some(1_048_576)
    ));
    // Zero-output length stop far below the window is not overflow.
    message.usage.cache_read = 0;
    message.usage.input = 100;
    assert!(!pir::ai::util::is_context_overflow(&message, Some(200_000)));

    // Recoverable length: stopped below the desired output cap.
    message.stop_reason = StopReason::Length;
    message.usage.output = 16;
    assert!(pir::ai::util::is_recoverable_length(&message, 128_000));
    assert!(!pir::ai::util::is_recoverable_length(&message, 16));
}

#[test]
fn retryable_assistant_error_matches_upstream_cases() {
    // Server-busy / at-capacity retryables.
    for message in [
        "Selected model is at capacity, please try again later",
        "server_busy: the server is overloaded",
        "servers are currently busy, please retry",
        "upstream connect error or disconnect/reset before headers",
        "Anthropic stream ended before message_stop",
        "pending stream has been canceled",
        "HTTP 503 Service Unavailable",
    ] {
        let message = error_assistant_message("openai", message);
        assert!(
            pir::ai::util::is_retryable_assistant_error(&message),
            "{} should be retryable",
            message.error_message.as_deref().unwrap()
        );
    }
    // Quota/subscription limits fail fast.
    for message in [
        "Error: 429 insufficient_quota: You exceeded your current quota",
        "GoUsageLimitError: monthly usage limit reached",
        "You have run out of budget",
    ] {
        let message = error_assistant_message("openai", message);
        assert!(
            !pir::ai::util::is_retryable_assistant_error(&message),
            "{} should not be retryable",
            message.error_message.as_deref().unwrap()
        );
    }
    // Non-error stops are never retryable.
    let mut message = error_assistant_message("openai", "overloaded");
    message.stop_reason = StopReason::Stop;
    assert!(!pir::ai::util::is_retryable_assistant_error(&message));
}

#[test]
fn retry_delay_doubles_and_caps() {
    assert_eq!(pir::ai::util::retry_delay_ms(500, 1, None), 500);
    assert_eq!(pir::ai::util::retry_delay_ms(500, 2, None), 1_000);
    assert_eq!(pir::ai::util::retry_delay_ms(500, 3, None), 2_000);
    // Cap applies (default 60s).
    assert_eq!(pir::ai::util::retry_delay_ms(500, 20, None), 60_000);
    // Custom cap.
    assert_eq!(pir::ai::util::retry_delay_ms(500, 5, Some(4_000)), 4_000);
}

#[test]
fn retry_after_parses_seconds_dates_and_falls_back() {
    use pir::ai::transport::parse_retry_after;
    use std::time::Duration;
    // Integer and fractional delay-seconds.
    assert_eq!(parse_retry_after("120"), Some(Duration::from_secs(120)));
    assert_eq!(parse_retry_after("0.5"), Some(Duration::from_millis(500)));
    // HTTP-date in the future resolves to the remaining delay.
    assert!(parse_retry_after("Wed, 21 Oct 2099 07:28:00 GMT").is_some());
    // Past HTTP-date clamps to zero.
    assert_eq!(
        parse_retry_after("Wed, 21 Oct 2015 07:28:00 GMT"),
        Some(Duration::ZERO)
    );
    // Unparseable values return None -> callers apply exponential backoff.
    assert_eq!(parse_retry_after("soon-ish"), None);
    assert_eq!(parse_retry_after(""), None);
}

// ---------------------------------------------------------------------------
// Message shape fields (deliverable 1)
// ---------------------------------------------------------------------------

#[test]
fn message_shape_fields_skip_when_none_and_roundtrip() {
    let assistant = AssistantMessage {
        api: "anthropic-messages".into(),
        provider: "anthropic".into(),
        model: "claude-sonnet-4-5".into(),
        stop_reason: StopReason::Stop,
        ..AssistantMessage::default()
    };
    let value = serde_json::to_value(&assistant).unwrap();
    // Optional fields are absent when unset (wire parity with upstream).
    assert!(value.get("thinkingLevel").is_none());
    assert!(value.get("durationMs").is_none());

    let assistant = AssistantMessage {
        thinking_level: Some(ThinkingLevel::High),
        duration_ms: Some(1_234),
        ..assistant
    };
    let value = serde_json::to_value(&assistant).unwrap();
    assert_eq!(value["thinkingLevel"], json!("high"));
    assert_eq!(value["durationMs"], json!(1_234));
    let decoded: AssistantMessage = serde_json::from_value(value).unwrap();
    assert_eq!(decoded.thinking_level, Some(ThinkingLevel::High));
    assert_eq!(decoded.duration_ms, Some(1_234));

    // Tool result: structuredContent / nestedCalls / durationMs.
    let result = ToolResultMessage {
        role: ToolResultRole::ToolResult,
        tool_call_id: "call_1".into(),
        tool_name: "search".into(),
        content: vec![],
        details: None,
        usage: None,
        added_tool_names: Vec::new(),
        structured_content: Some(json!({"hits": 3})),
        nested_calls: Some(NestedToolCalls {
            complete: false,
            calls: vec![NestedToolCallRecord {
                id: "nested_1".into(),
                name: "read".into(),
                arguments: Some([("path".to_owned(), json!("a.ts"))].into_iter().collect()),
                arguments_bytes: None,
                status: NestedToolCallStatus::Ok,
                duration_ms: Some(12),
                error: None,
            }],
        }),
        is_error: false,
        duration_ms: Some(50),
        timestamp: 0,
    };
    let value = serde_json::to_value(&result).unwrap();
    assert_eq!(value["structuredContent"], json!({"hits": 3}));
    assert_eq!(value["nestedCalls"]["complete"], json!(false));
    assert_eq!(value["nestedCalls"]["calls"][0]["status"], json!("ok"));
    assert_eq!(value["durationMs"], json!(50));
    let decoded: ToolResultMessage = serde_json::from_value(value).unwrap();
    assert_eq!(decoded, result);

    // Legacy JSON without the new fields decodes with None.
    let legacy = json!({
        "role": "toolResult",
        "toolCallId": "c",
        "toolName": "t",
        "content": [],
        "isError": false,
        "timestamp": 0,
    });
    let decoded: ToolResultMessage = serde_json::from_value(legacy).unwrap();
    assert!(decoded.structured_content.is_none());
    assert!(decoded.nested_calls.is_none());
    assert!(decoded.duration_ms.is_none());
}
