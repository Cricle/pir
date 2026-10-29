//! Adapter producer liveness (W13-Q): the fix W13-N landed in
//! `AssistantMessageEventStream` has to reach every ADAPTER spawn site, not
//! only the registry.
//!
//! W13-N gave the stream consumer liveness: `Clone`/`new()` are CONSUMER
//! handles, `producer()` is a non-counting producer handle, and `push_await`
//! returns `false` once every consumer handle is gone. The registry
//! (`ProviderRegistry::stream` / `stream_unchecked`) and the JS bridge were
//! converted; each provider adapter still forwarded its events through a plain
//! `output.clone()`, so its own forwarding task counted as a consumer and
//! could park forever on a full queue after the real consumer left.
//!
//! These tests pin the three properties the adapter conversion depends on:
//!
//! (a) a handle from `.producer()` never keeps the stream alive: dropping the
//!     last consumer makes it observable (`consumer_count() == 0`,
//!     `is_abandoned()`, and every `push_await` reports closure);
//! (b) an adapter-shaped forwarding loop stops when `push_await` returns
//!     `false` and releases its source instead of parking — driven twice: as a
//!     pure fixture loop, and end-to-end through a real adapter (`ProxyProvider`
//!     over the scripted in-process transport) where the HTTP body read must be
//!     released, not drained;
//! (c) a normally consumed stream is untouched: same event order, same terminal
//!     message, capacity bound intact, never abandoned.
//!
//! Everything is hermetic (no sockets, no external network, no writes under
//! `~/.pi`) and every wait is bounded by a timeout, so a regression FAILS
//! instead of hanging the harness.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use futures_util::StreamExt;
use reqwest::header::HeaderMap;
use serde_json::{json, Value};

use pir::agent::tool_execution::BoxFuture;
use pir::ai::error::{AiError, AiResult};
use pir::ai::event_stream::AssistantMessageEventStream;
use pir::ai::provider::Provider;
use pir::ai::transport::{
    HttpBodyChunk, HttpRequest, HttpResponse, HttpTransport, ProviderRequestClient,
    ProviderRequestConfig,
};
use pir::ai::types::{
    AssistantMessage, AssistantMessageEvent, ContentBlock, Context, Model, ModelCost,
    ModelCostRates, StreamOptions, TerminalReason,
};
use pir::sdk::{ProxyEndpoint, ProxyProvider};

// ---------------------------------------------------------------------------
// Shared fixtures
// ---------------------------------------------------------------------------

fn message(text: &str) -> AssistantMessage {
    AssistantMessage {
        content: vec![ContentBlock::Text(pir::ai::types::TextContent {
            text: text.into(),
            text_signature: None,
        })],
        ..AssistantMessage::default()
    }
}

fn delta(index: usize) -> AssistantMessageEvent {
    let token = format!("d{index} ");
    AssistantMessageEvent::TextDelta {
        content_index: 0,
        delta: token.clone(),
        partial: Arc::new(message(&token)),
    }
}

fn delta_text(event: &AssistantMessageEvent) -> String {
    match event {
        AssistantMessageEvent::TextDelta { delta, .. } => delta.clone(),
        other => panic!("expected a text delta, got {other:?}"),
    }
}

fn done_event(text: &str) -> AssistantMessageEvent {
    AssistantMessageEventStream::done_event(TerminalReason::Stop, message(text))
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

/// Poll `condition` until it holds or the guard expires (never waits forever).
async fn wait_until(mut condition: impl FnMut() -> bool) -> bool {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        if condition() {
            return true;
        }
        if tokio::time::Instant::now() >= deadline {
            return condition();
        }
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
}

// ---------------------------------------------------------------------------
// (a) a producer handle never keeps the stream alive
// ---------------------------------------------------------------------------

/// The exact defect the adapter sites had: the forwarding task held a
/// consumer-counting clone, so the real consumer leaving was invisible.
/// A `.producer()` handle must leave the stream abandoned the moment the
/// caller's handle drops — the producer itself can outlive it.
#[tokio::test]
async fn producer_handle_does_not_keep_the_stream_alive_after_the_consumer_drops() {
    let stream = AssistantMessageEventStream::with_capacity(4);
    let producer = stream.producer();
    assert_eq!(
        stream.consumer_count(),
        1,
        "only the caller's handle is a consumer"
    );
    assert_eq!(
        producer.consumer_count(),
        1,
        "both handles see the same inner count"
    );

    // Producer-side traffic while the consumer is still alive is normal.
    assert!(
        tokio::time::timeout(Duration::from_secs(2), producer.push_await(delta(0)))
            .await
            .expect("a live consumer accepts the push")
    );

    drop(stream);

    assert_eq!(producer.consumer_count(), 0, "the producer never counted");
    assert!(
        producer.is_abandoned(),
        "dropping the last consumer abandons the stream even while the producer holds a handle"
    );
    assert!(
        !tokio::time::timeout(Duration::from_secs(2), producer.push_await(delta(1)))
            .await
            .expect("an abandoned stream must not park the producer"),
        "push_await reports closure instead of accepting an unread event"
    );
    assert_eq!(
        producer.pending_count(),
        1,
        "the refused event was dropped, the queue was not touched"
    );

    // Holding the producer for the rest of the request's life changes nothing
    // and an abandon is not a termination the producer caused.
    assert!(!producer.is_done(), "abandonment is not termination");
}

// ---------------------------------------------------------------------------
// (b1) an adapter-shaped forwarding loop stops on `false`
// ---------------------------------------------------------------------------

/// The pure shape every adapter's forwarding loop has: pull an event from a
/// source, push it with backpressure, and on `false` stop and release the
/// source (every adapter's abort path ends the inner stream / drops the
/// transport body the same way). Returns how many events it forwarded.
async fn forward_until_abandoned(
    source: &mut tokio::sync::mpsc::Receiver<AssistantMessageEvent>,
    sink: AssistantMessageEventStream,
) -> usize {
    let mut forwarded = 0usize;
    while let Some(event) = source.recv().await {
        if !sink.push_await(event).await {
            // Nobody is reading: stop promptly instead of parking, and let the
            // source drop (the adapters' `inner.end(None)` equivalent).
            break;
        }
        forwarded += 1;
    }
    forwarded
}

#[tokio::test]
async fn adapter_shaped_forwarding_loop_stops_and_releases_its_source_on_closure() {
    let stream = AssistantMessageEventStream::with_capacity(2);
    let sink = stream.producer();
    let (source_tx, mut source_rx) = tokio::sync::mpsc::channel::<AssistantMessageEvent>(16);
    for index in 0..10 {
        source_tx
            .send(delta(index))
            .await
            .expect("channel has room");
    }
    source_tx
        .send(done_event("late"))
        .await
        .expect("channel has room");

    let task = tokio::spawn(async move { forward_until_abandoned(&mut source_rx, sink).await });

    assert!(
        wait_until(|| stream.pending_count() == 2).await,
        "the loop fills the queue to capacity"
    );
    assert!(
        wait_until(|| stream.push_wait_count() >= 1).await,
        "the loop parks on the full queue (backpressure still applies)"
    );

    drop(stream);

    let forwarded = tokio::time::timeout(Duration::from_secs(2), task)
        .await
        .expect("the forwarding loop must stop instead of parking forever")
        .expect("forwarding task");
    assert_eq!(forwarded, 2, "only the events that fit were forwarded");
    // Releasing the source is the abort path: the task owned the receiver, so
    // the source now observes a closed channel — the same signal that ends a
    // real transport's read loop.
    assert!(
        source_tx.is_closed(),
        "the abandoned loop must release its source"
    );
}

// ---------------------------------------------------------------------------
// (b2) end-to-end: a real adapter releases the HTTP body on closure
// ---------------------------------------------------------------------------

/// Scripted in-process transport (the transport's public test seam): the body
/// counts the chunks the adapter actually read and reports when the body
/// stream itself was dropped, so a test can prove the read loop stopped early
/// instead of draining the rest of the response.
struct ChunkCountingTransport {
    chunks: Vec<String>,
    polled: Arc<AtomicUsize>,
    body_dropped: Arc<AtomicBool>,
}

/// Sets the flag when the body stream (which owns it) is dropped.
struct DropSignal(Arc<AtomicBool>);

impl Drop for DropSignal {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

impl HttpTransport for ChunkCountingTransport {
    fn execute(&self, _request: HttpRequest) -> BoxFuture<'_, AiResult<HttpResponse>> {
        let chunks = self.chunks.clone();
        let polled = Arc::clone(&self.polled);
        let body_dropped = Arc::clone(&self.body_dropped);
        Box::pin(async move {
            // The counter and the drop signal ride the stream's own state: an
            // `FnMut` step function may only take them from its arguments (a
            // captured Arc would be moved out on the first poll), and keeping
            // them state-owned keeps the body `'static`.
            let body = futures_util::stream::unfold(
                (0usize, chunks, DropSignal(body_dropped), polled),
                |(index, chunks, signal, polled)| async move {
                    let chunk = chunks.get(index)?.clone();
                    polled.fetch_add(1, Ordering::SeqCst);
                    Some((
                        Ok::<HttpBodyChunk, AiError>(HttpBodyChunk::new(chunk.into_bytes())),
                        (index + 1, chunks, signal, polled),
                    ))
                },
            );
            Ok(HttpResponse {
                status: 200,
                headers: HeaderMap::new(),
                body: Box::pin(body),
            })
        })
    }
}

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

fn scripted_client(transport: ChunkCountingTransport) -> ProviderRequestClient {
    ProviderRequestClient::with_transport(
        ProviderRequestConfig {
            timeout: Duration::from_secs(5),
            max_response_bytes: 1 << 22,
            max_retries: 0,
            max_retry_delay: Duration::ZERO,
            proxy_url: None,
            http_idle_timeout: Some(Duration::from_secs(5)),
        },
        Arc::new(transport),
    )
}

fn proxy_event(value: Value) -> String {
    format!("data: {value}\n\n")
}

/// A long proxy stream: one delta per frame, so each consumed event is one
/// body chunk (the count is the read-loop progress).
fn long_proxy_body(deltas: usize) -> Vec<String> {
    let mut chunks = vec![
        proxy_event(json!({ "type": "start" })),
        proxy_event(json!({ "type": "text_start", "contentIndex": 0 })),
    ];
    for index in 0..deltas {
        chunks.push(proxy_event(json!({
            "type": "text_delta",
            "contentIndex": 0,
            "delta": format!("d{index} "),
        })));
    }
    chunks.push(proxy_event(json!({
        "type": "done",
        "reason": "stop",
        "usage": {
            "input": 1,
            "output": 1,
            "cacheRead": 0,
            "cacheWrite": 0,
            "totalTokens": 2,
            "cost": { "input": 0.0, "output": 0.0, "cacheRead": 0.0, "cacheWrite": 0.0, "total": 0.0 }
        }
    })));
    chunks
}

/// The adapter defect in situ: the proxy adapter's forwarding task held a
/// consumer clone, so dropping the caller's stream left it parked on a full
/// queue with the HTTP body read behind it stuck (and the rest of the response
/// eventually drained into a queue nobody read). With a producer handle the
/// task must observe closure and release the body.
#[tokio::test]
async fn a_real_adapter_releases_its_http_body_when_the_consumer_is_dropped() {
    const DELTAS: usize = 200;
    let chunks = long_proxy_body(DELTAS);
    let total = chunks.len();
    let polled = Arc::new(AtomicUsize::new(0));
    let body_dropped = Arc::new(AtomicBool::new(false));
    let provider = ProxyProvider::new(
        ProxyEndpoint::new("https://proxy.example.com", "test-token"),
        vec![model()],
    )
    .expect("proxy provider")
    .with_client(scripted_client(ChunkCountingTransport {
        chunks,
        polled: Arc::clone(&polled),
        body_dropped: Arc::clone(&body_dropped),
    }));
    let mut stream = provider.stream(&model(), Context::default(), StreamOptions::default());
    let mut seen = 0usize;
    while seen < 2 {
        match tokio::time::timeout(Duration::from_secs(5), stream.next()).await {
            Ok(Some(_)) => seen += 1,
            Ok(None) => break,
            Err(_) => panic!("the adapter stalled before delivering two events"),
        }
    }
    assert_eq!(seen, 2, "the scripted stream delivers its first events");
    assert!(
        stream.consumer_count() == 1,
        "exactly the caller's handle is a consumer"
    );

    // The consumer walks away mid-stream (the abort/teardown path).
    drop(stream);

    assert!(
        wait_until(|| body_dropped.load(Ordering::SeqCst)).await,
        "the adapter must release the HTTP body once the consumer is gone"
    );
    let at_release = polled.load(Ordering::SeqCst);
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(
        polled.load(Ordering::SeqCst),
        at_release,
        "no further body reads after the consumer left"
    );
    assert!(
        at_release < total,
        "the abandoned stream must not drain the whole body ({at_release}/{total} chunks read)"
    );
    assert!(
        at_release <= pir::ai::event_stream::DEFAULT_EVENT_QUEUE_CAPACITY + 16,
        "the read loop stops near the queue capacity, not at the end of the reply \
         ({at_release} chunks read of {total})"
    );
}

// ---------------------------------------------------------------------------
// (c) a normal stream is unaffected
// ---------------------------------------------------------------------------

/// Byte-identical normal operation: the converted adapter still yields the same
/// event order, the same terminal message, and stays inside the capacity bound.
#[tokio::test]
async fn a_normally_consumed_adapter_stream_is_unchanged() {
    let chunks = vec![
        proxy_event(json!({ "type": "start" })),
        proxy_event(json!({ "type": "text_start", "contentIndex": 0 })),
        proxy_event(json!({ "type": "text_delta", "contentIndex": 0, "delta": "hello " })),
        proxy_event(json!({ "type": "text_delta", "contentIndex": 0, "delta": "world" })),
        proxy_event(json!({ "type": "text_end", "contentIndex": 0 })),
        proxy_event(json!({
            "type": "done",
            "reason": "stop",
            "usage": {
                "input": 3,
                "output": 2,
                "cacheRead": 0,
                "cacheWrite": 0,
                "totalTokens": 5,
                "cost": { "input": 0.0, "output": 0.0, "cacheRead": 0.0, "cacheWrite": 0.0, "total": 0.0 }
            }
        })),
    ];
    let provider = ProxyProvider::new(
        ProxyEndpoint::new("https://proxy.example.com", "test-token"),
        vec![model()],
    )
    .expect("proxy provider")
    .with_client(scripted_client(ChunkCountingTransport {
        chunks,
        polled: Arc::new(AtomicUsize::new(0)),
        body_dropped: Arc::new(AtomicBool::new(false)),
    }));

    let mut stream = provider.stream(&model(), Context::default(), StreamOptions::default());
    let mut events = Vec::new();
    while let Some(event) = tokio::time::timeout(Duration::from_secs(5), stream.next())
        .await
        .expect("a normally consumed stream must not stall")
    {
        events.push(event);
    }

    assert_eq!(
        labels(&events),
        vec![
            "start",
            "text_start",
            "text_delta",
            "text_delta",
            "text_end",
            "done"
        ],
        "event order is unchanged by the producer-handle conversion"
    );
    let deltas: Vec<String> = events
        .iter()
        .filter(|event| matches!(event, AssistantMessageEvent::TextDelta { .. }))
        .map(delta_text)
        .collect();
    assert_eq!(deltas, vec!["hello ".to_string(), "world".to_string()]);
    assert!(
        !stream.is_abandoned(),
        "a fully consumed stream is never abandoned"
    );
    assert!(
        stream.max_queue_depth() <= stream.capacity() + 1,
        "the bounded queue holds (depth {} vs capacity {})",
        stream.max_queue_depth(),
        stream.capacity()
    );
    match events.last() {
        Some(AssistantMessageEvent::Done {
            message: _message, ..
        }) => {}
        other => panic!("expected a done event, got {other:?}"),
    }
}
