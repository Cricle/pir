//! `createAgentSession` error surface (the SDK child-session bridge).
//!
//! The shim's `pirAgentSessionRun` spawns `pir print --mode json` per prompt
//! and resolves the last assistant reply. `--mode json` deliberately exits 0
//! with a failed turn reported IN-BAND as an event (upstream print-mode.ts
//! maps a terminal `error`/`aborted` stopReason onto the exit code only in
//! TEXT mode; pinned by
//! `tests/suites/cli_entry_contract.rs::json_mode_provider_error_exits_0_with_header_and_error_event`).
//! Before the fix, an errored turn therefore resolved as `""` — a provider
//! failure read as a successful empty reply (the `reply2: ""` shape first
//! observed in `tests/session_manager_ext.rs`).
//!
//! This target drives the run path three times through one live session:
//!
//!   1. a normal turn resolves with the reply text (the unchanged contract);
//!   2. a provider failure (HTTP 401, non-retryable, fast) arrives in-band
//!      and the prompt REJECTS with the terminal message's `errorMessage`
//!      (upstream print-mode.ts:139-156's error text);
//!   3. a hanging turn is aborted via `session.abort()`: the kill removes the
//!      child before its terminal message can be delivered, and the prompt
//!      still rejects (`Request aborted`, upstream's stopReason fallback)
//!      instead of resolving a stale reply.
//!
//! Turn 3 also pins the queue: a failed turn rejects only ITS prompt's
//! promise — a later prompt of the same session must still spawn a child.
//!
//! Fixtures follow `tests/session_manager_ext.rs` (runtime-seeded JS probe +
//! loopback provider) and `tests/suites/sdk.rs` (in-process mock provider
//! idiom); environment variables are set once in this single-test binary
//! because the spawned child inherits the process environment.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicUsize;
use std::sync::Arc;

use futures_util::StreamExt;

use pir::ai::auth::ApiKeyAuth;
use pir::ai::event_stream::AssistantMessageEventStream;
use pir::ai::provider::{Provider, ProviderRegistry};
use pir::ai::types::{
    AssistantMessage, Context, Model, ModelCost, ModelCostRates, StopReason, StreamOptions,
    TerminalReason, ThinkingLevel, Usage,
};
use pir::extensions::{ExtensionConfig, ExtensionRuntime};
use pir::sdk::{AgentBuilder, SdkEvent, SessionMode};
use serde_json::{json, Value};

/// Per-test temp directory removed on drop.
struct TempDir(PathBuf);

impl TempDir {
    fn new(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "pir-sdk-errors-{label}-{}-{}",
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

/// The script-mode module runner boot (`pir js` defines the same shape; see
/// `main_cli/js_mode.rs` + `script_boot.js.txt`).
const SCRIPT_RUN_BOOT: &str = r#"
globalThis.__pirScriptRun = async (moduleName) => {
  try {
    await import(moduleName);
    return JSON.stringify({ ok: true });
  } catch (error) {
    const message = error && error.message !== undefined ? String(error.message) : String(error);
    const rawStack = error && error.stack ? String(error.stack) : "";
    const stack = rawStack.indexOf(message) >= 0 ? rawStack : message + (rawStack ? "\n" + rawStack : "");
    return JSON.stringify({ ok: false, message: message, stack: stack });
  }
};
"#;

fn script_config_at(cwd: &Path) -> ExtensionConfig {
    ExtensionConfig {
        cwd: cwd.to_path_buf(),
        mode: "script".to_string(),
        has_ui: false,
        model: None,
        session_id: None,
        system_prompt: String::new(),
        is_idle: true,
        limits: Some(Default::default()),
    }
}

// ---------------------------------------------------------------------------
// Loopback provider fixture
// ---------------------------------------------------------------------------

/// One SSE chunk pair + `[DONE]` — the openai-completions shape the child's
/// provider consumes (`tests/session_manager_ext.rs`'s mock, one turn).
const MOCK_SSE: &str = concat!(
    "data: {\"id\":\"chatcmpl-1\",\"object\":\"chat.completion.chunk\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"mock reply\"},\"finish_reason\":null}]}\n\n",
    "data: {\"id\":\"chatcmpl-1\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":1,\"completion_tokens\":2,\"total_tokens\":3}}\n\n",
    "data: [DONE]\n\n",
);

/// Read one full HTTP request (headers, then `Content-Length` bytes) before
/// answering; the response closes the connection, so one request = one
/// connection (the same fixture shape as `tests/session_manager_ext.rs`).
fn read_http_request(stream: &mut std::net::TcpStream) -> Vec<u8> {
    let _ = stream.set_read_timeout(Some(std::time::Duration::from_secs(10)));
    let mut received: Vec<u8> = Vec::new();
    let mut scratch = [0u8; 8192];
    loop {
        let header_end = received
            .windows(4)
            .position(|window| window == b"\r\n\r\n")
            .map(|index| index + 4);
        if let Some(head) = header_end {
            let headers = String::from_utf8_lossy(&received[..head]).to_ascii_lowercase();
            let content_length = headers
                .lines()
                .find_map(|line| line.strip_prefix("content-length:"))
                .and_then(|value| value.trim().parse::<usize>().ok())
                .unwrap_or(0);
            if received.len() >= head + content_length || received.len() > 64 * 1024 * 1024 {
                return received;
            }
        }
        match stream.read(&mut scratch) {
            Ok(0) | Err(_) => return received,
            Ok(read) => received.extend_from_slice(&scratch[..read]),
        }
    }
}

/// Answer one request: the transcript carries the newest prompt text, so the
/// routing keys off it (`Fail now` → 401, `Hang please` → hold the connection,
/// anything else with `Say hello` → the SSE success turn).
fn answer_mock_request(stream: &mut std::net::TcpStream, request: &[u8]) {
    let text = String::from_utf8_lossy(request);
    if text.contains("Hang please") {
        // No response: keep the connection open until the peer goes away
        // (the abort kill) or the stall budget expires.
        let _ = stream.set_read_timeout(Some(std::time::Duration::from_millis(200)));
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        let mut scratch = [0u8; 1024];
        while std::time::Instant::now() < deadline {
            match stream.read(&mut scratch) {
                Ok(0) => break,
                Ok(_) => continue,
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) =>
                {
                    continue
                }
                Err(_) => break,
            }
        }
        return;
    }
    let body = if text.contains("Fail now") {
        // HTTP 401: non-retryable, so the child fails fast with an in-band
        // error turn (no auto-retry backoff). The body must stay clear of the
        // retryable provider patterns (`src/ai/util.rs`).
        "{\"error\":{\"message\":\"mock provider rejected the request\"}}".to_owned()
    } else {
        MOCK_SSE.to_owned()
    };
    let status = if text.contains("Fail now") {
        "HTTP/1.1 401 Unauthorized"
    } else {
        "HTTP/1.1 200 OK"
    };
    let content_type = if text.contains("Fail now") {
        "application/json"
    } else {
        "text/event-stream"
    };
    let response = format!(
        "{status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(response.as_bytes());
    let _ = stream.flush();
}

// ---------------------------------------------------------------------------
// The probe
// ---------------------------------------------------------------------------

const SDK_ERRORS_PROBE: &str = r#"
import { SessionManager, createAgentSession } from "@earendil-works/pi-coding-agent";

export default (pi) => {};

const cwd = __PIR_CWD__;
const sessionDir = __PIR_SESSION_DIR__;
const out = {};

const sm = SessionManager.create(cwd, sessionDir);
const { session } = await createAgentSession({
  cwd: cwd,
  sessionManager: sm,
  model: { provider: "testprov", id: "test-model" },
});

// The raw in-band terminal events; `message_end` is the frame the shim
// consumes and the only place a JSON-mode child reports stopReason/errorMessage.
const assistantEnds = [];
session.subscribe((event) => {
  if (event === null || typeof event !== "object") return;
  if (event.type === "message_end" && event.message && event.message.role === "assistant") {
    assistantEnds.push({
      stopReason: event.message.stopReason,
      errorMessage: event.message.errorMessage,
    });
  }
});

// Turn 1: a normal turn resolves the reply text (unchanged contract).
out.reply1 = await session.prompt("Say hello");
out.turn1Ends = assistantEnds.length;
out.messagesAfterTurn1 = session.messages.length;

// Turn 2: the provider fails in-band. The child exits 0 (json-mode parity),
// so the failure is visible only through the terminal message — the run must
// reject with its errorMessage, never resolve with "".
out.reply2Rejected = false;
try {
  out.reply2 = await session.prompt("Fail now");
} catch (error) {
  out.reply2Rejected = true;
  out.reply2Error = error && error.message !== undefined ? String(error.message) : String(error);
}
out.turn2Ends = assistantEnds.slice(1);
out.messagesAfterTurn2 = session.messages.length;
out.lastAssistantStopReason = (() => {
  const messages = session.messages;
  for (let i = messages.length - 1; i >= 0; i--) {
    if (messages[i] && messages[i].role === "assistant") return messages[i].stopReason;
  }
  return null;
})();

// Turn 3: the provider never answers; the caller aborts mid-run. The kill
// removes the child before its terminal message can be delivered, so the run
// must reject with the aborted surface — not resolve the stale turn-2 reply
// (`session.abort()` must also reach a session whose previous turn rejected).
const pending = session.prompt("Hang please");
const killer = setInterval(() => {
  session.abort();
}, 250);
out.abortRejected = false;
try {
  out.abortReply = await pending;
} catch (error) {
  out.abortRejected = true;
  out.abortError = error && error.message !== undefined ? String(error.message) : String(error);
} finally {
  clearInterval(killer);
}

globalThis.__sdkErrorsResult = out;
"#;

#[tokio::test(flavor = "current_thread")]
async fn agent_session_prompt_rejects_on_failed_and_aborted_turns() {
    let dir = TempDir::new("cas-errors");
    let agent_dir = TempDir::new("cas-errors-agent");
    let session_dir = dir.path().join("sessions");
    std::fs::create_dir_all(&session_dir).expect("session dir");

    // Loopback provider: one thread serves every child request (one
    // connection per prompt run).
    let listener = TcpListener::bind("127.0.0.1:0").expect("loopback bind");
    let port = listener.local_addr().expect("local addr").port();
    let _server = std::thread::spawn(move || {
        for stream in listener.incoming() {
            // A transient accept error must not end the fixture: the test
            // drives three turns.
            let Ok(mut stream) = stream else { continue };
            let request = read_http_request(&mut stream);
            answer_mock_request(&mut stream, &request);
        }
    });

    // Isolated agent dir: models.json defines the mock provider; the spawned
    // child inherits these environment settings.
    let models = json!({
        "providers": {
            "testprov": {
                "name": "Test Provider",
                "baseUrl": format!("http://127.0.0.1:{port}/v1"),
                "apiKey": "test-key",
                "api": "openai-completions",
                "models": [{ "id": "test-model", "name": "Test Model" }],
            }
        }
    });
    std::fs::write(agent_dir.path().join("models.json"), models.to_string())
        .expect("write models.json");
    std::env::set_var("PIR_EXECUTABLE", env!("CARGO_BIN_EXE_pir"));
    std::env::set_var("PIR_AGENT_DIR", agent_dir.path());
    std::env::set_var("TESTPROV_API_KEY", "test-key");

    let source = SDK_ERRORS_PROBE
        .replace(
            "__PIR_CWD__",
            &format!("{:?}", dir.path().to_string_lossy().replace('\\', "/")),
        )
        .replace(
            "__PIR_SESSION_DIR__",
            &format!("{:?}", session_dir.to_string_lossy().replace('\\', "/")),
        );

    let rt = ExtensionRuntime::new(script_config_at(dir.path()))
        .await
        .expect("runtime construction");
    rt.eval_boot(SCRIPT_RUN_BOOT).await.expect("script boot");
    rt.seed_module("pir-ext:sdk_agent_session_errors.js", &source)
        .expect("seed module");
    let run = tokio::time::timeout(
        std::time::Duration::from_secs(240),
        rt.run_script("pir-ext:sdk_agent_session_errors.js"),
    )
    .await
    .expect("SDK error-surface probe timed out");
    match run {
        Ok(Ok(())) => {}
        Ok(Err(stack)) => panic!("probe module failed: {stack}"),
        Err(error) => panic!("script run failed: {error}"),
    }
    let v: Value = rt
        .eval_json("JSON.stringify(globalThis.__sdkErrorsResult ?? null)")
        .await
        .expect("read probe results");

    // Turn 1: a normal turn keeps resolving the last assistant reply.
    assert_eq!(v["reply1"], "mock reply", "{v}");
    assert_eq!(v["turn1Ends"], 1, "one assistant message_end per turn: {v}");
    // Current session lifecycle: turn 1 appends the user message, the
    // tool-loadout declaration and the assistant reply (the same 1 -> 3 shape
    // `session_manager_ext` pins): user + declaration + assistant.
    assert_eq!(v["messagesAfterTurn1"], 3, "{v}");

    // Turn 2: the 401 is in-band, and the run rejects with exactly that text.
    assert_eq!(
        v["turn2Ends"][0]["stopReason"], "error",
        "the in-band terminal message must carry stopReason error: {v}"
    );
    let in_band = v["turn2Ends"][0]["errorMessage"].as_str().unwrap_or("");
    assert!(
        in_band.contains("401"),
        "the mock 401 must reach the in-band errorMessage: {v}"
    );
    assert_eq!(
        v["reply2Rejected"], true,
        "a failed turn must reject, never resolve empty text: {v}"
    );
    assert_eq!(
        v["reply2Error"].as_str(),
        Some(in_band),
        "the rejection must carry the terminal errorMessage verbatim: {v}"
    );
    // Turn 2 adds only the user message and the failed assistant reply (the
    // tool-loadout declaration is unchanged), so the store grows 3 -> 5.
    assert_eq!(
        v["messagesAfterTurn2"], 5,
        "the failed assistant message still lands in session.messages: {v}"
    );
    assert_eq!(v["lastAssistantStopReason"], "error", "{v}");

    // Turn 3: the aborted run rejects; it never resolves a stale reply.
    assert_eq!(v["abortRejected"], true, "an aborted run must reject: {v}");
    assert_eq!(
        v["abortError"], "Request aborted",
        "no terminal message can follow a killed child; upstream's stopReason fallback applies: {v}"
    );
    assert!(
        v["abortReply"].is_null(),
        "the aborted prompt must not resolve: {v}"
    );
}

// ---------------------------------------------------------------------------
// `prompt_stream` admission must be released when the RUN settles, not when
// the consumer has drained the terminal events (R1-L6-015 / FIX-C16):
// a consumer that keeps a small-capacity stream open but never polls it would
// otherwise hold the agent's streaming slot forever.
// ---------------------------------------------------------------------------

/// In-process scripted provider: one stop turn per request, no network.
struct ScriptedProvider {
    calls: AtomicUsize,
}

impl Provider for ScriptedProvider {
    fn id(&self) -> &str {
        "scripted"
    }

    fn name(&self) -> &str {
        "Scripted provider"
    }

    fn get_models(&self) -> Vec<Model> {
        vec![scripted_model()]
    }

    fn api_key_auth(&self) -> Vec<ApiKeyAuth> {
        // `with_registry` streams unchecked, so no key source is required.
        Vec::new()
    }

    fn stream(
        &self,
        _model: &Model,
        _context: Context,
        _options: StreamOptions,
    ) -> AssistantMessageEventStream {
        self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let stream = AssistantMessageEventStream::new();
        stream.push(AssistantMessageEventStream::done_event(
            TerminalReason::Stop,
            AssistantMessage {
                api: "scripted".into(),
                provider: "scripted".into(),
                model: "scripted-model".into(),
                stop_reason: StopReason::Stop,
                timestamp: 1,
                usage: Usage::default(),
                ..Default::default()
            },
        ));
        stream
    }
}

fn scripted_model() -> Model {
    Model {
        id: "scripted-model".into(),
        name: "Scripted model".into(),
        api: "scripted-api".into(),
        provider: "scripted".into(),
        base_url: "http://127.0.0.1:1".into(),
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
        context_window: 8_192,
        max_tokens: 512,
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

#[tokio::test(flavor = "current_thread")]
async fn prompt_stream_releases_admission_when_the_run_settles_without_polling() {
    let agent_dir = TempDir::new("admission-agent");
    let provider = Arc::new(ScriptedProvider {
        calls: AtomicUsize::new(0),
    });
    let mut registry = ProviderRegistry::default();
    let provider_box: Arc<dyn Provider> = provider.clone();
    registry.set_provider(provider_box);
    let agent = AgentBuilder::new()
        .with_registry(Arc::new(registry), scripted_model())
        .agent_dir(agent_dir.path())
        .no_session()
        .no_compaction()
        .thinking_level(ThinkingLevel::Off)
        .build()
        .await
        .expect("scripted agent builds");

    // A capacity-1 channel: intermediate events fill it, so the terminal
    // sends park on backpressure until someone polls the stream.
    agent.set_event_channel_capacity(1);

    // Start the first run and never poll its stream.
    let first = agent.prompt_stream("first");
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
    // `is_idle()` is also true BEFORE the spawned task claims the session, so
    // first wait until the run has actually reached the provider...
    while provider.calls.load(std::sync::atomic::Ordering::SeqCst) == 0 {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the first scripted run never reached the provider"
        );
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    // ... then until the whole session run has settled.
    while !agent.is_idle() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the first scripted run never settled"
        );
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    // Let the spawned task run past `session.prompt`, release admission and
    // park on the backpressured terminal send (pre-fix it would still hold
    // admission there, because the guard only released it on Drop).
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    // The first stream is still open and unpolled, but its run is over: the
    // second stream must be ADMITTED (pre-fix, `stream_active` stayed true
    // until the unpolled terminal pair was delivered).
    let second: Vec<SdkEvent> = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        agent.prompt_stream("second").collect(),
    )
    .await
    .expect("the second stream timed out");
    assert!(
        !second
            .iter()
            .any(|event| matches!(event, SdkEvent::PromptRejected { .. })),
        "a settled, unpolled stream must not hold the streaming admission \
         ('another stream is already streaming'): {second:?}"
    );
    assert!(
        matches!(second.last(), Some(SdkEvent::AgentSettled { .. })),
        "the second stream must deliver its own terminal pair: {second:?}"
    );

    // The old stream still delivers its terminal events once drained.
    let first_events: Vec<SdkEvent> =
        tokio::time::timeout(std::time::Duration::from_secs(10), first.collect())
            .await
            .expect("the first stream timed out");
    assert!(
        matches!(first_events.last(), Some(SdkEvent::AgentSettled { .. })),
        "the old stream must still end with AgentSettled: {first_events:?}"
    );

    agent.shutdown().await.expect("shutdown");
}

// ---------------------------------------------------------------------------
// A dropping OLD stream must not tear down a NEWER stream's sink/admission
// (P1-01): `StreamRunGuard` is generation-scoped and detaches only its own
// channel.
// ---------------------------------------------------------------------------

/// In-process scripted provider whose FIRST request completes immediately and
/// whose later requests wait for a test-issued permit. Lets a test hold a
/// newer stream running while it drains/drops an older one.
struct GatedProvider {
    calls: AtomicUsize,
    release: Arc<tokio::sync::Semaphore>,
}

impl GatedProvider {
    fn new() -> Self {
        Self {
            calls: AtomicUsize::new(0),
            release: Arc::new(tokio::sync::Semaphore::new(0)),
        }
    }

    fn release_one(&self) {
        self.release.add_permits(1);
    }
}

impl Provider for GatedProvider {
    fn id(&self) -> &str {
        "scripted"
    }

    fn name(&self) -> &str {
        "Gated provider"
    }

    fn get_models(&self) -> Vec<Model> {
        vec![scripted_model()]
    }

    fn api_key_auth(&self) -> Vec<ApiKeyAuth> {
        // `with_registry` streams unchecked, so no key source is required.
        Vec::new()
    }

    fn stream(
        &self,
        _model: &Model,
        _context: Context,
        _options: StreamOptions,
    ) -> AssistantMessageEventStream {
        let call = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let stream = AssistantMessageEventStream::new();
        let done = AssistantMessageEventStream::done_event(
            TerminalReason::Stop,
            AssistantMessage {
                api: "gated".into(),
                provider: "gated".into(),
                model: "scripted-model".into(),
                stop_reason: StopReason::Stop,
                timestamp: 1,
                usage: Usage::default(),
                ..Default::default()
            },
        );
        if call == 0 {
            stream.push(done);
        } else {
            let producer = stream.producer();
            let release = Arc::clone(&self.release);
            tokio::spawn(async move {
                let _permit = release.acquire().await;
                producer.push(done);
            });
        }
        stream
    }
}

#[tokio::test(flavor = "current_thread")]
async fn dropping_an_old_stream_keeps_the_newer_streams_sink_and_admission() {
    let agent_dir = TempDir::new("generation-agent");
    let provider = Arc::new(GatedProvider::new());
    let mut registry = ProviderRegistry::default();
    let provider_box: Arc<dyn Provider> = provider.clone();
    registry.set_provider(provider_box);
    let agent = AgentBuilder::new()
        .with_registry(Arc::new(registry), scripted_model())
        .agent_dir(agent_dir.path())
        // A real (temp) session store: session-level `EntryAppended` events
        // are what proves the shared sink still belongs to the new stream.
        .session(SessionMode::Dir(agent_dir.path().join("sessions")))
        .no_compaction()
        .thinking_level(ThinkingLevel::Off)
        .build()
        .await
        .expect("gated agent builds");

    // A capacity-1 channel: the first run's intermediate events fill it, so
    // its terminal sends park on backpressure with the stream unpolled.
    agent.set_event_channel_capacity(1);
    let first = agent.prompt_stream("first");
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
    while provider.calls.load(std::sync::atomic::Ordering::SeqCst) == 0 {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the first gated run never reached the provider"
        );
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    while !agent.is_idle() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the first gated run never settled"
        );
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    // Let the first task release admission and park on the terminal send.
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    // The second stream gets a full-capacity channel (the first keeps its
    // capacity-1 backlog) and stays RUNNING on the provider gate.
    agent.set_event_channel_capacity(1024);
    let second = agent.prompt_stream("second");
    while provider.calls.load(std::sync::atomic::Ordering::SeqCst) < 2 {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the second gated run never reached the provider"
        );
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }

    // Drop the OLD stream while the NEW one is still running: the old task
    // wakes from its failed terminal send and its guard runs while the new
    // stream owns the slot.
    drop(first);
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    // The new stream's admission must still be held: a third stream is
    // rejected by the streaming slot, not by the session-busy check.
    let third: Vec<SdkEvent> = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        agent.prompt_stream("third").collect(),
    )
    .await
    .expect("the rejected third stream timed out");
    match third.first() {
        Some(SdkEvent::PromptRejected { message }) => assert!(
            message.contains("another stream is already streaming"),
            "the third stream must be rejected by the streaming ADMISSION \
             (a released one would hit the session-busy branch instead): {message}"
        ),
        other => panic!("the third stream must be rejected, got {other:?}"),
    }

    // Release the running second stream and collect it: its late session
    // events (the assistant EntryAppended) must still flow through the sink
    // the old stream's Drop previously cleared.
    provider.release_one();
    let second_events: Vec<SdkEvent> =
        tokio::time::timeout(std::time::Duration::from_secs(10), second.collect())
            .await
            .expect("the second stream timed out");
    assert!(
        second_events.iter().any(|event| matches!(
            event,
            SdkEvent::EntryAppended { entry }
                if entry
                    .get("message")
                    .and_then(|message| message.get("role"))
                    .and_then(|role| role.as_str())
                    == Some("assistant")
        )),
        "the running stream must keep receiving session-level events after \
         the old stream is dropped: {second_events:?}"
    );
    assert!(
        matches!(second_events.last(), Some(SdkEvent::AgentSettled { .. })),
        "the second stream must still deliver its terminal pair: {second_events:?}"
    );

    agent.shutdown().await.expect("shutdown");
}
