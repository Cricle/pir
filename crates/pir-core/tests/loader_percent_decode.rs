//! `%XX` decoding in the module loader: malformed escapes must stay literal
//! rather than panic inside the QuickJS loader callback.
//!
//! Moved out of `src/extensions/loader.rs` — unit tests do not live in `src/`.

use pir_core::extensions::loader::percent_decode;

#[test]
fn malformed_percent_escapes_stay_literal_without_panicking() {
    // `%` followed by a multi-byte character: `str` slice boundaries used
    // to land inside the character and panic inside the QuickJS loader
    // callback, aborting the host.
    assert_eq!(percent_decode("file:///%€"), "file:///%€");
    assert_eq!(percent_decode("file:///%中"), "file:///%中");
    // Truncated escapes are kept verbatim, like Node's URL parser.
    assert_eq!(percent_decode("%"), "%");
    assert_eq!(percent_decode("%2"), "%2");
    assert_eq!(percent_decode("100%"), "100%");
}

#[test]
fn valid_percent_escapes_decode() {
    assert_eq!(percent_decode("a%20b"), "a b");
    assert_eq!(percent_decode("file:///%E4%B8%AD"), "file:///中");
}
