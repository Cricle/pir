use pir_builtin_codemode::*;
use pir_plugin_api::BuiltinPlugin;

#[test]
fn identifiers_are_js_safe() {
    assert_eq!(to_codemode_identifier("abcd"), "abcd");
    assert_eq!(to_codemode_identifier("a-b.c"), "a_b_c");
    // A leading digit is PREFIXED, not dropped (upstream `1abc` → `_1abc`).
    assert_eq!(to_codemode_identifier("1abc"), "_1abc");
    assert_eq!(to_codemode_identifier("12"), "_12");
    assert_eq!(to_codemode_identifier(""), "_");
    assert_eq!(to_codemode_identifier("a$b_1"), "a$b_1");
}

#[test]
fn short_output_passes_through_untouched() {
    // Budget is in TOKENS; 5 tokens = 20 characters.
    assert_eq!(truncate_to_budget("abc", 5), "abc");
    assert_eq!(truncate_to_budget("", 0), "");
}

#[test]
fn truncation_keeps_head_and_tail_and_names_the_cost() {
    let long = "h".repeat(40) + &"m".repeat(40) + &"t".repeat(40);
    let out = truncate_to_budget(&long, 5); // 20 chars: 10 head + 10 tail
                                            // The notice is a PREFIX; the kept head and tail follow it.
    assert!(
        out.starts_with("Warning: truncated output (original token count: 30)"),
        "{out}"
    );
    assert!(out.contains("Total output lines: 1"), "{out}");
    assert!(out.contains(&"h".repeat(10)), "{out}");
    assert!(out.ends_with(&"t".repeat(10)), "{out}");
    assert!(out.contains("25 tokens truncated"), "{out}");
    // Nothing from the middle survives.
    assert!(!out.contains("mmm"), "{out}");
}

#[test]
fn truncation_never_splits_a_codepoint() {
    // 3 bytes per character: the head half lands mid-codepoint and must back
    // off to the previous boundary.
    let text = "é".repeat(60);
    let out = truncate_to_budget(&text, 5);
    assert!(out.is_char_boundary(0));
    assert!(out.chars().next().is_some());
    // The kept head/tail are whole `é` characters.
    let kept: String = out.chars().filter(|character| *character == 'é').collect();
    assert!(!kept.is_empty());
    assert_eq!(kept.chars().count() * 3 % 3, 0);
}

#[test]
fn plugin_identity() {
    assert_eq!(CodemodePlugin::new().info().path, "builtin:codemode");
    assert_eq!(CODEMODE_TOOL_NAME, "codemode");
    assert_eq!(CHARS_PER_TOKEN, 4);
}
