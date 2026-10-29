use pir::ai::provider::ProviderRegistry;
use serde_json::json;
use std::collections::BTreeMap;
use std::time::Duration;

#[test]
fn empty_registry_is_usable() {
    let registry = ProviderRegistry::default();
    assert!(registry.providers().is_empty());
    assert!(registry.get_models().is_empty());
}

fn reasoning_model() -> pir::ai::types::Model {
    pir::ai::types::Model {
        id: "test-model".into(),
        name: "Test Model".into(),
        api: "openai-chat".into(),
        provider: "test-family".into(),
        base_url: "https://example.invalid/v1".into(),
        reasoning: true,
        thinking_level_map: Some(
            [
                ("off".to_owned(), None),
                ("low".to_owned(), Some("low".to_owned())),
                ("high".to_owned(), Some("max".to_owned())),
            ]
            .into_iter()
            .collect(),
        ),
        input: vec!["text".into()],
        cost: pir::ai::types::ModelCost {
            rates: pir::ai::types::ModelCostRates {
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

/// OpenAI-style thinking emission: `thinkingLevelMap[level] ?? level`, `off`
/// only via the map's explicit off entry (mirrors upstream openai-completions).
#[test]
fn reasoning_effort_maps_through_thinking_level_map() {
    use pir::ai::api::openai::apply_reasoning_effort;
    use pir::ai::types::ThinkingLevel;

    let model = reasoning_model();
    // Map hit: high -> max
    let mut body = serde_json::json!({});
    apply_reasoning_effort(&mut body, &model, Some(ThinkingLevel::High));
    assert_eq!(body["reasoning_effort"], "max");
    // Map miss degrades to the wire key
    let mut body = serde_json::json!({});
    apply_reasoning_effort(&mut body, &model, Some(ThinkingLevel::Minimal));
    assert_eq!(body["reasoning_effort"], "minimal");
    // off: only sent when the map provides an explicit value
    let mut body = serde_json::json!({});
    apply_reasoning_effort(&mut body, &model, Some(ThinkingLevel::Off));
    assert!(
        body.get("reasoning_effort").is_none(),
        "off entry is None (explicit suppression): must not send"
    );

    // Non-reasoning models never send
    let mut plain = reasoning_model();
    plain.reasoning = false;
    let mut body = serde_json::json!({});
    apply_reasoning_effort(&mut body, &plain, Some(ThinkingLevel::High));
    assert!(body.get("reasoning_effort").is_none());
    apply_reasoning_effort(&mut body, &plain, None);
    assert!(body.get("reasoning_effort").is_none());
}

// ---------------------------------------------------------------------------
// reasoning_details streaming + replay (upstream A1)
// ---------------------------------------------------------------------------

use pir::ai::api::openai::{
    append_openai_reasoning_detail, apply_thinking_budget, build_openai_body,
    clamp_thinking_budget_to_answer_room, convert_messages, default_thinking_budget, detect_compat,
    is_openai_reasoning_detail, openai_request_headers, parse_openai_reasoning_details,
    parse_openai_sse,
};
use pir::ai::types::{
    AssistantMessage, CacheRetention, ContentBlock, Context, Message, ProviderHeaders, StopReason,
    StreamOptions, ThinkingContent, ThinkingTokenBudgetField, ToolCall, ToolResultMessage,
    ToolResultRole, UserMessage, UserRole,
};

fn user_message(text: &str) -> Message {
    Message::User(UserMessage {
        role: UserRole::User,
        content: vec![ContentBlock::Text(pir::ai::types::TextContent {
            text: text.into(),
            text_signature: None,
        })],
        tools_added: None,
        tools_removed: None,
        timestamp: 0,
        sections: None,
    })
}

fn sse_chunk(delta: serde_json::Value) -> String {
    sse_chunk_with_finish(delta, serde_json::Value::Null)
}

fn sse_chunk_with_finish(delta: serde_json::Value, finish_reason: serde_json::Value) -> String {
    format!(
        "data: {}\n\ndata: [DONE]\n\n",
        serde_json::json!({
            "id": "chatcmpl-1",
            "choices": [{ "index": 0, "delta": delta, "finish_reason": finish_reason }]
        })
    )
}

/// Direct merge semantics of `appendOpenAIReasoningDetail`: consecutive
/// text/summary concatenate, type switches and other types stay discrete.
#[test]
fn append_reasoning_detail_merges_consecutive_entries() {
    let mut details: Vec<serde_json::Value> = Vec::new();
    for detail in [
        serde_json::json!({ "type": "reasoning.text", "id": "rs_1", "index": 2, "text": "a" }),
        serde_json::json!({ "type": "reasoning.text", "text": "b" }),
        serde_json::json!({ "type": "reasoning.text", "text": "c", "signature": "s" }),
        serde_json::json!({ "type": "reasoning.summary", "summary": "x" }),
        serde_json::json!({ "type": "reasoning.summary", "summary": "y", "format": "f" }),
        serde_json::json!({ "type": "reasoning.encrypted", "data": "d1" }),
        serde_json::json!({ "type": "reasoning.encrypted", "data": "d2" }),
    ] {
        append_openai_reasoning_detail(&mut details, &detail);
    }
    assert_eq!(details.len(), 4);
    // text: concatenate + signature ||(=) + id/index backfill
    assert_eq!(details[0]["text"], "abc");
    assert_eq!(details[0]["signature"], "s");
    assert_eq!(details[0]["id"], "rs_1");
    assert_eq!(details[0]["index"], 2);
    // summary: concatenate + format backfill (missing id/index -> null)
    assert_eq!(details[1]["summary"], "xy");
    assert_eq!(details[1]["format"], "f");
    // encrypted: same type still does not merge
    assert_eq!(details[2]["data"], "d1");
    assert_eq!(details[3]["data"], "d2");
}

/// OpenRouter-style `reasoning_details` deltas merge into logical entries:
/// consecutive text deltas concatenate, encrypted stays discrete, invalid
/// entries are skipped, and common fields are backfilled.
#[test]
fn reasoning_details_streaming_merges_deltas() {
    let model = reasoning_model();
    let body = format!(
        "{}{}{}{}{}{}",
        sse_chunk(serde_json::json!({
            "reasoning_details": [
                { "type": "reasoning.text", "id": "rs_1", "format": "openai", "index": 0, "text": "abc" }
            ]
        })),
        sse_chunk(serde_json::json!({
            "reasoning_details": [
                { "type": "reasoning.text", "text": "def", "signature": "sig9" },
                { "type": "reasoning.encrypted", "data": "enc-payload" }
            ]
        })),
        sse_chunk(serde_json::json!({
            "reasoning_details": [
                { "type": "reasoning.text", "text": "ghi" },
                { "type": "reasoning.summary", "summary": "part" }
            ]
        })),
        sse_chunk(serde_json::json!({
            "reasoning_details": [
                { "type": "reasoning.summary", "summary": "digest" }
            ]
        })),
        sse_chunk(serde_json::json!({
            "reasoning_details": [
                // invalid entries: missing payload fields / unknown type
                { "type": "reasoning.summary" },
                { "type": "reasoning.encrypted" },
                { "type": "reasoning.text" },
                { "type": "reasoning.other", "data": "x" },
                "not-an-object"
            ]
        })),
        sse_chunk_with_finish(
            serde_json::json!({ "content": "answer" }),
            serde_json::json!("stop")
        ),
    );
    let events = parse_openai_sse(body.as_bytes(), &model).unwrap();

    let done = events
        .iter()
        .find_map(|event| match event {
            pir::ai::types::AssistantMessageEvent::Done { message, .. } => Some(message),
            _ => None,
        })
        .unwrap();
    // 6 valid entries (text abc / text def / encrypted / text ghi / summary
    // part / summary digest) merge into 4 logical entries
    let thinking = done
        .content
        .iter()
        .find_map(|block| match block {
            ContentBlock::Thinking(t) => Some(t),
            _ => None,
        })
        .expect("thinking block present");
    let signature = thinking
        .thinking_signature
        .as_deref()
        .expect("signature set");
    let details: Vec<serde_json::Value> = serde_json::from_str(signature).unwrap();
    // 6 valid entries (text abc / text def / encrypted / text ghi / summary
    // part / summary digest) merge into 4 logical entries
    assert_eq!(details.len(), 4);

    // text: consecutive same-type concatenation + signature backfill +
    // common-field backfill
    assert_eq!(details[0]["type"], "reasoning.text");
    assert_eq!(details[0]["text"], "abcdef");
    assert_eq!(details[0]["signature"], "sig9");
    assert_eq!(details[0]["id"], "rs_1");
    assert_eq!(details[0]["format"], "openai");
    assert_eq!(details[0]["index"], 0);
    // encrypted: opaque, discrete
    assert_eq!(details[1]["type"], "reasoning.encrypted");
    assert_eq!(details[1]["data"], "enc-payload");
    // Second text segment (separated by encrypted, standalone entry)
    assert_eq!(details[2]["type"], "reasoning.text");
    assert_eq!(details[2]["text"], "ghi");
    // summary: consecutive same-type concatenation
    assert_eq!(details[3]["type"], "reasoning.summary");
    assert_eq!(details[3]["summary"], "partdigest");

    // reasoning_details never produce user-visible thinking text deltas
    let raw_reasoning_chunks = events.iter().filter(|event| {
        matches!(
            event,
            pir::ai::types::AssistantMessageEvent::ThinkingDelta { delta, .. } if !delta.is_empty()
        )
    });
    assert_eq!(raw_reasoning_chunks.count(), 0);
}

#[test]
fn reasoning_detail_validation_matches_upstream() {
    assert!(is_openai_reasoning_detail(&serde_json::json!({
        "type": "reasoning.summary", "summary": "s"
    })));
    assert!(is_openai_reasoning_detail(&serde_json::json!({
        "type": "reasoning.encrypted", "data": "d"
    })));
    assert!(is_openai_reasoning_detail(&serde_json::json!({
        "type": "reasoning.text", "text": "t", "signature": null
    })));
    // encrypted no longer requires an id
    assert!(is_openai_reasoning_detail(&serde_json::json!({
        "type": "reasoning.encrypted", "id": null, "data": "d"
    })));
    assert!(!is_openai_reasoning_detail(&serde_json::json!({
        "type": "reasoning.text", "text": "t", "signature": 3
    })));
    assert!(!is_openai_reasoning_detail(&serde_json::json!({
        "type": "reasoning.text", "text": 1
    })));
    assert!(!is_openai_reasoning_detail(&serde_json::json!({
        "type": "reasoning.summary", "summary": 1
    })));
    assert!(!is_openai_reasoning_detail(&serde_json::json!({
        "type": "reasoning.encrypted", "data": null
    })));
    assert!(!is_openai_reasoning_detail(&serde_json::json!({
        "type": "unknown", "data": "d"
    })));
    assert!(!is_openai_reasoning_detail(&serde_json::json!("scalar")));
    assert!(!is_openai_reasoning_detail(&serde_json::json!(["array"])));
    assert!(!is_openai_reasoning_detail(&serde_json::json!({
        "type": "reasoning.text", "text": "t", "id": 5
    })));

    // Array replay parsing: non-empty and all entries valid
    let array = serde_json::json!([
        { "type": "reasoning.text", "id": "rs_1", "text": "abc" }
    ]);
    let parsed = parse_openai_reasoning_details(Some(&array.to_string())).unwrap();
    assert_eq!(parsed[0]["text"], "abc");
    // Invalid / empty / non-array signatures all mean no preserved details
    assert!(parse_openai_reasoning_details(Some("[]")).is_none());
    assert!(parse_openai_reasoning_details(Some("{\"type\":\"reasoning.text\"}")).is_none());
    assert!(parse_openai_reasoning_details(Some("not json")).is_none());
    assert!(parse_openai_reasoning_details(None).is_none());
}

fn assistant_with_thinking(thinking: &str, signature: Option<&str>) -> Message {
    Message::Assistant(AssistantMessage {
        // Same provider/api/model as `reasoning_model()` so transform_messages
        // preserves the thinking blocks (cross-model history is lowered to
        // text by design).
        api: "openai-chat".into(),
        provider: "test-family".into(),
        model: "test-model".into(),
        content: vec![
            ContentBlock::Thinking(ThinkingContent {
                thinking: thinking.into(),
                thinking_signature: signature.map(str::to_string),
                redacted: false,
            }),
            ContentBlock::Text(pir::ai::types::TextContent {
                text: "answer".into(),
                text_signature: None,
            }),
        ],
        ..AssistantMessage::default()
    })
}

/// Replay: a thinking block whose signature parses as `OpenAIReasoningDetail[]`
/// is sent verbatim as `reasoning_details` and the raw reasoning text field is
/// suppressed; otherwise the raw field (whitelisted names only) carries the text.
#[test]
fn reasoning_details_replay_suppresses_raw_reasoning_field() {
    let details = serde_json::json!([
        { "type": "reasoning.encrypted", "id": "rs_1", "data": "enc" },
        { "type": "reasoning.text", "id": "rs_1", "text": "thought" }
    ]);

    // preserved details -> verbatim `reasoning_details`, no raw reasoning field
    let context = Context {
        system_prompt: None,
        messages: vec![assistant_with_thinking(
            "thought",
            Some(&details.to_string()),
        )],
        tools: None,
    };
    let items = convert_messages(&reasoning_model(), &context, &OpenAiCompat::default());
    let assistant = &items[0];
    assert_eq!(assistant["role"], "assistant");
    assert_eq!(assistant["reasoning_details"], details);
    for field in ["reasoning", "reasoning_content", "reasoning_text"] {
        assert!(
            assistant.get(field).is_none(),
            "raw field {field} must be suppressed"
        );
    }

    // No preserved details, signature hits the whitelist -> the raw reasoning
    // field replays the thinking text
    let context = Context {
        system_prompt: None,
        messages: vec![assistant_with_thinking(
            "raw thought",
            Some("reasoning_content"),
        )],
        tools: None,
    };
    let items = convert_messages(&reasoning_model(), &context, &OpenAiCompat::default());
    assert_eq!(items[0]["reasoning_content"], "raw thought");
    assert!(items[0].get("reasoning_details").is_none());

    // Signature outside the whitelist (e.g. a base64 signature passed through
    // from Anthropic) -> raw field is not sent
    let context = Context {
        system_prompt: None,
        messages: vec![assistant_with_thinking(
            "raw thought",
            Some("aws-claude-xxx"),
        )],
        tools: None,
    };
    let items = convert_messages(&reasoning_model(), &context, &OpenAiCompat::default());
    for field in [
        "reasoning",
        "reasoning_content",
        "reasoning_text",
        "reasoning_details",
    ] {
        assert!(items[0].get(field).is_none(), "{field} must not be sent");
    }
}

// ---------------------------------------------------------------------------
// Thinking token budget (upstream A2)
// ---------------------------------------------------------------------------

#[test]
fn thinking_budget_defaults_and_clamping() {
    use pir::ai::types::ThinkingLevel;
    // DEFAULT_THINKING_BUDGETS; xhigh/max clamp to high
    assert_eq!(default_thinking_budget(ThinkingLevel::Minimal), 1024);
    assert_eq!(default_thinking_budget(ThinkingLevel::Low), 2048);
    assert_eq!(default_thinking_budget(ThinkingLevel::Medium), 8192);
    assert_eq!(default_thinking_budget(ThinkingLevel::High), 16384);
    assert_eq!(default_thinking_budget(ThinkingLevel::XHigh), 16384);
    assert_eq!(default_thinking_budget(ThinkingLevel::Max), 16384);
    assert_eq!(default_thinking_budget(ThinkingLevel::Off), 0);

    // min(budget, max(0, ceiling - 1024))
    assert_eq!(clamp_thinking_budget_to_answer_room(16384, 4096), 3072);
    assert_eq!(clamp_thinking_budget_to_answer_room(2048, 100_000), 2048);
    assert_eq!(clamp_thinking_budget_to_answer_room(16384, 1024), 0);
    assert_eq!(clamp_thinking_budget_to_answer_room(16384, 0), 0);
    assert_eq!(clamp_thinking_budget_to_answer_room(16384, 512), 0);
}

#[test]
fn thinking_budget_emission_follows_model_knob_and_ceiling() {
    use pir::ai::types::ThinkingLevel;

    let mut model = reasoning_model();
    model.thinking_token_budget_field = Some(ThinkingTokenBudgetField::ThinkingBudget);

    // Ceiling comes from max_tokens; xhigh maps to the high budget and is
    // clamped to the remaining room
    let mut body = serde_json::json!({ "max_tokens": 4096 });
    apply_thinking_budget(&mut body, &model, Some(ThinkingLevel::XHigh), None);
    assert_eq!(body["thinking_budget"], 3072);

    // Without max_tokens, fall back to max_completion_tokens, then
    // model.max_tokens
    let mut body = serde_json::json!({ "max_completion_tokens": 3072 });
    apply_thinking_budget(&mut body, &model, Some(ThinkingLevel::High), None);
    assert_eq!(body["thinking_budget"], 2048);

    // With neither, fall back to model.max_tokens = 2000 ->
    // min(16384, 2000-1024) = 976
    let mut body = serde_json::json!({});
    apply_thinking_budget(&mut body, &model, Some(ThinkingLevel::High), None);
    assert_eq!(body["thinking_budget"], 976);

    // Clamped to 0 (ceiling <= 1024): not sent
    let mut body = serde_json::json!({ "max_tokens": 1024 });
    apply_thinking_budget(&mut body, &model, Some(ThinkingLevel::High), None);
    assert!(body.get("thinking_budget").is_none());

    // off / None / non-reasoning model / missing field knob: not sent
    let mut body = serde_json::json!({ "max_tokens": 4096 });
    apply_thinking_budget(&mut body, &model, Some(ThinkingLevel::Off), None);
    assert!(body.get("thinking_budget").is_none());
    apply_thinking_budget(&mut body, &model, None, None);
    assert!(body.get("thinking_budget").is_none());
    let mut plain = reasoning_model();
    plain.reasoning = false;
    plain.thinking_token_budget_field = Some(ThinkingTokenBudgetField::ThinkingTokenBudget);
    let mut body = serde_json::json!({ "max_tokens": 4096 });
    apply_thinking_budget(&mut body, &plain, Some(ThinkingLevel::High), None);
    assert!(body.get("thinking_token_budget").is_none());
    let no_knob = reasoning_model();
    let mut body = serde_json::json!({ "max_tokens": 4096 });
    apply_thinking_budget(&mut body, &no_knob, Some(ThinkingLevel::High), None);
    assert!(body.get("thinking_token_budget").is_none());

    // The three field names follow the knob
    let mut model = reasoning_model();
    model.thinking_token_budget_field = Some(ThinkingTokenBudgetField::ThinkingBudgetTokens);
    let mut body = serde_json::json!({ "max_tokens": 4096 });
    apply_thinking_budget(&mut body, &model, Some(ThinkingLevel::Medium), None);
    assert_eq!(body["thinking_budget_tokens"], 3072);
}

// ---------------------------------------------------------------------------
// Compat-gated thinking emission (upstream openai-completions.ts 873-975)
// ---------------------------------------------------------------------------

use pir::ai::api::openai::apply_reasoning_effort as apply_reasoning_effort_compat;
use pir::ai::providers::{
    merge_pi_config, parse_pi_config, DynamicProviderConfig, RegisterDynamicProvider,
};
use pir::ai::types::{OpenAiCompat, ThinkingFormat, ThinkingLevel};

fn model_with_format(format: ThinkingFormat) -> pir::ai::types::Model {
    let mut model = reasoning_model();
    model.compat = Some(OpenAiCompat {
        thinking_format: format,
        ..OpenAiCompat::default()
    });
    model
}

/// Same, but without a thinking-level map (so `off` has no explicit entry and
/// the per-format fallback applies).
fn model_with_format_no_map(format: ThinkingFormat) -> pir::ai::types::Model {
    let mut model = model_with_format(format);
    model.thinking_level_map = None;
    model
}

/// `supportsReasoningEffort: false` suppresses `reasoning_effort` entirely
/// (openai format); other formats still emit their own markers.
#[test]
fn compat_supports_reasoning_effort_gates_openai_emission() {
    let mut model = model_with_format(ThinkingFormat::Openai);
    model.compat.as_mut().unwrap().supports_reasoning_effort = false;
    let mut body = serde_json::json!({});
    apply_reasoning_effort_compat(&mut body, &model, Some(ThinkingLevel::High));
    assert!(body.get("reasoning_effort").is_none());
    // Default compat keeps the openai behavior.
    let mut body = serde_json::json!({});
    apply_reasoning_effort_compat(&mut body, &reasoning_model(), Some(ThinkingLevel::High));
    assert_eq!(body["reasoning_effort"], "max");
}

/// OpenRouter: nested `reasoning: { effort }`; `off` falls back to the map's
/// off entry, then "none"; an explicit null off entry suppresses emission.
#[test]
fn compat_openrouter_sends_nested_reasoning_object() {
    let model = model_with_format(ThinkingFormat::Openrouter);
    let mut body = serde_json::json!({});
    apply_reasoning_effort_compat(&mut body, &model, Some(ThinkingLevel::High));
    assert_eq!(body["reasoning"]["effort"], "max");

    // off without a map: "none"
    let no_map = model_with_format_no_map(ThinkingFormat::Openrouter);
    let mut body = serde_json::json!({});
    apply_reasoning_effort_compat(&mut body, &no_map, Some(ThinkingLevel::Off));
    assert_eq!(body["reasoning"]["effort"], "none");

    // off with an explicit map value
    let mut mapped = model_with_format(ThinkingFormat::Openrouter);
    mapped.thinking_level_map = Some(
        [("off".to_owned(), Some("low".to_owned()))]
            .into_iter()
            .collect(),
    );
    let mut body = serde_json::json!({});
    apply_reasoning_effort_compat(&mut body, &mapped, Some(ThinkingLevel::Off));
    assert_eq!(body["reasoning"]["effort"], "low");

    // off with an explicit null map entry: nothing is sent
    let mut nulled = model_with_format(ThinkingFormat::Openrouter);
    nulled.thinking_level_map = Some([("off".to_owned(), None)].into_iter().collect());
    let mut body = serde_json::json!({});
    apply_reasoning_effort_compat(&mut body, &nulled, Some(ThinkingLevel::Off));
    assert!(body.get("reasoning").is_none());
}

/// DeepSeek: `thinking: { type }` marker always, `reasoning_effort` only when
/// supported and a level is requested.
#[test]
fn compat_deepseek_sends_thinking_type_marker() {
    let model = model_with_format(ThinkingFormat::Deepseek);
    let mut body = serde_json::json!({});
    apply_reasoning_effort_compat(&mut body, &model, Some(ThinkingLevel::High));
    assert_eq!(body["thinking"]["type"], "enabled");
    assert_eq!(body["reasoning_effort"], "max");

    // supportsReasoningEffort: false -> marker without reasoning_effort
    let mut no_effort = model_with_format(ThinkingFormat::Deepseek);
    no_effort.compat.as_mut().unwrap().supports_reasoning_effort = false;
    let mut body = serde_json::json!({});
    apply_reasoning_effort_compat(&mut body, &no_effort, Some(ThinkingLevel::High));
    assert_eq!(body["thinking"]["type"], "enabled");
    assert!(body.get("reasoning_effort").is_none());

    // off without a map -> thinking disabled
    let no_map = model_with_format_no_map(ThinkingFormat::Deepseek);
    let mut body = serde_json::json!({});
    apply_reasoning_effort_compat(&mut body, &no_map, Some(ThinkingLevel::Off));
    assert_eq!(body["thinking"]["type"], "disabled");
    assert!(body.get("reasoning_effort").is_none());

    // off with an explicit null map entry: no marker at all
    let mut body = serde_json::json!({});
    apply_reasoning_effort_compat(&mut body, &model, Some(ThinkingLevel::Off));
    assert!(body.get("thinking").is_none());
}

/// The remaining formats' wire shapes: zai / qwen / qwen-chat-template /
/// together / string-thinking / ant-ling.
#[test]
fn compat_other_thinking_formats_wire_shapes() {
    // zai: thinking { type, clear_thinking } + reasoning_effort
    let model = model_with_format(ThinkingFormat::Zai);
    let mut body = serde_json::json!({});
    apply_reasoning_effort_compat(&mut body, &model, Some(ThinkingLevel::Low));
    assert_eq!(body["thinking"]["type"], "enabled");
    assert_eq!(body["thinking"]["clear_thinking"], false);
    assert_eq!(body["reasoning_effort"], "low");
    let mut body = serde_json::json!({});
    apply_reasoning_effort_compat(&mut body, &model, Some(ThinkingLevel::Off));
    assert_eq!(body["thinking"]["type"], "disabled");

    // qwen: enable_thinking + reasoning_effort
    let model = model_with_format(ThinkingFormat::Qwen);
    let mut body = serde_json::json!({});
    apply_reasoning_effort_compat(&mut body, &model, Some(ThinkingLevel::Low));
    assert_eq!(body["enable_thinking"], true);
    assert_eq!(body["reasoning_effort"], "low");
    let mut body = serde_json::json!({});
    apply_reasoning_effort_compat(&mut body, &model, Some(ThinkingLevel::Off));
    assert_eq!(body["enable_thinking"], false);
    assert!(body.get("reasoning_effort").is_none());

    // qwen-chat-template: chat_template_kwargs
    let model = model_with_format(ThinkingFormat::QwenChatTemplate);
    let mut body = serde_json::json!({});
    apply_reasoning_effort_compat(&mut body, &model, Some(ThinkingLevel::Low));
    assert_eq!(body["chat_template_kwargs"]["enable_thinking"], true);
    assert_eq!(body["chat_template_kwargs"]["preserve_thinking"], true);

    // together: reasoning { enabled } + reasoning_effort
    let model = model_with_format(ThinkingFormat::Together);
    let mut body = serde_json::json!({});
    apply_reasoning_effort_compat(&mut body, &model, Some(ThinkingLevel::Low));
    assert_eq!(body["reasoning"]["enabled"], true);
    assert_eq!(body["reasoning_effort"], "low");
    let mut body = serde_json::json!({});
    apply_reasoning_effort_compat(&mut body, &model, Some(ThinkingLevel::Off));
    assert_eq!(body["reasoning"]["enabled"], false);

    // string-thinking: top-level thinking string
    let model = model_with_format(ThinkingFormat::StringThinking);
    let mut body = serde_json::json!({});
    apply_reasoning_effort_compat(&mut body, &model, Some(ThinkingLevel::High));
    assert_eq!(body["thinking"], "max");
    let no_map = model_with_format_no_map(ThinkingFormat::StringThinking);
    let mut body = serde_json::json!({});
    apply_reasoning_effort_compat(&mut body, &no_map, Some(ThinkingLevel::Off));
    assert_eq!(body["thinking"], "none");

    // ant-ling: only an explicit map entry emits
    let no_map = model_with_format_no_map(ThinkingFormat::AntLing);
    let mut body = serde_json::json!({});
    apply_reasoning_effort_compat(&mut body, &no_map, Some(ThinkingLevel::High));
    assert!(body.get("reasoning").is_none(), "no map entry, no emission");
    let mut mapped = model_with_format(ThinkingFormat::AntLing);
    mapped.thinking_level_map = Some(
        [("high".to_owned(), Some("max".to_owned()))]
            .into_iter()
            .collect(),
    );
    let mut body = serde_json::json!({});
    apply_reasoning_effort_compat(&mut body, &mapped, Some(ThinkingLevel::High));
    assert_eq!(body["reasoning"]["effort"], "max");
}

// ---------------------------------------------------------------------------
// pi models.json: full upstream schema, JSONC + BOM, compat, overrides
// ---------------------------------------------------------------------------

#[test]
fn pi_models_json_full_schema_with_compat_and_overrides() {
    let contents = "\u{feff}// pi models config (JSONC)\n{
        \"providers\": {
            \"openai\": {
                \"baseUrl\": \"https://proxy.example/v1\",
                \"compat\": { \"thinkingFormat\": \"openrouter\" },
                \"modelOverrides\": {
                    \"proxy-model\": { \"name\": \"Proxy via override\", \"contextWindow\": 64000 }
                },
                \"models\": [
                    {
                        \"id\": \"proxy-model\",
                        \"reasoning\": true,
                        \"thinkingLevelMap\": { \"off\": null, \"high\": \"max\" },
                        \"contextWindow\": 32000,
                        \"maxTokens\": 4096,
                        \"compat\": { \"supportsReasoningEffort\": false }
                    }
                ],
            },
        },
    }";
    let config = parse_pi_config(contents).expect("JSONC + BOM tolerated");
    let merged = merge_pi_config(&config).unwrap();

    // models.json entries are fully populated, the provider baseUrl applies,
    // and modelOverrides patch the entry by id.
    let proxy = merged.iter().find(|m| m.id == "proxy-model").unwrap();
    assert_eq!(proxy.provider, "openai");
    assert_eq!(proxy.name, "Proxy via override");
    assert!(proxy.reasoning);
    assert_eq!(proxy.context_window, 64_000);
    assert_eq!(proxy.max_tokens, 4_096);
    assert_eq!(proxy.base_url, "https://proxy.example/v1");

    // Provider compat merges under the model-level flags.
    let compat = proxy.compat.as_ref().unwrap();
    assert_eq!(compat.thinking_format, ThinkingFormat::Openrouter);
    assert!(!compat.supports_reasoning_effort);
    let map = proxy.thinking_level_map.as_ref().unwrap();
    assert_eq!(map.get("off"), Some(&None));
    assert_eq!(map.get("high"), Some(&Some("max".to_owned())));
}

#[test]
fn pi_models_json_provider_scoped_to_unknown_provider_creates_it() {
    let contents = r#"{
        "providers": {
            "acme": {
                "name": "Acme",
                "baseUrl": "https://acme.example/v1",
                "models": [{ "id": "acme-large", "contextWindow": 8000 }]
            }
        }
    }"#;
    let config = parse_pi_config(contents).unwrap();
    let merged = merge_pi_config(&config).unwrap();
    let model = merged.iter().find(|m| m.id == "acme-large").unwrap();
    assert_eq!(model.provider, "acme");
    assert_eq!(model.base_url, "https://acme.example/v1");
    assert_eq!(model.api, "openai-chat");
    assert_eq!(model.name, "acme-large");
}

// ---------------------------------------------------------------------------
// Dynamic provider registration (extension-facing registerProvider)
// ---------------------------------------------------------------------------

fn dyn_model(id: &str) -> pir::ai::types::Model {
    let mut model = reasoning_model();
    model.id = id.to_string();
    model.name = id.to_string();
    model.base_url = String::new();
    model.compat = None;
    model
}

#[test]
fn register_dynamic_provider_normalizes_and_registers() {
    let mut registry = ProviderRegistry::default();
    let config = DynamicProviderConfig {
        base_url: "https://acme.example/v1".into(),
        api_key: Some("sk-acme".into()),
        api: "openai-chat".into(),
        models: vec![dyn_model("acme-large"), dyn_model("acme-mini")],
        compat: Some(OpenAiCompat {
            thinking_format: ThinkingFormat::Together,
            ..OpenAiCompat::default()
        }),
        refresh: None,
    };
    let provider = registry
        .register_dynamic_provider("acme", config)
        .expect("dynamic provider registers");
    assert_eq!(provider.id(), "acme");
    let models = registry.get_provider_models("acme");
    assert_eq!(models.len(), 2);
    for model in &models {
        assert_eq!(model.provider, "acme");
        assert_eq!(model.base_url, "https://acme.example/v1");
        assert_eq!(model.api, "openai-chat");
        assert_eq!(
            model.compat.as_ref().unwrap().thinking_format,
            ThinkingFormat::Together
        );
    }
    // An empty model list is rejected.
    let empty = DynamicProviderConfig {
        base_url: "https://empty.example/v1".into(),
        models: vec![],
        ..DynamicProviderConfig::default()
    };
    assert!(registry.register_dynamic_provider("empty", empty).is_err());
}

#[test]
fn register_dynamic_provider_refresh_replaces_model_list() {
    use std::sync::Arc;
    let mut registry = ProviderRegistry::default();
    let config = DynamicProviderConfig {
        base_url: "https://acme.example/v1".into(),
        api: "openai-chat".into(),
        models: vec![dyn_model("acme-large"), dyn_model("acme-mini")],
        refresh: Some(Arc::new(|| Ok(vec![dyn_model("acme-large-v2")]))),
        ..DynamicProviderConfig::default()
    };
    registry
        .register_dynamic_provider("acme", config)
        .expect("dynamic provider registers");
    let models = registry.get_provider_models("acme");
    assert_eq!(models.len(), 1);
    assert_eq!(models[0].id, "acme-large-v2");
    assert_eq!(models[0].base_url, "https://acme.example/v1");

    // A failing or empty refresh keeps the configured models.
    let mut registry = ProviderRegistry::default();
    let config = DynamicProviderConfig {
        base_url: "https://acme.example/v1".into(),
        api: "openai-chat".into(),
        models: vec![dyn_model("acme-large")],
        refresh: Some(Arc::new(|| Ok(Vec::new()))),
        ..DynamicProviderConfig::default()
    };
    registry
        .register_dynamic_provider("acme", config)
        .expect("dynamic provider registers");
    let models = registry.get_provider_models("acme");
    assert_eq!(models.len(), 1);
    assert_eq!(models[0].id, "acme-large");
}

// ---------------------------------------------------------------------------
// Parity batch 2 (openai-completions.ts HEAD): prompt caching, cache_control,
// session affinity, detectCompat, tools:[], request options, routing knobs.
// ---------------------------------------------------------------------------

fn parity_model(provider: &str, base_url: &str) -> pir::ai::types::Model {
    let mut model = reasoning_model();
    model.provider = provider.into();
    model.base_url = base_url.into();
    model
}

/// `prompt_cache_key` is sent for standard OpenAI whenever caching is on, and
/// for other providers only with long retention; `prompt_cache_retention`
/// rides only with long retention (upstream buildParams:826-831).
#[test]
fn openai_prompt_cache_key_and_retention_gating() {
    let model = parity_model("openai", "https://api.openai.com/v1");
    let options = StreamOptions {
        session_id: Some("sess-1".into()),
        ..StreamOptions::default()
    };
    let body = build_openai_body(&model, &Context::default(), &options, &Default::default())
        .expect("body");
    assert_eq!(body["prompt_cache_key"], json!("sess-1"));
    assert!(body.get("prompt_cache_retention").is_none());

    // Non-OpenAI base URL: short retention sends nothing.
    let model = parity_model("acme", "https://acme.example/v1");
    let body = build_openai_body(&model, &Context::default(), &options, &Default::default())
        .expect("body");
    assert!(body.get("prompt_cache_key").is_none());

    // Long retention with supportsLongCacheRetention (default true): both.
    let options = StreamOptions {
        session_id: Some("sess-1".into()),
        cache_retention: Some(CacheRetention::Long),
        ..StreamOptions::default()
    };
    let body = build_openai_body(&model, &Context::default(), &options, &Default::default())
        .expect("body");
    assert_eq!(body["prompt_cache_key"], json!("sess-1"));
    assert_eq!(body["prompt_cache_retention"], json!("24h"));

    // Providers without long-retention support send neither.
    let compat = OpenAiCompat {
        supports_long_cache_retention: Some(false),
        ..OpenAiCompat::default()
    };
    let body = build_openai_body(&model, &Context::default(), &options, &compat).expect("body");
    assert!(body.get("prompt_cache_key").is_none());
    assert!(body.get("prompt_cache_retention").is_none());

    // Retention "none" disables the cache key even on standard OpenAI.
    let model = parity_model("openai", "https://api.openai.com/v1");
    let options = StreamOptions {
        session_id: Some("sess-1".into()),
        cache_retention: Some(CacheRetention::None),
        ..StreamOptions::default()
    };
    let body = build_openai_body(&model, &Context::default(), &options, &Default::default())
        .expect("body");
    assert!(body.get("prompt_cache_key").is_none());

    // Session keys clamp to the 64-char OpenAI limit.
    let long_key = "k".repeat(100);
    let options = StreamOptions {
        session_id: Some(long_key.clone()),
        ..StreamOptions::default()
    };
    let body = build_openai_body(&model, &Context::default(), &options, &Default::default())
        .expect("body");
    assert_eq!(body["prompt_cache_key"].as_str().unwrap().len(), 64);
}

/// No tools + no tool history -> `tools` omitted; tool history without
/// declared tools -> `"tools": []` (upstream openai-completions.ts:855-863).
#[test]
fn openai_tools_two_branch_behavior() {
    let model = parity_model("openai", "https://example.invalid/v1");

    // No history: omitted entirely.
    let context = Context {
        messages: vec![user_message("hi")],
        ..Context::default()
    };
    let body = build_openai_body(
        &model,
        &context,
        &StreamOptions::default(),
        &OpenAiCompat::default(),
    )
    .expect("body");
    assert!(body.get("tools").is_none());

    // Tool history (assistant tool call) without declared tools: [].
    let call = ContentBlock::ToolCall(ToolCall {
        id: "call_1".into(),
        name: "search".into(),
        arguments: BTreeMap::new(),
        thought_signature: None,
        namespace: None,
        arguments_error: None,
    });
    let context = Context {
        messages: vec![
            Message::Assistant(AssistantMessage {
                api: "openai-completions".into(),
                provider: "openai".into(),
                model: "gpt".into(),
                content: vec![call],
                stop_reason: StopReason::ToolUse,
                ..AssistantMessage::default()
            }),
            user_message("go"),
        ],
        ..Context::default()
    };
    let body = build_openai_body(
        &model,
        &context,
        &StreamOptions::default(),
        &OpenAiCompat::default(),
    )
    .expect("body");
    assert_eq!(body["tools"], json!([]));

    // Declared tools convert; zaiToolStream adds tool_stream.
    let tool = pir::ai::types::Tool {
        name: "search".into(),
        description: "Search".into(),
        parameters: serde_json::json!({"type": "object", "properties": {}}),
        constrained_sampling: None,
    };
    let compat = OpenAiCompat {
        zai_tool_stream: Some(true),
        ..OpenAiCompat::default()
    };
    let context = Context {
        tools: Some(vec![tool]),
        ..Context::default()
    };
    let body =
        build_openai_body(&model, &context, &StreamOptions::default(), &compat).expect("body");
    assert_eq!(body["tool_stream"], json!(true));
    assert!(body["tools"].as_array().expect("tools").len() == 1);
}

/// `cacheControlFormat: "anthropic"` injects Anthropic-style markers on the
/// system prompt, the last tool definition, and the last conversation message
/// (upstream applyAnthropicCacheControl, openai-completions.ts:1089-1191).
#[test]
fn openai_anthropic_cache_control_markers() {
    let model = parity_model("openrouter", "https://openrouter.ai/api/v1");
    let compat = OpenAiCompat {
        cache_control_format: Some(pir::ai::types::CacheControlFormat::Anthropic),
        ..OpenAiCompat::default()
    };
    let tool = pir::ai::types::Tool {
        name: "search".into(),
        description: "Search".into(),
        parameters: serde_json::json!({"type": "object", "properties": {}}),
        constrained_sampling: None,
    };
    let context = Context {
        system_prompt: Some("You are helpful.".into()),
        messages: vec![user_message("hi")],
        tools: Some(vec![tool]),
    };
    let body =
        build_openai_body(&model, &context, &StreamOptions::default(), &compat).expect("body");
    let marker = json!({"type": "ephemeral"});
    // System prompt becomes a text-parts array carrying the marker.
    assert_eq!(body["messages"][0]["role"], json!("system"));
    assert_eq!(body["messages"][0]["content"][0]["cache_control"], marker);
    // The last tool definition carries the marker.
    let tools = body["tools"].as_array().expect("tools");
    assert_eq!(tools.last().unwrap()["cache_control"], marker);
    // The last conversation message carries the marker.
    let messages = body["messages"].as_array().expect("messages");
    let last = messages.last().unwrap();
    let content = last["content"].as_array().expect("content parts");
    assert_eq!(content.last().unwrap()["cache_control"], marker);

    // Long retention adds ttl 1h.
    let options = StreamOptions {
        cache_retention: Some(CacheRetention::Long),
        ..StreamOptions::default()
    };
    let body = build_openai_body(&model, &context, &options, &compat).expect("body");
    let ttl_marker = json!({"type": "ephemeral", "ttl": "1h"});
    assert_eq!(
        body["messages"][0]["content"][0]["cache_control"],
        ttl_marker
    );
}

/// Session-affinity headers per `sessionAffinityFormat` (upstream
/// createClient:776-786); caller headers still win.
#[test]
fn openai_session_affinity_header_formats() {
    let model = parity_model("openrouter", "https://openrouter.ai/api/v1");
    let options = StreamOptions {
        session_id: Some("sess-9".into()),
        api_key: Some("key".into()),
        ..StreamOptions::default()
    };

    // OpenRouter (detected via compat knobs, upstream getCompat): x-session-id
    // only. The affinity headers ride the detected compat, not the serde
    // default.
    let headers = openai_request_headers(&model, &options, &detect_compat(&model), "authorization");
    assert_eq!(
        headers.get("x-session-id").map(String::as_str),
        Some("sess-9")
    );
    assert!(!headers.contains_key("x-client-request-id"));

    // openai format: session_id + x-client-request-id + x-session-affinity.
    let compat = OpenAiCompat {
        send_session_affinity_headers: true,
        session_affinity_format: Some(pir::ai::types::SessionAffinityFormat::Openai),
        ..OpenAiCompat::default()
    };
    let headers = openai_request_headers(&model, &options, &compat, "authorization");
    assert_eq!(
        headers.get("session_id").map(String::as_str),
        Some("sess-9")
    );
    assert_eq!(
        headers.get("x-client-request-id").map(String::as_str),
        Some("sess-9")
    );
    assert_eq!(
        headers.get("x-session-affinity").map(String::as_str),
        Some("sess-9")
    );

    // openai-nosession: request-id + affinity, no session_id.
    let compat = OpenAiCompat {
        send_session_affinity_headers: true,
        session_affinity_format: Some(pir::ai::types::SessionAffinityFormat::OpenaiNosession),
        ..OpenAiCompat::default()
    };
    let headers = openai_request_headers(&model, &options, &compat, "authorization");
    assert!(!headers.contains_key("session_id"));
    assert_eq!(
        headers.get("x-client-request-id").map(String::as_str),
        Some("sess-9")
    );

    // No session id: no affinity headers at all.
    let options = StreamOptions::default();
    let compat = OpenAiCompat {
        send_session_affinity_headers: true,
        ..OpenAiCompat::default()
    };
    let headers = openai_request_headers(&model, &options, &compat, "authorization");
    assert!(!headers.contains_key("x-session-id"));

    // Caller headers override the defaults.
    let options = StreamOptions {
        session_id: Some("sess-9".into()),
        headers: Some(ProviderHeaders::from([(
            "x-session-id".to_owned(),
            "custom".to_owned(),
        )])),
        ..StreamOptions::default()
    };
    let headers =
        openai_request_headers(&model, &options, &OpenAiCompat::default(), "authorization");
    assert_eq!(
        headers.get("x-session-id").map(String::as_str),
        Some("custom")
    );
}

/// detectCompat per-provider table (upstream openai-completions.ts:1593-1690);
/// explicit model.compat still wins.
#[test]
fn detect_compat_table_cases() {
    use pir::ai::api::openai::{detect_compat, resolve_compat};
    let case = |provider: &str, base_url: &str, id: &str| {
        let mut model = parity_model(provider, base_url);
        model.id = id.into();
        model
    };

    // z.ai: zai thinking format, max_tokens, no reasoning_effort.
    let detected = detect_compat(&case("zai", "https://api.z.ai/api/paas/v4", "glm"));
    assert_eq!(detected.thinking_format, ThinkingFormat::Zai);
    assert_eq!(detected.max_tokens_field.as_deref(), Some("max_tokens"));
    assert!(!detected.supports_reasoning_effort);
    assert_eq!(detected.supports_store, Some(false));

    // DeepSeek: deepseek thinking format + empty reasoning_content replay.
    let detected = detect_compat(&case("deepseek", "https://api.deepseek.com/v1", "v4"));
    assert_eq!(detected.thinking_format, ThinkingFormat::Deepseek);
    assert_eq!(
        detected.requires_reasoning_content_on_assistant_messages,
        Some(true)
    );

    // OpenRouter anthropic/* models: openrouter format + anthropic cache
    // control + session affinity on.
    let detected = detect_compat(&case(
        "openrouter",
        "https://openrouter.ai/api/v1",
        "anthropic/claude-x",
    ));
    assert_eq!(detected.thinking_format, ThinkingFormat::Openrouter);
    assert_eq!(
        detected.cache_control_format,
        Some(pir::ai::types::CacheControlFormat::Anthropic)
    );
    assert!(detected.send_session_affinity_headers);

    // OpenRouter non-anthropic: no cache_control.
    let detected = detect_compat(&case(
        "openrouter",
        "https://openrouter.ai/api/v1",
        "openai/gpt",
    ));
    assert_eq!(detected.cache_control_format, None);

    // Together / xAI / NVIDIA / ant-ling / moonshot: no reasoning_effort;
    // together keeps the together format.
    for (provider, base_url) in [
        ("together", "https://api.together.xyz/v1"),
        ("xai", "https://api.x.ai/v1"),
        ("nvidia", "https://integrate.api.nvidia.com/v1"),
        ("ant-ling", "https://api.ant-ling.com/v1"),
        ("moonshotai", "https://api.moonshot.ai/v1"),
    ] {
        let detected = detect_compat(&case(provider, base_url, "m"));
        assert!(!detected.supports_reasoning_effort, "{provider}");
    }
    let detected = detect_compat(&case("together", "https://api.together.xyz/v1", "m"));
    assert_eq!(detected.thinking_format, ThinkingFormat::Together);

    // Cloudflare / cerebras / opencode: non-standard (no store).
    for (provider, base_url) in [
        (
            "cloudflare-workers-ai",
            "https://api.cloudflare.com/client/v4",
        ),
        ("cerebras", "https://api.cerebras.ai/v1"),
        ("opencode", "https://opencode.ai/zen/v1"),
    ] {
        let detected = detect_compat(&case(provider, base_url, "m"));
        assert_eq!(detected.supports_store, Some(false), "{provider}");
    }

    // Plain OpenAI-compatible: openai format, store, developer role.
    let detected = detect_compat(&case("acme", "https://acme.example/v1", "m"));
    assert_eq!(detected.thinking_format, ThinkingFormat::Openai);
    assert_eq!(detected.supports_store, Some(true));
    assert_eq!(detected.supports_developer_role, Some(true));
    assert_eq!(
        detected.max_tokens_field.as_deref(),
        Some("max_completion_tokens")
    );

    // Explicit model.compat wins per field (xAI detected false, explicit true).
    let mut model = case("xai", "https://api.x.ai/v1", "grok");
    model.compat = Some(OpenAiCompat {
        supports_reasoning_effort: true,
        thinking_format: ThinkingFormat::Openai,
        ..OpenAiCompat::default()
    });
    let resolved = resolve_compat(&model);
    assert!(resolved.supports_reasoning_effort);
    assert_eq!(resolved.thinking_format, ThinkingFormat::Openai);
    // Detected values not overridden stay detected (xai keeps
    // max_completion_tokens; it is not on upstream's max_tokens list).
    assert_eq!(
        resolved.max_tokens_field.as_deref(),
        Some("max_completion_tokens")
    );
}

/// Per-request `tool_choice` rides the body (upstream buildParams:869-871).
#[test]
fn openai_tool_choice_passthrough() {
    let model = parity_model("openai", "https://example.invalid/v1");
    for (choice, wire) in [
        (pir::ai::types::ToolChoice::Auto, "auto"),
        (pir::ai::types::ToolChoice::None, "none"),
    ] {
        let options = StreamOptions {
            tool_choice: Some(choice),
            ..StreamOptions::default()
        };
        let body = build_openai_body(
            &model,
            &Context::default(),
            &options,
            &OpenAiCompat::default(),
        )
        .expect("body");
        assert_eq!(body["tool_choice"], json!(wire));
    }
}

/// Per-request `thinkingBudgets` override the level defaults (upstream
/// `thinkingBudgetForLevel`).
#[test]
fn openai_thinking_budgets_request_option() {
    let mut model = parity_model("openai", "https://example.invalid/v1");
    model.thinking_token_budget_field = Some(ThinkingTokenBudgetField::ThinkingBudget);
    let options = StreamOptions {
        reasoning: Some(ThinkingLevel::High),
        max_tokens: Some(100_000),
        thinking_budgets: Some(pir::ai::types::ThinkingBudgets {
            high: Some(4_096),
            ..pir::ai::types::ThinkingBudgets::default()
        }),
        ..StreamOptions::default()
    };
    let body = build_openai_body(
        &model,
        &Context::default(),
        &options,
        &OpenAiCompat::default(),
    )
    .expect("body");
    assert_eq!(body["thinking_budget"], 4_096);

    // Without the override the level default applies (high -> 16384).
    let options = StreamOptions {
        reasoning: Some(ThinkingLevel::High),
        max_tokens: Some(100_000),
        ..StreamOptions::default()
    };
    let body = build_openai_body(
        &model,
        &Context::default(),
        &options,
        &OpenAiCompat::default(),
    )
    .expect("body");
    assert_eq!(body["thinking_budget"], 16_384);
}

/// Store / vLLM priority / provider routing knobs ride the body (upstream
/// buildParams:838-841, 873-875, 987-1001).
#[test]
fn openai_store_priority_and_routing_knobs() {
    let mut model = parity_model("openai", "https://example.invalid/v1");
    // supportsStore -> store:false.
    let compat = OpenAiCompat {
        supports_store: Some(true),
        vllm_priority: Some(-1.5),
        open_router_routing: Some(serde_json::json!({"order": ["deepseek"]})),
        vercel_gateway_routing: Some(serde_json::json!({"only": ["bedrock"]})),
        ..OpenAiCompat::default()
    };
    let body = build_openai_body(
        &model,
        &Context::default(),
        &StreamOptions::default(),
        &compat,
    )
    .expect("body");
    assert_eq!(body["store"], json!(false));
    assert_eq!(body["priority"], json!(-1.5));
    // Routing fields come from the RAW model.compat (upstream reads
    // model.compat, not the resolved compat).
    assert!(body.get("provider").is_none());
    assert!(body.get("providerOptions").is_none());
    model.compat = Some(OpenAiCompat {
        open_router_routing: Some(serde_json::json!({"order": ["deepseek"]})),
        vercel_gateway_routing: Some(serde_json::json!({"only": ["bedrock"]})),
        ..OpenAiCompat::default()
    });
    let body = build_openai_body(
        &model,
        &Context::default(),
        &StreamOptions::default(),
        &OpenAiCompat::default(),
    )
    .expect("body");
    assert_eq!(body["provider"], json!({"order": ["deepseek"]}));
    assert_eq!(
        body["providerOptions"]["gateway"],
        json!({"only": ["bedrock"]})
    );
}

/// requires* replay knobs on convert_messages (upstream convertMessages).
#[test]
fn openai_requires_knobs_in_convert_messages() {
    let model = parity_model("acme", "https://example.invalid/v1");
    let compat = OpenAiCompat {
        supports_developer_role: Some(true),
        requires_tool_result_name: Some(true),
        requires_assistant_after_tool_result: Some(true),
        requires_thinking_as_text: Some(true),
        requires_reasoning_content_on_assistant_messages: Some(true),
        ..OpenAiCompat::default()
    };

    // Developer role for reasoning models.
    let context = Context {
        system_prompt: Some("Be terse.".into()),
        messages: vec![user_message("hi")],
        tools: None,
    };
    let messages = convert_messages(&model, &context, &compat);
    assert_eq!(messages[0]["role"], json!("developer"));

    // Tool results carry `name`; a user message after tool results is bridged
    // by the synthetic assistant message.
    let result = ToolResultMessage {
        role: ToolResultRole::ToolResult,
        tool_call_id: "call_1".into(),
        tool_name: "search".into(),
        content: vec![text_block_plain("ok")],
        details: None,
        usage: None,
        added_tool_names: Vec::new(),
        structured_content: None,
        nested_calls: None,
        is_error: false,
        duration_ms: None,
        timestamp: 1,
    };
    let context = Context {
        messages: vec![Message::ToolResult(result), user_message("next")],
        ..Context::default()
    };
    let messages = convert_messages(&model, &context, &compat);
    assert_eq!(messages[0]["name"], json!("search"));
    assert_eq!(
        messages[1]["role"],
        json!("assistant"),
        "synthetic bridge assistant"
    );
    assert_eq!(
        messages[1]["content"],
        json!("I have processed the tool results.")
    );
    assert_eq!(messages[2]["role"], json!("user"));

    // Thinking blocks replay as plain text; assistant messages carry the
    // empty reasoning_content marker. The replayed message is same-model
    // (upstream transformMessages keeps thinking blocks only for the same
    // model; cross-model thinking is lowered to text before convertMessages).
    let context = Context {
        messages: vec![Message::Assistant(AssistantMessage {
            api: "openai-chat".into(),
            provider: "acme".into(),
            model: "test-model".into(),
            content: vec![
                ContentBlock::Thinking(ThinkingContent {
                    thinking: "pondering".into(),
                    thinking_signature: None,
                    redacted: false,
                }),
                ContentBlock::Text(pir::ai::types::TextContent {
                    text: "answer".into(),
                    text_signature: None,
                }),
            ],
            stop_reason: StopReason::Stop,
            ..AssistantMessage::default()
        })],
        ..Context::default()
    };
    let messages = convert_messages(&model, &context, &compat);
    let content = messages[0]["content"].as_array().expect("content");
    assert_eq!(content[0]["type"], json!("text"));
    assert_eq!(content[0]["text"], json!("pondering"));
    assert_eq!(content[1]["text"], json!("answer"));
    assert_eq!(messages[0]["reasoning_content"], json!(""));

    // Without the developer-role knob the system prompt stays "system".
    let compat = OpenAiCompat::default();
    let context = Context {
        system_prompt: Some("Be terse.".into()),
        messages: vec![user_message("hi")],
        tools: None,
    };
    let messages = convert_messages(&model, &context, &compat);
    assert_eq!(messages[0]["role"], json!("system"));
}

fn text_block_plain(text: &str) -> ContentBlock {
    ContentBlock::Text(pir::ai::types::TextContent {
        text: text.into(),
        text_signature: None,
    })
}

/// Grammar-configured tools convert to OpenAI custom tools when the provider
/// supports the format (upstream convertTools:1482-1517); otherwise they stay
/// function tools.
#[test]
fn openai_grammar_tool_conversion() {
    let tool = pir::ai::types::Tool {
        name: "emit".into(),
        description: "Emit".into(),
        parameters: serde_json::json!({
            "type": "object",
            "properties": {"input": {"type": "string"}},
            "required": ["input"]
        }),
        constrained_sampling: Some(pir::ai::types::ConstrainedSamplingConfig::Grammar {
            variants: BTreeMap::from([("openai_lark".to_owned(), "start: \"x\"".to_owned())]),
        }),
    };
    let compat = OpenAiCompat {
        supports_openai_grammar_tools: Some(true),
        ..OpenAiCompat::default()
    };
    let converted = pir::ai::api::openai::convert_tool(&tool, &compat).expect("tool");
    assert_eq!(converted["type"], json!("custom"));
    assert_eq!(converted["custom"]["name"], json!("emit"));
    assert_eq!(
        converted["custom"]["format"]["grammar"]["syntax"],
        json!("lark")
    );

    // Without the knob the tool stays a function tool.
    let compat = OpenAiCompat::default();
    let converted = pir::ai::api::openai::convert_tool(&tool, &compat).expect("tool");
    assert_eq!(converted["type"], json!("function"));
}

/// `service_tier` pricing reconciliation (upstream getServiceTierCostMultiplier
/// + applyServiceTierPricing).
#[test]
fn service_tier_pricing_reconciliation() {
    use pir::ai::api::openai_responses::shared::{
        apply_service_tier_pricing, service_tier_cost_multiplier,
    };
    assert_eq!(service_tier_cost_multiplier("gpt", Some("flex")), 0.5);
    assert_eq!(service_tier_cost_multiplier("gpt", Some("priority")), 2.0);
    assert_eq!(service_tier_cost_multiplier("gpt-5.5", Some("fast")), 2.5);
    assert_eq!(service_tier_cost_multiplier("gpt", Some("default")), 1.0);
    assert_eq!(service_tier_cost_multiplier("gpt", None), 1.0);

    let mut usage = pir::ai::types::Usage {
        input: 10,
        output: 20,
        ..pir::ai::types::Usage::default()
    };
    usage.cost = pir::ai::types::Cost {
        input: 1.0,
        output: 2.0,
        cache_read: 0.5,
        cache_write: 0.5,
        total: 4.0,
        total_from_provider: None,
    };
    apply_service_tier_pricing("gpt", &mut usage, Some("flex"));
    assert_eq!(usage.cost.input, 0.5);
    assert_eq!(usage.cost.total, 2.0);
}

/// `service_tier` rides the `/responses` request body (upstream
/// openai-responses.ts buildParams:348-349).
#[test]
fn openai_responses_service_tier_passthrough() {
    use pir::ai::api::openai_responses::shared::build_responses_params;
    use pir::ai::api::openai_responses::ResponsesCompat;
    let mut model = parity_model("openai", "https://api.openai.com/v1");
    model.reasoning = false;
    let options = StreamOptions {
        service_tier: Some("flex".into()),
        ..StreamOptions::default()
    };
    let body = build_responses_params(
        &model,
        &Context::default(),
        &options,
        &ResponsesCompat::default(),
        &model.id,
        pir::ai::api::openai_responses::shared::OPENAI_TOOL_CALL_PROVIDERS,
        false,
        None,
    )
    .expect("body");
    assert_eq!(body["service_tier"], json!("flex"));

    // ChatGPT sign-in tokens reject the field (omit_unsupported_fields).
    let body = build_responses_params(
        &model,
        &Context::default(),
        &options,
        &ResponsesCompat::default(),
        &model.id,
        pir::ai::api::openai_responses::shared::OPENAI_TOOL_CALL_PROVIDERS,
        true,
        None,
    )
    .expect("body");
    assert!(body.get("service_tier").is_none());
}

/// `error.metadata.raw` is appended to provider error messages when present in
/// the embedded body and not already printed (upstream
/// openai-completions.ts:720-727).
#[test]
fn raw_error_metadata_appended_once() {
    use pir::ai::api::append_raw_error_metadata;
    // pir's transport embeds the raw HTTP error body in the message text, so
    // the raw metadata is already printed inside the embedded JSON body:
    // unchanged (upstream skips the append when the message already contains
    // the raw text, openai-completions.ts:720-727).
    let body =
        r#"HTTP 400: {"error":{"message":"bad","metadata":{"raw":"quota exceeded for org"}}}"#;
    let message = append_raw_error_metadata(body);
    assert_eq!(message, body, "already printed: not appended twice");

    // A raw value whose text is not already printed (JSON-escaped quotes in
    // the embedded body decode to a different literal) is appended once.
    let escaped =
        r#"HTTP 400: {"error":{"message":"bad","metadata":{"raw":"quota \"exceeded\" for org"}}}"#;
    let appended = append_raw_error_metadata(escaped);
    assert!(
        appended.ends_with("quota \"exceeded\" for org"),
        "raw metadata appended: {appended}"
    );
    // Already present after the first append: unchanged.
    let again = append_raw_error_metadata(&appended);
    assert_eq!(appended, again);

    // No metadata: unchanged.
    let plain = append_raw_error_metadata("HTTP 500: oops");
    assert_eq!(plain, "HTTP 500: oops");
}

/// `deferred` is a terminal stop reason with the upstream wire name
/// (upstream types.ts:814).
#[test]
fn deferred_stop_reason_wire_shape() {
    let stop = StopReason::Deferred;
    assert_eq!(serde_json::to_string(&stop).unwrap(), "\"deferred\"");
    let parsed: StopReason = serde_json::from_str("\"deferred\"").unwrap();
    assert_eq!(parsed, StopReason::Deferred);
    let terminal = pir::ai::types::TerminalReason::Deferred;
    assert_eq!(serde_json::to_string(&terminal).unwrap(), "\"deferred\"");
}

/// models.json `authHeader: true` stamps the compat so the registry stream
/// path sends `Authorization: Bearer <apiKey>` (upstream
/// withConfiguredAuth, provider-composer.ts:374-386).
#[test]
fn auth_header_flag_becomes_bearer_authorization() {
    use pir::ai::provider::apply_auth_header;
    let mut model = parity_model("acme", "https://acme.example/v1");
    model.compat = Some(OpenAiCompat {
        auth_header: Some(true),
        ..OpenAiCompat::default()
    });

    // Bearer authorization derived from the resolved key.
    let headers = apply_auth_header(&model, None, None, Some("sk-acme"));
    assert_eq!(
        headers
            .as_ref()
            .and_then(|h| h.get("authorization"))
            .map(String::as_str),
        Some("Bearer sk-acme")
    );

    // A caller-supplied authorization header still wins.
    let caller = ProviderHeaders::from([("authorization".to_owned(), "Bearer caller".to_owned())]);
    let headers = apply_auth_header(&model, Some(caller.clone()), Some(&caller), Some("sk-acme"));
    assert_eq!(
        headers
            .as_ref()
            .and_then(|h| h.get("authorization"))
            .map(String::as_str),
        Some("Bearer caller")
    );

    // Without the flag nothing is injected.
    model.compat = None;
    let headers = apply_auth_header(&model, None, None, Some("sk-acme"));
    assert!(headers.is_none());
}

// ---------------------------------------------------------------------------
// models.json per-model metadata + authHeader stamping (upstream
// model-config.ts / provider-composer.ts; the provider_config_fixes suite was
// merged away, so these live here with the other models.json tests).
// ---------------------------------------------------------------------------

/// Per-model `inputLimits`, `promptCache`, and `samplingParamsByThinkingLevel`
/// from models.json are honored (carried on the model's compat block, where
/// pir keeps the per-model metadata; the sampling-by-level map feeds the
/// existing merge chain).
#[test]
fn models_json_metadata_fields_are_honored() {
    let config = parse_pi_config(
        r#"{
            "providers": {
                "acme": {
                    "baseUrl": "https://acme.example/v1",
                    "apiKey": "$ACME_KEY",
                    "models": [{
                        "id": "acme-large",
                        "reasoning": true,
                        "samplingParamsByThinkingLevel": {
                            "high": { "top_p": 0.9 }
                        },
                        "inputLimits": { "images": { "maxImageSize": 1024 } },
                        "promptCache": { "retention": "short" }
                    }],
                    "modelOverrides": {
                        "acme-large": {
                            "samplingParamsByThinkingLevel": {
                                "high": { "top_k": 40 }
                            }
                        }
                    }
                }
            }
        }"#,
    )
    .expect("parse models.json");
    let models = merge_pi_config(&config).expect("merge");
    let model = models
        .iter()
        .find(|model| model.provider == "acme" && model.id == "acme-large")
        .expect("model present");
    let compat = model.compat.as_ref().expect("compat carried");
    let by_level = compat
        .sampling_params_by_thinking_level
        .as_ref()
        .expect("samplingParamsByThinkingLevel");
    // Entry + override merge per level.
    let high = by_level.get("high").expect("high level params");
    assert_eq!(high.get("top_p"), Some(&serde_json::json!(0.9)));
    assert_eq!(high.get("top_k"), Some(&serde_json::json!(40)));
    assert_eq!(
        compat.input_limits.as_ref().and_then(|v| v.get("images")),
        Some(&serde_json::json!({ "maxImageSize": 1024 }))
    );
    assert_eq!(
        compat
            .prompt_cache
            .as_ref()
            .and_then(|v| v.get("retention")),
        Some(&serde_json::json!("short"))
    );

    // The merge chain consumes the level params.
    let body = build_openai_body(
        model,
        &Context::default(),
        &StreamOptions {
            reasoning: Some(ThinkingLevel::High),
            ..StreamOptions::default()
        },
        &OpenAiCompat::default(),
    )
    .expect("body");
    assert_eq!(body["top_p"], serde_json::json!(0.9));
    assert_eq!(body["top_k"], serde_json::json!(40));
}

/// Provider-level `authHeader: true` stamps every model of the provider so the
/// request path sends `Authorization: Bearer <apiKey>`.
#[test]
fn models_json_auth_header_stamps_models() {
    let config = parse_pi_config(
        r#"{
            "providers": {
                "acme": {
                    "baseUrl": "https://acme.example/v1",
                    "apiKey": "literal-key",
                    "authHeader": true,
                    "models": ["acme-large"]
                }
            }
        }"#,
    )
    .expect("parse models.json");
    let models = merge_pi_config(&config).expect("merge");
    let model = models
        .iter()
        .find(|model| model.provider == "acme")
        .expect("model present");
    assert_eq!(
        model.compat.as_ref().and_then(|compat| compat.auth_header),
        Some(true)
    );
}

/// Provider-level `headers` from models.json apply to EVERY model of the
/// provider — including models that come from the built-in catalog when the
/// config sets no explicit `models` — with the model's own (catalog / entry /
/// override) headers winning per name.
#[test]
fn models_json_provider_headers_apply_to_every_model() {
    let config = parse_pi_config(
        r#"{
            "providers": {
                "anthropic": {
                    "headers": {"X-Provider": "p", "X-Shared": "provider"}
                },
                "github-copilot": {
                    "headers": {"User-Agent": "custom-ua", "X-Provider": "copilot"}
                }
            }
        }"#,
    )
    .expect("parse models.json");
    let models = merge_pi_config(&config).expect("merge");

    let anthropic: Vec<_> = models
        .iter()
        .filter(|model| model.provider == "anthropic")
        .collect();
    assert!(
        !anthropic.is_empty(),
        "the built-in anthropic catalog participates"
    );
    for model in anthropic {
        let headers = model
            .headers
            .as_ref()
            .expect("provider headers attached to a built-in model");
        assert_eq!(headers.get("X-Provider").map(String::as_str), Some("p"));
        assert_eq!(
            headers.get("X-Shared").map(String::as_str),
            Some("provider")
        );
    }

    // The catalog's static headers still win over the provider-level ones.
    let copilot = models
        .iter()
        .find(|model| model.provider == "github-copilot")
        .expect("copilot model");
    let headers = copilot.headers.as_ref().expect("copilot headers");
    assert_eq!(
        headers.get("X-Provider").map(String::as_str),
        Some("copilot")
    );
    assert_eq!(
        headers.get("User-Agent").map(String::as_str),
        Some("GitHubCopilotChat/0.35.0"),
        "model/catalog headers win per name"
    );
}

/// Provider-level `headers` also survive a `modelOverrides` block that carries
/// its own headers: the override header wins per name and the provider header
/// stays for every other name.
#[test]
fn models_json_provider_headers_survive_model_overrides() {
    let config = parse_pi_config(
        r#"{
            "providers": {
                "anthropic": {
                    "headers": {"X-Provider": "p", "X-Keep": "yes"},
                    "modelOverrides": {
                        "claude-haiku-4-5": {
                            "headers": {"X-Provider": "override"}
                        }
                    }
                }
            }
        }"#,
    )
    .expect("parse models.json");
    let models = merge_pi_config(&config).expect("merge");
    let overridden = models
        .iter()
        .find(|model| model.provider == "anthropic" && model.id == "claude-haiku-4-5")
        .expect("overridden model");
    let headers = overridden.headers.as_ref().expect("headers");
    assert_eq!(
        headers.get("X-Provider").map(String::as_str),
        Some("override")
    );
    assert_eq!(headers.get("X-Keep").map(String::as_str), Some("yes"));

    // Every other built-in model keeps the provider value.
    let other = models
        .iter()
        .find(|model| model.provider == "anthropic" && model.id != "claude-haiku-4-5")
        .expect("another anthropic model");
    assert_eq!(
        other
            .headers
            .as_ref()
            .and_then(|headers| headers.get("X-Provider"))
            .map(String::as_str),
        Some("p")
    );
}

// ---------------------------------------------------------------------------
// streamSimple / completeSimple (upstream `Models.streamSimple` /
// `completeSimple`): model-derived thinking defaults and budget preservation
// ---------------------------------------------------------------------------

#[test]
fn simple_stream_options_clamps_the_level_and_preserves_budget_inputs() {
    use pir::ai::provider::simple_stream_options;
    use pir::ai::types::{
        clamp_thinking_level, get_supported_thinking_levels, ThinkingBudgets, ThinkingLevel,
    };

    let mut model = reasoning_model();
    model.thinking_level_map = Some(
        [
            ("off".to_owned(), None),
            ("low".to_owned(), Some("low".to_owned())),
            ("medium".to_owned(), Some("medium".to_owned())),
            ("high".to_owned(), Some("high".to_owned())),
            // Extended levels need an explicit non-null mapping; `max` is
            // explicitly unsupported, so xhigh/max clamp down to high.
            ("max".to_owned(), None),
        ]
        .into_iter()
        .collect(),
    );

    let supported = get_supported_thinking_levels(&model);
    assert!(
        !supported.contains(&ThinkingLevel::Off),
        "off is null in the map"
    );
    assert!(!supported.contains(&ThinkingLevel::XHigh));
    assert!(!supported.contains(&ThinkingLevel::Max));
    assert_eq!(
        clamp_thinking_level(&model, ThinkingLevel::XHigh),
        ThinkingLevel::High,
        "no supported level above xhigh: walk down"
    );
    assert_eq!(
        clamp_thinking_level(&model, ThinkingLevel::Max),
        ThinkingLevel::High
    );

    // Max clamps down to the highest mapped level; the rest of the simple
    // options surface (max_tokens, temperature, budgets) is left untouched
    // because the adapters own the budget/max_tokens coupling.
    let options = StreamOptions {
        reasoning: Some(ThinkingLevel::Max),
        max_tokens: Some(4096),
        temperature: Some(0.2),
        thinking_budgets: Some(ThinkingBudgets {
            high: Some(20_000),
            ..ThinkingBudgets::default()
        }),
        ..StreamOptions::default()
    };
    let simple = simple_stream_options(&model, options);
    assert_eq!(simple.reasoning, Some(ThinkingLevel::High));
    assert_eq!(simple.max_tokens, Some(4096));
    assert_eq!(simple.temperature, Some(0.2));
    assert_eq!(
        simple.thinking_budgets.and_then(|budgets| budgets.high),
        Some(20_000)
    );

    // A model with no map supports every non-extended level; extended levels
    // fall back to high.
    let unmapped = reasoning_model();
    assert_eq!(
        clamp_thinking_level(&unmapped, ThinkingLevel::Max),
        ThinkingLevel::High
    );

    // Non-reasoning models collapse every request to "off".
    let mut plain = model.clone();
    plain.reasoning = false;
    let simple = simple_stream_options(
        &plain,
        StreamOptions {
            reasoning: Some(ThinkingLevel::High),
            ..StreamOptions::default()
        },
    );
    assert_eq!(simple.reasoning, Some(ThinkingLevel::Off));

    // An omitted level stays omitted (no implicit thinking request).
    let simple = simple_stream_options(&model, StreamOptions::default());
    assert_eq!(simple.reasoning, None);
}

type SeenThinkingRequests =
    std::sync::Arc<std::sync::Mutex<Vec<(Option<pir::ai::types::ThinkingLevel>, Option<u64>)>>>;

struct SimpleCaptureProvider {
    model: pir::ai::types::Model,
    seen: SeenThinkingRequests,
}

impl pir::ai::provider::Provider for SimpleCaptureProvider {
    fn id(&self) -> &str {
        "simple-capture"
    }

    fn name(&self) -> &str {
        "Simple capture"
    }

    fn get_models(&self) -> Vec<pir::ai::types::Model> {
        vec![self.model.clone()]
    }

    fn api_key_auth(&self) -> Vec<pir::ai::auth::ApiKeyAuth> {
        vec![pir::ai::auth::ApiKeyAuth::new(
            "simple test key",
            vec!["PIR_SIMPLE_TEST_KEY".to_string()],
        )]
    }

    fn stream(
        &self,
        model: &pir::ai::types::Model,
        _context: pir::ai::types::Context,
        options: pir::ai::types::StreamOptions,
    ) -> pir::ai::event_stream::AssistantMessageEventStream {
        self.seen
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push((options.reasoning, options.max_tokens));
        let stream = pir::ai::event_stream::AssistantMessageEventStream::new();
        let producer = stream.producer();
        let message = pir::ai::types::AssistantMessage {
            api: model.api.clone(),
            provider: model.provider.clone(),
            model: model.id.clone(),
            stop_reason: pir::ai::types::StopReason::Stop,
            ..pir::ai::types::AssistantMessage::default()
        };
        producer.push(pir::ai::types::AssistantMessageEvent::Start {
            partial: std::sync::Arc::new(message.clone()),
        });
        producer.push(
            pir::ai::event_stream::AssistantMessageEventStream::done_event(
                pir::ai::types::TerminalReason::Stop,
                message,
            ),
        );
        producer.end(None);
        stream
    }
}

#[tokio::test]
async fn stream_simple_and_complete_simple_clamp_before_dispatch() {
    use pir::ai::types::{Context, StreamOptions, ThinkingLevel};

    let mut model = reasoning_model();
    model.provider = "simple-capture".into();
    model.thinking_level_map = Some(
        [
            ("low".to_owned(), Some("low".to_owned())),
            ("high".to_owned(), Some("high".to_owned())),
            ("max".to_owned(), None),
        ]
        .into_iter()
        .collect(),
    );
    let provider = SimpleCaptureProvider {
        model: model.clone(),
        seen: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
    };
    let seen = std::sync::Arc::clone(&provider.seen);
    let mut registry = ProviderRegistry::default();
    registry.set_provider(std::sync::Arc::new(provider));

    let options = || StreamOptions {
        api_key: Some("test-key".into()),
        reasoning: Some(ThinkingLevel::Max),
        ..StreamOptions::default()
    };

    let stream = registry.stream_simple(model.clone(), Context::default(), options());
    let message = stream
        .drain_and_result()
        .await
        .expect("the scripted stream ends with a message");
    assert_eq!(message.stop_reason, pir::ai::types::StopReason::Stop);

    let message = registry
        .complete_simple(model, Context::default(), options())
        .await
        .expect("completeSimple drains the same stream");
    assert_eq!(message.stop_reason, pir::ai::types::StopReason::Stop);

    assert_eq!(
        seen.lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_slice(),
        [
            (Some(ThinkingLevel::High), Some(2_000)),
            (Some(ThinkingLevel::High), Some(2_000))
        ],
        "both simple entry points clamp before provider dispatch"
    );
}

/// Records the model a request is dispatched with.
struct SeenModelProvider {
    model: pir::ai::types::Model,
    seen_base_urls: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
}

impl pir::ai::provider::Provider for SeenModelProvider {
    fn id(&self) -> &str {
        "cf-capture"
    }

    fn name(&self) -> &str {
        "Cloudflare capture"
    }

    fn get_models(&self) -> Vec<pir::ai::types::Model> {
        vec![self.model.clone()]
    }

    fn api_key_auth(&self) -> Vec<pir::ai::auth::ApiKeyAuth> {
        vec![pir::ai::auth::ApiKeyAuth::new(
            "capture key",
            vec!["PIR_CF_CAPTURE_KEY".to_string()],
        )]
    }

    fn stream(
        &self,
        model: &pir::ai::types::Model,
        _context: pir::ai::types::Context,
        _options: pir::ai::types::StreamOptions,
    ) -> pir::ai::event_stream::AssistantMessageEventStream {
        self.seen_base_urls
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(model.base_url.clone());
        let stream = pir::ai::event_stream::AssistantMessageEventStream::new();
        let producer = stream.producer();
        let message = pir::ai::types::AssistantMessage {
            api: model.api.clone(),
            provider: model.provider.clone(),
            model: model.id.clone(),
            stop_reason: pir::ai::types::StopReason::Stop,
            ..pir::ai::types::AssistantMessage::default()
        };
        producer.push(pir::ai::types::AssistantMessageEvent::Start {
            partial: std::sync::Arc::new(message.clone()),
        });
        producer.push(
            pir::ai::event_stream::AssistantMessageEventStream::done_event(
                pir::ai::types::TerminalReason::Stop,
                message,
            ),
        );
        producer.end(None);
        stream
    }
}

/// The strict `stream`/`complete` path materializes the Cloudflare endpoint
/// placeholders from the resolved request env exactly like the
/// `stream_resolving_auth` path: the adapter sends `model.base_url` verbatim,
/// so an unresolved `{CLOUDFLARE_ACCOUNT_ID}` would reach the wire.
#[tokio::test]
async fn stream_resolves_cloudflare_endpoint_placeholders() {
    use pir::ai::types::{Context, StreamOptions};

    let mut model = reasoning_model();
    model.provider = "cf-capture".into();
    model.base_url =
        "https://api.cloudflare.com/client/v4/accounts/{CLOUDFLARE_ACCOUNT_ID}/ai/v1".into();
    let provider = SeenModelProvider {
        model: model.clone(),
        seen_base_urls: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
    };
    let seen = std::sync::Arc::clone(&provider.seen_base_urls);
    let mut registry = ProviderRegistry::default();
    registry.set_provider(std::sync::Arc::new(provider));

    let env = BTreeMap::from([("CLOUDFLARE_ACCOUNT_ID".to_string(), "acc-42".to_string())]);
    let message = registry
        .complete(
            model,
            Context::default(),
            StreamOptions {
                api_key: Some("test-key".into()),
                env: Some(env),
                ..StreamOptions::default()
            },
        )
        .await
        .expect("the scripted provider completes");

    assert_eq!(
        message.stop_reason,
        pir::ai::types::StopReason::Stop,
        "the model reached the provider and the stream completed"
    );
    assert_eq!(
        seen.lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .as_slice(),
        ["https://api.cloudflare.com/client/v4/accounts/acc-42/ai/v1"]
    );
}

#[test]
fn simple_max_tokens_clamp_matches_upstream_boundaries() {
    use pir::ai::provider::{clamp_max_tokens_to_context, simple_stream_options_for_context};
    use pir::ai::types::{Context, StreamOptions, ThinkingLevel};

    let mut model = reasoning_model();
    // A model declaring no window keeps the requested budget (floor 1).
    model.context_window = 0;
    model.max_tokens = 10_000;
    assert_eq!(
        clamp_max_tokens_to_context(&model, &Context::default(), 123_456),
        123_456,
        "contextWindow <= 0 disables the clamp"
    );
    assert_eq!(
        clamp_max_tokens_to_context(&model, &Context::default(), 0),
        1,
        "the floor is MIN_MAX_TOKENS, never zero"
    );

    // Empty context, normal window: only the 4096-token safety reserve is
    // subtracted.
    model.context_window = 8_192;
    model.max_tokens = 8_000;
    assert_eq!(
        clamp_max_tokens_to_context(&model, &Context::default(), 100_000),
        4_096
    );
    // The clamp never raises a smaller explicit request.
    assert_eq!(
        clamp_max_tokens_to_context(&model, &Context::default(), 100),
        100
    );
    // An omitted request falls back to model.max_tokens, then clamps; the
    // simple helper always leaves an explicit value behind.
    let options =
        simple_stream_options_for_context(&model, &Context::default(), StreamOptions::default());
    assert_eq!(options.max_tokens, Some(4_096));
    let options = simple_stream_options_for_context(
        &model,
        &Context::default(),
        StreamOptions {
            max_tokens: Some(100_000),
            ..StreamOptions::default()
        },
    );
    assert_eq!(options.max_tokens, Some(4_096));
    // Thinking-level clamping still applies alongside the budget clamp.
    let options = simple_stream_options_for_context(
        &model,
        &Context::default(),
        StreamOptions {
            reasoning: Some(ThinkingLevel::Max),
            ..StreamOptions::default()
        },
    );
    assert_eq!(options.reasoning, Some(ThinkingLevel::High));

    // Small window: an empty context already has no room after the safety
    // reserve; the clamp bottoms out at the floor instead of underflowing.
    model.context_window = 4_096;
    assert_eq!(
        clamp_max_tokens_to_context(&model, &Context::default(), 100_000),
        1
    );

    // Estimated context tokens reduce the room (4000 chars = 1000 tokens).
    model.context_window = 8_192;
    let context = Context {
        messages: vec![user_message(&"x".repeat(4_000))],
        ..Context::default()
    };
    assert_eq!(
        clamp_max_tokens_to_context(&model, &context, 100_000),
        3_096
    );
    // A context bigger than the window saturates at the same floor.
    let huge = Context {
        messages: vec![user_message(&"x".repeat(80_000))],
        ..Context::default()
    };
    assert_eq!(clamp_max_tokens_to_context(&model, &huge, 100_000), 1);
}

#[tokio::test]
async fn stream_simple_entries_clamp_max_tokens_before_dispatch() {
    use pir::ai::types::{Context, StreamOptions};

    let mut model = reasoning_model();
    model.provider = "simple-capture".into();
    model.context_window = 8_192;
    model.max_tokens = 8_000;
    let provider = SimpleCaptureProvider {
        model: model.clone(),
        seen: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
    };
    let seen = std::sync::Arc::clone(&provider.seen);
    let mut registry = ProviderRegistry::default();
    registry.set_provider(std::sync::Arc::new(provider));

    let options = || StreamOptions {
        api_key: Some("test-key".into()),
        ..StreamOptions::default()
    };

    // Empty context: 8192 - 0 - 4096 = 4096 available, so the model default
    // (8000) is clamped down to it.
    let message = registry
        .stream_simple(model.clone(), Context::default(), options())
        .drain_and_result()
        .await
        .expect("the scripted stream ends with a message");
    assert_eq!(message.stop_reason, pir::ai::types::StopReason::Stop);

    // completeSimple shares the entry: the caller asked for far more than the
    // remaining window fits (8192 - 1000 estimated - 4096 = 3096).
    let context = Context {
        messages: vec![user_message(&"x".repeat(4_000))],
        ..Context::default()
    };
    let message = registry
        .complete_simple(
            model,
            context,
            StreamOptions {
                max_tokens: Some(100_000),
                ..options()
            },
        )
        .await
        .expect("completeSimple drains the same stream");
    assert_eq!(message.stop_reason, pir::ai::types::StopReason::Stop);

    assert_eq!(
        seen.lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_slice(),
        [(None, Some(4_096)), (None, Some(3_096))],
        "both simple entries hand the provider a context-clamped budget"
    );
}

/// The overlay's first network hop is derived from the persisted
/// `checkedAt`, not a blind full interval: a fresh check sleeps only the
/// remainder of its freshness window, an expired one fires immediately, and
/// a store with no timestamp keeps the legacy one-interval startup delay.
#[test]
fn remote_catalog_first_refresh_delay_follows_checked_at() {
    use pir::ai::providers::{remote_catalog_initial_delay, REMOTE_CATALOG_REFRESH_INTERVAL};

    const NOW: u64 = 1_700_000_000_000; // Unix milliseconds
    let interval = REMOTE_CATALOG_REFRESH_INTERVAL;
    assert_eq!(
        remote_catalog_initial_delay(interval, None, NOW),
        interval,
        "no persisted check: keep the delayed first hop (no startup burst)"
    );
    assert_eq!(
        remote_catalog_initial_delay(interval, Some(NOW - 60_000), NOW),
        interval - Duration::from_secs(60),
        "a check 60s old sleeps the remaining window"
    );
    assert_eq!(
        remote_catalog_initial_delay(interval, Some(NOW - interval.as_millis() as u64 - 1), NOW),
        Duration::ZERO,
        "an expired check fires its first hop immediately"
    );
    assert_eq!(
        remote_catalog_initial_delay(interval, Some(NOW + 10_000), NOW),
        interval,
        "a future timestamp clamps to one interval, never longer"
    );
}

/// A same-length in-place rewrite can keep `(len, mtime, ctime)` unchanged
/// inside one filesystem timestamp tick. The parse cache must still detect it
/// through the content identity and reparse instead of serving the old entry.
#[test]
fn models_store_cache_detects_same_stamp_content_rewrite() {
    use pir::ai::providers::models_store::{
        models_store_cache_stats, models_store_path, read_models_store_entry,
    };

    let dir = std::env::temp_dir().join(format!(
        "pir-models-store-content-{}-{}",
        std::process::id(),
        uuid::Uuid::new_v4()
    ));
    std::fs::create_dir_all(&dir).expect("create temp agent dir");
    let path = models_store_path(&dir);
    let first = r#"{"openai":{"models":[{"id":"model-a"}]}}"#;
    let second = r#"{"openai":{"models":[{"id":"model-b"}]}}"#;
    assert_eq!(
        first.len(),
        second.len(),
        "the rewrite must keep the length"
    );

    std::fs::write(&path, first).expect("seed store");
    let entry = read_models_store_entry(&dir, "openai").expect("first parse");
    assert_eq!(entry.models[0]["id"], "model-a");
    let baseline = models_store_cache_stats(&dir);

    // Rewrite in place with the same byte length, then pin the modification
    // time back to its previous value. Creation time is unchanged because the
    // same file is truncated and rewritten, so the metadata stamp matches.
    let metadata = std::fs::metadata(&path).expect("metadata before rewrite");
    let modified = metadata.modified().expect("modified before rewrite");
    let created = metadata.created().ok();
    std::fs::write(&path, second).expect("same-length rewrite");
    let file = std::fs::OpenOptions::new()
        .write(true)
        .open(&path)
        .expect("open store for set_times");
    file.set_times(std::fs::FileTimes::new().set_modified(modified))
        .expect("restore modification time");
    drop(file);
    let metadata = std::fs::metadata(&path).expect("metadata after rewrite");
    assert_eq!(metadata.len(), first.len() as u64);
    assert_eq!(
        metadata.modified().ok(),
        Some(modified),
        "the test must reproduce a same-stamp rewrite"
    );
    assert_eq!(metadata.created().ok(), created);

    let entry = read_models_store_entry(&dir, "openai").expect("content change reparses");
    assert_eq!(
        entry.models[0]["id"], "model-b",
        "the content identity must reject the stale parse"
    );
    let after = models_store_cache_stats(&dir);
    assert_eq!(
        after.parses,
        baseline.parses + 1,
        "exactly one reparse for the changed content: {after:?}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// A restart with an expired persisted `checkedAt` must fire the first
/// background revalidation immediately instead of sleeping a full 4h
/// interval. (A fresh timestamp takes the helper-tested remaining-window
/// path; the no-timestamp legacy delay is pinned by the helper test too.)
#[tokio::test]
async fn remote_catalog_overlay_first_hop_uses_persisted_checked_at() {
    use pir::ai::providers::models_store::{write_models_store_entry, ModelsStoreEntry};
    use pir::ai::providers::{
        RemoteCatalogOverlay, RemoteCatalogOverlayConfig, REMOTE_CATALOG_REFRESH_INTERVAL,
    };
    use std::io::Write as _;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::time::Instant;

    let dir = std::env::temp_dir().join(format!(
        "pir-overlay-checked-at-{}-{}",
        std::process::id(),
        uuid::Uuid::new_v4()
    ));
    std::fs::create_dir_all(&dir).expect("create temp agent dir");

    let now = pir::ai::util::now_ms();
    let expired = now.saturating_sub(REMOTE_CATALOG_REFRESH_INTERVAL.as_millis() as u64 + 60_000);
    write_models_store_entry(
        &dir,
        "anthropic",
        ModelsStoreEntry {
            checked_at: Some(expired),
            ..ModelsStoreEntry::default()
        },
    )
    .expect("seed persisted checkedAt");

    // One-shot mock catalog endpoint: record the first request and answer 404
    // (the overlay clears itself; the test only pins that the hop happened).
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind mock catalog");
    let addr = listener.local_addr().expect("mock address");
    let requests = Arc::new(AtomicUsize::new(0));
    let server_requests = Arc::clone(&requests);
    std::thread::spawn(move || {
        if let Ok((mut stream, _)) = listener.accept() {
            server_requests.fetch_add(1, Ordering::SeqCst);
            let response =
                "HTTP/1.1 404 Not Found\r\ncontent-length: 0\r\nconnection: close\r\n\r\n";
            let _ = stream.write_all(response.as_bytes());
        }
    });

    let registry = Arc::new(ProviderRegistry::default());
    let overlay = Arc::new(
        RemoteCatalogOverlay::new(
            RemoteCatalogOverlayConfig {
                provider_id: "anthropic".to_string(),
                catalog_url: format!("http://{addr}/catalog"),
                agent_dir: dir.clone(),
                interval: None,
            },
            registry,
        )
        .expect("overlay construction"),
    );
    assert!(overlay.spawn_refresh_task(), "spawn inside the runtime");

    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline && requests.load(Ordering::SeqCst) == 0 {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(
        requests.load(Ordering::SeqCst) >= 1,
        "an expired persisted checkedAt must not wait another full interval"
    );

    let _ = std::fs::remove_dir_all(&dir);
}
