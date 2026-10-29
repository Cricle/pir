//! Aborting (or failing) a stream keeps the content it already streamed.
//!
//! Upstream mutates the parser's accumulated `output` in its `catch` block, so
//! a user abort records `stopReason: "aborted"` and a transport failure
//! `stopReason: "error"` on the SAME object whose content was streamed
//! (`pi/packages/ai/src/api/openai-completions.ts:708-730`; same shape in the
//! anthropic/bedrock/google/mistral/responses adapters). pir's adapter-side
//! finalizer (`api::finish_provider_stream`) only sees the event sink, so the
//! accumulated content is carried into the terminal message by the registry
//! forwarding loop (`ai::provider::StreamedContent`) that every provider's
//! events pass through.
//!
//! The fixtures here are scripted providers and a local mock HTTP server — no
//! network.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use futures_util::StreamExt;
use pir::agent::tool_execution::BoxFuture;
use pir::ai::api::openai::OpenAiCompatibleProvider;
use pir::ai::auth::{InMemoryCredentialStore, MapAuthContext};
use pir::ai::error::AiResult;
use pir::ai::event_stream::AssistantMessageEventStream;
use pir::ai::provider::{Provider, ProviderRegistry};
use pir::ai::transport::{
    HttpBodyChunk, HttpRequest, HttpResponse, HttpTransport, ProviderRequestClient,
    ProviderRequestConfig,
};
use pir::ai::types::*;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

// ---------------------------------------------------------------------------
// Scripted provider: emits a fixed event list, so the registry forwarding path
// is exercised without any wire codec.
// ---------------------------------------------------------------------------

struct ScriptedProvider {
    provider_id: String,
    model: Model,
    events: Vec<AssistantMessageEvent>,
}

impl ScriptedProvider {
    fn new(provider_id: &str, model: Model, events: Vec<AssistantMessageEvent>) -> Self {
        Self {
            provider_id: provider_id.to_string(),
            model,
            events,
        }
    }
}

impl Provider for ScriptedProvider {
    fn id(&self) -> &str {
        &self.provider_id
    }
    fn name(&self) -> &str {
        &self.provider_id
    }
    fn get_models(&self) -> Vec<Model> {
        vec![self.model.clone()]
    }
    fn api_key_auth(&self) -> Vec<pir::ai::auth::ApiKeyAuth> {
        Vec::new()
    }
    fn stream(
        &self,
        _model: &Model,
        _context: Context,
        _options: StreamOptions,
    ) -> AssistantMessageEventStream {
        let output = AssistantMessageEventStream::new();
        for event in &self.events {
            output.push(event.clone());
        }
        output
    }
}

fn model(provider: &str) -> Model {
    Model {
        id: "test-model".into(),
        name: "Test Model".into(),
        api: "openai-chat".into(),
        provider: provider.into(),
        base_url: "https://example.invalid".into(),
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
        context_window: 32_000,
        max_tokens: 8_192,
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

fn registry_with(provider: ScriptedProvider) -> (ProviderRegistry, Model) {
    let mut registry = ProviderRegistry::new(
        Arc::new(InMemoryCredentialStore::new()),
        Arc::new(MapAuthContext::new(BTreeMap::new())),
    );
    let model = provider.get_models()[0].clone();
    registry.set_provider(Arc::new(provider));
    (registry, model)
}

fn text_block(text: &str) -> ContentBlock {
    ContentBlock::Text(TextContent {
        text: text.into(),
        text_signature: None,
    })
}

fn partial_message(model: &Model, text: &str) -> AssistantMessage {
    AssistantMessage {
        content: vec![text_block(text)],
        api: model.api.clone(),
        provider: model.provider.clone(),
        model: model.id.clone(),
        timestamp: 1,
        ..AssistantMessage::default()
    }
}

/// The terminal message the adapter-side finalizer builds: history-free by
/// construction (`error_message()` has no parser state), which is exactly the
/// message the registry must fill with the streamed content. `stop_reason` is
/// what `error_message()` sets before the registry reclassifies it.
fn empty_terminal(model: &Model, reason: StopReason, text: &str) -> AssistantMessage {
    AssistantMessage {
        api: model.api.clone(),
        provider: model.provider.clone(),
        model: model.id.clone(),
        stop_reason: reason,
        error_message: Some(text.into()),
        timestamp: 2,
        ..AssistantMessage::default()
    }
}

fn text_of(message: &AssistantMessage) -> String {
    message
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text(text) => Some(text.text.as_str()),
            _ => None,
        })
        .collect()
}

async fn drain(stream: AssistantMessageEventStream) -> Vec<AssistantMessageEvent> {
    let mut stream = Box::pin(stream);
    let mut events = Vec::new();
    while let Some(event) = stream.next().await {
        events.push(event);
    }
    events
}

fn terminal_message(events: &[AssistantMessageEvent]) -> &AssistantMessage {
    match events.last().expect("at least one event") {
        AssistantMessageEvent::Done { message, .. } => message,
        AssistantMessageEvent::Error { error, .. } => error,
        other => panic!("last event must be terminal, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// (a)+(b) A user abort keeps the deltas' text and records `aborted`.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn abort_after_deltas_keeps_partial_text_and_records_aborted() {
    let model = model("scripted");
    let partial = partial_message(&model, "Hello world");
    let events = vec![
        AssistantMessageEvent::Start {
            partial: Arc::new(AssistantMessage::default()),
        },
        AssistantMessageEvent::TextStart {
            content_index: 0,
            partial: Arc::new(partial_message(&model, "")),
        },
        AssistantMessageEvent::TextDelta {
            content_index: 0,
            delta: "Hello ".into(),
            partial: Arc::new(partial_message(&model, "Hello ")),
        },
        AssistantMessageEvent::TextDelta {
            content_index: 0,
            delta: "world".into(),
            partial: Arc::new(partial.clone()),
        },
        AssistantMessageEvent::TextEnd {
            content_index: 0,
            content: "Hello world".into(),
            partial: Arc::new(partial),
        },
        // Adapter-side finalizer output for a user abort: an empty message
        // carrying the abort wording.
        AssistantMessageEvent::Error {
            reason: ErrorReason::Aborted,
            error: empty_terminal(&model, StopReason::Error, "Request was aborted"),
        },
    ];
    let (registry, model) = registry_with(ScriptedProvider::new("scripted", model, events));
    let events =
        drain(registry.stream_unchecked(model, Context::default(), StreamOptions::default())).await;
    let terminal = terminal_message(&events);

    assert_eq!(text_of(terminal), "Hello world");
    assert_eq!(terminal.stop_reason, StopReason::Aborted);
    assert_eq!(
        terminal.error_message.as_deref(),
        Some("Request was aborted")
    );
    assert!(matches!(
        events.last(),
        Some(AssistantMessageEvent::Error {
            reason: ErrorReason::Aborted,
            ..
        })
    ));
}

// ---------------------------------------------------------------------------
// (c) A transport/server failure records `error` and still carries the partial.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn transport_failure_keeps_partial_text_and_records_error() {
    let model = model("scripted");
    let events = vec![
        AssistantMessageEvent::TextStart {
            content_index: 0,
            partial: Arc::new(partial_message(&model, "")),
        },
        AssistantMessageEvent::TextDelta {
            content_index: 0,
            delta: "half a reply".into(),
            partial: Arc::new(partial_message(&model, "half a reply")),
        },
        AssistantMessageEvent::Error {
            reason: ErrorReason::Error,
            error: empty_terminal(&model, StopReason::Error, "HTTP 500: boom"),
        },
    ];
    let (registry, model) = registry_with(ScriptedProvider::new("scripted", model, events));
    let events =
        drain(registry.stream_unchecked(model, Context::default(), StreamOptions::default())).await;
    let terminal = terminal_message(&events);

    assert_eq!(text_of(terminal), "half a reply");
    assert_eq!(terminal.stop_reason, StopReason::Error);
    assert_eq!(terminal.error_message.as_deref(), Some("HTTP 500: boom"));
}

/// A failure that never delivered a delta keeps the empty terminal message
/// (nothing was shown, so nothing can be carried).
#[tokio::test]
async fn failure_before_any_delta_stays_empty() {
    let model = model("scripted");
    let events = vec![AssistantMessageEvent::Error {
        reason: ErrorReason::Aborted,
        error: empty_terminal(&model, StopReason::Error, "Request was aborted"),
    }];
    let (registry, model) = registry_with(ScriptedProvider::new("scripted", model, events));
    let events =
        drain(registry.stream_unchecked(model, Context::default(), StreamOptions::default())).await;
    let terminal = terminal_message(&events);

    assert!(terminal.content.is_empty());
    assert_eq!(terminal.stop_reason, StopReason::Aborted);
}

// ---------------------------------------------------------------------------
// (d) A successful stream is unchanged: the terminal message is the one the
// provider emitted, byte for byte.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn successful_stream_terminal_message_is_unchanged() {
    let model = model("scripted");
    let done_message = AssistantMessage {
        content: vec![text_block("all good")],
        api: model.api.clone(),
        provider: model.provider.clone(),
        model: model.id.clone(),
        stop_reason: StopReason::Stop,
        usage: Usage {
            input: 3,
            output: 2,
            total_tokens: 5,
            // The registry's terminal-message cost pass runs `calculateCost`,
            // which stamps the estimate marker even for a zero-rate model.
            cost: Cost {
                total_from_provider: Some(false),
                ..Cost::default()
            },
            ..Usage::default()
        },
        response_id: Some("resp-1".into()),
        timestamp: 7,
        ..AssistantMessage::default()
    };
    let emitted = vec![
        AssistantMessageEvent::Start {
            partial: Arc::new(partial_message(&model, "")),
        },
        AssistantMessageEvent::TextStart {
            content_index: 0,
            partial: Arc::new(partial_message(&model, "")),
        },
        AssistantMessageEvent::TextDelta {
            content_index: 0,
            delta: "all good".into(),
            partial: Arc::new(partial_message(&model, "all good")),
        },
        AssistantMessageEvent::TextEnd {
            content_index: 0,
            content: "all good".into(),
            partial: Arc::new(partial_message(&model, "all good")),
        },
        AssistantMessageEvent::Done {
            reason: TerminalReason::Stop,
            message: done_message.clone(),
        },
    ];
    let (registry, model) =
        registry_with(ScriptedProvider::new("scripted", model, emitted.clone()));
    let events =
        drain(registry.stream_unchecked(model, Context::default(), StreamOptions::default())).await;

    // Event order and payloads are untouched.
    assert_eq!(events.len(), emitted.len());
    for (got, want) in events.iter().zip(emitted.iter()) {
        assert_eq!(
            serde_json::to_value(got).expect("serialize event"),
            serde_json::to_value(want).expect("serialize event"),
            "a successful stream must be unchanged"
        );
    }
    assert_eq!(terminal_message(&events), &done_message);
}

// ---------------------------------------------------------------------------
// End to end: a real adapter, a real abort signal, a local SSE server that
// stops mid-reply. The terminal message carries what reached the client.
// ---------------------------------------------------------------------------

struct MockServer {
    base_url: String,
    _handle: tokio::task::JoinHandle<()>,
}

impl MockServer {
    /// Serve the body prefix as soon as the request head is read, then keep the
    /// response alive past the caller's deadline with in-protocol SSE
    /// comments.
    ///
    /// Two properties matter here, and the fixture needs BOTH:
    ///
    /// 1. **Chunked framing.** The transport surfaces a `content-length` body
    ///    only once the whole declared length has arrived, so a fixture that
    ///    declares a large length but sends a short prefix delivers NOTHING
    ///    until the socket dies — the deltas and the transport error then
    ///    arrive together and the consumer's abort races that error (the
    ///    recorded `left: Error, right: Aborted` flake, reproduced 1/120 under
    ///    load). Chunked frames reach the reader as they arrive, so the abort
    ///    lands on a live, already-started stream.
    /// 2. **A lifetime longer than the test's own 10 s budget.** The keepalive
    ///    loop used to run 2 s and hold for 5 s; when a loaded box delayed the
    ///    consumer past ~7 s, the socket died while the caller still waited,
    ///    and the stream ended through the transport's error path
    ///    (upstream's `signal.aborted ? "aborted" : "error"` rule then
    ///    correctly says `error` — the abort had not landed yet). Keeping the
    ///    response alive well past the deadline makes the consumer's abort the
    ///    ONLY way this stream can end, which is what the test is about. The
    ///    happy path returns on the first failed write after the client
    ///    closes, so the longer window costs nothing when the test passes.
    async fn start_hosting(body_prefix: Vec<u8>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind mock server");
        let addr = listener.local_addr().expect("local addr");
        let handle = tokio::spawn(async move {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let _ = read_request_head(&mut socket).await;
            let head = "HTTP/1.1 200 OK\r\ntransfer-encoding: chunked\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n";
            let _ = socket.write_all(head.as_bytes()).await;
            let _ = socket.write_all(&chunk_frame(&body_prefix)).await;
            let _ = socket.flush().await;
            // 600 * 50 ms = 30 s: longer than the test's 10 s timeout, so
            // only the abort can end the stream (see property 2 above).
            for _ in 0..600 {
                tokio::time::sleep(Duration::from_millis(50)).await;
                if socket
                    .write_all(&chunk_frame(b": keepalive\n\n"))
                    .await
                    .is_err()
                {
                    return;
                }
                let _ = socket.flush().await;
            }
            tokio::time::sleep(Duration::from_secs(5)).await;
        });
        Self {
            base_url: format!("http://{addr}"),
            _handle: handle,
        }
    }

    fn base_url(&self) -> &str {
        &self.base_url
    }
}

/// One HTTP/1.1 chunk: hex length, CRLF, payload, CRLF.
fn chunk_frame(data: &[u8]) -> Vec<u8> {
    let mut frame = format!("{:x}\r\n", data.len()).into_bytes();
    frame.extend_from_slice(data);
    frame.extend_from_slice(b"\r\n");
    frame
}

async fn read_request_head(socket: &mut TcpStream) -> Option<()> {
    let mut buf = Vec::new();
    let mut tmp = [0u8; 4096];
    loop {
        let n = socket.read(&mut tmp).await.ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&tmp[..n]);
        if buf.windows(4).any(|window| window == b"\r\n\r\n") {
            return Some(());
        }
    }
}

fn openai_model(base_url: &str) -> Model {
    Model {
        id: "gpt-test".into(),
        name: "GPT Test".into(),
        api: "openai-chat".into(),
        provider: "openai".into(),
        base_url: base_url.to_string(),
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
        context_window: 32_000,
        max_tokens: 8_192,
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

// A MULTI-THREAD runtime, deliberately: the consumer must run concurrently
// with the provider task, exactly as it does in the app. On the default
// current-thread runtime the producer can hold the only thread across a body
// read, so the consumer observes every queued event at once — including the
// terminal one — and its `abort()` lands AFTER the stream already ended
// (measured: aborted 1 ms after the terminal event). That made this test
// assert upstream's `signal.aborted ? "aborted" : "error"` rule against a
// signal that was genuinely still un-aborted when the stream finished.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn openai_adapter_abort_mid_stream_keeps_streamed_text() {
    let delta_one = "data: {\"id\":\"chatcmpl-1\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"ALPHA \"},\"finish_reason\":null}]}\n\n";
    let delta_two = "data: {\"id\":\"chatcmpl-1\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"BETA\"},\"finish_reason\":null}]}\n\n";
    let prefix = format!("{delta_one}{delta_two}").into_bytes();
    let server = MockServer::start_hosting(prefix).await;

    let model = openai_model(server.base_url());
    let client = ProviderRequestClient::new(ProviderRequestConfig {
        timeout: Duration::from_secs(10),
        max_response_bytes: 8 * 1024 * 1024,
        max_retries: 0,
        max_retry_delay: Duration::from_millis(10),
        proxy_url: None,
        http_idle_timeout: None,
    })
    .expect("client");
    let provider =
        OpenAiCompatibleProvider::new("openai", server.base_url().to_string(), vec![model.clone()])
            .expect("provider")
            .with_client(client);
    let mut registry = ProviderRegistry::new(
        Arc::new(InMemoryCredentialStore::new()),
        Arc::new(MapAuthContext::new(BTreeMap::new())),
    );
    registry.set_provider(Arc::new(provider));

    let signal = AbortSignal::new();
    let abort_from_consumer = signal.clone();
    let stream = registry.stream_unchecked(
        model,
        Context {
            messages: vec![Message::User(UserMessage {
                role: UserRole::User,
                content: vec![text_block("go")],
                tools_added: None,
                tools_removed: None,
                timestamp: 1,
                sections: None,
            })],
            ..Context::default()
        },
        StreamOptions {
            signal: Some(signal),
            ..StreamOptions::default()
        },
    );

    let mut stream = Box::pin(stream);
    let events = tokio::time::timeout(Duration::from_secs(10), async {
        let mut saw_first_delta = false;
        let mut events = Vec::new();
        while let Some(event) = stream.next().await {
            if matches!(event, AssistantMessageEvent::TextDelta { .. }) && !saw_first_delta {
                saw_first_delta = true;
                abort_from_consumer.abort();
            }
            events.push(event);
        }
        assert!(
            saw_first_delta,
            "the fixture must deliver at least one delta"
        );
        events
    })
    .await
    .expect("the abort must terminate the stream");
    let terminal = terminal_message(&events);
    assert_eq!(terminal.stop_reason, StopReason::Aborted);
    let text = text_of(terminal);
    assert!(
        text.starts_with("ALPHA "),
        "the streamed prefix must survive the abort, got {text:?}"
    );
    assert!(
        "ALPHA BETA".starts_with(&text),
        "only streamed content may be carried, got {text:?}"
    );
}

/// A transport that answers every attempt with ONE body chunk holding the
/// scripted bytes, so a valid event and a malformed line land in the same
/// parser slice deterministically (no socket splitting).
struct SingleChunkTransport {
    body: Vec<u8>,
}

impl HttpTransport for SingleChunkTransport {
    fn execute(&self, _request: HttpRequest) -> BoxFuture<'_, AiResult<HttpResponse>> {
        let body = self.body.clone();
        Box::pin(async move {
            Ok(HttpResponse {
                status: 200,
                headers: reqwest::header::HeaderMap::new(),
                body: Box::pin(futures_util::stream::once(async move {
                    Ok(HttpBodyChunk::new(body))
                })),
            })
        })
    }
}

/// A malformed SSE line aborts the request, but the events parsed EARLIER in
/// the same body chunk must still be forwarded: the terminal message carries
/// the deltas the provider had already delivered (upstream pushes each parsed
/// event as it walks the chunk). Dropping the not-yet-pushed batch would
/// silently lose them.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn openai_adapter_keeps_deltas_parsed_before_a_malformed_event() {
    let body = concat!(
        "data: {\"id\":\"chatcmpl-1\",\"choices\":[{\"index\":0,",
        "\"delta\":{\"content\":\"ALPHA\"},\"finish_reason\":null}]}\n\n",
        "data: {not valid json}\n\n"
    );
    let transport = Arc::new(SingleChunkTransport {
        body: body.as_bytes().to_vec(),
    });
    let client = ProviderRequestClient::with_transport(
        ProviderRequestConfig {
            timeout: Duration::from_secs(10),
            max_response_bytes: 8 * 1024 * 1024,
            max_retries: 0,
            max_retry_delay: Duration::from_millis(10),
            proxy_url: None,
            http_idle_timeout: None,
        },
        transport,
    );
    let model = openai_model("http://transport.invalid");
    let provider = OpenAiCompatibleProvider::new(
        "openai",
        "http://transport.invalid".to_string(),
        vec![model.clone()],
    )
    .expect("provider")
    .with_client(client);
    let mut registry = ProviderRegistry::new(
        Arc::new(InMemoryCredentialStore::new()),
        Arc::new(MapAuthContext::new(BTreeMap::new())),
    );
    registry.set_provider(Arc::new(provider));

    let events = tokio::time::timeout(
        Duration::from_secs(10),
        drain(registry.stream_unchecked(
            model,
            Context {
                messages: vec![Message::User(UserMessage {
                    role: UserRole::User,
                    content: vec![text_block("go")],
                    tools_added: None,
                    tools_removed: None,
                    timestamp: 1,
                    sections: None,
                })],
                ..Context::default()
            },
            StreamOptions::default(),
        )),
    )
    .await
    .expect("the malformed event must terminate the stream");
    let terminal = terminal_message(&events);
    assert_eq!(terminal.stop_reason, StopReason::Error);
    assert_eq!(
        text_of(terminal),
        "ALPHA",
        "deltas parsed before the malformed line must survive"
    );
}
