//! Regression test for upstream `6fcb48670` (#11036): summarization requests
//! use the model's own output limit, never a fraction of the compaction
//! reserve. The helpers are the single source of the max-token value.

use pir::ai::types::{Model, ModelCost, ModelCostRates};
use pir::compaction::{
    branch_summary_max_tokens, summarization_max_tokens, turn_prefix_max_tokens,
};

fn model_with_limit(max_tokens: u64) -> Model {
    Model {
        id: "test-model".into(),
        name: "Test model".into(),
        api: "openai-completions".into(),
        provider: "test".into(),
        base_url: "http://localhost".into(),
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
        context_window: 128_000,
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

#[test]
fn summary_limits_ignore_the_reserve_and_use_the_model_limit() {
    let model = model_with_limit(8_192);

    // A huge reserve must not shrink the request: upstream removed the
    // `0.8 * reserveTokens` / `0.5 * reserveTokens` caps.
    assert_eq!(summarization_max_tokens(1_000_000, &model), 8_192);
    assert_eq!(turn_prefix_max_tokens(1_000_000, &model), 8_192);
    assert_eq!(branch_summary_max_tokens(&model), 8_192);
}

#[test]
fn a_zero_model_limit_stays_zero_to_mean_no_cap() {
    let model = model_with_limit(0);

    assert_eq!(summarization_max_tokens(64_000, &model), 0);
    assert_eq!(turn_prefix_max_tokens(64_000, &model), 0);
    assert_eq!(branch_summary_max_tokens(&model), 0);
}
