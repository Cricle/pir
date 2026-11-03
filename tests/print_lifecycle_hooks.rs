//! Print-host lifecycle hook parity (PIR audit P1-2).
//!
//! These tests exercise the PRODUCTION print seams (`transform_print_input_parts`,
//! `dispatch_print_before_agent_start`, `print_turn_end_hook`,
//! `dispatch_print_agent_before_settle`, `dispatch_print_resources_discover`,
//! `print_compaction_hook`) against a real `PluginHost` + inline `AgentPlugin`,
//! the same host shape `run_print` builds. The assertions pin:
//! - every positional prompt passes through `input`;
//! - `before_agent_start` can replace the system prompt and inject messages;
//! - `turn_end` / `agent_before_settle` fire with the AgentSession payload and
//!   honor `continue`;
//! - `resources_discover` returns contributed paths;
//! - `session_before_compact` can cancel or replace, and success emits
//!   `session_compact`;
//! - no handler means byte-identical behavior and zero emissions.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use pir::agent::agent_loop::{run_with_options, LoopOptions, StreamFn};
use pir::agent::tool_batch::ToolResult;
use pir::agent::tool_execution::{AgentTool, BoxFuture, ToolContext, ToolRegistry};
use pir::agent::tool_scheduler::{ToolScheduler, ToolSchedulerConfig};
use pir::ai::auth::ApiKeyAuth;
use pir::ai::event_stream::AssistantMessageEventStream;
use pir::ai::provider::{Provider, ProviderRegistry};
use pir::ai::providers::PiEnvironment;
use pir::ai::types::{
    AssistantMessage, ContentBlock, Context, Message, Model, ModelCost, ModelCostRates,
    ProviderStreamHooks, StopReason, StreamOptions, TerminalReason, TextContent, ThinkingLevel,
    ToolCall, Usage, UserMessage, UserRole,
};
use pir::coding_agent::json::to_session_event_json;
use pir::coding_agent::print::{
    dispatch_print_agent_before_settle, dispatch_print_before_agent_start,
    dispatch_print_resources_discover, drain_print_extension_notifications, print_compaction_hook,
    print_loop_deps, print_plugin_host, print_turn_end_hook, run_print_with_environment,
    sync_print_plugin_providers, transform_print_input_parts, OutputMode, PrintNotificationPhase,
    PrintOptions, PrintOutcome, PrintSessionEventSink, SharedSession,
};
use pir::coding_agent::resources::ResourcePathOptions;
use pir::coding_agent::session_resolve::SessionPlan;
use pir::compaction::CompactionConfig;
use pir::config::settings::{CompactionSettings, Settings};
use pir::config::Config;
use pir::extensions::{ExtensionConfig, ExtensionHandle};
use pir::plugins::{
    AgentPlugin, EventInterest, PluginEvent, PluginEventOutcome, PluginFuture, PluginHost,
    PluginInfo,
};
use pir::session_manager::{FileEntry, SessionManager};

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

struct TempDir(std::path::PathBuf);

impl TempDir {
    fn new(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "pir-print-hooks-{label}-{}-{}",
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    InputTransform,
    InputHandled,
    InputImages,
    BeforeStart,
    TurnEndContinue,
    SettleContinue,
    Resources,
    CompactCancel,
    CompactReplace,
}

struct ProbePlugin {
    info: PluginInfo,
    mode: Mode,
    seen: Arc<Mutex<Vec<String>>>,
    calls: Arc<AtomicUsize>,
    /// `context_edit` target for the settle-draft test.
    settle_target: Option<String>,
}

impl ProbePlugin {
    fn new(mode: Mode) -> Arc<Self> {
        Arc::new(Self {
            info: PluginInfo::inline(match mode {
                Mode::InputTransform => "probe-input-transform",
                Mode::InputHandled => "probe-input-handled",
                Mode::InputImages => "probe-input-images",
                Mode::BeforeStart => "probe-before-start",
                Mode::TurnEndContinue => "probe-turn-end",
                Mode::SettleContinue => "probe-settle",
                Mode::Resources => "probe-resources",
                Mode::CompactCancel => "probe-compact-cancel",
                Mode::CompactReplace => "probe-compact-replace",
            }),
            mode,
            seen: Arc::new(Mutex::new(Vec::new())),
            calls: Arc::new(AtomicUsize::new(0)),
            settle_target: None,
        })
    }

    fn with_settle_target(mode: Mode, target: String) -> Arc<Self> {
        let mut plugin = Arc::try_unwrap(Self::new(mode)).unwrap_or_else(|_| unreachable!());
        plugin.settle_target = Some(target);
        Arc::new(plugin)
    }

    fn host(plugin: Arc<Self>) -> Arc<PluginHost> {
        Arc::new(PluginHost::build(None, vec![plugin]))
    }

    fn event_count(&self, event: &str) -> usize {
        self.seen
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .filter(|seen| seen.as_str() == event)
            .count()
    }
}

impl AgentPlugin for ProbePlugin {
    fn info(&self) -> &PluginInfo {
        &self.info
    }

    fn events(&self) -> EventInterest {
        EventInterest::All
    }

    fn on_event<'a>(&'a self, event: PluginEvent<'a>) -> PluginFuture<'a, PluginEventOutcome> {
        self.seen
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(event.name.to_string());
        let call = self.calls.fetch_add(1, Ordering::SeqCst);
        let name = event.name.to_string();
        let text = event
            .payload
            .get("text")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_owned();
        Box::pin(async move {
            match (self.mode, name.as_str()) {
                (Mode::InputTransform, "input") => PluginEventOutcome::Input {
                    action: pir::plugins::InputAction::Transform,
                    text: Some(format!("{text} [transformed]")),
                    images: None,
                },
                (Mode::InputHandled, "input") => PluginEventOutcome::Input {
                    action: pir::plugins::InputAction::Handled,
                    text: None,
                    images: None,
                },
                (Mode::InputImages, "input") => PluginEventOutcome::Input {
                    action: pir::plugins::InputAction::Transform,
                    text: Some(text),
                    // No `mimeType`: the shared dispatcher must default it to
                    // `image/png` (the AgentSession contract print used to
                    // miss with its private copy).
                    images: Some(serde_json::json!([{ "data": "aGVsbG8=" }])),
                },
                (Mode::BeforeStart, "before_agent_start") => PluginEventOutcome::BeforeAgentStart {
                    system_prompt: Some("PRINT-SYSTEM-OVERRIDE".to_owned()),
                    message: None,
                    messages: vec![serde_json::json!({
                        "customType": "probe",
                        "content": [{ "type": "text", "text": "INJECTED-BY-HOOK" }],
                    })],
                },
                (Mode::TurnEndContinue, "turn_end") if call == 0 => {
                    PluginEventOutcome::Generic(serde_json::json!({ "continue": true }))
                }
                (Mode::SettleContinue, "agent_before_settle") if call == 0 => {
                    let mut entries = vec![
                        serde_json::json!({
                            "type": "custom",
                            "customType": "probe-draft",
                            "data": { "from": "agent_before_settle" },
                        }),
                        serde_json::json!({
                            "type": "custom_message",
                            "customType": "probe-message",
                            "content": [{ "type": "text", "text": "FROM-CUSTOM-MESSAGE" }],
                        }),
                    ];
                    if let Some(target) = &self.settle_target {
                        entries.push(serde_json::json!({
                            "type": "context_edit",
                            "targetId": target,
                            "replacement": [{
                                "type": "text",
                                "text": "EDITED-BY-HOOK",
                            }],
                        }));
                    }
                    PluginEventOutcome::Generic(serde_json::json!({
                        "continue": true,
                        "entries": entries,
                    }))
                }
                (Mode::Resources, "resources_discover") => PluginEventOutcome::Discover {
                    skill_paths: vec!["skills/one".to_owned()],
                    prompt_paths: vec!["prompts/two.md".to_owned()],
                    theme_paths: vec![],
                },
                (Mode::CompactCancel, "session_before_compact") => {
                    PluginEventOutcome::SessionBeforeCompact {
                        cancel: true,
                        compaction: None,
                    }
                }
                (Mode::CompactReplace, "session_before_compact") => {
                    PluginEventOutcome::SessionBeforeCompact {
                        cancel: false,
                        compaction: Some(serde_json::json!({
                            "summary": "EXTENSION COMPACTION SUMMARY",
                            "tokensBefore": 42,
                            "details": { "readFiles": ["a.rs"], "modifiedFiles": ["b.rs"] },
                        })),
                    }
                }
                _ => PluginEventOutcome::None,
            }
        })
    }
}

fn test_model(context_window: u64) -> Model {
    Model {
        id: "test-model".into(),
        name: "Test Model".into(),
        api: "openai-completions".into(),
        provider: "test".into(),
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
        context_window,
        max_tokens: 4_096,
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

fn assistant_text(text: &str, stop_reason: StopReason) -> AssistantMessage {
    AssistantMessage {
        api: "fake".into(),
        provider: "fake".into(),
        model: "test-model".into(),
        content: vec![ContentBlock::Text(TextContent {
            text: text.to_owned(),
            text_signature: None,
        })],
        stop_reason,
        timestamp: 1,
        ..AssistantMessage::default()
    }
}

fn assistant_tool_call(id: &str, name: &str) -> AssistantMessage {
    AssistantMessage {
        api: "fake".into(),
        provider: "fake".into(),
        model: "test-model".into(),
        content: vec![ContentBlock::ToolCall(ToolCall {
            id: id.to_owned(),
            name: name.to_owned(),
            arguments: BTreeMap::new(),
            thought_signature: None,
            namespace: None,
            arguments_error: None,
        })],
        stop_reason: StopReason::ToolUse,
        timestamp: 1,
        ..AssistantMessage::default()
    }
}

struct ProbeTool;

impl AgentTool for ProbeTool {
    fn name(&self) -> &'static str {
        "probe"
    }

    fn description(&self) -> &'static str {
        "print hook probe tool"
    }

    fn parameters(&self) -> Option<serde_json::Value> {
        Some(serde_json::json!({ "type": "object", "properties": {} }))
    }

    fn run(&self, _args: &serde_json::Value, _ctx: &ToolContext) -> BoxFuture<'static, ToolResult> {
        Box::pin(async {
            let mut result = ToolResult::ok("probe tool done");
            // Leave the turn on a tool result: `context_can_continue` is true,
            // so `turn_end {continue:true}` must force another request.
            result.terminate = true;
            result
        })
    }
}

fn scripted_stream(script: Vec<AssistantMessage>, requests: Arc<AtomicUsize>) -> StreamFn {
    Box::new(move |_request| {
        let index = requests.fetch_add(1, Ordering::SeqCst);
        let message = script
            .get(index)
            .cloned()
            .unwrap_or_else(|| assistant_text("script exhausted", StopReason::Stop));
        Box::pin(async move { message })
    })
}

fn in_memory_session(label: &str) -> SharedSession {
    let cwd = std::env::temp_dir().join(format!(
        "pir-print-hooks-session-{label}-{}",
        std::process::id()
    ));
    let manager =
        SessionManager::in_memory(&cwd.to_string_lossy(), None).expect("in-memory session");
    Arc::new(tokio::sync::Mutex::new(Some(manager)))
}

fn recording_sink(frames: Arc<Mutex<Vec<serde_json::Value>>>) -> PrintSessionEventSink {
    Arc::new(move |event| {
        frames
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(to_session_event_json(event));
    })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// Every positional prompt runs `input`, and a `handled` outcome consumes
/// only the prompt that produced it.
#[tokio::test(flavor = "current_thread")]
async fn print_runs_input_for_every_prompt_and_honors_handled() {
    let transform = ProbePlugin::new(Mode::InputTransform);
    let host = ProbePlugin::host(Arc::clone(&transform));

    let first = transform_print_input_parts(Some(&host), None, "prompt one", Vec::new()).await;
    let second = transform_print_input_parts(Some(&host), None, "prompt two", Vec::new()).await;
    assert_eq!(first.text, "prompt one [transformed]");
    assert_eq!(second.text, "prompt two [transformed]");
    assert_eq!(
        transform.event_count("input"),
        2,
        "one input dispatch per positional prompt"
    );

    let handled = ProbePlugin::new(Mode::InputHandled);
    let host = ProbePlugin::host(Arc::clone(&handled));
    let outcome = transform_print_input_parts(Some(&host), None, "swallow me", Vec::new()).await;
    assert!(outcome.handled);
    assert_eq!(handled.event_count("input"), 1);

    // `images` transform: the shared dispatcher keeps the block and defaults
    // a missing `mimeType` to `image/png`.
    let images = ProbePlugin::new(Mode::InputImages);
    let host = ProbePlugin::host(Arc::clone(&images));
    let outcome = transform_print_input_parts(Some(&host), None, "with image", Vec::new()).await;
    assert_eq!(outcome.text, "with image");
    assert_eq!(outcome.images.len(), 1);
    match &outcome.images[0] {
        ContentBlock::Image(image) => {
            assert_eq!(image.data, "aGVsbG8=");
            assert_eq!(image.mime_type, "image/png");
        }
        other => panic!("expected an image block, got {other:?}"),
    }
    assert_eq!(images.event_count("input"), 1);
}

/// The JS face reaches the same print seams: one JS extension's `input`,
/// `before_agent_start` and `resources_discover` handlers produce the results
/// the AgentSession/TUI host contracts define.
#[tokio::test(flavor = "current_thread")]
async fn print_js_extension_hooks_flow_through_the_shared_host() {
    let temp = TempDir::new("js-hooks");
    let source = r#"export default function (pi) {
        pi.on("input", (event) => ({ action: "transform", text: event.text + " [js]" }));
        pi.on("before_agent_start", () => ({ systemPrompt: "JS-SYSTEM" }));
        pi.on("resources_discover", () => ({ skillPaths: ["js-skill"] }));
    }"#;
    let path = temp.0.join("ext.ts");
    std::fs::write(&path, source).expect("write extension");
    let runtime = ExtensionHandle::start(ExtensionConfig {
        cwd: temp.0.clone(),
        mode: "print".to_owned(),
        has_ui: false,
        model: Some("test-model".to_owned()),
        session_id: Some("print-hook-js".to_owned()),
        system_prompt: String::new(),
        is_idle: true,
        limits: Some(Default::default()),
    })
    .expect("extension runtime");
    runtime
        .load(std::slice::from_ref(&path))
        .await
        .expect("load extension");
    let host = Arc::new(PluginHost::build(Some(runtime), Vec::new()));

    let input = transform_print_input_parts(Some(&host), None, "js prompt", Vec::new()).await;
    assert_eq!(input.text, "js prompt [js]");

    let before = dispatch_print_before_agent_start(
        Some(&host),
        "js prompt",
        &[],
        "ORIGINAL".to_owned(),
        serde_json::Value::Null,
    )
    .await;
    assert_eq!(before.system_prompt.as_deref(), Some("JS-SYSTEM"));

    let paths = dispatch_print_resources_discover(Some(&host), &temp.0, "startup")
        .await
        .expect("JS contributed a skill path");
    assert_eq!(paths.skill_paths, vec![temp.0.join("js-skill")]);
}

/// Actionable notifications (`pi.appendEntry`, `pi.setSessionName`) queued by
/// a handler are APPLIED by the print drain instead of being discarded with
/// the presentation class. `pi.sendMessage` is covered end to end by
/// [`print_run_wires_lifecycle_hooks_end_to_end`].
#[tokio::test(flavor = "current_thread")]
async fn print_applies_actionable_notifications() {
    let temp = TempDir::new("js-actions");
    let source = r#"export default function (pi) {
        pi.on("before_agent_start", () => {
            pi.appendEntry("probe-entry", { from: "before_agent_start" });
            pi.setSessionName("PRINT-SESSION-NAME");
        });
    }"#;
    let path = temp.0.join("ext.ts");
    std::fs::write(&path, source).expect("write extension");
    let runtime = ExtensionHandle::start(ExtensionConfig {
        cwd: temp.0.clone(),
        mode: "print".to_owned(),
        has_ui: false,
        model: Some("test-model".to_owned()),
        session_id: Some("print-actions".to_owned()),
        system_prompt: String::new(),
        is_idle: true,
        limits: Some(Default::default()),
    })
    .expect("extension runtime");
    runtime
        .load(std::slice::from_ref(&path))
        .await
        .expect("load extension");
    let host = Arc::new(PluginHost::build(Some(runtime), Vec::new()));
    let session = in_memory_session("actions");

    dispatch_print_before_agent_start(
        Some(&host),
        "hello",
        &[],
        "ORIGINAL".to_owned(),
        serde_json::Value::Null,
    )
    .await;
    let injected = drain_print_extension_notifications(
        Some(&host),
        None,
        &session,
        PrintNotificationPhase::RunStart,
        None,
    )
    .await;
    assert!(
        injected.is_empty(),
        "appendEntry/setSessionName are not run messages"
    );

    let guard = session.lock().await;
    let manager = guard.as_ref().expect("session");
    assert!(
        manager.entries().iter().any(|entry| {
            matches!(entry, FileEntry::Custom(custom) if custom.custom_type == "probe-entry")
        }),
        "pi.appendEntry must persist a custom entry"
    );
    assert_eq!(
        manager.session_name().as_deref(),
        Some("PRINT-SESSION-NAME"),
        "pi.setSessionName must persist the session name"
    );
}

/// `before_agent_start` reaches the print prompt through the same shared
/// adapter: system prompt replacement, message injection, and a null no-op
/// when nothing handles the event.
#[tokio::test(flavor = "current_thread")]
async fn print_before_agent_start_replaces_prompt_and_injects_messages() {
    let plugin = ProbePlugin::new(Mode::BeforeStart);
    let host = ProbePlugin::host(Arc::clone(&plugin));

    let outcome = dispatch_print_before_agent_start(
        Some(&host),
        "hello",
        &[],
        "ORIGINAL-SYSTEM".to_owned(),
        serde_json::Value::Null,
    )
    .await;
    assert_eq!(
        outcome.system_prompt.as_deref(),
        Some("PRINT-SYSTEM-OVERRIDE")
    );
    assert_eq!(outcome.injected_messages, vec!["INJECTED-BY-HOOK"]);
    assert_eq!(plugin.event_count("before_agent_start"), 1);

    // No handler: the caller keeps its own prompt, no dispatch result.
    let empty = Arc::new(PluginHost::build(None, Vec::new()));
    let untouched = dispatch_print_before_agent_start(
        Some(&empty),
        "hello",
        &[],
        "ORIGINAL-SYSTEM".to_owned(),
        serde_json::Value::Null,
    )
    .await;
    assert!(untouched.system_prompt.is_none());
    assert!(untouched.injected_messages.is_empty());
}

/// `turn_end` is the shared AgentSession boundary: one event per completed
/// turn, and `continue` forces the extra request the loop would otherwise
/// skip after a terminating tool result.
#[tokio::test(flavor = "current_thread")]
async fn print_turn_end_fires_per_turn_and_can_continue() {
    let plugin = ProbePlugin::new(Mode::TurnEndContinue);
    let host = ProbePlugin::host(Arc::clone(&plugin));
    let requests = Arc::new(AtomicUsize::new(0));

    let mut deps = print_loop_deps(
        Arc::new({
            let mut registry = ToolRegistry::new();
            registry.register(Arc::new(ProbeTool));
            registry
        }),
        ToolScheduler::new(ToolSchedulerConfig::default()),
        std::path::PathBuf::from("."),
        None,
        None,
        None,
        ThinkingLevel::Off,
    );
    deps.stream = scripted_stream(
        vec![
            assistant_tool_call("call-1", "probe"),
            assistant_text("after the continuation", StopReason::Stop),
        ],
        Arc::clone(&requests),
    );
    deps.finish_turn = print_turn_end_hook(
        Some(Arc::clone(&host)),
        in_memory_session("turn-end"),
        None,
        None,
    );

    let result = run_with_options(
        deps,
        Context {
            system_prompt: Some("sys".into()),
            messages: vec![user_message("start")],
            tools: None,
        },
        None,
        LoopOptions::default(),
    )
    .await;

    assert_eq!(
        requests.load(Ordering::SeqCst),
        2,
        "turn_end continue must force a request after the terminating tool result"
    );
    assert_eq!(result.turns, 2);
    assert_eq!(
        plugin.event_count("turn_end"),
        2,
        "one turn_end per completed turn"
    );

    // Without a handler the hook is not installed at all (no loop change).
    assert!(print_turn_end_hook(None, in_memory_session("no-handler"), None, None).is_none());
}

/// `agent_before_settle` uses the same shared boundary: `continue` is honored
/// only while the context can run, and returned entry drafts are committed to
/// the print session.
#[tokio::test(flavor = "current_thread")]
async fn print_agent_before_settle_continues_and_commits_drafts() {
    let plugin = ProbePlugin::new(Mode::SettleContinue);
    let host = ProbePlugin::host(Arc::clone(&plugin));
    let session = in_memory_session("settle");

    let context = Context {
        system_prompt: Some("sys".into()),
        messages: vec![
            user_message("start"),
            Message::ToolResult(pir::ai::types::ToolResultMessage {
                role: pir::ai::types::ToolResultRole::ToolResult,
                tool_call_id: "call-1".into(),
                tool_name: "probe".into(),
                content: vec![ContentBlock::Text(TextContent {
                    text: "done".into(),
                    text_signature: None,
                })],
                details: None,
                usage: None,
                added_tool_names: Vec::new(),
                structured_content: None,
                nested_calls: None,
                is_error: false,
                duration_ms: None,
                timestamp: 1,
            }),
        ],
        tools: None,
    };
    let terminal = assistant_text("terminal", StopReason::Stop);

    let continued = dispatch_print_agent_before_settle(
        Some(&host),
        Arc::clone(&session),
        &context,
        &terminal,
        false,
        None,
    )
    .await;
    assert!(continued, "the first handler call asks for a continuation");
    assert_eq!(plugin.event_count("agent_before_settle"), 1);

    let committed = {
        let guard = session.lock().await;
        guard
            .as_ref()
            .expect("session")
            .entries()
            .iter()
            .any(|entry| {
                matches!(
                    entry,
                    FileEntry::Custom(custom) if custom.custom_type == "probe-draft"
                )
            })
    };
    assert!(
        committed,
        "agent_before_settle entries draft must be persisted"
    );

    let again = dispatch_print_agent_before_settle(
        Some(&host),
        Arc::clone(&session),
        &context,
        &terminal,
        false,
        None,
    )
    .await;
    assert!(!again, "the second call has no continue decision");

    // No handler = no event and no behavior change.
    let empty = Arc::new(PluginHost::build(None, Vec::new()));
    assert!(
        !dispatch_print_agent_before_settle(
            Some(&empty),
            session,
            &context,
            &terminal,
            false,
            None,
        )
        .await
    );
}

/// `apply_extension_notifications`-style settlement: `custom_message` and
/// `context_edit` drafts are persisted by the boundary AND visible in the
/// session projection the next settle iteration consumes. This is exactly the
/// data path `run_print`'s continuation uses after P2-PRINT1-01.
#[tokio::test(flavor = "current_thread")]
async fn print_settle_drafts_project_into_the_next_context() {
    let session = in_memory_session("settle-projection");
    let (target_id, stored) = {
        let mut guard = session.lock().await;
        let manager = guard.as_mut().expect("session");
        let first = user_message("first question");
        let target_id = manager.append_message(first.clone()).expect("append");
        let answer = Message::Assistant(assistant_text("first answer", StopReason::Stop));
        manager.append_message(answer.clone()).expect("append");
        (target_id, vec![first, answer])
    };
    let plugin = ProbePlugin::with_settle_target(Mode::SettleContinue, target_id);
    let host = ProbePlugin::host(Arc::clone(&plugin));
    let context = Context {
        system_prompt: Some("sys".into()),
        messages: stored,
        tools: None,
    };
    let terminal = assistant_text("terminal", StopReason::Stop);
    assert!(
        dispatch_print_agent_before_settle(
            Some(&host),
            Arc::clone(&session),
            &context,
            &terminal,
            // The test context ends on an assistant message; `pending_messages`
            // is the upstream `canContinue` escape hatch for a handler that
            // wants a continuation anyway.
            true,
            None,
        )
        .await
    );

    let (projected, custom_message_persisted) = {
        let guard = session.lock().await;
        let manager = guard.as_ref().expect("session");
        (
            manager
                .build_session_projection()
                .expect("projection")
                .messages,
            manager.entries().iter().any(|entry| {
                matches!(
                    entry,
                    FileEntry::CustomMessage(custom)
                        if custom.custom_type == "probe-message"
                )
            }),
        )
    };
    assert!(
        custom_message_persisted,
        "custom_message draft must persist as a custom message entry"
    );
    let text = messages_text(&projected);
    assert!(
        text.contains("FROM-CUSTOM-MESSAGE"),
        "custom_message must project into the next context: {text}"
    );
    assert!(
        text.contains("EDITED-BY-HOOK"),
        "context_edit replacement must project into the next context: {text}"
    );
    assert!(
        !text.contains("first question"),
        "context_edit must replace the edited target: {text}"
    );
}

/// One FULL `run_print` invocation (real extension runtime, real loop,
/// deterministic provider): input transform, `before_agent_start` system
/// prompt + message injection, `pi.sendMessage` steering, and an
/// `agent_before_settle` continuation whose `custom_message` draft must be
/// re-projected into the second request. Deleting the `run_print` wiring (or
/// reverting the settle re-projection) makes this test fail.
#[tokio::test(flavor = "current_thread")]
async fn print_run_wires_lifecycle_hooks_end_to_end() {
    let temp = TempDir::new("run-print-e2e");
    let source = r#"export default function (pi) {
        pi.on("input", (event) => ({ action: "transform", text: event.text + " [input]" }));
        pi.on("tool_call", () => ({ block: true, terminate: true, reason: "probe" }));
        let settle = 0;
        pi.on("before_agent_start", () => {
            pi.sendMessage({ customType: "probe", content: "FROM-SEND-MESSAGE" });
            return {
                systemPrompt: "SYS-FROM-HOOK",
                message: {
                    customType: "probe",
                    content: [{ type: "text", text: "INJECTED-BY-BEFORE" }],
                },
            };
        });
        pi.on("agent_before_settle", () => {
            settle += 1;
            if (settle === 1) {
                return {
                    continue: true,
                    entries: [{
                        type: "custom_message",
                        customType: "probe",
                        content: "FROM-SETTLE",
                    }],
                };
            }
        });
    }"#;
    let extension = temp.0.join("ext.ts");
    std::fs::write(&extension, source).expect("write extension");

    let requests: Arc<Mutex<Vec<Context>>> = Arc::new(Mutex::new(Vec::new()));
    let model = test_model(128_000);
    let mut registry = ProviderRegistry::default();
    registry.set_provider(Arc::new(RecordingProvider {
        model: model.clone(),
        requests: Arc::clone(&requests),
    }));
    let environment = PiEnvironment {
        registry,
        models: vec![model.clone()],
        api_keys: BTreeMap::from([("test".to_owned(), "test-key".to_owned())]),
        settings: Settings::default(),
    };
    let session_dir = temp.0.join("sessions");
    std::fs::create_dir_all(&session_dir).expect("create sessions dir");
    let config = Config {
        session_dir: session_dir.to_string_lossy().into_owned(),
        ..Config::default()
    };
    let options = PrintOptions {
        prompt: "hi".to_owned(),
        additional_messages: Vec::new(),
        mode: OutputMode::Text,
        session: SessionPlan::New { id: None },
        no_context_files: true,
        cli_provider: Some("test".to_owned()),
        cli_model: Some("test-model".to_owned()),
        cli_thinking: None,
        api_key_override: Some("test-key".to_owned()),
        model_patterns: None,
        session_name: None,
        system_prompt_override: Some("BASE-SYSTEM".to_owned()),
        append_system_prompt: Vec::new(),
        settings_default_tools: None,
        cli_tools: Vec::new(),
        cli_exclude_tools: Vec::new(),
        no_tools: false,
        no_builtin_tools: false,
        is_continuing: false,
        resources: ResourcePathOptions {
            no_skills: true,
            no_prompt_templates: true,
            ..ResourcePathOptions::default()
        },
        images: Vec::new(),
    };

    let outcome = run_print_with_environment(
        options,
        &config,
        std::slice::from_ref(&extension),
        temp.0.clone(),
        environment,
    )
    .await
    .expect("run_print");
    assert_eq!(outcome.exit_code, 0);

    let requests = requests
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    assert_eq!(
        requests.len(),
        2,
        "agent_before_settle continue must issue exactly one more request"
    );
    let first = messages_text(&requests[0].messages);
    assert!(
        first.contains("hi [input]"),
        "input transform must reach the first request: {first}"
    );
    assert!(
        first.contains("INJECTED-BY-BEFORE"),
        "before_agent_start messages must reach the first request: {first}"
    );
    assert!(
        first.contains("FROM-SEND-MESSAGE"),
        "pi.sendMessage must steer into the preparing run: {first}"
    );
    assert_eq!(
        requests[0].system_prompt.as_deref(),
        Some("SYS-FROM-HOOK"),
        "before_agent_start systemPrompt must reach the first request"
    );
    let second = messages_text(&requests[1].messages);
    assert_eq!(
        requests[1].system_prompt.as_deref(),
        Some("SYS-FROM-HOOK"),
        "the settled continuation keeps the run's system prompt"
    );
    assert!(
        second.contains("FROM-SETTLE"),
        "agent_before_settle custom_message must be re-projected into the \
         continuation request: {second}"
    );
    assert!(
        second.contains("INJECTED-BY-BEFORE") && second.contains("FROM-SEND-MESSAGE"),
        "the preamble messages must persist and project into the continuation: {second}"
    );
}

/// `turn_end` must be wired by `run_print` itself (not only by tests that
/// install the hook manually): a handler `continue` forces the extra request
/// after a terminating tool result. Deleting `deps.finish_turn` in
/// `run_print` leaves only one provider request and turns this red.
#[tokio::test(flavor = "current_thread")]
async fn print_run_wires_turn_end_end_to_end() {
    let source = r#"export default function (pi) {
        let turns = 0;
        pi.on("tool_call", () => ({ block: true, terminate: true, reason: "probe" }));
        pi.on("turn_end", () => {
            turns += 1;
            if (turns === 1) return { continue: true };
        });
    }"#;
    let model = test_model(128_000);
    let provider = ScriptedProvider::new(
        model.clone(),
        vec![
            assistant_tool_call_with_args(
                "call-turn-end",
                "bash",
                BTreeMap::from([("command".into(), serde_json::json!("echo hi"))]),
            ),
            assistant_text("turn-end continuation answered", StopReason::Stop),
        ],
    );
    let requests = provider.requests();
    let (_temp, outcome) = run_print_e2e(RunPrintE2E {
        label: "run-turn-end",
        source,
        prompt: "start",
        additional_messages: Vec::new(),
        model,
        settings: Settings::default(),
        provider,
        files: &[],
    })
    .await;
    assert_eq!(outcome.exit_code, 0);
    let requests = requests
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    assert_eq!(
        requests.len(),
        2,
        "turn_end continue must force the second provider request through run_print"
    );
    let second = messages_text(&requests[1].messages);
    assert!(
        second.contains("start") && second.contains("probe"),
        "the continuation request must carry the transcript: {second}"
    );
}

/// `resources_discover` must be dispatched by `run_print` before `/skill:`
/// expansion. Removing that wiring leaves the literal `/skill:demo` prompt and
/// the contributed skill body never reaches the provider request.
#[tokio::test(flavor = "current_thread")]
async fn print_run_wires_resources_discover_end_to_end() {
    let source = r#"export default function (pi) {
        pi.on("resources_discover", () => ({ skillPaths: ["skills"] }));
    }"#;
    let model = test_model(128_000);
    let provider = ScriptedProvider::new(
        model.clone(),
        vec![assistant_text("skill answer", StopReason::Stop)],
    );
    let requests = provider.requests();
    let (_temp, outcome) = run_print_e2e(RunPrintE2E {
        label: "run-resources",
        source,
        prompt: "/skill:demo",
        additional_messages: Vec::new(),
        model,
        settings: Settings::default(),
        provider,
        files: &[(
            "skills/demo/SKILL.md",
            "---\nname: demo\ndescription: A skill contributed by resources_discover\n---\nRESOURCE-DISCOVER-SKILL-BODY\n",
        )],
    })
    .await;
    assert_eq!(outcome.exit_code, 0);
    let requests = requests
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    assert_eq!(requests.len(), 1);
    let first = messages_text(&requests[0].messages);
    assert!(
        first.contains("RESOURCE-DISCOVER-SKILL-BODY"),
        "the extension-contributed skill must expand into the first request: {first}"
    );
}

/// `session_start` must be dispatched by `run_print` and its actionable
/// `sendMessage` must join the first prompt. Deleting the startup dispatch or
/// the startup drain leaves the marker out of the first request.
#[tokio::test(flavor = "current_thread")]
async fn print_run_wires_session_start_end_to_end() {
    let source = r#"export default function (pi) {
        pi.on("session_start", () => {
            pi.sendMessage({ customType: "probe", content: "FROM-SESSION-START" });
        });
    }"#;
    let model = test_model(128_000);
    let provider = ScriptedProvider::new(
        model.clone(),
        vec![assistant_text("session-start answer", StopReason::Stop)],
    );
    let requests = provider.requests();
    let (_temp, outcome) = run_print_e2e(RunPrintE2E {
        label: "run-session-start",
        source,
        prompt: "hello",
        additional_messages: Vec::new(),
        model,
        settings: Settings::default(),
        provider,
        files: &[],
    })
    .await;
    assert_eq!(outcome.exit_code, 0);
    let requests = requests
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    assert_eq!(requests.len(), 1);
    let first = messages_text(&requests[0].messages);
    assert!(
        first.contains("FROM-SESSION-START"),
        "the session_start sendMessage must reach the first request: {first}"
    );
}

/// `session_before_compact` / `session_compact` must be wired by `run_print`:
/// the threshold hook replaces the context with the handler's summary and the
/// success event handler's `sendMessage` persists for the NEXT positional
/// prompt. Deleting the compaction hook wiring loses the summary; deleting
/// the `session_compact` emitter loses the marker in the second prompt.
#[tokio::test(flavor = "current_thread")]
async fn print_run_wires_compaction_end_to_end() {
    let source = r#"export default function (pi) {
        pi.on("session_before_compact", () => ({
            compaction: {
                summary: "COMPACTED-BY-HOOK",
                tokensBefore: 90000,
                details: { readFiles: [], modifiedFiles: [] },
            },
        }));
        pi.on("session_compact", () => {
            pi.sendMessage({ customType: "probe", content: "SESSION-COMPACT-EVENT" });
        });
    }"#;
    let model = test_model(50_000);
    let provider = ScriptedProvider::new(
        model.clone(),
        vec![
            // The first prompt's response carries the usage that puts the
            // NEXT prompt's projected context over the threshold.
            with_usage(assistant_text("first answer", StopReason::Stop), 90_000),
            assistant_text("after compaction", StopReason::Stop),
            assistant_text("third prompt answer", StopReason::Stop),
        ],
    );
    let requests = provider.requests();
    let settings = Settings {
        compaction: Some(CompactionSettings {
            enabled: Some(true),
            reserve_tokens: Some(1),
            // Keep only the newest message so the tiny transcript still has a
            // cut point (the production default would keep everything verbatim).
            keep_recent_tokens: Some(1),
            model_overrides: None,
        }),
        ..Settings::default()
    };
    let (_temp, outcome) = run_print_e2e(RunPrintE2E {
        label: "run-compaction",
        source,
        prompt: "first prompt",
        additional_messages: vec!["second prompt".to_owned(), "third prompt".to_owned()],
        model,
        settings,
        provider,
        files: &[],
    })
    .await;
    assert_eq!(outcome.exit_code, 0);
    let requests = requests
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    assert_eq!(
        requests.len(),
        3,
        "one provider request per positional prompt (the second is compacted first)"
    );
    let compacted = messages_text(&requests[1].messages);
    assert!(
        compacted.contains("COMPACTED-BY-HOOK"),
        "the session_before_compact replacement must build the second prompt's context: {compacted}"
    );
    let third_prompt = messages_text(&requests[2].messages);
    assert!(
        third_prompt.contains("SESSION-COMPACT-EVENT"),
        "the session_compact handler's sendMessage must persist into the next prompt: {third_prompt}"
    );
}

/// An extension `ctx.abort()` in `before_agent_start` aborts ONLY the current
/// run: the run settles through the normal drain, emits `agent_settled`
/// exactly once, returns immediately (no fixed 5 s termination sleep), and the
/// next positional prompt still executes with a fresh abort signal.
#[tokio::test(flavor = "current_thread")]
async fn print_run_extension_abort_settles_and_continues_end_to_end() {
    let source = r#"export default function (pi) {
        let settled = 0;
        pi.on("before_agent_start", (event, ctx) => {
            if (event.prompt.includes("ABORT-ME")) ctx.abort();
        });
        pi.on("agent_settled", () => {
            settled += 1;
            pi.sendMessage({ customType: "probe", content: "SETTLED-" + settled });
        });
    }"#;
    let model = test_model(128_000);
    let provider = ScriptedProvider::new(
        model.clone(),
        vec![assistant_text("second prompt answer", StopReason::Stop)],
    );
    let requests = provider.requests();
    let started = std::time::Instant::now();
    let (_temp, outcome) = run_print_e2e(RunPrintE2E {
        label: "run-extension-abort",
        source,
        prompt: "ABORT-ME",
        additional_messages: vec!["SECOND".to_owned()],
        model,
        settings: Settings::default(),
        provider,
        files: &[],
    })
    .await;
    let elapsed = started.elapsed();
    assert_eq!(outcome.exit_code, 0);
    assert!(
        elapsed < std::time::Duration::from_secs(4),
        "an extension abort must not take the old 5 s termination park (took {elapsed:?})"
    );
    let requests = requests
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    assert_eq!(
        requests.len(),
        1,
        "the aborted first run must issue no provider request; the second prompt must issue one"
    );
    let second = messages_text(&requests[0].messages);
    assert!(
        second.contains("SECOND"),
        "the next positional prompt must still execute: {second}"
    );
    assert!(
        second.contains("SETTLED-1"),
        "the aborted run must emit exactly one agent_settled: {second}"
    );
    assert!(
        !second.contains("SETTLED-2"),
        "agent_settled must not be emitted twice for one run: {second}"
    );
}

struct RecordingProvider {
    model: Model,
    requests: Arc<Mutex<Vec<Context>>>,
}

impl Provider for RecordingProvider {
    #[allow(clippy::misnamed_getters)]
    fn id(&self) -> &str {
        &self.model.provider
    }

    fn name(&self) -> &str {
        "recording provider"
    }

    fn get_models(&self) -> Vec<Model> {
        vec![self.model.clone()]
    }

    fn api_key_auth(&self) -> Vec<ApiKeyAuth> {
        vec![ApiKeyAuth::new(
            "recording provider key",
            vec!["PIR_TEST_PRINT_HOOKS_KEY".to_owned()],
        )]
    }

    fn stream(
        &self,
        _model: &Model,
        context: Context,
        _options: StreamOptions,
    ) -> AssistantMessageEventStream {
        let index = {
            let mut requests = self
                .requests
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let index = requests.len();
            requests.push(context);
            index
        };
        let message = if index == 0 {
            // Leave the run on a terminating tool RESULT so the
            // `agent_before_settle` boundary can continue (upstream
            // `canContinue` accepts a final non-assistant context). The JS
            // `tool_call` handler blocks it and sets `terminate`.
            assistant_tool_call_with_args(
                "call-1",
                "bash",
                BTreeMap::from([("command".into(), serde_json::json!("echo hi"))]),
            )
        } else {
            assistant_text("done", StopReason::Stop)
        };
        let stream = AssistantMessageEventStream::new();
        stream.push(AssistantMessageEventStream::done_event(
            TerminalReason::Stop,
            message,
        ));
        stream
    }
}

fn assistant_tool_call_with_args(
    id: &str,
    name: &str,
    arguments: BTreeMap<String, serde_json::Value>,
) -> AssistantMessage {
    let mut message = assistant_tool_call(id, name);
    if let Some(ContentBlock::ToolCall(call)) = message.content.first_mut() {
        call.arguments = arguments;
    }
    message
}

fn with_usage(mut message: AssistantMessage, total_tokens: u64) -> AssistantMessage {
    message.usage = Usage {
        input: total_tokens,
        total_tokens,
        ..Usage::default()
    };
    // The estimator's usage guard requires the assistant timestamp to be at
    // least the newest timestamp seen so far; scripted messages are built
    // before the run's user messages (which use `now_ms()`), so pin a value
    // newer than any wall clock.
    message.timestamp = u64::MAX;
    message
}

/// Deterministic provider for the `run_print`-level wiring tests: records
/// every request context and answers with the next scripted assistant message.
struct ScriptedProvider {
    model: Model,
    requests: Arc<Mutex<Vec<Context>>>,
    script: Mutex<Vec<AssistantMessage>>,
}

impl ScriptedProvider {
    fn new(model: Model, script: Vec<AssistantMessage>) -> Arc<Self> {
        Arc::new(Self {
            model,
            requests: Arc::new(Mutex::new(Vec::new())),
            script: Mutex::new(script),
        })
    }

    fn requests(&self) -> Arc<Mutex<Vec<Context>>> {
        Arc::clone(&self.requests)
    }
}

impl Provider for ScriptedProvider {
    #[allow(clippy::misnamed_getters)]
    fn id(&self) -> &str {
        &self.model.provider
    }

    fn name(&self) -> &str {
        "scripted provider"
    }

    fn get_models(&self) -> Vec<Model> {
        vec![self.model.clone()]
    }

    fn api_key_auth(&self) -> Vec<ApiKeyAuth> {
        vec![ApiKeyAuth::new(
            "scripted provider key",
            vec!["PIR_TEST_PRINT_HOOKS_KEY".to_owned()],
        )]
    }

    fn stream(
        &self,
        _model: &Model,
        context: Context,
        _options: StreamOptions,
    ) -> AssistantMessageEventStream {
        self.requests
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(context);
        let message = {
            let mut script = self
                .script
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if script.is_empty() {
                assistant_text("script exhausted", StopReason::Stop)
            } else {
                script.remove(0)
            }
        };
        let stream = AssistantMessageEventStream::new();
        stream.push(AssistantMessageEventStream::done_event(
            TerminalReason::Stop,
            message,
        ));
        stream
    }
}

/// Named inputs for [`run_print_e2e`].
struct RunPrintE2E<'a> {
    label: &'a str,
    source: &'a str,
    prompt: &'a str,
    additional_messages: Vec<String>,
    model: Model,
    settings: Settings,
    provider: Arc<dyn Provider>,
    files: &'a [(&'a str, &'a str)],
}

/// Run the REAL `run_print` pipeline (real extension runtime, real loop,
/// deterministic provider) over one invocation. `files` are written under the
/// temp cwd before the extension loads (resources/skill fixtures).
async fn run_print_e2e(input: RunPrintE2E<'_>) -> (TempDir, PrintOutcome) {
    let RunPrintE2E {
        label,
        source,
        prompt,
        additional_messages,
        model,
        settings,
        provider,
        files,
    } = input;
    let temp = TempDir::new(label);
    for (relative, contents) in files {
        let path = temp.0.join(relative);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create fixture dir");
        }
        std::fs::write(path, contents).expect("write fixture");
    }
    let extension = temp.0.join("ext.ts");
    std::fs::write(&extension, source).expect("write extension");
    let mut registry = ProviderRegistry::default();
    registry.set_provider(provider);
    let environment = PiEnvironment {
        registry,
        models: vec![model],
        api_keys: BTreeMap::from([("test".to_owned(), "test-key".to_owned())]),
        settings,
    };
    let session_dir = temp.0.join("sessions");
    std::fs::create_dir_all(&session_dir).expect("create sessions dir");
    let config = Config {
        session_dir: session_dir.to_string_lossy().into_owned(),
        ..Config::default()
    };
    let options = PrintOptions {
        prompt: prompt.to_owned(),
        additional_messages,
        mode: OutputMode::Text,
        session: SessionPlan::New { id: None },
        no_context_files: true,
        cli_provider: Some("test".to_owned()),
        cli_model: Some("test-model".to_owned()),
        cli_thinking: None,
        api_key_override: Some("test-key".to_owned()),
        model_patterns: None,
        session_name: None,
        system_prompt_override: Some("BASE-SYSTEM".to_owned()),
        append_system_prompt: Vec::new(),
        settings_default_tools: None,
        cli_tools: Vec::new(),
        cli_exclude_tools: Vec::new(),
        no_tools: false,
        no_builtin_tools: false,
        is_continuing: false,
        resources: ResourcePathOptions {
            no_skills: true,
            no_prompt_templates: true,
            ..ResourcePathOptions::default()
        },
        images: Vec::new(),
    };
    let outcome = run_print_with_environment(
        options,
        &config,
        std::slice::from_ref(&extension),
        temp.0.clone(),
        environment,
    )
    .await
    .expect("run_print");
    (temp, outcome)
}

fn messages_text(messages: &[Message]) -> String {
    let mut text = String::new();
    for message in messages {
        let blocks: &[ContentBlock] = match message {
            Message::User(user) | Message::System(user) | Message::Developer(user) => &user.content,
            Message::Assistant(assistant) => &assistant.content,
            Message::ToolResult(result) => &result.content,
        };
        for block in blocks {
            match block {
                ContentBlock::Text(block) => {
                    text.push_str(&block.text);
                    text.push('\n');
                }
                ContentBlock::Image(_) => text.push_str("[image]\n"),
                _ => {}
            }
        }
    }
    text
}

/// `resources_discover` reaches the print host and returns its contributed
/// paths; a no-handler host contributes nothing.
#[tokio::test(flavor = "current_thread")]
async fn print_resources_discover_returns_contributed_paths() {
    let temp = TempDir::new("resources");
    let plugin = ProbePlugin::new(Mode::Resources);
    let host = ProbePlugin::host(Arc::clone(&plugin));

    let paths = dispatch_print_resources_discover(Some(&host), &temp.0, "startup")
        .await
        .expect("contributed paths");
    assert_eq!(paths.skill_paths, vec![temp.0.join("skills/one")]);
    assert_eq!(paths.prompt_paths, vec![temp.0.join("prompts/two.md")]);
    assert_eq!(plugin.event_count("resources_discover"), 1);

    let empty = Arc::new(PluginHost::build(None, Vec::new()));
    assert!(
        dispatch_print_resources_discover(Some(&empty), &temp.0, "startup")
            .await
            .is_none()
    );
}

async fn compaction_fixture(
    mode: Mode,
    label: &str,
) -> (
    SharedSession,
    Arc<ProbePlugin>,
    Arc<PluginHost>,
    Arc<Context>,
    Arc<Context>,
    Vec<serde_json::Value>,
) {
    let session = in_memory_session(label);
    let messages = {
        let mut guard = session.lock().await;
        let manager = guard.as_mut().expect("session");
        let messages = vec![
            user_message("first question"),
            Message::Assistant(assistant_text("first answer", StopReason::Stop)),
            user_message("second question"),
            Message::Assistant(assistant_text("second answer", StopReason::Stop)),
        ];
        for message in &messages {
            manager.append_message(message.clone()).expect("append");
        }
        messages
    };
    let plugin = ProbePlugin::new(mode);
    let host = ProbePlugin::host(Arc::clone(&plugin));
    let frames: Arc<Mutex<Vec<serde_json::Value>>> = Arc::new(Mutex::new(Vec::new()));
    let model = test_model(1);
    let hook = print_compaction_hook(pir::coding_agent::print::PrintCompactionInputs {
        config: CompactionConfig {
            enabled: true,
            reserve_tokens: 1,
            keep_recent_tokens: 1,
        },
        model,
        registry: Arc::new(pir::ai::provider::ProviderRegistry::default()),
        api_key: None,
        thinking_level: ThinkingLevel::Off,
        hooks: ProviderStreamHooks::default(),
        session: Arc::clone(&session),
        projected: None,
        plugins: Some(Arc::clone(&host)),
        runtime: None,
        session_events: Some(recording_sink(Arc::clone(&frames))),
    });
    let context = Arc::new(Context {
        system_prompt: Some("sys".into()),
        messages,
        tools: None,
    });
    let result = hook(Arc::clone(&context)).await;
    let frames = frames
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    (session, plugin, host, context, result, frames)
}

/// `session_before_compact {cancel:true}` aborts the threshold run: the
/// context is unchanged, no compaction entry is written, and the aborted
/// end/failed pair is emitted.
#[tokio::test(flavor = "current_thread")]
async fn print_compaction_can_be_cancelled_by_extension() {
    let (session, plugin, _host, context, result, frames) =
        compaction_fixture(Mode::CompactCancel, "compact-cancel").await;
    assert!(
        Arc::ptr_eq(&context, &result),
        "a cancelled compaction must return the unchanged context"
    );

    assert_eq!(
        plugin.event_count("session_before_compact"),
        1,
        "the handler must observe the threshold run"
    );
    assert_eq!(plugin.event_count("session_compact_failed"), 1);
    assert_eq!(
        frames.len(),
        2,
        "compaction_start + aborted compaction_end: {frames:?}"
    );
    assert_eq!(frames[0]["type"], serde_json::json!("compaction_start"));
    assert_eq!(frames[1]["type"], serde_json::json!("compaction_end"));
    assert_eq!(frames[1]["aborted"], serde_json::json!(true));
    assert!(frames[1].get("errorMessage").is_none());
    let stored = {
        let guard = session.lock().await;
        guard
            .as_ref()
            .expect("session")
            .entries()
            .iter()
            .filter(|entry| matches!(entry, FileEntry::Compaction(_)))
            .count()
    };
    assert_eq!(stored, 0, "cancelled compaction must persist nothing");
}

/// P2-S8-4 print residual: the threshold summarizer must resolve through a
/// session-scoped provider lookup. A child/sibling session sharing the
/// registry may own a shared dynamic entry under the same provider id; before
/// the fix that entry captured print's summary request. The static provider
/// must answer instead, and the foreign provider must never be called.
#[tokio::test(flavor = "current_thread")]
async fn print_compaction_summary_ignores_another_sessions_shared_provider() {
    let session = in_memory_session("print-session-scope");
    let messages = {
        let mut guard = session.lock().await;
        let manager = guard.as_mut().expect("session");
        let messages = vec![
            user_message("first question"),
            Message::Assistant(assistant_text("first answer", StopReason::Stop)),
            user_message("second question"),
            Message::Assistant(assistant_text("second answer", StopReason::Stop)),
        ];
        for message in &messages {
            manager.append_message(message.clone()).expect("append");
        }
        messages
    };
    let model = test_model(1);
    let static_provider = ScriptedProvider::new(
        model.clone(),
        (0..8)
            .map(|_| assistant_text("STATIC_SUMMARY", StopReason::Stop))
            .collect(),
    );
    let mut registry = ProviderRegistry::default();
    registry.set_provider(static_provider.clone());
    let registry = Arc::new(registry);

    // A different LIVE session owns the shared dynamic entry for `test`.
    let foreign = registry.standalone_provider_session();
    let foreign_provider = ScriptedProvider::new(
        model.clone(),
        vec![assistant_text("FOREIGN_SUMMARY", StopReason::Stop)],
    );
    registry.register_session_owned_dynamic_provider_for_test(
        foreign.key(),
        "test",
        foreign_provider.clone(),
    );

    let hook = print_compaction_hook(pir::coding_agent::print::PrintCompactionInputs {
        config: CompactionConfig {
            enabled: true,
            reserve_tokens: 1,
            keep_recent_tokens: 1,
        },
        model,
        registry: Arc::clone(&registry),
        api_key: None,
        thinking_level: ThinkingLevel::Off,
        hooks: ProviderStreamHooks::default(),
        session: Arc::clone(&session),
        projected: None,
        plugins: None,
        runtime: None,
        session_events: None,
    });
    let context = Arc::new(Context {
        system_prompt: Some("sys".into()),
        messages,
        tools: None,
    });
    let result = hook(Arc::clone(&context)).await;
    let summary_text: String = result
        .messages
        .iter()
        .filter_map(|message| match message {
            Message::Assistant(assistant) => Some(
                assistant
                    .content
                    .iter()
                    .filter_map(|block| match block {
                        ContentBlock::Text(text) => Some(text.text.clone()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n"),
            ),
            Message::User(user) => Some(
                user.content
                    .iter()
                    .filter_map(|block| match block {
                        ContentBlock::Text(text) => Some(text.text.clone()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n"),
            ),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        summary_text.contains("STATIC_SUMMARY"),
        "the static provider must answer the print summary: {summary_text}"
    );
    assert!(
        !summary_text.contains("FOREIGN_SUMMARY"),
        "another session's shared provider must not answer print's summary: {summary_text}"
    );
    assert!(
        !static_provider
            .requests()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_empty(),
        "the static provider must have served the summarization request"
    );
    assert_eq!(
        foreign_provider
            .requests()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len(),
        0,
        "the foreign session's provider must never see print's request"
    );
}

/// `session_before_compact` replacement is used verbatim, persisted with
/// `fromHook`, and emits the success `session_compact`.
#[tokio::test(flavor = "current_thread")]
async fn print_compaction_can_be_replaced_by_extension() {
    let (session, plugin, _host, _original, result, frames) =
        compaction_fixture(Mode::CompactReplace, "compact-replace").await;

    assert_eq!(plugin.event_count("session_before_compact"), 1);
    assert_eq!(plugin.event_count("session_compact"), 1);
    assert_eq!(
        frames.len(),
        2,
        "compaction_start + successful compaction_end: {frames:?}"
    );
    assert_eq!(frames[1]["type"], serde_json::json!("compaction_end"));
    assert_eq!(frames[1]["aborted"], serde_json::json!(false));
    assert_eq!(
        frames[1]["result"]["summary"],
        serde_json::json!("EXTENSION COMPACTION SUMMARY")
    );
    let first = match &result.messages[0] {
        Message::User(user) => user
            .content
            .iter()
            .filter_map(|block| match block {
                ContentBlock::Text(text) => Some(text.text.as_str()),
                _ => None,
            })
            .collect::<String>(),
        _ => String::new(),
    };
    assert!(
        first.contains("EXTENSION COMPACTION SUMMARY"),
        "the rebuilt context must start with the replacement summary: {result:?}"
    );
    let (stored, from_hook) = {
        let guard = session.lock().await;
        let manager = guard.as_ref().expect("session");
        let entry = manager
            .entries()
            .iter()
            .find_map(|entry| match entry {
                FileEntry::Compaction(compaction) => {
                    Some((compaction.summary.clone(), compaction.from_hook))
                }
                _ => None,
            })
            .expect("replacement compaction persisted");
        entry
    };
    assert_eq!(stored, "EXTENSION COMPACTION SUMMARY");
    assert_eq!(from_hook, Some(true));
}

/// A handler-less compaction hook is untouched: below the threshold it
/// returns the SAME context Arc and emits nothing.
#[tokio::test(flavor = "current_thread")]
async fn print_compaction_without_handlers_keeps_the_unchanged_context() {
    let session = in_memory_session("compact-no-handler");
    let model = test_model(1_000_000);
    let hook = print_compaction_hook(pir::coding_agent::print::PrintCompactionInputs {
        config: CompactionConfig {
            enabled: true,
            reserve_tokens: 1_000,
            keep_recent_tokens: 1,
        },
        model,
        registry: Arc::new(pir::ai::provider::ProviderRegistry::default()),
        api_key: None,
        thinking_level: ThinkingLevel::Off,
        hooks: ProviderStreamHooks::default(),
        session,
        projected: None,
        plugins: None,
        runtime: None,
        session_events: None,
    });
    let context = Arc::new(Context {
        system_prompt: Some("sys".into()),
        messages: vec![user_message("only a short prompt")],
        tools: None,
    });
    let result = hook(Arc::clone(&context)).await;
    assert!(Arc::ptr_eq(&context, &result));
}

// ---------------------------------------------------------------------------
// P2-06: print applies plugin providers / virtual models
// ---------------------------------------------------------------------------

const PLUGIN_PROVIDER_TS: &str = r#"
export default function (pi) {
  const usage = {
    input: 1, output: 1, cacheRead: 0, cacheWrite: 0, totalTokens: 2,
    cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, total: 0 },
  };
  const done = (model) => ({
    type: "done", reason: "stop",
    message: {
      role: "assistant",
      content: [{ type: "text", text: "PLUGIN-OK" }],
      api: model.api, provider: model.provider, model: model.id,
      usage, stopReason: "stop", timestamp: 1,
    },
  });
  pi.registerProvider("plug", {
    baseUrl: "http://plugin.invalid/v1",
    api: "openai-completions",
    apiKey: "plug-key",
    models: [{
      id: "plug-model",
      name: "Plug model",
      contextWindow: 128000,
      maxTokens: 4096,
      reasoning: false,
      input: ["text"],
      cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0 },
    }],
    streamSimple: (model) => [
      { type: "text_delta", content_index: 0, delta: "PLUGIN-OK" },
      done(model),
    ],
  });
  pi.registerVirtualModel({
    provider: "plug-virtual",
    id: "virt-model",
    name: "Virtual plug model",
    physical: { provider: "plug", id: "plug-model" },
  });
}
"#;

#[tokio::test(flavor = "current_thread")]
async fn print_syncs_plugin_provider_and_virtual_model_defs() {
    let temp = TempDir::new("plugin-provider-sync");
    let extension = temp.0.join("provider.ts");
    std::fs::write(&extension, PLUGIN_PROVIDER_TS).expect("write extension");
    let handle = ExtensionHandle::start(ExtensionConfig {
        cwd: temp.0.clone(),
        mode: "print".to_string(),
        has_ui: false,
        ..Default::default()
    })
    .expect("start runtime");
    handle
        .load(std::slice::from_ref(&extension))
        .await
        .expect("load extension");
    let host = print_plugin_host(Some(&handle)).expect("plugin host");
    let registry = ProviderRegistry::default();
    sync_print_plugin_providers(&host, &registry);

    assert!(
        registry.get_model("plug", "plug-model").is_some(),
        "the print provider registry must carry the plugin provider's model"
    );
    assert!(
        registry.get_model("plug-virtual", "virt-model").is_some(),
        "the print provider registry must carry the plugin virtual model"
    );
}

/// End-to-end: with a settings default pointing at a plugin provider, print
/// resolves that provider from the merged registry (not the pre-extension
/// environment list) and streams through its JS `streamSimple` callback.
#[tokio::test(flavor = "current_thread")]
async fn print_run_selects_and_uses_a_plugin_provider_model() {
    let temp = TempDir::new("plugin-provider-e2e");
    let extension = temp.0.join("provider.ts");
    std::fs::write(&extension, PLUGIN_PROVIDER_TS).expect("write extension");

    let requests: Arc<Mutex<Vec<Context>>> = Arc::new(Mutex::new(Vec::new()));
    let model = test_model(128_000);
    let mut registry = ProviderRegistry::default();
    registry.set_provider(Arc::new(RecordingProvider {
        model: model.clone(),
        requests: Arc::clone(&requests),
    }));
    let settings = Settings {
        default_provider: Some("plug".to_owned()),
        default_model: Some("plug-model".to_owned()),
        ..Settings::default()
    };
    let environment = PiEnvironment {
        registry,
        models: vec![model],
        api_keys: BTreeMap::from([
            ("test".to_owned(), "test-key".to_owned()),
            ("plug".to_owned(), "plug-key".to_owned()),
        ]),
        settings,
    };
    let session_dir = temp.0.join("sessions");
    std::fs::create_dir_all(&session_dir).expect("create sessions dir");
    let config = Config {
        session_dir: session_dir.to_string_lossy().into_owned(),
        ..Config::default()
    };
    let options = PrintOptions {
        prompt: "hi".to_owned(),
        additional_messages: Vec::new(),
        mode: OutputMode::Text,
        session: SessionPlan::New { id: None },
        no_context_files: true,
        cli_provider: None,
        cli_model: None,
        cli_thinking: None,
        api_key_override: None,
        model_patterns: None,
        session_name: None,
        system_prompt_override: Some("BASE-SYSTEM".to_owned()),
        append_system_prompt: Vec::new(),
        settings_default_tools: None,
        cli_tools: Vec::new(),
        cli_exclude_tools: Vec::new(),
        no_tools: false,
        no_builtin_tools: false,
        is_continuing: false,
        resources: ResourcePathOptions {
            no_skills: true,
            no_prompt_templates: true,
            ..ResourcePathOptions::default()
        },
        images: Vec::new(),
    };

    let outcome = run_print_with_environment(
        options,
        &config,
        std::slice::from_ref(&extension),
        temp.0.clone(),
        environment,
    )
    .await
    .expect("run_print");
    assert_eq!(outcome.exit_code, 0);
    assert!(
        outcome.text.contains("PLUGIN-OK"),
        "print must select the plugin provider model and use its streamSimple callback: {:?}",
        outcome.text
    );
    assert!(
        requests.lock().unwrap().is_empty(),
        "the pre-extension test provider must not receive the request"
    );
}
