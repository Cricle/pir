//! The print host's AGENT-level retry lifecycle on the `--mode json` stream
//! (H8).
//!
//! H4a added `LoopDeps::retry_events` and the session path forwards
//! `AgentRetryEvent` to `SessionEvent::AutoRetryStart` / `AutoRetryEnd`.
//! `src/coding_agent/print.rs` drives `agent_loop` itself, so before this
//! suite it installed no sink and `--mode json` only carried the TRANSPORT
//! retry observers (`StreamOptions::hooks`). The print host now maps the loop
//! lifecycle onto the same session events and writes them through
//! [`pir::coding_agent::print::install_print_retry_events`]:
//!
//! - a retried turn emits `auto_retry_start {attempt, maxAttempts, delayMs,
//!   errorMessage}` and, once the lifecycle closes, `auto_retry_end {success,
//!   attempt, finalError?}` — exactly the upstream session-event shapes;
//! - a cancel during the backoff reports `finalError: "Retry cancelled"`;
//! - a plain success emits no retry frame at all;
//! - text mode installs no sink (its output stays byte-identical).
//!
//! The loop-level half runs in-process over [`print_loop_deps`] with a fake
//! stream and a recording session-event sink; the end-to-end half runs the
//! real `pir print --mode json` binary against a loopback OpenAI-compatible
//! mock that truncates its first stream (a `RetryableTurn` failure the
//! transport does NOT retry because a chunk was already delivered) and
//! completes the next one.
//!
//! H11 extends the same suite with the SUMMARIZER's own retry budget on the
//! two out-of-loop summary paths: the threshold compaction hook
//! ([`pir::coding_agent::print::print_compaction_hook`], source `compaction` +
//! reason `threshold`) and H8's storage-level `navigateTree({summarize})`
//! summary (source `branchSummary`). Both report the upstream
//! `summarization_retry_scheduled` / `_attempt_start` / `_finished` triple
//! through the same JSON sink, each exactly once and never relabeled.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use pir::agent::agent_loop::{run_with_options, LoopDeps, LoopOptions, ProviderRequest};
use pir::agent::tool_execution::ToolRegistry;
use pir::agent::tool_scheduler::{ToolScheduler, ToolSchedulerConfig};
use pir::agent_session::{
    prepare_session_command_to_storage, session_event_to_json, NavigateSummaryBinding,
    SessionCommandAction, SessionCommandStep, SessionEvent,
};
use pir::ai::auth::ApiKeyAuth;
use pir::ai::event_stream::AssistantMessageEventStream;
use pir::ai::provider::{Provider, ProviderRegistry};
use pir::ai::types::{
    AbortSignal, AssistantMessage, ContentBlock, Context, Message, Model, StopReason,
    StreamOptions, TerminalReason, TextContent, ThinkingLevel, Usage, UserMessage, UserRole,
};
use pir::coding_agent::json::to_session_event_json;
use pir::coding_agent::print::{
    agent_retry_event_to_session_event, generate_navigation_summary_with_retry_events,
    install_print_retry_events, print_compaction_hook, print_loop_deps, PrintSessionEventSink,
    SharedSession,
};
use pir::compaction::{CompactionConfig, CompleteFn};
use pir::session_manager::{FileEntry, SessionManager};

// ---------------------------------------------------------------------------
// In-process loop fixtures
// ---------------------------------------------------------------------------

fn test_model() -> Model {
    serde_json::from_value(json!({
        "id": "test-model",
        "name": "Test Model",
        "api": "openai-completions",
        "provider": "test",
        "baseUrl": "http://localhost",
        "reasoning": false,
        "input": ["text"],
        "cost": {},
        "contextWindow": 131072,
        "maxTokens": 4096,
    }))
    .expect("model fixture")
}

/// A retryable MID-STREAM terminal (the transport rendered no verdict), built
/// through the same finalizer the real adapters use so the loop sees the
/// structural `RetryableTurn` class.
fn classified_error() -> AssistantMessage {
    pir::ai::api::terminal_error_message(
        &test_model(),
        &pir::ai::error::AiError::StreamNotTerminated,
        None,
    )
}

fn recovered_assistant() -> AssistantMessage {
    AssistantMessage {
        api: "fake".into(),
        provider: "fake".into(),
        model: "fake-model".into(),
        content: vec![ContentBlock::Text(TextContent {
            text: "recovered".into(),
            text_signature: None,
        })],
        stop_reason: StopReason::Stop,
        timestamp: 1,
        ..AssistantMessage::default()
    }
}

/// The production-shaped print deps for a prompt run (the builder `run_print`
/// fills), with the agent-level retry policy the settings snapshot resolves.
fn print_retry_deps(calls: Arc<AtomicUsize>, max_retries: u32, base_delay_ms: u64) -> LoopDeps {
    let mut deps = print_loop_deps(
        Arc::new(ToolRegistry::new()),
        ToolScheduler::new(ToolSchedulerConfig::default()),
        PathBuf::from("."),
        None,
        None,
        None,
        ThinkingLevel::Off,
    );
    deps.retry = Some(pir::ai::util::AgentRetryPolicy {
        max_retries,
        base_delay_ms,
        max_agent_delay_ms: base_delay_ms.max(1),
    });
    deps.stream = Box::new(move |_request: ProviderRequest| {
        let attempt = calls.fetch_add(1, Ordering::SeqCst);
        Box::pin(async move {
            if attempt == 0 {
                classified_error()
            } else {
                recovered_assistant()
            }
        })
    });
    deps
}

/// A recording stand-in for [`pir::coding_agent::print::json_session_event_sink`]:
/// the frames are the exact wire objects the JSON-mode writer serializes.
fn recording_sink(frames: Arc<Mutex<Vec<Value>>>) -> PrintSessionEventSink {
    Arc::new(move |event: &SessionEvent| {
        frames
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(to_session_event_json(event));
    })
}

fn taken(frames: &Arc<Mutex<Vec<Value>>>) -> Vec<Value> {
    std::mem::take(
        &mut *frames
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner),
    )
}

/// The retry failure the fake stream reports (the classified terminal text).
const RETRY_ERROR: &str = "stream ended without a terminal event";

// ---------------------------------------------------------------------------
// In-process: the installed print sink
// ---------------------------------------------------------------------------

/// A recovered turn emits exactly one start and one successful end with the
/// upstream fields — and nothing from the transport layer (no fake transport
/// hooks are installed here, but the assertion also pins the FRAME COUNT so a
/// future double-install cannot slip past).
#[tokio::test]
async fn print_json_agent_retry_start_and_success_end_are_emitted() {
    let calls = Arc::new(AtomicUsize::new(0));
    let frames = Arc::new(Mutex::new(Vec::new()));
    let mut deps = print_retry_deps(Arc::clone(&calls), 1, 0);
    install_print_retry_events(&mut deps, Some(recording_sink(Arc::clone(&frames))));
    assert!(
        deps.retry_events.is_some(),
        "JSON mode must install the loop's retry sink"
    );

    let result = run_with_options(deps, Context::default(), None, LoopOptions::default()).await;
    assert_eq!(result.terminal.stop_reason, StopReason::Stop);
    assert_eq!(
        calls.load(Ordering::SeqCst),
        2,
        "initial attempt + one retry"
    );

    assert_eq!(
        taken(&frames),
        vec![
            json!({
                "type": "auto_retry_start",
                "attempt": 1,
                "maxAttempts": 1,
                "delayMs": 0,
                "errorMessage": RETRY_ERROR,
            }),
            json!({
                "type": "auto_retry_end",
                "success": true,
                "attempt": 1,
            }),
        ]
    );
}

/// Exhausting the budget closes the lifecycle with a failed end whose
/// `finalError` is the terminal provider error (upstream
/// `_handlePostAgentRun`).
#[tokio::test]
async fn print_json_agent_retry_exhausted_reports_the_final_error() {
    let calls = Arc::new(AtomicUsize::new(0));
    let frames = Arc::new(Mutex::new(Vec::new()));
    let mut deps = print_retry_deps(Arc::clone(&calls), 2, 0);
    let calls_for_stream = Arc::clone(&calls);
    deps.stream = Box::new(move |_request: ProviderRequest| {
        calls_for_stream.fetch_add(1, Ordering::SeqCst);
        Box::pin(async move { classified_error() })
    });
    install_print_retry_events(&mut deps, Some(recording_sink(Arc::clone(&frames))));

    let result = run_with_options(deps, Context::default(), None, LoopOptions::default()).await;
    assert_eq!(result.terminal.stop_reason, StopReason::Error);
    assert_eq!(
        calls.load(Ordering::SeqCst),
        3,
        "initial attempt + two retries"
    );

    assert_eq!(
        taken(&frames),
        vec![
            json!({
                "type": "auto_retry_start",
                "attempt": 1,
                "maxAttempts": 2,
                "delayMs": 0,
                "errorMessage": RETRY_ERROR,
            }),
            json!({
                "type": "auto_retry_start",
                "attempt": 2,
                "maxAttempts": 2,
                "delayMs": 0,
                "errorMessage": RETRY_ERROR,
            }),
            json!({
                "type": "auto_retry_end",
                "success": false,
                "attempt": 2,
                "finalError": RETRY_ERROR,
            }),
        ]
    );
}

/// A cancel during the backoff closes the lifecycle with
/// `finalError: "Retry cancelled"` (upstream `_finishCancelledRetry`) and ends
/// the run as an abort, so a `--mode json` consumer always sees the retry
/// close instead of a dangling countdown.
#[tokio::test]
async fn print_json_agent_retry_cancelled_reports_retry_cancelled() {
    let calls = Arc::new(AtomicUsize::new(0));
    let frames = Arc::new(Mutex::new(Vec::new()));
    let mut deps = print_retry_deps(Arc::clone(&calls), 3, 10_000);
    let calls_for_stream = Arc::clone(&calls);
    deps.stream = Box::new(move |_request: ProviderRequest| {
        calls_for_stream.fetch_add(1, Ordering::SeqCst);
        Box::pin(async move { classified_error() })
    });
    install_print_retry_events(&mut deps, Some(recording_sink(Arc::clone(&frames))));
    let signal = AbortSignal::new();
    let abort = signal.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(20)).await;
        abort.abort();
    });

    let result = run_with_options(
        deps,
        Context::default(),
        Some(signal),
        LoopOptions::default(),
    )
    .await;
    assert_eq!(calls.load(Ordering::SeqCst), 1, "the retry never ran");
    assert_eq!(result.terminal.stop_reason, StopReason::Aborted);

    assert_eq!(
        taken(&frames),
        vec![
            json!({
                "type": "auto_retry_start",
                "attempt": 1,
                "maxAttempts": 3,
                "delayMs": 10_000,
                "errorMessage": RETRY_ERROR,
            }),
            json!({
                "type": "auto_retry_end",
                "success": false,
                "attempt": 1,
                "finalError": "Retry cancelled",
            }),
        ]
    );
}

/// A turn that succeeds on the first attempt emits NO retry frame, even with
/// the sink installed.
#[tokio::test]
async fn print_json_plain_success_emits_no_retry_frames() {
    let calls = Arc::new(AtomicUsize::new(0));
    let frames = Arc::new(Mutex::new(Vec::new()));
    let mut deps = print_retry_deps(Arc::clone(&calls), 3, 0);
    let calls_for_stream = Arc::clone(&calls);
    deps.stream = Box::new(move |_request: ProviderRequest| {
        calls_for_stream.fetch_add(1, Ordering::SeqCst);
        Box::pin(async move { recovered_assistant() })
    });
    install_print_retry_events(&mut deps, Some(recording_sink(Arc::clone(&frames))));

    let result = run_with_options(deps, Context::default(), None, LoopOptions::default()).await;
    assert_eq!(result.terminal.stop_reason, StopReason::Stop);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert!(
        taken(&frames).is_empty(),
        "a first-attempt success must not emit auto_retry_*"
    );
}

/// Text mode (`run_print` passes `None`) leaves the loop field uninstalled, so
/// the text output never grows session-event lines.
#[test]
fn print_text_mode_installs_no_retry_sink() {
    let mut deps = print_loop_deps(
        Arc::new(ToolRegistry::new()),
        ToolScheduler::new(ToolSchedulerConfig::default()),
        PathBuf::from("."),
        None,
        None,
        None,
        ThinkingLevel::Off,
    );
    install_print_retry_events(&mut deps, None);
    assert!(deps.retry_events.is_none());
}

/// The mapping itself: every `AgentRetryEvent` field lands on the upstream
/// session-event wire shape (the interactive session's own mapping).
#[test]
fn agent_retry_event_mapping_matches_the_session_wire_shapes() {
    let start =
        agent_retry_event_to_session_event(&pir::agent::agent_loop::AgentRetryEvent::Start {
            attempt: 2,
            max_attempts: 4,
            delay_ms: 1_500,
            error_message: "boom".into(),
        });
    assert_eq!(
        session_event_to_json(&start),
        json!({
            "type": "auto_retry_start",
            "attempt": 2,
            "maxAttempts": 4,
            "delayMs": 1_500,
            "errorMessage": "boom",
        })
    );

    let end = agent_retry_event_to_session_event(&pir::agent::agent_loop::AgentRetryEvent::End {
        success: false,
        attempt: 4,
        final_error: Some("final".into()),
    });
    assert_eq!(
        session_event_to_json(&end),
        json!({
            "type": "auto_retry_end",
            "success": false,
            "attempt": 4,
            "finalError": "final",
        })
    );

    let success =
        agent_retry_event_to_session_event(&pir::agent::agent_loop::AgentRetryEvent::End {
            success: true,
            attempt: 1,
            final_error: None,
        });
    assert_eq!(
        session_event_to_json(&success),
        json!({ "type": "auto_retry_end", "success": true, "attempt": 1 }),
        "finalError is omitted on success"
    );
}

// ---------------------------------------------------------------------------
// End-to-end: `pir print --mode json` against a loopback SSE mock
// ---------------------------------------------------------------------------

/// How long a spawned `pir print` may take before the watchdog kills it.
const RUN_TIMEOUT: Duration = Duration::from_secs(120);

/// How long the watchdog waits for a killed child to be reaped.
const KILL_GRACE: Duration = Duration::from_secs(10);

/// How long the recorded request list may lag the child's exit.
const REQUEST_SETTLE: Duration = Duration::from_secs(5);

struct TempDir(PathBuf);

impl TempDir {
    fn new(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "pir-print-retry-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|duration| duration.as_nanos())
                .unwrap_or(0)
        ));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("create temp dir");
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A tiny blocking HTTP/1.1 SSE provider: the first `truncated` POSTs answer a
/// stream that ends WITHOUT `finish_reason`/`[DONE]` (a clean EOF the adapter
/// classifies `StreamTruncated` → `RetryableTurn`, and the transport does not
/// retry because a chunk was already delivered); later POSTs answer a complete
/// `stop` response.
struct MockProvider {
    base_url: String,
    requests: Arc<Mutex<Vec<Value>>>,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl MockProvider {
    fn start(truncated: usize) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback mock provider");
        listener
            .set_nonblocking(true)
            .expect("mock listener non-blocking");
        let addr = listener.local_addr().expect("mock local addr");
        let requests: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let recorded = Arc::clone(&requests);
        let stopping = Arc::clone(&stop);
        let thread = std::thread::spawn(move || {
            let mut posts = 0usize;
            while !stopping.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        if let Some(body) = serve(stream, posts < truncated) {
                            recorded
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner)
                                .push(body);
                            posts += 1;
                        }
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    Err(_) => break,
                }
            }
        });
        Self {
            base_url: format!("http://{addr}/v1"),
            requests,
            stop,
            thread: Some(thread),
        }
    }

    fn requests(&self) -> Vec<Value> {
        self.requests
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    fn wait_for_requests(&self, count: usize) -> Vec<Value> {
        let deadline = Instant::now() + REQUEST_SETTLE;
        loop {
            let requests = self.requests();
            if requests.len() >= count || Instant::now() >= deadline {
                return requests;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}

impl Drop for MockProvider {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

/// Read one HTTP request, answer it, and return the parsed POST body.
fn serve(mut stream: TcpStream, truncate: bool) -> Option<Value> {
    let _ = stream.set_nonblocking(false);
    let _ = stream.set_read_timeout(Some(Duration::from_secs(30)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(30)));
    let mut buffer: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 8192];
    let head_end = loop {
        if let Some(position) = find_subslice(&buffer, b"\r\n\r\n") {
            break position + 4;
        }
        if buffer.len() > 1 << 20 {
            return None;
        }
        match stream.read(&mut chunk) {
            Ok(0) => return None,
            Ok(read) => buffer.extend_from_slice(&chunk[..read]),
            Err(_) => return None,
        }
    };
    let head = String::from_utf8_lossy(&buffer[..head_end]).to_string();
    let content_length = head
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            if name.trim().eq_ignore_ascii_case("content-length") {
                value.trim().parse::<usize>().ok()
            } else {
                None
            }
        })
        .unwrap_or(0);
    let mut body = buffer[head_end..].to_vec();
    while body.len() < content_length {
        match stream.read(&mut chunk) {
            Ok(0) => break,
            Ok(read) => body.extend_from_slice(&chunk[..read]),
            Err(_) => break,
        }
    }
    let is_post = head.starts_with("POST");
    let parsed = if is_post && !body.is_empty() {
        serde_json::from_slice::<Value>(&body).ok()
    } else {
        None
    };
    let response = if is_post {
        sse_response(truncate)
    } else {
        http_response(
            "application/json",
            br#"{"object":"list","data":[{"id":"mock-model","object":"model"}]}"#.to_vec(),
        )
    };
    let _ = stream.write_all(&response);
    let _ = stream.flush();
    parsed
}

fn http_response(content_type: &str, body: Vec<u8>) -> Vec<u8> {
    let mut response = format!(
        "HTTP/1.1 200 OK\r\ncontent-type: {content_type}\r\ncache-control: no-cache\r\n\
         connection: close\r\ncontent-length: {}\r\n\r\n",
        body.len()
    )
    .into_bytes();
    response.extend_from_slice(&body);
    response
}

fn chunk(delta: Value, finish_reason: Option<&str>) -> String {
    let payload = json!({
        "id": "mock-print-retry",
        "object": "chat.completion.chunk",
        "created": 1,
        "model": "mock-model",
        "choices": [{ "index": 0, "delta": delta, "finish_reason": finish_reason }],
    });
    format!("data: {payload}\n\n")
}

fn sse_response(truncate: bool) -> Vec<u8> {
    let mut body = String::new();
    body.push_str(&chunk(json!({ "role": "assistant", "content": "" }), None));
    body.push_str(&chunk(json!({ "content": "OK" }), None));
    if truncate {
        // Deliberately NO finish_reason and NO `[DONE]`: a stream that died
        // after it started (the agent-level retry budget owns it).
        return http_response("text/event-stream", body.into_bytes());
    }
    body.push_str(&chunk(json!({}), Some("stop")));
    body.push_str("data: [DONE]\n\n");
    http_response("text/event-stream", body.into_bytes())
}

struct PrintRun {
    exit_code: Option<i32>,
    timed_out: bool,
    stdout: String,
    stderr: String,
}

impl PrintRun {
    fn stderr_tail(&self) -> String {
        let lines: Vec<&str> = self.stderr.lines().collect();
        let start = lines.len().saturating_sub(12);
        lines[start..].join("\n")
    }
}

struct Case {
    provider: MockProvider,
    run: PrintRun,
}

/// Start the mock, write an isolated agent dir (models.json + settings.json
/// with the retry block), and run one `pir print --mode json` turn.
fn run_case(label: &str, truncated: usize, retry: Value) -> Case {
    let provider = MockProvider::start(truncated);
    let root = TempDir::new(label);
    let agent = root.path().join("agent");
    let work = root.path().join("work");
    std::fs::create_dir_all(&agent).expect("create agent dir");
    std::fs::create_dir_all(&work).expect("create work dir");

    let models = json!({
        "providers": {
            "mock": {
                "name": "Mock",
                "baseUrl": provider.base_url.clone(),
                "api": "openai-completions",
                "apiKey": "sk-mock-print-retry-not-a-real-credential",
                "models": [{
                    "id": "mock-model",
                    "name": "Mock Model",
                    "reasoning": false,
                    "input": ["text"],
                    "contextWindow": 128000,
                    "maxTokens": 4096,
                }],
            }
        }
    });
    std::fs::write(
        agent.join("models.json"),
        serde_json::to_string_pretty(&models).expect("serialize models.json"),
    )
    .expect("write models.json");

    let settings = json!({
        "defaultProvider": "mock",
        "defaultModel": "mock-model",
        "defaultThinkingLevel": "off",
        "retry": retry,
    });
    std::fs::write(
        agent.join("settings.json"),
        serde_json::to_string_pretty(&settings).expect("serialize settings.json"),
    )
    .expect("write settings.json");

    let stdout_path = root.path().join("stdout.txt");
    let stderr_path = root.path().join("stderr.txt");
    let stdout_file = std::fs::File::create(&stdout_path).expect("create stdout file");
    let stderr_file = std::fs::File::create(&stderr_path).expect("create stderr file");

    let mut command = Command::new(env!("CARGO_BIN_EXE_pir"));
    command
        .current_dir(&work)
        .env("PIR_AGENT_DIR", &agent)
        .env("PIR_CODING_AGENT_DIR", &agent)
        .env("PI_CODING_AGENT_DIR", &agent)
        .env("HOME", root.path())
        .env("USERPROFILE", root.path())
        .env("PI_OFFLINE", "1")
        .env("PI_SKIP_VERSION_CHECK", "1")
        .env("NO_PROXY", "127.0.0.1,localhost")
        .env("no_proxy", "127.0.0.1,localhost")
        .env("RUST_MIN_STACK", "8388608")
        .args([
            "print",
            "--mode",
            "json",
            "--no-session",
            "--no-context-files",
            "--no-skills",
            "--no-prompt-templates",
            "--provider",
            "mock",
            "--model",
            "mock-model",
        ])
        .arg("Reply with the single word OK.")
        .stdin(Stdio::null())
        .stdout(Stdio::from(stdout_file))
        .stderr(Stdio::from(stderr_file));

    let mut child = command.spawn().expect("spawn the pir binary");
    let deadline = Instant::now() + RUN_TIMEOUT;
    let mut timed_out = false;
    let mut reaped = true;
    let status = loop {
        match child.try_wait().expect("poll the pir child") {
            Some(status) => break Some(status),
            None if Instant::now() >= deadline => {
                let _ = child.kill();
                let reap_deadline = Instant::now() + KILL_GRACE;
                reaped = loop {
                    match child.try_wait() {
                        Ok(Some(_)) => break true,
                        Ok(None) if Instant::now() >= reap_deadline => break false,
                        Ok(None) => std::thread::sleep(Duration::from_millis(20)),
                        Err(_) => break false,
                    }
                };
                timed_out = true;
                break None;
            }
            None => std::thread::sleep(Duration::from_millis(20)),
        }
    };

    let stdout = std::fs::read_to_string(&stdout_path).unwrap_or_default();
    let stderr = std::fs::read_to_string(&stderr_path).unwrap_or_default();
    let run = PrintRun {
        exit_code: status.and_then(|status| status.code()),
        timed_out,
        stdout,
        stderr,
    };
    assert!(
        !run.timed_out && reaped,
        "pir print did not exit within {RUN_TIMEOUT:?} (case {label}, reaped={reaped}); \
         stderr tail:\n{}",
        run.stderr_tail()
    );
    Case { provider, run }
}

/// Every `auto_retry_*` line on stdout, parsed in order.
fn auto_retry_frames(run: &PrintRun) -> Vec<Value> {
    run.stdout
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|value| {
            value["type"]
                .as_str()
                .is_some_and(|kind| kind.starts_with("auto_retry"))
        })
        .collect()
}

/// The first truncated stream is retried by the AGENT loop; JSON mode carries
/// the loop's `auto_retry_start` / `auto_retry_end` line pair and the second
/// request succeeds. Exactly one pair — the transport layer never retried.
#[test]
fn json_mode_agent_retry_emits_start_and_success_end() {
    let case = run_case(
        "success",
        1,
        json!({ "maxRetries": 1, "baseDelayMs": 0, "maxAgentDelayMs": 0 }),
    );
    assert_eq!(
        case.run.exit_code,
        Some(0),
        "stderr:\n{}",
        case.run.stderr_tail()
    );
    assert_eq!(
        auto_retry_frames(&case.run),
        vec![
            json!({
                "type": "auto_retry_start",
                "attempt": 1,
                "maxAttempts": 1,
                "delayMs": 0,
                "errorMessage": "Stream ended without finish_reason",
            }),
            json!({
                "type": "auto_retry_end",
                "success": true,
                "attempt": 1,
            }),
        ],
        "stdout:\n{}",
        case.run.stdout
    );
    let requests = case.provider.wait_for_requests(2);
    assert_eq!(
        requests.len(),
        2,
        "one initial request plus exactly one agent-level re-issue"
    );
}

/// A spent budget emits one start per retry and a failing end whose
/// `finalError` is the terminal provider error.
#[test]
fn json_mode_agent_retry_exhausted_emits_final_error() {
    let case = run_case(
        "exhausted",
        usize::MAX,
        json!({ "maxRetries": 2, "baseDelayMs": 0, "maxAgentDelayMs": 0 }),
    );
    assert_eq!(
        case.run.exit_code,
        Some(0),
        "JSON mode reports provider errors in-band; stderr:\n{}",
        case.run.stderr_tail()
    );
    assert_eq!(
        auto_retry_frames(&case.run),
        vec![
            json!({
                "type": "auto_retry_start",
                "attempt": 1,
                "maxAttempts": 2,
                "delayMs": 0,
                "errorMessage": "Stream ended without finish_reason",
            }),
            json!({
                "type": "auto_retry_start",
                "attempt": 2,
                "maxAttempts": 2,
                "delayMs": 0,
                "errorMessage": "Stream ended without finish_reason",
            }),
            json!({
                "type": "auto_retry_end",
                "success": false,
                "attempt": 2,
                "finalError": "Stream ended without finish_reason",
            }),
        ],
        "stdout:\n{}",
        case.run.stdout
    );
    let requests = case.provider.wait_for_requests(3);
    assert_eq!(requests.len(), 3, "initial attempt plus two retries");
}

/// A run that succeeds on its first request emits no retry frame: the sink is
/// installed but the loop has nothing to report.
#[test]
fn json_mode_plain_success_has_no_auto_retry_lines() {
    let case = run_case(
        "plain",
        0,
        json!({ "maxRetries": 3, "baseDelayMs": 0, "maxAgentDelayMs": 0 }),
    );
    assert_eq!(
        case.run.exit_code,
        Some(0),
        "stderr:\n{}",
        case.run.stderr_tail()
    );
    assert!(
        auto_retry_frames(&case.run).is_empty(),
        "a first-attempt success must not emit auto_retry_*: {}",
        case.run.stdout
    );
    assert_eq!(case.provider.wait_for_requests(1).len(), 1);
}

// ---------------------------------------------------------------------------
// H11: the threshold compaction summarizer's own retry
// (`summarization_retry_*`, source `compaction` + reason `threshold`)
// ---------------------------------------------------------------------------

/// A model whose window makes a small probe context cross the threshold: with
/// `reserve_tokens: 4_000` the gate sits at 4_000 estimated tokens.
fn threshold_test_model() -> Model {
    serde_json::from_value(json!({
        "id": "threshold-model",
        "name": "Threshold Model",
        "api": "openai-completions",
        "provider": "test",
        "baseUrl": "http://localhost",
        "reasoning": false,
        "input": ["text"],
        "cost": {},
        "contextWindow": 8000,
        "maxTokens": 4096,
    }))
    .expect("threshold model fixture")
}

fn text_assistant(text: &str, model: &str) -> AssistantMessage {
    AssistantMessage {
        api: "fake".into(),
        provider: "test".into(),
        model: model.to_owned(),
        content: vec![ContentBlock::Text(TextContent {
            text: text.to_owned(),
            text_signature: None,
        })],
        stop_reason: StopReason::Stop,
        usage: Usage {
            total_tokens: 5,
            ..Usage::default()
        },
        timestamp: 1,
        ..AssistantMessage::default()
    }
}

fn user_message(text: &str) -> Message {
    Message::User(UserMessage {
        role: UserRole::User,
        content: vec![ContentBlock::Text(TextContent {
            text: text.to_owned(),
            text_signature: None,
        })],
        tools_added: None,
        tools_removed: None,
        sections: None,
        timestamp: 1,
    })
}

/// The probe's trailing assistant. Its usage is the context size upstream's
/// estimator reads (`estimateContextTokens` prefers the last assistant usage),
/// so it must report the tokens above the probe's compaction gate.
fn usage_assistant(usage_tokens: u64) -> AssistantMessage {
    AssistantMessage {
        api: "fake".into(),
        provider: "test".into(),
        model: "threshold-model".into(),
        content: vec![ContentBlock::Text(TextContent {
            text: "done".to_owned(),
            text_signature: None,
        })],
        stop_reason: StopReason::Stop,
        usage: Usage {
            input: usage_tokens,
            total_tokens: usage_tokens,
            ..Usage::default()
        },
        timestamp: 1,
        ..AssistantMessage::default()
    }
}

/// The summarizer double: the first request answers an ENDED stream with no
/// terminal message — the "summarization stream did not terminate" shape the
/// summarizer retries once — and every later request answers a stop message.
struct SummarizerProvider {
    model: Model,
    calls: Arc<AtomicUsize>,
    drop_first: bool,
}

impl Provider for SummarizerProvider {
    // The double registers under its model's provider string; `id` and the
    // model's `provider` are deliberately the same value here.
    #[allow(clippy::misnamed_getters)]
    fn id(&self) -> &str {
        &self.model.provider
    }

    fn name(&self) -> &str {
        "summarizer double"
    }

    fn get_models(&self) -> Vec<Model> {
        vec![self.model.clone()]
    }

    fn api_key_auth(&self) -> Vec<ApiKeyAuth> {
        Vec::new()
    }

    fn stream(
        &self,
        _model: &Model,
        _context: Context,
        _options: StreamOptions,
    ) -> AssistantMessageEventStream {
        let attempt = self.calls.fetch_add(1, Ordering::SeqCst);
        let stream = AssistantMessageEventStream::new();
        if attempt == 0 && self.drop_first {
            // No terminal event: `CompleteFn` reports an error, exactly like a
            // summarizer stream that died before its final message.
            stream.end(None);
        } else {
            stream.push(AssistantMessageEventStream::done_event(
                TerminalReason::Stop,
                text_assistant("COMPACTION SUMMARY", "threshold-model"),
            ));
        }
        stream
    }
}

struct ThresholdProbe {
    frames: Vec<Value>,
    calls: usize,
    compacted: bool,
    stored_compactions: usize,
}

/// Run the production threshold-compaction hook directly with a JSON-mode
/// sink: the context crosses the gate, the hook runs the resident pipeline and
/// persists the compaction entry into a real (in-memory) session.
async fn run_threshold_compaction_probe(label: &str, drop_first: bool) -> ThresholdProbe {
    let frames = Arc::new(Mutex::new(Vec::new()));
    let calls = Arc::new(AtomicUsize::new(0));
    let model = threshold_test_model();
    let registry = {
        let mut registry = ProviderRegistry::default();
        registry.set_provider(Arc::new(SummarizerProvider {
            model: model.clone(),
            calls: Arc::clone(&calls),
            drop_first,
        }));
        Arc::new(registry)
    };
    let cwd = std::env::temp_dir().join(format!(
        "pir-threshold-retry-{label}-{}",
        std::process::id()
    ));
    let session: SharedSession = Arc::new(tokio::sync::Mutex::new(Some(
        SessionManager::in_memory(cwd.to_string_lossy().as_ref(), None).expect("in-memory session"),
    )));
    let hook = print_compaction_hook(
        CompactionConfig {
            enabled: true,
            reserve_tokens: 4_000,
            keep_recent_tokens: 1,
        },
        model,
        registry,
        None,
        ThinkingLevel::Off,
        pir::ai::types::ProviderStreamHooks::default(),
        Arc::clone(&session),
        None,
        None,
        Some(recording_sink(Arc::clone(&frames))),
    );
    let context = Arc::new(Context {
        system_prompt: None,
        messages: vec![
            // ~6_500 estimated tokens of conversation: the cut-point walk
            // summarizes this span.
            user_message(&"threshold compaction probe line\n".repeat(1_000)),
            // The recorded usage is the trigger: 12_000 tokens > the
            // 8_000 - 4_000 gate.
            Message::Assistant(usage_assistant(12_000)),
        ],
        tools: None,
    });
    let result = hook(Arc::clone(&context)).await;
    let stored_compactions = {
        let guard = session.lock().await;
        guard
            .as_ref()
            .map(|manager| {
                manager
                    .entries()
                    .iter()
                    .filter(|entry| matches!(entry, FileEntry::Compaction(_)))
                    .count()
            })
            .unwrap_or(0)
    };
    ThresholdProbe {
        frames: taken(&frames),
        calls: calls.load(Ordering::SeqCst),
        compacted: !Arc::ptr_eq(&result, &context),
        stored_compactions,
    }
}

fn summarization_retry_frames(frames: &[Value]) -> Vec<&Value> {
    frames
        .iter()
        .filter(|frame| {
            frame["type"]
                .as_str()
                .is_some_and(|kind| kind.starts_with("summarization_retry"))
        })
        .collect()
}

/// A dropped summarizer stream is retried ONCE by the summarizer's own budget,
/// and that lifecycle reaches the JSON session-event sink as the upstream
/// `summarization_retry_*` triple between the compaction pair, with
/// `source: "compaction"` and `reason: "threshold"`.
#[tokio::test]
async fn print_json_threshold_compaction_retry_emits_the_summarization_lifecycle() {
    let probe = run_threshold_compaction_probe("retry", true).await;
    assert!(
        probe.compacted,
        "the recovered summary must complete the compaction (frames: {:?}, calls: {})",
        probe.frames, probe.calls
    );
    assert_eq!(probe.stored_compactions, 1);
    assert_eq!(probe.calls, 2, "initial summarizer call plus one retry");
    assert_eq!(
        probe.frames.len(),
        5,
        "compaction pair + three retry frames: {:?}",
        probe.frames
    );
    assert_eq!(
        probe.frames[0],
        json!({ "type": "compaction_start", "reason": "threshold" })
    );
    assert_eq!(
        probe.frames[1],
        json!({
            "type": "summarization_retry_scheduled",
            "attempt": 1,
            "maxAttempts": 1,
            "delayMs": 0,
            "errorMessage": "summarization stream did not terminate",
        })
    );
    assert_eq!(
        probe.frames[2],
        json!({
            "type": "summarization_retry_attempt_start",
            "source": "compaction",
            "reason": "threshold",
        })
    );
    assert_eq!(
        probe.frames[3],
        json!({ "type": "summarization_retry_finished" })
    );
    assert_eq!(probe.frames[4]["type"], json!("compaction_end"));
    assert_eq!(probe.frames[4]["reason"], json!("threshold"));
    assert!(
        probe.frames[4].get("errorMessage").is_none(),
        "the recovered run succeeds: {}",
        probe.frames[4]
    );
}

/// A summarizer that succeeds on its first attempt emits NO
/// `summarization_retry_*` frame: the observer is installed but the retry
/// budget never fires, and the compaction pair still brackets the run.
#[tokio::test]
async fn print_json_threshold_compaction_without_retry_emits_no_summarization_frames() {
    let probe = run_threshold_compaction_probe("clean", false).await;
    assert!(
        probe.compacted,
        "frames: {:?}, calls: {}",
        probe.frames, probe.calls
    );
    assert_eq!(probe.calls, 1, "one summarizer call, no retry");
    assert_eq!(probe.stored_compactions, 1);
    assert!(
        summarization_retry_frames(&probe.frames).is_empty(),
        "a first-attempt summarizer success emits no summarization_retry_*: {:?}",
        probe.frames
    );
    assert_eq!(
        probe.frames.first(),
        Some(&json!({ "type": "compaction_start", "reason": "threshold" }))
    );
    assert_eq!(
        probe.frames.last().map(|frame| frame["type"].clone()),
        Some(json!("compaction_end"))
    );
}

/// A `complete` that drops the first call and recovers on the second (the
/// branch summarizer's own dropped-stream shape).
fn dropped_then_recovered_complete(calls: Arc<AtomicUsize>) -> Arc<CompleteFn> {
    Arc::new(Box::new(move |_context: Context| {
        let calls = Arc::clone(&calls);
        Box::pin(async move {
            if calls.fetch_add(1, Ordering::SeqCst) == 0 {
                Err("summarization stream did not terminate".to_owned())
            } else {
                Ok(text_assistant("BRANCH SUMMARY", "test-model"))
            }
        })
    }))
}

/// H8's `navigateTree({summarize})` retry path keeps reporting
/// `source: "branchSummary"` and exactly three frames — the threshold
/// compaction's new install point cannot double-report or relabel it.
#[tokio::test]
async fn print_branch_summary_retry_events_keep_the_branch_summary_source() {
    let dir = TempDir::new("branch-summary-retry");
    let mut storage = Some(
        SessionManager::in_memory(dir.path().to_string_lossy().as_ref(), None)
            .expect("in-memory session"),
    );
    let first_id = {
        let manager = storage.as_mut().expect("session");
        manager
            .append_message(user_message("first prompt"))
            .expect("append user")
    };
    {
        let manager = storage.as_mut().expect("session");
        manager
            .append_message(Message::Assistant(text_assistant(
                "branch reply",
                "test-model",
            )))
            .expect("append assistant");
    }
    let calls = Arc::new(AtomicUsize::new(0));
    let binding = NavigateSummaryBinding {
        model: test_model(),
        complete: dropped_then_recovered_complete(Arc::clone(&calls)),
        reserve_tokens: 1024,
    };
    let action = SessionCommandAction::NavigateTree {
        target_id: first_id,
        summarize: true,
        custom_instructions: None,
        replace_instructions: false,
        label: None,
    };
    let step =
        prepare_session_command_to_storage(&mut storage, dir.path(), &action, Some(&binding))
            .expect("prepare navigate");
    let plan = match step {
        SessionCommandStep::Navigate(plan) => plan,
        SessionCommandStep::Applied => panic!("expected a navigate plan"),
    };
    let frames = Arc::new(Mutex::new(Vec::new()));
    let outcome = generate_navigation_summary_with_retry_events(
        &plan,
        &binding,
        recording_sink(Arc::clone(&frames)),
    )
    .await
    .expect("the retried branch summary must succeed");
    assert!(outcome.summary.is_some());
    assert_eq!(
        calls.load(Ordering::SeqCst),
        2,
        "one dropped call, one retry"
    );
    assert_eq!(
        taken(&frames),
        vec![
            json!({
                "type": "summarization_retry_scheduled",
                "attempt": 1,
                "maxAttempts": 1,
                "delayMs": 0,
                "errorMessage": "summarization stream did not terminate",
            }),
            json!({
                "type": "summarization_retry_attempt_start",
                "source": "branchSummary",
            }),
            json!({ "type": "summarization_retry_finished" }),
        ]
    );
}
