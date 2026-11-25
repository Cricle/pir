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

use std::sync::Arc;
use std::time::Duration;

use futures_util::StreamExt;

use super::index::{IndexDoc, DEFAULT_FETCH_TTL_SECS};
use super::ContextModeState;
use crate::agent::tool_batch::ToolResult;
use crate::agent::tool_execution::{AgentTool, BoxFuture, ToolContext};
/// Content-type mapping, body decoding and HTML→markdown are pure, so they
/// live in the engine-free crate. `html_to_text` is re-exported because the
/// index path (`index.rs`) renders already-fetched HTML through it.
pub use pir_builtin_context_mode::{html_to_text, normalize_response_body};

// The fetch bounds are shared with the crate (its request validation clamps to
// the same values), so they are re-exported rather than restated.
pub use pir_builtin_context_mode::{MAX_BODY_BYTES, MAX_REDIRECTS};

/// The request timeout as a `Duration` (the crate owns the second count).
const FETCH_TIMEOUT: Duration = Duration::from_secs(pir_builtin_context_mode::FETCH_TIMEOUT_SECS);

// ---------------------------------------------------------------------------
// HTTP fetch + HTML conversion
// ---------------------------------------------------------------------------

pub struct FetchedPage {
    pub body: String,
    pub content_type: String,
    pub bytes: usize,
    pub status: u16,
}

/// The scheme rule is the crate's (the same predicate the string-URL validator
/// uses); the diagnostic wording stays here because it names the original URL
/// or the redirect that produced the hop.
fn validate_http_scheme(url: &reqwest::Url, context: &str) -> Result<(), String> {
    if pir_builtin_context_mode::is_http_scheme(url.as_str()) {
        return Ok(());
    }
    Err(format!(
        "ctx_fetch_and_index: {context} uses unsupported URL scheme `{}` (only http and https are allowed)",
        url.scheme()
    ))
}

fn is_redirect(status: reqwest::StatusCode) -> bool {
    matches!(status.as_u16(), 301 | 302 | 303 | 307 | 308)
}

pub async fn fetch_page(url: &str) -> Result<FetchedPage, String> {
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
    // The raw header as an owned string: `normalize_response_body` strips
    // parameters and classifies the MIME type itself, so pre-normalizing here
    // would classify the same header twice.
    let remote_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_owned();

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
    // Content-type mapping + body decoding + HTML→markdown live in the
    // engine-free crate, so the tool and the pure tests share one
    // implementation.
    let (body, content_type) = normalize_response_body(bytes, &remote_type);
    let content_type = content_type.to_owned();

    Ok(FetchedPage {
        body,
        content_type,
        bytes: byte_len,
        status: status.as_u16(),
    })
}

// ---------------------------------------------------------------------------
// The tool
// ---------------------------------------------------------------------------

pub struct CtxFetchAndIndexTool {
    state: Arc<ContextModeState>,
}

impl CtxFetchAndIndexTool {
    pub fn new(state: Arc<ContextModeState>) -> Self {
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
        let cwd = ctx.cwd.clone();

        Box::pin(async move {
            let Some(url) = url else {
                return ToolResult::error("ctx_fetch_and_index: the `url` parameter is required");
            };
            let source = source.unwrap_or_else(|| url.clone());
            state.bind_scope(&cwd);

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
                        return ToolResult::ok(metadata_json(FetchMetadata {
                            url: &url,
                            source: &source,
                            content_type: &cached.content_type,
                            bytes: cached.body.len(),
                            status: 0,
                            chunks: indexed,
                            ttl_seconds,
                            cached: true,
                        }));
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
            state
                .journal(
                    "fetch",
                    super::CTX_FETCH_AND_INDEX_TOOL_NAME,
                    Some(url.clone()),
                    true,
                    None,
                    page.bytes,
                    format!("fetched {} B from {url}", page.bytes),
                )
                .await;
            ToolResult::ok(metadata_json(FetchMetadata {
                url: &url,
                source: &source,
                content_type: &page.content_type,
                bytes: page.bytes,
                status: page.status,
                chunks: report.chunks,
                ttl_seconds,
                cached: false,
            }))
        })
    }
}

/// The fetch/cache facts one `ctx_fetch_and_index` result reports.
struct FetchMetadata<'a> {
    url: &'a str,
    source: &'a str,
    content_type: &'a str,
    bytes: usize,
    status: u16,
    chunks: usize,
    ttl_seconds: u64,
    cached: bool,
}

fn metadata_json(metadata: FetchMetadata<'_>) -> String {
    let FetchMetadata {
        url,
        source,
        content_type,
        bytes,
        status,
        chunks,
        ttl_seconds,
        cached,
    } = metadata;
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
