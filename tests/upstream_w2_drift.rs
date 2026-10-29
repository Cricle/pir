//! W2 upstream-alignment drift fixes (UPDATING.md ledger): the breaking / API-shape
//! changes of the pi v1.2.0 window, each with the upstream commit it follows.
//!
//! - **D22** `a447ba11f` (PR #10739, closes #10267/#5581): `before_agent_start`
//!   is a union on `messageType` ("user" carries `prompt`/`images`, "custom"
//!   the starting `message` and no `prompt`); a run started by
//!   `pi.sendMessage(..., { triggerTurn: true })` goes through the same setup as
//!   a prompt; the session counts as busy while a run starts (`ctx.isIdle()` is
//!   false, `sendMessage` joins the starting run, `sendUserMessage` requires
//!   `deliverAs`, an abort requested meanwhile stops the run); and the system
//!   prompt of a run no longer changes after its first tool call.
//! - **D23** `24382bedd` (PR #10723): tool definitions carry `reachesTools`,
//!   tool namespaces carry `aliases`.
//! - **D24** `96abe9a5a` (PR #10751, #9880): the generated configuration
//!   schemas are published at `https://pi.dev/schemas/*`; a theme validates the
//!   same way when loaded at startup and on reload; `showHardwareCursor` uses
//!   only the terminal cursor instead of also drawing Pi's reverse-video one.
//!
//! Standalone binary (no `g_*` suite registration): tests/ and tests/suites/
//! are a concurrent-edit conflict zone.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use pir::agent::tool_batch::ToolResult;
use pir::agent::tool_execution::{AgentTool, BoxFuture, ToolContext, ToolReach, ToolRegistry};
use pir::agent_session::{AgentSession, AgentSessionConfig, CustomRunMessage};
use pir::ai::auth::ApiKeyAuth;
use pir::ai::event_stream::AssistantMessageEventStream;
use pir::ai::provider::{Provider, ProviderRegistry};
use pir::ai::types::{
    AssistantMessage, ContentBlock, Context, Message, Model, ModelCost, ModelCostRates, StopReason,
    StreamOptions, TerminalReason, TextContent, ThinkingLevel, ToolCall, Usage,
};
use pir::extensions::{ExtensionConfig, ExtensionHandle};
use serde_json::{json, Value};

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

struct TempDir(PathBuf);

impl TempDir {
    fn new(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "pir-upstream-w2-{label}-{}-{}",
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
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

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
        context_window: 8192,
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

fn test_config_at(cwd: &Path, label: &str) -> ExtensionConfig {
    ExtensionConfig {
        cwd: cwd.to_path_buf(),
        mode: "headless".to_string(),
        has_ui: false,
        model: Some("fake-model".to_string()),
        session_id: Some(format!("w2-{label}")),
        system_prompt: "base system prompt".to_string(),
        is_idle: true,
        limits: Some(Default::default()),
    }
}

async fn load_runtime(dir: &TempDir, label: &str, source: &str) -> ExtensionHandle {
    let path = dir.0.join("ext.ts");
    std::fs::write(&path, source).expect("write fixture");
    let runtime = ExtensionHandle::start(test_config_at(&dir.0, label)).expect("runtime");
    let result = runtime
        .load(std::slice::from_ref(&path))
        .await
        .expect("load");
    assert!(result.errors.is_empty(), "load errors: {:?}", result.errors);
    runtime
}

async fn probe(runtime: &ExtensionHandle, code: &str) -> Value {
    runtime
        .eval_json(&format!("JSON.stringify({code})"))
        .await
        .expect("probe eval")
}

/// One captured provider request.
#[derive(Clone)]
struct CapturedRequest {
    system_prompt: Option<String>,
    messages: Vec<Message>,
    tool_names: Vec<String>,
}

impl CapturedRequest {
    fn text(&self) -> String {
        serde_json::to_string(&self.messages).unwrap_or_default()
    }
    fn contains(&self, needle: &str) -> bool {
        self.text().contains(needle)
    }
}

type Script = Arc<dyn Fn(usize, &Context) -> AssistantMessage + Send + Sync>;

/// A provider answering every stream with the scripted message for the call
/// index, recording what the run asked for.
struct ScriptedProvider {
    calls: AtomicUsize,
    script: Script,
    captured: Arc<Mutex<Vec<CapturedRequest>>>,
}

impl Provider for ScriptedProvider {
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
        self.captured
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(CapturedRequest {
                system_prompt: context.system_prompt.clone(),
                messages: context.messages.clone(),
                tool_names: context
                    .tools
                    .as_ref()
                    .map(|tools| tools.iter().map(|tool| tool.name.clone()).collect())
                    .unwrap_or_default(),
            });
        let message = (self.script)(index, &context);
        let stream = AssistantMessageEventStream::new();
        stream.push(AssistantMessageEventStream::done_event(
            TerminalReason::Stop,
            message,
        ));
        stream
    }
}

fn text_block(text: &str) -> ContentBlock {
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
        usage: Usage::default(),
        ..AssistantMessage::default()
    }
}

fn tool_call(name: &str) -> ContentBlock {
    ContentBlock::ToolCall(ToolCall {
        id: "call-1".into(),
        name: name.into(),
        arguments: std::collections::BTreeMap::new(),
        thought_signature: None,
        namespace: None,
        arguments_error: None,
    })
}

/// A tool that always succeeds (it exists so a run's second request can be a
/// post-tool-call turn).
struct NoopTool;

impl AgentTool for NoopTool {
    fn name(&self) -> &'static str {
        "noop"
    }
    fn description(&self) -> &'static str {
        "Does nothing"
    }
    fn parameters(&self) -> Option<Value> {
        Some(json!({"type": "object"}))
    }
    fn run(&self, _args: &Value, _ctx: &ToolContext) -> BoxFuture<'static, ToolResult> {
        Box::pin(async { ToolResult::ok("ok") })
    }
}

struct SessionFixture {
    session: Arc<AgentSession>,
    captured: Arc<Mutex<Vec<CapturedRequest>>>,
}

fn session_with_extension(
    cwd: &Path,
    runtime: ExtensionHandle,
    script: Script,
    with_tool: bool,
) -> SessionFixture {
    session_with_extension_options(cwd, runtime, script, with_tool, false)
}

/// Like [`session_with_extension`], with a session store option. Upstream's
/// harness always has a session manager, so contracts about appended custom
/// messages (upstream `_appendCustomMessage` → the message list of the next
/// request) need the persisted variant; the ephemeral one keeps the transcript
/// in memory only.
fn session_with_extension_options(
    cwd: &Path,
    runtime: ExtensionHandle,
    script: Script,
    with_tool: bool,
    persisted: bool,
) -> SessionFixture {
    let captured: Arc<Mutex<Vec<CapturedRequest>>> = Arc::new(Mutex::new(Vec::new()));
    let mut registry = ProviderRegistry::default();
    let provider = Arc::new(ScriptedProvider {
        calls: AtomicUsize::new(0),
        script,
        captured: Arc::clone(&captured),
    });
    registry.set_provider(provider.clone());
    let mut tools = ToolRegistry::new();
    if with_tool {
        tools.register(Arc::new(NoopTool));
    }
    let session = if persisted {
        Some(
            pir::session_manager::SessionManager::in_memory(&cwd.display().to_string(), None)
                .expect("in-memory session"),
        )
    } else {
        None
    };
    let session = Arc::new(AgentSession::new(AgentSessionConfig {
        provider_registry: Arc::new(registry),
        model: fake_model(),
        api_key: None,
        thinking: ThinkingLevel::Off,
        tools,
        session,
        compaction: None,
        max_turns: 6,
        system_prompt: Some("base system prompt".into()),
        cwd: Some(cwd.to_path_buf()),
        event_stream: None,
        extensions: Some(runtime),
        tool_selection: None,
    }));
    SessionFixture { session, captured }
}

fn captured(fixture: &SessionFixture) -> Vec<CapturedRequest> {
    fixture
        .captured
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
}

// ---------------------------------------------------------------------------
// D22 — before_agent_start union + custom-message runs (a447ba11f)
// ---------------------------------------------------------------------------

/// The upstream `agent-session-prompt.test.ts` case: a run started by
/// `sendCustomMessage(..., { triggerTurn: true })` keeps the system prompt
/// `before_agent_start` gave it for EVERY request of the run — including the
/// request after its first tool call (before the fix, the next-turn refresh fell
/// back to the base prompt options and patched the handler's sections away,
/// changing the prompt mid-run and breaking the provider prompt cache).
#[tokio::test(flavor = "current_thread")]
async fn d22_custom_turn_keeps_the_system_prompt_for_every_request() {
    let temp = TempDir::new("d22-custom-setup");
    let runtime = load_runtime(
        &temp,
        "d22-custom-setup",
        r#"
        export default function (pi) {
          globalThis.__bas = [];
          pi.on("before_agent_start", (event) => {
            globalThis.__bas.push({
              messageType: event.messageType,
              prompt: typeof event.prompt,
              customType: event.message ? event.message.customType : null,
            });
            // The pir surface for "change the prompt for this run" is the
            // replacement text (upstream sections/`systemPrompt`).
            return { systemPrompt: "forced:" + event.messageType };
          });
        }
        "#,
    )
    .await;

    let fixture = session_with_extension(
        &temp.0,
        runtime.clone(),
        Arc::new(|index, _context| match index {
            0 => assistant(vec![text_block("hi")], StopReason::Stop),
            1 => assistant(vec![tool_call("noop")], StopReason::ToolUse),
            _ => assistant(vec![text_block("done")], StopReason::Stop),
        }),
        true,
    );

    fixture
        .session
        .prompt("hello", Arc::new(|_| {}))
        .await
        .expect("prompt");
    fixture
        .session
        .run_custom_turn(
            CustomRunMessage {
                custom_type: "task-done".into(),
                content: json!([{ "type": "text", "text": "background task finished" }]),
                display: true,
                details: None,
            },
            Arc::new(|_| {}),
        )
        .await
        .expect("custom turn");

    let requests = captured(&fixture);
    assert_eq!(
        requests.len(),
        3,
        "one request for the prompt, two for the custom run (tool call + final): {requests:?}",
        requests = requests
            .iter()
            .map(|request| request.system_prompt.clone())
            .collect::<Vec<_>>()
    );
    // The prompt run's request carries the prompt branch of the handler, the
    // custom run's two requests carry the custom branch — and both carry the
    // SAME prompt: a run's system prompt never changes between requests.
    assert_eq!(
        requests[0].system_prompt.as_deref(),
        Some("forced:user"),
        "the prompt run must see messageType \"user\""
    );
    assert_eq!(requests[1].system_prompt.as_deref(), Some("forced:custom"));
    assert_eq!(
        requests[2].system_prompt.as_deref(),
        Some("forced:custom"),
        "the post-tool-call request must keep the run's prompt (the cache-segment bug)"
    );
    // The custom run's starting message is the model-visible text.
    assert!(requests[1].contains("background task finished"));
    assert!(
        requests[1].tool_names.iter().any(|name| name == "noop"),
        "the custom run gets the same tool loadout: {:?}",
        requests[1].tool_names
    );

    let events = probe(&runtime, "globalThis.__bas").await;
    assert_eq!(
        events,
        json!([
            { "messageType": "user", "prompt": "string", "customType": null },
            { "messageType": "custom", "prompt": "undefined", "customType": "task-done" },
        ]),
        "the event is a union: a custom run carries `message` and no `prompt`"
    );
}

/// The cache-segment regression for a plain user run: `before_agent_start`
/// replaces the run's system prompt, and the request AFTER the run's first tool
/// call carries the SAME prompt (upstream: the next-turn refresh used to fall
/// back to the base prompt options and patch the handler's sections away,
/// changing the prompt mid-run and breaking the provider prompt cache).
#[tokio::test(flavor = "current_thread")]
async fn d22_user_run_keeps_the_system_prompt_after_a_tool_call() {
    let temp = TempDir::new("d22-user-setup");
    let runtime = load_runtime(
        &temp,
        "d22-user-setup",
        r#"
        export default function (pi) {
          pi.on("before_agent_start", (event) => {
            return { systemPrompt: "forced:" + event.messageType };
          });
        }
        "#,
    )
    .await;

    let fixture = session_with_extension(
        &temp.0,
        runtime,
        Arc::new(|index, _context| match index {
            0 => assistant(vec![tool_call("noop")], StopReason::ToolUse),
            _ => assistant(vec![text_block("done")], StopReason::Stop),
        }),
        true,
    );

    fixture
        .session
        .prompt("hello", Arc::new(|_| {}))
        .await
        .expect("prompt");

    let requests = captured(&fixture);
    assert_eq!(requests.len(), 2, "tool call + final request");
    assert_eq!(requests[0].system_prompt.as_deref(), Some("forced:user"));
    assert_eq!(
        requests[1].system_prompt.as_deref(),
        Some("forced:user"),
        "the post-tool-call request of the run keeps the run's prompt"
    );
}

/// The busy-while-starting contract: a `before_agent_start` handler sees
/// `ctx.isIdle() === false`, and a `pi.sendMessage(..., { triggerTurn: true })`
/// it sends joins the STARTING run instead of starting a second one.
///
/// Delivery follows the default one-at-a-time steering mode (upstream
/// `PendingMessageQueue` default): the oldest queued message rides the run's
/// first request, the next one is delivered at the following drain point — a
/// second request of the SAME run, not a second run (upstream's
/// `agent-session-prompt.test.ts` fixture queues only one message and so sees a
/// single request; the run identity is what both assert).
#[tokio::test(flavor = "current_thread")]
async fn d22_send_message_while_starting_joins_the_starting_run() {
    let temp = TempDir::new("d22-busy");
    let runtime = load_runtime(
        &temp,
        "d22-busy",
        r#"
        export default function (pi) {
          globalThis.__bas = [];
          pi.on("before_agent_start", (event, ctx) => {
            globalThis.__bas.push(event.messageType);
            if (event.messageType !== "user") return;
            globalThis.__idleWhileStarting = ctx.isIdle();
            pi.sendMessage(
              { customType: "note", content: "task two finished", display: true },
              { triggerTurn: true },
            );
            // While busy the delivery mode is mandatory (upstream
            // sendUserMessage → prompt(..., {streamingBehavior})).
            pi.sendUserMessage("dropped without deliverAs");
            pi.sendUserMessage("kept with deliverAs", { deliverAs: "steer" });
          });
        }
        "#,
    )
    .await;

    let fixture = session_with_extension(
        &temp.0,
        runtime.clone(),
        Arc::new(|_index, _context| assistant(vec![text_block("done")], StopReason::Stop)),
        false,
    );

    fixture
        .session
        .prompt("hello", Arc::new(|_| {}))
        .await
        .expect("prompt");

    let requests = captured(&fixture);
    assert_eq!(
        requests.len(),
        2,
        "both steered messages ride THIS run (one per drain point): {:?}",
        requests
            .iter()
            .map(|request| request.text())
            .collect::<Vec<_>>()
    );
    assert!(
        requests[0].contains("task two finished"),
        "the queued custom message joins the starting run's first request: {}",
        requests[0].text()
    );
    assert!(
        requests[1].contains("kept with deliverAs"),
        "sendUserMessage with deliverAs is queued into the same run: {}",
        requests[1].text()
    );
    assert!(
        !requests
            .iter()
            .any(|request| request.contains("dropped without deliverAs")),
        "sendUserMessage without deliverAs is refused while busy"
    );
    assert_eq!(
        probe(&runtime, "globalThis.__idleWhileStarting").await,
        json!(false),
        "the session is busy while a run starts (before_agent_start)"
    );
    assert_eq!(
        probe(&runtime, "globalThis.__bas").await,
        json!(["user"]),
        "neither queued message started a second run (no extra before_agent_start)"
    );
    assert!(!fixture.session.is_streaming(), "idle after the run");
}

/// An abort requested while the run is still starting stops it before its first
/// request; the run still settles with an aborted assistant message.
#[tokio::test(flavor = "current_thread")]
async fn d22_abort_during_setup_stops_the_run() {
    let temp = TempDir::new("d22-abort");
    let runtime = load_runtime(
        &temp,
        "d22-abort",
        r#"
        export default function (pi) {
          pi.on("before_agent_start", (_event, ctx) => {
            ctx.abort();
          });
        }
        "#,
    )
    .await;

    let fixture = session_with_extension(
        &temp.0,
        runtime,
        Arc::new(|_index, _context| {
            assistant(vec![text_block("should not complete")], StopReason::Stop)
        }),
        false,
    );

    let terminal = fixture
        .session
        .run_custom_turn(
            CustomRunMessage {
                custom_type: "task-done".into(),
                content: "background task finished".into(),
                display: true,
                details: None,
            },
            Arc::new(|_| {}),
        )
        .await
        .expect("custom turn settles");

    assert_eq!(
        terminal.stop_reason,
        StopReason::Aborted,
        "the abort during setup settles the run as aborted"
    );
    assert!(
        captured(&fixture).is_empty(),
        "no provider request may happen after an abort requested during setup"
    );
    let transcript = fixture.session.transcript().await;
    let last = transcript.last().expect("transcript");
    match last {
        Message::Assistant(message) => assert_eq!(message.stop_reason, StopReason::Aborted),
        other => panic!("the last message must be the aborted assistant message, got {other:?}"),
    }
    assert!(!fixture.session.is_streaming(), "the session is idle again");
}

/// An idle `pi.sendMessage(..., { triggerTurn: true })` queues a run of its own
/// (upstream starts it immediately); the host drains the queue and runs it
/// through the same setup. Before the fix it was queued as a follow-up the next
/// prompt picked up.
#[tokio::test(flavor = "current_thread")]
async fn d22_idle_trigger_turn_queues_a_custom_run() {
    let temp = TempDir::new("d22-idle-queue");
    let runtime = load_runtime(
        &temp,
        "d22-idle-queue",
        r#"
        export default function (pi) {
          pi.registerCommand("queue-task", {
            handler: () => {
              pi.sendMessage(
                { customType: "task-done", content: "background task finished", display: true },
                { triggerTurn: true },
              );
            },
          });
          globalThis.__bas = [];
          pi.on("before_agent_start", (event) => {
            globalThis.__bas.push(event.messageType);
          });
        }
        "#,
    )
    .await;

    let fixture = session_with_extension(
        &temp.0,
        runtime.clone(),
        Arc::new(|_index, _context| assistant(vec![text_block("handled")], StopReason::Stop)),
        false,
    );

    // Idle: the host applies the notification and finds the queued run.
    runtime
        .dispatch_command("queue-task", "")
        .await
        .expect("command");
    assert!(!fixture.session.has_pending_custom_runs());
    fixture
        .session
        .apply_pending_extension_notifications()
        .await;
    assert!(
        fixture.session.has_pending_custom_runs(),
        "an idle triggerTurn message queues a run"
    );
    assert!(
        fixture.session.get_steering_messages().await.is_empty()
            && fixture.session.get_follow_up_messages().await.is_empty(),
        "it must not be queued behind the next prompt"
    );

    let queued = fixture
        .session
        .take_pending_custom_run()
        .expect("pending custom run");
    assert_eq!(queued.custom_type, "task-done");
    assert_eq!(queued.text(), "background task finished");
    assert!(!fixture.session.has_pending_custom_runs());

    fixture
        .session
        .run_custom_turn(queued, Arc::new(|_| {}))
        .await
        .expect("custom turn");
    let requests = captured(&fixture);
    assert_eq!(requests.len(), 1);
    assert!(requests[0].contains("background task finished"));
    assert_eq!(
        probe(&runtime, "globalThis.__bas").await,
        json!(["custom"]),
        "the queued run fires before_agent_start with messageType \"custom\""
    );
}

/// `deliverAs: "nextTurn"` is held for the next run's starting message instead
/// of riding the follow-up queue.
#[tokio::test(flavor = "current_thread")]
async fn d22_next_turn_message_rides_the_next_run() {
    let temp = TempDir::new("d22-next-turn");
    let runtime = load_runtime(
        &temp,
        "d22-next-turn",
        r#"
        export default function (pi) {
          pi.registerCommand("queue-next", {
            handler: () => {
              pi.sendMessage(
                { customType: "note", content: "context for the next turn", display: false },
                { deliverAs: "nextTurn" },
              );
            },
          });
        }
        "#,
    )
    .await;

    let fixture = session_with_extension(
        &temp.0,
        runtime.clone(),
        Arc::new(|_index, _context| assistant(vec![text_block("ok")], StopReason::Stop)),
        false,
    );

    runtime
        .dispatch_command("queue-next", "")
        .await
        .expect("command");
    fixture
        .session
        .apply_pending_extension_notifications()
        .await;
    assert_eq!(
        fixture.session.peek_pending_next_turn_messages().len(),
        1,
        "nextTurn messages wait for the next run"
    );
    assert!(fixture.session.get_follow_up_messages().await.is_empty());

    fixture
        .session
        .prompt("hello", Arc::new(|_| {}))
        .await
        .expect("prompt");
    let requests = captured(&fixture);
    assert_eq!(requests.len(), 1);
    assert!(
        requests[0].contains("context for the next turn"),
        "the held message is delivered with the next run"
    );
    assert!(
        fixture.session.peek_pending_next_turn_messages().is_empty(),
        "the run consumes the held messages"
    );
}

/// A `triggerTurn: false` message sent while the run starts is appended (not
/// steered) and rides the run's FIRST request (upstream
/// `agent-session-prompt.test.ts`: "includes a custom message sent without
/// triggerTurn while the run starts in its first request").
#[tokio::test(flavor = "current_thread")]
async fn d22_plain_message_while_starting_rides_the_first_request() {
    let temp = TempDir::new("d22-plain-while-starting");
    let runtime = load_runtime(
        &temp,
        "d22-plain-while-starting",
        r#"
        export default function (pi) {
          pi.on("before_agent_start", () => {
            pi.sendMessage(
              { customType: "note", content: "context from handler", display: true },
              { triggerTurn: false },
            );
          });
        }
        "#,
    )
    .await;

    let fixture = session_with_extension_options(
        &temp.0,
        runtime,
        Arc::new(|_index, _context| assistant(vec![text_block("done")], StopReason::Stop)),
        false,
        // The appended custom message lives in session storage (upstream
        // `_appendCustomMessage`); the persisted variant carries it.
        true,
    );

    fixture
        .session
        .prompt("hello", Arc::new(|_| {}))
        .await
        .expect("prompt");

    let requests = captured(&fixture);
    assert_eq!(requests.len(), 1, "{:?}", requests.len());
    assert!(
        requests[0].contains("context from handler"),
        "the appended custom message rides the first request: {}",
        requests[0].text()
    );
}

/// The store-less (ephemeral) sibling of
/// [`d22_plain_message_while_starting_rides_the_first_request`]: a session with
/// no session manager (e.g. a subagent child session) must hold the
/// `triggerTurn: false` message in memory instead of silently dropping it, so it
/// still reaches the run's first request — upstream `_appendCustomMessage`
/// always has a session manager to append to.
#[tokio::test(flavor = "current_thread")]
async fn d22_plain_message_while_starting_rides_the_first_request_ephemeral() {
    let temp = TempDir::new("d22-plain-ephemeral");
    let runtime = load_runtime(
        &temp,
        "d22-plain-ephemeral",
        r#"
        export default function (pi) {
          pi.on("before_agent_start", () => {
            pi.sendMessage(
              { customType: "note", content: "context from handler", display: true },
              { triggerTurn: false },
            );
          });
        }
        "#,
    )
    .await;

    // No session store: the transcript lives in memory only.
    let fixture = session_with_extension(
        &temp.0,
        runtime,
        Arc::new(|_index, _context| assistant(vec![text_block("done")], StopReason::Stop)),
        false,
    );

    fixture
        .session
        .prompt("hello", Arc::new(|_| {}))
        .await
        .expect("prompt");

    let requests = captured(&fixture);
    assert_eq!(requests.len(), 1, "{:?}", requests.len());
    assert_eq!(
        requests[0].text().matches("context from handler").count(),
        1,
        "the held custom message rides the store-less run's first request exactly once: {}",
        requests[0].text()
    );

    // It lives in the in-memory transcript (upstream appends to the session
    // manager), so a later run keeps it rather than losing it.
    let transcript = fixture.session.transcript().await;
    assert!(
        transcript.iter().any(|message| {
            serde_json::to_string(message)
                .unwrap_or_default()
                .contains("context from handler")
        }),
        "the held message is part of the ephemeral transcript: {transcript:?}"
    );
}

// ---------------------------------------------------------------------------
// D23 — reachesTools + namespace aliases (24382bedd)
// ---------------------------------------------------------------------------

/// `pi.getAllTools()` exposes a tool's `reachesTools` hook as a callable (the
/// host cannot serialize a function; the live hook is re-attached in QuickJS)
/// and carries the namespace's `aliases` through the host round trip.
#[tokio::test(flavor = "current_thread")]
async fn d23_reaches_tools_and_namespace_aliases_reach_get_all_tools() {
    let temp = TempDir::new("d23-tools");
    let runtime = load_runtime(
        &temp,
        "d23-tools",
        r#"
        export default function (pi) {
          pi.registerTool({
            name: "codemode",
            description: "runs a script",
            parameters: { type: "object", properties: {} },
            reachesTools: (input) => (input.code === "everything" ? "all" : [input.code]),
            async execute() { return { content: [{ type: "text", text: "ok" }] }; },
          });
          pi.registerTool({
            name: "plain",
            description: "does not reach other tools",
            parameters: { type: "object", properties: {} },
            namespace: { name: "mcp__docs", aliases: ["docs"], description: "Docs server" },
            async execute() { return { content: [{ type: "text", text: "ok" }] }; },
          });
          pi.registerCommand("inspect-tools", {
            handler: () => {
              globalThis.__tools = pi.getAllTools().map((tool) => ({
                name: tool.name,
                hasReach: "reachesTools" in tool,
                reachKind: tool.reachesTools ? typeof tool.reachesTools : null,
                all: tool.reachesTools ? tool.reachesTools({ code: "everything" }) : null,
                named: tool.reachesTools ? tool.reachesTools({ code: "mcp__docs__search" }) : null,
                aliases: tool.namespace ? (tool.namespace.aliases ?? null) : null,
                namespace: tool.namespace ? tool.namespace.name : null,
              }));
            },
          });
        }
        "#,
    )
    .await;

    runtime
        .dispatch_command("inspect-tools", "")
        .await
        .expect("command");
    let tools = probe(&runtime, "globalThis.__tools").await;
    let tools = tools.as_array().expect("tools");
    let codemode = tools
        .iter()
        .find(|tool| tool["name"] == json!("codemode"))
        .expect("codemode tool");
    assert_eq!(codemode["hasReach"], json!(true));
    assert_eq!(
        codemode["reachKind"],
        json!("function"),
        "the live reachesTools function is re-attached (ToolInfo carries it)"
    );
    assert_eq!(codemode["all"], json!("all"));
    assert_eq!(
        codemode["named"],
        json!(["mcp__docs__search"]),
        "the hook sees the call's input"
    );
    let plain = tools
        .iter()
        .find(|tool| tool["name"] == json!("plain"))
        .expect("plain tool");
    assert_eq!(
        plain["hasReach"],
        json!(false),
        "a tool without the hook carries no `reachesTools` key"
    );
    assert_eq!(plain["namespace"], json!("mcp__docs"));
    assert_eq!(
        plain["aliases"],
        json!(["docs"]),
        "namespace aliases survive the host round trip"
    );
}

/// The Rust mirror of the reach rule (upstream `reachesServer`,
/// `extensions/mcp/index.ts:224-228`): a call reaches every namespace with
/// `"all"`, and otherwise namespaces one of its words contains.
#[test]
fn d23_tool_reach_matches_namespaces_like_upstream() {
    assert!(ToolReach::All.reaches_namespace("mcp__docs"));
    assert!(ToolReach::All.reaches_namespace("anything"));

    let names = ToolReach::Names(vec!["tools.mcp__docs__search".into()]);
    assert!(names.reaches_namespace("mcp__docs"));
    assert!(!names.reaches_namespace("mcp__other"));

    let partial = ToolReach::Names(vec!["docs".into()]);
    assert!(
        !partial.reaches_namespace("mcp__docs"),
        "a word matches a namespace whose name it CONTAINS, not the reverse"
    );
    assert!(partial.reaches_namespace("docs"));
    assert!(!ToolReach::Names(Vec::new()).reaches_namespace("mcp__docs"));
}

// ---------------------------------------------------------------------------
// D24 — published configuration schemas (96abe9a5a)
// ---------------------------------------------------------------------------

/// The four generated schemas declare the published `$id`s on pi.dev while the
/// committed artifacts keep their repository path (`generate-schemas.ts` keeps
/// the URL and the output path separate).
#[test]
fn d24_schemas_publish_pi_dev_urls() {
    let rendered = pir::config::schema::render_config_schemas();
    assert_eq!(
        rendered
            .iter()
            .map(|(path, _)| path.to_string())
            .collect::<Vec<_>>(),
        vec![
            "schemas/models.schema.json",
            "schemas/settings.schema.json",
            "schemas/keybindings.schema.json",
            "schemas/theme.schema.json",
        ],
        "the artifacts stay committed under schemas/"
    );
    for (path, schema) in &rendered {
        let id = schema["$id"].as_str().expect("$id");
        assert_eq!(
            id,
            format!("https://pi.dev/schemas/{}", &path["schemas/".len()..]),
            "the published $id of {path}"
        );
        assert_eq!(
            schema["$schema"],
            "https://json-schema.org/draft/2020-12/schema"
        );
        assert!(
            !id.contains("raw.githubusercontent.com"),
            "the repository URL is not the published one: {id}"
        );
    }
    assert_eq!(
        pir::config::schema::SCHEMA_BASE_URL_SETTINGS,
        "https://pi.dev/schemas/settings.schema.json"
    );
    assert_eq!(
        pir::config::schema::SCHEMA_BASE_URL_MODELS,
        "https://pi.dev/schemas/models.schema.json"
    );
    assert_eq!(
        pir::config::schema::SCHEMA_BASE_URL_KEYBINDINGS,
        "https://pi.dev/schemas/keybindings.schema.json"
    );
    assert_eq!(
        pir::config::schema::SCHEMA_BASE_URL_THEMES,
        "https://pi.dev/schemas/theme.schema.json"
    );
}

/// The committed artifacts match the generated ones (the freshness gate reads
/// the same files the docs point at).
#[test]
fn d24_committed_schemas_carry_the_published_ids() {
    for (relative, generated) in pir::config::schema::render_config_schemas() {
        let committed: Value = serde_json::from_str(
            &std::fs::read_to_string(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(relative))
                .unwrap_or_else(|error| panic!("missing committed schema {relative}: {error}")),
        )
        .expect("committed schema is valid JSON");
        assert_eq!(committed, generated, "{relative} is stale");
    }
}

/// A theme loaded at startup passes through exactly the same validation as a
/// reload: the same unknown-field and unknown-color errors, and a valid theme
/// loads.
#[test]
fn d24_theme_validation_is_the_same_at_startup_and_reload() {
    let temp = TempDir::new("d24-theme");
    let themes = temp.0.join("themes");
    std::fs::create_dir_all(&themes).expect("themes dir");
    // Unknown top-level property (upstream: `additionalProperties: false`).
    std::fs::write(
        themes.join("bad-top.json"),
        r#"{"name":"bad-top","metadata":{"x":1},"colors":{}}"#,
    )
    .expect("write");
    // Unknown color token.
    std::fs::write(
        themes.join("bad-color.json"),
        r##"{"name":"bad-color","colors":{"notAColor":"#ffffff"}}"##,
    )
    .expect("write");
    // A valid theme, loaded through the SAME startup path. The built-in dark
    // palette renamed: every non-optional token must be present (upstream
    // `validateThemeJson` also rejects a partial `colors` object).
    let mut vars = serde_json::Map::new();
    for (name, value) in pir::tui::theme::DARK_PALETTE.vars {
        vars.insert((*name).to_string(), json!(value));
    }
    let mut colors = serde_json::Map::new();
    for (name, value) in pir::tui::theme::DARK_PALETTE.colors {
        colors.insert((*name).to_string(), json!(value));
    }
    std::fs::write(
        themes.join("good.json"),
        json!({
            "$schema": "https://pi.dev/schemas/theme.schema.json",
            "name": "w2-good-theme",
            "appearance": "dark",
            "vars": vars,
            "colors": colors,
        })
        .to_string(),
    )
    .expect("write");

    let loaded = pir::tui::theme::load_custom_themes(&themes).expect("load dir");
    assert_eq!(loaded.errors.len(), 2, "errors: {:?}", loaded.errors);
    for (file, error) in &loaded.errors {
        assert!(
            error.contains("unknown field") || error.contains("unknown color token"),
            "{file}: {error}"
        );
    }
    assert_eq!(
        loaded.names,
        vec!["w2-good-theme".to_string()],
        "only the valid theme registers (errors: {:?})",
        loaded.errors
    );
}

/// `showHardwareCursor` uses ONLY the terminal cursor: with it on, the editor
/// cell stays plain (no reverse-video fake cursor); with it off (the default),
/// the reverse-video cell is what the user sees.
#[test]
fn d24_show_hardware_cursor_drops_the_reverse_video_cell() {
    let editor = pir::tui::editor::Editor::new();
    editor.set_text("hello");

    let (lines, _row, _col) = editor.render_with_cursor(40, 10);
    let painted = lines.join("\n");
    assert!(
        painted.contains("\x1b[7m"),
        "the default draws the reverse-video cursor: {painted:?}"
    );

    editor.set_show_hardware_cursor(true);
    let (lines, _row, _col) = editor.render_with_cursor(40, 10);
    let painted = lines.join("\n");
    assert!(
        !painted.contains("\x1b[7m"),
        "with showHardwareCursor the terminal cursor is the only cursor: {painted:?}"
    );
    assert!(painted.contains("hello"), "the text stays: {painted:?}");
}

/// The default stays off unless the setting or `PI_HARDWARE_CURSOR=1` asks for
/// it (upstream `getShowHardwareCursor`).
#[test]
fn d24_show_hardware_cursor_defaults_off() {
    let temp = TempDir::new("d24-cursor-default");
    let settings = pir::config::settings::Settings::default();
    assert_eq!(settings.show_hardware_cursor, None);
    let manager = pir::config::settings::SettingsManager::load(&temp.0, &temp.0, &temp.0, false);
    // Without the setting the environment variable decides (upstream
    // `getShowHardwareCursor`).
    let env_asks = std::env::var("PI_HARDWARE_CURSOR").is_ok_and(|value| value == "1");
    assert_eq!(manager.show_hardware_cursor(), env_asks);
}

// ---------------------------------------------------------------------------
// Timers: keep the fixture honest about waits
// ---------------------------------------------------------------------------

/// Guard against a hung run in this binary: the runs above are all in-process
/// and must settle well inside this bound.
#[tokio::test(flavor = "current_thread")]
async fn w2_fixture_runtime_settles_quickly() {
    let temp = TempDir::new("w2-fixture");
    let fixture = session_with_extension(
        &temp.0,
        load_runtime(&temp, "w2-fixture", "export default function () {}\n").await,
        Arc::new(|_index, _context| assistant(vec![text_block("ok")], StopReason::Stop)),
        false,
    );
    let run = fixture.session.prompt("hello", Arc::new(|_| {}));
    let terminal = tokio::time::timeout(Duration::from_secs(10), run)
        .await
        .expect("run settles")
        .expect("prompt");
    assert_eq!(terminal.stop_reason, StopReason::Stop);
}
