//! Google Generative AI adapter tests: golden request-body serialization
//! (shapes mirrored from upstream `packages/ai/test/google-*` fixtures),
//! thinking-level/budget mapping, SSE stream parsing (multi-chunk, thought
//! parts, usage, finish reasons, malformed payloads), error mapping, and an
//! end-to-end provider contract test against a local mock server. No live
//! network is ever touched.

use pir::ai::api::google::{
    build_google_body, clamp_google_thinking_level, convert_google_contents,
    google_thinking_budget, map_stop_reason, parse_google_sse, requires_tool_call_id,
    resolve_google_thinking_level, uses_google_thinking_level, GoogleGenerativeAiProvider,
    GOOGLE_API_ID, GOOGLE_BASE_URL,
};
use pir::ai::provider::Provider;
use pir::ai::providers::{DynamicProviderConfig, RegisterDynamicProvider};
use pir::ai::transport::{ProviderRequestClient, ProviderRequestConfig};
use pir::ai::types::*;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

const VALID_SIGNATURE: &str = "AAAAAAAAAAAAAAAAAAAAAA==";

fn google_model(id: &str, base_url: &str) -> Model {
    Model {
        id: id.to_string(),
        name: id.to_string(),
        api: GOOGLE_API_ID.to_string(),
        provider: "google".to_string(),
        base_url: base_url.to_string(),
        reasoning: true,
        thinking_level_map: None,
        input: vec!["text".into(), "image".into()],
        cost: ModelCost {
            rates: ModelCostRates {
                input: 1.25,
                output: 10.0,
                cache_read: 0.31,
                cache_write: 0.0,
            },
            tiers: Vec::new(),
        },
        context_window: 1_048_576,
        max_tokens: 65_536,
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

fn with_level_map(model: &Model, map: &[(&str, Option<&str>)]) -> Model {
    let mut mapped = model.clone();
    let mut level_map = BTreeMap::new();
    for (key, value) in map {
        level_map.insert((*key).to_string(), value.map(str::to_string));
    }
    mapped.thinking_level_map = Some(level_map);
    mapped
}

fn text_block(text: &str) -> ContentBlock {
    ContentBlock::Text(TextContent {
        text: text.to_string(),
        text_signature: None,
    })
}

fn text_block_signed(text: &str, signature: Option<&str>) -> ContentBlock {
    ContentBlock::Text(TextContent {
        text: text.to_string(),
        text_signature: signature.map(str::to_string),
    })
}

fn thinking_block(text: &str, signature: Option<&str>) -> ContentBlock {
    ContentBlock::Thinking(ThinkingContent {
        thinking: text.to_string(),
        thinking_signature: signature.map(str::to_string),
        redacted: false,
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

fn assistant_message(content: Vec<ContentBlock>, same_model: bool) -> Message {
    Message::Assistant(AssistantMessage {
        content,
        api: GOOGLE_API_ID.into(),
        provider: if same_model {
            "google".into()
        } else {
            "other".into()
        },
        model: if same_model {
            "gemini-2.5-flash".into()
        } else {
            "other-model".into()
        },
        stop_reason: StopReason::ToolUse,
        ..AssistantMessage::default()
    })
}

fn tool_result(tool_call_id: &str, tool_name: &str, text: &str, is_error: bool) -> Message {
    Message::ToolResult(ToolResultMessage {
        role: ToolResultRole::ToolResult,
        tool_call_id: tool_call_id.to_string(),
        tool_name: tool_name.to_string(),
        content: if text.is_empty() {
            Vec::new()
        } else {
            vec![text_block(text)]
        },
        details: None,
        usage: None,
        added_tool_names: Vec::new(),
        structured_content: None,
        nested_calls: None,
        is_error,
        duration_ms: None,
        timestamp: 2,
    })
}

// ---------------------------------------------------------------------------
// Golden request-body serialization
// ---------------------------------------------------------------------------

#[test]
fn golden_request_body_matches_upstream_shapes() {
    let model = google_model("gemini-2.5-flash", GOOGLE_BASE_URL);
    let mut args = BTreeMap::new();
    args.insert("command".to_string(), json!("ls -la"));
    let context = Context {
        system_prompt: Some("You are helpful".into()),
        messages: vec![
            user_message("Hello"),
            assistant_message(
                vec![
                    thinking_block(" pondering", Some(VALID_SIGNATURE)),
                    text_block("Here you go"),
                    ContentBlock::ToolCall(ToolCall {
                        id: "call 1/x".into(),
                        name: "bash".into(),
                        arguments: args,
                        thought_signature: Some(VALID_SIGNATURE.into()),
                        namespace: None,
                        arguments_error: None,
                    }),
                ],
                true,
            ),
            tool_result("call 1/x", "bash", "total 0", false),
            Message::User(UserMessage {
                role: UserRole::User,
                content: vec![
                    text_block("What is this?"),
                    ContentBlock::Image(ImageContent {
                        data: "aGVsbG8=".into(),
                        mime_type: "image/png".into(),
                    }),
                ],
                tools_added: None,
                tools_removed: None,
                timestamp: 3,
                sections: None,
            }),
        ],
        tools: Some(vec![Tool {
            name: "bash".into(),
            description: "Run a command".into(),
            parameters: json!({
                "type": "object",
                "properties": { "command": { "type": "string" } },
            }),
            constrained_sampling: None,
        }]),
    };

    let body = build_google_body(
        &model,
        &context,
        &StreamOptions {
            temperature: Some(0.7),
            max_tokens: Some(512),
            reasoning: Some(ThinkingLevel::High),
            ..StreamOptions::default()
        },
    )
    .unwrap();

    let expected = json!({
        "contents": [
            { "role": "user", "parts": [{ "text": "Hello" }] },
            { "role": "model", "parts": [
                { "thought": true, "text": " pondering", "thoughtSignature": VALID_SIGNATURE },
                { "text": "Here you go" },
                { "functionCall": { "name": "bash", "args": { "command": "ls -la" } },
                  "thoughtSignature": VALID_SIGNATURE },
            ] },
            { "role": "user", "parts": [
                { "functionResponse": { "name": "bash", "response": { "output": "total 0" } } }
            ] },
            { "role": "user", "parts": [
                { "text": "What is this?" },
                { "inlineData": { "mimeType": "image/png", "data": "aGVsbG8=" } }
            ] },
        ],
        "systemInstruction": { "parts": [{ "text": "You are helpful" }] },
        "generationConfig": {
            "temperature": 0.7,
            "maxOutputTokens": 512,
            "thinkingConfig": { "includeThoughts": true, "thinkingBudget": 24576 },
        },
        "tools": [{ "functionDeclarations": [
            {
                "name": "bash",
                "description": "Run a command",
                "parametersJsonSchema": {
                    "type": "object",
                    "properties": { "command": { "type": "string" } },
                },
            },
        ] }],
    });
    assert_eq!(
        body, expected,
        "golden request body diverged from upstream shape"
    );
}

#[test]
fn gemini3_ids_normalize_tool_call_ids_and_add_function_call_ids() {
    let model = google_model("gemini-3-flash-preview", GOOGLE_BASE_URL);
    let context = Context {
        messages: vec![
            assistant_message(
                vec![ContentBlock::ToolCall(ToolCall {
                    id: "call_1/odd id".into(),
                    name: "bash".into(),
                    arguments: BTreeMap::new(),
                    thought_signature: None,
                    namespace: None,
                    arguments_error: None,
                })],
                true,
            ),
            tool_result("call_1/odd id", "bash", "ok", false),
        ],
        ..Context::default()
    };
    let body = build_google_body(&model, &context, &StreamOptions::default()).unwrap();
    let parts = &body["contents"][0]["parts"];
    assert_eq!(parts[0]["functionCall"]["id"], "call_1_odd_id");
    assert_eq!(parts[0]["functionCall"]["name"], "bash");
    let response = &body["contents"][1]["parts"][0]["functionResponse"];
    assert_eq!(response["id"], "call_1_odd_id");
    assert_eq!(response["response"]["output"], "ok");
}

#[test]
fn tool_results_merge_into_one_user_turn_and_errors_use_error_key() {
    let model = google_model("gemini-2.5-flash", GOOGLE_BASE_URL);
    let context = Context {
        messages: vec![
            tool_result("call_1", "search", "hit", false),
            tool_result("call_2", "read", "", true),
        ],
        ..Context::default()
    };
    let body = build_google_body(&model, &context, &StreamOptions::default()).unwrap();
    assert_eq!(
        body["contents"].as_array().unwrap().len(),
        1,
        "responses merge into one user turn"
    );
    let parts = &body["contents"][0]["parts"];
    assert_eq!(parts[0]["functionResponse"]["name"], "search");
    assert_eq!(parts[0]["functionResponse"]["response"]["output"], "hit");
    assert_eq!(parts[1]["functionResponse"]["name"], "read");
    assert_eq!(parts[1]["functionResponse"]["response"]["error"], "");
}

#[test]
fn cross_model_thinking_drops_and_unsigned_empty_blocks_drop() {
    let model = google_model("gemini-2.5-flash", GOOGLE_BASE_URL);
    let context = Context {
        messages: vec![assistant_message(
            vec![
                thinking_block("", None),
                thinking_block("foreign thoughts", Some(VALID_SIGNATURE)),
                text_block_signed("", Some(VALID_SIGNATURE)),
                text_block("visible"),
            ],
            false,
        )],
        ..Context::default()
    };
    let (contents, _) = convert_google_contents(&model, &context);
    let parts = contents[0]["parts"].as_array().unwrap();
    // Upstream `transformMessages`: thinking from another model is dropped
    // outright (signatures are model-bound, and replaying readable thinking
    // makes the next model treat it as part of the reply).
    assert_eq!(
        parts,
        &vec![json!({ "text": "visible" })],
        "cross-model thinking drops; unsigned empty blocks drop"
    );
}

#[test]
fn same_model_signed_empty_blocks_are_kept() {
    let model = google_model("gemini-2.5-flash", GOOGLE_BASE_URL);
    let context = Context {
        messages: vec![assistant_message(
            vec![
                thinking_block("", Some(VALID_SIGNATURE)),
                ContentBlock::ToolCall(ToolCall {
                    id: "call_1".into(),
                    name: "bash".into(),
                    arguments: BTreeMap::new(),
                    thought_signature: None,
                    namespace: None,
                    arguments_error: None,
                }),
            ],
            true,
        )],
        ..Context::default()
    };
    let (contents, _) = convert_google_contents(&model, &context);
    let parts = contents[0]["parts"].as_array().unwrap();
    assert_eq!(parts.len(), 2, "signed empty thinking block is echoed back");
    assert_eq!(parts[0]["thought"], true);
    assert_eq!(parts[0]["thoughtSignature"], VALID_SIGNATURE);
    assert!(parts[1].get("functionCall").is_some());
}

#[test]
fn tool_result_images_require_image_input_and_gemini3_routing() {
    // Gemini 3: images ride inside functionResponse.parts.
    let gemini3 = google_model("gemini-3-flash-preview", GOOGLE_BASE_URL);
    let context = Context {
        messages: vec![Message::ToolResult(ToolResultMessage {
            role: ToolResultRole::ToolResult,
            tool_call_id: "call_1".into(),
            tool_name: "screenshot".into(),
            content: vec![ContentBlock::Image(ImageContent {
                data: "aGk=".into(),
                mime_type: "image/png".into(),
            })],
            details: None,
            usage: None,
            added_tool_names: Vec::new(),
            structured_content: None,
            nested_calls: None,
            is_error: false,
            duration_ms: None,
            timestamp: 1,
        })],
        ..Context::default()
    };
    let body = build_google_body(&gemini3, &context, &StreamOptions::default()).unwrap();
    let response = &body["contents"][0]["parts"][0]["functionResponse"];
    assert_eq!(response["response"]["output"], "(see attached image)");
    assert_eq!(response["parts"][0]["inlineData"]["mimeType"], "image/png");
}

#[test]
fn non_reasoning_models_omit_thinking_config_and_empty_tools_are_skipped() {
    let mut model = google_model("gemini-2.5-flash", GOOGLE_BASE_URL);
    model.reasoning = false;
    let context = Context::default();
    let body = build_google_body(&model, &context, &StreamOptions::default()).unwrap();
    assert!(body.get("generationConfig").is_none());
    assert!(body.get("tools").is_none());
    assert!(body.get("toolConfig").is_none());
    assert!(body.get("systemInstruction").is_none());
    assert_eq!(body["contents"], json!([]));
}

#[test]
fn strict_tool_sampling_on_gemini3_emits_validated_mode() {
    let model = google_model("gemini-3-flash-preview", GOOGLE_BASE_URL);
    let context = Context {
        tools: Some(vec![Tool {
            name: "search".into(),
            description: "Search".into(),
            parameters: json!({
                "type": "object",
                "properties": {},
                "$schema": "http://json-schema.org/draft-07/schema#",
            }),
            constrained_sampling: Some(ConstrainedSamplingConfig::JsonSchema {
                strict: StrictPreference::Require,
            }),
        }]),
        ..Context::default()
    };
    let body = build_google_body(&model, &context, &StreamOptions::default()).unwrap();
    assert_eq!(
        body["toolConfig"]["functionCallingConfig"]["mode"],
        "VALIDATED"
    );
    let schema = &body["tools"][0]["functionDeclarations"][0]["parametersJsonSchema"];
    assert!(schema.is_object());
    assert_eq!(
        schema["$schema"],
        json!("http://json-schema.org/draft-07/schema#"),
        "meta declarations are forwarded verbatim (upstream convertTools)"
    );
}

#[test]
fn strict_tool_schema_forwards_meta_and_rewrites_const_as_enum() {
    // Upstream `convertTools` sends the strict-transformed schema with its
    // meta declarations intact (`parametersJsonSchema` is full JSON Schema),
    // then rewrites `const` to a single-value `enum` because Gemini ignores
    // `const`.
    let model = google_model("gemini-3-flash-preview", GOOGLE_BASE_URL);
    let context = Context {
        tools: Some(vec![Tool {
            name: "choose".into(),
            description: "Choose".into(),
            parameters: json!({
                "type": "object",
                "$schema": "http://json-schema.org/draft-07/schema#",
                "$comment": "meta stays",
                "properties": {"mode": {"type": "string", "const": "fast"}},
            }),
            constrained_sampling: Some(ConstrainedSamplingConfig::JsonSchema {
                strict: StrictPreference::Require,
            }),
        }]),
        ..Context::default()
    };
    let body = build_google_body(&model, &context, &StreamOptions::default()).unwrap();
    assert_eq!(
        body["toolConfig"]["functionCallingConfig"]["mode"],
        "VALIDATED"
    );
    let schema = &body["tools"][0]["functionDeclarations"][0]["parametersJsonSchema"];
    assert_eq!(
        schema["$schema"],
        json!("http://json-schema.org/draft-07/schema#")
    );
    assert_eq!(schema["$comment"], json!("meta stays"));
    assert_eq!(schema["additionalProperties"], json!(false));
    assert_eq!(
        schema["properties"]["mode"],
        json!({"anyOf": [{"type": "string", "enum": ["fast"]}, {"type": "null"}]})
    );
}

#[test]
fn non_strict_tool_schema_rewrites_const_as_enum_recursively() {
    // Upstream `rewriteConstAsEnum`: Gemini ignores `const` in
    // `parametersJsonSchema`, so string/number constants become single-value
    // enums. Name-map contexts (`properties`, `$defs`, ...) keep their keys
    // from being read as schema keywords, and literal data (`default`,
    // `enum`) is never rewritten.
    let model = google_model("gemini-2.5-flash", GOOGLE_BASE_URL);
    let tool = Tool {
        name: "choose".into(),
        description: "Choose".into(),
        parameters: json!({
            "type": "object",
            "$schema": "http://json-schema.org/draft-07/schema#",
            "properties": {
                "kind": {"const": "file"},
                "count": {"const": 3},
                "flag": {"const": true},
                "nested": {
                    "type": "object",
                    "properties": {"mode": {"type": "string", "const": "fast"}},
                },
                "list": {
                    "type": "array",
                    "items": {"type": "object", "properties": {"name": {"const": "x"}}},
                },
                "union": {
                    "anyOf": [{"type": "string", "const": "a"}, {"type": "string", "const": "b"}],
                },
                "one": {"oneOf": [{"type": "string", "const": "c"}, {"const": 4}]},
                "both": {"type": "string", "const": "kept", "enum": ["kept", "other"]},
                "enum": {"type": "string", "const": "property-named-enum"},
                "literal": {"type": "string", "default": {"const": "not-a-schema"}},
            },
            "$defs": {
                "Named": {"type": "string", "const": "def"},
            },
        }),
        constrained_sampling: None,
    };
    let (tools, mode) = pir::ai::api::google::shared::convert_google_tools(&model, &[tool], None)
        .expect("convert")
        .expect("tools present");
    assert!(mode.is_none());
    let schema = &tools[0]["functionDeclarations"][0]["parametersJsonSchema"];

    assert_eq!(
        schema["$schema"],
        json!("http://json-schema.org/draft-07/schema#")
    );
    assert_eq!(schema["properties"]["kind"], json!({"enum": ["file"]}));
    assert_eq!(schema["properties"]["count"], json!({"enum": [3]}));
    // Booleans are not rewritten (upstream only handles string/number).
    assert_eq!(schema["properties"]["flag"], json!({"const": true}));
    assert_eq!(
        schema["properties"]["nested"]["properties"]["mode"],
        json!({"type": "string", "enum": ["fast"]})
    );
    assert_eq!(
        schema["properties"]["list"]["items"]["properties"]["name"],
        json!({"enum": ["x"]})
    );
    assert_eq!(
        schema["properties"]["union"]["anyOf"][0],
        json!({"type": "string", "enum": ["a"]})
    );
    assert_eq!(
        schema["properties"]["union"]["anyOf"][1],
        json!({"type": "string", "enum": ["b"]})
    );
    assert_eq!(
        schema["properties"]["one"]["oneOf"][0],
        json!({"type": "string", "enum": ["c"]})
    );
    assert_eq!(
        schema["properties"]["one"]["oneOf"][1],
        json!({"enum": [4]})
    );
    // An existing `enum` wins: `const` is left untouched (upstream
    // `result.enum === undefined` guard).
    assert_eq!(
        schema["properties"]["both"],
        json!({"type": "string", "const": "kept", "enum": ["kept", "other"]})
    );
    // A property literally named `enum` is still a schema value: name-map
    // context prevents its key from being read as a schema keyword.
    assert_eq!(
        schema["properties"]["enum"],
        json!({"type": "string", "enum": ["property-named-enum"]})
    );
    // Literal data under `default` is not a schema and is not rewritten.
    assert_eq!(
        schema["properties"]["literal"]["default"],
        json!({"const": "not-a-schema"})
    );
    assert_eq!(
        schema["$defs"]["Named"],
        json!({"type": "string", "enum": ["def"]})
    );
}

// ---------------------------------------------------------------------------
// Thinking level / budget mapping (upstream google-thinking-level-map tests)
// ---------------------------------------------------------------------------

#[test]
fn thinking_budgets_follow_upstream_per_model_tables() {
    let levels = [
        (ThinkingLevel::Minimal, 0usize),
        (ThinkingLevel::Low, 1),
        (ThinkingLevel::Medium, 2),
        (ThinkingLevel::High, 3),
    ];
    for (level, slot) in levels {
        assert_eq!(
            google_thinking_budget("gemini-2.5-pro", resolve(level)),
            [128, 2048, 8192, 32768][slot]
        );
        assert_eq!(
            google_thinking_budget("gemini-2.5-flash", resolve(level)),
            [128, 2048, 8192, 24576][slot]
        );
        assert_eq!(
            google_thinking_budget("gemini-2.5-flash-lite", resolve(level)),
            [512, 2048, 8192, 24576][slot]
        );
        assert_eq!(
            google_thinking_budget("gemini-2.0-flash", resolve(level)),
            -1,
            "dynamic"
        );
    }
}

fn resolve(level: ThinkingLevel) -> pir::ai::api::google::ResolvedGoogleLevel {
    let model = google_model("gemini-2.5-flash", GOOGLE_BASE_URL);
    resolve_google_thinking_level(&model, level).unwrap()
}

#[test]
fn thinking_config_disables_and_enables_like_upstream() {
    let base = "https://example.invalid/v1beta";
    // Disabled (reasoning omitted or off): budget 0.
    let flash = google_model("gemini-2.5-flash", base);
    let body = build_google_body(&flash, &Context::default(), &StreamOptions::default()).unwrap();
    assert_eq!(
        body["generationConfig"]["thinkingConfig"],
        json!({ "thinkingBudget": 0 })
    );
    let body = build_google_body(
        &flash,
        &Context::default(),
        &StreamOptions {
            reasoning: Some(ThinkingLevel::Off),
            ..StreamOptions::default()
        },
    )
    .unwrap();
    assert_eq!(
        body["generationConfig"]["thinkingConfig"],
        json!({ "thinkingBudget": 0 })
    );

    // Budget-based enable.
    let body = build_google_body(
        &flash,
        &Context::default(),
        &StreamOptions {
            reasoning: Some(ThinkingLevel::High),
            ..StreamOptions::default()
        },
    )
    .unwrap();
    assert_eq!(
        body["generationConfig"]["thinkingConfig"],
        json!({ "includeThoughts": true, "thinkingBudget": 24576 })
    );

    // Discrete thinkingLevel for Gemini 3.
    let gemini3 = google_model("gemini-3-flash-preview", base);
    let body = build_google_body(
        &gemini3,
        &Context::default(),
        &StreamOptions {
            reasoning: Some(ThinkingLevel::High),
            ..StreamOptions::default()
        },
    )
    .unwrap();
    assert_eq!(
        body["generationConfig"]["thinkingConfig"],
        json!({ "includeThoughts": true, "thinkingLevel": "HIGH" })
    );
}

#[test]
fn xhigh_and_max_clamp_like_upstream() {
    let base = "https://example.invalid/v1beta";
    // Without a map entry: xhigh clamps down to high.
    let flash = google_model("gemini-2.5-flash", base);
    assert_eq!(
        clamp_google_thinking_level(&flash, ThinkingLevel::XHigh),
        ThinkingLevel::High
    );
    let body = build_google_body(
        &flash,
        &Context::default(),
        &StreamOptions {
            reasoning: Some(ThinkingLevel::XHigh),
            ..StreamOptions::default()
        },
    )
    .unwrap();
    assert_eq!(
        body["generationConfig"]["thinkingConfig"]["thinkingBudget"],
        24576
    );

    // With a map entry: xhigh resolves through it.
    let mapped = with_level_map(
        &google_model("gemini-3.7-flash", base),
        &[("xhigh", Some("high")), ("max", Some("high"))],
    );
    let body = build_google_body(
        &mapped,
        &Context::default(),
        &StreamOptions {
            reasoning: Some(ThinkingLevel::Max),
            ..StreamOptions::default()
        },
    )
    .unwrap();
    assert_eq!(
        body["generationConfig"]["thinkingConfig"],
        json!({ "includeThoughts": true, "thinkingLevel": "HIGH" })
    );
}

#[test]
fn lowest_supported_level_used_when_reasoning_omitted_regression() {
    // Regression parity for upstream google-thinking-level-map: with
    // off/minimal mapped to null, omitting reasoning picks the lowest
    // supported level ("LOW") without includeThoughts.
    let base = "https://example.invalid/v1beta";
    let model = with_level_map(
        &google_model("gemini-3.8-flash", base),
        &[
            ("off", None),
            ("minimal", None),
            ("low", Some("low")),
            ("medium", Some("medium")),
            ("high", Some("high")),
            ("xhigh", None),
            ("max", None),
        ],
    );
    let body = build_google_body(&model, &Context::default(), &StreamOptions::default()).unwrap();
    assert_eq!(
        body["generationConfig"]["thinkingConfig"],
        json!({ "thinkingLevel": "LOW" })
    );
}

#[test]
fn uppercase_and_custom_level_map_values_resolve() {
    let base = "https://example.invalid/v1beta";
    let model = with_level_map(
        &google_model("gemini-3.7-flash", base),
        &[("high", Some("LOW"))],
    );
    let body = build_google_body(
        &model,
        &Context::default(),
        &StreamOptions {
            reasoning: Some(ThinkingLevel::High),
            ..StreamOptions::default()
        },
    )
    .unwrap();
    assert_eq!(
        body["generationConfig"]["thinkingConfig"]["thinkingLevel"],
        "LOW"
    );
}

#[test]
fn invalid_level_mapping_surfaces_upstream_error_message() {
    let base = "https://example.invalid/v1beta";
    let model = with_level_map(
        &google_model("gemini-3.7-flash", base),
        &[("xhigh", Some("extreme"))],
    );
    let error = resolve_google_thinking_level(&model, ThinkingLevel::XHigh).unwrap_err();
    assert!(
        error
            .user_message()
            .contains("Unsupported Google thinking level mapping for google/gemini-3.7-flash: xhigh -> extreme"),
        "got: {}",
        error.user_message()
    );
    let unmapped = google_model("gemini-3.7-flash", base);
    let error = resolve_google_thinking_level(&unmapped, ThinkingLevel::Max).unwrap_err();
    assert!(
        error
            .user_message()
            .contains("Unsupported Google thinking level mapping for google/gemini-3.7-flash: max -> undefined"),
        "got: {}",
        error.user_message()
    );
}

#[test]
fn thinking_level_model_probes_match_upstream() {
    for id in [
        "gemini-3-pro-preview",
        "gemini-3-flash-preview",
        "gemini-3.1-pro-preview",
        "gemini-3.8-flash",
        "gemini-flash-latest",
        "gemini-flash-lite-latest",
        "gemma-4-27b",
        "gemma4-27b",
    ] {
        assert!(
            uses_google_thinking_level(id),
            "{id} should use thinkingLevel"
        );
    }
    for id in [
        "gemini-2.5-flash",
        "gemini-2.5-pro",
        "gpt-oss-120b",
        "claude-sonnet-4",
    ] {
        assert!(
            !uses_google_thinking_level(id),
            "{id} should use thinkingBudget"
        );
    }
}

#[test]
fn requires_tool_call_id_matches_upstream() {
    assert!(requires_tool_call_id("gemini-3-flash-preview"));
    assert!(requires_tool_call_id("claude-sonnet-4"));
    assert!(requires_tool_call_id("gpt-oss-120b"));
    assert!(!requires_tool_call_id("gemini-2.5-flash"));
    assert!(!requires_tool_call_id("llama-4"));
}

#[test]
fn finish_reasons_map_like_upstream() {
    assert_eq!(map_stop_reason("STOP"), StopReason::Stop);
    assert_eq!(map_stop_reason("MAX_TOKENS"), StopReason::Length);
    for raw in [
        "SAFETY",
        "RECITATION",
        "BLOCKLIST",
        "PROHIBITED_CONTENT",
        "SPII",
        "LANGUAGE",
        "MALFORMED_FUNCTION_CALL",
        "FINISH_REASON_UNSPECIFIED",
        "OTHER",
    ] {
        assert_eq!(map_stop_reason(raw), StopReason::Error, "{raw}");
    }
}

// ---------------------------------------------------------------------------
// SSE stream parsing
// ---------------------------------------------------------------------------

/// Modeled on upstream Gemini SSE shapes: thought parts, text parts, a
/// function call with a thought signature, finish reason, and usage metadata.
const GOOGLE_FIXTURE: &str = r#"data: {"responseId":"resp-1","candidates":[{"content":{"role":"model","parts":[{"thought":true,"text":"Think"}]}}],"usageMetadata":{"promptTokenCount":10,"candidatesTokenCount":0,"thoughtsTokenCount":1,"totalTokenCount":11}}

data: {"responseId":"resp-1","candidates":[{"content":{"role":"model","parts":[{"thought":true,"text":"ing hard","thoughtSignature":"AAAAAAAAAAAAAAAAAAAAAA=="}]}}]}

data: {"responseId":"resp-1","candidates":[{"content":{"role":"model","parts":[{"text":"Hello "}]}}]}

data: {"responseId":"resp-1","candidates":[{"content":{"role":"model","parts":[{"text":"world","thoughtSignature":"BBBBBBBBBBBBBBBBBBBBAA=="}]}}]}

data: {"responseId":"resp-1","candidates":[{"content":{"role":"model","parts":[{"functionCall":{"name":"get_weather","args":{"city":"Paris"}},"thoughtSignature":"CCCCCCCCCCCCCCCCCCCCAA=="}]}}]}

data: {"responseId":"resp-1","candidates":[{"content":{"role":"model","parts":[{"text":""}]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":12,"candidatesTokenCount":9,"thoughtsTokenCount":3,"cachedContentTokenCount":2,"totalTokenCount":24}}

"#;

fn strip_timestamps(value: &mut Value) {
    match value {
        Value::Object(map) => {
            map.remove("timestamp");
            for (_, child) in map.iter_mut() {
                strip_timestamps(child);
            }
        }
        Value::Array(items) => {
            for child in items {
                strip_timestamps(child);
            }
        }
        Value::String(text) => {
            // Normalize wall-clock-generated tool call ids
            // (`${name}_${Date.now()}_${counter}`) so batch vs incremental
            // parses compare equal.
            let segments: Vec<&str> = text.split('_').collect();
            let is_numeric = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
            if segments.len() >= 3
                && is_numeric(segments[segments.len() - 1])
                && is_numeric(segments[segments.len() - 2])
            {
                *text = "GENERATED".to_string();
            }
        }
        _ => {}
    }
}

fn event_key(event: &AssistantMessageEvent) -> String {
    let mut value = serde_json::to_value(event).unwrap();
    strip_timestamps(&mut value);
    serde_json::to_string(&value).unwrap()
}

fn assert_same_events(batch: &[AssistantMessageEvent], incremental: &[AssistantMessageEvent]) {
    assert_eq!(batch.len(), incremental.len(), "event count mismatch");
    for (index, (batch_event, stream_event)) in batch.iter().zip(incremental.iter()).enumerate() {
        assert_eq!(
            event_key(batch_event),
            event_key(stream_event),
            "event {index} mismatch"
        );
    }
}

fn random_splits(body: &[u8], seed: u64, max_step: usize) -> Vec<Vec<u8>> {
    let mut state = seed | 1;
    let mut chunks = Vec::new();
    let mut start = 0;
    while start < body.len() {
        state = state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        let step = 1 + ((state >> 33) as usize) % max_step;
        let end = (start + step).min(body.len());
        chunks.push(body[start..end].to_vec());
        start = end;
    }
    chunks
}

fn feed_chunks(chunks: &[Vec<u8>], model: &Model) -> Vec<AssistantMessageEvent> {
    let mut parser = pir::ai::api::google::GoogleSseParser::new(model);
    let mut out = vec![AssistantMessageEvent::Start {
        partial: parser.snapshot(),
    }];
    for chunk in chunks {
        parser.feed(chunk, &mut out).unwrap();
    }
    parser.finish(&mut out).unwrap();
    out
}

#[test]
fn google_sse_fixture_maps_thoughts_text_tools_usage() {
    let model = google_model("gemini-2.5-flash", GOOGLE_BASE_URL);
    let events = parse_google_sse(GOOGLE_FIXTURE.as_bytes(), &model).unwrap();

    // Stream protocol: start, thinking start/delta/end, text start/deltas/end,
    // tool call start/delta/end, done.
    let kinds: Vec<&str> = events
        .iter()
        .map(|event| match event {
            AssistantMessageEvent::Start { .. } => "start",
            AssistantMessageEvent::ThinkingStart { .. } => "thinking_start",
            AssistantMessageEvent::ThinkingDelta { .. } => "thinking_delta",
            AssistantMessageEvent::ThinkingEnd { .. } => "thinking_end",
            AssistantMessageEvent::TextStart { .. } => "text_start",
            AssistantMessageEvent::TextDelta { .. } => "text_delta",
            AssistantMessageEvent::TextEnd { .. } => "text_end",
            AssistantMessageEvent::ToolCallStart { .. } => "toolcall_start",
            AssistantMessageEvent::ToolCallDelta { .. } => "toolcall_delta",
            AssistantMessageEvent::ToolCallEnd { .. } => "toolcall_end",
            AssistantMessageEvent::Done { .. } => "done",
            AssistantMessageEvent::Error { .. } => "error",
        })
        .collect();
    assert_eq!(
        kinds,
        vec![
            "start",
            "thinking_start",
            "thinking_delta",
            "thinking_delta",
            "thinking_end",
            "text_start",
            "text_delta",
            "text_delta",
            "text_end",
            "toolcall_start",
            "toolcall_delta",
            "toolcall_end",
            // The final `{"text": ""}` part still opens a text block
            // (upstream: `part.text !== undefined`).
            "text_start",
            "text_delta",
            "text_end",
            "done",
        ]
    );

    let Some(AssistantMessageEvent::Done { message, .. }) = events.last() else {
        panic!("stream must end with Done");
    };
    assert_eq!(
        message.stop_reason,
        StopReason::ToolUse,
        "STOP with a tool call becomes toolUse"
    );
    assert_eq!(message.raw_stop_reason.as_deref(), Some("STOP"));
    assert_eq!(message.response_id.as_deref(), Some("resp-1"));
    assert_eq!(message.content.len(), 4);
    match &message.content[0] {
        ContentBlock::Thinking(thinking) => {
            assert_eq!(thinking.thinking, "Thinking hard");
            assert_eq!(
                thinking.thinking_signature.as_deref(),
                Some(VALID_SIGNATURE)
            );
        }
        other => panic!("expected thinking block, got {other:?}"),
    }
    match &message.content[1] {
        ContentBlock::Text(text) => {
            assert_eq!(text.text, "Hello world");
            assert_eq!(
                text.text_signature.as_deref(),
                Some("BBBBBBBBBBBBBBBBBBBBAA==")
            );
        }
        other => panic!("expected text block, got {other:?}"),
    }
    match &message.content[2] {
        ContentBlock::ToolCall(call) => {
            assert_eq!(call.name, "get_weather");
            assert_eq!(
                call.arguments.get("city").and_then(Value::as_str),
                Some("Paris")
            );
            assert_eq!(
                call.thought_signature.as_deref(),
                Some("CCCCCCCCCCCCCCCCCCCCAA==")
            );
            assert!(
                !call.id.is_empty(),
                "missing function call id must be generated"
            );
        }
        other => panic!("expected tool call, got {other:?}"),
    }
    match &message.content[3] {
        ContentBlock::Text(text) => {
            assert_eq!(text.text, "", "empty-text part yields an empty block")
        }
        other => panic!("expected trailing empty text block, got {other:?}"),
    }
    // usage: input = prompt - cached, output = candidates + thoughts.
    assert_eq!(message.usage.input, 10);
    assert_eq!(message.usage.output, 12);
    assert_eq!(message.usage.cache_read, 2);
    assert_eq!(message.usage.reasoning, Some(3));
    assert_eq!(message.usage.total_tokens, 24);
}

#[test]
fn google_sse_incremental_matches_batch() {
    let model = google_model("gemini-2.5-flash", GOOGLE_BASE_URL);
    let body = GOOGLE_FIXTURE.as_bytes();
    let batch = parse_google_sse(body, &model).unwrap();
    for seed in 1..=5u64 {
        let chunks = random_splits(body, seed, 7);
        assert_same_events(&batch, &feed_chunks(&chunks, &model));
    }
    let chunks = body.iter().map(|byte| vec![*byte]).collect::<Vec<_>>();
    assert_same_events(&batch, &feed_chunks(&chunks, &model));
}

#[test]
fn google_sse_handles_crlf_and_missing_trailing_blank_line() {
    let model = google_model("gemini-2.5-flash", GOOGLE_BASE_URL);
    let crlf = GOOGLE_FIXTURE.replace('\n', "\r\n");
    let batch_crlf = parse_google_sse(crlf.as_bytes(), &model).unwrap();
    let batch = parse_google_sse(GOOGLE_FIXTURE.as_bytes(), &model).unwrap();
    assert_same_events(&batch, &batch_crlf);

    let trimmed = GOOGLE_FIXTURE.trim_end_matches('\n');
    let batch_trimmed = parse_google_sse(trimmed.as_bytes(), &model).unwrap();
    assert_same_events(&batch, &batch_trimmed);
}

#[test]
fn google_sse_max_tokens_with_tool_call_stays_length() {
    let model = google_model("gemini-2.5-flash", GOOGLE_BASE_URL);
    let body = concat!(
        r#"data: {"candidates":[{"content":{"parts":[{"functionCall":{"name":"echo","args":{}}}]},"finishReason":"MAX_TOKENS"}],"usageMetadata":{"promptTokenCount":1,"candidatesTokenCount":1,"totalTokenCount":2}}"#,
        "\n\n"
    );
    let events = parse_google_sse(body.as_bytes(), &model).unwrap();
    let Some(AssistantMessageEvent::Done { message, .. }) = events.last() else {
        panic!("stream must end with Done");
    };
    assert_eq!(
        message.stop_reason,
        StopReason::Length,
        "MAX_TOKENS keeps length even with tool call"
    );
    assert_eq!(message.raw_stop_reason.as_deref(), Some("MAX_TOKENS"));
}

#[test]
fn google_sse_error_finish_reasons_terminate_as_stream_error() {
    let model = google_model("gemini-2.5-flash", GOOGLE_BASE_URL);
    let body = concat!(
        r#"data: {"candidates":[{"finishReason":"MALFORMED_FUNCTION_CALL"}]}"#,
        "\n\n"
    );
    let error = parse_google_sse(body.as_bytes(), &model).unwrap_err();
    assert!(
        error
            .user_message()
            .contains("Provider stopped with: MALFORMED_FUNCTION_CALL"),
        "got: {}",
        error.user_message()
    );

    let body = r#"data: {"candidates":[{"finishReason":"SAFETY"}]}"#;
    let error = parse_google_sse(body.as_bytes(), &model).unwrap_err();
    assert!(
        error
            .user_message()
            .contains("Provider stopped with: SAFETY"),
        "got: {}",
        error.user_message()
    );
}

#[test]
fn google_sse_without_finish_reason_fails_like_upstream() {
    let model = google_model("gemini-2.5-flash", GOOGLE_BASE_URL);
    let body = r#"data: {"candidates":[{"content":{"parts":[{"text":"partial"}]}}]}"#;
    let error = parse_google_sse(body.as_bytes(), &model).unwrap_err();
    assert!(
        error
            .user_message()
            .contains("Google stream ended without a finish reason"),
        "got: {}",
        error.user_message()
    );
}

#[test]
fn google_sse_error_payload_maps_to_provider_error() {
    let model = google_model("gemini-2.5-flash", GOOGLE_BASE_URL);
    let body = r#"data: {"error":{"code":429,"message":"Resource exhausted","status":"RESOURCE_EXHAUSTED"}}"#;
    let error = parse_google_sse(body.as_bytes(), &model).unwrap_err();
    assert!(
        error
            .user_message()
            .contains("Google API error 429 (RESOURCE_EXHAUSTED): Resource exhausted"),
        "got: {}",
        error.user_message()
    );
}

#[test]
fn google_sse_prompt_feedback_block_maps_to_provider_error() {
    let model = google_model("gemini-2.5-flash", GOOGLE_BASE_URL);
    let body = r#"data: {"promptFeedback":{"blockReason":"SAFETY"}}"#;
    let error = parse_google_sse(body.as_bytes(), &model).unwrap_err();
    assert!(
        error.user_message().contains("Prompt blocked: SAFETY"),
        "got: {}",
        error.user_message()
    );
}

#[test]
fn google_sse_malformed_data_line_fails_parse() {
    let model = google_model("gemini-2.5-flash", GOOGLE_BASE_URL);
    let body = "data: {not json}\n\n";
    assert!(parse_google_sse(body.as_bytes(), &model).is_err());
}

#[test]
fn google_sse_null_and_sparse_chunks_do_not_break_stream() {
    let model = google_model("gemini-2.5-flash", GOOGLE_BASE_URL);
    let body = concat!(
        "data: {}\n\n",
        "data: {\"candidates\":[{}]}\n\n",
        "data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":null}]}}]}\n\n",
        "data: {\"responseId\":\"\"}\n\n",
        "data: {\"usageMetadata\":null}\n\n",
        "data: {\"candidates\":[{\"finishReason\":\"STOP\"}],\"usageMetadata\":{\"promptTokenCount\":3,\"totalTokenCount\":3}}\n\n",
    );
    let events = parse_google_sse(body.as_bytes(), &model).unwrap();
    let Some(AssistantMessageEvent::Done { message, .. }) = events.last() else {
        panic!("stream must end with Done");
    };
    assert!(message.content.is_empty());
    assert_eq!(message.stop_reason, StopReason::Stop);
    assert_eq!(message.usage.input, 3);
    assert_eq!(message.usage.total_tokens, 3);
    assert_eq!(
        message.response_id, None,
        "empty responseId is not captured"
    );
}

#[test]
fn google_sse_keeps_first_non_empty_response_id() {
    let model = google_model("gemini-2.5-flash", GOOGLE_BASE_URL);
    let body = concat!(
        "data: {\"responseId\":\"first\"}\n\n",
        "data: {\"responseId\":\"second\"}\n\n",
        "data: {\"candidates\":[{\"finishReason\":\"STOP\"}]}\n\n",
    );
    let events = parse_google_sse(body.as_bytes(), &model).unwrap();
    let Some(AssistantMessageEvent::Done { message, .. }) = events.last() else {
        panic!("stream must end with Done");
    };
    assert_eq!(message.response_id.as_deref(), Some("first"));
}

// ---------------------------------------------------------------------------
// End-to-end provider contract (local mock server, no live network)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct MockRequest {
    method: String,
    path: String,
    headers: BTreeMap<String, String>,
    body: Vec<u8>,
}

struct MockServer {
    base_url: String,
    requests: Arc<Mutex<Vec<MockRequest>>>,
    _handle: tokio::task::JoinHandle<()>,
}

impl MockServer {
    async fn start(response_body: String) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let requests: Arc<Mutex<Vec<MockRequest>>> = Arc::new(Mutex::new(Vec::new()));
        let recorded = requests.clone();
        let handle = tokio::spawn(async move {
            let (mut socket, _) = match listener.accept().await {
                Ok(accepted) => accepted,
                Err(_) => return,
            };
            let mut buf = Vec::new();
            let mut tmp = [0u8; 8192];
            loop {
                match socket.read(&mut tmp).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => buf.extend_from_slice(&tmp[..n]),
                }
                if buf.windows(4).any(|window| window == b"\r\n\r\n") {
                    // Headers complete; try to read the content-length body.
                    let head = String::from_utf8_lossy(&buf).into_owned();
                    let content_length = head
                        .lines()
                        .find_map(|line| {
                            let (key, value) = line.split_once(':')?;
                            key.trim()
                                .eq_ignore_ascii_case("content-length")
                                .then(|| value.trim().parse::<usize>().ok())?
                        })
                        .unwrap_or(0);
                    let header_end = buf.windows(4).position(|w| w == b"\r\n\r\n").unwrap() + 4;
                    while buf.len().saturating_sub(header_end) < content_length {
                        match socket.read(&mut tmp).await {
                            Ok(0) | Err(_) => break,
                            Ok(n) => buf.extend_from_slice(&tmp[..n]),
                        }
                    }
                    break;
                }
            }
            let head = String::from_utf8_lossy(&buf).into_owned();
            let mut lines = head.lines();
            let mut parts = lines.next().unwrap_or_default().split_whitespace();
            let method = parts.next().unwrap_or_default().to_string();
            let path = parts.next().unwrap_or_default().to_string();
            let mut headers = BTreeMap::new();
            for line in lines {
                if let Some((key, value)) = line.split_once(':') {
                    headers.insert(key.trim().to_ascii_lowercase(), value.trim().to_string());
                }
            }
            recorded
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .push(MockRequest {
                    method,
                    path,
                    headers,
                    body: buf[buf
                        .windows(4)
                        .position(|w| w == b"\r\n\r\n")
                        .map(|p| p + 4)
                        .unwrap_or(buf.len())
                        .min(buf.len())..]
                        .to_vec(),
                });
            let head = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                response_body.len()
            );
            let _ = socket.write_all(head.as_bytes()).await;
            let _ = socket.write_all(response_body.as_bytes()).await;
            let _ = socket.flush().await;
        });
        Self {
            base_url: format!("http://{addr}"),
            requests,
            _handle: handle,
        }
    }
}

fn fast_client() -> ProviderRequestClient {
    ProviderRequestClient::new(ProviderRequestConfig {
        timeout: Duration::from_secs(10),
        max_response_bytes: 8 * 1024 * 1024,
        max_retries: 0,
        max_retry_delay: Duration::from_millis(20),
        proxy_url: None,
        http_idle_timeout: None,
    })
    .unwrap()
}

#[tokio::test]
async fn google_provider_contract_posts_sse_endpoint_with_api_key() {
    let sse_body = concat!(
        r#"data: {"responseId":"resp-9","candidates":[{"content":{"role":"model","parts":[{"text":"Hi "}]}}]}"#,
        "\n\n",
        r#"data: {"responseId":"resp-9","candidates":[{"content":{"role":"model","parts":[{"text":"there"}]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":5,"candidatesTokenCount":2,"totalTokenCount":7}}"#,
        "\n\n"
    );
    let server = MockServer::start(sse_body.to_string()).await;
    // Base-url override support: point the model at the mock server.
    let provider = GoogleGenerativeAiProvider::new(
        "google",
        server.base_url.clone(),
        vec![google_model("gemini-2.5-flash", &server.base_url)],
    )
    .unwrap()
    .with_client(fast_client());
    assert_eq!(provider.name(), "Google");
    let model = provider.get_models()[0].clone();
    let context = Context {
        system_prompt: Some("Be terse".into()),
        messages: vec![user_message("Say hi")],
        ..Context::default()
    };
    let stream = provider.stream(
        &model,
        context,
        StreamOptions {
            api_key: Some("test-key".into()),
            temperature: Some(0.2),
            max_tokens: Some(64),
            ..StreamOptions::default()
        },
    );
    let message = stream.result().await.unwrap().clone();
    assert_eq!(message.stop_reason, StopReason::Stop);
    assert_eq!(message.response_id.as_deref(), Some("resp-9"));
    match message.content.first() {
        Some(ContentBlock::Text(text)) => assert_eq!(text.text, "Hi there"),
        other => panic!("expected text block, got {other:?}"),
    }
    assert_eq!(message.usage.input, 5);
    assert_eq!(message.usage.output, 2);

    let requests = server
        .requests
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .clone();
    assert_eq!(requests.len(), 1);
    let request = &requests[0];
    assert_eq!(request.method, "POST");
    assert_eq!(
        request.path,
        "/models/gemini-2.5-flash:streamGenerateContent?alt=sse"
    );
    assert_eq!(
        request.headers.get("x-goog-api-key").map(String::as_str),
        Some("test-key")
    );
    assert_eq!(
        request.headers.get("content-type").map(String::as_str),
        Some("application/json")
    );
    let body: Value = serde_json::from_slice(&request.body).unwrap();
    assert_eq!(body["contents"][0]["role"], "user");
    assert_eq!(body["contents"][0]["parts"][0]["text"], "Say hi");
    assert_eq!(body["systemInstruction"]["parts"][0]["text"], "Be terse");
    assert_eq!(body["generationConfig"]["temperature"], 0.2);
    assert_eq!(body["generationConfig"]["maxOutputTokens"], 64);
    // Reasoning omitted → thinking disabled like upstream.
    assert_eq!(
        body["generationConfig"]["thinkingConfig"],
        json!({ "thinkingBudget": 0 })
    );
}

#[tokio::test]
async fn google_provider_surfaces_transport_errors_as_stream_errors() {
    let server = MockServer::start("data: {\"error\":{\"code\":400,\"message\":\"bad request\",\"status\":\"INVALID_ARGUMENT\"}}\n\n".to_string()).await;
    let provider = GoogleGenerativeAiProvider::new(
        "google",
        server.base_url.clone(),
        vec![google_model("gemini-2.5-flash", &server.base_url)],
    )
    .unwrap()
    .with_client(fast_client());
    let model = provider.get_models()[0].clone();
    let stream = provider.stream(
        &model,
        Context::default(),
        StreamOptions {
            api_key: Some("test-key".into()),
            ..StreamOptions::default()
        },
    );
    let message = stream.result().await.unwrap().clone();
    assert_eq!(message.stop_reason, StopReason::Error);
    let error_text = message.error_message.as_deref().unwrap_or_default();
    assert!(
        error_text.contains("Google API error 400"),
        "in-stream error payload should surface, got: {error_text}"
    );
}

/// `StreamOptions.hooks.onResponse` is part of the shared adapter contract
/// (upstream `onResponse`); every other provider adapter fires it once the
/// HTTP response status/headers are known. The Google adapter used to discard
/// the transport's `(status, headers)` result, so a host hook installed for a
/// Gemini request never ran.
type CapturedResponseHeaders = Arc<Mutex<Vec<(u16, BTreeMap<String, String>)>>>;

#[tokio::test]
async fn google_provider_fires_the_on_response_hook() {
    let sse_body = concat!(
        r#"data: {"candidates":[{"content":{"parts":[{"text":"ok"}]},"finishReason":"STOP"}]}"#,
        "\n\n"
    );
    let server = MockServer::start(sse_body.to_string()).await;
    let provider = GoogleGenerativeAiProvider::new(
        "google",
        server.base_url.clone(),
        vec![google_model("gemini-2.5-flash", &server.base_url)],
    )
    .unwrap()
    .with_client(fast_client());
    let model = provider.get_models()[0].clone();

    let captured: CapturedResponseHeaders = Arc::new(Mutex::new(Vec::new()));
    let seen = Arc::clone(&captured);
    let hooks = ProviderStreamHooks {
        on_response: Some(Arc::new(move |status, headers| {
            seen.lock()
                .unwrap_or_else(|poison| poison.into_inner())
                .push((status, headers.clone()));
        })),
        ..ProviderStreamHooks::default()
    };

    let stream = provider.stream(
        &model,
        Context::default(),
        StreamOptions {
            api_key: Some("test-key".into()),
            hooks,
            ..StreamOptions::default()
        },
    );
    let message = stream.result().await.unwrap().clone();
    assert_eq!(message.stop_reason, StopReason::Stop);
    assert_eq!(message.content, {
        vec![ContentBlock::Text(TextContent {
            text: "ok".to_string(),
            text_signature: None,
        })]
    });

    let captured = captured.lock().unwrap_or_else(|poison| poison.into_inner());
    assert_eq!(
        captured.len(),
        1,
        "onResponse fires exactly once per provider request: {captured:?}"
    );
    assert_eq!(captured[0].0, 200);
    assert!(
        captured[0]
            .1
            .keys()
            .any(|name| name.eq_ignore_ascii_case("content-type")),
        "the response headers reach the hook: {:?}",
        captured[0].1
    );
}

#[tokio::test]
async fn google_provider_registry_env_auth_and_dynamic_registration() {
    let sse_body = concat!(
        r#"data: {"candidates":[{"content":{"parts":[{"text":"pong"}]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":1,"candidatesTokenCount":1,"totalTokenCount":2}}"#,
        "\n\n"
    );
    let server = MockServer::start(sse_body.to_string()).await;
    let model = google_model("gemini-2.5-flash", &server.base_url);
    let mut registry = pir::ai::provider::ProviderRegistry::new(
        Arc::new(pir::ai::auth::InMemoryCredentialStore::new()),
        Arc::new(pir::ai::auth::MapAuthContext::new(BTreeMap::from([(
            "GEMINI_API_KEY".to_string(),
            "env-key".to_string(),
        )]))),
    );
    // Dynamic registration through the models.json-style api id wires the
    // Google adapter (construction dispatch).
    let provider = registry
        .register_dynamic_provider(
            "google",
            DynamicProviderConfig {
                base_url: server.base_url.clone(),
                api: GOOGLE_API_ID.to_string(),
                models: vec![model],
                ..DynamicProviderConfig::default()
            },
        )
        .unwrap();
    assert_eq!(provider.id(), "google");
    let auth = provider.api_key_auth();
    assert_eq!(auth.len(), 1);
    assert!(auth[0].env_vars.contains(&"GEMINI_API_KEY".to_string()));
    assert!(auth[0].env_vars.contains(&"GOOGLE_API_KEY".to_string()));

    let model = registry.get_model("google", "gemini-2.5-flash").unwrap();
    let message = registry
        .complete(model, Context::default(), StreamOptions::default())
        .await
        .unwrap()
        .clone();
    assert_eq!(message.stop_reason, StopReason::Stop);
    let requests = server
        .requests
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .clone();
    assert_eq!(
        requests[0]
            .headers
            .get("x-goog-api-key")
            .map(String::as_str),
        Some("env-key"),
        "registry resolves GEMINI_API_KEY from the env context"
    );
}
