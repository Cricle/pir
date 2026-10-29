//! Glob matching over `globset` (lane W11-G).
//!
//! Covers the behaviour the removed hand-rolled MODEL-scope matcher was pinned
//! to (ported from `model_resolution_units.rs::glob_matches_patterns`), plus
//! the syntax the replacement adds — `**`, escapes, character classes — the
//! documented case-insensitivity, the model-scope integration (pattern against
//! `provider/model` *and* the bare id), and a many-star regression that must
//! finish without backtracking cost.
//!
//! This is model-scope syntax, NOT the tool-selection language: tool names are
//! matched by `coding_agent::tool_selection::tool_pattern_matches` with
//! upstream `toolPatternRegExp` semantics — only `*` is a wildcard, `?` is
//! literal, and matching is case-sensitive. The last test of section 1 pins
//! those rules through the public `resolve_tool_selection`.

use pir::ai::providers::construction::model;
use pir::ai::providers::resolution::{glob_matches, resolve_model_scope, CompiledGlob};
use pir::ai::types::Model;
use pir::coding_agent::resolve_tool_selection;

fn catalog() -> Vec<Model> {
    vec![
        model(
            "anthropic",
            "claude-sonnet-4-5",
            "Claude Sonnet",
            "anthropic-messages",
            "https://api.anthropic.com",
            200_000,
            16_384,
            vec!["text"],
        ),
        model(
            "anthropic",
            "claude-opus-4-8",
            "Claude Opus",
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
        // A bracketed id: `[1m]` would otherwise parse as a character class.
        model(
            "custom",
            "bracketed-model[1m]",
            "Bracketed",
            "openai-completions",
            "https://example.invalid/v1",
            128_000,
            16_384,
            vec!["text"],
        ),
    ]
}

// ---------------------------------------------------------------------------
// 1. Cases the old model-scope matcher was pinned to (ported)
// ---------------------------------------------------------------------------

#[test]
fn ported_model_scope_patterns() {
    // model_resolution_units.rs::glob_matches_patterns
    assert!(glob_matches("anthropic/*", "anthropic/claude"));
    assert!(glob_matches("*sonnet*", "anthropic/claude-sonnet-4"));
    assert!(glob_matches("mcp__srv__*", "mcp__srv__tool"));
    assert!(!glob_matches("a/*", "b/c"));
    assert!(glob_matches("a?c", "abc"));
}

#[test]
fn generic_glob_helper_star_and_question_cases() {
    // Generic `glob_matches` syntax only. The names look like tools, but these
    // assertions do NOT describe tool-selection semantics (which are
    // `*`-only, `?`-literal, case-sensitive — see the next test).
    assert!(glob_matches("read*", "read_many_files"));
    assert!(glob_matches("*", "anything"));
    assert!(glob_matches("mcp__*", "mcp__docs_search"));
    assert!(!glob_matches("read*", "write"));
    assert!(glob_matches("r?ad", "read"));
}

#[test]
fn tool_selection_patterns_are_star_only_and_case_sensitive() {
    // The public resolver behind every host's `--tools`: upstream
    // `toolPatternRegExp` semantics, NOT `glob_matches` — `*` is the only
    // wildcard, `?` is literal, and matching is case-sensitive.
    let available: Vec<String> = ["read", "read_many_files", "write", "mcp__docs_search"]
        .iter()
        .map(|name| (*name).to_string())
        .collect();
    let resolve = |pattern: &str| {
        resolve_tool_selection(
            None,
            &[pattern.to_string()],
            &[],
            false,
            false,
            &available,
            &["read"],
        )
        .expect("selection resolves")
        .expect("a plain --tools entry changes the selection")
        .enabled
    };
    assert_eq!(resolve("r*"), vec!["read", "read_many_files"]);
    assert!(resolve("r?ad").is_empty(), "`?` is literal in tool names");
    assert!(
        resolve("READ").is_empty(),
        "tool matching is case-sensitive"
    );
}

#[test]
fn star_crosses_a_separator() {
    // `literal_separator(false)`: `*` keeps matching `/`, as the old matcher
    // did. One pattern can therefore cover `provider/model` and a bare id.
    assert!(glob_matches("anthropic*", "anthropic/claude-sonnet-4-5"));
    assert!(glob_matches("*", "anthropic/claude"));
    assert!(glob_matches("*/*", "anthropic/claude"));
}

#[test]
fn empty_and_exact_patterns() {
    assert!(glob_matches("", ""));
    assert!(!glob_matches("", "x"));
    assert!(glob_matches("read", "read"));
    assert!(!glob_matches("read", "reads"));
    // No anchors: the whole string must match.
    assert!(!glob_matches("rea", "read"));
}

// ---------------------------------------------------------------------------
// 2. New syntax: `**`, escapes, classes, alternates
// ---------------------------------------------------------------------------

#[test]
fn recursive_double_star() {
    // Intentional widening over the old matcher, which collapsed `**` to `*`
    // (so `a/**/b` needed at least one segment in the middle).
    assert!(glob_matches("a/**/b", "a/b"));
    assert!(glob_matches("a/**/b", "a/x/b"));
    assert!(glob_matches("a/**/b", "a/x/y/b"));
    assert!(!glob_matches("a/**/b", "a/x"));
    // Leading recursive prefix matches a bare name.
    assert!(glob_matches("**/b", "b"));
    assert!(glob_matches("**/b", "x/y/b"));
    // A lone `**` matches everything, separators included.
    assert!(glob_matches("**", ""));
    assert!(glob_matches("**", "a/b/c"));
    // A recursive suffix keeps one separator, like minimatch.
    assert!(glob_matches("a/**", "a/b"));
    assert!(!glob_matches("a/**", "a"));
    // `**` mid-segment is two plain stars.
    assert!(glob_matches("a**b", "a/x/b"));
}

#[test]
fn backslash_escapes_wildcards() {
    assert!(glob_matches(r"a\*b", "a*b"));
    assert!(!glob_matches(r"a\*b", "axb"));
    assert!(glob_matches(r"a\?b", "a?b"));
    assert!(!glob_matches(r"a\?b", "axb"));
    // `\` is forced to escape on every platform, so a pattern means the same
    // thing on Windows and Unix.
    assert!(glob_matches(r"\*", "*"));
    assert!(!glob_matches(r"\*", "x"));
}

#[test]
fn character_classes_and_alternates() {
    assert!(glob_matches("[abc]at", "bat"));
    assert!(!glob_matches("[abc]at", "dat"));
    assert!(glob_matches("[a-c]at", "cat"));
    assert!(!glob_matches("[a-c]at", "eat"));
    // Negated classes (both `!` and `^`, as minimatch/globset accept).
    assert!(glob_matches("[!a]bc", "xbc"));
    assert!(!glob_matches("[!a]bc", "abc"));
    assert!(glob_matches("[^a]bc", "xbc"));
    // Alternates.
    assert!(glob_matches("{read,write}", "write"));
    assert!(!glob_matches("{read,write}", "edit"));
    // A closed class is a class, not a literal — which is exactly why
    // `resolve_model_scope` exact-resolves ids like `model[1m]` first.
    assert!(glob_matches("model[1m]", "model1"));
    assert!(!glob_matches("model[1m]", "model[1m]"));
}

#[test]
fn unclosed_class_and_invalid_patterns_stay_literal() {
    // Unclosed `[` stays literal (`allow_unclosed_class`), matching the old
    // matcher's treatment of every bracket.
    assert!(glob_matches("bracketed-model[1m", "bracketed-model[1m"));
    assert!(!glob_matches("bracketed-model[1m", "bracketed-model1"));
    // A pattern globset rejects (dangling escape, unopened `{`) falls back to
    // a literal comparison instead of panicking or matching nothing.
    assert!(glob_matches(r"a\", r"a\"));
    assert!(!glob_matches(r"a\", "ab"));
    assert!(glob_matches("a}", "a}"));
}

// ---------------------------------------------------------------------------
// 3. Case sensitivity
// ---------------------------------------------------------------------------

#[test]
fn matching_is_case_insensitive() {
    assert!(glob_matches("Anthropic/*", "anthropic/claude"));
    assert!(glob_matches("anthropic/*", "Anthropic/Claude"));
    assert!(glob_matches("*SONNET*", "anthropic/claude-sonnet-4"));
    assert!(glob_matches("READ*", "read_many_files"));
    assert!(glob_matches("[A-C]at", "bat"));
    // Case-insensitivity does not turn the pattern into a substring match.
    assert!(!glob_matches("SONNET", "claude-sonnet-4"));
}

// ---------------------------------------------------------------------------
// 4. Model-scope integration (pair vs bare id)
// ---------------------------------------------------------------------------

#[test]
fn scope_matches_provider_pair_and_bare_id() {
    let models = catalog();
    // `anthropic/*` matches the `provider/id` pair.
    let (scoped, diagnostics) = resolve_model_scope(&["anthropic/*".to_string()], &models);
    assert_eq!(scoped.len(), 2, "both anthropic models");
    assert!(diagnostics.is_empty());

    // `*sonnet*` matches through the bare id as well.
    let (scoped, _) = resolve_model_scope(&["*sonnet*".to_string()], &models);
    assert_eq!(scoped.len(), 1);
    assert_eq!(scoped[0].model.id, "claude-sonnet-4-5");

    // A recursive pattern finds nested gateway-style ids.
    let (scoped, _) = resolve_model_scope(&["openai/**".to_string()], &models);
    assert_eq!(scoped.len(), 1);
    assert_eq!(scoped[0].model.id, "gpt-5.5");

    // Case-insensitive at the scope level too.
    let (scoped, _) = resolve_model_scope(&["ANTHROPIC/CLAUDE-OPUS*".to_string()], &models);
    assert_eq!(scoped.len(), 1);
    assert_eq!(scoped[0].model.id, "claude-opus-4-8");
}

#[test]
fn scope_exact_resolves_bracketed_ids_before_globbing() {
    let models = catalog();
    // `[1m]` is not a glob: the exact reference wins and no diagnostics fire.
    let (scoped, diagnostics) = resolve_model_scope(&["bracketed-model[1m]".to_string()], &models);
    assert_eq!(scoped.len(), 1);
    assert_eq!(scoped[0].model.id, "bracketed-model[1m]");
    assert!(diagnostics.is_empty());
}

#[test]
fn scope_reports_no_match_once() {
    let models = catalog();
    let (scoped, diagnostics) = resolve_model_scope(&["nomatch*".to_string()], &models);
    assert!(scoped.is_empty());
    assert_eq!(diagnostics.len(), 1);
}

// ---------------------------------------------------------------------------
// 5. Regression: no exponential backtracking, and one compile per scan
// ---------------------------------------------------------------------------

#[test]
fn many_star_pattern_is_linear() {
    // The shape that made a recursive backtracking matcher exponential
    // (`*a*a*a...*`): the compiled regex engine evaluates it in one pass.
    // A direct assert is the regression check — hanging here is the failure
    // signal.
    let pattern = format!("{}b", "*a".repeat(32));
    // A 4 KiB text of only `a`s cannot end in `b`.
    let text = "a".repeat(4_096);
    assert!(!glob_matches(&pattern, &text));
    // The same pattern must still match the text it describes, so the test
    // cannot pass by always returning false.
    assert!(glob_matches(&pattern, &format!("{text}b")));
    // Not a match either when the text is long and ends in `b`s: the pattern
    // ends `a` then `b`, so the final `ab` must be present.
    let mixed = "a".repeat(4_000) + &"b".repeat(96);
    assert!(!glob_matches(&pattern, &mixed));
}

#[test]
fn compiled_glob_is_reusable() {
    let glob = CompiledGlob::new("mcp__*");
    assert!(glob.is_match("mcp__docs_search"));
    assert!(glob.is_match("mcp__docs_fetch"));
    assert!(!glob.is_match("read"));
    // Case-insensitive, like the wrapper.
    let glob = CompiledGlob::new("READ*");
    assert!(glob.is_match("read_many_files"));
}
