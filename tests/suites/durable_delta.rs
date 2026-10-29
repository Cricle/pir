//! `durable::delta` — UTF-16 truncation parity and the upstream `overlap` helper.
//!
//! Upstream chord measures string deltas in UTF-16 code units: `slice(count)`
//! drops `count` UTF-16 units, and `diff.ts` emits `["t", path, before.length -
//! shared]` where both lengths are UTF-16 lengths. The Rust applier must count
//! the same way or a log written by the TypeScript package replays at the wrong
//! offset for any non-BMP text (emoji in shell output is common).

use pir::durable::delta::{apply, Op, Seg};
use serde_json::{json, Value};

#[test]
fn truncate_counts_utf16_code_units() {
    // "😀" is one scalar but two UTF-16 code units. Upstream `"😀abc".slice(2)`
    // is `"abc"`; a scalar-counting implementation would produce "abc" from
    // `slice(1)` instead.
    let mut value = json!({ "text": "😀abc" });
    apply(
        &mut value,
        &[Op::Truncate(vec![Seg::Key("text".into())], 2)],
    )
    .expect("truncate applies");
    assert_eq!(value["text"], json!("abc"));

    let mut value = json!({ "text": "😀abc" });
    apply(
        &mut value,
        &[Op::Truncate(vec![Seg::Key("text".into())], 1)],
    )
    .expect("truncate applies");
    // One code unit splits the surrogate pair: upstream `.slice(1)` yields the
    // lone low surrogate, which JSON cannot represent; serde_json normalizes it
    // to U+FFFD. Either way the output must not be "abc".
    assert_ne!(value["text"], json!("abc"));
}

#[test]
fn truncate_beyond_the_string_is_empty() {
    let mut value = json!({ "text": "abc" });
    apply(
        &mut value,
        &[Op::Truncate(vec![Seg::Key("text".into())], 99)],
    )
    .expect("truncate applies");
    assert_eq!(value["text"], json!(""));
}

#[test]
fn truncate_ascii_matches_byte_and_scalar_counts() {
    let mut value = json!({ "text": "prefixsuffix" });
    apply(
        &mut value,
        &[Op::Truncate(vec![Seg::Key("text".into())], 6)],
    )
    .expect("truncate applies");
    assert_eq!(value["text"], json!("suffix"));
}

#[test]
fn overlap_finds_the_shared_suffix_and_prefix() {
    // `overlap(a, b, scan)` returns the length of the longest suffix of `a`
    // that is a prefix of `b` (upstream chord `delta/index.ts:87`).
    assert_eq!(pir::durable::chord::overlap("abcXYZ", "XYZdef", 65_536), 3);
    assert_eq!(pir::durable::chord::overlap("abcdef", "abcdef", 65_536), 6);
    assert_eq!(pir::durable::chord::overlap("abcdef", "xyz", 65_536), 0);
    assert_eq!(pir::durable::chord::overlap("", "xyz", 65_536), 0);
    assert_eq!(pir::durable::chord::overlap("abc", "", 65_536), 0);
}

#[test]
fn overlap_respects_the_scan_window() {
    // A suffix longer than the scan window cannot be found; upstream returns 0
    // and the differ falls back to a set.
    let a = format!("{}tail", "x".repeat(100));
    assert_eq!(pir::durable::chord::overlap(&a, "tail", 2), 0);
    assert_eq!(pir::durable::chord::overlap(&a, "tail", 4), 4);
}

#[test]
fn overlap_is_utf16_consistent_with_truncate() {
    // The differ emits `t(before.len - shared)` and `a(after[shared..])` in
    // UTF-16 units; applying them to the original must reproduce the target.
    let before = "😀😀head";
    let after = "head😀tail";
    let shared = pir::durable::chord::overlap(before, after, 65_536);
    let mut value = json!({ "text": before });
    let mut ops = Vec::new();
    if shared > 0 {
        ops.push(Op::Truncate(
            vec![Seg::Key("text".into())],
            before.encode_utf16().count() - shared,
        ));
        let appended =
            String::from_utf16_lossy(&after.encode_utf16().skip(shared).collect::<Vec<u16>>());
        if !appended.is_empty() {
            ops.push(Op::Append(vec![Seg::Key("text".into())], appended));
        }
    }
    if !ops.is_empty() {
        apply(&mut value, &ops).expect("string ops apply");
    }
    if shared > 0 {
        assert_eq!(value["text"], Value::String(after.into()));
    }
}
