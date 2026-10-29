//! Shared HTTP helpers for the OAuth token endpoints.
//!
//! Bounded requests (30s timeout, upstream's per-flow request timeout), JSON
//! and form bodies, and error text that mirrors the upstream messages.

use std::time::Duration;

use serde_json::Value;

use crate::ai::error::{AiError, AiResult};

/// The per-request timeout shared by the OAuth flows (upstream sends
/// `AbortSignal.timeout(30_000)` on token endpoints).
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Maximum response body accepted from an OAuth endpoint (1 MiB). Token
/// responses are small JSON documents; the endpoints are trusted, but a
/// compromised/misbehaving endpoint (or a proxy) must not make the client
/// buffer an unbounded body. Oversized bodies fail with
/// [`AiError::ResponseTooLarge`].
pub const MAX_RESPONSE_BODY_BYTES: usize = 1024 * 1024;

fn client() -> AiResult<reqwest::Client> {
    reqwest::Client::builder()
        .timeout(REQUEST_TIMEOUT)
        .build()
        .map_err(|error| AiError::Http(error.to_string()))
}

/// A completed HTTP response: status plus body text.
pub struct HttpResponse {
    pub status: u16,
    pub body: String,
}

/// A completed HTTP response with response headers (for retry-after handling).
pub struct HttpResponseWithHeaders {
    pub status: u16,
    pub body: String,
    pub headers: Vec<(String, String)>,
}

impl HttpResponseWithHeaders {
    /// The first header value matching `name` (case-insensitive).
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }

    /// Parse the body as JSON.
    pub fn json(&self) -> AiResult<Value> {
        serde_json::from_str(&self.body).map_err(|error| {
            AiError::Authentication(format!("OAuth response was not valid JSON: {error}"))
        })
    }

    /// Whether the status is 2xx.
    pub fn ok(&self) -> bool {
        (200..300).contains(&self.status)
    }
}

impl HttpResponse {
    /// Parse the body as JSON.
    pub fn json(&self) -> AiResult<Value> {
        serde_json::from_str(&self.body).map_err(|error| {
            AiError::Authentication(format!("OAuth response was not valid JSON: {error}"))
        })
    }

    /// Whether the status is 2xx.
    pub fn ok(&self) -> bool {
        (200..300).contains(&self.status)
    }
}

impl From<HttpResponseWithHeaders> for HttpResponse {
    fn from(response: HttpResponseWithHeaders) -> Self {
        Self {
            status: response.status,
            body: response.body,
        }
    }
}

/// Send a prepared request and collect `(status, headers, body-text)`: the
/// shared tail of every OAuth request helper below.
async fn send_and_collect(
    request: reqwest::RequestBuilder,
    url: &str,
) -> AiResult<HttpResponseWithHeaders> {
    let mut response = request
        .send()
        .await
        .map_err(|error| AiError::Http(format!("{url}: {error}")))?;
    let status = response.status().as_u16();
    let headers = response
        .headers()
        .iter()
        .map(|(name, value)| {
            (
                name.to_string(),
                value.to_str().unwrap_or_default().to_string(),
            )
        })
        .collect();
    // Reject an announced oversized body before reading anything (the common
    // path), then enforce the same cap while streaming because the header may
    // be absent (chunked) or lie.
    if let Some(length) = response.content_length() {
        if length > MAX_RESPONSE_BODY_BYTES as u64 {
            return Err(AiError::ResponseTooLarge {
                limit: MAX_RESPONSE_BODY_BYTES,
            });
        }
    }
    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|error| AiError::Http(format!("{url}: {error}")))?
    {
        if body.len().saturating_add(chunk.len()) > MAX_RESPONSE_BODY_BYTES {
            return Err(AiError::ResponseTooLarge {
                limit: MAX_RESPONSE_BODY_BYTES,
            });
        }
        body.extend_from_slice(&chunk);
    }
    // `Response::text()` decodes lossily as UTF-8 and strips a leading BOM;
    // keep the same body shape while the read itself is bounded above.
    let body = String::from_utf8_lossy(&body);
    let body = body.strip_prefix('\u{feff}').unwrap_or(&body).to_string();
    Ok(HttpResponseWithHeaders {
        status,
        body,
        headers,
    })
}

/// POST a JSON body.
pub async fn post_json(url: &str, body: &Value) -> AiResult<HttpResponse> {
    let response = post_json_with_headers(url, body, &[]).await?;
    Ok(HttpResponse {
        status: response.status,
        body: response.body,
    })
}

/// POST a JSON body with extra request headers, keeping response headers.
pub async fn post_json_with_headers(
    url: &str,
    body: &Value,
    extra_headers: &[(&str, &str)],
) -> AiResult<HttpResponseWithHeaders> {
    let client = client()?;
    let mut request = client
        .post(url)
        .header("content-type", "application/json")
        .header("accept", "application/json")
        .json(body);
    for (name, value) in extra_headers {
        request = request.header(*name, *value);
    }
    send_and_collect(request, url).await
}

/// POST a form body.
pub async fn post_form(url: &str, fields: &[(&str, &str)]) -> AiResult<HttpResponse> {
    post_form_with_headers(url, fields, &[]).await
}

/// POST a form body with extra request headers (the MCP OAuth token request
/// needs `Authorization: Basic` for `client_secret_basic`).
pub async fn post_form_with_headers(
    url: &str,
    fields: &[(&str, &str)],
    extra_headers: &[(&str, &str)],
) -> AiResult<HttpResponse> {
    let client = client()?;
    let mut request = client
        .post(url)
        .header("content-type", "application/x-www-form-urlencoded")
        .header("accept", "application/json")
        .form(fields);
    for (name, value) in extra_headers {
        request = request.header(*name, *value);
    }
    Ok(send_and_collect(request, url).await?.into())
}

/// GET with an optional bearer token, keeping response headers.
pub async fn get_with_headers(
    url: &str,
    bearer: Option<&str>,
    extra_headers: &[(&str, &str)],
) -> AiResult<HttpResponseWithHeaders> {
    let client = client()?;
    let mut request = client.get(url).header("accept", "application/json");
    if let Some(bearer) = bearer {
        request = request.header("authorization", format!("Bearer {bearer}"));
    }
    for (name, value) in extra_headers {
        request = request.header(*name, *value);
    }
    send_and_collect(request, url).await
}

/// GET with an optional bearer token.
pub async fn get_json(
    url: &str,
    bearer: Option<&str>,
    extra_headers: &[(&str, &str)],
) -> AiResult<HttpResponse> {
    Ok(get_with_headers(url, bearer, extra_headers).await?.into())
}

/// Extract a required non-empty string field from a JSON object.
pub fn required_string(body: &Value, field: &str) -> AiResult<String> {
    match body.get(field) {
        Some(Value::String(value)) if !value.is_empty() => Ok(value.clone()),
        _ => Err(AiError::Authentication(format!(
            "Invalid OAuth response field: {field}"
        ))),
    }
}

/// Extract a required positive finite number field from a JSON object.
pub fn positive_number(body: &Value, field: &str) -> AiResult<f64> {
    match body.get(field) {
        Some(Value::Number(value)) => {
            let value = value.as_f64().unwrap_or(0.0);
            if value.is_finite() && value > 0.0 {
                Ok(value)
            } else {
                Err(AiError::Authentication(format!(
                    "Invalid OAuth response field: {field}"
                )))
            }
        }
        _ => Err(AiError::Authentication(format!(
            "Invalid OAuth response field: {field}"
        ))),
    }
}

/// JSON keys whose values are credentials in an OAuth endpoint body.
fn is_sensitive_oauth_key(key: &str) -> bool {
    matches!(
        key.to_ascii_lowercase().as_str(),
        "access_token" | "refresh_token" | "id_token" | "token" | "client_secret" | "code_verifier"
    )
}

/// Recursively replace credential-bearing values with `[REDACTED]`.
fn redact_oauth_body(value: &Value) -> Value {
    match value {
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(key, value)| {
                    let value = if is_sensitive_oauth_key(key) {
                        Value::String("[REDACTED]".to_string())
                    } else {
                        redact_oauth_body(value)
                    };
                    (key.clone(), value)
                })
                .collect(),
        ),
        Value::Array(items) => Value::Array(items.iter().map(redact_oauth_body).collect()),
        other => other.clone(),
    }
}

/// Render a token-endpoint response body for an error message with every
/// credential-bearing field redacted.
///
/// Token endpoints sometimes echo the submitted `access_token`,
/// `refresh_token` or `id_token` back inside an error payload; embedding the
/// raw body would persist them in logs/UI. JSON bodies keep their shape with
/// the secret values replaced; a non-JSON body (an HTML proxy page, a plain
/// text relay) is omitted entirely because it cannot be redacted structurally.
pub fn redact_oauth_body_text(body: &str) -> String {
    if body.trim().is_empty() {
        return String::new();
    }
    match serde_json::from_str::<Value>(body) {
        Ok(value) => redact_oauth_body(&value).to_string(),
        Err(_) => "[OAuth error response body omitted]".to_string(),
    }
}

/// Format an OAuth error response detail as `error: description` when present.
pub fn error_detail(body: &Value) -> Option<String> {
    let error = body.get("error").and_then(Value::as_str);
    let description = body.get("error_description").and_then(Value::as_str);
    match (error, description) {
        (Some(error), Some(description)) => Some(format!("{error}: {description}")),
        (Some(error), None) => Some((*error).to_string()),
        (None, Some(description)) => Some((*description).to_string()),
        (None, None) => None,
    }
}

/// The unified failure text for a non-2xx OAuth response
/// (mirrors the upstream `<action> failed (HTTP <status>): <detail>` shapes).
pub fn oauth_http_failure(action: &str, status: u16, body: &Value) -> AiError {
    let redacted = redact_oauth_body(body);
    let detail = error_detail(&redacted).unwrap_or_else(|| redacted.to_string());
    AiError::Authentication(format!("OAuth {action} failed (HTTP {status}): {detail}"))
}
