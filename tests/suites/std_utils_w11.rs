//! W11-H: crate-backed utilities. `semver` backs the update-check version
//! comparison (`src/tui/update_check.rs`) and `textwrap` backs the
//! printable-ASCII word-wrap path in `src/tui/ansi.rs`; everything with ANSI
//! state or grapheme-width semantics deliberately stays hand-rolled.
//!
//! The tables pin three things: semver precedence (including prerelease
//! identifiers and build metadata), byte-identical output for the wrapping
//! path that now delegates to `textwrap`, and unchanged output for the
//! CJK/combining/emoji/tab/ANSI paths that do not.

use std::cmp::Ordering;

use pir::tui::ansi::{strip_ansi, visible_width, wrap_text_with_ansi};
use pir::tui::update_check::{compare_versions, is_newer_version, parse_version};

// ---------------------------------------------------------------------------
// Version comparison (`semver` crate)
// ---------------------------------------------------------------------------

#[test]
fn compare_versions_orders_core_components_numerically() {
    assert_eq!(compare_versions("1.2.3", "1.2.3"), Some(Ordering::Equal));
    assert_eq!(compare_versions("1.10.0", "1.9.9"), Some(Ordering::Greater));
    assert_eq!(
        compare_versions("2.0.0", "1.99.99"),
        Some(Ordering::Greater)
    );
    assert_eq!(compare_versions("1.2.3", "1.2.4"), Some(Ordering::Less));
}

#[test]
fn compare_versions_orders_prereleases_per_the_semver_spec() {
    // Numeric identifiers compare numerically; the previous string comparison
    // ranked `beta.20` below `beta.9`.
    assert_eq!(
        compare_versions("5.0.0-beta.20", "5.0.0-beta.9"),
        Some(Ordering::Greater)
    );
    // The precedence chain from semver.org section 11.
    let chain = [
        "1.0.0-alpha",
        "1.0.0-alpha.1",
        "1.0.0-alpha.beta",
        "1.0.0-beta",
        "1.0.0-beta.2",
        "1.0.0-beta.11",
        "1.0.0-rc.1",
        "1.0.0",
    ];
    for pair in chain.windows(2) {
        assert_eq!(
            compare_versions(pair[0], pair[1]),
            Some(Ordering::Less),
            "{} should rank below {}",
            pair[0],
            pair[1]
        );
        assert_eq!(compare_versions(pair[1], pair[0]), Some(Ordering::Greater));
    }
}

#[test]
fn compare_versions_ignores_build_metadata() {
    assert_eq!(
        compare_versions("1.2.3+build.1", "1.2.3+build.2"),
        Some(Ordering::Equal)
    );
    assert_eq!(
        compare_versions("1.2.3+abc", "1.2.3"),
        Some(Ordering::Equal)
    );
    assert_eq!(
        compare_versions("1.2.3-rc.1+abc", "1.2.3-rc.1"),
        Some(Ordering::Equal)
    );
    assert!(!is_newer_version("1.2.3+build.2", "1.2.3+build.1"));
}

#[test]
fn compare_versions_accepts_upstream_lenience() {
    // node-semver's `valid()` trims and allows one leading lowercase `v`
    // (its FULL grammar is `^v?<major>.<minor>.<patch>...$`), which is what
    // upstream's `comparePackageVersions` feeds it.
    assert_eq!(compare_versions("v1.2.3", "1.2.3"), Some(Ordering::Equal));
    assert_eq!(
        compare_versions("  v2.1.0  ", "2.0.9"),
        Some(Ordering::Greater)
    );
    assert_eq!(
        compare_versions("v1.0.0-alpha.2", "1.0.0-alpha.10"),
        Some(Ordering::Less)
    );
    assert!(is_newer_version(" v1.3.0 ", "1.2.9"));
}

#[test]
fn compare_versions_rejects_what_upstream_rejects() {
    // Upstream `semver.valid` is strict: partial versions, a fourth numeric
    // component, leading zeros and free-form tags are not versions, so
    // compare_versions returns None and is_newer_version uses upstream's
    // trimmed-string fallback. (An uppercase `V` is not upstream's `v?`.)
    assert_eq!(compare_versions("1.2", "1.2.0"), None);
    assert_eq!(compare_versions("1", "1.0.0"), None);
    assert_eq!(compare_versions("1.2.3.4", "1.2.3.4"), None);
    assert_eq!(compare_versions("01.2.3", "1.2.3"), None);
    assert_eq!(compare_versions("nightly-42", "1.2.3"), None);
    assert_eq!(compare_versions("", "1.2.3"), None);
    assert_eq!(compare_versions("V1.2.3", "1.2.3"), None);

    assert!(is_newer_version("nightly-42", "1.2.3"));
    assert!(!is_newer_version(" 1.2.3 ", "1.2.3"));
    assert!(is_newer_version("1.2", "1.2.0"));
}

#[test]
fn parse_version_exposes_the_semver_crate_value() {
    assert_eq!(
        parse_version(" v1.2.3-beta.4+build.5 ").map(|version| version.to_string()),
        Some("1.2.3-beta.4+build.5".to_owned())
    );
    assert!(parse_version("1.2").is_none());
}

// ---------------------------------------------------------------------------
// Wrapping: the printable-ASCII path now delegates to `textwrap`
// ---------------------------------------------------------------------------

/// Reference: `textwrap` configured exactly like the production fast path.
fn textwrap_reference(text: &str, width: usize) -> Vec<String> {
    let options = textwrap::Options::new(width)
        .word_separator(textwrap::WordSeparator::AsciiSpace)
        .word_splitter(textwrap::WordSplitter::NoHyphenation)
        .wrap_algorithm(textwrap::WrapAlgorithm::FirstFit);
    textwrap::wrap(text, options)
        .into_iter()
        .map(|line| line.into_owned())
        .collect()
}

#[test]
fn plain_ascii_wrap_is_byte_identical_to_textwrap_first_fit() {
    // Enumerate every string over {a, b, space, -} of length 1..=5 against
    // widths 1..=8: the delegated path must match `textwrap` line for line,
    // including leading/trailing space runs and over-wide words.
    let alphabet = ['a', 'b', ' ', '-'];
    let mut cases: Vec<String> = vec![
        String::new(),
        "hello world".to_owned(),
        "the quick brown fox".to_owned(),
        "ab abcdefgh".to_owned(),
        "hello   world".to_owned(),
        "hello ".to_owned(),
        "   ab".to_owned(),
        "a-b-c-d-e".to_owned(),
    ];
    for len in 1..=5usize {
        let mut indices = vec![0usize; len];
        loop {
            cases.push(indices.iter().map(|&i| alphabet[i]).collect());
            let mut carry = true;
            for slot in indices.iter_mut().rev() {
                if *slot + 1 < alphabet.len() {
                    *slot += 1;
                    carry = false;
                    break;
                }
                *slot = 0;
            }
            if carry {
                break;
            }
        }
    }

    for text in &cases {
        for width in 1..=8usize {
            // Lines that fit return verbatim (trailing spaces included) from
            // the caller, before the delegated path; only wrapped lines are
            // compared here.
            if visible_width(text) <= width {
                continue;
            }
            assert_eq!(
                wrap_text_with_ansi(text, width),
                textwrap_reference(text, width),
                "text={text:?} width={width}"
            );
        }
    }
}

#[test]
fn wrap_plain_ascii_table() {
    let cases: &[(&str, usize, &[&str])] = &[
        ("hello world", 8, &["hello", "world"]),
        ("hello world", 6, &["hello", "world"]),
        ("the quick brown fox", 10, &["the quick", "brown fox"]),
        ("aaaaaaaaaaaa", 5, &["aaaaa", "aaaaa", "aa"]),
        ("ab abcdefgh", 4, &["ab", "abcd", "efgh"]),
        ("abc defghij", 5, &["abc", "defgh", "ij"]),
        ("hello   world", 8, &["hello", "world"]),
        ("ab  cd", 5, &["ab", "cd"]),
        ("a    b", 2, &["a", "b"]),
        ("   ab", 2, &["", "ab"]),
        ("a-b-c-d-e", 3, &["a-b", "-c-", "d-e"]),
        ("hello ", 5, &["hello"]),
        ("hello ", 10, &["hello "]),
        ("abc", 5, &["abc"]),
    ];
    for &(text, width, expected) in cases {
        assert_eq!(
            wrap_text_with_ansi(text, width),
            expected,
            "{text:?}/{width}"
        );
    }
    assert_eq!(wrap_text_with_ansi("", 10), vec![""]);
    assert_eq!(wrap_text_with_ansi("hello", 0), vec![""]);
}

// ---------------------------------------------------------------------------
// Wrapping: paths that deliberately keep the hand-rolled machinery
// ---------------------------------------------------------------------------

#[test]
fn wrap_keeps_grapheme_clusters_intact() {
    // textwrap measures per char and would split ZWJ sequences and combining
    // marks; the hand-rolled wrapper breaks only between UAX #29 clusters.
    assert_eq!(wrap_text_with_ansi("你好世界", 6), vec!["你好世", "界"]);
    assert_eq!(
        wrap_text_with_ansi("👨\u{200d}👩\u{200d}👧👨\u{200d}👩\u{200d}👧", 2),
        vec!["👨\u{200d}👩\u{200d}👧", "👨\u{200d}👩\u{200d}👧"]
    );
    assert_eq!(
        wrap_text_with_ansi("e\u{301}e\u{301}e", 1),
        vec!["e\u{301}", "e\u{301}", "e"]
    );
}

#[test]
fn wrap_keeps_tab_width_semantics() {
    // A tab is three cells here; textwrap's unicode-width measurement gives
    // it zero and would not treat it as a word separator.
    assert_eq!(visible_width("a\tb"), 5);
    assert_eq!(wrap_text_with_ansi("a\tb", 3), vec!["a", "b"]);
}

#[test]
fn wrap_keeps_ansi_state_and_osc8_links() {
    let wrapped = wrap_text_with_ansi("\x1b[31mhello world\x1b[39m", 8);
    assert_eq!(wrapped, vec!["\x1b[31mhello", "\x1b[31mworld\x1b[39m"]);

    // OSC 8 hyperlink: each line closes and re-opens the link with the same
    // BEL terminator (textwrap knows nothing about SGR/OSC-8 replay).
    let linked = wrap_text_with_ansi("\x1b]8;;u\x07abcd\x1b]8;;\x07", 2);
    assert_eq!(
        linked,
        vec!["\x1b]8;;u\x07ab\x1b]8;;\x07", "\x1b]8;;u\x07cd\x1b]8;;\x07"]
    );
    assert_eq!(strip_ansi(&linked.join("")), "abcd");
}

#[test]
fn wrap_keeps_multiline_input_splitting() {
    assert_eq!(wrap_text_with_ansi("one\ntwo", 10), vec!["one", "two"]);
    assert_eq!(
        wrap_text_with_ansi("one\n\ntwo", 10),
        vec!["one", "", "two"]
    );
}
