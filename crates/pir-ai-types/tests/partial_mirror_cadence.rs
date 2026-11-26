//! Mirror cadence: when a streaming mirror should re-parse the accumulated JSON.
//!
//! Moved out of `src/partial_mirror.rs` — unit tests do not live in `src/`.

use pir_ai_types::partial_mirror::{MirrorCadence, MIRROR_REFRESH_BYTES};

/// Number of times the caller would run a whole-document parse: every
/// `record()` that reports a refresh.
fn count_refreshes(fragments: &[&str]) -> usize {
    let mut cadence = MirrorCadence::new();
    fragments
        .iter()
        .filter(|fragment| cadence.record(*fragment))
        .count()
}

fn split_every(text: &str, step: usize) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut start = 0;
    while start < text.len() {
        let mut end = (start + step).min(text.len());
        while end < text.len() && !text.is_char_boundary(end) {
            end += 1;
        }
        parts.push(&text[start..end]);
        start = end;
    }
    parts
}

/// Split immediately after every structural `}`/`]` outside strings. This
/// is the adversarial fragmentation for the old completion rule: almost
/// every fragment ends in a closer, so the old implementation re-parsed
/// the whole accumulated document once per fragment.
fn split_after_structural_closers(text: &str) -> Vec<&str> {
    let bytes = text.as_bytes();
    let mut parts = Vec::new();
    let mut start = 0;
    let mut in_string = false;
    let mut escaped = false;
    for (index, &byte) in bytes.iter().enumerate() {
        if in_string {
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                in_string = false;
            }
            continue;
        }
        if byte == b'"' {
            in_string = true;
        } else if byte == b'}' || byte == b']' {
            parts.push(&text[start..=index]);
            start = index + 1;
        }
    }
    if start < text.len() {
        parts.push(&text[start..]);
    }
    parts
}

fn nested_arguments(items: usize) -> String {
    let items: Vec<serde_json::Value> = (0..items)
        .map(|index| {
            serde_json::json!({
                "index": index,
                "path": format!("src/module_{index}.rs"),
                "meta": {
                    "before": [1, 2, 3],
                    "after": {
                        "lines": [format!(
                            "brace }} bracket ] quote \" slash \\ {index}"
                        )],
                    },
                },
                "nested": {"deeper": {"flag": true}},
            })
        })
        .collect();
    serde_json::to_string(&serde_json::json!({ "items": items })).expect("fixture serializes")
}

/// The pre-fix `MirrorCadence`: any `}`/`]` outside a string could end a
/// fragment and request a refresh, even when nested containers were still
/// open. Kept only for regression comparisons in tests.
#[derive(Default)]
struct LegacyCadence {
    appended_since_refresh: usize,
    refreshed: bool,
    in_string: bool,
    escaped: bool,
    depth: usize,
}

impl LegacyCadence {
    fn record(&mut self, fragment: &str) -> bool {
        if fragment.is_empty() {
            return false;
        }
        self.appended_since_refresh += fragment.len();
        let mut closed_top_level = false;
        let mut last_structural_close = false;
        for byte in fragment.bytes() {
            if self.in_string {
                if self.escaped {
                    self.escaped = false;
                } else if byte == b'\\' {
                    self.escaped = true;
                } else if byte == b'"' {
                    self.in_string = false;
                    last_structural_close = false;
                }
                continue;
            }
            match byte {
                b' ' | b'\t' | b'\n' | b'\r' => {}
                b'"' => {
                    self.in_string = true;
                    last_structural_close = false;
                }
                b'{' | b'[' => {
                    self.depth = self.depth.saturating_add(1);
                    last_structural_close = false;
                }
                b'}' | b']' => {
                    if self.depth > 0 {
                        self.depth -= 1;
                        closed_top_level |= self.depth == 0;
                    }
                    last_structural_close = true;
                }
                _ => last_structural_close = false,
            }
        }
        if self.refreshed
            && !(closed_top_level || last_structural_close)
            && self.appended_since_refresh < MIRROR_REFRESH_BYTES
        {
            return false;
        }
        self.appended_since_refresh = 0;
        self.refreshed = true;
        true
    }
}

fn legacy_refresh_count(fragments: &[&str]) -> usize {
    let mut cadence = LegacyCadence::default();
    fragments
        .iter()
        .filter(|fragment| cadence.record(fragment))
        .count()
}

#[test]
fn only_the_root_close_completes_a_nested_stream() {
    let mut cadence = MirrorCadence::new();
    assert!(
        cadence.record(r#"{"a":{"b":[1,2,3"#),
        "first fragment refreshes"
    );
    assert!(
        !cadence.record("]"),
        "closing a nested array must not refresh"
    );
    assert!(
        !cadence.record("}"),
        "closing a nested object must not refresh"
    );
    assert!(cadence.record("}"), "closing the root object refreshes");
    assert_eq!(cadence.scan_state(), (0, false));
}

#[test]
fn nested_container_closes_stay_on_the_byte_cadence() {
    let document = nested_arguments(600);
    assert!(
        document.len() > MIRROR_REFRESH_BYTES * 8,
        "fixture must span several cadence windows (got {} bytes)",
        document.len()
    );
    let fragments = split_after_structural_closers(&document);
    assert!(
        fragments.len() > 1_000,
        "fixture must have many nested closes"
    );

    let refreshes = count_refreshes(&fragments);
    // First fragment + one refresh per MIRROR_REFRESH_BYTES window + the
    // final root close. This is linear in the input size and independent
    // of the number of nested containers.
    let linear_bound = 3 + document.len() / MIRROR_REFRESH_BYTES;
    assert!(
            refreshes <= linear_bound,
            "nested closes caused {refreshes} whole-document parses for {} bytes (bound {linear_bound})",
            document.len()
        );

    let legacy = legacy_refresh_count(&fragments);
    assert!(
        legacy > refreshes * 8,
        "the old completion rule should be clearly superlinear: legacy={legacy}, fixed={refreshes}"
    );
}

#[test]
fn braces_and_escapes_inside_strings_never_end_the_document() {
    let document = r#"{"path":"C:\\temp\\}","text":"closing } ] and opening { [ plus \"quote\" and \\ backslash","tail":{"k":[1,2]}}"#;
    assert!(serde_json::from_str::<serde_json::Value>(document).is_ok());

    // One-byte fragments split every escape sequence and every brace.
    let fragments = split_every(document, 1);
    let refreshes = count_refreshes(&fragments);
    assert_eq!(
        refreshes, 2,
        "only the first fragment and the root close may parse"
    );

    let mut cadence = MirrorCadence::new();
    for fragment in &fragments {
        cadence.record(*fragment);
    }
    assert_eq!(cadence.scan_state(), (0, false));
}

#[test]
fn truncated_json_does_not_complete_but_keeps_the_byte_cadence() {
    // Open object -> nested object -> array, then a string whose `\u`
    // escape is split across fragments and a `}` that is merely string
    // content. No completion may fire while the containers stay open.
    let mut cadence = MirrorCadence::new();
    assert!(cadence.record(r#"{"a":{"b":["#));
    assert!(!cadence.record(r#""unterminated \u00"#));
    assert!(
        !cadence.record("}"),
        "a closer inside a string is not structural"
    );
    assert!(!cadence.record(r#"12"#));
    // End the string literal; this is not a structural close either, but
    // it makes the following `]`/`}` real container closers.
    assert!(!cadence.record("\""));
    assert!(!cadence.record("]}"));
    assert!(cadence.record("}"));

    // A genuinely truncated stream keeps refreshing only at the byte
    // cadence; it never reports structural completion.
    let mut cadence = MirrorCadence::new();
    assert!(cadence.record(r#"{"tool":{"args":["#));
    let mut refreshes = 1;
    for _ in 0..20_000 {
        if cadence.record("1234567890") {
            refreshes += 1;
        }
    }
    assert_eq!(
        refreshes,
        1 + 200_000 / MIRROR_REFRESH_BYTES,
        "truncated streams must stay on the byte cadence"
    );
    assert_eq!(cadence.scan_state(), (3, false));
}
