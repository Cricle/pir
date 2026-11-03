//! `/model <ref>[:<level>]` resolution: a plain reference, a bare id on a
//! second provider, and an id whose own colon is not a thinking level.
//!
//! Standalone binary (the `tests/suites/` and `g_*` harnesses belong to other
//! lanes); the module lived inline in `src/tui/slash.rs`, which the source
//! hygiene gate forbids.

use pir::ai::types::ThinkingLevel;
use pir::tui::slash::{resolve_model_argument, ModelMatch};

/// Providers/ids covering the three match shapes: a plain reference, a
/// bare id on a second provider, and an id whose own colon is not a
/// thinking level.
fn catalog() -> Vec<(String, String)> {
    [
        ("anthropic", "claude-sonnet-4-5"),
        ("openai", "gpt-5"),
        ("openai", "gpt-5-mini"),
        ("openrouter", "qwen/qwen3-coder:exacto"),
        ("zai", "glm-5"),
    ]
    .into_iter()
    .map(|(provider, id)| (provider.to_string(), id.to_string()))
    .collect()
}

fn found(provider: &str, model_id: &str, thinking_level: Option<ThinkingLevel>) -> ModelMatch {
    ModelMatch::Found {
        provider: provider.to_string(),
        model_id: model_id.to_string(),
        thinking_level,
    }
}

#[test]
fn model_argument_matches_a_plain_reference_without_a_level() {
    let models = catalog();
    assert_eq!(
        resolve_model_argument("anthropic/claude-sonnet-4-5", &models),
        found("anthropic", "claude-sonnet-4-5", None)
    );
    // A bare id (case-insensitively) still matches, with no level.
    assert_eq!(
        resolve_model_argument("GLM-5", &models),
        found("zai", "glm-5", None)
    );
}

#[test]
fn model_argument_splits_a_trailing_thinking_level() {
    let models = catalog();
    assert_eq!(
        resolve_model_argument("anthropic/claude-sonnet-4-5:high", &models),
        found("anthropic", "claude-sonnet-4-5", Some(ThinkingLevel::High))
    );
    // The split happens on a bare id too, and covers the whole ladder.
    assert_eq!(
        resolve_model_argument("glm-5:xhigh", &models),
        found("zai", "glm-5", Some(ThinkingLevel::XHigh))
    );
}

#[test]
fn model_argument_keeps_an_unknown_suffix_literal() {
    let models = catalog();
    // `exacto` is not a thinking level, so the colon stays part of the id.
    assert_eq!(
        resolve_model_argument("openrouter/qwen/qwen3-coder:exacto", &models),
        found("openrouter", "qwen/qwen3-coder:exacto", None)
    );
    // An unparsable suffix never rescues an otherwise-unknown reference.
    assert_eq!(
        resolve_model_argument("gpt-5:turbo", &models),
        ModelMatch::NotFound
    );
}

#[test]
fn model_argument_keeps_ambiguity_and_not_found() {
    let models = catalog();
    assert_eq!(
        resolve_model_argument("gpt-5-mini", &models),
        found("openai", "gpt-5-mini", None)
    );
    assert_eq!(resolve_model_argument("", &models), ModelMatch::NotFound);
    assert_eq!(
        resolve_model_argument("nope:nope", &models),
        ModelMatch::NotFound
    );

    // Two providers serving the same id and no `/` in the term: still
    // ambiguous, and a thinking suffix does not change the match set.
    let mut duplicated = catalog();
    duplicated.push(("anthropic".to_string(), "glm-5".to_string()));
    let expected = ModelMatch::Ambiguous(vec![
        ("zai".to_string(), "glm-5".to_string()),
        ("anthropic".to_string(), "glm-5".to_string()),
    ]);
    assert_eq!(resolve_model_argument("glm-5", &duplicated), expected);
    assert_eq!(resolve_model_argument("glm-5:max", &duplicated), expected);
}
