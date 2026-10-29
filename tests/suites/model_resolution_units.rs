//! Unit tests for model resolution migrated out of
//! `src/ai/providers/resolution.rs` inline `#[cfg(test)]` tests: exact
//! reference matching, fuzzy pattern matching, thinking-level suffix parsing,
//! glob scoping, CLI fallback, and the initial-model priority order.

use std::collections::BTreeMap;

use pir::ai::providers::construction::model;
use pir::ai::providers::resolution::{
    find_exact_model_reference_match, find_initial_model, glob_matches, parse_model_pattern,
    resolve_cli_model, resolve_model_scope, try_match_model, InitialModelOptions, ScopedModel,
};
use pir::ai::types::{Model, ThinkingLevel};

fn catalog() -> Vec<Model> {
    vec![
        model(
            "anthropic",
            "claude-opus-4-8",
            "Claude Opus",
            "anthropic-messages",
            "https://api.anthropic.com",
            200_000,
            16_384,
            vec!["text", "image"],
        ),
        model(
            "anthropic",
            "claude-opus-4-8-20260101",
            "Claude Opus dated",
            "anthropic-messages",
            "https://api.anthropic.com",
            200_000,
            16_384,
            vec!["text"],
        ),
        model(
            "openai",
            "gpt-5.5",
            "GPT",
            "openai-completions",
            "https://api.openai.com/v1",
            128_000,
            16_384,
            vec!["text"],
        ),
    ]
}

fn always_auth(_: &str) -> bool {
    true
}

fn never_auth(_: &str) -> bool {
    false
}

#[test]
fn exact_reference_matches_canonical_and_bare_id() {
    let models = catalog();
    let by_canonical = find_exact_model_reference_match("anthropic/claude-opus-4-8", &models)
        .expect("canonical match");
    assert_eq!(by_canonical.id, "claude-opus-4-8");
    // A bare id that uniquely matches resolves directly.
    let bare = find_exact_model_reference_match("gpt-5.5", &models).expect("unique bare id");
    assert_eq!(bare.provider, "openai");
    // Ambiguous bare-id matches are rejected.
    let mut ambiguous = catalog();
    let mut dup = ambiguous[2].clone();
    dup.provider = "other".to_string();
    ambiguous.push(dup);
    assert!(find_exact_model_reference_match("gpt-5.5", &ambiguous).is_none());
}

#[test]
fn pattern_matching_prefers_aliases() {
    let models = catalog();
    let matched = try_match_model("opus", &models).expect("fuzzy match");
    // The alias (no date suffix) wins over the dated version.
    assert_eq!(matched.id, "claude-opus-4-8");
}

#[test]
fn parse_model_pattern_extracts_thinking_suffix() {
    let models = catalog();
    let parsed = parse_model_pattern("claude-opus-4-8:high", &models, false);
    assert_eq!(parsed.model.expect("model").id, "claude-opus-4-8");
    assert_eq!(parsed.thinking_level, Some(ThinkingLevel::High));
}

#[test]
fn resolve_model_scope_supports_globs() {
    let models = catalog();
    let (scoped, diagnostics) = resolve_model_scope(&["anthropic/*".to_string()], &models);
    assert_eq!(scoped.len(), 2);
    assert!(diagnostics.is_empty());
    let (scoped, diagnostics) = resolve_model_scope(&["nomatch*".to_string()], &models);
    assert!(scoped.is_empty());
    assert_eq!(diagnostics.len(), 1);
}

#[test]
fn cli_model_builds_fallback_for_unknown_ids() {
    let models = catalog();
    let (resolved, thinking, warning) =
        resolve_cli_model(Some("anthropic"), "made-up-model", &models).expect("fallback");
    let fallback = resolved.expect("model");
    assert_eq!(fallback.id, "made-up-model");
    assert_eq!(fallback.provider, "anthropic");
    assert!(warning.is_some());
    assert!(thinking.is_none());
    // Unknown provider errors.
    assert!(resolve_cli_model(Some("nope"), "made-up-model", &models).is_err());
}

#[test]
fn initial_model_follows_the_upstream_order() {
    let models = catalog();
    let empty_scope: Vec<ScopedModel> = Vec::new();
    let empty_levels = BTreeMap::new();

    // 1. CLI args win.
    let result = find_initial_model(InitialModelOptions {
        cli_provider: Some("openai"),
        cli_model: Some("gpt-5.5"),
        cli_thinking: Some(ThinkingLevel::High),
        scoped_models: &empty_scope,
        models: &models,
        has_auth: &always_auth,
        ..InitialModelOptions::default()
    })
    .expect("resolve");
    assert_eq!(result.model.expect("model").provider, "openai");
    assert_eq!(result.thinking_level, ThinkingLevel::High);

    // 2. First scoped model.
    let (scoped, _) = resolve_model_scope(&["gpt-5.5:low".to_string()], &models);
    let result = find_initial_model(InitialModelOptions {
        scoped_models: &scoped,
        models: &models,
        has_auth: &always_auth,
        ..InitialModelOptions::default()
    })
    .expect("resolve");
    assert_eq!(result.model.expect("model").id, "gpt-5.5");
    assert_eq!(result.thinking_level, ThinkingLevel::Low);

    // 3. Settings defaults require auth.
    let result = find_initial_model(InitialModelOptions {
        default_provider: Some("anthropic"),
        default_model_id: Some("claude-opus-4-8"),
        default_thinking_level: Some(ThinkingLevel::Low),
        scoped_models: &empty_scope,
        models: &models,
        has_auth: &never_auth,
        ..InitialModelOptions::default()
    })
    .expect("resolve");
    assert!(result.model.is_none());

    // 4. Known-provider default id (anthropic -> claude-opus-4-8).
    let result = find_initial_model(InitialModelOptions {
        scoped_models: &empty_scope,
        models: &models,
        has_auth: &always_auth,
        model_thinking_levels: &empty_levels,
        ..InitialModelOptions::default()
    })
    .expect("resolve");
    assert_eq!(result.model.expect("model").id, "claude-opus-4-8");
    assert_eq!(result.thinking_level, ThinkingLevel::Medium);
}

#[test]
fn per_model_thinking_levels_apply_to_defaults() {
    let models = catalog();
    let mut levels = BTreeMap::new();
    levels.insert("openai/gpt-5.5".to_string(), ThinkingLevel::XHigh);
    let empty_scope: Vec<ScopedModel> = Vec::new();
    let result = find_initial_model(InitialModelOptions {
        default_provider: Some("openai"),
        default_model_id: Some("gpt-5.5"),
        scoped_models: &empty_scope,
        model_thinking_levels: &levels,
        models: &models,
        has_auth: &always_auth,
        ..InitialModelOptions::default()
    })
    .expect("resolve");
    assert_eq!(result.thinking_level, ThinkingLevel::XHigh);
}

#[test]
fn glob_matches_patterns() {
    assert!(glob_matches("anthropic/*", "anthropic/claude"));
    assert!(glob_matches("*sonnet*", "anthropic/claude-sonnet-4"));
    assert!(glob_matches("mcp__srv__*", "mcp__srv__tool"));
    assert!(!glob_matches("a/*", "b/c"));
    assert!(glob_matches("a?c", "abc"));
}
