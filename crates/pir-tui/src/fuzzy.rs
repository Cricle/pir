//! Fuzzy matching — a direct Rust transcription of upstream
//! `packages/tui/src/fuzzy.ts`.
//!
//! Matches when all query characters appear in order (not necessarily
//! consecutively). Lower score = better match.

/// One match result (upstream `FuzzyMatch`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FuzzyMatch {
    pub matches: bool,
    pub score: f64,
}

impl FuzzyMatch {
    fn miss() -> Self {
        Self {
            matches: false,
            score: 0.0,
        }
    }
}

fn is_word_boundary(prev: char) -> bool {
    prev.is_whitespace() || matches!(prev, '-' | '_' | '.' | '/' | ':')
}

/// ASCII counterpart of [`is_word_boundary`]. `char::is_whitespace` on ASCII
/// includes the vertical tab (0x0B), which `u8::is_ascii_whitespace` omits, so
/// the byte path spells it out to keep scores bit-for-bit identical.
fn is_word_boundary_byte(prev: u8) -> bool {
    prev.is_ascii_whitespace()
        || prev == b'\x0B'
        || matches!(prev, b'-' | b'_' | b'.' | b'/' | b':')
}

/// Score one alignment (upstream `scorePositions`): consecutive matches are
/// rewarded, gaps penalized, word boundaries rewarded and later matches
/// slightly penalized.
fn score_positions(text: &[char], positions: &[usize]) -> f64 {
    let mut score = 0.0f64;
    let mut last_match_index: Option<usize> = None;
    let mut consecutive = 0i64;

    for &i in positions {
        match last_match_index {
            Some(prev) if prev + 1 == i => {
                consecutive += 1;
                score -= (consecutive * 5) as f64;
            }
            _ => {
                consecutive = 0;
                if let Some(prev) = last_match_index {
                    score += ((i - prev - 1) * 2) as f64;
                }
            }
        }
        if i == 0 || is_word_boundary(text[i - 1]) {
            score -= 10.0;
        }
        score += i as f64 * 0.1;
        last_match_index = Some(i);
    }

    score
}

/// The first index at or after `from` where `needle` occurs contiguously.
fn find_from(haystack: &[char], needle: &[char], from: usize) -> Option<usize> {
    if needle.len() > haystack.len() {
        return None;
    }
    (from..=haystack.len() - needle.len())
        .find(|&start| haystack[start..start + needle.len()] == *needle)
}

fn match_query(query: &[char], text: &[char]) -> FuzzyMatch {
    if query.is_empty() {
        return FuzzyMatch {
            matches: true,
            score: 0.0,
        };
    }
    if query.len() > text.len() {
        return FuzzyMatch::miss();
    }

    // Greedy alignment: each query character at its first occurrence after the
    // previous one (upstream `matchQuery`).
    let mut positions: Vec<usize> = Vec::with_capacity(query.len());
    let mut last_match_index: Option<usize> = None;
    for &wanted in query {
        let Some(i) = find_from(text, &[wanted], last_match_index.map_or(0, |prev| prev + 1))
        else {
            return FuzzyMatch::miss();
        };
        positions.push(i);
        last_match_index = Some(i);
    }

    let mut score = score_positions(text, &positions);

    // The greedy alignment can scatter the query over an earlier, unrelated
    // part of the text, e.g. "opus" in "openrouter openrouter/anthropic/claude-opus"
    // takes o, p, u from "openrouter". Also score every contiguous occurrence
    // and keep the best alignment.
    let mut start = find_from(text, query, 0);
    while let Some(offset) = start {
        for (k, slot) in positions.iter_mut().enumerate() {
            *slot = offset + k;
        }
        score = score.min(score_positions(text, &positions));
        start = find_from(text, query, offset + 1);
    }

    if query == text {
        score -= 100.0;
    }

    FuzzyMatch {
        matches: true,
        score,
    }
}

/// ASCII counterpart of [`score_positions`]: byte indices are char indices for
/// ASCII text and the boundary sets agree (see [`is_word_boundary_byte`]), so
/// the score is identical to the char implementation.
fn score_positions_bytes(text: &[u8], positions: &[usize]) -> f64 {
    let mut score = 0.0f64;
    let mut last_match_index: Option<usize> = None;
    let mut consecutive = 0i64;

    for &i in positions {
        match last_match_index {
            Some(prev) if prev + 1 == i => {
                consecutive += 1;
                score -= (consecutive * 5) as f64;
            }
            _ => {
                consecutive = 0;
                if let Some(prev) = last_match_index {
                    score += ((i - prev - 1) * 2) as f64;
                }
            }
        }
        if i == 0 || is_word_boundary_byte(text[i - 1]) {
            score -= 10.0;
        }
        score += i as f64 * 0.1;
        last_match_index = Some(i);
    }

    score
}

fn match_query_bytes(query: &[u8], text: &[u8]) -> FuzzyMatch {
    if query.is_empty() {
        return FuzzyMatch {
            matches: true,
            score: 0.0,
        };
    }
    if query.len() > text.len() {
        return FuzzyMatch::miss();
    }

    // Greedy alignment on bytes; `query` is already ASCII-lowercase and the
    // comparison folds `text` byte-by-byte, so no lowercase copy is needed.
    let mut positions: Vec<usize> = Vec::with_capacity(query.len());
    let mut last_match_index: Option<usize> = None;
    for &wanted in query {
        let from = last_match_index.map_or(0, |prev| prev + 1);
        let Some(found) = text[from..]
            .iter()
            .position(|candidate| candidate.eq_ignore_ascii_case(&wanted))
        else {
            return FuzzyMatch::miss();
        };
        let i = from + found;
        positions.push(i);
        last_match_index = Some(i);
    }

    let mut score = score_positions_bytes(text, &positions);

    // Score every contiguous occurrence too and keep the best alignment
    // (upstream `matchQuery`; see [`match_query`]).
    let mut start = 0usize;
    while start + query.len() <= text.len() {
        if text[start..start + query.len()].eq_ignore_ascii_case(query) {
            for (offset, slot) in positions.iter_mut().enumerate() {
                *slot = start + offset;
            }
            score = score.min(score_positions_bytes(text, &positions));
        }
        start += 1;
    }

    if text.eq_ignore_ascii_case(query) {
        score -= 100.0;
    }

    FuzzyMatch {
        matches: true,
        score,
    }
}

/// Swap the letters/digits runs of an all-alphanumeric lowercase query:
/// `"abc123"` → `"123abc"`, `"123abc"` → `"abc123"`; empty result when the
/// query is not exactly one letters run followed by one digits run (either
/// order).
fn swap_alpha_numeric(query: &[char]) -> String {
    if query.is_empty() {
        return String::new();
    }
    // Find the single boundary between the letters run and the digits run
    // (strictly alphanumeric, like the upstream regexes).
    if !query.iter().all(|c| c.is_ascii_alphanumeric()) {
        return String::new();
    }
    let Some(split) = (1..query.len()).find(|split| {
        let (head, tail) = query.split_at(*split);
        let head_letters = head[0].is_ascii_lowercase();
        let head_uniform = head.iter().all(|c| c.is_ascii_lowercase() == head_letters);
        let tail_letters = tail[0].is_ascii_lowercase();
        let tail_uniform = tail.iter().all(|c| c.is_ascii_lowercase() == tail_letters);
        head_uniform && tail_uniform && head_letters != tail_letters
    }) else {
        return String::new();
    };
    let (head, tail) = query.split_at(split);
    let mut swapped = String::new();
    swapped.extend(tail);
    swapped.extend(head);
    swapped
}

fn swap_alpha_numeric_bytes(query: &[u8]) -> Vec<u8> {
    if query.is_empty() || !query.iter().all(u8::is_ascii_alphanumeric) {
        return Vec::new();
    }
    let Some(split) = (1..query.len()).find(|split| {
        let (head, tail) = query.split_at(*split);
        let head_letters = head[0].is_ascii_lowercase();
        let head_uniform = head.iter().all(|c| c.is_ascii_lowercase() == head_letters);
        let tail_letters = tail[0].is_ascii_lowercase();
        let tail_uniform = tail.iter().all(|c| c.is_ascii_lowercase() == tail_letters);
        head_uniform && tail_uniform && head_letters != tail_letters
    }) else {
        return Vec::new();
    };
    let (head, tail) = query.split_at(split);
    tail.iter().chain(head.iter()).copied().collect()
}

/// ASCII fast path: no `Vec<char>` and no lowercase copy of `text`. `query` is
/// already ASCII-lowercase; comparisons fold `text` byte-by-byte.
fn fuzzy_match_ascii(query_lower: &str, text: &str) -> FuzzyMatch {
    let text_bytes = text.as_bytes();
    let primary = match_query_bytes(query_lower.as_bytes(), text_bytes);
    if primary.matches {
        return primary;
    }

    let swapped = swap_alpha_numeric_bytes(query_lower.as_bytes());
    if swapped.is_empty() {
        return primary;
    }
    let swapped_match = match_query_bytes(&swapped, text_bytes);
    if !swapped_match.matches {
        return primary;
    }
    FuzzyMatch {
        matches: true,
        score: swapped_match.score + 5.0,
    }
}

/// Unicode path: both inputs are already lowercase.
fn fuzzy_match_unicode(query_lower: &str, text_lower: &str) -> FuzzyMatch {
    let query_chars: Vec<char> = query_lower.chars().collect();
    let text_chars: Vec<char> = text_lower.chars().collect();

    let primary = match_query(&query_chars, &text_chars);
    if primary.matches {
        return primary;
    }

    // Swapped alphanumeric fallback: "abc123" → "123abc" and vice versa
    // (upstream `alphaNumericMatch` / `numericAlphaMatch`).
    let swapped = swap_alpha_numeric(&query_chars);

    if swapped.is_empty() {
        return primary;
    }

    let swapped_chars: Vec<char> = swapped.chars().collect();
    let swapped_match = match_query(&swapped_chars, &text_chars);
    if !swapped_match.matches {
        return primary;
    }

    FuzzyMatch {
        matches: true,
        score: swapped_match.score + 5.0,
    }
}

/// Fuzzy match `query` against `text` (upstream `fuzzyMatch`). Falls back to
/// the swapped letters/digits form of an alphanumeric query (`ab12` ↔ `12ab`)
/// with a +5 penalty, exactly like upstream.
///
/// ASCII input is matched on bytes (no whole-string `Vec<char>` and no
/// lowercase copy of `text`); non-ASCII input keeps the Unicode char path.
pub fn fuzzy_match(query: &str, text: &str) -> FuzzyMatch {
    if query.is_ascii() && text.is_ascii() {
        return fuzzy_match_ascii(&query.to_ascii_lowercase(), text);
    }
    fuzzy_match_unicode(&query.to_lowercase(), &text.to_lowercase())
}

/// [`fuzzy_match`] for a query the caller has already lowercased (the selector
/// stores its filter key lowercased). The text is still normalized here; this
/// only skips re-lowercasing the query for every scored item. Callers must pass
/// the result of `str::to_lowercase`.
pub(crate) fn fuzzy_match_prelowered(query_lower: &str, text: &str) -> FuzzyMatch {
    debug_assert_eq!(query_lower, query_lower.to_lowercase());
    if query_lower.is_ascii() && text.is_ascii() {
        fuzzy_match_ascii(query_lower, text)
    } else {
        fuzzy_match_unicode(query_lower, &text.to_lowercase())
    }
}

/// Filter and sort items by fuzzy match quality, best (lowest score) first.
/// The query is split on whitespace and `/`; every token must match
/// (upstream `fuzzyFilter`).
pub fn fuzzy_filter<T: Clone>(items: &[T], query: &str, get_text: impl Fn(&T) -> &str) -> Vec<T> {
    if query.trim().is_empty() {
        return items.to_vec();
    }
    let tokens: Vec<String> = query
        .trim()
        .split(|c: char| c.is_whitespace() || c == '/')
        .filter(|token| !token.is_empty())
        .map(|token| token.to_lowercase())
        .collect();
    if tokens.is_empty() {
        return items.to_vec();
    }

    let mut results: Vec<(T, f64)> = Vec::new();
    for item in items {
        let text = get_text(item);
        let mut total_score = 0.0;
        let mut all_match = true;
        for token in &tokens {
            let matched = fuzzy_match_prelowered(token, text);
            if matched.matches {
                total_score += matched.score;
            } else {
                all_match = false;
                break;
            }
        }
        if all_match {
            results.push((item.clone(), total_score));
        }
    }

    results.sort_by(|a, b| a.1.total_cmp(&b.1));
    results.into_iter().map(|(item, _)| item).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
