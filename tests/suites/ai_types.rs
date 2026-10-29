use std::collections::BTreeMap;

use pir::ai::types::*;
use serde_json::json;

#[test]
fn defaults_and_abort_signal_work() {
    assert_eq!(Usage::default().total_tokens, 0);
    let signal = AbortSignal::new();
    assert!(!signal.is_aborted());
    signal.abort();
    assert!(signal.check().is_err());
}

#[test]
fn assistant_message_serde_roundtrip() {
    let msg = AssistantMessage {
        role: AssistantRole::Assistant,
        content: vec![ContentBlock::Text(TextContent {
            text: "Hello".into(),
            text_signature: None,
        })],
        api: "faux".into(),
        provider: "faux".into(),
        model: "faux-1".into(),
        response_model: None,
        response_id: Some("resp-1".into()),
        diagnostics: vec![],
        usage: Usage::default(),
        stop_reason: StopReason::Stop,
        deferred: None,
        error_message: None,
        raw_stop_reason: None,
        end_turn: None,
        provider_thinking_level: None,
        thinking_level: None,
        duration_ms: None,
        timestamp: 1000,
    };
    let json = serde_json::to_string(&msg).unwrap();
    let decoded: AssistantMessage = serde_json::from_str(&json).unwrap();
    assert_eq!(decoded.model, "faux-1");
    assert_eq!(decoded.response_id.as_deref(), Some("resp-1"));
    let value = serde_json::to_value(&msg).unwrap();
    assert_eq!(
        value.get("stopReason").and_then(|v| v.as_str()),
        Some("stop")
    );
    assert!(value.get("stop_reason").is_none());
}

fn sample_model() -> Model {
    Model {
        id: "model-1".into(),
        name: "Model One".into(),
        api: "openai-completions".into(),
        provider: "openai".into(),
        base_url: "https://example.test/v1".into(),
        model_type: Some(ModelKind::Chat),
        reasoning: true,
        thinking_level_map: None,
        input: vec!["text".into()],
        input_limits: Some(json!({ "maxRequestBytes": 1024 })),
        cost: ModelCost {
            rates: ModelCostRates {
                input: 1.0,
                output: 2.0,
                cache_read: 0.1,
                cache_write: 0.2,
            },
            tiers: vec![ModelCostTier {
                rates: ModelCostRates {
                    input: 3.0,
                    output: 4.0,
                    cache_read: 0.3,
                    cache_write: 0.4,
                },
                input_tokens_above: 200_000,
            }],
        },
        context_window: 128_000,
        max_tokens: 4096,
        sampling_params: None,
        sampling_params_by_thinking_level: Some(BTreeMap::from([(
            "high".into(),
            BTreeMap::from([("temperature".into(), json!(0.2))]),
        )])),
        headers: None,
        prompt_cache: Some(json!({ "short": 300 })),
        thinking_token_budget_field: None,
        compat: Some(OpenAiCompat {
            thinking_token_budget_field_compat: Some(
                ThinkingTokenBudgetField::ThinkingBudgetTokens,
            ),
            ..Default::default()
        }),
    }
}

#[test]
fn model_wire_uses_camel_case_and_round_trips() {
    let model = sample_model();
    let value = serde_json::to_value(&model).unwrap();

    assert_eq!(value["type"], json!("chat"));
    assert_eq!(value["inputLimits"]["maxRequestBytes"], json!(1024));
    assert_eq!(value["promptCache"]["short"], json!(300));
    assert_eq!(
        value["samplingParamsByThinkingLevel"]["high"]["temperature"],
        json!(0.2)
    );
    assert_eq!(value["cost"]["cacheRead"], json!(0.1));
    assert_eq!(
        value["cost"]["tiers"][0]["inputTokensAbove"],
        json!(200_000)
    );
    assert_eq!(value["cost"]["tiers"][0]["cacheWrite"], json!(0.4));
    assert_eq!(
        value["compat"]["thinkingTokenBudgetField"],
        json!("thinking_budget_tokens")
    );
    assert!(value.get("thinkingTokenBudgetField").is_none());
    assert!(value.get("thinking_token_budget_field").is_none());

    let round: Model = serde_json::from_value(value).unwrap();
    assert_eq!(round, model);
}

#[test]
fn model_reads_legacy_aliases_and_nested_compat_fields() {
    let value = json!({
        "id": "model-1",
        "name": "Model One",
        "api": "openai-completions",
        "provider": "openai",
        "baseUrl": "https://example.test/v1",
        "type": "chat",
        "reasoning": true,
        "input": ["text"],
        "input_limits": { "maxRequestBytes": 5 },
        "cost": {
            "input": 1.0,
            "output": 2.0,
            "cacheRead": 0.1,
            "cacheWrite": 0.2
        },
        "contextWindow": 1000,
        "maxTokens": 100,
        "sampling_params_by_thinking_level": {
            "high": { "temperature": 0.3 }
        },
        "promptCache": { "short": 60 },
        "thinkingTokenBudgetField": "thinking_budget",
        "compat": {
            "inputLimits": { "maxRequestBytes": 7 },
            "promptCache": { "long": 120 },
            "samplingParamsByThinkingLevel": {
                "low": { "top_p": 0.9 }
            }
        }
    });

    let model: Model = serde_json::from_value(value).unwrap();
    assert_eq!(model.model_type, Some(ModelKind::Chat));
    assert_eq!(model.input_limits, Some(json!({ "maxRequestBytes": 5 })));
    assert_eq!(model.prompt_cache, Some(json!({ "short": 60 })));
    assert_eq!(
        model.sampling_params_by_thinking_level,
        Some(BTreeMap::from([(
            "high".into(),
            BTreeMap::from([("temperature".into(), json!(0.3))]),
        )]))
    );
    assert_eq!(
        model.thinking_token_budget_field,
        Some(ThinkingTokenBudgetField::ThinkingBudget)
    );

    let compat = model.compat.expect("compat block");
    assert_eq!(compat.input_limits, Some(json!({ "maxRequestBytes": 7 })));
    assert_eq!(compat.prompt_cache, Some(json!({ "long": 120 })));
    assert_eq!(
        compat.sampling_params_by_thinking_level,
        Some(BTreeMap::from([(
            "low".into(),
            BTreeMap::from([("top_p".into(), json!(0.9))]),
        )]))
    );
}

#[test]
fn tool_call_wire_includes_type_and_arguments_error() {
    let call = ToolCall {
        id: "call-1".into(),
        name: "search".into(),
        arguments: BTreeMap::from([("q".into(), json!("rust"))]),
        thought_signature: Some("sig".into()),
        namespace: Some("ns".into()),
        arguments_error: Some("arguments were not valid JSON".into()),
    };

    let value = serde_json::to_value(&call).unwrap();
    assert_eq!(value["type"], json!("toolCall"));
    assert_eq!(
        value["argumentsError"],
        json!("arguments were not valid JSON")
    );
    assert!(value.get("arguments_error").is_none());
    assert!(value.get("thought_signature").is_none());

    let round: ToolCall = serde_json::from_value(value).unwrap();
    assert_eq!(round, call);

    let block = ContentBlock::ToolCall(call);
    let text = serde_json::to_string(&block).unwrap();
    assert_eq!(text.matches("\"type\":\"toolCall\"").count(), 1);
    let value = serde_json::to_value(&block).unwrap();
    assert_eq!(value["type"], json!("toolCall"));
    let round: ContentBlock = serde_json::from_value(value).unwrap();
    assert_eq!(round, block);

    let mut without_error = match &block {
        ContentBlock::ToolCall(call) => call.clone(),
        other => panic!("unexpected block: {other:?}"),
    };
    without_error.arguments_error = None;
    let value = serde_json::to_value(&without_error).unwrap();
    assert!(value.get("argumentsError").is_none());
}

#[test]
fn diagnostic_info_preserves_error_stack_and_code() {
    let diagnostic = DiagnosticInfo {
        diag_type: "retry".into(),
        timestamp: 42,
        error_message: Some("timeout".into()),
        error_name: Some("TimeoutError".into()),
        error_stack: Some("at request".into()),
        error_code: Some(json!("ETIMEDOUT")),
        details: Some(BTreeMap::from([("attempt".into(), json!(2))])),
    };

    let value = serde_json::to_value(&diagnostic).unwrap();
    assert_eq!(value["type"], json!("retry"));
    assert_eq!(value["error"]["name"], json!("TimeoutError"));
    assert_eq!(value["error"]["message"], json!("timeout"));
    assert_eq!(value["error"]["stack"], json!("at request"));
    assert_eq!(value["error"]["code"], json!("ETIMEDOUT"));
    assert!(value["error"].get("error_stack").is_none());

    let round: DiagnosticInfo = serde_json::from_value(value).unwrap();
    assert_eq!(round, diagnostic);

    let legacy: DiagnosticInfo = serde_json::from_value(json!({
        "type": "legacy",
        "timestamp": 7,
        "errorMessage": "boom",
        "errorName": "Boom",
        "details": { "key": "value" }
    }))
    .unwrap();
    assert_eq!(legacy.error_message.as_deref(), Some("boom"));
    assert_eq!(legacy.error_name.as_deref(), Some("Boom"));
}

#[test]
fn diagnostic_without_message_round_trips_as_none() {
    let diagnostic = DiagnosticInfo {
        diag_type: "retry".into(),
        timestamp: 43,
        error_message: None,
        error_name: Some("TimeoutError".into()),
        error_stack: Some("at request".into()),
        error_code: Some(json!(524)),
        details: None,
    };

    let value = serde_json::to_value(&diagnostic).unwrap();
    assert_eq!(value["error"]["message"], json!(""));
    assert_eq!(value["error"]["name"], json!("TimeoutError"));
    assert_eq!(value["error"]["stack"], json!("at request"));
    assert_eq!(value["error"]["code"], json!(524));

    let round: DiagnosticInfo = serde_json::from_value(value).unwrap();
    assert_eq!(round, diagnostic);
}

#[test]
fn deferred_handle_reads_legacy_snake_case_and_writes_camel_case() {
    let handle: DeferredHandle = serde_json::from_value(json!({
        "provider": "openai",
        "model_id": "gpt-x",
        "api": "openai-responses",
        "id": "resp-1",
        "expires_at": 1_700_000_060_000_u64,
        "poll_after_ms": 500,
        "data": { "poll": true }
    }))
    .unwrap();
    assert_eq!(handle.model_id, "gpt-x");
    assert_eq!(handle.expires_at, Some(1_700_000_060_000));
    assert_eq!(handle.poll_after_ms, Some(500));

    let value = serde_json::to_value(&handle).unwrap();
    assert_eq!(value["modelId"], json!("gpt-x"));
    assert_eq!(value["expiresAt"], json!(1_700_000_060_000_u64));
    assert_eq!(value["pollAfterMs"], json!(500));
    assert!(value.get("model_id").is_none());
}

#[cfg(feature = "provider-openai")]
#[test]
fn resolve_sampling_params_falls_back_to_model_level_thinking_map() {
    let mut model = sample_model();
    model.compat = Some(OpenAiCompat::default());
    model.sampling_params = None;

    let merged =
        pir::ai::api::openai::resolve_sampling_params(&model, Some(ThinkingLevel::High), None)
            .expect("model-level thinking map");
    assert_eq!(merged.get("temperature"), Some(&json!(0.2)));
}

#[test]
fn construction_model_defaults_to_chat_type() {
    let model = pir::ai::providers::construction::model(
        "test",
        "test-model",
        "Test Model",
        "openai-completions",
        "https://example.test/v1",
        1_000,
        100,
        vec!["text"],
    );
    assert_eq!(model.model_type, Some(ModelKind::Chat));
    let value = serde_json::to_value(&model).unwrap();
    assert_eq!(value["type"], json!("chat"));
}

#[test]
fn usage_wire_uses_camel_case() {
    let usage = Usage {
        input: 1,
        output: 2,
        cache_read: 3,
        cache_write: 4,
        cache_write_1h: Some(5),
        reasoning: Some(6),
        total_tokens: 10,
        cost: Cost {
            input: 0.1,
            output: 0.2,
            cache_read: 0.3,
            cache_write: 0.4,
            total: 1.0,
            total_from_provider: None,
        },
    };

    let value = serde_json::to_value(usage).unwrap();
    assert_eq!(value["cacheRead"], json!(3));
    assert_eq!(value["cacheWrite"], json!(4));
    assert_eq!(value["cacheWrite1h"], json!(5));
    assert_eq!(value["totalTokens"], json!(10));
    assert_eq!(value["cost"]["cacheRead"], json!(0.3));
    assert_eq!(value["cost"]["cacheWrite"], json!(0.4));
    assert!(value.get("cache_read").is_none());

    let round: Usage = serde_json::from_value(value).unwrap();
    assert_eq!(round, usage);
}

// ---------------------------------------------------------------------------
// Usage.cost.totalFromProvider (upstream `calculateCost` /
// `applyOpenRouterReportedCost` / `combineUsage`)
// ---------------------------------------------------------------------------

#[test]
fn cost_total_from_provider_wire_shape_and_legacy_default() {
    let billed = Cost {
        input: 0.1,
        output: 0.2,
        cache_read: 0.0,
        cache_write: 0.0,
        total: 0.3,
        total_from_provider: Some(true),
    };
    let value = serde_json::to_value(billed).unwrap();
    assert_eq!(value["totalFromProvider"], json!(true));
    assert!(billed.is_provider_billed());

    // The estimate marker is written explicitly...
    let estimate = Cost {
        total: 1.0,
        total_from_provider: Some(false),
        ..Cost::default()
    };
    let value = serde_json::to_value(estimate).unwrap();
    assert_eq!(value["totalFromProvider"], json!(false));
    assert!(!estimate.is_provider_billed());

    // ...while the absent flag is omitted (upstream optional field) and reads
    // with the same `false` semantics.
    let absent = Cost {
        total: 2.0,
        ..Cost::default()
    };
    let value = serde_json::to_value(absent).unwrap();
    assert!(value.get("totalFromProvider").is_none());
    assert!(!absent.is_provider_billed());

    // A legacy persisted usage record without the field must still load.
    let legacy: Usage = serde_json::from_value(json!({
        "input": 1,
        "output": 2,
        "cacheRead": 0,
        "cacheWrite": 0,
        "totalTokens": 3,
        "cost": {
            "input": 0.1,
            "output": 0.2,
            "cacheRead": 0.0,
            "cacheWrite": 0.0,
            "total": 0.3
        }
    }))
    .unwrap();
    assert_eq!(legacy.cost.total_from_provider, None);
    assert!(!legacy.cost.is_provider_billed());
}

#[test]
fn calculate_cost_stamps_only_the_estimate_marker() {
    let model = sample_model();
    let mut usage = Usage {
        input: 1_000_000,
        output: 1_000_000,
        total_tokens: 2_000_000,
        // A stale provider-billed marker must not survive a catalog estimate.
        cost: Cost {
            total_from_provider: Some(true),
            ..Cost::default()
        },
        ..Usage::default()
    };
    let cost = calculate_cost(&model, &mut usage);
    // Two million input-side tokens cross the sample model's 200k tier, so
    // the tier rates (3.0 / 4.0 per million) apply.
    assert_eq!(cost.input, 3.0);
    assert_eq!(cost.output, 4.0);
    assert_eq!(cost.total, 7.0);
    assert_eq!(usage.cost.total_from_provider, Some(false));
    assert!(!usage.cost.is_provider_billed());
}

#[test]
fn openrouter_reported_cost_replaces_only_a_complete_valid_total() {
    use pir::ai::util::apply_openrouter_reported_cost;

    let mut usage = Usage {
        input: 10,
        output: 2,
        total_tokens: 12,
        cost: Cost {
            input: 0.5,
            output: 0.5,
            total: 1.0,
            total_from_provider: Some(false),
            ..Cost::default()
        },
        ..Usage::default()
    };

    // Direct (non-BYOK) accounting: the billed cost replaces `total` only;
    // the per-component estimates stay untouched.
    assert!(apply_openrouter_reported_cost(
        &mut usage,
        &json!({"cost": 2.5, "is_byok": false})
    ));
    assert_eq!(usage.cost.total, 2.5);
    assert_eq!(usage.cost.input, 0.5);
    assert_eq!(usage.cost.total_from_provider, Some(true));

    // BYOK with an aggregate upstream cost: billed + upstream.
    assert!(apply_openrouter_reported_cost(
        &mut usage,
        &json!({
            "cost": 2.5,
            "is_byok": true,
            "cost_details": {"upstream_inference_cost": 0.75}
        })
    ));
    assert_eq!(usage.cost.total, 3.25);
    assert_eq!(usage.cost.total_from_provider, Some(true));

    // BYOK with a null aggregate falls back to prompt + completion costs.
    assert!(apply_openrouter_reported_cost(
        &mut usage,
        &json!({
            "cost": 1.0,
            "is_byok": true,
            "cost_details": {
                "upstream_inference_cost": null,
                "upstream_inference_prompt_cost": 0.25,
                "upstream_inference_completions_cost": 0.5
            }
        })
    ));
    assert_eq!(usage.cost.total, 1.75);
    assert_eq!(usage.cost.total_from_provider, Some(true));

    // A missing marker, an invalid cost, or incomplete BYOK details leaves
    // the catalog estimate (and its false marker) intact.
    let before = usage.cost;
    for reported in [
        json!({"cost": 9.0}),
        json!({"cost": -1.0, "is_byok": false}),
        json!({"cost": "9.0", "is_byok": false}),
        json!({"cost": 9.0, "is_byok": true}),
        json!({
            "cost": 9.0,
            "is_byok": true,
            "cost_details": {"upstream_inference_cost": null}
        }),
    ] {
        assert!(!apply_openrouter_reported_cost(&mut usage, &reported));
        assert_eq!(
            usage.cost, before,
            "estimate must stay untouched for {reported}"
        );
    }
}

#[test]
fn combined_usage_is_provider_billed_only_when_every_part_is() {
    use pir::agent::nested_tool_calls::combine_usage;

    let billed = Usage {
        input: 1,
        total_tokens: 1,
        cost: Cost {
            total: 0.0,
            total_from_provider: Some(true),
            ..Cost::default()
        },
        ..Usage::default()
    };
    let estimate = Usage {
        input: 2,
        total_tokens: 2,
        cost: Cost {
            total: 1.0,
            total_from_provider: Some(false),
            ..Cost::default()
        },
        ..Usage::default()
    };
    let absent = Usage {
        input: 3,
        total_tokens: 3,
        cost: Cost {
            total: 2.0,
            ..Cost::default()
        },
        ..Usage::default()
    };

    assert!(combine_usage(billed, billed).cost.is_provider_billed());
    assert!(!combine_usage(billed, estimate).cost.is_provider_billed());
    assert!(!combine_usage(billed, absent).cost.is_provider_billed());
    assert!(!combine_usage(estimate, absent).cost.is_provider_billed());
    let combined = combine_usage(absent, absent);
    assert_eq!(combined.cost.total, 4.0);
    assert!(!combined.cost.is_provider_billed());
}

// ---------------------------------------------------------------------------
// Message content string shape (upstream `SystemMessage.content` and
// `UserMessage.content`: a plain string OR an array of content blocks)
// ---------------------------------------------------------------------------

#[test]
fn message_string_content_deserializes_to_one_text_block() {
    for role in ["user", "system", "developer"] {
        let message: Message = serde_json::from_value(json!({
            "role": role,
            "content": "hello from a bare string",
            "timestamp": 7
        }))
        .unwrap_or_else(|error| panic!("{role}: {error}"));
        let payload = match &message {
            Message::User(payload) | Message::System(payload) | Message::Developer(payload) => {
                payload
            }
            other => panic!("{role}: wrong variant {other:?}"),
        };
        assert_eq!(payload.timestamp, 7);
        assert_eq!(
            payload.content,
            vec![ContentBlock::Text(TextContent {
                text: "hello from a bare string".into(),
                text_signature: None,
            })],
            "{role}: string content becomes one text block"
        );

        // Serialization is always the canonical block array (upstream's
        // in-memory shape), so a reparse sees the block form.
        let value = serde_json::to_value(&message).unwrap();
        assert_eq!(
            value["content"],
            json!([{"type": "text", "text": "hello from a bare string"}]),
            "{role}: canonical wire shape"
        );
        let round: Message = serde_json::from_value(value).unwrap();
        assert_eq!(round, message);
    }

    // The block array still parses, including the empty array.
    let block_form: Message = serde_json::from_value(json!({
        "role": "system",
        "content": [{"type": "text", "text": "block form"}],
        "timestamp": 1
    }))
    .unwrap();
    let empty: Message =
        serde_json::from_value(json!({"role": "user", "content": [], "timestamp": 1})).unwrap();
    match (&block_form, &empty) {
        (Message::System(payload), Message::User(payload_empty)) => {
            assert_eq!(payload.content.len(), 1);
            assert!(payload_empty.content.is_empty());
        }
        other => panic!("wrong variants: {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Request-level thinking option enums
// ---------------------------------------------------------------------------

#[test]
fn thinking_request_option_enums_use_upstream_wire_names() {
    for (value, name) in [
        (ThinkingDisplay::Summarized, "summarized"),
        (ThinkingDisplay::Omitted, "omitted"),
    ] {
        assert_eq!(serde_json::to_value(value).unwrap(), json!(name));
        assert_eq!(value.as_str(), name);
    }
    for (value, name) in [
        (ReasoningSummary::Auto, "auto"),
        (ReasoningSummary::Concise, "concise"),
        (ReasoningSummary::Detailed, "detailed"),
        (ReasoningSummary::Off, "off"),
        (ReasoningSummary::On, "on"),
    ] {
        assert_eq!(serde_json::to_value(value).unwrap(), json!(name));
        assert_eq!(value.as_str(), name);
    }
    for (value, name) in [
        (TextVerbosity::Low, "low"),
        (TextVerbosity::Medium, "medium"),
        (TextVerbosity::High, "high"),
    ] {
        assert_eq!(serde_json::to_value(value).unwrap(), json!(name));
        assert_eq!(value.as_str(), name);
    }
}
