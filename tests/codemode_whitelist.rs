//! Tests for the codemode tool-allowlist enforcement (the P1 fix that makes
//! the `tools` whitelist a real boundary; upstream `host.ts` `start()` hands
//! the worker an already-filtered tool table, while pir keeps the full table
//! and enforces the list at three layers):
//!
//! - Layer 1 (thread bridge): the raw host bridge `__pir_callTool` refuses
//!   names outside the allowlist (the prelude's `tools` Proxy is only the
//!   first gate, never the sole one). [`pir::plugins::codemode::
//!   allowlist_rejects`] is the shared decision both enforcement points use.
//! - Layer 2 (worker-process parent): the parent's `call`-frame handler
//!   answers a frame naming a tool outside `Init.allowed_tools` with a
//!   tool-failure result instead of executing it (unit-tested through the
//!   shared decision helper; the full parent loop needs a real worker child
//!   spawned by `current_exe`, which integration tests cannot drive).
//! - Layer 3 (prelude closure): `__pir_callTool` / `__pir_hostOutput` /
//!   `__pir_modelCall` / `__pir_allowedTools` are captured into the
//!   prelude's closure and cleared from `globalThis`, so a script cannot
//!   call the bridge directly or widen the list (upstream keeps the bridge
//!   in the prelude's closure too — prelude-source.ts header).
//!
//! Plus the parent frame reader's cancel-safety: a dropped
//! [`read_event_line`] — the parent's `select!` loop re-arms it every
//! iteration while the abort/deadline branches can win — must not lose a
//! half-received frame (tokio's `read_line` is not cancel safe; the fix is
//! a persistent [`tokio::io::Lines`] adapter).
//!
//! New file owned by the whitelist fix; the `g_*.rs` / `suites/` files
//! belong to another workstream and are untouched.

use std::collections::BTreeMap;
use std::io::Write;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

use pir::agent::agent_loop::AgentEvent;
use pir::agent::tool_batch::ToolResult;
use pir::agent::tool_execution::{AgentTool, BoxFuture, ToolContext, ToolExposure, ToolRegistry};
use pir::agent_session::AgentSession;
use pir::agent_session::AgentSessionConfig;
use pir::ai::auth::ApiKeyAuth;
use pir::ai::event_stream::AssistantMessageEventStream;
use pir::ai::provider::{Provider, ProviderRegistry};
use pir::ai::types::*;
use pir::plugins::codemode::worker::{
    decode_event, encode_request, read_event_line, WorkerEvent, WorkerRequest,
    CODEMODE_WORKER_PROTOCOL_VERSION, WORKER_SUBCOMMAND,
};

// ---------------------------------------------------------------------------
// Thread-sandbox harness (same pattern as the codemode builtin suite)
// ---------------------------------------------------------------------------

fn fake_model() -> Model {
    Model {
        id: "fake-model".into(),
        name: "Fake model".into(),
        api: "fake-api".into(),
        provider: "fake".into(),
        base_url: "http://localhost".into(),
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

fn text(text: &str) -> ContentBlock {
    ContentBlock::Text(TextContent {
        text: text.into(),
        text_signature: None,
    })
}

fn assistant(content: Vec<ContentBlock>, stop_reason: StopReason) -> AssistantMessage {
    AssistantMessage {
        api: "fake".into(),
        provider: "fake".into(),
        model: "fake-model".into(),
        content,
        stop_reason,
        timestamp: 1,
        ..Default::default()
    }
}

fn tool_call(id: &str, name: &str, arguments: BTreeMap<String, serde_json::Value>) -> ContentBlock {
    ContentBlock::ToolCall(ToolCall {
        id: id.into(),
        name: name.into(),
        arguments,
        thought_signature: None,
        namespace: None,
        arguments_error: None,
    })
}

type Responder = Arc<dyn Fn(usize, &Context) -> AssistantMessage + Send + Sync>;

struct FakeProvider {
    calls: AtomicUsize,
    respond: Responder,
}

impl Provider for FakeProvider {
    fn id(&self) -> &str {
        "fake"
    }
    fn name(&self) -> &str {
        "Fake provider"
    }
    fn get_models(&self) -> Vec<Model> {
        vec![fake_model()]
    }
    fn api_key_auth(&self) -> Vec<ApiKeyAuth> {
        vec![ApiKeyAuth::new("fake key", vec!["FAKE_TEST_KEY".into()])]
    }
    fn stream(
        &self,
        _model: &Model,
        context: Context,
        _options: StreamOptions,
    ) -> AssistantMessageEventStream {
        let index = self.calls.fetch_add(1, Ordering::SeqCst);
        let message = (self.respond)(index, &context);
        let stream = AssistantMessageEventStream::new();
        stream.push(AssistantMessageEventStream::done_event(
            TerminalReason::Stop,
            message,
        ));
        stream
    }
}

fn fake_registry(respond: Responder) -> Arc<ProviderRegistry> {
    let mut registry = ProviderRegistry::default();
    registry.set_provider(Arc::new(FakeProvider {
        calls: AtomicUsize::new(0),
        respond,
    }));
    Arc::new(registry)
}

/// A trivial registry tool the script can call (`tools.echo({...})`).
struct EchoTool;

impl AgentTool for EchoTool {
    fn name(&self) -> &'static str {
        "echo"
    }
    fn description(&self) -> &'static str {
        "Echoes its text argument"
    }
    fn parameters(&self) -> Option<serde_json::Value> {
        Some(serde_json::json!({
            "type": "object",
            "properties": { "text": { "type": "string" } },
            "required": ["text"],
        }))
    }
    fn run(&self, args: &serde_json::Value, _ctx: &ToolContext) -> BoxFuture<'static, ToolResult> {
        let text = args
            .get("text")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_owned();
        Box::pin(async move { ToolResult::ok(text) })
    }
}

/// Per-test temp dir (session storage + cwd).
struct TempDir(std::path::PathBuf);

impl TempDir {
    fn new(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "pir-codemode-whitelist-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|duration| duration.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&path).expect("create temp dir");
        Self(path)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn codemode_tools() -> ToolRegistry {
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(EchoTool));
    tools
}

/// Activates `codemode` (plus the fixture's direct tools) through the
/// session's live `setActiveTools` cell. `codemode` is upstream `model-only`
/// with `defaultActive: false`, so this suite's behavior tests must activate
/// it; the live override keeps every registered fixture reachable.
struct ActivateCodemode {
    info: pir::plugins::PluginInfo,
    names: Vec<String>,
}

impl ActivateCodemode {
    fn new(tools: &ToolRegistry) -> Self {
        let mut names: Vec<String> = tools
            .iter()
            .filter(|tool| tool.exposure() == ToolExposure::Direct)
            .map(|tool| tool.name().to_string())
            .collect();
        names.push(pir::plugins::codemode::CODEMODE_TOOL_NAME.to_owned());
        Self {
            info: pir::plugins::PluginInfo::inline("activate-codemode"),
            names,
        }
    }
}

impl pir::plugins::AgentPlugin for ActivateCodemode {
    fn info(&self) -> &pir::plugins::PluginInfo {
        &self.info
    }

    fn on_context(&self, ctx: &pir::plugins::plugin::PluginContext) {
        if let Some(session) = ctx.session() {
            let names: Vec<Arc<str>> = self
                .names
                .iter()
                .map(|name| Arc::from(name.as_str()))
                .collect();
            session.set_active_tools(&names);
        }
    }
}

fn codemode_session(
    registry: Arc<ProviderRegistry>,
    tools: ToolRegistry,
    temp: &TempDir,
) -> Arc<AgentSession> {
    let activator = ActivateCodemode::new(&tools);
    Arc::new(AgentSession::new_with_plugins(
        AgentSessionConfig {
            provider_registry: registry,
            model: fake_model(),
            api_key: None,
            thinking: ThinkingLevel::Off,
            tools,
            session: None,
            compaction: None,
            max_turns: 4,
            system_prompt: Some("test system prompt".into()),
            cwd: Some(temp.0.clone()),
            event_stream: None,
            extensions: None,
            tool_selection: None,
        },
        vec![
            pir::plugins::codemode::new_codemode_plugin(),
            Arc::new(activator),
        ],
    ))
}

type EndLog = Arc<Mutex<Vec<Arc<pir::agent::agent_loop::LoopResult>>>>;

fn end_handler(log: EndLog) -> pir::agent_session::EventHandler {
    Arc::new(move |event: AgentEvent| {
        if let AgentEvent::AgentEnd { result } = event {
            log.lock().unwrap().push(result);
        }
    })
}

/// Run one prompt whose first provider response calls `codemode` with the
/// given script, returning the run's `LoopResult`.
async fn run_codemode_script(
    code: &str,
    requested_tools: Option<Vec<String>>,
) -> pir::agent::agent_loop::LoopResult {
    let temp = TempDir::new("script");
    let mut arguments = BTreeMap::new();
    arguments.insert(
        "code".to_owned(),
        serde_json::Value::String(code.to_owned()),
    );
    if let Some(tools) = requested_tools {
        arguments.insert(
            "tools".to_owned(),
            serde_json::Value::Array(tools.into_iter().map(serde_json::Value::String).collect()),
        );
    }
    let registry = fake_registry(Arc::new(move |index, _context| {
        if index == 0 {
            assistant(
                vec![tool_call("call-1", "codemode", arguments.clone())],
                StopReason::ToolUse,
            )
        } else {
            assistant(vec![text("done")], StopReason::Stop)
        }
    }));
    let agent = codemode_session(registry, codemode_tools(), &temp);
    let log: EndLog = Arc::new(Mutex::new(Vec::new()));
    agent
        .prompt("compose", end_handler(log.clone()))
        .await
        .expect("prompt");
    let last = log.lock().unwrap().pop().expect("agent end");
    Arc::unwrap_or_clone(last)
}

fn codemode_result(
    result: &pir::agent::agent_loop::LoopResult,
) -> &pir::ai::types::ToolResultMessage {
    result
        .tool_results
        .iter()
        .find(|message| message.tool_name == "codemode")
        .expect("codemode tool result message")
}

fn text_of(message: &pir::ai::types::ToolResultMessage) -> String {
    message
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text(text) => Some(text.text.clone()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

// ---------------------------------------------------------------------------
// Layer 3: the raw bridge and the prelude helpers are not script globals
// ---------------------------------------------------------------------------

#[tokio::test]
async fn prelude_hides_the_bridge_and_helpers_from_scripts() {
    let result = run_codemode_script(
        "return JSON.stringify([typeof __pir_callTool, typeof __pir_hostOutput, \
         typeof __pir_modelCall, typeof __pir_allowedTools, typeof __pir_output]);",
        None,
    )
    .await;
    let codemode = codemode_result(&result);
    assert!(
        text_of(codemode)
            .contains("[\"undefined\",\"undefined\",\"undefined\",\"undefined\",\"undefined\"]"),
        "the bridge must not be a script-reachable global: {}",
        text_of(codemode)
    );
}

// ---------------------------------------------------------------------------
// Layer 1 + the Proxy gate: the allowlist decides what a script may call
// ---------------------------------------------------------------------------

#[tokio::test]
async fn allowlisted_script_still_calls_allowed_tools() {
    let result = run_codemode_script(
        "return await tools.echo({ text: 'allowed' });",
        Some(vec!["echo".to_owned()]),
    )
    .await;
    let codemode = codemode_result(&result);
    assert!(
        text_of(codemode).contains("Script completed"),
        "content: {}",
        text_of(codemode)
    );
    assert!(
        text_of(codemode).contains("allowed"),
        "content: {}",
        text_of(codemode)
    );
}

#[tokio::test]
async fn without_an_allowlist_every_registered_tool_is_callable() {
    let result = run_codemode_script("return await tools.echo({ text: 'free' });", None).await;
    let codemode = codemode_result(&result);
    assert!(
        text_of(codemode).contains("Script completed"),
        "content: {}",
        text_of(codemode)
    );
    assert!(
        text_of(codemode).contains("free"),
        "content: {}",
        text_of(codemode)
    );
}

#[tokio::test]
async fn disallowed_tool_is_not_callable_through_the_proxy() {
    let result = run_codemode_script(
        "await tools.echo({ text: 'x' });",
        Some(vec!["other".to_owned()]),
    )
    .await;
    let codemode = codemode_result(&result);
    assert!(codemode.is_error, "content: {}", text_of(codemode));
    assert!(
        text_of(codemode).contains("Script failed"),
        "content: {}",
        text_of(codemode)
    );
}

/// The shared decision both enforcement points use: `None` (unconstrained)
/// allows everything, a list allows only its members, and the hidden
/// `models.` bridge names are always exempt (they are not part of the
/// callable-tools set — upstream: the models namespace is independent of the
/// `tools` allowlist).
#[test]
fn allowlist_decision_covers_all_three_layers() {
    let allowed = vec!["echo".to_owned(), "read".to_owned()];
    // Layer 1 (thread bridge) + layer 2 (worker parent) share this.
    assert!(pir::plugins::codemode::allowlist_rejects(
        Some(&allowed),
        "bash"
    ));
    assert!(pir::plugins::codemode::allowlist_rejects(
        Some(&allowed),
        "codemode"
    ));
    assert!(!pir::plugins::codemode::allowlist_rejects(
        Some(&allowed),
        "echo"
    ));
    assert!(!pir::plugins::codemode::allowlist_rejects(
        Some(&allowed),
        "read"
    ));
    // Unconstrained run: nothing is rejected here (codemode itself is hard-
    // rejected by the bridge/parent before this decision).
    assert!(!pir::plugins::codemode::allowlist_rejects(None, "bash"));
    assert!(!pir::plugins::codemode::allowlist_rejects(None, "echo"));
    // The models bridge names cross the same channel and stay exempt.
    assert!(!pir::plugins::codemode::allowlist_rejects(
        Some(&allowed),
        "models.generateImages"
    ));
    assert!(!pir::plugins::codemode::allowlist_rejects(
        Some(&allowed),
        "models.classify"
    ));
    assert!(pir::plugins::codemode::is_models_bridge_name(
        "models.generateImages"
    ));
    assert!(pir::plugins::codemode::is_models_bridge_name(
        "models.classify"
    ));
    assert!(!pir::plugins::codemode::is_models_bridge_name(
        "models.evil"
    ));
    // The rejected reply carries the standard tool-failure text.
    assert_eq!(
        pir::plugins::codemode::allowlist_error_text("bash"),
        "tool 'bash' is not in the allowed tools for this script"
    );
}

// ---------------------------------------------------------------------------
// Worker-process mode: the child's prelude filter + protocol-level routing
// (the real pir binary stands in for `<current_exe> __codemode-worker`)
// ---------------------------------------------------------------------------

struct WorkerProc {
    child: Child,
    stdin: std::process::ChildStdin,
    lines: mpsc::Receiver<String>,
}

fn spawn_worker() -> Option<WorkerProc> {
    let mut child = match Command::new(env!("CARGO_BIN_EXE_pir"))
        .arg(WORKER_SUBCOMMAND)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(child) => child,
        Err(error) => {
            eprintln!("SKIP: cannot spawn the pir binary for the whitelist e2e: {error}");
            return None;
        }
    };
    let stdout = child.stdout.take()?;
    let stdin = child.stdin.take()?;
    let (tx, rx) = mpsc::channel::<String>();
    std::thread::spawn(move || {
        use std::io::BufRead;
        let reader = std::io::BufReader::new(stdout);
        for line in reader.lines() {
            match line {
                Ok(line) => {
                    if tx.send(line).is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });
    Some(WorkerProc {
        child,
        stdin,
        lines: rx,
    })
}

fn send_request(proc: &mut WorkerProc, frame: &WorkerRequest) {
    let mut line = encode_request(frame).unwrap_or_else(|error| panic!("encode failed: {error}"));
    line.push('\n');
    proc.stdin
        .write_all(line.as_bytes())
        .unwrap_or_else(|error| panic!("failed to write a worker frame: {error}"));
    let _ = proc.stdin.flush();
}

fn recv_event(proc: &WorkerProc, timeout: Duration, context: &str) -> WorkerEvent {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        match proc.lines.recv_timeout(remaining) {
            Ok(line) => match decode_event(&line) {
                Ok(event) => return event,
                Err(_) => continue, // harness noise; keep waiting
            },
            Err(mpsc::RecvTimeoutError::Timeout) => {
                panic!("timed out waiting for a worker event ({context})");
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                panic!("worker stdout closed before the expected event ({context})");
            }
        }
    }
}

fn init_with_tools(code: &str, allowed_tools: Option<Vec<String>>) -> WorkerRequest {
    WorkerRequest::Init {
        v: CODEMODE_WORKER_PROTOCOL_VERSION,
        code: code.to_owned(),
        allowed_tools,
        max_output_chars: 1024 * 1024,
        max_output_items: 100_000,
        timeout_ms: 20_000,
        // No heap cap for the whitelist e2e (the frame's `None`).
        memory_bytes: None,
    }
}

fn consume_ready(proc: &WorkerProc) {
    match recv_event(proc, Duration::from_secs(15), "ready") {
        WorkerEvent::Ready { .. } => {}
        other => panic!("expected a ready frame, got {other:?}"),
    }
}

fn shutdown_worker(proc: WorkerProc) {
    let WorkerProc {
        mut child,
        stdin,
        lines,
    } = proc;
    drop(stdin);
    drop(lines);
    let _ = child.wait();
}

/// With an allowlist, the worker's prelude never exposes a tool outside the
/// list: a script calling one fails WITHOUT any `call` frame crossing the
/// protocol (the child-side mirror of the same list the parent enforces).
#[test]
fn worker_child_never_emits_a_call_frame_outside_the_allowlist() {
    let Some(mut proc) = spawn_worker() else {
        return;
    };
    send_request(
        &mut proc,
        &init_with_tools(
            "text('go'); return await tools.bash({});",
            Some(vec!["echo".to_owned()]),
        ),
    );
    consume_ready(&proc);
    loop {
        match recv_event(&proc, Duration::from_secs(20), "allowlisted done") {
            WorkerEvent::Call { name, .. } => {
                panic!("the worker must not emit a call frame outside the allowlist, got '{name}'")
            }
            WorkerEvent::Output { text, .. } => assert_eq!(text, "go"),
            WorkerEvent::Done { ok, .. } => {
                assert!(!ok, "the disallowed tool call must fail the script");
                break;
            }
            other => panic!("unexpected frame {other:?}"),
        }
    }
    shutdown_worker(proc);
}

/// Without an allowlist every tool name is routed: the `call` frame crosses
/// the protocol for an arbitrary name (the parent then executes or refuses
/// it — here it is answered with a tool failure and the script reports it).
#[test]
fn worker_child_routes_calls_for_every_tool_when_unconstrained() {
    let Some(mut proc) = spawn_worker() else {
        return;
    };
    send_request(
        &mut proc,
        &init_with_tools("return await tools.anything({});", None),
    );
    consume_ready(&proc);
    let call = match recv_event(&proc, Duration::from_secs(20), "unconstrained call") {
        WorkerEvent::Call { id, name, .. } => {
            assert_eq!(name, "anything");
            id
        }
        other => panic!("expected a call frame, got {other:?}"),
    };
    send_request(
        &mut proc,
        &WorkerRequest::Result {
            v: CODEMODE_WORKER_PROTOCOL_VERSION,
            id: call,
            ok: false,
            value: String::new(),
            json: false,
            error: Some("no such tool".to_owned()),
        },
    );
    match recv_event(&proc, Duration::from_secs(20), "unconstrained done") {
        WorkerEvent::Done { ok, error, .. } => {
            assert!(!ok, "the failing tool call must fail the script");
            assert_eq!(error.as_deref(), Some("no such tool"));
        }
        other => panic!("expected a done frame, got {other:?}"),
    }
    shutdown_worker(proc);
}

// ---------------------------------------------------------------------------
// Frame-reader cancel-safety: a dropped read must not lose a half frame
// ---------------------------------------------------------------------------

/// The parent's `select!` loop re-arms `read_event_line` every iteration
/// while the abort-poll / deadline branches can win the race. With the old
/// `read_line`-based reader, a read dropped mid-frame discarded the bytes it
/// had already consumed, so the retried read saw only the tail of the frame
/// and the graceful abort surfaced as a bogus "invalid event frame" protocol
/// failure. The persistent `Lines` adapter keeps the partial line buffered,
/// so the retried read resumes the SAME frame.
#[tokio::test]
async fn read_event_line_resumes_a_half_frame_after_a_dropped_read() {
    let (mut client, server) = tokio::io::duplex(64 * 1024);
    let reader = tokio::io::BufReader::new(server);
    // One persistent adapter for the whole run, like the parent loop owns.
    let mut lines = reader.lines();

    // Half of the first frame arrives...
    client.write_all(b"{\"type\":\"read").await.unwrap();
    // ...and a competing branch wins the race while the read is parked.
    let raced = tokio::time::timeout(Duration::from_millis(100), read_event_line(&mut lines)).await;
    assert!(
        raced.is_err(),
        "the read must still be parked (half a frame)"
    );

    // The rest arrives; the retried read must see the WHOLE first line.
    client
        .write_all(b"y\",\"v\":1}\n{\"type\":\"crash\",\"v\":1}\n")
        .await
        .unwrap();
    let first = read_event_line(&mut lines)
        .await
        .unwrap()
        .expect("the half frame must not be lost");
    assert_eq!(first, "{\"type\":\"ready\",\"v\":1}", "full first frame");
    let second = read_event_line(&mut lines)
        .await
        .unwrap()
        .expect("the next frame must follow intact");
    assert_eq!(second, "{\"type\":\"crash\",\"v\":1}", "second frame");
}
