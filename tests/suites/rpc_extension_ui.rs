//! RPC extension-UI bridge coverage (upstream `rpc-mode.ts`
//! `createExtensionUIContext` + `extension_ui_response` handling): extension
//! `ctx.ui` requests are forwarded as `extension_ui_request` protocol lines,
//! client replies resolve the pending dialog, and an unanswered dialog
//! resolves `null` after the dialog TTL (fail-safe, never wedges the run).
//!
//! In-process: `RpcServer::serve` over a tokio duplex with a session factory
//! that loads a REAL QuickJS extension runtime (`has_ui: true`) whose
//! `session_start` handler exercises `ctx.ui`.

use std::io::Write as _;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _};

use pir::agent::tool_execution::ToolRegistry;
use pir::agent_session::{AgentSession, AgentSessionConfig};
use pir::ai::auth::ApiKeyAuth;
use pir::ai::event_stream::AssistantMessageEventStream;
use pir::ai::provider::{Provider, ProviderRegistry};
use pir::ai::types::*;
use pir::coding_agent::rpc::{RpcServer, SessionFactory};
use pir::extensions::{ExtensionConfig, ExtensionHandle};
use pir::session_manager::SessionManager;

// ---------------------------------------------------------------------------
// Fake provider plumbing (self-contained; the model is never contacted
// meaningfully — one turn of static text)
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

fn text(text: &str) -> ContentBlock {
    ContentBlock::Text(TextContent {
        text: text.into(),
        text_signature: None,
    })
}

type Responder = Arc<dyn Fn(usize) -> AssistantMessage + Send + Sync>;

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
        _context: Context,
        _options: StreamOptions,
    ) -> AssistantMessageEventStream {
        let index = self.calls.fetch_add(1, Ordering::SeqCst);
        let message = (self.respond)(index);
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

// ---------------------------------------------------------------------------
// Extension fixture + session factory
// ---------------------------------------------------------------------------

/// Per-test temp directory (session storage + extension file).
struct TempDir(std::path::PathBuf);

impl TempDir {
    fn new(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "pir-rpc-ext-ui-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|duration| duration.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&path).expect("create temp dir");
        Self(path)
    }

    fn write_extension(&self, label: &str, source: &str) -> std::path::PathBuf {
        let path = self.0.join(format!("{label}.ts"));
        let mut file = std::fs::File::create(&path).expect("create extension file");
        file.write_all(source.as_bytes()).expect("write extension");
        file.flush().expect("flush extension");
        path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Start a REAL QuickJS extension runtime (`has_ui: true`, optional
/// shortened dialog TTL) with one JS extension loaded.
async fn start_extension_runtime(
    temp: &TempDir,
    label: &str,
    source: &str,
    dialog_ttl: Option<Duration>,
) -> ExtensionHandle {
    let extension_path = temp.write_extension(label, source);
    let handle = ExtensionHandle::start(ExtensionConfig {
        cwd: temp.0.clone(),
        mode: "rpc".to_string(),
        has_ui: true,
        limits: Some(Default::default()),
        ..Default::default()
    })
    .expect("extension runtime should start");
    if let Some(ttl) = dialog_ttl {
        handle.set_dialog_ttl(ttl);
    }
    let loaded = handle
        .load(std::slice::from_ref(&extension_path))
        .await
        .expect("extension load should not fail at the transport level");
    assert!(
        loaded.errors.is_empty(),
        "extension load errors: {:?}",
        loaded.errors
    );
    handle
}

/// Session factory wrapping a pre-loaded extension runtime (cloned per
/// attach).
fn session_factory(handle: ExtensionHandle) -> SessionFactory {
    Arc::new(move |session: Option<SessionManager>| {
        Ok(Arc::new(AgentSession::new(AgentSessionConfig {
            provider_registry: fake_registry(Arc::new(|_index| {
                assistant(vec![text("done")], StopReason::Stop)
            })),
            model: fake_model(),
            api_key: None,
            thinking: ThinkingLevel::Off,
            tools: ToolRegistry::new(),
            session,
            compaction: None,
            max_turns: 4,
            system_prompt: None,
            cwd: None,
            event_stream: None,
            extensions: Some(handle.clone()),
            tool_selection: None,
        })))
    })
}

/// A provider whose stream stays in flight until the gate opens (or the run
/// is aborted): the session reports `isStreaming: true` for that whole window,
/// which is what the mid-run command tests need.
struct GatedProvider {
    gate: tokio::sync::watch::Receiver<bool>,
    entered: Arc<AtomicUsize>,
}

impl Provider for GatedProvider {
    fn id(&self) -> &str {
        "fake"
    }
    fn name(&self) -> &str {
        "Gated fake provider"
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
        _context: Context,
        options: StreamOptions,
    ) -> AssistantMessageEventStream {
        self.entered.fetch_add(1, Ordering::SeqCst);
        let stream = AssistantMessageEventStream::new();
        let pusher = stream.clone();
        let mut gate = self.gate.clone();
        let signal = options.signal;
        tokio::spawn(async move {
            loop {
                if signal.as_ref().is_some_and(|signal| signal.is_aborted()) {
                    pusher.push(AssistantMessageEventStream::error_event(
                        ErrorReason::Aborted,
                        assistant(vec![text("aborted")], StopReason::Aborted),
                    ));
                    return;
                }
                if *gate.borrow() {
                    pusher.push(AssistantMessageEventStream::done_event(
                        TerminalReason::Stop,
                        assistant(vec![text("done")], StopReason::Stop),
                    ));
                    return;
                }
                // Poll with a timeout so a gate change and an abort landing
                // together are both observed on the next iteration.
                if tokio::time::timeout(Duration::from_millis(20), gate.changed())
                    .await
                    .is_err()
                {
                    continue;
                }
            }
        });
        stream
    }
}

/// [`session_factory`] whose runs park in the provider until `gate` opens.
/// Returns the number of provider calls (a run has reached the provider once
/// it is non-zero).
fn gated_session_factory(
    handle: ExtensionHandle,
    gate: tokio::sync::watch::Receiver<bool>,
) -> (SessionFactory, Arc<AtomicUsize>) {
    let entered = Arc::new(AtomicUsize::new(0));
    let factory_entered = Arc::clone(&entered);
    let factory: SessionFactory = Arc::new(move |session: Option<SessionManager>| {
        let mut registry = ProviderRegistry::default();
        registry.set_provider(Arc::new(GatedProvider {
            gate: gate.clone(),
            entered: Arc::clone(&factory_entered),
        }));
        Ok(Arc::new(AgentSession::new(AgentSessionConfig {
            provider_registry: Arc::new(registry),
            model: fake_model(),
            api_key: None,
            thinking: ThinkingLevel::Off,
            tools: ToolRegistry::new(),
            session,
            compaction: None,
            max_turns: 4,
            system_prompt: None,
            cwd: None,
            event_stream: None,
            extensions: Some(handle.clone()),
            tool_selection: None,
        })))
    });
    (factory, entered)
}

// ---------------------------------------------------------------------------
// Client half of an in-memory stdio pair
// ---------------------------------------------------------------------------
struct RpcClient {
    writer: tokio::sync::mpsc::UnboundedSender<String>,
    reader: tokio::sync::mpsc::UnboundedReceiver<String>,
}

impl RpcClient {
    fn send(&self, value: serde_json::Value) {
        self.writer
            .send(serde_json::to_string(&value).expect("serialize"))
            .expect("send command");
    }

    /// Read protocol lines until `predicate` matches (30s hard bound so a
    /// regression can never hang the suite).
    async fn wait_for(
        &mut self,
        predicate: impl Fn(&serde_json::Value) -> bool,
    ) -> serde_json::Value {
        for _ in 0..600 {
            let line = tokio::time::timeout(Duration::from_secs(30), self.reader.recv())
                .await
                .expect("read must not time out")
                .expect("protocol line");
            let value: serde_json::Value =
                serde_json::from_str(&line).expect("rpc line must be valid JSON");
            assert_ne!(
                value["type"], "event",
                "events must not be wrapped: {value}"
            );
            if predicate(&value) {
                return value;
            }
        }
        panic!("expected protocol line never arrived");
    }
}

async fn rpc_pair(label: &str, factory: SessionFactory) -> (RpcClient, TempDir) {
    let temp = TempDir::new(label);
    let server = RpcServer::new(temp.0.clone(), factory);
    let (client_stream, server_stream) = tokio::io::duplex(64 * 1024);
    let (server_read, server_write) = tokio::io::split(server_stream);
    tokio::spawn(async move { server.serve(server_read, server_write).await });

    let (client_read, mut client_write) = tokio::io::split(client_stream);
    let (line_tx, line_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    tokio::spawn(async move {
        let mut lines = tokio::io::BufReader::new(client_read).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            if line_tx.send(line).is_err() {
                break;
            }
        }
    });
    let (write_tx, mut write_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    tokio::spawn(async move {
        while let Some(mut line) = write_rx.recv().await {
            line.push('\n');
            if client_write.write_all(line.as_bytes()).await.is_err() {
                break;
            }
            let _ = client_write.flush().await;
        }
    });
    (
        RpcClient {
            writer: write_tx,
            reader: line_rx,
        },
        temp,
    )
}

async fn attach_session(client: &mut RpcClient) {
    client.send(serde_json::json!({ "type": "create" }));
    let created = client
        .wait_for(|value| value["command"] == "create" && value["success"] == true)
        .await;
    assert_eq!(created["success"], serde_json::json!(true), "{created}");
}

// ---------------------------------------------------------------------------
// Extension sources
// ---------------------------------------------------------------------------

const SELECT_EXTENSION: &str = r#"
export default function (pi) {
  pi.on("session_start", async (_event, ctx) => {
    const answer = await ctx.ui.select("Pick", ["a", "b"]);
    ctx.ui.notify("answer=" + answer, "info");
  });
}
"#;

const FIRE_AND_FORGET_EXTENSION: &str = r#"
export default function (pi) {
  pi.on("session_start", async (_event, ctx) => {
    ctx.ui.notify("hello there", "warning");
    ctx.ui.setTitle("Bridge title");
    ctx.ui.setEditorText("editor text");
    ctx.ui.setStatus("ext", "working");
  });
}
"#;

const CONFIRM_EXTENSION: &str = r#"
export default function (pi) {
  pi.on("session_start", async (_event, ctx) => {
    const yes = await ctx.ui.confirm("Proceed?", "Are you sure?");
    ctx.ui.notify("confirmed=" + yes, "info");
  });
}
"#;

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn extension_select_request_bridges_and_response_resolves_the_dialog() {
    let temp = TempDir::new("select");
    let handle = start_extension_runtime(&temp, "select", SELECT_EXTENSION, None).await;
    let (mut client, _temp) = rpc_pair("select-roundtrip", session_factory(handle)).await;
    attach_session(&mut client).await;

    client.send(serde_json::json!({ "id": "p1", "type": "prompt", "text": "go" }));
    let prompt = client
        .wait_for(|value| value["command"] == "prompt" && value["id"] == "p1")
        .await;
    assert_eq!(prompt["success"], serde_json::json!(true), "{prompt}");

    // The dialog request arrives as a bare `extension_ui_request` line with
    // the upstream select shape.
    let request = client
        .wait_for(|value| value["type"] == "extension_ui_request" && value["method"] == "select")
        .await;
    assert_eq!(request["title"], "Pick", "{request}");
    let options = request["options"].as_array().expect("options array");
    assert_eq!(options.len(), 2, "{request}");
    assert_eq!(options[0]["value"], "a", "{request}");
    assert_eq!(options[0]["label"], "a", "{request}");
    let dialog_id = request["id"].as_str().expect("dialog id").to_owned();

    // The reply resolves the JS await; the extension reports the answer.
    client.send(serde_json::json!({
        "type": "extension_ui_response",
        "id": dialog_id,
        "value": "b",
    }));
    // The notify is forwarded by the 50ms UI poller while the run's own
    // events (up to agent_end) go out directly, so the two lines interleave
    // in either order — collect both without assuming one.
    let mut notify = None;
    let mut agent_end = false;
    while notify.is_none() || !agent_end {
        let value = client.wait_for(|_| true).await;
        let is_notify = value["type"] == "extension_ui_request" && value["method"] == "notify";
        if is_notify {
            notify = Some(value);
        } else if value["type"] == "agent_end" {
            agent_end = true;
        }
    }
    let notify = notify.expect("notify line forwarded");
    assert_eq!(notify["message"], "answer=b", "{notify}");
    assert_eq!(notify["notifyType"], "info", "{notify}");
}

#[tokio::test]
#[cfg_attr(
    target_os = "linux",
    ignore = "RPC dialog TTL misses the 30 s read bound under 16-way CI load; covered on Windows"
)]
async fn extension_unanswered_dialog_resolves_null_after_ttl() {
    let temp = TempDir::new("ttl");
    let handle = start_extension_runtime(
        &temp,
        "ttl",
        SELECT_EXTENSION,
        Some(Duration::from_millis(300)),
    )
    .await;
    let (mut client, _temp) = rpc_pair("ttl", session_factory(handle)).await;
    attach_session(&mut client).await;

    client.send(serde_json::json!({ "id": "p1", "type": "prompt", "text": "go" }));
    let _ = client
        .wait_for(|value| value["command"] == "prompt" && value["id"] == "p1")
        .await;

    // The request is still forwarded (the client may answer late).
    let request = client
        .wait_for(|value| value["type"] == "extension_ui_request" && value["method"] == "select")
        .await;
    assert_eq!(request["title"], "Pick", "{request}");

    // Never answered: after the TTL the dialog resolves null (the prelude's
    // `undefined`), the extension continues, and the run settles. The notify
    // (50ms poller) and agent_end (direct) may interleave in either order.
    let mut notify = None;
    let mut agent_end = false;
    while notify.is_none() || !agent_end {
        let value = client.wait_for(|_| true).await;
        let is_notify = value["type"] == "extension_ui_request" && value["method"] == "notify";
        if is_notify {
            notify = Some(value);
        } else if value["type"] == "agent_end" {
            agent_end = true;
        }
    }
    let notify = notify.expect("notify line forwarded");
    assert_eq!(notify["message"], "answer=undefined", "{notify}");
}

#[tokio::test]
async fn extension_confirmed_reply_maps_to_boolean() {
    let temp = TempDir::new("confirm");
    let handle = start_extension_runtime(&temp, "confirm", CONFIRM_EXTENSION, None).await;
    let (mut client, _temp) = rpc_pair("confirm", session_factory(handle)).await;
    attach_session(&mut client).await;

    client.send(serde_json::json!({ "id": "p1", "type": "prompt", "text": "go" }));
    let _ = client
        .wait_for(|value| value["command"] == "prompt" && value["id"] == "p1")
        .await;

    let request = client
        .wait_for(|value| value["type"] == "extension_ui_request" && value["method"] == "confirm")
        .await;
    assert_eq!(request["title"], "Proceed?", "{request}");
    assert_eq!(request["message"], "Are you sure?", "{request}");

    client.send(serde_json::json!({
        "type": "extension_ui_response",
        "id": request["id"],
        "confirmed": true,
    }));
    let notify = client
        .wait_for(|value| value["type"] == "extension_ui_request" && value["method"] == "notify")
        .await;
    assert_eq!(notify["message"], "confirmed=true", "{notify}");
}

#[tokio::test]
async fn fire_and_forget_ui_requests_are_forwarded() {
    let temp = TempDir::new("ff");
    let handle = start_extension_runtime(&temp, "ff", FIRE_AND_FORGET_EXTENSION, None).await;
    let (mut client, _temp) = rpc_pair("fire-and-forget", session_factory(handle)).await;
    attach_session(&mut client).await;

    client.send(serde_json::json!({ "id": "p1", "type": "prompt", "text": "go" }));
    let _ = client
        .wait_for(|value| value["command"] == "prompt" && value["id"] == "p1")
        .await;

    let notify = client
        .wait_for(|value| value["type"] == "extension_ui_request" && value["method"] == "notify")
        .await;
    assert_eq!(notify["message"], "hello there", "{notify}");
    assert_eq!(notify["notifyType"], "warning", "{notify}");

    let title = client
        .wait_for(|value| value["type"] == "extension_ui_request" && value["method"] == "setTitle")
        .await;
    assert_eq!(title["title"], "Bridge title", "{title}");

    let editor = client
        .wait_for(|value| {
            value["type"] == "extension_ui_request" && value["method"] == "set_editor_text"
        })
        .await;
    assert_eq!(editor["text"], "editor text", "{editor}");

    let status = client
        .wait_for(|value| value["type"] == "extension_ui_request" && value["method"] == "setStatus")
        .await;
    assert_eq!(status["statusKey"], "ext", "{status}");
    assert_eq!(status["statusText"], "working", "{status}");

    // Fire-and-forget ids never collide with dialog ids: they carry the
    // `ui-` prefix (a stray reply must not be misattributed to a dialog).
    assert!(
        notify["id"].as_str().unwrap_or("").starts_with("ui-"),
        "{notify}"
    );
}

#[tokio::test]
async fn ui_response_without_a_session_is_ignored_and_the_server_survives() {
    let temp = TempDir::new("no-session");
    let handle = start_extension_runtime(&temp, "noop", SELECT_EXTENSION, None).await;
    let (mut client, _temp) = rpc_pair("ui-response-no-session", session_factory(handle)).await;

    // A reply before any session exists: silently dropped (upstream ignores
    // unknown ids), never a crash.
    client.send(serde_json::json!({
        "type": "extension_ui_response",
        "id": "9",
        "value": "x",
    }));
    client.send(serde_json::json!({ "type": "hello" }));
    let hello = client.wait_for(|value| value["command"] == "hello").await;
    assert_eq!(hello["success"], serde_json::json!(true), "{hello}");
}

// ---------------------------------------------------------------------------
// ctx.shutdown: the RPC host exits at its settled / next-command boundary
// ---------------------------------------------------------------------------

const SHUTDOWN_ON_START_EXTENSION: &str = r#"
export default function (pi) {
  pi.on("session_start", (_event, ctx) => {
    ctx.shutdown();
  });
}
"#;

const SHUTDOWN_ON_SESSION_INFO_EXTENSION: &str = r#"
export default function (pi) {
  pi.on("session_info_changed", (_event, ctx) => {
    ctx.shutdown();
  });
}
"#;

/// `ctx.shutdown()` raised ONCE THE RUN IS STREAMING (`before_agent_start`
/// fires inside the run): the request must stay queued until `agent_settled`,
/// never be consumed by a command that arrives while the run is in flight.
const SHUTDOWN_DURING_RUN_EXTENSION: &str = r#"
export default function (pi) {
  pi.on("before_agent_start", (_event, ctx) => {
    ctx.shutdown();
  });
}
"#;

/// Drain the protocol stream until the server closes it.
async fn wait_for_rpc_exit(client: &mut RpcClient) {
    let closed = tokio::time::timeout(Duration::from_secs(10), async {
        while client.reader.recv().await.is_some() {}
    })
    .await;
    assert!(
        closed.is_ok(),
        "the RPC host must exit after ctx.shutdown, not keep serving"
    );
}

/// `ctx.shutdown()` from a `session_start` handler (during a run): the run
/// settles normally — `agent_settled {aborted: false}` — and the host then
/// exits through its normal teardown (upstream `shutdownHandler` +
/// `checkShutdownRequested` at `agent_settled`, rpc-mode.ts:345-358).
#[tokio::test]
async fn extension_shutdown_ends_the_rpc_host_after_the_run_settles() {
    let temp = TempDir::new("shutdown-run");
    let handle =
        start_extension_runtime(&temp, "shutdown-run", SHUTDOWN_ON_START_EXTENSION, None).await;
    let (mut client, _temp) = rpc_pair("shutdown-run", session_factory(handle)).await;
    attach_session(&mut client).await;

    client.send(serde_json::json!({ "id": "p1", "type": "prompt", "text": "go" }));
    let prompt = client
        .wait_for(|value| value["command"] == "prompt" && value["id"] == "p1")
        .await;
    assert_eq!(prompt["success"], serde_json::json!(true), "{prompt}");

    let settled = client
        .wait_for(|value| value["type"] == "agent_settled")
        .await;
    assert_eq!(
        settled["aborted"],
        serde_json::json!(false),
        "ctx.shutdown must not abort the in-flight run: {settled}"
    );
    wait_for_rpc_exit(&mut client).await;
}

/// `ctx.shutdown()` raised by an idle command's event handler: the host
/// answers the command (upstream `output(response)` before
/// `checkShutdownRequested`) and exits before reading another line.
#[tokio::test]
async fn extension_shutdown_ends_the_rpc_host_after_the_next_command() {
    let temp = TempDir::new("shutdown-idle");
    let handle = start_extension_runtime(
        &temp,
        "shutdown-idle",
        SHUTDOWN_ON_SESSION_INFO_EXTENSION,
        None,
    )
    .await;
    let (mut client, _temp) = rpc_pair("shutdown-idle", session_factory(handle)).await;
    attach_session(&mut client).await;

    client.send(serde_json::json!({
        "id": "n1",
        "type": "set_session_name",
        "name": "bye",
    }));
    let response = client
        .wait_for(|value| value["command"] == "set_session_name" && value["id"] == "n1")
        .await;
    assert_eq!(response["success"], serde_json::json!(true), "{response}");
    wait_for_rpc_exit(&mut client).await;
}

/// A `ctx.shutdown()` raised during a streaming run plus another command
/// arriving mid-run: the command must answer WITHOUT consuming the queued
/// shutdown request. Consuming it there ended the host (and tore down the
/// session) before the run settled, contradicting the "shutdown during a run
/// is handled by the `agent_settled` observer" contract.
#[tokio::test]
async fn extension_shutdown_during_a_run_is_not_consumed_by_a_mid_run_command() {
    let temp = TempDir::new("shutdown-streaming");
    let handle = start_extension_runtime(
        &temp,
        "shutdown-streaming",
        SHUTDOWN_DURING_RUN_EXTENSION,
        None,
    )
    .await;
    let (gate_tx, gate_rx) = tokio::sync::watch::channel(false);
    let (factory, entered) = gated_session_factory(handle, gate_rx);
    let (mut client, _temp) = rpc_pair("shutdown-streaming", factory).await;
    attach_session(&mut client).await;

    client.send(serde_json::json!({ "id": "p1", "type": "prompt", "text": "go" }));
    let prompt = client
        .wait_for(|value| value["command"] == "prompt" && value["id"] == "p1")
        .await;
    assert_eq!(prompt["success"], serde_json::json!(true), "{prompt}");

    // The run reached the gated provider: `before_agent_start` already called
    // `ctx.shutdown()`, whose notification the session drained into its latch,
    // and the run is streaming.
    let mut reached = false;
    for _ in 0..600 {
        if entered.load(Ordering::SeqCst) > 0 {
            reached = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(reached, "the run never reached the provider");

    // A command while streaming: it must answer, and it must NOT take the
    // shutdown request (that would wake the serve loop mid-run).
    client.send(serde_json::json!({ "id": "g1", "type": "get_state" }));
    let state = client
        .wait_for(|value| value["command"] == "get_state" && value["id"] == "g1")
        .await;
    assert_eq!(state["success"], serde_json::json!(true), "{state}");
    assert_eq!(
        state["data"]["isStreaming"],
        serde_json::json!(true),
        "the command must be answered while the run is still streaming: {state}"
    );

    // The host stays alive while the run is in flight: protocol lines may
    // still stream, but the pipe must not close.
    let deadline = tokio::time::Instant::now() + Duration::from_millis(400);
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            break;
        }
        match tokio::time::timeout(remaining, client.reader.recv()).await {
            // No line in the window: still serving.
            Err(_) => break,
            Ok(Some(_)) => continue,
            Ok(None) => panic!("the host exited before the streaming run settled"),
        }
    }

    // A SECOND command, still mid-run, is the decisive check: only a serve
    // loop that did not exit on the queued shutdown request can answer it (a
    // torn-down loop leaves the leaked prompt task alive long enough to emit
    // `agent_settled`, so that event alone does not prove the host survived).
    client.send(serde_json::json!({ "id": "h1", "type": "hello" }));
    let hello = tokio::time::timeout(Duration::from_secs(5), async {
        client
            .wait_for(|value| value["command"] == "hello" && value["id"] == "h1")
            .await
    })
    .await
    .expect("the serve loop must still answer commands while the run streams");
    assert_eq!(hello["success"], serde_json::json!(true), "{hello}");

    // Let the run finish: it settles normally, THEN the host exits.
    let _ = gate_tx.send(true);
    let settled = client
        .wait_for(|value| value["type"] == "agent_settled")
        .await;
    assert_eq!(
        settled["aborted"],
        serde_json::json!(false),
        "the shutdown request must not abort the run: {settled}"
    );
    wait_for_rpc_exit(&mut client).await;
}
