//! Native Rust plugin gates: the result-contract events (`tool_call`,
//! `tool_result`) and the host-dispatched (`user_bash`, `project_trust`,
//! print `session_start`/`input`) events reach the unified plugin host's
//! native fan-out, not only the JS `ExtensionHandle`.
//!
//! Parity contract pinned here:
//!
//! - a native `tool_call` handler blocks a call: the tool never runs and the
//!   model-visible tool result is the handler's block reason;
//! - a native `tool_call` handler rewrites `input` in place: the scheduler
//!   executes the rewritten arguments;
//! - a native `tool_result` handler overlays `content`/`details`: the
//!   transcript (and therefore the model-visible result) carries the native
//!   merge;
//! - `user_bash`, `project_trust`, and the print host's
//!   `session_start`/`input` reach a native plugin exactly once with the same
//!   wire payload the JS face gets.
//!
//! The fixture plugins are inline Rust implementations (no QuickJS); the
//! plan-only extension handles cover the builtin-only session shape.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use serde_json::{json, Value};

use pir::agent::agent_loop::{run_with_options, LoopDeps, LoopOptions, ProviderRequest};
use pir::agent::tool_batch::ToolResult;
use pir::agent::tool_execution::{AgentTool, BoxFuture, ToolContext, ToolRegistry};
use pir::agent::tool_scheduler::{ToolScheduler, ToolSchedulerConfig};
use pir::ai::types::{
    AssistantMessage, ContentBlock, Context, StopReason, TextContent, ToolCall, ToolResultMessage,
};
use pir::extensions::{ExtensionConfig, ExtensionHandle};
use pir::plugins::{
    AgentPlugin, EventInterest, InputAction, PluginEvent, PluginEventOutcome, PluginFuture,
    PluginHost, PluginInfo,
};

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

struct TempDir(PathBuf);

impl TempDir {
    fn new(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "pir-plugin-gates-{label}-{}-{}",
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

fn extension_config(cwd: &std::path::Path) -> ExtensionConfig {
    ExtensionConfig {
        cwd: cwd.to_path_buf(),
        mode: "headless".to_owned(),
        has_ui: false,
        model: None,
        session_id: None,
        system_prompt: String::new(),
        is_idle: true,
        limits: Some(Default::default()),
    }
}

/// The producer side of one `tool_call` event record.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Behavior {
    /// Block every `tool_call` with [`BLOCK_REASON`].
    Block,
    /// Rewrite `tool_call.input.value` to `"rewritten"`.
    Rewrite,
    /// Overlay every `tool_result` with [`OVERRIDE_CONTENT`].
    OverrideResult,
    /// Transform every `input` event by appending `"[native]"`.
    TransformInput,
}

const BLOCK_REASON: &str = "denied by the native plugin";
const OVERRIDE_CONTENT: &str = "redacted by the native plugin";

/// Events recorded by the plugin, shared with the test body.
type RecordedEvents = Arc<Mutex<Vec<(String, Value)>>>;

/// A native plugin that records every event it observes and applies the one
/// configured gate behavior.
struct GatePlugin {
    info: PluginInfo,
    behavior: Behavior,
    seen: Arc<Mutex<Vec<(String, Value)>>>,
}

impl GatePlugin {
    fn new(behavior: Behavior) -> (Arc<Self>, RecordedEvents) {
        let seen = Arc::new(Mutex::new(Vec::new()));
        (
            Arc::new(Self {
                info: PluginInfo::inline("gate-recorder"),
                behavior,
                seen: Arc::clone(&seen),
            }),
            seen,
        )
    }
}

impl AgentPlugin for GatePlugin {
    fn info(&self) -> &PluginInfo {
        &self.info
    }

    fn events(&self) -> EventInterest {
        // The recorder must observe every event family under test; the gate
        // behavior itself only fires for its configured event.
        EventInterest::All
    }

    fn on_event<'a>(&'a self, mut event: PluginEvent<'a>) -> PluginFuture<'a, PluginEventOutcome> {
        Box::pin(async move {
            let name = event.name.to_string();
            self.seen
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push((name.clone(), event.payload.clone()));
            match (self.behavior, name.as_str()) {
                (Behavior::Block, "tool_call") => PluginEventOutcome::ToolCall {
                    block: true,
                    reason: Some(BLOCK_REASON.to_owned()),
                    terminate: false,
                },
                (Behavior::Rewrite, "tool_call") => {
                    // In-place mutation: later handlers and the host observe
                    // the rewrite (the same contract a JS handler has).
                    if let Some(input) = event.tool_call_input_mut() {
                        if let Some(object) = input.as_object_mut() {
                            object.insert("value".to_owned(), json!("rewritten"));
                        }
                    }
                    PluginEventOutcome::None
                }
                (Behavior::OverrideResult, "tool_result") => PluginEventOutcome::ToolResult {
                    content: Some(json!([
                        { "type": "text", "text": OVERRIDE_CONTENT }
                    ])),
                    details: Some(json!({ "native": true })),
                    structured_content: None,
                    is_error: None,
                    usage: None,
                },
                (Behavior::TransformInput, "input") => PluginEventOutcome::Input {
                    action: InputAction::Transform,
                    text: Some(format!(
                        "{}[native]",
                        event.payload["text"].as_str().unwrap_or_default()
                    )),
                    images: None,
                },
                _ => PluginEventOutcome::None,
            }
        })
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
        api: "test".into(),
        provider: "test".into(),
        model: "plugin-gates-model".into(),
        content,
        stop_reason,
        timestamp: 1,
        ..AssistantMessage::default()
    }
}

fn tool_call_block(name: &str, arguments: Value) -> ContentBlock {
    let arguments: BTreeMap<String, Value> =
        serde_json::from_value(arguments).expect("object arguments");
    ContentBlock::ToolCall(ToolCall {
        id: "call-1".into(),
        name: name.into(),
        arguments,
        thought_signature: None,
        namespace: None,
        arguments_error: None,
    })
}

/// A tool that records that it ran and the arguments it received.
struct ProbeTool {
    ran: Arc<AtomicBool>,
    received: Arc<Mutex<Vec<Value>>>,
}

impl ProbeTool {
    fn new() -> (Arc<Self>, Arc<AtomicBool>, Arc<Mutex<Vec<Value>>>) {
        let ran = Arc::new(AtomicBool::new(false));
        let received = Arc::new(Mutex::new(Vec::new()));
        (
            Arc::new(Self {
                ran: Arc::clone(&ran),
                received: Arc::clone(&received),
            }),
            ran,
            received,
        )
    }
}

impl AgentTool for ProbeTool {
    fn name(&self) -> &'static str {
        "probe"
    }

    fn description(&self) -> &'static str {
        "gate probe"
    }

    fn run(&self, args: &Value, _ctx: &ToolContext) -> BoxFuture<'static, ToolResult> {
        self.ran.store(true, Ordering::SeqCst);
        self.received
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(args.clone());
        Box::pin(async { ToolResult::ok("tool ran") })
    }
}

/// Two scripted model responses: one `probe` tool call, then a final text
/// turn. Uses the direct [`LoopDeps::stream`] seam (no event stream needed:
/// the gate and result paths do not depend on stream lifecycle events).
fn gate_deps(temp: &TempDir, plugin: Arc<GatePlugin>, tool: Arc<ProbeTool>) -> LoopDeps {
    let mut registry = ToolRegistry::new();
    registry.register(tool);
    let calls = Arc::new(AtomicUsize::new(0));
    LoopDeps {
        stream: Box::new(move |_request: ProviderRequest| {
            let turn = calls.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move {
                if turn == 0 {
                    assistant(
                        vec![tool_call_block("probe", json!({ "value": "original" }))],
                        StopReason::ToolUse,
                    )
                } else {
                    assistant(vec![text_block("done")], StopReason::Stop)
                }
            })
        }),
        tools: Arc::new(registry),
        scheduler: ToolScheduler::new(ToolSchedulerConfig::default()),
        plugin_events: Some(Arc::new(PluginHost::build(
            None,
            vec![plugin as Arc<dyn AgentPlugin>],
        ))),
        // Native-only plan: no JS runtime handle at all, the shape the
        // `tool_call`/`tool_result` fan-out had to be fixed for.
        extensions: None,
        max_turns: 4,
        cwd: temp.0.clone(),
        ..LoopDeps::default()
    }
}

fn message_text(message: &ToolResultMessage) -> String {
    message
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text(text) => Some(text.text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn event_count(seen: &Arc<Mutex<Vec<(String, Value)>>>, name: &str) -> usize {
    seen.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .iter()
        .filter(|(event, _)| event == name)
        .count()
}

// ---------------------------------------------------------------------------
// 1. Native block
// ---------------------------------------------------------------------------

#[tokio::test]
async fn native_tool_call_handler_blocks_the_call_and_surfaces_the_reason() {
    let temp = TempDir::new("block");
    let (plugin, seen) = GatePlugin::new(Behavior::Block);
    let (tool, ran, received) = ProbeTool::new();

    let result = run_with_options(
        gate_deps(&temp, plugin, tool),
        Context::default(),
        None,
        LoopOptions::default(),
    )
    .await;

    assert!(
        !ran.load(Ordering::SeqCst),
        "a native `tool_call` block must prevent the tool from running"
    );
    assert!(
        received
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_empty(),
        "a blocked tool receives no arguments"
    );
    assert_eq!(result.tool_results.len(), 1, "one synthesized result");
    let blocked = &result.tool_results[0];
    assert!(blocked.is_error, "the block is model-visible as an error");
    assert_eq!(
        message_text(blocked),
        BLOCK_REASON,
        "the model sees the native handler's block reason verbatim"
    );
    assert_eq!(
        event_count(&seen, "tool_call"),
        1,
        "the native plugin observes `tool_call` exactly once"
    );
    assert_eq!(
        event_count(&seen, "tool_result"),
        0,
        "a blocked call never produces a `tool_result`"
    );
}

// ---------------------------------------------------------------------------
// 2. Native rewrite
// ---------------------------------------------------------------------------

#[tokio::test]
async fn native_tool_call_handler_rewrites_the_input_the_tool_executes() {
    let temp = TempDir::new("rewrite");
    let (plugin, seen) = GatePlugin::new(Behavior::Rewrite);
    let (tool, ran, received) = ProbeTool::new();

    let result = run_with_options(
        gate_deps(&temp, plugin, tool),
        Context::default(),
        None,
        LoopOptions::default(),
    )
    .await;

    assert!(ran.load(Ordering::SeqCst), "the rewritten call executes");
    let received = received
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    assert_eq!(received.len(), 1);
    assert_eq!(
        received[0]["value"],
        json!("rewritten"),
        "the tool sees the native handler's in-place input rewrite: {received:?}"
    );
    assert_eq!(result.tool_results.len(), 1);
    assert!(!result.tool_results[0].is_error);
    assert_eq!(message_text(&result.tool_results[0]), "tool ran");
    assert_eq!(event_count(&seen, "tool_call"), 1);
}

// ---------------------------------------------------------------------------
// 3. Native result overlay
// ---------------------------------------------------------------------------

#[tokio::test]
async fn native_tool_result_handler_overrides_the_transcript_result() {
    let temp = TempDir::new("result");
    let (plugin, seen) = GatePlugin::new(Behavior::OverrideResult);
    let (tool, _ran, _received) = ProbeTool::new();

    let result = run_with_options(
        gate_deps(&temp, plugin, tool),
        Context::default(),
        None,
        LoopOptions::default(),
    )
    .await;

    assert_eq!(result.tool_results.len(), 1);
    let overridden = &result.tool_results[0];
    assert_eq!(
        message_text(overridden),
        OVERRIDE_CONTENT,
        "the transcript carries the native content overlay"
    );
    assert_eq!(
        overridden.details,
        Some(json!({ "native": true })),
        "the transcript carries the native details overlay"
    );
    assert_eq!(
        event_count(&seen, "tool_result"),
        1,
        "the native plugin observes `tool_result` exactly once"
    );
}

// ---------------------------------------------------------------------------
// 4. user_bash / project_trust / print session_start + input
// ---------------------------------------------------------------------------

#[tokio::test]
async fn native_plugin_observes_user_bash_project_trust_and_print_events_once() {
    let temp = TempDir::new("host-events");
    let (plugin, seen) = GatePlugin::new(Behavior::TransformInput);

    // The builtin-only/production plan shape: a plan-only handle (no QuickJS
    // actor) recorded in the path plan, plus the native plugin. The host
    // therefore carries the JS adapter too, and every dispatch below must
    // still reach the native plugin exactly once.
    let handle = ExtensionHandle::start_plan_only(extension_config(&temp.0));
    assert!(!handle.has_js_runtime(), "the plan is builtin-only");
    handle.set_path_plan(Vec::new());
    let host = Arc::new(PluginHost::build(
        Some(handle.clone()),
        vec![plugin as Arc<dyn AgentPlugin>],
    ));

    // `user_bash`: no JS handlers, native interest only.
    let outcome = pir::agent_session::dispatch_user_bash_with_host(
        Some(&handle),
        Some(&host),
        "echo hi",
        true,
    )
    .await;
    assert!(
        matches!(outcome, pir::agent_session::UserBashOutcome::Local),
        "a non-deciding native handler keeps local execution"
    );

    // `project_trust`: the native half of the loader's startup event.
    pir::agent_session::dispatch_project_trust(None, Some(&host), "/tmp/project").await;

    // Print host lifecycle: `session_start` then `input`, both through the
    // unified host (the JS adapter is dispatched once, at its slot).
    pir::coding_agent::print::dispatch_print_session_start(&host, "startup").await;
    let transformed =
        pir::coding_agent::print::transform_print_input(Some(&host), Some(&handle), "hello").await;
    assert_eq!(
        transformed, "hello[native]",
        "a native `input` transform rewrites the print prompt"
    );

    let seen_events = seen
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    for name in ["user_bash", "project_trust", "session_start", "input"] {
        assert_eq!(
            event_count(&seen, name),
            1,
            "`{name}` must reach the native plugin exactly once: {:?}",
            seen_events
                .iter()
                .map(|(event, _)| event.as_str())
                .collect::<Vec<_>>()
        );
    }

    let payload = |name: &str| {
        seen_events
            .iter()
            .find(|(event, _)| event == name)
            .map(|(_, payload)| payload)
            .unwrap_or_else(|| panic!("`{name}` payload missing"))
    };

    let user_bash = payload("user_bash");
    assert_eq!(user_bash["type"], json!("user_bash"));
    assert_eq!(user_bash["command"], json!("echo hi"));
    assert_eq!(user_bash["excludeFromContext"], json!(true));
    assert!(user_bash["cwd"].is_string(), "{user_bash}");

    let project_trust = payload("project_trust");
    assert_eq!(project_trust["type"], json!("project_trust"));
    assert_eq!(project_trust["cwd"], json!("/tmp/project"));

    let session_start = payload("session_start");
    assert_eq!(session_start["type"], json!("session_start"));
    assert_eq!(session_start["reason"], json!("startup"));

    let input = payload("input");
    assert_eq!(input["type"], json!("input"));
    assert_eq!(input["text"], json!("hello"));
    assert_eq!(input["source"], json!("rpc"));
    assert_eq!(input["images"], json!([]));
}
