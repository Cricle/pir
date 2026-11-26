//! Fuzzy-match parity: the ASCII byte fast path must score bit-for-bit like
//! the Unicode char path, including for whitespace that only one path
//! considers a separator.
//!
//! Moved out of `src/fuzzy.rs` — unit tests do not live in `src/`.

use pir_tui::fuzzy::{fuzzy_match, fuzzy_match_prelowered, fuzzy_match_unicode};

/// The ASCII byte fast path must be bit-for-bit equivalent to the original
/// char path (`to_lowercase` + `match_query`); non-ASCII input keeps the
/// Unicode path and stays correct.
#[test]
fn ascii_fast_path_matches_unicode_path_scores() {
    let cases = [
        ("", "anything"),
        ("abc", "abc"),
        ("ABC", "abc"),
        ("abc", "xxabcxx"),
        ("fb", "foobar"),
        ("fz", "foobar"),
        ("f", "foo_bar"),
        ("_", "foo_bar"),
        ("ab", "abc"),
        ("ac", "abc"),
        ("foo", "foo"),
        ("foo", "foobar"),
        ("FOO", "foobar"),
        ("ab12", "foobar 12ab"),
        ("12ab", "foobar 12ab"),
        ("rpln", "Release Planning"),
        ("WRSP", "C:\\Work\\Rust_Projects"),
        ("abcdef", "abc"),
        ("z", "abc"),
        ("a-b", "a-b-c"),
        // Vertical tab is ASCII whitespace for `char::is_whitespace` but
        // not for `u8::is_ascii_whitespace`; the fast path must not drift.
        ("f", "\x0Bfoo"),
        ("a", "foo\x0Bbar"),
        ("a b", "a\x0Bb"),
        ("über", "Ärger Über"),
        ("ÜB", "ärger über"),
        ("ärger", "ÄRGER"),
        ("é", "café"),
        ("cafe", "café"),
        ("日本語", "これは日本語のテキスト"),
        ("本語", "日本語"),
    ];
    for (query, text) in cases {
        let expected = fuzzy_match_unicode(&query.to_lowercase(), &text.to_lowercase());
        let actual = fuzzy_match(query, text);
        assert_eq!(
            actual.matches, expected.matches,
            "match parity for {query:?} against {text:?}"
        );
        assert_eq!(
            actual.score, expected.score,
            "score parity for {query:?} against {text:?}"
        );
    }
}

#[test]
fn prelowered_query_matches_full_api_scores() {
    let cases = [
        ("abc", "ABC"),
        ("ü", "Ärger Über"),
        ("über", "ÄRGER ÜBER"),
        ("é", "CAFÉ"),
        ("ab12", "FOOBAR 12AB"),
        ("f", "FOO\x0BBAR"),
        ("x", "日本語"),
    ];
    for (query_lower, text) in cases {
        assert_eq!(
            fuzzy_match_prelowered(query_lower, text),
            fuzzy_match(query_lower, text),
            "prelowered parity for {query_lower:?} against {text:?}"
        );
    }
}

#[test]
fn ascii_fast_path_keeps_rank_order() {
    // Same assertions as the public API tests, exercised through the byte
    // path (all-ASCII inputs).
    assert!(fuzzy_match("abc", "abc").score < fuzzy_match("abc", "xabcx").score);
    assert!(fuzzy_match("fb", "foobar").score < fuzzy_match("fz", "foobar").score);
    let swapped = fuzzy_match("ab12", "foobar 12ab");
    let direct = fuzzy_match("12ab", "foobar 12ab");
    assert!(swapped.matches && direct.matches);
    assert!(swapped.score > direct.score);
}
