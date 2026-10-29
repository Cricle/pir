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
    // Do NOT disable proxies: a process/network-configured egress proxy is
    // part of the managed environment and must apply. Localhost stays
    // reachable (no address filtering).
    //
    // Redirects are followed manually so every hop is scheme-validated and
    // the hop count is capped; reqwest's policy is disabled to keep the
    // per-hop check here.
    let client = reqwest::Client::builder()
        .timeout(FETCH_TIMEOUT)
        .redirect(reqwest::redirect::Policy::none())
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

    let (body, content_type) = if remote_type.contains("html") {
        let html = String::from_utf8_lossy(&bytes);
        (html_to_text(&html), "markdown".to_owned())
    } else if remote_type.contains("json") {
        (
            String::from_utf8_lossy(&bytes).into_owned(),
            "json".to_owned(),
        )
    } else if remote_type.contains("yaml") {
        (
            String::from_utf8_lossy(&bytes).into_owned(),
            "yaml".to_owned(),
        )
    } else {
        (
            String::from_utf8_lossy(&bytes).into_owned(),
            "text".to_owned(),
        )
    };

    Ok(FetchedPage {
        body,
        content_type,
        bytes: bytes.len(),
        status: status.as_u16(),
    })
}

fn script_style_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    // The regex crate has no backreferences, so the closing-tag alternation
    // is written out; mismatched nesting is irrelevant for text extraction.
    RE.get_or_init(|| {
        Regex::new(
            r"(?is)<(script|style|noscript|template)\b[^>]*>.*?</(?:script|style|noscript|template)\s*>",
        )
        .expect("regex")
    })
}

fn comment_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?s)<!--.*?-->").expect("regex"))
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

fn br_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?i)<br\s*/?>").expect("regex"))
}

fn block_break_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?i)</(?:p|div|li|tr|section|article|header|footer|blockquote|pre)\s*>")
            .expect("regex")
    })
}

fn list_item_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?i)<li\b[^>]*>").expect("regex"))
}

/// HTML → plain text: scripts/styles/comments removed, headings converted to
/// markdown headings, block boundaries to newlines, tags stripped, common
/// entities decoded, whitespace normalized.
pub(crate) fn html_to_text(html: &str) -> String {
    let without_scripts = script_style_re().replace_all(html, " ");
    let without_comments = comment_re().replace_all(&without_scripts, " ");
    let with_headings =
        heading_re().replace_all(&without_comments, |captures: &regex::Captures<'_>| {
            let level = captures[1].len();
            let inner = tag_re().replace_all(&captures[2], " ");
            let title = decode_entities(inner.trim());
            format!("\n\n{} {}\n\n", "#".repeat(level), title)
        });
    let with_breaks = br_re().replace_all(&with_headings, "\n");
    let with_blocks = block_break_re().replace_all(&with_breaks, "\n");
    let with_items = list_item_re().replace_all(&with_blocks, "\n- ");
    let without_tags = tag_re().replace_all(&with_items, " ");
    let decoded = decode_entities(&without_tags);

    // Normalize: one space per run, at most one blank line between lines.
    let mut output = String::with_capacity(decoded.len().min(MAX_BODY_BYTES));
    let mut blank_run = 0usize;
    for line in decoded.lines() {
        let line = line.split_whitespace().collect::<Vec<_>>().join(" ");
        if line.is_empty() {
            blank_run += 1;
            if blank_run > 1 {
                continue;
            }
        } else {
            blank_run = 0;
        }
        output.push_str(&line);
        output.push('\n');
    }
    output.trim().to_owned()
}

fn decode_entities(text: &str) -> String {
    let mut output = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(index) = rest.find('&') {
        output.push_str(&rest[..index]);
        let tail = &rest[index..];
        // Entities are short; a `;` beyond this window is almost certainly a
        // literal ampersand in prose.
        let name_end = tail
            .find(';')
            .filter(|end| *end <= 12)
            .and_then(|end| entity_char(&tail[1..end]).map(|ch| (ch, end)));
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
    match entity.to_ascii_lowercase().as_str() {
        "amp" => Some('&'),
        "lt" => Some('<'),
        "gt" => Some('>'),
        "quot" => Some('"'),
        "apos" => Some('\''),
        "nbsp" => Some(' '),
        "mdash" => Some('—'),
        "ndash" => Some('–'),
        "hellip" => Some('…'),
        "copy" => Some('©'),
        "reg" => Some('®'),
        "trade" => Some('™'),
        "rsquo" => Some('’'),
        "lsquo" => Some('‘'),
        "ldquo" => Some('“'),
        "rdquo" => Some('”'),
        _ => None,
    }
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
