//! Provider request-body scratch reuse (RSS-D).
//!
//! These tests pin the transport-side property: serialization writes into a
//! pooled buffer, the buffer is returned only after the request finishes, and
//! concurrent requests never share one mutable allocation.

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use futures_util::Stream;
use pir::ai::api::openai::{build_openai_body, resolve_compat, OpenAiCompatibleProvider};
use pir::ai::error::{AiError, AiResult};
use pir::ai::provider::Provider;
use pir::ai::transport::{
    HttpBodyChunk, HttpRequest, HttpResponse, HttpTransport, ProviderRequestClient,
    ProviderRequestConfig,
};
use pir::ai::types::{
    ContentBlock, Context, Message, Model, ModelCost, ModelCostRates, ProviderStreamHooks,
    StreamOptions, TextContent, UserMessage, UserRole,
};
use tokio::sync::Barrier;

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poison| poison.into_inner())
}

fn empty_response() -> HttpResponse {
    let stream: Pin<Box<dyn Stream<Item = Result<HttpBodyChunk, AiError>> + Send>> =
        Box::pin(futures_util::stream::empty());
    HttpResponse {
        status: 200,
        headers: Default::default(),
        body: stream,
    }
}

/// Records every request body; optionally gates all requests until `width`
/// concurrent executions have arrived, so the test truly interleaves them.
struct RecordingTransport {
    requests: Mutex<Vec<Vec<u8>>>,
    executes: AtomicUsize,
    gate: Option<Arc<Barrier>>,
}

impl RecordingTransport {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            requests: Mutex::new(Vec::new()),
            executes: AtomicUsize::new(0),
            gate: None,
        })
    }

    fn with_gate(width: usize) -> Arc<Self> {
        Arc::new(Self {
            requests: Mutex::new(Vec::new()),
            executes: AtomicUsize::new(0),
            gate: Some(Arc::new(Barrier::new(width))),
        })
    }

    fn bodies(&self) -> Vec<Vec<u8>> {
        lock(&self.requests).clone()
    }

    fn executes(&self) -> usize {
        self.executes.load(Ordering::SeqCst)
    }
}

impl HttpTransport for RecordingTransport {
    fn execute(
        &self,
        request: HttpRequest,
    ) -> Pin<Box<dyn std::future::Future<Output = AiResult<HttpResponse>> + Send + '_>> {
        self.executes.fetch_add(1, Ordering::SeqCst);
        lock(&self.requests).push(request.body);
        let gate = self.gate.clone();
        Box::pin(async move {
            if let Some(gate) = gate {
                gate.wait().await;
            }
            Ok(empty_response())
        })
    }
}

fn client(transport: Arc<dyn HttpTransport>) -> ProviderRequestClient {
    ProviderRequestClient::with_transport(
        ProviderRequestConfig {
            max_retries: 0,
            ..ProviderRequestConfig::default()
        },
        transport,
    )
}

fn find_header_end(buffer: &[u8]) -> Option<usize> {
    buffer
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .map(|index| index + 4)
}

/// A minimal keep-alive-free HTTP server: reads one full request per turn and
/// answers 200 with an empty body. Returns the URL and the accepted bodies.
fn spawn_empty_body_server(turns: usize) -> (String, Arc<Mutex<Vec<Vec<u8>>>>) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind body server");
    let addr = listener.local_addr().expect("addr");
    let bodies: Arc<Mutex<Vec<Vec<u8>>>> = Arc::new(Mutex::new(Vec::new()));
    let recorded = Arc::clone(&bodies);
    std::thread::spawn(move || {
        for _ in 0..turns {
            let Ok((mut stream, _)) = listener.accept() else {
                return;
            };
            let mut buffer = Vec::new();
            let mut chunk = [0u8; 4096];
            let mut header_end = None;
            while header_end.is_none() {
                match stream.read(&mut chunk) {
                    Ok(0) | Err(_) => break,
                    Ok(read) => buffer.extend_from_slice(&chunk[..read]),
                }
                header_end = find_header_end(&buffer);
            }
            let Some(header_end) = header_end else {
                return;
            };
            let headers = String::from_utf8_lossy(&buffer[..header_end]).to_ascii_lowercase();
            let content_length: usize = headers
                .lines()
                .find_map(|line| line.strip_prefix("content-length:"))
                .and_then(|value| value.trim().parse().ok())
                .unwrap_or(0);
            while buffer.len() < header_end + content_length {
                match stream.read(&mut chunk) {
                    Ok(0) | Err(_) => break,
                    Ok(read) => buffer.extend_from_slice(&chunk[..read]),
                }
            }
            lock(&recorded).push(buffer[header_end..].to_vec());
            let _ = stream
                .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 0\r\nconnection: close\r\n\r\n");
            let _ = stream.flush();
        }
    });
    (format!("http://{addr}/v1/chat/completions"), bodies)
}

fn payload(turn: usize) -> serde_json::Value {
    // Fixed width so every request has exactly the same serialized length:
    // later turns must not force even one capacity growth.
    serde_json::json!({
        "model": "mock-model",
        "stream": true,
        "messages": [{
            "role": "user",
            "content": format!("turn-{turn:03}:{}", "x".repeat(32 * 1024)),
        }],
    })
}

/// Allocation bytes a fresh `serde_json::to_vec` pays to reach `len` bytes:
/// it starts at capacity 128 and doubles on growth. Every turn of the old
/// implementation paid this whole sequence and then dropped it.
fn fresh_vec_alloc_bytes(len: usize) -> u64 {
    let mut capacity = 128u64;
    let mut allocated = 0u64;
    while (capacity as usize) < len {
        allocated = allocated.saturating_add(capacity);
        capacity = capacity.saturating_mul(2);
    }
    allocated.saturating_add(capacity)
}

fn openai_model(base_url: &str) -> Model {
    Model {
        id: "mock-model".into(),
        name: "Mock model".into(),
        api: "openai-completions".into(),
        provider: "mock".into(),
        base_url: base_url.into(),
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
        context_window: 128_000,
        max_tokens: 8_192,
        sampling_params: None,
        sampling_params_by_thinking_level: None,
        headers: None,
        prompt_cache: None,
        thinking_token_budget_field: None,
        input_limits: None,
        model_type: None,
        compat: None,
    }
}

fn user_context(text: &str) -> Context {
    Context {
        system_prompt: None,
        messages: vec![Message::User(UserMessage {
            role: UserRole::User,
            content: vec![ContentBlock::Text(TextContent {
                text: text.into(),
                text_signature: None,
            })],
            tools_added: None,
            tools_removed: None,
            timestamp: 1,
            sections: None,
        })],
        tools: None,
    }
}

#[tokio::test]
async fn same_session_requests_keep_body_bytes_exact() {
    const TURNS: usize = 8;

    let transport = RecordingTransport::new();
    let client = client(transport.clone());

    let mut expected_total = 0u64;
    for turn in 0..TURNS {
        let body = payload(turn);
        client
            .post_json_streaming(
                "http://recording.example/v1/chat/completions",
                &BTreeMap::new(),
                &body,
                None,
                &ProviderStreamHooks::default(),
                &mut |_chunk| Ok(()),
            )
            .await
            .expect("request succeeds");
        let expected = serde_json::to_vec(&body).expect("serialize expected body");
        expected_total = expected_total.saturating_add(expected.len() as u64);
        let recorded = transport.bodies();
        assert_eq!(
            recorded[turn], expected,
            "turn {turn}: the wire body must stay byte-identical"
        );
    }
    assert_eq!(transport.executes(), TURNS, "one attempt per turn");
    assert_eq!(
        client.payload_diagnostics().body_bytes,
        expected_total,
        "body_bytes must count exactly the bytes written for every turn"
    );
}

#[tokio::test]
async fn scratch_reuse_does_not_scale_allocation_with_turns() {
    const TURNS: usize = 12;

    let transport = RecordingTransport::new();
    let client = client(transport.clone());

    let body = payload(0);
    client
        .post_json_streaming(
            "http://recording.example/v1/chat/completions",
            &BTreeMap::new(),
            &body,
            None,
            &ProviderStreamHooks::default(),
            &mut |_chunk| Ok(()),
        )
        .await
        .expect("warm-up request succeeds");
    let after_warmup = client.payload_diagnostics();
    assert_eq!(
        after_warmup.body_reuses, 0,
        "the first turn must allocate the scratch (nothing to reuse yet)"
    );

    for turn in 1..TURNS {
        client
            .post_json_streaming(
                "http://recording.example/v1/chat/completions",
                &BTreeMap::new(),
                &payload(turn),
                None,
                &ProviderStreamHooks::default(),
                &mut |_chunk| Ok(()),
            )
            .await
            .expect("request succeeds");
    }

    let diagnostics = client.payload_diagnostics();
    assert_eq!(
        diagnostics.allocations, TURNS as u64,
        "one serialization per turn"
    );
    assert_eq!(
        diagnostics.body_reuses,
        (TURNS - 1) as u64,
        "every turn after the first reuses a checked-in scratch buffer"
    );
    assert_eq!(
        diagnostics.body_alloc_bytes, after_warmup.body_alloc_bytes,
        "twelve turns must not allocate more scratch bytes than the warm-up"
    );
    assert!(
        diagnostics.body_alloc_bytes >= 32 * 1024,
        "the first turn must actually allocate the body-sized scratch: {diagnostics:?}"
    );
    // `allocations - body_reuses` is the number of serializations that had to
    // allocate a new body buffer. It must stay at the one warm-up allocation,
    // not grow with the turn count.
    assert_eq!(
        diagnostics.allocations - diagnostics.body_reuses,
        1,
        "exactly one body allocation across all turns: {diagnostics:?}"
    );
    let body_len = serde_json::to_vec(&payload(0)).expect("serialize").len();
    assert_eq!(
        diagnostics.body_bytes,
        (TURNS * body_len) as u64,
        "body_bytes counts every serialized turn"
    );
    let old_alloc_bytes = fresh_vec_alloc_bytes(body_len) * TURNS as u64;
    println!(
        "[RSS-D] body={body_len}B turns={TURNS} new_body_alloc_bytes={} old_fresh_vec_alloc_bytes={} saved_bytes={} ({:.1} KiB/turn)",
        diagnostics.body_alloc_bytes,
        old_alloc_bytes,
        old_alloc_bytes.saturating_sub(diagnostics.body_alloc_bytes),
        old_alloc_bytes.saturating_sub(diagnostics.body_alloc_bytes) as f64
            / TURNS as f64
            / 1024.0,
    );
    assert!(
        diagnostics.body_alloc_bytes < old_alloc_bytes,
        "one pooled scratch must allocate less than {TURNS} fresh Vecs"
    );
}

/// The production transport (reqwest, not the test seam) must also recover the
/// scratch after a completed request. This is the path QA measures.
#[tokio::test]
async fn reqwest_path_reuses_scratch_across_turns() {
    const TURNS: usize = 6;

    let (url, bodies) = spawn_empty_body_server(TURNS);
    let client = ProviderRequestClient::new(ProviderRequestConfig {
        timeout: std::time::Duration::from_secs(10),
        max_response_bytes: 1024 * 1024,
        max_retries: 0,
        max_retry_delay: std::time::Duration::from_millis(5),
        proxy_url: None,
        http_idle_timeout: None,
    })
    .expect("reqwest client");

    for turn in 0..TURNS {
        client
            .post_json_streaming(
                &url,
                &BTreeMap::new(),
                &payload(turn),
                None,
                &ProviderStreamHooks::default(),
                &mut |_chunk| Ok(()),
            )
            .await
            .expect("request succeeds");
    }

    let diagnostics = client.payload_diagnostics();
    assert_eq!(diagnostics.allocations, TURNS as u64);
    assert_eq!(
        diagnostics.body_reuses,
        (TURNS - 1) as u64,
        "reqwest releases the Bytes handle after the response, so the scratch \
         must return to the pool each turn: {diagnostics:?}"
    );
    let body_len = serde_json::to_vec(&payload(0)).expect("serialize").len();
    assert_eq!(diagnostics.body_bytes, (TURNS * body_len) as u64);
    assert_eq!(lock(&bodies).len(), TURNS, "every turn reached the server");
}

#[test]
fn file_payload_reuses_staging_scratch_and_counts_bytes() {
    const TURNS: usize = 5;

    let transport = RecordingTransport::new();
    let client = client(transport.clone());
    let body = payload(0);
    let expected = serde_json::to_vec(&body).expect("serialize expected body");

    let mut after_warmup = None;
    for turn in 0..TURNS {
        let payload = client
            .serialize_payload_to_file(&body)
            .expect("file serialization succeeds");
        let written = std::fs::read(payload.path()).expect("read temp body");
        assert_eq!(
            written, expected,
            "turn {turn}: the file body must stay byte-identical"
        );
        assert_eq!(payload.len(), expected.len() as u64);
        let diagnostics = client.payload_diagnostics();
        if turn == 0 {
            assert_eq!(diagnostics.body_reuses, 0);
            after_warmup = Some(diagnostics);
        } else {
            assert_eq!(
                diagnostics.body_reuses, turn as u64,
                "the file staging scratch is reused after the first turn"
            );
        }
    }

    let diagnostics = client.payload_diagnostics();
    let warm = after_warmup.expect("warm-up snapshot");
    assert_eq!(
        diagnostics.allocations, TURNS as u64,
        "one file serialization per turn"
    );
    assert_eq!(
        diagnostics.body_alloc_bytes, warm.body_alloc_bytes,
        "the 256 KiB staging window is allocated once, not per turn"
    );
    assert!(
        diagnostics.body_alloc_bytes >= 256 * 1024,
        "the staging writer must own a reusable window: {diagnostics:?}"
    );
    assert_eq!(
        diagnostics.allocations - diagnostics.body_reuses,
        1,
        "exactly one staging allocation across all file turns"
    );
    assert_eq!(
        diagnostics.body_bytes,
        (TURNS * expected.len()) as u64,
        "file bodies are counted by their on-disk length"
    );
}

/// The OpenAI streamed body is the production path QA exercises: it feeds
/// `OpenAiStreamedBody` (one message at a time) through
/// `serialize_payload_with_capacity` with a size estimate, then
/// `post_payload_streaming_with`. The body must remain byte-identical to the
/// value builder while the scratch is reused across turns.
#[tokio::test]
async fn openai_streamed_body_reuses_scratch_across_turns() {
    const TURNS: usize = 6;

    let transport = RecordingTransport::new();
    let client = client(transport.clone());
    let base = "http://recording.example/v1";
    let provider = OpenAiCompatibleProvider::new("mock", base, vec![openai_model(base)])
        .expect("provider")
        .with_client(client.clone());
    let model = provider.get_models()[0].clone();
    let context = user_context("streamed-body-reuse");

    let compat = resolve_compat(&model);
    let options = StreamOptions::default();
    let expected =
        serde_json::to_vec(&build_openai_body(&model, &context, &options, &compat).expect("body"))
            .expect("serialize value body");

    let mut warm_body_alloc = None;
    for turn in 0..TURNS {
        let stream = provider.stream(
            &model,
            context.clone(),
            StreamOptions {
                api_key: Some("test-key".into()),
                max_retries: Some(0),
                ..StreamOptions::default()
            },
        );
        assert!(
            stream.drain_and_result().await.is_some(),
            "turn {turn}: the provider stream must terminate"
        );
        let bodies = transport.bodies();
        assert_eq!(
            bodies[turn], expected,
            "turn {turn}: the streamed body must equal the value body byte for byte"
        );
        if turn == 0 {
            warm_body_alloc = Some(client.payload_diagnostics().body_alloc_bytes);
        }
    }

    let diagnostics = client.payload_diagnostics();
    assert_eq!(diagnostics.allocations, TURNS as u64);
    assert_eq!(
        diagnostics.body_reuses,
        (TURNS - 1) as u64,
        "the OpenAI streamed path must use the pooled scratch: {diagnostics:?}"
    );
    assert_eq!(
        diagnostics.body_alloc_bytes,
        warm_body_alloc.expect("warm-up body allocation"),
        "streamed turns must not grow the scratch after the first turn"
    );
    assert_eq!(
        diagnostics.allocations - diagnostics.body_reuses,
        1,
        "one body allocation for all streamed turns: {diagnostics:?}"
    );
    assert_eq!(
        diagnostics.body_bytes,
        (TURNS * expected.len()) as u64,
        "body_bytes counts the streamed serializations"
    );
}

#[tokio::test]
async fn qa_sized_body_allocation_comparison() {
    const TURNS: usize = 8;
    const BODY_TEXT_BYTES: usize = 220 * 1024;

    let transport = RecordingTransport::new();
    let client = client(transport.clone());
    let body = serde_json::json!({
        "model": "mock-model",
        "stream": true,
        "messages": [{"role": "user", "content": "q".repeat(BODY_TEXT_BYTES)}],
    });
    let expected = serde_json::to_vec(&body).expect("serialize expected body");

    for _ in 0..TURNS {
        client
            .post_json_streaming(
                "http://recording.example/v1/chat/completions",
                &BTreeMap::new(),
                &body,
                None,
                &ProviderStreamHooks::default(),
                &mut |_chunk| Ok(()),
            )
            .await
            .expect("request succeeds");
    }

    let diagnostics = client.payload_diagnostics();
    let old_alloc_bytes = fresh_vec_alloc_bytes(expected.len()) * TURNS as u64;
    let saved = old_alloc_bytes.saturating_sub(diagnostics.body_alloc_bytes);
    println!(
        "[RSS-D-220K] body={}B turns={TURNS} new_body_alloc_bytes={} old_fresh_vec_alloc_bytes={} saved={}B ({:.1} KiB/turn); body_bytes={} reuses={}",
        expected.len(),
        diagnostics.body_alloc_bytes,
        old_alloc_bytes,
        saved,
        saved as f64 / TURNS as f64 / 1024.0,
        diagnostics.body_bytes,
        diagnostics.body_reuses,
    );
    assert_eq!(diagnostics.body_bytes, (TURNS * expected.len()) as u64);
    assert_eq!(diagnostics.allocations - diagnostics.body_reuses, 1);
    assert!(
        diagnostics.body_alloc_bytes < old_alloc_bytes / 4,
        "the warm scratch must allocate far less than per-turn fresh Vecs"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_requests_never_cross_bytes() {
    const WIDTH: usize = 4;

    let transport = RecordingTransport::with_gate(WIDTH);
    let client = client(transport.clone());

    let requests = (0..WIDTH).map(|index| {
        let client = client.clone();
        async move {
            let body = serde_json::json!({
                "model": "mock-model",
                "marker": format!("request-{index}"),
                "messages": [{"role": "user", "content": "y".repeat(8 * 1024)}],
            });
            client
                .post_json_streaming(
                    "http://recording.example/v1/chat/completions",
                    &BTreeMap::new(),
                    &body,
                    None,
                    &ProviderStreamHooks::default(),
                    &mut |_chunk| Ok(()),
                )
                .await
                .expect("concurrent request succeeds");
        }
    });
    futures_util::future::join_all(requests).await;

    let bodies = transport.bodies();
    assert_eq!(
        bodies.len(),
        WIDTH,
        "all concurrent requests reached the fake"
    );
    let mut seen = std::collections::BTreeSet::new();
    for body in &bodies {
        let value: serde_json::Value =
            serde_json::from_slice(body).expect("every body is valid JSON");
        let marker = value
            .get("marker")
            .and_then(|marker| marker.as_str())
            .expect("marker survives serialization")
            .to_string();
        assert!(
            seen.insert(marker.clone()),
            "a body was delivered twice or crossed with another request: {marker}"
        );
        let text = String::from_utf8_lossy(body);
        assert_eq!(
            text.matches("request-").count(),
            1,
            "a body must carry exactly its own marker"
        );
    }
    assert_eq!(seen.len(), WIDTH);
}
