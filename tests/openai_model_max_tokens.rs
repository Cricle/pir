//! P2 parity: the OpenAI-compatible `/chat/completions` body always carries
//! the response token cap. Upstream `streamSimple`/`buildBaseOptions` fills
//! `options.maxTokens` with the effective `options?.maxTokens ??
//! model.maxTokens` before `buildParams` writes it — as
//! `max_completion_tokens`, or `max_tokens` on the per-compat exception path.
//!
//! Standalone test binary (the `tests/suites/` and `g_*` harnesses belong to
//! another work stream).

use pir::ai::api::openai::build_openai_body;
use pir::ai::types::*;

fn model(max_tokens: u64) -> Model {
    Model {
        id: "gpt".into(),
        name: "gpt".into(),
        api: "openai-completions".into(),
        provider: "test".into(),
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

fn body_for(model: &Model, options: &StreamOptions, compat: &OpenAiCompat) -> serde_json::Value {
    build_openai_body(model, &Context::default(), options, compat).expect("body")
}

/// No request-side `max_tokens`: the model's `maxTokens` default is sent as
/// `max_completion_tokens` (upstream `options?.maxTokens ?? model.maxTokens`).
#[test]
fn openai_body_defaults_the_token_cap_to_the_model_max_tokens() {
    let body = body_for(
        &model(8192),
        &StreamOptions::default(),
        &OpenAiCompat::default(),
    );
    assert_eq!(
        body["max_completion_tokens"], 8192,
        "the model maxTokens default fills the cap: {body}"
    );
    assert!(
        body.get("max_tokens").is_none(),
        "the deprecated field stays off by default"
    );
}

/// An explicit request option still wins over the model default, on both
/// field branches.
#[test]
fn openai_body_explicit_max_tokens_option_wins() {
    let options = StreamOptions {
        max_tokens: Some(512),
        ..StreamOptions::default()
    };
    let body = body_for(&model(8192), &options, &OpenAiCompat::default());
    assert_eq!(body["max_completion_tokens"], 512);
    assert!(body.get("max_tokens").is_none());

    let compat = OpenAiCompat {
        max_tokens_field: Some("max_tokens".into()),
        ..OpenAiCompat::default()
    };
    let body = body_for(&model(8192), &options, &compat);
    assert_eq!(body["max_tokens"], 512);
    assert!(
        body.get("max_completion_tokens").is_none(),
        "the compat branch selects exactly one field"
    );
}

/// The compat `maxTokensField: "max_tokens"` branch keeps its behavior for
/// the model default too, and never emits two cap fields at once.
#[test]
fn openai_body_compat_field_branch_uses_the_model_default() {
    let compat = OpenAiCompat {
        max_tokens_field: Some("max_tokens".into()),
        ..OpenAiCompat::default()
    };
    let body = body_for(&model(2048), &StreamOptions::default(), &compat);
    assert_eq!(body["max_tokens"], 2048);
    assert!(body.get("max_completion_tokens").is_none());
}

/// A zero model default sends nothing (upstream guards the assignment on the
/// JS-falsy `options.maxTokens`); a model without a configured cap is not
/// forced onto the gateway.
#[test]
fn openai_body_zero_model_max_tokens_sends_nothing() {
    let body = body_for(
        &model(0),
        &StreamOptions::default(),
        &OpenAiCompat::default(),
    );
    assert!(
        body.get("max_completion_tokens").is_none(),
        "a zero cap is not sent: {body}"
    );
    assert!(body.get("max_tokens").is_none());
}

/// The agent main loop's options (no `max_tokens`) produce the cap field the
/// perf probe asserts.
#[test]
fn openai_body_from_default_stream_options_has_the_probe_field() {
    let options = StreamOptions {
        api_key: Some("test-key".into()),
        reasoning: Some(ThinkingLevel::Off),
        ..StreamOptions::default()
    };
    let body = body_for(&model(8192), &options, &OpenAiCompat::default());
    assert!(
        body.get("max_completion_tokens").is_some(),
        "otherKeys must contain max_completion_tokens: {body}"
    );
}
