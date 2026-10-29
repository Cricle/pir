//! W13-C `streamProxy` residual-closure tests.
//!
//! Covers the four defects the W12 read-only audit filed against
//! `src/sdk/proxy.rs` (W12 code-review P2 items 1-4):
//!
//! 1. an unknown proxy event `type` must be IGNORED (upstream
//!    `processProxyEvent` falls through for anything it does not recognize), so
//!    a newer server can add event types without breaking an older client,
//!    while a genuinely malformed frame is still a protocol error;
//! 2. a failure while flushing the trailing (not newline-terminated) frame
//!    must report the accumulated partial through `decoder.fail` instead of
//!    losing it;
//! 3. neither `ProxyEndpoint` nor `ProxyStreamOptions` may print the bearer
//!    token (or a per-request `api_key`) in `Debug` output;
//! 4. dropping the consumer must let the producer observe closure instead of
//!    parking in `push_await` forever. **Not covered here**: the signal needs a
//!    consumer-drop seam on `AssistantMessageEventStream`, which is outside
//!    this lane's file scope — see this lane's report.
//!
//! Everything is hermetic: the scripted in-process transport from the
//! transport's public test seam; no sockets, no external network, no writes
//! under `~/.pi`.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::StreamExt;
use reqwest::header::HeaderMap;
use serde_json::{json, Value};

use pir::agent::tool_execution::BoxFuture;
use pir::ai::error::AiResult;
use pir::ai::provider::Provider;
use pir::ai::transport::{
    HttpBodyChunk, HttpRequest, HttpResponse, HttpTransport, ProviderRequestClient,
    ProviderRequestConfig,
};
use pir::ai::types::{
    AssistantMessage, AssistantMessageEvent, ContentBlock, Context, ErrorReason, Model, ModelCost,
    ModelCostRates, StopReason, StreamOptions, Usage,
};
use pir::sdk::{
    stream_proxy, ProxyEndpoint, ProxyProvider, ProxyStreamOptions, PROXY_CONNECTION_CLOSED_MESSAGE,
};

// ---------------------------------------------------------------------------
// Scripted HTTP transport (the transport's public test seam; no sockets)
// ---------------------------------------------------------------------------

struct ScriptedTransport {
    status: u16,
    /// Body chunks, delivered in order (several chunks exercise the
    /// incremental decoder; one chunk is the common scripted case).
    chunks: Vec<String>,
    requests: Arc<Mutex<Vec<HttpRequest>>>,
}

impl ScriptedTransport {
    fn new(status: u16, body: impl Into<String>) -> (Self, Arc<Mutex<Vec<HttpRequest>>>) {
        Self::from_chunks(status, vec![body.into()])
    }

    fn from_chunks(status: u16, chunks: Vec<String>) -> (Self, Arc<Mutex<Vec<HttpRequest>>>) {
        let requests = Arc::new(Mutex::new(Vec::new()));
        (
            Self {
                status,
                chunks,
                requests: Arc::clone(&requests),
            },
            requests,
        )
    }
}

impl HttpTransport for ScriptedTransport {
    fn execute(&self, request: HttpRequest) -> BoxFuture<'_, AiResult<HttpResponse>> {
        self.requests
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(request);
        let status = self.status;
        let chunks = self.chunks.clone();
        Box::pin(async move {
            let body = futures_util::stream::iter(chunks.into_iter().map(|chunk| {
                Ok::<HttpBodyChunk, pir::ai::error::AiError>(HttpBodyChunk::new(chunk.into_bytes()))
            }));
            Ok(HttpResponse {
                status,
                headers: HeaderMap::new(),
                body: Box::pin(body),
            })
        })
    }
}

fn scripted_client(transport: ScriptedTransport) -> ProviderRequestClient {
    // No retry sleeps: every scripted case must resolve immediately.
    ProviderRequestClient::with_transport(
        ProviderRequestConfig {
            timeout: Duration::from_secs(5),
            max_response_bytes: 1 << 20,
            max_retries: 0,
            max_retry_delay: Duration::ZERO,
            proxy_url: None,
            http_idle_timeout: Some(Duration::from_secs(5)),
        },
        Arc::new(transport),
    )
}

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

fn model() -> Model {
    Model {
        id: "gpt-test".into(),
        name: "Test model".into(),
        api: "openai-responses".into(),
        provider: "proxy".into(),
        base_url: "https://api.example.com/v1".into(),
        reasoning: false,
        thinking_level_map: None,
        input: vec!["text".into()],
        cost: ModelCost {
            rates: ModelCostRates {
                input: 0.0,
                output: 0.0,
                cache_read: 0.0,
                cache_write: 0.0,
            },
            tiers: Vec::new(),
        },
        context_window: 4096,
        max_tokens: 256,
        sampling_params: None,
        headers: None,
        thinking_token_budget_field: None,
        input_limits: None,
        prompt_cache: None,
        sampling_params_by_thinking_level: None,
        model_type: None,
        compat: None,
    }
}

fn usage() -> Value {
    json!({
        "input": 3,
        "output": 2,
        "cacheRead": 0,
        "cacheWrite": 0,
        "totalTokens": 5,
        "cost": { "input": 0.0, "output": 0.0, "cacheRead": 0.0, "cacheWrite": 0.0, "total": 0.0 }
    })
}

/// Render proxy events as the SSE body the server sends.
fn sse_body(events: &[Value]) -> String {
    events
        .iter()
        .map(|event| format!("data: {event}\n\n"))
        .collect()
}

fn done_event() -> Value {
    json!({ "type": "done", "reason": "stop", "usage": usage() })
}

/// The text of the terminal message (the accumulated partial).
fn message_text(message: &AssistantMessage) -> String {
    message
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text(text) => Some(text.text.as_str()),
            _ => None,
        })
        .collect()
}

fn provider_with_body(body: String) -> ProxyProvider {
    provider_with_chunks(vec![body])
}

fn provider_with_chunks(chunks: Vec<String>) -> ProxyProvider {
    let (transport, _requests) = ScriptedTransport::from_chunks(200, chunks);
    ProxyProvider::new(
        ProxyEndpoint::new("https://proxy.example.com", "test-token"),
        vec![model()],
    )
    .expect("proxy provider")
    .with_client(scripted_client(transport))
}

/// Drain a provider stream to its terminal message, collecting events.
async fn collect(provider: &ProxyProvider) -> (Vec<AssistantMessageEvent>, Arc<AssistantMessage>) {
    let mut stream =
        Box::pin(provider.stream(&model(), Context::default(), StreamOptions::default()));
    let mut events = Vec::new();
    while let Some(event) = stream.as_mut().next().await {
        events.push(event);
    }
    let result = tokio::time::timeout(Duration::from_secs(5), stream.result())
        .await
        .expect("the stream must terminate")
        .expect("terminal result");
    (events, result)
}

fn labels(events: &[AssistantMessageEvent]) -> Vec<&'static str> {
    events
        .iter()
        .map(|event| match event {
            AssistantMessageEvent::Start { .. } => "start",
            AssistantMessageEvent::TextStart { .. } => "text_start",
            AssistantMessageEvent::TextDelta { .. } => "text_delta",
            AssistantMessageEvent::TextEnd { .. } => "text_end",
            AssistantMessageEvent::ThinkingStart { .. } => "thinking_start",
            AssistantMessageEvent::ThinkingDelta { .. } => "thinking_delta",
            AssistantMessageEvent::ThinkingEnd { .. } => "thinking_end",
            AssistantMessageEvent::ToolCallStart { .. } => "toolcall_start",
            AssistantMessageEvent::ToolCallDelta { .. } => "toolcall_delta",
            AssistantMessageEvent::ToolCallEnd { .. } => "toolcall_end",
            AssistantMessageEvent::Done { .. } => "done",
            AssistantMessageEvent::Error { .. } => "error",
        })
        .collect()
}

// ---------------------------------------------------------------------------
// (1) Unknown proxy event types are ignored (P2 item 1)
// ---------------------------------------------------------------------------

/// A proxy server may emit event types this client does not know (it is a
/// separately deployed component). Upstream's `processProxyEvent` ignores
/// them; failing the stream instead would break every older client the moment
/// the server ships a new event type.
#[tokio::test]
async fn unknown_proxy_event_types_are_ignored() {
    let body = sse_body(&[
        json!({ "type": "start" }),
        json!({ "type": "text_start", "contentIndex": 0 }),
        // Forward-compatibility frame: unknown type, arbitrary future payload.
        json!({ "type": "usage_delta", "cacheHitTokens": 12, "nested": { "x": [1, 2] } }),
        json!({ "type": "text_delta", "contentIndex": 0, "delta": "kept" }),
        // Another unknown type, at a different position in the stream.
        json!({ "type": "some_future_event" }),
        json!({ "type": "text_end", "contentIndex": 0 }),
        done_event(),
    ]);
    let provider = provider_with_body(body);

    let (events, result) = collect(&provider).await;

    assert_eq!(
        labels(&events),
        vec!["start", "text_start", "text_delta", "text_end", "done"],
        "unknown event types must produce no assistant event and no failure"
    );
    assert_eq!(result.stop_reason, StopReason::Stop);
    assert_eq!(message_text(&result), "kept");
    assert!(
        result.error_message.is_none(),
        "an ignored frame must not set an error message: {:?}",
        result.error_message
    );
}

/// An unknown event must not disturb the stream's terminal semantics either:
/// a clean EOF after unknown frames is still the upstream "connection closed"
/// error, so ignoring the frames can never fake a successful completion.
#[tokio::test]
async fn unknown_proxy_event_types_do_not_fake_termination() {
    let body = sse_body(&[
        json!({ "type": "start" }),
        json!({ "type": "some_future_event" }),
    ]);
    let provider = provider_with_body(body);

    let (events, result) = collect(&provider).await;

    assert_eq!(labels(&events), vec!["start", "error"]);
    assert_eq!(result.stop_reason, StopReason::Error);
    assert_eq!(
        result.error_message.as_deref(),
        Some(PROXY_CONNECTION_CLOSED_MESSAGE)
    );
}

/// The counterpart of (1): a frame that is genuinely malformed — not a JSON
/// object, or a KNOWN type whose payload is wrong — is still a protocol error
/// that fails the stream. Only version skew is tolerated.
#[tokio::test]
async fn malformed_proxy_frames_still_fail_the_stream() {
    // First chunk decodes cleanly; the second carries a known type with a
    // missing required field ("delta"). Splitting the chunks keeps the events
    // decoded before the failure observable (one transport read parses its
    // whole chunk before the sink hands the events out).
    let provider = provider_with_chunks(vec![
        sse_body(&[
            json!({ "type": "start" }),
            json!({ "type": "text_start", "contentIndex": 0 }),
        ]),
        format!(
            "data: {}\n\n",
            json!({ "type": "text_delta", "contentIndex": 0 })
        ),
    ]);
    let (events, result) = collect(&provider).await;

    assert_eq!(labels(&events), vec!["start", "text_start", "error"]);
    assert_eq!(result.stop_reason, StopReason::Error);
    let message = result.error_message.clone().unwrap_or_default();
    assert!(
        message.contains("bad proxy event"),
        "a malformed known frame must report the decode failure, got: {message}"
    );
}

/// A `data:` line that is not a JSON object at all has no `type` tag to
/// fall back on, so it is malformed rather than unknown.
#[tokio::test]
async fn a_non_object_proxy_frame_is_malformed() {
    let provider = provider_with_chunks(vec![
        sse_body(&[
            json!({ "type": "start" }),
            json!({ "type": "text_start", "contentIndex": 0 }),
            json!({ "type": "text_delta", "contentIndex": 0, "delta": "ok " }),
        ]),
        "data: \"just a string\"\n\n".to_owned(),
    ]);
    let (events, result) = collect(&provider).await;

    assert_eq!(
        labels(&events),
        vec!["start", "text_start", "text_delta", "error"]
    );
    assert_eq!(result.stop_reason, StopReason::Error);
    // The chunk decodes into the decoder before the bad frame fails it, so the
    // failure path reports the partial rather than an empty message.
    assert_eq!(message_text(&result), "ok ");
    assert!(
        result
            .error_message
            .as_deref()
            .is_some_and(|message| message.contains("bad proxy event")),
        "unexpected error: {:?}",
        result.error_message
    );
}

// ---------------------------------------------------------------------------
// (2) A trailing decode failure preserves the accumulated partial (P2 item 2)
// ---------------------------------------------------------------------------

/// Upstream regression #8996: the terminal frame is not always
/// newline-terminated, so `finish` flushes the trailing bytes. When THAT flush
/// fails to decode, the old `decoder.finish(&mut tail)?` handed the error to
/// `finish_provider_stream`, which built a fresh empty error message — every
/// delta that had already streamed was gone from the terminal result. The
/// failure must instead go through `decoder.fail`, which keeps the partial.
#[tokio::test]
async fn a_failed_trailing_frame_preserves_the_partial() {
    let mut body = sse_body(&[
        json!({ "type": "start" }),
        json!({ "type": "text_start", "contentIndex": 0 }),
        json!({ "type": "text_delta", "contentIndex": 0, "delta": "streamed " }),
        json!({ "type": "text_delta", "contentIndex": 0, "delta": "before the crash" }),
    ]);
    // Trailing, NOT newline-terminated, and undecodable.
    body.push_str("data: {\"type\":\"text_delta\",\"contentIndex\":0");

    let (events, result) = collect(&provider_with_body(body)).await;

    // The events before the bad frame are delivered, then the terminal error.
    assert_eq!(
        labels(&events),
        vec!["start", "text_start", "text_delta", "text_delta", "error"]
    );
    assert_eq!(result.stop_reason, StopReason::Error);
    // The whole partial survives: this is the regression the audit filed.
    assert_eq!(
        message_text(&result),
        "streamed before the crash",
        "a failed trailing decode must not discard the accumulated partial"
    );
    assert!(
        result
            .error_message
            .as_deref()
            .is_some_and(|message| message.contains("bad proxy event")),
        "the decode failure must be reported, got: {:?}",
        result.error_message
    );
    // The event and the result agree on the same partial.
    let Some(AssistantMessageEvent::Error { error, .. }) = events.last() else {
        panic!("the last event must be the terminal error");
    };
    assert_eq!(message_text(error), "streamed before the crash");
}

/// The same guarantee on the standalone `stream_proxy` entry, which builds its
/// own stream instead of going through the provider: the fix lives in the
/// shared driver, so both entries must agree.
#[tokio::test]
async fn stream_proxy_also_preserves_the_partial_on_a_failed_trailing_frame() {
    let mut body = sse_body(&[
        json!({ "type": "start" }),
        json!({ "type": "text_start", "contentIndex": 0 }),
        json!({ "type": "text_delta", "contentIndex": 0, "delta": "direct entry" }),
    ]);
    body.push_str("data: not-json");
    let (transport, _requests) = ScriptedTransport::new(200, body);
    let options = ProxyStreamOptions::new("https://proxy.example.com", "test-token")
        .with_client(scripted_client(transport));

    let mut stream = Box::pin(stream_proxy(&model(), Context::default(), options));
    while stream.as_mut().next().await.is_some() {}
    let result = tokio::time::timeout(Duration::from_secs(5), stream.result())
        .await
        .expect("the stream must terminate")
        .expect("terminal result");

    assert_eq!(result.stop_reason, StopReason::Error);
    assert_eq!(message_text(&result), "direct entry");
    assert!(
        result
            .error_message
            .as_deref()
            .is_some_and(|message| message.contains("bad proxy event")),
        "unexpected error: {:?}",
        result.error_message
    );
}

// ---------------------------------------------------------------------------
// (3) `Debug` output never prints the credential (P2 item 3)
// ---------------------------------------------------------------------------

/// `ProxyEndpoint` derives `Debug` in the audit's first shape; a bearer token
/// must never reach a log line, a panic message, or a `{endpoint:?}` in a
/// test failure. Only its presence is reported.
#[test]
fn proxy_endpoint_debug_redacts_the_bearer_token() {
    let endpoint = ProxyEndpoint::new("https://genai.example.com", "super-secret-token");
    let rendered = format!("{endpoint:?}");

    assert!(
        !rendered.contains("super-secret-token"),
        "the bearer token must never appear in Debug output: {rendered}"
    );
    assert!(
        rendered.contains("https://genai.example.com"),
        "the URL is not a secret and stays visible: {rendered}"
    );
    assert!(
        rendered.contains("<redacted>"),
        "the token's presence must still be observable: {rendered}"
    );

    // An empty token reads as unset rather than as a redacted secret.
    let unset = format!("{:?}", ProxyEndpoint::new("https://genai.example.com", ""));
    assert!(unset.contains("<unset>"), "got: {unset}");
}

/// `ProxyStreamOptions` owns a bearer token too, and its `options` carries a
/// per-request `api_key` that a derived impl would print. Neither may leak.
#[test]
fn proxy_stream_options_debug_redacts_the_token_and_api_key() {
    let options = ProxyStreamOptions::new("https://genai.example.com", "proxy-bearer-secret")
        .with_stream_options(StreamOptions {
            api_key: Some("provider-api-key-secret".to_owned()),
            session_id: Some("visible-session".to_owned()),
            temperature: Some(0.25),
            ..Default::default()
        });
    let rendered = format!("{options:?}");

    assert!(
        !rendered.contains("proxy-bearer-secret"),
        "the bearer token must never appear in Debug output: {rendered}"
    );
    assert!(
        !rendered.contains("provider-api-key-secret"),
        "the per-request api key must never appear in Debug output: {rendered}"
    );
    // Non-secret diagnostic fields stay useful.
    assert!(rendered.contains("visible-session"), "got: {rendered}");
    assert!(rendered.contains("0.25"), "got: {rendered}");
    assert!(
        rendered.contains("https://genai.example.com"),
        "got: {rendered}"
    );
}

/// The redaction is `Debug`-only: the token still reaches the wire as the
/// request's bearer credential (redacting the struct must not silently break
/// authentication).
#[tokio::test]
async fn redacted_debug_still_sends_the_bearer_token() {
    let (transport, requests) =
        ScriptedTransport::new(200, sse_body(&[json!({ "type": "start" }), done_event()]));
    let options = ProxyStreamOptions::new("https://genai.example.com", "still-sent")
        .with_client(scripted_client(transport));

    let mut stream = Box::pin(stream_proxy(&model(), Context::default(), options));
    while stream.as_mut().next().await.is_some() {}
    let _ = stream.result().await;

    let requests = requests.lock().unwrap();
    assert_eq!(requests.len(), 1);
    assert_eq!(
        requests[0]
            .headers
            .get("authorization")
            .and_then(|value| value.to_str().ok()),
        Some("Bearer still-sent")
    );
}

// ---------------------------------------------------------------------------
// Guard rails for the surrounding contract (unchanged by this lane)
// ---------------------------------------------------------------------------

/// The bounded provider-event queue contract is load-bearing: a result-only
/// consumer must use `drain_and_result`, and the proxy driver must keep
/// feeding the stream through `push_await` so a slow consumer parks the body
/// read instead of growing the queue. This pins the observable half: a large
/// stream consumed behind a drain stays inside the capacity bound.
#[tokio::test]
async fn a_large_proxy_stream_stays_within_the_queue_bound() {
    let mut events = vec![
        json!({ "type": "start" }),
        json!({ "type": "text_start", "contentIndex": 0 }),
    ];
    for index in 0..200 {
        events.push(json!({
            "type": "text_delta",
            "contentIndex": 0,
            "delta": format!("d{index} "),
        }));
    }
    events.push(done_event());
    let provider = provider_with_body(sse_body(&events));

    let stream = provider.stream(&model(), Context::default(), StreamOptions::default());
    let result = tokio::time::timeout(Duration::from_secs(10), stream.drain_and_result())
        .await
        .expect("a result-only consumer must not park the producer")
        .expect("terminal result");

    assert_eq!(result.stop_reason, StopReason::Stop);
    assert!(
        stream.max_queue_depth() <= stream.capacity() + 1,
        "the bounded queue must hold: depth {} vs capacity {}",
        stream.max_queue_depth(),
        stream.capacity()
    );

    let text = message_text(&result);
    assert!(text.starts_with("d0 d1 "), "got: {text}");
    assert!(text.ends_with("d199 "), "got: {text}");
}

/// An aborted transport reports the abort reason (not a generic error) with
/// the partial preserved, the same way the decode-failure path now does.
#[tokio::test]
async fn an_aborted_proxy_stream_reports_the_abort_reason() {
    let body = sse_body(&[
        json!({ "type": "start" }),
        json!({ "type": "text_start", "contentIndex": 0 }),
        json!({ "type": "text_delta", "contentIndex": 0, "delta": "partial" }),
        json!({
            "type": "error",
            "reason": "aborted",
            "errorMessage": "client cancelled",
            "usage": usage()
        }),
    ]);
    let provider = provider_with_body(body);

    let (events, result) = collect(&provider).await;

    assert_eq!(
        labels(&events),
        vec!["start", "text_start", "text_delta", "error"]
    );
    assert_eq!(result.stop_reason, StopReason::Aborted);
    assert_eq!(message_text(&result), "partial");
    assert!(matches!(
        events.last(),
        Some(AssistantMessageEvent::Error {
            reason: ErrorReason::Aborted,
            ..
        })
    ));
    assert_eq!(
        result.usage,
        Usage {
            input: 3,
            output: 2,
            total_tokens: 5,
            ..Usage::default()
        }
    );
}
