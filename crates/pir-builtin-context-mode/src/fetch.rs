//! Pure HTTP/text fetch normalization.
//!
//! The network half of `ctx_fetch_and_index` (redirect policy, proxy setup,
//! body streaming) stays in `pir-core`. This module owns the deterministic
//! post-processing that can be tested offline:
//!
//! - scheme validation (`http`/`https` only, every redirect hop re-checked),
//! - the response body/size limits,
//! - content-type classification (`html` → markdown, `json`, `yaml`, text),
//! - UTF-8 body decoding (lossy only for genuinely invalid bytes),
//! - HTML → plain text: scripts/styles/comments removed, headings converted
//!   to markdown headings, block boundaries to newlines, list items to `- `,
//!   tags stripped, common entities decoded, whitespace normalized.

pub const MAX_BODY_BYTES: usize = 8 * 1024 * 1024;
pub const FETCH_TIMEOUT_SECS: u64 = 20;
/// Redirect hops followed before the fetch is refused.
pub const MAX_REDIRECTS: usize = 5;

/// Whether a URL string uses the `http`/`https` scheme (case-insensitive).
///
/// Split out from [`validate_http_scheme`] because a host that already holds a
/// parsed URL still needs the SAME scheme rule while keeping its own diagnostic
/// wording (the host phrases it with the original-vs-redirect context).
pub fn is_http_scheme(url: &str) -> bool {
    url.split_once("://").is_some_and(|(scheme, _)| {
        scheme.eq_ignore_ascii_case("http") || scheme.eq_ignore_ascii_case("https")
    })
}

/// Validate that `url` (or a redirect target) is absolute and uses http(s).
pub fn validate_http_scheme(url: &str) -> Result<(), String> {
    let Some((scheme, _)) = url.split_once("://") else {
        return Err(format!(
            "ctx_fetch_and_index: `{url}` is not an absolute URL (only http and https are allowed)"
        ));
    };
    if is_http_scheme(url) {
        Ok(())
    } else {
        Err(format!(
            "ctx_fetch_and_index: `{url}` uses unsupported URL scheme `{scheme}` (only http and https are allowed)"
        ))
    }
}

/// Indexed content type for a response `Content-Type` header. The header may
/// carry parameters (`text/html; charset=utf-8`), which are ignored.
pub fn content_type_for(remote_type: &str) -> &'static str {
    let mime = remote_type
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();
    if mime.contains("html") {
        "markdown"
    } else if mime.contains("json") {
        "json"
    } else if mime.contains("yaml") {
        "yaml"
    } else {
        "text"
    }
}

/// UTF-8-decode a response body, consuming the buffer: valid UTF-8 (the
/// overwhelmingly common case) moves the existing allocation into the
/// `String`, while invalid bytes still take the documented lossy path.
pub fn decode_body(bytes: Vec<u8>) -> String {
    match String::from_utf8(bytes) {
        Ok(text) => text,
        Err(error) => String::from_utf8_lossy(error.as_bytes()).into_owned(),
    }
}

/// Decode and normalize one fetched body, returning `(body, content_type)`.
pub fn normalize_response_body(bytes: Vec<u8>, remote_type: &str) -> (String, &'static str) {
    let content_type = content_type_for(remote_type);
    let body = if content_type == "markdown" {
        html_to_text(&decode_body(bytes))
    } else {
        decode_body(bytes)
    };
    (body, content_type)
}

// ---------------------------------------------------------------------------
// HTML → text
// ---------------------------------------------------------------------------

fn find_ascii_ci(haystack: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    if needle.is_empty() {
        return None;
    }
    let tail = haystack.get(from..)?;
    if tail.len() < needle.len() {
        return None;
    }
    tail.windows(needle.len())
        .position(|window| window.eq_ignore_ascii_case(needle))
        .map(|offset| from + offset)
}

/// Name of a raw-text element when `rest` starts with its opening tag.
fn raw_text_element(rest: &str) -> Option<&'static str> {
    for name in ["script", "style", "noscript", "template"] {
        let bytes = rest.as_bytes();
        if bytes.len() <= name.len() {
            continue;
        }
        if bytes[0] == b'<' && bytes[1..1 + name.len()].eq_ignore_ascii_case(name.as_bytes()) {
            if let Some(b'>' | b'/' | b' ' | b'\t' | b'\n' | b'\r') = bytes.get(1 + name.len()) {
                return Some(name);
            }
        }
    }
    None
}

/// Remove `<script>`/`<style>`/`<noscript>`/`<template>` elements and HTML
/// comments wholesale.
fn strip_scripts_and_comments(html: &str) -> String {
    let bytes = html.as_bytes();
    let mut output = String::with_capacity(html.len());
    let mut copied = 0usize;
    let mut index = 0usize;
    while index < bytes.len() {
        if bytes[index] != b'<' {
            index += 1;
            continue;
        }
        let rest = &html[index..];
        let mut skipped_to = None;
        if let Some(comment) = rest.strip_prefix("<!--") {
            if let Some(end) = comment.find("-->") {
                skipped_to = Some(index + 4 + end + 3);
            }
        } else if let Some(name) = raw_text_element(rest) {
            if let Some(open_end) = rest.find('>') {
                let closing = format!("</{name}");
                if let Some(close_start) =
                    find_ascii_ci(bytes, closing.as_bytes(), index + open_end + 1)
                {
                    if let Some(close_end) =
                        bytes[close_start..].iter().position(|byte| *byte == b'>')
                    {
                        skipped_to = Some(close_start + close_end + 1);
                    }
                }
            }
        }
        match skipped_to {
            Some(next) => {
                output.push_str(&html[copied..index]);
                index = next;
                copied = next;
            }
            None => index += 1,
        }
    }
    output.push_str(&html[copied..]);
    output
}

/// Heading level when `rest` starts with `<h1>` … `<h6>` (or a spaced/
/// attributed spelling).
fn heading_open(rest: &str) -> Option<usize> {
    let bytes = rest.as_bytes();
    if bytes.len() < 4 || bytes[0] != b'<' || !bytes[1].eq_ignore_ascii_case(&b'h') {
        return None;
    }
    let level = match bytes[2] {
        digit @ b'1'..=b'6' => usize::from(digit - b'0'),
        _ => return None,
    };
    match bytes[3] {
        b'>' | b'/' | b' ' | b'\t' | b'\n' | b'\r' => Some(level),
        _ => None,
    }
}

/// Replace remaining tags with a space (used inside heading text).
fn strip_tags_to_space(fragment: &str) -> String {
    let mut output = String::with_capacity(fragment.len());
    let mut copied = 0usize;
    let mut index = 0usize;
    while let Some(offset) = fragment[index..].find('<') {
        let start = index + offset;
        let Some(end_offset) = fragment[start..].find('>') else {
            break;
        };
        output.push_str(&fragment[copied..start]);
        output.push(' ');
        index = start + end_offset + 1;
        copied = index;
    }
    output.push_str(&fragment[copied..]);
    output
}

/// Convert `<h1>`…`<h6>` to markdown headings. Other markup is left alone.
fn headings_to_markdown(html: &str) -> String {
    let mut output = String::with_capacity(html.len() + 16);
    let mut copied = 0usize;
    let mut index = 0usize;
    while index < html.len() {
        if html.as_bytes()[index] != b'<' {
            index += 1;
            continue;
        }
        let rest = &html[index..];
        let Some(level) = heading_open(rest) else {
            index += 1;
            continue;
        };
        let Some(open_end) = rest.find('>') else {
            break;
        };
        let inner_start = index + open_end + 1;
        // Closing level is matched permissively: `<h2>…</h3>` still closes.
        let closing = b"</h";
        let Some(close_start) = find_ascii_ci(html.as_bytes(), closing, inner_start) else {
            index += 1;
            continue;
        };
        let Some(close_end) = html.as_bytes()[close_start..]
            .iter()
            .position(|byte| *byte == b'>')
        else {
            index += 1;
            continue;
        };
        let inner = strip_tags_to_space(&html[inner_start..close_start]);
        let title = decode_entities(inner.trim());
        output.push_str(&html[copied..index]);
        output.push_str("\n\n");
        output.push_str(&"#".repeat(level));
        if !title.is_empty() {
            output.push(' ');
            output.push_str(&title);
        }
        output.push_str("\n\n");
        index = close_start + close_end + 1;
        copied = index;
    }
    output.push_str(&html[copied..]);
    output
}

/// `<li ...>` opens a list item; `</li>` is a block closer and must not add
/// the `- ` marker.
fn is_list_item_tag(tag: &str) -> bool {
    let bytes = tag.as_bytes();
    bytes.len() >= 4
        && bytes[1..3].eq_ignore_ascii_case(b"li")
        && matches!(bytes[3], b'>' | b'/' | b' ' | b'\t' | b'\n' | b'\r')
}

/// `<br...>` and the block-closing tags converted to a newline.
fn is_line_break_tag(tag: &str) -> bool {
    let bytes = tag.as_bytes();
    if bytes.len() >= 4 && bytes[1..3].eq_ignore_ascii_case(b"br") {
        // Only a bare `<br>`/`<br/>`/`<br />` is a line break; `<br class>`
        // is an unknown tag (space).
        let rest = tag[3..].trim_start_matches([' ', '\t', '\n', '\r']);
        if rest == ">" || rest == "/>" {
            return true;
        }
    }
    let Some(name) = tag.strip_prefix("</") else {
        return false;
    };
    let name = name
        .trim_start()
        .trim_end_matches('>')
        .trim_end()
        .trim_end_matches('/');
    [
        "p",
        "div",
        "li",
        "tr",
        "section",
        "article",
        "header",
        "footer",
        "blockquote",
        "pre",
    ]
    .iter()
    .any(|closer| name.eq_ignore_ascii_case(closer))
}

/// One pass over the remaining tags: `<br>` and block closers become a
/// newline, `<li>` opens a markdown list item, every other tag is a space.
fn replace_line_breaks_and_tags(input: &str) -> String {
    let mut output = String::with_capacity(input.len() + 8);
    let mut copied = 0usize;
    let mut index = 0usize;
    while let Some(offset) = input[index..].find('<') {
        let start = index + offset;
        let Some(end_offset) = input[start..].find('>') else {
            break;
        };
        let end = start + end_offset + 1;
        let tag = &input[start..end];
        output.push_str(&input[copied..start]);
        if is_list_item_tag(tag) {
            output.push_str("\n- ");
        } else if is_line_break_tag(tag) {
            output.push('\n');
        } else {
            output.push(' ');
        }
        index = end;
        copied = end;
    }
    output.push_str(&input[copied..]);
    output
}

/// HTML → plain text: scripts/styles/comments removed, headings converted to
/// markdown headings, block boundaries to newlines, tags stripped, common
/// entities decoded, whitespace normalized.
pub fn html_to_text(html: &str) -> String {
    let without_scripts_or_comments = strip_scripts_and_comments(html);
    let with_headings = headings_to_markdown(&without_scripts_or_comments);
    let without_tags = replace_line_breaks_and_tags(&with_headings);
    let decoded = decode_entities(&without_tags);

    // Normalize: one space per run, at most one blank line between lines.
    let mut output = String::with_capacity(decoded.len().min(MAX_BODY_BYTES));
    let mut blank_run = 0usize;
    for line in decoded.lines() {
        let mut has_word = false;
        for word in line.split_whitespace() {
            if has_word {
                output.push(' ');
            }
            output.push_str(word);
            has_word = true;
        }
        if !has_word {
            blank_run += 1;
            if blank_run > 1 {
                continue;
            }
        } else {
            blank_run = 0;
        }
        output.push('\n');
    }
    // Trim in place (one allocation) instead of copying the document again.
    let start = output.len() - output.trim_start().len();
    let end = output.trim_end().len();
    output.truncate(end);
    if start < output.len() {
        output.drain(..start);
    }
    output
}

fn decode_entities(text: &str) -> String {
    let mut output = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(index) = rest.find('&') {
        output.push_str(&rest[..index]);
        let tail = &rest[index..];
        // Entities are short; a `;` beyond this window is almost certainly a
        // literal ampersand in prose. Search only the first 12 bytes.
        let search_end = tail.len().min(13);
        let entity_end = tail.as_bytes()[1..search_end]
            .iter()
            .position(|byte| *byte == b';')
            .map(|position| position + 1);
        let name_end = entity_end.and_then(|end| entity_char(&tail[1..end]).map(|ch| (ch, end)));
        match name_end {
            Some((ch, end)) => {
                output.push(ch);
                rest = &tail[end + 1..];
            }
            None => {
                output.push('&');
                rest = &tail[1..];
            }
        }
    }
    output.push_str(rest);
    output
}

fn entity_char(entity: &str) -> Option<char> {
    if let Some(numeric) = entity.strip_prefix('#') {
        if let Some(hex) = numeric
            .strip_prefix('x')
            .or_else(|| numeric.strip_prefix('X'))
        {
            return u32::from_str_radix(hex, 16).ok().and_then(char::from_u32);
        }
        return numeric.parse::<u32>().ok().and_then(char::from_u32);
    }
    const NAMED: [(&str, char); 16] = [
        ("amp", '&'),
        ("lt", '<'),
        ("gt", '>'),
        ("quot", '"'),
        ("apos", '\''),
        ("nbsp", ' '),
        ("mdash", '—'),
        ("ndash", '–'),
        ("hellip", '…'),
        ("copy", '©'),
        ("reg", '®'),
        ("trade", '™'),
        ("rsquo", '’'),
        ("lsquo", '‘'),
        ("ldquo", '“'),
        ("rdquo", '”'),
    ];
    NAMED
        .iter()
        .find(|(name, _)| entity.eq_ignore_ascii_case(name))
        .map(|(_, ch)| *ch)
}
