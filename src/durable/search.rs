//! BM25 tool search.
//!
//! Ports upstream `src/search.ts`: a self-contained copy of the coding agent's
//! tool-search ranking (durable keeps its own copy of what it needs from the
//! coding agent). It has no harness dependency, so the whole file ports.
//!
//! Tokenization lowercases, splits at camelCase boundaries and non-alphanumeric
//! runs, drops a small stop-word set and applies a naive singular form.

use std::borrow::Cow;

/// One searchable document.
#[derive(Clone, Copy, Debug)]
pub struct SearchDocument<'a> {
    /// The document's stable name.
    pub name: &'a str,
    /// The text indexed for search.
    pub text: &'a str,
}

const STOP_WORDS: &[&str] = &[
    "a", "an", "and", "are", "as", "at", "be", "by", "for", "from", "in", "is", "it", "of", "on",
    "or", "that", "the", "this", "to", "with",
];

fn is_stop_word(term: &str) -> bool {
    STOP_WORDS.contains(&term)
}

/// Naive singular form, so `issues` matches `issue` and `searches` matches
/// `search`.
fn stem(term: &str) -> Cow<'_, str> {
    let length = term.chars().count();
    if length > 4 && term.ends_with("ies") {
        let mut stemmed: String = term.chars().take(length - 3).collect();
        stemmed.push('y');
        return Cow::Owned(stemmed);
    }
    if length > 4
        && ["ches", "shes", "sses", "xes", "zes"]
            .iter()
            .any(|suffix| term.ends_with(suffix))
    {
        return Cow::Owned(term.chars().take(length - 2).collect());
    }
    if length > 3 && term.ends_with('s') && !term.ends_with("ss") {
        return Cow::Owned(term.chars().take(length - 1).collect());
    }
    Cow::Borrowed(term)
}

/// Visit the terms of `text` in order, reusing one token buffer.
///
/// This is the single-pass form of the old
/// `split_camel_case(text).to_lowercase().split(...)` pipeline: it inserts the
/// same camelCase boundaries (judged on the original characters), lowercases
/// each character exactly like `str::to_lowercase` (including expansions such
/// as `İ` → `i̇`), splits on lowercased non-ASCII-alphanumerics, drops stop
/// words and applies [`stem`]. It never allocates a whole-text copy; the
/// per-token buffer is reused across terms.
fn for_each_token(text: &str, mut visit: impl FnMut(&str)) {
    let mut current = String::new();
    let mut previous: Option<char> = None;
    let mut chars = text.chars().peekable();
    while let Some(character) = chars.next() {
        if let Some(previous) = previous {
            // ([a-z0-9])([A-Z]) → "$1 $2"
            let lower_then_upper = (previous.is_ascii_lowercase() || previous.is_ascii_digit())
                && character.is_ascii_uppercase();
            // ([A-Z]+)([A-Z][a-z]) → "$1 $2"
            let acronym_then_word = previous.is_ascii_uppercase()
                && character.is_ascii_uppercase()
                && chars.peek().is_some_and(|next| next.is_ascii_lowercase());
            if lower_then_upper || acronym_then_word {
                flush_token(&mut current, &mut visit);
            }
        }
        for lower in character.to_lowercase() {
            if lower.is_ascii_alphanumeric() {
                current.push(lower);
            } else {
                flush_token(&mut current, &mut visit);
            }
        }
        previous = Some(character);
    }
    flush_token(&mut current, &mut visit);
}

fn flush_token(current: &mut String, visit: &mut impl FnMut(&str)) {
    if current.is_empty() {
        return;
    }
    if !is_stop_word(current) {
        let stemmed = stem(current);
        visit(stemmed.as_ref());
    }
    current.clear();
}

/// Lowercase terms, split at camelCase boundaries and non-alphanumerics,
/// without stop words.
pub fn tokenize(text: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    for_each_token(text, |term| tokens.push(term.to_owned()));
    tokens
}

/// Okapi BM25 (k1 1.2, b 0.75): the names of the best matches, best first;
/// ties keep document order.
pub fn rank_documents(query: &str, documents: &[SearchDocument<'_>], limit: usize) -> Vec<String> {
    const K1: f64 = 1.2;
    const B: f64 = 0.75;
    let mut query_terms: Vec<String> = Vec::new();
    for term in tokenize(query) {
        if !query_terms.contains(&term) {
            query_terms.push(term);
        }
    }
    if query_terms.is_empty() || documents.is_empty() || limit == 0 {
        return Vec::new();
    }

    let term_counts: Vec<Vec<(String, usize)>> = documents
        .iter()
        .map(|document| {
            let mut counts: Vec<(String, usize)> = Vec::new();
            for_each_token(document.text, |term| {
                match counts.iter_mut().find(|(existing, _)| existing == term) {
                    Some((_, count)) => *count += 1,
                    None => counts.push((term.to_owned(), 1)),
                }
            });
            counts
        })
        .collect();
    let lengths: Vec<usize> = term_counts
        .iter()
        .map(|counts| counts.iter().map(|(_, count)| count).sum())
        .collect();
    let total_length: usize = lengths.iter().sum();
    let average_length = {
        let average = total_length as f64 / documents.len() as f64;
        if average == 0.0 {
            1.0
        } else {
            average
        }
    };
    let idf: Vec<f64> = query_terms
        .iter()
        .map(|term| {
            let frequency = term_counts
                .iter()
                .filter(|counts| counts.iter().any(|(candidate, _)| candidate == term))
                .count();
            let numerator = documents.len() as f64 - frequency as f64 + 0.5;
            let denominator = frequency as f64 + 0.5;
            (1.0 + numerator / denominator).ln()
        })
        .collect();

    let mut matches: Vec<(usize, f64)> = Vec::new();
    for (index, counts) in term_counts.iter().enumerate() {
        let mut score = 0.0;
        for (term_index, term) in query_terms.iter().enumerate() {
            let Some((_, count)) = counts.iter().find(|(candidate, _)| candidate == term) else {
                continue;
            };
            let norm = K1 * (1.0 - B + (B * lengths[index] as f64) / average_length);
            score += idf[term_index] * ((*count as f64 * (K1 + 1.0)) / (*count as f64 + norm));
        }
        if score > 0.0 {
            matches.push((index, score));
        }
    }
    // Stable sort: equal scores keep document order.
    matches.sort_by(|left, right| {
        right
            .1
            .partial_cmp(&left.1)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    matches
        .into_iter()
        .take(limit)
        .map(|(index, _)| documents[index].name.to_owned())
        .collect()
}
