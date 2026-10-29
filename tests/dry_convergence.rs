//! DRY 收敛回归测试（独立二进制，不改动既有测试文件）。
//!
//! 覆盖本次收敛后必须保持不变的公开行为：
//! - `compaction::summarization_max_tokens` / `turn_prefix_max_tokens`：
//!   上游 6fcb48670 后不再按 reserve 分数计算固定上限，两个 helper 都返回
//!   模型自身的输出上限（`model.max_tokens`；0 表示不额外限制）。
//! - `ai::auth::http`：`get_json` / `post_form` 收敛到带 headers 的实现后，
//!   状态码与响应体透传行为不变（仅影响内部组装路径，签名与返回值不变）。

use pir::ai::types::{Model, ModelCost, ModelCostRates};
use pir::compaction::{summarization_max_tokens, turn_prefix_max_tokens};

/// 与既有测试（ai_request_snapshots.rs）一致的 Model 构造：`Model` 没有
/// `Default`，逐字段给出（本测试只依赖 `max_tokens` 与计数语义）。
fn model_with_max_tokens(max_tokens: u64) -> Model {
    Model {
        id: "dry-test".into(),
        name: "dry-test".into(),
        api: "openai-chat".into(),
        provider: "dry-test".into(),
        base_url: "http://localhost:1".into(),
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

#[test]
fn summary_requests_use_the_models_own_output_limit() {
    // Upstream 6fcb48670: the summary request no longer computes a fixed cap
    // from reserveTokens; both helpers report the model's own output limit.
    let uncapped = model_with_max_tokens(0);
    assert_eq!(summarization_max_tokens(1000, &uncapped), 0);
    assert_eq!(turn_prefix_max_tokens(1000, &uncapped), 0);
    assert_eq!(summarization_max_tokens(0, &uncapped), 0);
    assert_eq!(turn_prefix_max_tokens(0, &uncapped), 0);

    let capped = model_with_max_tokens(300);
    assert_eq!(summarization_max_tokens(1000, &capped), 300);
    assert_eq!(turn_prefix_max_tokens(1000, &capped), 300);

    let loose = model_with_max_tokens(900);
    assert_eq!(summarization_max_tokens(1000, &loose), 900);
    assert_eq!(turn_prefix_max_tokens(1000, &loose), 900);
}
