//! Session-selector search, a Rust port of upstream
//! `modes/interactive/components/session-selector-search.ts`.
//!
//! The selector filters a session listing with:
//!
//! - **token mode** (the default): whitespace-separated tokens, ALL of which
//!   must match (multi-word AND). `"quoted phrases"` match with whitespace
//!   normalized; unbalanced quotes fall back to plain whitespace splitting.
//! - **regex mode** (`re:<pattern>`): a case-insensitive regular expression
//!   over the name, id, every user/assistant message, and the cwd.
//! - **tiered scoring**: an exact (case-insensitive) hit in the session NAME
//!   beats an exact hit in the id, message text, or cwd, which in turn beats a
//!   scattered FUZZY subsequence in the name or cwd. Each token is scored by
//!   its best field, and tier offsets dominate the fuzzy score, so an exact
//!   hit anywhere beats a short scattered match. Lower total score is better.
//! - **case-insensitivity** everywhere (upstream lowercases every field).
//!
//! The searchable text for a session includes `all_messages_text` 鈥?every
//! user and assistant message's text joined with a space (upstream
//! `buildSessionInfo`) 鈥?so a query can hit a message from any point in the
//! conversation, not just the first one.
//!
//! [`SessionSearchQuery`] is the parsed form (parse once per keystroke, match
//! against every session); [`match_session_search`] returns the relevance
//! score when the session matches. The selector uses these directly in
//! `coding_agent::session_pick`.

use regex::{Regex, RegexBuilder};

use super::SessionSummary;

/// Exact-hit tier: the query text appears in the session name.
pub const TIER_NAME: f64 = 0.0;
/// Exact-hit tier: the query text appears in the id, messages, or cwd.
pub const TIER_TEXT: f64 = 1.0;
/// Scattered-match tier: the query characters appear in order in name/cwd.
pub const TIER_FUZZY: f64 = 2.0;
/// Tier offset applied before the within-tier score (upstream
/// `TIER_WEIGHT`). Tier offsets dominate fuzzy scores.
pub const TIER_WEIGHT: f64 = 1_000_000.0;

/// Whether a token is an exact-match phrase or a fuzzy token
/// (upstream `ParsedSearchQuery.tokens[].kind`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionSearchTokenKind {
    Fuzzy,
    Phrase,
}

/// One parsed query token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionSearchToken {
    pub kind: SessionSearchTokenKind,
    /// Raw fuzzy token or whitespace-normalized lowercase phrase.
    pub value: String,
}

/// A parsed session search query (upstream `ParsedSearchQuery`).
///
/// `error` is set when the query was `re:` with an empty or invalid pattern;
/// such a query matches nothing (upstream `filterAndSortSessions` returns an
/// empty list).
#[derive(Debug, Clone, Default)]
pub struct SessionSearchQuery {
    pub tokens: Vec<SessionSearchToken>,
    /// Compiled case-insensitive pattern for `re:<pattern>`.
    pub regex: Option<Regex>,
    pub error: Option<String>,
}

/// Lowercase and collapse every whitespace run (upstream
/// `normalizeWhitespaceLower`): `" Foo\t Bar "` 鈫?`"foo bar"`.
pub fn normalize_whitespace_lower(text: &str) -> String {
    text.to_lowercase()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// The fields the search matches against (upstream `SessionInfo` facets).
#[derive(Debug, Clone, Copy)]
pub struct SessionSearchFields<'a> {
    pub id: &'a str,
    pub cwd: &'a str,
    pub name: Option<&'a str>,
    /// Every user/assistant message's text joined with a space.
    pub all_messages_text: &'a str,
}

impl SessionSummary {
    /// The searchable facets of this listing record.
    pub fn search_fields(&self) -> SessionSearchFields<'_> {
        SessionSearchFields {
            id: &self.id,
            cwd: &self.cwd,
            name: self.name.as_deref(),
            all_messages_text: &self.all_messages_text,
        }
    }
}

impl SessionSearchQuery {
    /// Parse a selector query (upstream `parseSearchQuery`):
    ///
    /// - an empty/whitespace query has no tokens and matches everything;
    /// - `re:<pattern>` is regex mode (case-insensitive; an empty or invalid
    ///   pattern sets `error` and matches nothing);
    /// - otherwise whitespace tokenizes with `"quoted phrases"`. An unclosed
    ///   quote falls back to plain whitespace splitting (every token fuzzy).
    pub fn parse(query: &str) -> Self {
        let trimmed = query.trim();
        if trimmed.is_empty() {
            return Self::default();
        }

        if let Some(rest) = trimmed.strip_prefix("re:") {
            let pattern = rest.trim();
            if pattern.is_empty() {
                return Self {
                    tokens: Vec::new(),
                    regex: None,
                    error: Some("Empty regex".to_owned()),
                };
            }
            return match RegexBuilder::new(pattern).case_insensitive(true).build() {
                Ok(regex) => Self {
                    tokens: Vec::new(),
                    regex: Some(regex),
                    error: None,
                },
                Err(error) => Self {
                    tokens: Vec::new(),
                    regex: None,
                    error: Some(error.to_string()),
                },
            };
        }

        // Token mode with quote support: `foo "node cve" bar`.
        let mut tokens: Vec<SessionSearchToken> = Vec::new();
        let mut buffer = String::new();
        let mut in_quote = false;
        let mut had_unclosed_quote = false;

        fn flush(
            tokens: &mut Vec<SessionSearchToken>,
            buffer: &mut String,
            kind: SessionSearchTokenKind,
        ) {
            let value = buffer.trim();
            if !value.is_empty() {
                tokens.push(SessionSearchToken {
                    kind,
                    value: value.to_owned(),
                });
            }
            buffer.clear();
        }

        for character in trimmed.chars() {
            if character == '"' {
                if in_quote {
                    flush(&mut tokens, &mut buffer, SessionSearchTokenKind::Phrase);
                    in_quote = false;
                } else {
                    flush(&mut tokens, &mut buffer, SessionSearchTokenKind::Fuzzy);
                    in_quote = true;
                }
                continue;
            }
            if !in_quote && character.is_whitespace() {
                flush(&mut tokens, &mut buffer, SessionSearchTokenKind::Fuzzy);
                continue;
            }
            buffer.push(character);
        }
        if in_quote {
            had_unclosed_quote = true;
        }

        if had_unclosed_quote {
            // Upstream falls back to plain whitespace tokenization, EVERY
            // token fuzzy (a stray quote is not a phrase delimiter).
            return Self {
                tokens: trimmed
                    .split_whitespace()
                    .filter(|token| !token.is_empty())
                    .map(|token| SessionSearchToken {
                        kind: SessionSearchTokenKind::Fuzzy,
                        value: token.to_owned(),
                    })
                    .collect(),
                regex: None,
                error: None,
            };
        }

        flush(&mut tokens, &mut buffer, SessionSearchTokenKind::Fuzzy);

        Self {
            tokens,
            regex: None,
            error: None,
        }
    }

    /// Whether this query imposes no constraint (matches every session).
    pub fn is_empty(&self) -> bool {
        self.regex.is_none() && self.tokens.is_empty() && self.error.is_none()
    }
}

// ---------------------------------------------------------------------------
// Fuzzy matching (the port of upstream `packages/tui/src/fuzzy.ts`)
// ---------------------------------------------------------------------------

/// One fuzzy match (upstream `FuzzyMatch`).
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

fn is_word_boundary(previous: char) -> bool {
    previous.is_whitespace() || matches!(previous, '-' | '_' | '.' | '/' | ':')
}

fn score_positions(text: &[char], positions: &[usize]) -> f64 {
    let mut score = 0.0;
    let mut last_match: Option<usize> = None;
    let mut consecutive = 0i64;
    for &index in positions {
        match last_match {
            Some(previous) if previous + 1 == index => {
                consecutive += 1;
                score -= (consecutive * 5) as f64;
            }
            _ => {
                consecutive = 0;
                if let Some(previous) = last_match {
                    score += ((index - previous - 1) * 2) as f64;
                }
            }
        }
        if index == 0 || is_word_boundary(text[index - 1]) {
            score -= 10.0;
        }
        score += index as f64 * 0.1;
        last_match = Some(index);
    }
    score
}

fn greedy_positions(query: &[char], text: &[char]) -> Option<Vec<usize>> {
    let mut positions = Vec::with_capacity(query.len());
    let mut cursor = 0usize;
    for &wanted in query {
        let found = text[cursor..]
            .iter()
            .position(|&candidate| candidate == wanted)?
            + cursor;
        positions.push(found);
        cursor = found + 1;
    }
    Some(positions)
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

    let Some(mut positions) = greedy_positions(query, text) else {
        return FuzzyMatch::miss();
    };
    let mut score = score_positions(text, &positions);

    // The greedy alignment can scatter the query over an earlier, unrelated
    // part of the text; also score every contiguous occurrence and keep the
    // best alignment (upstream `matchQuery`).
    let mut start = 0usize;
    while start + query.len() <= text.len() {
        if text[start..start + query.len()] == *query {
            for (offset, position) in positions.iter_mut().enumerate() {
                *position = start + offset;
            }
            score = score.min(score_positions(text, &positions));
        }
        start += 1;
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
/// `"abc123"` 鈫?`"123abc"` and vice versa (upstream `fuzzyMatch` fallback).
fn swap_alpha_numeric(query: &[char]) -> Vec<char> {
    if query.is_empty()
        || !query
            .iter()
            .all(|character| character.is_ascii_alphanumeric())
    {
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

/// Fuzzy match `query` against `text` (upstream `fuzzyMatch`). Lower score is
/// better; falls back to the swapped letters/digits form with a +5 penalty.
pub fn fuzzy_match(query: &str, text: &str) -> FuzzyMatch {
    let query_lower: Vec<char> = query.to_lowercase().chars().collect();
    let text_lower: Vec<char> = text.to_lowercase().chars().collect();

    let primary = match_query(&query_lower, &text_lower);
    if primary.matches {
        return primary;
    }

    let swapped = swap_alpha_numeric(&query_lower);
    if swapped.is_empty() {
        return primary;
    }
    let swapped_match = match_query(&swapped, &text_lower);
    if !swapped_match.matches {
        return primary;
    }
    FuzzyMatch {
        matches: true,
        score: swapped_match.score + 5.0,
    }
}

// ---------------------------------------------------------------------------
// Matching
// ---------------------------------------------------------------------------

fn tier_score(tier: f64, score: f64) -> f64 {
    tier * TIER_WEIGHT + score
}

/// Whether the session carries a non-blank display name (upstream
/// `hasSessionName`).
pub fn has_session_name(name: Option<&str>) -> bool {
    name.is_some_and(|name| !name.trim().is_empty())
}

fn match_literal_token(fields: &SessionSearchFields<'_>, value: &str) -> Option<f64> {
    if let Some(name) = fields.name {
        let name = name.to_lowercase();
        if name.contains(value) {
            return Some(tier_score(TIER_NAME, fuzzy_match(value, &name).score));
        }
    }
    if fields.id.to_lowercase().contains(value) {
        return Some(tier_score(TIER_TEXT, 0.0));
    }
    if fields.all_messages_text.to_lowercase().contains(value) {
        return Some(tier_score(TIER_TEXT, 0.0));
    }
    if fields.cwd.to_lowercase().contains(value) {
        return Some(tier_score(TIER_TEXT, 0.0));
    }
    None
}

fn match_fuzzy_token(fields: &SessionSearchFields<'_>, value: &str) -> Option<f64> {
    let literal = match_literal_token(fields, &value.to_lowercase());
    if literal.is_some() {
        return literal;
    }

    let name = fields.name.unwrap_or("");
    let mut best: Option<f64> = None;
    for field in [name, fields.cwd] {
        let matched = fuzzy_match(value, field);
        if matched.matches && best.is_none_or(|score| matched.score < score) {
            best = Some(matched.score);
        }
    }
    best.map(|score| tier_score(TIER_FUZZY, score))
}

fn match_phrase_token(fields: &SessionSearchFields<'_>, phrase: &str) -> Option<f64> {
    let normalized = normalize_whitespace_lower(phrase);
    if normalized.is_empty() {
        return Some(0.0);
    }
    if let Some(name) = fields.name {
        let name = normalize_whitespace_lower(name);
        if name.contains(&normalized) {
            return Some(tier_score(TIER_NAME, fuzzy_match(&normalized, &name).score));
        }
    }
    if fields.id.to_lowercase().contains(&normalized) {
        return Some(tier_score(TIER_TEXT, 0.0));
    }
    if normalize_whitespace_lower(fields.all_messages_text).contains(&normalized) {
        return Some(tier_score(TIER_TEXT, 0.0));
    }
    if normalize_whitespace_lower(fields.cwd).contains(&normalized) {
        return Some(tier_score(TIER_TEXT, 0.0));
    }
    None
}

fn match_regex(fields: &SessionSearchFields<'_>, regex: &Regex) -> Option<f64> {
    if fields
        .name
        .is_some_and(|name| !name.is_empty() && regex.is_match(name))
    {
        return Some(tier_score(TIER_NAME, 0.0));
    }
    if regex.is_match(fields.id)
        || regex.is_match(fields.all_messages_text)
        || regex.is_match(fields.cwd)
    {
        return Some(tier_score(TIER_TEXT, 0.0));
    }
    None
}

/// Match one session, returning `Some(score)` (lower is better) when it
/// matches, `None` otherwise (upstream `matchSession`).
///
/// Each field is matched separately, so a token never matches across field
/// boundaries; different tokens may match different fields. An empty query
/// matches with score `0`; a query whose parse failed matches nothing.
pub fn match_session_search(
    fields: &SessionSearchFields<'_>,
    query: &SessionSearchQuery,
) -> Option<f64> {
    if query.error.is_some() {
        return None;
    }
    if let Some(regex) = query.regex.as_ref() {
        return match_regex(fields, regex);
    }
    if query.tokens.is_empty() {
        return Some(0.0);
    }
    let mut total = 0.0;
    for token in &query.tokens {
        let score = match token.kind {
            SessionSearchTokenKind::Phrase => match_phrase_token(fields, &token.value),
            SessionSearchTokenKind::Fuzzy => match_fuzzy_token(fields, &token.value),
        }?;
        total += score;
    }
    Some(total)
}
