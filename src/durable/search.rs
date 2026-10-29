//! BM25 tool search.
//!
//! Ports upstream `src/search.ts`: a self-contained copy of the coding agent's
//! tool-search ranking (durable keeps its own copy of what it needs from the
//! coding agent). It has no harness dependency, so the whole file ports.
//!
//! Tokenization lowercases, splits at camelCase boundaries and non-alphanumeric
//! runs, drops a small stop-word set and applies a naive singular form.

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
fn stem(term: &str) -> String {
    let length = term.chars().count();
    if length > 4 && term.ends_with("ies") {
        let mut stemmed: String = term.chars().take(length - 3).collect();
        stemmed.push('y');
        return stemmed;
    }
    if length > 4
        && ["ches", "shes", "sses", "xes", "zes"]
            .iter()
            .any(|suffix| term.ends_with(suffix))
    {
        return term.chars().take(length - 2).collect();
    }
    if length > 3 && term.ends_with('s') && !term.ends_with("ss") {
        return term.chars().take(length - 1).collect();
    }
    term.to_owned()
}

/// Insert a space at camelCase boundaries.
fn split_camel_case(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len() + 8);
    for (index, current) in chars.iter().enumerate() {
        let previous = index
            .checked_sub(1)
            .and_then(|previous| chars.get(previous));
        let next = chars.get(index + 1);
        if let Some(previous) = previous {
            // ([a-z0-9])([A-Z]) → "$1 $2"
            let lower_then_upper = (previous.is_ascii_lowercase() || previous.is_ascii_digit())
                && current.is_ascii_uppercase();
            // ([A-Z]+)([A-Z][a-z]) → "$1 $2"
            let acronym_then_word = previous.is_ascii_uppercase()
                && current.is_ascii_uppercase()
                && next.is_some_and(|next| next.is_ascii_lowercase());
            if lower_then_upper || acronym_then_word {
                out.push(' ');
            }
        }
        out.push(*current);
    }
    out
}

/// Lowercase terms, split at camelCase boundaries and non-alphanumerics,
/// without stop words.
pub fn tokenize(text: &str) -> Vec<String> {
    split_camel_case(text)
        .to_lowercase()
        .split(|character: char| !character.is_ascii_alphanumeric())
        .filter(|term| !term.is_empty() && !is_stop_word(term))
        .map(stem)
        .collect()
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
            for term in tokenize(document.text) {
                match counts.iter_mut().find(|(existing, _)| *existing == term) {
                    Some((_, count)) => *count += 1,
                    None => counts.push((term, 1)),
                }
            }
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
