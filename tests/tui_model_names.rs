//! Model-name display rule (upstream `getDistinctModelName`,
//! model-search.ts:13-28): `/model` and `/scoped-models` show a name in
//! parentheses only when it adds words the id lacks.

use pir::tui::selector::get_distinct_model_name;

#[test]
fn a_renamed_override_adds_information() {
    assert_eq!(
        get_distinct_model_name("minimax/minimax-m3", "MiniMax M3 [parasail]"),
        Some("MiniMax M3 [parasail]".to_owned())
    );
    assert_eq!(
        get_distinct_model_name("claude-sonnet-4", "Claude Sonnet 4.5"),
        Some("Claude Sonnet 4.5".to_owned())
    );
    // Single-character words must match a whole id token: "4" is not covered
    // by "mistral-small-2603".
    assert_eq!(
        get_distinct_model_name("mistral-small-2603", "Mistral Small 4"),
        Some("Mistral Small 4".to_owned())
    );
}

#[test]
fn names_that_only_restate_the_id_stay_hidden() {
    // Different casing and separators.
    assert_eq!(get_distinct_model_name("gpt-5", "GPT 5"), None);
    // A dropped namespace/version suffix.
    assert_eq!(
        get_distinct_model_name(
            "eu.anthropic.claude-sonnet-4-5-20250929-v1:0",
            "Claude Sonnet 4.5 (EU)"
        ),
        None
    );
    assert_eq!(
        get_distinct_model_name("kimi-k3.1-pro", "Kimi K3.1 Pro"),
        None
    );
    assert_eq!(get_distinct_model_name("gpt-5", ""), None);
}
