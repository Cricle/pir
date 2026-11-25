use super::*;

/// Inline LaTeX candidate found by [`tokenize_inline_latex`].
pub(super) struct InlineLatex {
    /// Exclusive end offset (char index) of the raw token.
    pub(super) end: usize,
    /// Math source between the delimiters (pending: everything after the
    /// opening delimiter, including newlines — shown raw).
    pub(super) text: String,
    /// Streamed form whose closing delimiter has not arrived yet.
    pub(super) pending: bool,
}

const CLOSE_DOUBLE_DOLLAR: &[char] = &['$', '$'];
const CLOSE_PAREN: &[char] = &['\\', ')'];
const CLOSE_BRACKET: &[char] = &['\\', ']'];
const CLOSE_DOLLAR: &[char] = &['$'];

/// Port of upstream `tokenizeInlineLatex` (markdown.ts:52-99): attempts an
/// inline math token at `chars[at]` (`$`, `$$`, `\(` or `\[`).
pub(super) fn tokenize_inline_latex(chars: &[char], at: usize) -> Option<InlineLatex> {
    let src = &chars[at..];
    let (opening, closing): (&str, &[char]) = if src.starts_with(&['$', '$']) {
        ("$$", CLOSE_DOUBLE_DOLLAR)
    } else if src.starts_with(&['\\', '(']) {
        ("\\(", CLOSE_PAREN)
    } else if src.starts_with(&['\\', '[']) {
        ("\\[", CLOSE_BRACKET)
    } else if src.first() == Some(&'$') && !src.get(1).is_some_and(|c| c.is_whitespace()) {
        ("$", CLOSE_DOLLAR)
    } else {
        return None;
    };
    let opening_len = opening.chars().count();
    if let Some(closing_index) = find_closing_delimiter(src, closing, opening_len) {
        if opening == "$" {
            let content = &src[opening_len..closing_index];
            let after = &src[closing_index + closing.len()..];
            let ends_with_space = content.last().is_some_and(|c| c.is_whitespace());
            let digit_after = after.first().is_some_and(|c| c.is_ascii_digit());
            let shell_variable = is_env_var_like(content)
                && after
                    .first()
                    .is_some_and(|c| c.is_ascii_alphabetic() || *c == '_');
            let contains_backtick = content.contains(&'`');
            if ends_with_space || digit_after || shell_variable || contains_backtick {
                return None;
            }
        }
        let text: String = src[opening_len..closing_index].iter().collect();
        if text.is_empty() || text.contains('\n') {
            return None;
        }
        return Some(InlineLatex {
            end: at + closing_index + closing.len(),
            text,
            pending: false,
        });
    }
    let pending: String = src[opening_len..].iter().collect();
    if opening.starts_with('\\') || looks_like_pending_dollar_math(&pending) {
        return Some(InlineLatex {
            end: chars.len(),
            text: pending,
            pending: true,
        });
    }
    None
}

/// Render one inline math token (markdown.ts:655-663): pending or
/// unsupported math keeps the raw source; otherwise Unicode rendering.
pub(super) fn emit_inline_latex(
    chars: &[char],
    at: usize,
    token: &InlineLatex,
    options: &MarkdownOptions,
) -> String {
    let raw: String = chars[at..token.end].iter().collect();
    if !token.pending && options.render_latex {
        latex::render_latex(&token.text, false).unwrap_or(raw)
    } else {
        raw
    }
}

/// Upstream `findClosingDelimiter` (markdown.ts:40-46): first unescaped
/// occurrence of `closing` at or after `start`.
fn find_closing_delimiter(source: &[char], closing: &[char], start: usize) -> Option<usize> {
    let mut index = start;
    while index + closing.len() <= source.len() {
        if source[index..index + closing.len()] == *closing {
            if !is_escaped(source, index) {
                return Some(index);
            }
            index += closing.len();
            continue;
        }
        index += 1;
    }
    None
}

/// Upstream `isEscaped` (markdown.ts:32-38): odd number of backslashes
/// immediately before `index`.
fn is_escaped(source: &[char], index: usize) -> bool {
    let mut backslashes = 0usize;
    let mut position = index;
    while position > 0 && source[position - 1] == '\\' {
        backslashes += 1;
        position -= 1;
    }
    backslashes % 2 == 1
}

/// Upstream `/^[A-Z_][A-Z0-9_]*(?:[^A-Za-z0-9_\s])?$/` (markdown.ts:77).
fn is_env_var_like(content: &[char]) -> bool {
    match content.first() {
        Some(&c) if c.is_ascii_uppercase() || c == '_' => {}
        _ => return false,
    }
    let mut index = 1usize;
    while index < content.len()
        && (content[index].is_ascii_uppercase()
            || content[index].is_ascii_digit()
            || content[index] == '_')
    {
        index += 1;
    }
    if index < content.len() {
        let c = content[index];
        if c.is_ascii_alphanumeric() || c == '_' || c.is_whitespace() {
            return false;
        }
        index += 1;
    }
    index == content.len()
}

/// Upstream `looksLikePendingDollarMath` (markdown.ts:48-50).
fn looks_like_pending_dollar_math(source: &str) -> bool {
    let chars: Vec<char> = source.chars().collect();
    for (index, c) in chars.iter().enumerate() {
        if *c == '\\'
            && chars
                .get(index + 1)
                .is_some_and(|n| n.is_ascii_alphabetic())
        {
            return true;
        }
        if matches!(
            c,
            '_' | '^'
                | '='
                | '+'
                | '*'
                | '/'
                | '<'
                | '>'
                | '('
                | ')'
                | '['
                | ']'
                | '|'
                | '-'
                | '±'
                | '≤'
                | '≥'
                | '≠'
                | '≈'
                | '∈'
                | '→'
                | '⇒'
                | '∞'
                | '∫'
                | '∑'
                | '√'
        ) {
            return true;
        }
    }
    false
}

/// Block LaTeX candidate found by [`scan_block_latex`].
pub(super) struct BlockLatex {
    pub(super) text: String,
    pub(super) pending: bool,
    pub(super) raw: String,
}

/// Port of upstream `tokenizeBlockLatex` (markdown.ts:101-121): a `$$…$$`
/// or `\[…\]` math block starting at `rest`. An unclosed `\[` block is
/// always a pending token; an unclosed `$$` block only when its body looks
/// like math (markdown.ts:112-119).
pub(super) fn scan_block_latex(rest: &str) -> Option<BlockLatex> {
    let chars: Vec<char> = rest.chars().collect();
    let mut pos = 0usize;
    while pos < chars.len() && (chars[pos] == ' ' || chars[pos] == '\t') {
        pos += 1;
    }
    if chars[pos..].starts_with(&['$', '$']) {
        let content_start = skip_latex_open(&chars, pos + 2);
        if let Some(close) = find_block_close(&chars, content_start, '$') {
            let content: String = chars[content_start..close].iter().collect();
            if !content.is_empty() {
                let raw_end = consume_latex_tail(&chars, close + 2);
                return Some(BlockLatex {
                    text: content.trim().to_string(),
                    pending: false,
                    raw: chars[..raw_end].iter().collect(),
                });
            }
        }
        let content: String = chars[content_start..].iter().collect();
        if !content.is_empty() && looks_like_pending_dollar_math(&content) {
            return Some(BlockLatex {
                text: content,
                pending: true,
                raw: rest.to_string(),
            });
        }
        return None;
    }
    if chars[pos..].starts_with(&['\\', '[']) {
        let content_start = skip_latex_open(&chars, pos + 2);
        if let Some(close) = find_block_close(&chars, content_start, ']') {
            let content: String = chars[content_start..close].iter().collect();
            if !content.is_empty() {
                let raw_end = consume_latex_tail(&chars, close + 2);
                return Some(BlockLatex {
                    text: content.trim().to_string(),
                    pending: false,
                    raw: chars[..raw_end].iter().collect(),
                });
            }
        }
        // Upstream `pendingBracket` (markdown.ts:112-115): any unclosed `\[`
        // block is a pending token regardless of its content.
        let content: String = chars[content_start..].iter().collect();
        return Some(BlockLatex {
            text: content,
            pending: true,
            raw: rest.to_string(),
        });
    }
    None
}

/// `[ \t]*(?:\n)?` right after the opening delimiter (regex prefix).
fn skip_latex_open(chars: &[char], mut position: usize) -> usize {
    while position < chars.len() && (chars[position] == ' ' || chars[position] == '\t') {
        position += 1;
    }
    if chars.get(position) == Some(&'\n') {
        position += 1;
    }
    position
}

/// First closing candidate whose tail matches `[ \t]*(?:\n|$)` (regex
/// backtracking order: the earliest viable candidate wins).
fn find_block_close(chars: &[char], from: usize, kind: char) -> Option<usize> {
    let mut index = from;
    while index + 1 < chars.len() {
        let pair = match kind {
            '$' => chars[index] == '$' && chars[index + 1] == '$',
            _ => chars[index] == '\\' && chars[index + 1] == ']',
        };
        if pair && block_tail_ok(chars, index + 2) {
            return Some(index);
        }
        index += 1;
    }
    None
}

fn block_tail_ok(chars: &[char], mut position: usize) -> bool {
    while position < chars.len() && (chars[position] == ' ' || chars[position] == '\t') {
        position += 1;
    }
    position >= chars.len() || chars[position] == '\n'
}

/// Consumes `[ \t]*(?:\n|$)` into the token raw (the block regex includes
/// this trailing whitespace/newline) — the same skip as [`skip_latex_open`],
/// used on the closing side.
fn consume_latex_tail(chars: &[char], position: usize) -> usize {
    skip_latex_open(chars, position)
}
