//! `ctx_fetch_and_index`: fetch a URL, convert HTML to text, index it, and
//! return metadata only.
//!
//! The raw page never enters the tool result. HTTP(S) only (every redirect hop
//! is re-validated and capped at 5), a 20 s deadline, an 8 MiB body cap, and a
//! URL+source TTL cache in the same SQLite database (default 24 h;
//! `force: true` or `ttl: 0` bypasses it). The client honors configured
//! proxies; loopback addresses remain reachable. HTML is converted
//! with a small tag/entity pass (scripts and styles removed, headings mapped to
//! markdown headings) so the markdown chunker can use its headings.

use std::sync::{Arc, OnceLock};
use std::time::Duration;

use futures_util::StreamExt;
use regex::Regex;

use super::index::{IndexDoc, DEFAULT_FETCH_TTL_SECS};
use super::ContextModeState;
use crate::agent::tool_batch::ToolResult;
use crate::agent::tool_execution::{AgentTool, BoxFuture, ToolContext};

const FETCH_TIMEOUT: Duration = Duration::from_secs(20);
const MAX_BODY_BYTES: usize = 8 * 1024 * 1024;
/// Redirect hops followed before the fetch is refused.
const MAX_REDIRECTS: usize = 5;

// ---------------------------------------------------------------------------
// HTTP fetch + HTML conversion
// ---------------------------------------------------------------------------

struct FetchedPage {
    body: String,
    content_type: String,
    bytes: usize,
    status: u16,
}

fn validate_http_scheme(url: &reqwest::Url, context: &str) -> Result<(), String> {
    match url.scheme() {
        "http" | "https" => Ok(()),
        scheme => Err(format!(
            "ctx_fetch_and_index: {context} uses unsupported URL scheme `{scheme}` (only http and https are allowed)"
        )),
    }
}

fn is_redirect(status: reqwest::StatusCode) -> bool {
    matches!(status.as_u16(), 301 | 302 | 303 | 307 | 308)
}

async fn fetch_page(url: &str) -> Result<FetchedPage, String> {
    let mut current = reqwest::Url::parse(url)
        .map_err(|error| format!("ctx_fetch_and_index: invalid URL `{url}`: {error}"))?;
    validate_http_scheme(&current, &format!("`{url}`"))?;
    // Honor the SAME proxy configuration every other Pi-owned HTTP client
    // uses (settings `httpProxy`, then PI_HTTP_PROXY / HTTPS_PROXY /
    // HTTP_PROXY). reqwest is built without its `system-proxy` feature, so a
    // bare builder would silently bypass the managed egress proxy — the
    // "context-mode has no network" failure in proxied environments.
    // Localhost stays reachable: NO_PROXY applies when set.
    //
    // Redirects are followed manually so every hop is scheme-validated and
    // the hop count is capped; reqwest's policy is disabled to keep the
    // per-hop check here.
    let mut builder = reqwest::Client::builder()
        .timeout(FETCH_TIMEOUT)
        .redirect(reqwest::redirect::Policy::none());
    if let Some(proxy_url) = crate::ai::transport::ProviderRequestConfig::env_proxy_url() {
        let proxy = reqwest::Proxy::all(&proxy_url).map_err(|_| {
            "ctx_fetch_and_index: the configured HTTP proxy URL is invalid".to_string()
        })?;
        // `NO_PROXY`/`no_proxy` applies to the explicit proxy (reqwest's
        // automatic system-proxy matcher is what normally reads it).
        let proxy = proxy.no_proxy(reqwest::NoProxy::from_env());
        builder = builder.proxy(proxy);
    }
    let client = builder
        .build()
        .map_err(|error| format!("ctx_fetch_and_index: HTTP client setup failed: {error}"))?;

    let mut redirects = 0usize;
    let response = loop {
        let response = client
            .get(current.clone())
            .send()
            .await
            .map_err(|error| format!("ctx_fetch_and_index: fetch failed: {error}"))?;
        let status = response.status();
        if !is_redirect(status) {
            break response;
        }
        if redirects >= MAX_REDIRECTS {
            return Err(format!(
                "ctx_fetch_and_index: `{url}` exceeded the redirect limit ({MAX_REDIRECTS} hops)"
            ));
        }
        let location = response
            .headers()
            .get(reqwest::header::LOCATION)
            .and_then(|value| value.to_str().ok())
            .ok_or_else(|| {
                format!(
                    "ctx_fetch_and_index: `{current}` returned HTTP {status} without a Location header"
                )
            })?;
        let next = current.join(location).map_err(|error| {
            format!("ctx_fetch_and_index: invalid redirect `{location}` from `{current}`: {error}")
        })?;
        // A redirect is not a trusted continuation of the original URL: a
        // public http(s) page must not be able to hand the fetcher a file:/
        // ftp: target (or a metadata endpoint via another scheme).
        validate_http_scheme(&next, &format!("redirect `{location}`"))?;
        current = next;
        redirects += 1;
    };

    let status = response.status();
    if !status.is_success() {
        return Err(format!(
            "ctx_fetch_and_index: `{url}` returned HTTP {status}"
        ));
    }
    if let Some(length) = response.content_length() {
        if length > MAX_BODY_BYTES as u64 {
            return Err(format!(
                "ctx_fetch_and_index: response body is {length} bytes; the limit is {} MiB",
                MAX_BODY_BYTES / (1024 * 1024)
            ));
        }
    }
    let remote_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();

    // Stream with a hard byte cap instead of `bytes()` (which would buffer an
    // unbounded body before the check could run).
    let mut stream = response.bytes_stream();
    let mut bytes: Vec<u8> = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk =
            chunk.map_err(|error| format!("ctx_fetch_and_index: body read failed: {error}"))?;
        if bytes.len().saturating_add(chunk.len()) > MAX_BODY_BYTES {
            return Err(format!(
                "ctx_fetch_and_index: response body exceeds the {} MiB limit",
                MAX_BODY_BYTES / (1024 * 1024)
            ));
        }
        bytes.extend_from_slice(&chunk);
    }

    let byte_len = bytes.len();
    let (body, content_type) = if remote_type.contains("html") {
        let html = decode_body(bytes);
        (html_to_text(&html), "markdown".to_owned())
    } else if remote_type.contains("json") {
        (decode_body(bytes), "json".to_owned())
    } else if remote_type.contains("yaml") {
        (decode_body(bytes), "yaml".to_owned())
    } else {
        (decode_body(bytes), "text".to_owned())
    };

    Ok(FetchedPage {
        body,
        content_type,
        bytes: byte_len,
        status: status.as_u16(),
    })
}

/// UTF-8-decode a response body, consuming the buffer: valid UTF-8 (the
/// overwhelmingly common case) moves the existing allocation into the
/// `String`, while invalid bytes still take the documented lossy path.
fn decode_body(bytes: Vec<u8>) -> String {
    match String::from_utf8(bytes) {
        Ok(text) => text,
        Err(error) => String::from_utf8_lossy(error.as_bytes()).into_owned(),
    }
}

fn script_comment_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    // The regex crate has no backreferences, so the closing-tag alternation
    // is written out; mismatched nesting is irrelevant for text extraction.
    // Comments are folded into the same pass: both alternatives are removed
    // wholesale and cannot affect one another's result, so one document copy
    // replaces the previous script/style pass + comment pass.
    RE.get_or_init(|| {
        Regex::new(
            r"(?is)<(script|style|noscript|template)\b[^>]*>.*?</(?:script|style|noscript|template)\s*>|<!--.*?-->",
        )
        .expect("regex")
    })
}

fn heading_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    // No backreferences in the regex crate: the level comes from the opening
    // tag, the closing level is matched permissively.
    RE.get_or_init(|| Regex::new(r"(?is)<h([1-6])\b[^>]*>(.*?)</h[1-6]\s*>").expect("regex"))
}

fn tag_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?s)<[^>]+>").expect("regex"))
}

fn line_break_tag_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    // One pass for every remaining tag: `<br>` and block closers become a
    // newline, `<li>` opens a markdown list item, every other tag is a space.
    // The specific alternatives precede the generic one, so `<br>`/`<li>`
    // keep their replacement. This is order-equivalent to the old four
    // sequential passes (br → block → li → strip) because replacements never
    // contain tags.
    RE.get_or_init(|| {
        Regex::new(
            r"(?i)<br\s*/?>|</(?:p|div|li|tr|section|article|header|footer|blockquote|pre)\s*>|<li\b[^>]*>|<[^>]+>",
        )
        .expect("regex")
    })
}

/// `<li ...>` opens a list item; `</li>` is a block closer and must not add
/// the `- ` marker.
fn is_list_item_tag(tag: &str) -> bool {
    let bytes = tag.as_bytes();
    bytes.len() >= 4
        && bytes[1..3].eq_ignore_ascii_case(b"li")
        && matches!(bytes[3], b'>' | b'/' | b' ' | b'\t' | b'\n' | b'\r')
}

/// `<br...>` and the block-closing tags the combined regex recognizes.
fn is_line_break_tag(tag: &str) -> bool {
    let bytes = tag.as_bytes();
    if bytes.len() >= 4 && bytes[1..3].eq_ignore_ascii_case(b"br") {
        // Only a bare `<br>`/`<br/>`/`<br />` is a line break; `<br class>`
        // is an unknown tag that the old `br_re` left for `tag_re` (space).
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

fn replace_line_breaks_and_tags(input: &str) -> std::borrow::Cow<'_, str> {
    let regex = line_break_tag_re();
    if !regex.is_match(input) {
        return std::borrow::Cow::Borrowed(input);
    }
    let mut output = String::with_capacity(input.len() + 8);
    let mut last = 0usize;
    for matched in regex.find_iter(input) {
        output.push_str(&input[last..matched.start()]);
        let tag = matched.as_str();
        if is_list_item_tag(tag) {
            output.push_str("\n- ");
        } else if is_line_break_tag(tag) {
            output.push('\n');
        } else {
            output.push(' ');
        }
        last = matched.end();
    }
    output.push_str(&input[last..]);
    std::borrow::Cow::Owned(output)
}

/// HTML → plain text: scripts/styles/comments removed, headings converted to
/// markdown headings, block boundaries to newlines, tags stripped, common
/// entities decoded, whitespace normalized.
pub(crate) fn html_to_text(html: &str) -> String {
    let without_scripts_or_comments = script_comment_re().replace_all(html, " ");
    let with_headings = heading_re().replace_all(
        &without_scripts_or_comments,
        |captures: &regex::Captures<'_>| {
            let level = captures[1].len();
            let inner = tag_re().replace_all(&captures[2], " ");
            let title = decode_entities(inner.trim());
            format!("\n\n{} {}\n\n", "#".repeat(level), title)
        },
    );
    let without_tags = replace_line_breaks_and_tags(&with_headings);
    let decoded = decode_entities(&without_tags);

    // Normalize: one space per run, at most one blank line between lines.
    // `split_whitespace` streams the words into the output; the old
    // `collect::<Vec<_>>().join(" ")` allocated a vector per source line.
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
    // Trim in place (one allocation) instead of `trim().to_owned()` copying
    // the whole document a second time.
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
        // literal ampersand in prose. Search only the first 12 bytes: the old
        // `tail.find(';')` scanned the whole remaining document on every
        // stray `&` before applying the same window check.
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

// ---------------------------------------------------------------------------
// The tool
// ---------------------------------------------------------------------------

pub(crate) struct CtxFetchAndIndexTool {
    state: Arc<ContextModeState>,
}

impl CtxFetchAndIndexTool {
    pub(crate) fn new(state: Arc<ContextModeState>) -> Self {
        Self { state }
    }
}

impl AgentTool for CtxFetchAndIndexTool {
    fn name(&self) -> &'static str {
        super::CTX_FETCH_AND_INDEX_TOOL_NAME
    }

    fn description(&self) -> &'static str {
        "Fetch an http/https URL (20s timeout, 8 MiB body cap, redirects followed only to http/https and capped at 5 hops), convert HTML to plain text, and index it into the local knowledge base. The raw page bytes never enter context — only fetch metadata (status, size, chunk count, cache state) is returned; retrieve the sections later with ctx_search. \
The result is cached by URL+source for ttl seconds (default 24 hours); pass force: true or ttl: 0 to re-fetch. Cache hits return metadata without a network request. \
Use this for docs, specs and changelogs you want to query repeatedly across turns. Only http/https is supported; pages that need JavaScript to render are out of scope (this is a plain HTTP fetch, no browser). For content you already have locally, use ctx_index instead."
    }

    fn parameters(&self) -> Option<serde_json::Value> {
        Some(serde_json::json!({
            "type": "object",
            "properties": {
                "url": {
                    "type": "string",
                    "description": "http:// or https:// URL to fetch and index."
                },
                "source": {
                    "type": "string",
                    "description": "Label for the indexed content (e.g. 'react-docs'). Defaults to the URL."
                },
                "force": {
                    "type": "boolean",
                    "description": "Skip the cache and re-fetch even if the URL was recently indexed."
                },
                "ttl": {
                    "type": "number",
                    "description": "Cache freshness window in seconds (default 86400). 0 bypasses the cache like force: true."
                }
            },
            "required": ["url"],
            "additionalProperties": false
        }))
    }

    fn has_external_side_effects(&self) -> bool {
        true
    }

    fn run(&self, args: &serde_json::Value, ctx: &ToolContext) -> BoxFuture<'static, ToolResult> {
        let state = Arc::clone(&self.state);
        let url = args
            .get("url")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|url| !url.is_empty())
            .map(str::to_owned);
        let source = args
            .get("source")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|source| !source.is_empty())
            .map(str::to_owned);
        let force = args
            .get("force")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);
        let ttl_seconds = args
            .get("ttl")
            .and_then(serde_json::Value::as_f64)
            .filter(|ttl| *ttl >= 0.0)
            .map(|ttl| ttl as u64)
            .unwrap_or(DEFAULT_FETCH_TTL_SECS);
        let _ = ctx;

        Box::pin(async move {
            let Some(url) = url else {
                return ToolResult::error("ctx_fetch_and_index: the `url` parameter is required");
            };
            let source = source.unwrap_or_else(|| url.clone());

            // TTL cache: force and ttl=0 skip it.
            if !force && ttl_seconds > 0 {
                match state
                    .index
                    .cached_fetch(
                        url.clone(),
                        source.clone(),
                        Duration::from_secs(ttl_seconds),
                    )
                    .await
                {
                    Ok(Some(cached)) => {
                        let indexed = match state.index.source_chunk_count(source.clone()).await {
                            Ok(count) if count > 0 => count,
                            Ok(_) => {
                                // Cache survived an external purge: re-index
                                // the stored body instead of returning a
                                // pointer to nothing.
                                let doc = IndexDoc::from_text(
                                    source.clone(),
                                    cached.content_type.clone(),
                                    &cached.body,
                                );
                                match state.index.index_documents(vec![doc]).await {
                                    Ok(report) => report.chunks,
                                    Err(error) => {
                                        return ToolResult::error(format!(
                                            "ctx_fetch_and_index: could not re-index the cached page: {error}"
                                        ))
                                    }
                                }
                            }
                            Err(error) => {
                                return ToolResult::error(format!("ctx_fetch_and_index: {error}"))
                            }
                        };
                        return ToolResult::ok(metadata_json(
                            &url,
                            &source,
                            &cached.content_type,
                            cached.body.len(),
                            0,
                            indexed,
                            ttl_seconds,
                            true,
                        ));
                    }
                    Ok(None) => {}
                    Err(error) => {
                        return ToolResult::error(format!("ctx_fetch_and_index: {error}"))
                    }
                }
            }

            let page = match fetch_page(&url).await {
                Ok(page) => page,
                Err(message) => return ToolResult::error(message),
            };
            let doc = IndexDoc::from_text(source.clone(), page.content_type.clone(), &page.body);
            let report = match state.index.index_documents(vec![doc]).await {
                Ok(report) => report,
                Err(error) => {
                    return ToolResult::error(format!("ctx_fetch_and_index: index failed: {error}"))
                }
            };
            if let Err(error) = state
                .index
                .store_fetch(
                    url.clone(),
                    source.clone(),
                    page.content_type.clone(),
                    page.body,
                )
                .await
            {
                // The content is indexed; a cache write failure only costs a
                // future re-fetch, so report it without failing the call.
                tracing::debug!("ctx_fetch_and_index: cache write failed: {error}");
            }
            ToolResult::ok(metadata_json(
                &url,
                &source,
                &page.content_type,
                page.bytes,
                page.status,
                report.chunks,
                ttl_seconds,
                false,
            ))
        })
    }
}

#[allow(clippy::too_many_arguments)]
fn metadata_json(
    url: &str,
    source: &str,
    content_type: &str,
    bytes: usize,
    status: u16,
    chunks: usize,
    ttl_seconds: u64,
    cached: bool,
) -> String {
    let value = serde_json::json!({
        "url": url,
        "source": source,
        "contentType": content_type,
        "status": status,
        "bytes": bytes,
        "chunks": chunks,
        "ttlSeconds": ttl_seconds,
        "cached": cached,
    });
    serde_json::to_string_pretty(&value).unwrap_or_else(|_| "{}".to_owned())
}
