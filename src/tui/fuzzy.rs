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

    let mut query_index = 0usize;
    let mut score = 0.0f64;
    let mut last_match_index: Option<usize> = None;
    let mut consecutive = 0i64;

    for (i, &c) in text.iter().enumerate() {
        if query_index >= query.len() {
            break;
        }
        if c == query[query_index] {
            let boundary = i == 0 || text[i - 1].is_whitespace() || is_word_boundary(text[i - 1]);
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
            if boundary {
                score -= 10.0;
            }
            score += i as f64 * 0.1;
            last_match_index = Some(i);
            query_index += 1;
        }
    }

    if query_index < query.len() {
        return FuzzyMatch::miss();
    }

    if query == text {
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

/// Fuzzy match `query` against `text` (upstream `fuzzyMatch`). Falls back to
/// the swapped letters/digits form of an alphanumeric query (`ab12` ↔ `12ab`)
/// with a +5 penalty, exactly like upstream.
pub fn fuzzy_match(query: &str, text: &str) -> FuzzyMatch {
    let query_lower: Vec<char> = query.to_lowercase().chars().collect();
    let text_lower: Vec<char> = text.to_lowercase().chars().collect();

    let primary = match_query(&query_lower, &text_lower);
    if primary.matches {
        return primary;
    }

    // Swapped alphanumeric fallback: "abc123" → "123abc" and vice versa
    // (upstream `alphaNumericMatch` / `numericAlphaMatch`).
    let swapped = swap_alpha_numeric(&query_lower);

    if swapped.is_empty() {
        return primary;
    }

    let swapped_chars: Vec<char> = swapped.chars().collect();
    let swapped_match = match_query(&swapped_chars, &text_lower);
    if !swapped_match.matches {
        return primary;
    }

    FuzzyMatch {
        matches: true,
        score: swapped_match.score + 5.0,
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
        .map(str::to_string)
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
            let matched = fuzzy_match(token, text);
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
