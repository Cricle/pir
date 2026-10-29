//! Deterministic tests for the AI transport's HTTP seam
//! (`pir::ai::transport::HttpTransport`).
//!
//! Every case here used to need a local HTTP server and real time (see
//! `tests/suites/ai_transport_idle.rs` and `tests/suites/ai_provider_contracts.rs`,
//! which remain the production-path regression net). A scripted in-process
//! transport now pins the transport logic itself — retry classification and
//! attempt counts, the backoff delays handed to the retry-wait seam, the
//! `auto_retry_start`/`auto_retry_end` payloads, mid-stream failures,
//! abort/cancel and sink backpressure — with no sockets and no wall-clock
//! races.
//!
//! Clock note: pir has no clock seam (`tokio::time::pause` needs the
//! `test-util` feature, which the workspace does not enable), so "backoff
//! honoured" is pinned two ways: the exact delay the transport computes is
//! asserted where it is observable (`AutoRetryStart::delay_ms` and the
//! `RetryNotice` handed to the `on_retry` wait hook), and one test
//! wall-clock-verifies that the default `sleep_backoff` really waits out a
//! server-requested `Retry-After`.

use std::collections::{BTreeMap, VecDeque};
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use futures_util::Stream;
use pir::ai::error::{AiError, AiResult};
use pir::ai::transport::{
    ChunkSink, HttpBodyChunk, HttpRequest, HttpResponse, HttpTransport, ProviderRequestClient,
    ProviderRequestConfig,
};
use pir::ai::types::{
    AbortSignal, AutoRetryEnd, AutoRetryStart, ProviderStreamHooks, RetryNotice, RetryOutcome,
    RetryWaitHook,
};
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use reqwest::Method;
use tokio::sync::Notify;

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poison| poison.into_inner())
}

// ---------------------------------------------------------------------------
// Scripted transport
// ---------------------------------------------------------------------------

/// What one `execute` call should do.
enum Plan {
    /// A complete response head with a scripted body.
    Respond(Respond),
    /// The send itself fails (connection refused, TLS, DNS, ...).
    SendError(&'static str),
    /// The send itself fails with a typed terminal verdict (the shape a
    /// custom `options.fetch` transport may produce).
    SendTerminal(AiError),
    /// The response head never arrives (the execute future stays pending).
    Hang,
}

struct Respond {
    status: u16,
    headers: Vec<(&'static str, &'static str)>,
    /// Body frames in order; an `Err` frame is a mid-body read failure.
    items: Vec<Result<HttpBodyChunk, AiError>>,
    /// After the items, keep the body open instead of ending it.
    hang_after: bool,
}

fn respond(status: u16) -> Respond {
    Respond {
        status,
        headers: Vec::new(),
        items: Vec::new(),
        hang_after: false,
    }
}

fn chunk(text: &str) -> HttpBodyChunk {
    HttpBodyChunk::new(text.as_bytes().to_vec())
}

impl Respond {
    fn header(mut self, name: &'static str, value: &'static str) -> Self {
        self.headers.push((name, value));
        self
    }

    fn body(mut self, chunks: &[&str]) -> Self {
        self.items
            .extend(chunks.iter().copied().map(|text| Ok(chunk(text))));
        self
    }

    fn body_error(mut self, message: &str) -> Self {
        self.items.push(Err(AiError::Http(message.to_string())));
        self
    }

    /// A mid-body read failure carrying a typed terminal verdict (the shape a
    /// custom transport may produce).
    fn body_terminal_error(mut self, error: AiError) -> Self {
        self.items.push(Err(error));
        self
    }

    fn hang_after_items(mut self) -> Self {
        self.hang_after = true;
        self
    }
}

/// One request the fake was asked to execute.
#[derive(Clone)]
struct Recorded {
    method: Method,
    url: String,
    headers: HeaderMap,
    body: Vec<u8>,
}

impl Recorded {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).and_then(|value| value.to_str().ok())
    }
}

/// The seam fake: answers each attempt from a script in order, records every
/// request, and counts its calls and body polls.
struct FakeTransport {
    plans: Mutex<VecDeque<Plan>>,
    requests: Mutex<Vec<Recorded>>,
    executes: AtomicUsize,
    /// Shared with every scripted body; every `poll_next` counts.
    body_polls: Arc<AtomicUsize>,
}

impl FakeTransport {
    fn start(plans: Vec<Plan>) -> Arc<Self> {
        Arc::new(Self {
            plans: Mutex::new(plans.into()),
            requests: Mutex::new(Vec::new()),
            executes: AtomicUsize::new(0),
            body_polls: Arc::new(AtomicUsize::new(0)),
        })
    }

    fn requests(&self) -> Vec<Recorded> {
        lock(&self.requests).clone()
    }

    fn executes(&self) -> usize {
        self.executes.load(Ordering::SeqCst)
    }

    fn body_polls(&self) -> usize {
        self.body_polls.load(Ordering::SeqCst)
    }
}

impl HttpTransport for FakeTransport {
    fn execute(
        &self,
        request: HttpRequest,
    ) -> Pin<Box<dyn std::future::Future<Output = AiResult<HttpResponse>> + Send + '_>> {
        self.executes.fetch_add(1, Ordering::SeqCst);
        lock(&self.requests).push(Recorded {
            method: request.method.clone(),
            url: request.url.clone(),
            headers: request.headers.clone(),
            body: request.body.clone(),
        });
        let plan = lock(&self.plans)
            .pop_front()
            .expect("scripted transport ran out of planned responses");
        let polls = Arc::clone(&self.body_polls);
        Box::pin(async move {
            match plan {
                Plan::SendError(message) => Err(AiError::Http(message.to_string())),
                Plan::SendTerminal(error) => Err(error),
                Plan::Hang => std::future::pending::<AiResult<HttpResponse>>().await,
                Plan::Respond(respond) => {
                    let mut headers = HeaderMap::new();
                    for (name, value) in &respond.headers {
                        headers.insert(
                            HeaderName::from_static(name),
                            HeaderValue::from_static(value),
                        );
                    }
                    Ok(HttpResponse {
                        status: respond.status,
                        headers,
                        body: Box::pin(ScriptedBody {
                            items: respond.items.into(),
                            polls,
                            hang_after: respond.hang_after,
                        }),
                    })
                }
            }
        })
    }
}

/// Scripted body: items in order, then EOF — or `Pending` forever with
/// `hang_after` (a stalled body). Counts every poll so a test can prove the
/// read loop stopped pulling while a sink was parked.
struct ScriptedBody {
    items: VecDeque<Result<HttpBodyChunk, AiError>>,
    polls: Arc<AtomicUsize>,
    hang_after: bool,
}

impl Stream for ScriptedBody {
    type Item = Result<HttpBodyChunk, AiError>;

    fn poll_next(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        this.polls.fetch_add(1, Ordering::SeqCst);
        match this.items.pop_front() {
            Some(item) => Poll::Ready(Some(item)),
            None if this.hang_after => Poll::Pending,
            None => Poll::Ready(None),
        }
    }
}

// ---------------------------------------------------------------------------
// Sinks
// ---------------------------------------------------------------------------

/// Collecting sink (the awaitable entry always returns immediately).
#[derive(Default)]
struct CollectSink {
    bytes: Vec<u8>,
    sends: usize,
}

impl ChunkSink for CollectSink {
    fn send<'a>(
        &'a mut self,
        chunk: &'a [u8],
    ) -> Pin<Box<dyn std::future::Future<Output = AiResult<()>> + Send + 'a>> {
        Box::pin(async move {
            self.bytes.extend_from_slice(chunk);
            self.sends += 1;
            Ok(())
        })
    }
}

/// Sink whose FIRST send parks until the test releases the gate (a full
/// downstream queue); every send is counted.
struct GatedSink {
    bytes: Vec<u8>,
    sends: usize,
    arrived: Arc<Notify>,
    gate: Arc<Notify>,
    parked: bool,
}

impl GatedSink {
    fn new(arrived: Arc<Notify>, gate: Arc<Notify>) -> Self {
        Self {
            bytes: Vec::new(),
            sends: 0,
            arrived,
            gate,
            parked: false,
        }
    }
}

impl ChunkSink for GatedSink {
    fn send<'a>(
        &'a mut self,
        chunk: &'a [u8],
    ) -> Pin<Box<dyn std::future::Future<Output = AiResult<()>> + Send + 'a>> {
        Box::pin(async move {
            self.bytes.extend_from_slice(chunk);
            self.sends += 1;
            if !self.parked {
                self.parked = true;
                self.arrived.notify_one();
                self.gate.notified().await;
            }
            Ok(())
        })
    }
}

// ---------------------------------------------------------------------------
// Clients, hooks and assertions
// ---------------------------------------------------------------------------

fn config_with(
    max_retries: u32,
    max_retry_delay: Duration,
    http_idle_timeout: Option<Duration>,
) -> ProviderRequestConfig {
    ProviderRequestConfig {
        timeout: Duration::from_secs(5),
        max_response_bytes: 1024 * 1024,
        max_retries,
        max_retry_delay,
        proxy_url: None,
        http_idle_timeout,
    }
}

fn config(max_retries: u32, http_idle_timeout: Option<Duration>) -> ProviderRequestConfig {
    config_with(max_retries, Duration::from_secs(1), http_idle_timeout)
}

fn seam_client(
    transport: &Arc<FakeTransport>,
    config: ProviderRequestConfig,
) -> ProviderRequestClient {
    let transport = Arc::clone(transport);
    let transport: Arc<dyn HttpTransport> = transport;
    ProviderRequestClient::with_transport(config, transport)
}

/// `on_retry_start` / `on_retry_end` recorder (the wire-event payloads).
#[derive(Default)]
struct RetryLog {
    starts: Vec<AutoRetryStart>,
    ends: Vec<AutoRetryEnd>,
}

fn hook_log(shared: &Arc<Mutex<RetryLog>>) -> ProviderStreamHooks {
    let starts = Arc::clone(shared);
    let ends = Arc::clone(shared);
    ProviderStreamHooks {
        on_retry_start: Some(Arc::new(move |start| {
            lock(&starts).starts.push(start.clone());
        })),
        on_retry_end: Some(Arc::new(move |end| {
            lock(&ends).ends.push(end.clone());
        })),
        ..Default::default()
    }
}

/// Installs an instant `on_retry` wait that records the notice instead of
/// sleeping, so retry tests do not pay the backoff delay.
fn instant_retry_wait() -> (RetryWaitHook, Arc<Mutex<Vec<RetryNotice>>>) {
    let notices: Arc<Mutex<Vec<RetryNotice>>> = Arc::new(Mutex::new(Vec::new()));
    let recorder = Arc::clone(&notices);
    let hook = RetryWaitHook::new(Arc::new(move |notice| {
        lock(&recorder).push(notice);
        Box::pin(async { RetryOutcome::Proceed })
    }));
    (hook, notices)
}

/// Assert a delay lies in the jittered exponential band for a 0-based
/// attempt: `min(0.5 * 2^n, 8s)` shrunk by up to 25% (`exponential_backoff_delay`).
fn assert_backoff_band(attempt: u32, delay_ms: u64) {
    let base = 500_u64.saturating_mul(1_u64 << attempt.min(5)).min(8_000);
    let lower = base * 3 / 4;
    assert!(
        delay_ms >= lower && delay_ms <= base,
        "attempt {attempt}: delay {delay_ms}ms outside the jittered band [{lower}, {base}]"
    );
}

fn assert_error_contains(error: &AiError, needle: &str) {
    assert!(
        error.to_string().contains(needle),
        "error {error} does not contain {needle:?}"
    );
}

// ---------------------------------------------------------------------------
// Retry classification
// ---------------------------------------------------------------------------

/// 429 + `Retry-After-Ms` is retried with the exact attempt/budget/delay/error
/// payload the wire `auto_retry_start` event carries; `retry-after-ms` wins
/// over `retry-after`; the retried request reports `auto_retry_end {success}`;
/// the error status body never reaches the chunk sink.
#[tokio::test]
async fn retryable_status_retries_and_reports_auto_retry_start_and_end() {
    let transport = FakeTransport::start(vec![
        Plan::Respond(
            respond(429)
                .header("retry-after-ms", "5")
                .header("retry-after", "60")
                .body(&["slow down"]),
        ),
        Plan::Respond(
            respond(200)
                .header("content-type", "text/event-stream")
                .body(&["data: [DONE]\n\n"]),
        ),
    ]);
    let log = Arc::new(Mutex::new(RetryLog::default()));
    let (wait, notices) = instant_retry_wait();
    let mut hooks = hook_log(&log);
    hooks.on_retry = wait;
    let client = seam_client(&transport, config(2, None));

    let mut sink = CollectSink::default();
    let (status, headers) = client
        .request_streaming_with(
            Method::GET,
            "http://fake/stream",
            &BTreeMap::new(),
            Vec::new(),
            None,
            &hooks,
            &mut sink,
        )
        .await
        .expect("the retried request succeeds");

    assert_eq!(status, 200);
    assert_eq!(
        headers.get("content-type").map(String::as_str),
        Some("text/event-stream")
    );
    assert_eq!(
        sink.bytes,
        b"data: [DONE]\n\n".to_vec(),
        "only the successful attempt's body is delivered"
    );
    assert_eq!(transport.executes(), 2, "one retry + the final attempt");
    assert_eq!(
        lock(&notices).as_slice(),
        &[RetryNotice {
            attempt: 1,
            max_attempts: 2,
            delay_ms: 5,
        }],
        "retry-after-ms (5ms) wins over retry-after (60s)"
    );
    let log = lock(&log);
    assert_eq!(
        log.starts,
        vec![AutoRetryStart {
            attempt: 1,
            max_attempts: 2,
            delay_ms: 5,
            error_message: "HTTP 429".to_string(),
        }]
    );
    assert_eq!(
        log.ends,
        vec![AutoRetryEnd {
            success: true,
            attempt: 1,
            final_error: None,
        }]
    );
}

/// A non-retryable status buffers its body into the message, fails after the
/// first attempt, and fires no retry lifecycle at all. Uses the legacy
/// synchronous-closure entry to pin that its public signature still works
/// over the seam.
#[tokio::test]
async fn non_retryable_status_fails_once_with_the_error_body() {
    let transport = FakeTransport::start(vec![Plan::Respond(respond(400).body(&["bad request"]))]);
    let log = Arc::new(Mutex::new(RetryLog::default()));
    let hooks = hook_log(&log);
    let client = seam_client(&transport, config(2, None));

    let mut delivered = Vec::new();
    let error = client
        .request_streaming(
            Method::POST,
            "http://fake/v1/messages",
            &BTreeMap::new(),
            b"{}".to_vec(),
            None,
            &hooks,
            &mut |chunk| {
                delivered.extend_from_slice(chunk);
                Ok(())
            },
        )
        .await
        .expect_err("400 is fatal");

    assert!(matches!(error, AiError::NonRetryable(_)), "{error}");
    assert_eq!(
        error.to_string(),
        "non-retryable provider error: HTTP 400: bad request"
    );
    assert!(
        delivered.is_empty(),
        "error bodies are never delivered to the chunk sink"
    );
    assert_eq!(
        transport.executes(),
        1,
        "no retry follows a non-retryable status"
    );
    let log = lock(&log);
    assert!(
        log.starts.is_empty() && log.ends.is_empty(),
        "a one-shot failure has no retry lifecycle"
    );
}

/// The `x-should-retry` header keeps deciding over the status policy.
#[tokio::test]
async fn x_should_retry_header_overrides_the_status_policy() {
    let transport = FakeTransport::start(vec![Plan::Respond(
        respond(500).header("x-should-retry", "false").body(&["no"]),
    )]);
    let client = seam_client(&transport, config(2, None));
    let mut sink = CollectSink::default();
    let error = client
        .request_streaming_with(
            Method::GET,
            "http://fake",
            &BTreeMap::new(),
            Vec::new(),
            None,
            &ProviderStreamHooks::default(),
            &mut sink,
        )
        .await
        .expect_err("x-should-retry: false is fatal");
    assert!(
        matches!(error, AiError::NonRetryable(_)),
        "unexpected error: {error}"
    );
    assert_error_contains(&error, "HTTP 500");
    assert_eq!(transport.executes(), 1, "the header vetoes the 5xx retry");

    // `true` forces a retry even for a status the default policy rejects.
    let transport = FakeTransport::start(vec![
        Plan::Respond(
            respond(400)
                .header("x-should-retry", "true")
                .body(&["retry me"]),
        ),
        Plan::Respond(respond(200).body(&["ok"])),
    ]);
    let (wait, _) = instant_retry_wait();
    let hooks = ProviderStreamHooks {
        on_retry: wait,
        ..Default::default()
    };
    let client = seam_client(&transport, config(1, None));
    let mut sink = CollectSink::default();
    let (status, _) = client
        .request_streaming_with(
            Method::GET,
            "http://fake",
            &BTreeMap::new(),
            Vec::new(),
            None,
            &hooks,
            &mut sink,
        )
        .await
        .expect("the forced retry succeeds");
    assert_eq!(status, 200);
    assert_eq!(sink.bytes, b"ok".to_vec());
    assert_eq!(transport.executes(), 2, "the header forces one retry");
}

/// Send failures retry over the exact attempt count; exhausting the budget
/// reports `RetryExhausted { attempts }` and one failed `auto_retry_end`.
#[tokio::test]
async fn connection_errors_retry_and_exhaust_with_the_attempt_count() {
    // First failure, then success: one retry, both attempts carry the body.
    let transport = FakeTransport::start(vec![
        Plan::SendError("connection refused"),
        Plan::Respond(respond(200).body(&["ok"])),
    ]);
    let log = Arc::new(Mutex::new(RetryLog::default()));
    let (wait, _) = instant_retry_wait();
    let mut hooks = hook_log(&log);
    hooks.on_retry = wait;
    let client = seam_client(&transport, config(2, None));
    let mut sink = CollectSink::default();
    let (status, _) = client
        .request_streaming_with(
            Method::POST,
            "http://fake/v1",
            &BTreeMap::new(),
            b"payload".to_vec(),
            None,
            &hooks,
            &mut sink,
        )
        .await
        .expect("the retry succeeds");
    assert_eq!(status, 200);
    assert_eq!(transport.executes(), 2);
    let requests = transport.requests();
    assert_eq!(requests[0].body, b"payload".to_vec());
    assert_eq!(
        requests[0].body, requests[1].body,
        "the retry replays the same bytes"
    );
    // The guard lives in its own block: clippy's `await_holding_lock` is
    // scope-based and does not model the explicit `drop` this used to rely on.
    {
        let log_guard = lock(&log);
        assert_eq!(log_guard.starts.len(), 1);
        assert!(
            log_guard.starts[0]
                .error_message
                .contains("connection refused"),
            "the trigger summary carries the transport error, got: {}",
            log_guard.starts[0].error_message
        );
        assert_backoff_band(0, log_guard.starts[0].delay_ms);
        assert_eq!(
            log_guard.ends,
            vec![AutoRetryEnd {
                success: true,
                attempt: 1,
                final_error: None,
            }]
        );
    }

    // Always failing: 1 initial + 2 retries = 3 attempts.
    let transport = FakeTransport::start(vec![
        Plan::SendError("connection refused"),
        Plan::SendError("connection refused"),
        Plan::SendError("connection refused"),
    ]);
    let log = Arc::new(Mutex::new(RetryLog::default()));
    let (wait, _) = instant_retry_wait();
    let mut hooks = hook_log(&log);
    hooks.on_retry = wait;
    let client = seam_client(&transport, config(2, None));
    let mut sink = CollectSink::default();
    let error = client
        .request_streaming_with(
            Method::POST,
            "http://fake/v1",
            &BTreeMap::new(),
            Vec::new(),
            None,
            &hooks,
            &mut sink,
        )
        .await
        .expect_err("all attempts fail");
    let error_text = error.to_string();
    match error {
        AiError::RetryExhausted { attempts, message } => {
            assert_eq!(attempts, 3, "1 initial attempt + 2 retries");
            assert!(message.contains("connection refused"), "{message}");
        }
        other => panic!("unexpected error: {other}"),
    }
    assert_eq!(transport.executes(), 3);
    let log = lock(&log);
    assert_eq!(
        log.starts
            .iter()
            .map(|start| start.attempt)
            .collect::<Vec<_>>(),
        vec![1, 2],
        "one auto_retry_start per scheduled retry"
    );
    assert_eq!(
        log.ends,
        vec![AutoRetryEnd {
            success: false,
            attempt: 2,
            final_error: Some(error_text),
        }]
    );
}

/// A body failure before any chunk was delivered retries the whole request;
/// the same failure after a delivered chunk is fatal and reaches the caller
/// with the already-delivered chunk intact.
#[tokio::test]
async fn a_body_error_retries_only_before_the_first_delivered_chunk() {
    let transport = FakeTransport::start(vec![
        Plan::Respond(respond(200).body_error("connection reset mid-body")),
        Plan::Respond(respond(200).body(&["data: [DONE]\n\n"])),
    ]);
    let log = Arc::new(Mutex::new(RetryLog::default()));
    let (wait, _) = instant_retry_wait();
    let mut hooks = hook_log(&log);
    hooks.on_retry = wait;
    let client = seam_client(&transport, config(1, None));
    let mut sink = CollectSink::default();
    let (status, _) = client
        .request_streaming_with(
            Method::GET,
            "http://fake",
            &BTreeMap::new(),
            Vec::new(),
            None,
            &hooks,
            &mut sink,
        )
        .await
        .expect("the retried request succeeds");
    assert_eq!(status, 200);
    assert_eq!(sink.sends, 1, "chunks are never double-delivered");
    assert_eq!(sink.bytes, b"data: [DONE]\n\n".to_vec());
    assert_eq!(transport.executes(), 2);
    // The guard lives in its own block so clippy's scope-based
    // `await_holding_lock` sees it end before the test's next await.
    {
        let log = lock(&log);
        assert!(
            log.starts[0]
                .error_message
                .contains("connection reset mid-body"),
            "unexpected trigger: {}",
            log.starts[0].error_message
        );
    }

    // Now the same failure AFTER a chunk: fatal, no retry, chunk preserved.
    let transport = FakeTransport::start(vec![Plan::Respond(
        respond(200)
            .body(&["data: partial"])
            .body_error("connection reset mid-body"),
    )]);
    let log = Arc::new(Mutex::new(RetryLog::default()));
    let client = seam_client(&transport, config(2, None));
    let mut sink = CollectSink::default();
    let error = client
        .request_streaming_with(
            Method::GET,
            "http://fake",
            &BTreeMap::new(),
            Vec::new(),
            None,
            &hook_log(&log),
            &mut sink,
        )
        .await
        .expect_err("a delivered chunk makes the failure fatal");
    assert!(
        matches!(error, AiError::Http(_)),
        "unexpected error: {error}"
    );
    assert_error_contains(&error, "connection reset mid-body");
    assert_eq!(
        sink.bytes,
        b"data: partial".to_vec(),
        "the chunk delivered before the failure stays delivered"
    );
    assert_eq!(transport.executes(), 1, "no retry after a delivered chunk");
    assert!(
        lock(&log).starts.is_empty(),
        "a fatal mid-body error schedules no retry"
    );
}

/// A server-requested delay above `max_retry_delay` fails immediately, with
/// no wait and no re-send (upstream behavior).
#[tokio::test]
async fn an_over_cap_retry_after_fails_without_waiting_or_resending() {
    let transport = FakeTransport::start(vec![Plan::Respond(
        respond(429)
            .header("retry-after", "120")
            .body(&["slow down"]),
    )]);
    let log = Arc::new(Mutex::new(RetryLog::default()));
    let hooks = hook_log(&log);
    let client = seam_client(&transport, config(2, None));
    let mut sink = CollectSink::default();
    let error = client
        .request_streaming_with(
            Method::GET,
            "http://fake",
            &BTreeMap::new(),
            Vec::new(),
            None,
            &hooks,
            &mut sink,
        )
        .await
        .expect_err("an over-cap server delay is fatal");
    assert!(
        error
            .to_string()
            .contains("Server requested 120s retry delay (max: 1s)."),
        "{error}"
    );
    assert_eq!(
        transport.executes(),
        1,
        "no attempt follows the over-cap delay"
    );
    assert!(
        lock(&log).starts.is_empty(),
        "the over-cap delay fails before a retry wait is scheduled"
    );
}

// ---------------------------------------------------------------------------
// Backoff
// ---------------------------------------------------------------------------

/// The exponential ladder (`min(0.5 * 2^n, 8s)` minus <=25% jitter) is what
/// the retry-wait seam is handed, and `auto_retry_start` reports the same
/// exact `delay_ms` the sleep receives.
#[tokio::test]
async fn exponential_backoff_delays_follow_the_ladder() {
    let transport = FakeTransport::start(vec![
        Plan::SendError("boom one"),
        Plan::SendError("boom two"),
        Plan::Respond(respond(200).body(&["ok"])),
    ]);
    let log = Arc::new(Mutex::new(RetryLog::default()));
    let (wait, notices) = instant_retry_wait();
    let mut hooks = hook_log(&log);
    hooks.on_retry = wait;
    let client = seam_client(&transport, config(2, None));
    let mut sink = CollectSink::default();
    let (status, _) = client
        .request_streaming_with(
            Method::GET,
            "http://fake",
            &BTreeMap::new(),
            Vec::new(),
            None,
            &hooks,
            &mut sink,
        )
        .await
        .expect("the third attempt succeeds");
    assert_eq!(status, 200);
    assert_eq!(transport.executes(), 3);

    let notices = lock(&notices).clone();
    assert_eq!(notices.len(), 2, "one wait per scheduled retry");
    for (index, notice) in notices.iter().enumerate() {
        let attempt = index as u32;
        assert_eq!(notice.attempt, attempt + 1, "1-based retry ordinal");
        assert_eq!(notice.max_attempts, 2);
        assert_backoff_band(attempt, notice.delay_ms);
    }
    let log = lock(&log);
    assert_eq!(
        log.starts
            .iter()
            .map(|start| start.delay_ms)
            .collect::<Vec<_>>(),
        notices
            .iter()
            .map(|notice| notice.delay_ms)
            .collect::<Vec<_>>(),
        "auto_retry_start.delay_ms is the delay handed to the retry wait"
    );
    assert_eq!(
        log.ends,
        vec![AutoRetryEnd {
            success: true,
            attempt: 2,
            final_error: None,
        }]
    );
}

/// Without a wait hook the transport really sleeps the server-requested
/// delay (no clock seam: verified on the wall clock, under the only
/// lower-bound direction that is race-free).
#[tokio::test]
async fn the_default_retry_wait_actually_sleeps() {
    let transport = FakeTransport::start(vec![
        Plan::Respond(
            respond(503)
                .header("retry-after-ms", "150")
                .body(&["unavailable"]),
        ),
        Plan::Respond(respond(200).body(&["ok"])),
    ]);
    let client = seam_client(&transport, config(1, None));
    let mut sink = CollectSink::default();
    let started = Instant::now();
    let (status, _) = client
        .request_streaming_with(
            Method::GET,
            "http://fake",
            &BTreeMap::new(),
            Vec::new(),
            None,
            &ProviderStreamHooks::default(),
            &mut sink,
        )
        .await
        .expect("the retry succeeds");
    let elapsed = started.elapsed();
    assert_eq!(status, 200);
    assert_eq!(transport.executes(), 2);
    assert!(
        elapsed >= Duration::from_millis(150),
        "the transport must wait out Retry-After before re-sending (elapsed {elapsed:?})"
    );
}

// ---------------------------------------------------------------------------
// Stalls, aborts and backpressure
// ---------------------------------------------------------------------------

/// A response head that never arrives is a pre-body idle timeout and flows
/// through the retry classification like any other send failure.
#[tokio::test]
async fn a_head_that_never_arrives_idle_times_out_and_retries() {
    let transport =
        FakeTransport::start(vec![Plan::Hang, Plan::Respond(respond(200).body(&["ok"]))]);
    let log = Arc::new(Mutex::new(RetryLog::default()));
    let (wait, _) = instant_retry_wait();
    let mut hooks = hook_log(&log);
    hooks.on_retry = wait;
    let client = seam_client(&transport, config(1, Some(Duration::from_millis(50))));
    let mut sink = CollectSink::default();
    let (status, _) = client
        .request_streaming_with(
            Method::GET,
            "http://fake",
            &BTreeMap::new(),
            Vec::new(),
            None,
            &hooks,
            &mut sink,
        )
        .await
        .expect("the retry succeeds");
    assert_eq!(status, 200);
    assert_eq!(transport.executes(), 2);
    assert_eq!(sink.bytes, b"ok".to_vec());
    let log = lock(&log);
    assert_eq!(log.starts.len(), 1);
    assert!(
        log.starts[0].error_message.contains("idle timeout"),
        "unexpected trigger: {}",
        log.starts[0].error_message
    );
}

/// A body that stops making progress mid-stream hits the per-read idle
/// timeout; chunks delivered before the stall stay delivered.
#[tokio::test]
async fn a_body_that_never_completes_hits_the_body_idle_timeout() {
    let transport = FakeTransport::start(vec![Plan::Respond(
        respond(200).body(&["data: partial"]).hang_after_items(),
    )]);
    let client = seam_client(&transport, config(0, Some(Duration::from_millis(50))));
    let mut sink = CollectSink::default();
    let error = client
        .request_streaming_with(
            Method::GET,
            "http://fake",
            &BTreeMap::new(),
            Vec::new(),
            None,
            &ProviderStreamHooks::default(),
            &mut sink,
        )
        .await
        .expect_err("a stalled body is an idle timeout");
    assert!(
        matches!(error, AiError::Http(_)),
        "unexpected error: {error}"
    );
    assert_error_contains(&error, "HTTP body idle timeout");
    assert_eq!(sink.bytes, b"data: partial".to_vec());
    assert_eq!(transport.executes(), 1, "no retry budget is configured");
}

/// Aborting mid-stream stops the read loop at the next chunk boundary: the
/// parked sink is released, the requester sees `Aborted`, and the scripted
/// body is never polled for the second chunk.
#[tokio::test]
async fn abort_mid_stream_stops_before_the_next_chunk() {
    let transport = FakeTransport::start(vec![Plan::Respond(
        respond(200).body(&["chunk one", "chunk two"]),
    )]);
    let client = seam_client(&transport, config(2, None));
    let signal = AbortSignal::new();
    let arrived = Arc::new(Notify::new());
    let gate = Arc::new(Notify::new());
    let mut sink = GatedSink::new(Arc::clone(&arrived), Arc::clone(&gate));

    let aborter = signal.clone();
    let coordinator = async move {
        arrived.notified().await;
        // The transport is parked in `ChunkSink::send` right now: abort, then
        // release the sink so the loop can reach its abort check.
        aborter.abort();
        gate.notify_one();
    };
    let headers = BTreeMap::new();
    let hooks = ProviderStreamHooks::default();
    let request = client.request_streaming_with(
        Method::GET,
        "http://fake",
        &headers,
        Vec::new(),
        Some(&signal),
        &hooks,
        &mut sink,
    );
    let (result, ()) = tokio::join!(request, coordinator);

    let error = result.expect_err("the aborted request fails");
    assert!(
        matches!(error, AiError::Aborted),
        "unexpected error: {error}"
    );
    assert_eq!(sink.sends, 1, "only the parked chunk reached the sink");
    assert_eq!(sink.bytes, b"chunk one".to_vec());
    assert_eq!(
        transport.body_polls(),
        1,
        "the next chunk must not be pulled after the abort"
    );
    assert_eq!(transport.executes(), 1);
}

/// A caller abort cancels an in-flight response-header wait immediately: the
/// request does not stay parked on a silent server until the idle/total timer
/// fires (upstream `fetch` rejects as soon as its signal aborts).
#[tokio::test]
async fn abort_while_waiting_for_the_response_head_is_observed_immediately() {
    let transport = FakeTransport::start(vec![Plan::Hang]);
    // No idle/total timeout and a retry budget: only the abort can end this.
    let client = seam_client(&transport, config(2, None));
    let signal = AbortSignal::new();
    let aborter = signal.clone();
    let coordinator = async move {
        tokio::time::sleep(Duration::from_millis(50)).await;
        aborter.abort();
    };
    let mut sink = CollectSink::default();
    let headers = BTreeMap::new();
    let hooks = ProviderStreamHooks::default();
    let request = client.request_streaming_with(
        Method::GET,
        "http://fake",
        &headers,
        Vec::new(),
        Some(&signal),
        &hooks,
        &mut sink,
    );
    let (result, ()) = tokio::time::timeout(Duration::from_secs(2), async {
        tokio::join!(request, coordinator)
    })
    .await
    .expect("the abort must cancel the in-flight header wait");
    let error = result.expect_err("the aborted request fails");
    assert!(
        matches!(error, AiError::Aborted),
        "unexpected error: {error}"
    );
    assert_eq!(transport.executes(), 1, "an aborted attempt is not re-sent");
}

/// A caller abort cancels an in-flight body read immediately, even when the
/// provider has gone silent and no idle timer is configured.
#[tokio::test]
async fn abort_while_waiting_for_the_next_body_chunk_is_observed_immediately() {
    let transport = FakeTransport::start(vec![Plan::Respond(
        respond(200).body(&["one"]).hang_after_items(),
    )]);
    let client = seam_client(&transport, config(0, None));
    let signal = AbortSignal::new();
    let aborter = signal.clone();
    let coordinator = async move {
        tokio::time::sleep(Duration::from_millis(50)).await;
        aborter.abort();
    };
    let mut sink = CollectSink::default();
    let headers = BTreeMap::new();
    let hooks = ProviderStreamHooks::default();
    let request = client.request_streaming_with(
        Method::GET,
        "http://fake",
        &headers,
        Vec::new(),
        Some(&signal),
        &hooks,
        &mut sink,
    );
    let (result, ()) = tokio::time::timeout(Duration::from_secs(2), async {
        tokio::join!(request, coordinator)
    })
    .await
    .expect("the abort must cancel the in-flight body read");
    let error = result.expect_err("the aborted request fails");
    assert!(
        matches!(error, AiError::Aborted),
        "unexpected error: {error}"
    );
    assert_eq!(transport.executes(), 1);
    assert!(
        sink.bytes.is_empty() || sink.bytes == b"one".to_vec(),
        "the delivered bytes are the partial body, got {:?}",
        sink.bytes
    );
}

/// A terminal verdict returned by the transport seam (`Aborted` /
/// `NonRetryable`, e.g. from a custom `options.fetch` transport) is not
/// retried: it keeps its type instead of becoming a `RetryExhausted` after
/// duplicate requests and backoff sleeps.
#[tokio::test]
async fn a_terminal_send_verdict_is_not_retried() {
    let transport = FakeTransport::start(vec![Plan::SendTerminal(AiError::NonRetryable(
        "request rejected".to_string(),
    ))]);
    let log = Arc::new(Mutex::new(RetryLog::default()));
    let hooks = hook_log(&log);
    let client = seam_client(&transport, config(3, None));
    let mut sink = CollectSink::default();
    let error = client
        .request_streaming_with(
            Method::GET,
            "http://fake",
            &BTreeMap::new(),
            Vec::new(),
            None,
            &hooks,
            &mut sink,
        )
        .await
        .expect_err("the terminal verdict fails the request");
    assert!(
        matches!(error, AiError::NonRetryable(_)),
        "unexpected error: {error}"
    );
    assert_eq!(
        transport.executes(),
        1,
        "the terminal verdict is not re-sent"
    );
    let log = lock(&log);
    assert!(log.starts.is_empty(), "no retry may be scheduled");
    assert!(log.ends.is_empty());
}

/// A terminal mid-body failure before the first delivered chunk is not
/// retried either (the same verdict rule as the send path).
#[tokio::test]
async fn a_terminal_body_verdict_is_not_retried() {
    let transport = FakeTransport::start(vec![Plan::Respond(
        respond(200).body_terminal_error(AiError::NonRetryable("stream rejected".to_string())),
    )]);
    let log = Arc::new(Mutex::new(RetryLog::default()));
    let hooks = hook_log(&log);
    let client = seam_client(&transport, config(3, None));
    let mut sink = CollectSink::default();
    let error = client
        .request_streaming_with(
            Method::GET,
            "http://fake",
            &BTreeMap::new(),
            Vec::new(),
            None,
            &hooks,
            &mut sink,
        )
        .await
        .expect_err("the terminal verdict fails the request");
    assert!(
        matches!(error, AiError::NonRetryable(_)),
        "unexpected error: {error}"
    );
    assert_eq!(
        transport.executes(),
        1,
        "the terminal verdict is not re-sent"
    );
    assert!(lock(&log).starts.is_empty(), "no retry may be scheduled");
}

/// A full downstream queue pauses the body reader: while the sink is parked,
/// the scripted body is not polled for the next chunk; releasing the sink
/// resumes the read in order.
#[tokio::test]
async fn a_full_sink_pauses_the_body_reader() {
    let transport = FakeTransport::start(vec![Plan::Respond(
        respond(200).body(&["chunk one", "chunk two"]),
    )]);
    let client = seam_client(&transport, config(0, None));
    let arrived = Arc::new(Notify::new());
    let gate = Arc::new(Notify::new());
    let mut sink = GatedSink::new(Arc::clone(&arrived), Arc::clone(&gate));

    let checked = Arc::clone(&transport);
    let coordinator = async move {
        arrived.notified().await;
        // Give a wrongly eager reader room to pull the next frame, then prove
        // it did not: `body_polls` counts every `poll_next`.
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(
            checked.body_polls(),
            1,
            "the body reader must stay paused while the sink is full"
        );
        assert_eq!(checked.executes(), 1, "no retry while a chunk is in flight");
        gate.notify_one();
    };
    let headers = BTreeMap::new();
    let hooks = ProviderStreamHooks::default();
    let request = client.request_streaming_with(
        Method::GET,
        "http://fake",
        &headers,
        Vec::new(),
        None,
        &hooks,
        &mut sink,
    );
    let (result, ()) = tokio::join!(request, coordinator);
    result.expect("the request completes once the sink accepts");

    assert_eq!(sink.sends, 2);
    assert_eq!(sink.bytes, b"chunk onechunk two".to_vec());
    assert_eq!(
        transport.body_polls(),
        3,
        "two frames plus the EOF poll, after the sink drained"
    );
}

/// Aborting during a retry wait cuts the wait short and reports the retry as
/// cancelled through `auto_retry_end` (upstream `_finishCancelledRetry`).
#[tokio::test]
async fn abort_during_the_retry_wait_reports_a_cancelled_retry() {
    let transport = FakeTransport::start(vec![
        Plan::Respond(
            respond(429)
                .header("retry-after", "30")
                .body(&["slow down"]),
        ),
        Plan::Respond(respond(200).body(&["never reached"])),
    ]);
    let log = Arc::new(Mutex::new(RetryLog::default()));
    let started = Arc::new(Notify::new());
    let starts = Arc::clone(&log);
    let notifier = Arc::clone(&started);
    let ends = Arc::clone(&log);
    let hooks = ProviderStreamHooks {
        on_retry_start: Some(Arc::new(move |start| {
            lock(&starts).starts.push(start.clone());
            notifier.notify_one();
        })),
        on_retry_end: Some(Arc::new(move |end| {
            lock(&ends).ends.push(end.clone());
        })),
        ..Default::default()
    };
    let client = seam_client(&transport, config_with(1, Duration::from_secs(60), None));
    let signal = AbortSignal::new();
    let aborter = signal.clone();
    let coordinator = async move {
        started.notified().await;
        aborter.abort();
    };
    let mut sink = CollectSink::default();
    let headers = BTreeMap::new();
    let request = client.request_streaming_with(
        Method::GET,
        "http://fake",
        &headers,
        Vec::new(),
        Some(&signal),
        &hooks,
        &mut sink,
    );
    let (result, ()) = tokio::join!(request, coordinator);

    let error = result.expect_err("the aborted retry fails");
    assert!(
        matches!(error, AiError::Aborted),
        "unexpected error: {error}"
    );
    assert_eq!(
        transport.executes(),
        1,
        "the cancelled retry never re-sends"
    );
    assert_eq!(
        lock(&log).ends,
        vec![AutoRetryEnd {
            success: false,
            attempt: 1,
            final_error: Some("Retry cancelled".to_string()),
        }]
    );
}

// ---------------------------------------------------------------------------
// Seam plumbing
// ---------------------------------------------------------------------------

/// `post_json_streaming_with` serializes once and replays the identical bytes
/// through the seam; validated headers reach the transport unchanged.
#[tokio::test]
async fn post_json_streaming_replays_the_serialized_body_and_headers() {
    let transport = FakeTransport::start(vec![
        Plan::Respond(respond(429).header("retry-after-ms", "1").body(&["slow"])),
        Plan::Respond(respond(200).body(&["ok"])),
    ]);
    let (wait, _) = instant_retry_wait();
    let hooks = ProviderStreamHooks {
        on_retry: wait,
        ..Default::default()
    };
    let client = seam_client(&transport, config(1, None));
    let payload = serde_json::json!({
        "model": "mock-model",
        "messages": [{"role": "user", "content": "x".repeat(256)}],
    });
    let mut headers = BTreeMap::new();
    headers.insert("authorization".to_string(), "Bearer test-key".to_string());
    let mut sink = CollectSink::default();
    let (status, _) = client
        .post_json_streaming_with(
            "http://fake/v1/chat/completions",
            &headers,
            &payload,
            None,
            &hooks,
            &mut sink,
        )
        .await
        .expect("the retried POST succeeds");
    assert_eq!(status, 200);
    assert_eq!(sink.bytes, b"ok".to_vec());
    let requests = transport.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0].method, Method::POST);
    assert_eq!(requests[0].url, "http://fake/v1/chat/completions");
    assert_eq!(
        requests[0].header("authorization"),
        Some("Bearer test-key"),
        "validated headers reach the transport verbatim"
    );
    assert_eq!(
        requests[0].body, requests[1].body,
        "the retry replays the same serialized bytes"
    );
    assert_eq!(
        requests[0].body,
        serde_json::to_vec(&payload).expect("serialize")
    );
    let diagnostics = client.payload_diagnostics();
    assert_eq!(
        (diagnostics.allocations, diagnostics.copies),
        (1, 1),
        "one serialization and exactly one keep-for-retry copy"
    );
}

/// The transport error also reaches the adapter's caller-facing stream: a real
/// OpenAI-compatible provider streamed over the fake terminates its
/// `AssistantMessageEventStream` with an Error carrying the mid-body message
/// (after the deltas delivered before the failure).
#[tokio::test]
async fn a_mid_body_error_reaches_the_provider_event_stream() {
    use pir::ai::api::openai::OpenAiCompatibleProvider;
    use pir::ai::provider::Provider;
    use pir::ai::types::{
        ContentBlock, Context, Message, Model, ModelCost, ModelCostRates, StopReason,
        StreamOptions, TextContent, UserMessage, UserRole,
    };

    let transport = FakeTransport::start(vec![Plan::Respond(
        respond(200)
            .header("content-type", "text/event-stream")
            .body(&[
                "data: {\"id\":\"chatcmpl-1\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"Hello\"},\"finish_reason\":null}]}\n\n",
            ])
            .body_error("connection reset mid-body"),
    )]);
    let client = seam_client(&transport, config(0, None));
    let base = "http://fake/v1";
    let provider = OpenAiCompatibleProvider::new(
        "openai",
        base,
        vec![Model {
            id: "gpt-4o-mini".into(),
            name: "GPT-4o mini".into(),
            api: "openai-chat".into(),
            provider: "openai".into(),
            base_url: base.to_string(),
            reasoning: false,
            thinking_level_map: None,
            input: vec!["text".into()],
            cost: ModelCost {
                rates: ModelCostRates {
                    input: 0.15,
                    output: 0.6,
                    cache_read: 0.075,
                    cache_write: 0.15,
                },
                tiers: Vec::new(),
            },
            context_window: 128_000,
            max_tokens: 16_384,
            sampling_params: None,
            headers: None,
            thinking_token_budget_field: None,
            input_limits: None,
            prompt_cache: None,
            sampling_params_by_thinking_level: None,
            model_type: None,
            compat: None,
        }],
    )
    .expect("provider")
    .with_client(client);
    let model = provider.get_models()[0].clone();
    let context = Context {
        messages: vec![Message::User(UserMessage {
            role: UserRole::User,
            content: vec![ContentBlock::Text(TextContent {
                text: "hi".into(),
                text_signature: None,
            })],
            tools_added: None,
            tools_removed: None,
            timestamp: 1,
            sections: None,
        })],
        ..Context::default()
    };
    let stream = provider.stream(
        &model,
        context,
        StreamOptions {
            api_key: Some("test-key".into()),
            ..StreamOptions::default()
        },
    );
    let final_message = stream.result().await.expect("the stream terminates");
    assert_eq!(final_message.stop_reason, StopReason::Error);
    let message = final_message.error_message.as_deref().unwrap_or("");
    assert!(
        message.contains("connection reset mid-body"),
        "the caller-facing error must carry the transport message, got: {message}"
    );
    assert_eq!(
        transport.executes(),
        1,
        "a mid-body failure after a delivered chunk is not retried"
    );
}
