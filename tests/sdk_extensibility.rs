//! SDK extensibility-surface tests (extensibility-audit items #2/#5/#6/#7):
//!
//! 1. tool selection (`tools` / `exclude_tools` / `no_tools`) becomes the
//!    session's active override exactly like the CLI flags (shared
//!    `resolve_tool_selection`) — it never filters the registry, so an
//!    unselected tool stays registered but undeclared — and the default (no
//!    selection) keeps every registered tool callable;
//! 2. `on_event` closure plugins join the unified plugin dispatch in
//!    registration order and can observe and transform dispatch events;
//! 3. `prompt_stream_with` filters streamed events while `prompt_stream`
//!    stays pass-all;
//! 4. `Agent::set_model` / `set_thinking_level` switch the session state, and
//!    the session-layer `model_select` dispatch reaches an SDK `on_event`
//!    closure.
//!
//! Fixtures follow the pure in-memory `FakeProvider` pattern of
//! `tests/image_classifier_models.rs` (no network, no tracked files).

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use pir::agent::tool_batch::ToolResult;
use pir::ai::auth::ApiKeyAuth;
use pir::ai::event_stream::AssistantMessageEventStream;
use pir::ai::provider::{Provider, ProviderRegistry};
use pir::ai::types::{
    AssistantMessage, AssistantMessageEvent, ContentBlock, Context, Message, Model, ModelCost,
    ModelCostRates, StopReason, StreamOptions, TerminalReason, TextContent, ThinkingLevel,
    ToolCall,
};
use pir::extensions::ExtensionHandle;
use pir::plugins::{AgentPlugin, InputAction, PluginEventOutcome, PluginInfo};
use pir::sdk::{Agent, AgentBuilder, SdkEvent};

use serde_json::{json, Value as JsonValue};

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

/// A minimal model served by the scripted provider.
fn fake_model(id: &str, reasoning: bool) -> Model {
    Model {
        id: id.to_string(),
        name: id.to_string(),
        api: "openai-completions".to_string(),
        provider: "fake".to_string(),
        base_url: "http://fake.invalid/v1".to_string(),
        reasoning,
        thinking_level_map: None,
        input: vec!["text".to_string()],
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

fn assistant_text(text: &str) -> AssistantMessage {
    AssistantMessage {
        api: "fake".into(),
        provider: "fake".into(),
        model: "model-a".into(),
        content: vec![ContentBlock::Text(TextContent {
            text: text.to_owned(),
            text_signature: None,
        })],
        stop_reason: StopReason::Stop,
        timestamp: 1,
        ..Default::default()
    }
}

fn assistant_tool_call(
    id: &str,
    name: &str,
    arguments: BTreeMap<String, JsonValue>,
) -> AssistantMessage {
    AssistantMessage {
        api: "fake".into(),
        provider: "fake".into(),
        model: "model-a".into(),
        content: vec![ContentBlock::ToolCall(ToolCall {
            id: id.to_owned(),
            name: name.to_owned(),
            arguments,
            thought_signature: None,
            namespace: None,
            arguments_error: None,
        })],
        stop_reason: StopReason::ToolUse,
        timestamp: 1,
        ..Default::default()
    }
}

fn arguments(pairs: &[(&str, JsonValue)]) -> BTreeMap<String, JsonValue> {
    pairs
        .iter()
        .map(|(key, value)| ((*key).to_owned(), value.clone()))
        .collect()
}

type Responder = Arc<dyn Fn(usize, &Context) -> AssistantMessage + Send + Sync>;

/// A provider that answers the Nth model request with the Nth scripted
/// message (repeating the last one when the script is exhausted) and records
/// the prompt text of every request it sees.
struct ScriptedProvider {
    calls: AtomicUsize,
    respond: Responder,
    seen_prompts: Arc<Mutex<Vec<String>>>,
}

impl ScriptedProvider {
    fn new(script: Vec<AssistantMessage>) -> Self {
        let script = Arc::new(script);
        Self::with_responder(Arc::new(move |index, _context| {
            script
                .get(index)
                .cloned()
                .unwrap_or_else(|| assistant_text("done"))
        }))
    }

    fn with_responder(respond: Responder) -> Self {
        Self {
            calls: AtomicUsize::new(0),
            respond,
            seen_prompts: Arc::new(Mutex::new(Vec::new())),
        }
    }
}

impl Provider for ScriptedProvider {
    fn id(&self) -> &str {
        "fake"
    }

    fn name(&self) -> &str {
        "Scripted provider"
    }

    fn get_models(&self) -> Vec<Model> {
        vec![fake_model("model-a", false), fake_model("model-b", true)]
    }

    fn stream(
        &self,
        _model: &Model,
        context: Context,
        _options: StreamOptions,
    ) -> AssistantMessageEventStream {
        let index = self.calls.fetch_add(1, Ordering::SeqCst);
        // Record the prompt text the run actually sent (post `input` event).
        if let Some(text) = context
            .messages
            .iter()
            .rev()
            .find_map(|message| match message {
                Message::User(user) => user.content.iter().find_map(|block| match block {
                    ContentBlock::Text(text) => Some(text.text.clone()),
                    _ => None,
                }),
                _ => None,
            })
        {
            self.seen_prompts
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(text);
        }
        let message = (self.respond)(index, &context);
        let stream = AssistantMessageEventStream::new();
        // Text-bearing answers stream Start + one TextDelta per text block so
        // the SDK surface observes MessageStart/MessageUpdate/MessageEnd.
        if message
            .content
            .iter()
            .any(|block| matches!(block, ContentBlock::Text(_)))
        {
            stream.push(AssistantMessageEvent::Start {
                partial: Arc::new(message.clone()),
            });
            for block in &message.content {
                if let ContentBlock::Text(text) = block {
                    stream.push(AssistantMessageEvent::TextDelta {
                        content_index: 0,
                        delta: text.text.clone(),
                        partial: Arc::new(message.clone()),
                    });
                }
            }
        }
        stream.push(AssistantMessageEventStream::done_event(
            TerminalReason::Stop,
            message,
        ));
        stream
    }

    fn api_key_auth(&self) -> Vec<ApiKeyAuth> {
        vec![ApiKeyAuth::new("fake key", vec!["FAKE_TEST_KEY".into()])]
    }
}

fn fake_registry(provider: ScriptedProvider) -> Arc<ProviderRegistry> {
    let mut registry = ProviderRegistry::default();
    registry.set_provider(Arc::new(provider));
    Arc::new(registry)
}

/// Per-test temp dir (extension fixtures + cwd).
struct TempDir(std::path::PathBuf);

impl TempDir {
    fn new(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "pir-sdk-extensibility-{label}-{}-{}",
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

// ---------------------------------------------------------------------------
// 1. Tool selection (`tools` / `exclude_tools` / `no_tools`)
// ---------------------------------------------------------------------------

/// Build a probe-tool agent, run one prompt whose scripted answer calls the
/// tool, and report how often the tool body ran plus the tool-result texts.
async fn run_probe_agent(
    select: impl FnOnce(AgentBuilder) -> AgentBuilder,
) -> (usize, Vec<String>) {
    let provider = ScriptedProvider::new(vec![
        assistant_tool_call("call-1", "probe", arguments(&[("n", json!(1))])),
        assistant_text("done"),
    ]);
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&calls);
    let builder = AgentBuilder::new()
        .with_registry(fake_registry(provider), fake_model("model-a", false))
        .tool(
            "probe",
            "Counts its own executions",
            json!({
                "type": "object",
                "properties": { "n": { "type": "integer" } }
            }),
            move |args| {
                let counter = Arc::clone(&counter);
                async move {
                    counter.fetch_add(1, Ordering::SeqCst);
                    ToolResult::ok(format!("probe n={}", args["n"]))
                }
            },
        )
        .no_session();
    let agent = select(builder).build().await.expect("build agent");
    let outcome = agent.prompt("call the probe tool").await.expect("prompt");
    (calls.load(Ordering::SeqCst), outcome.tool_result_texts())
}

#[tokio::test(flavor = "current_thread")]
async fn tool_selection_default_keeps_registered_tools_callable() {
    let (calls, results) = run_probe_agent(|builder| builder).await;
    assert_eq!(calls, 1, "no selection: the probe tool must run");
    assert!(
        results.iter().any(|text| text.contains("probe n=1")),
        "tool result texts: {results:?}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn tool_selection_no_tools_disables_every_tool() {
    let (calls, results) = run_probe_agent(|builder| builder.no_tools()).await;
    assert_eq!(calls, 0, "no_tools(): the tool body must never run");
    assert!(
        results
            .iter()
            .any(|text| text.contains("Tool probe not found")),
        "the model must see an error result naming the disabled tool: {results:?}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn tool_selection_allowlist_excludes_unlisted_tools() {
    let (calls, results) =
        run_probe_agent(|builder| builder.tools(["other"]).exclude_tools(["unused"])).await;
    assert_eq!(calls, 0, "allowlist without `probe`: the tool must not run");
    assert!(
        results
            .iter()
            .any(|text| text.contains("Tool probe not found")),
        "the model must see an error result naming the filtered tool: {results:?}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn tool_selection_mixed_allowlist_forms_fail_build() {
    let provider = ScriptedProvider::new(vec![assistant_text("ok")]);
    let error = AgentBuilder::new()
        .with_registry(fake_registry(provider), fake_model("model-a", false))
        .no_session()
        .tools(["+extra", "plain"])
        .build()
        .await
        .expect_err("mixed allowlist forms must fail the build");
    assert!(
        error.to_string().contains("--tools"),
        "the CLI-shaped validation error must surface: {error}"
    );
}

// ---------------------------------------------------------------------------
// 2. `on_event` closure plugins
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn on_event_closures_dispatch_in_registration_order_and_transform() {
    let provider =
        ScriptedProvider::with_responder(Arc::new(|_index, _context| assistant_text("ok")));
    let seen_prompts = Arc::clone(&provider.seen_prompts);
    let order: Arc<Mutex<Vec<&'static str>>> = Arc::new(Mutex::new(Vec::new()));
    let order_first = Arc::clone(&order);
    let order_second = Arc::clone(&order);
    let observed: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let observed_handler = Arc::clone(&observed);

    let agent = AgentBuilder::new()
        .with_registry(fake_registry(provider), fake_model("model-a", false))
        .no_session()
        // Handler 1 (registered first): pure observer of the `input` event.
        .on_event("input", move |payload| {
            order_first
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push("first");
            if let Some(text) = payload["text"].as_str() {
                observed_handler
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .push(text.to_owned());
            }
        })
        // Handler 2: transform the prompt text (upstream `input` contract).
        .on_event("input", |payload| {
            let text = payload["text"].as_str().unwrap_or_default().to_owned();
            PluginEventOutcome::Input {
                action: InputAction::Transform,
                text: Some(format!("{text}-via-on-event")),
                images: None,
            }
        })
        // Handler 3 (registered last): second observer, same event.
        .on_event("input", move |_payload| {
            order_second
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push("second");
        })
        .build()
        .await
        .expect("build agent");

    agent.prompt("original prompt").await.expect("prompt");

    assert_eq!(
        *order
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner),
        vec!["first", "second"],
        "stacked on_event handlers must dispatch in registration order"
    );
    assert_eq!(
        observed
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .last()
            .map(String::as_str),
        Some("original prompt"),
        "the observer sees the pre-transform payload"
    );
    assert_eq!(
        seen_prompts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .last()
            .map(String::as_str),
        Some("original prompt-via-on-event"),
        "the transform outcome must reach the model request"
    );
}

// ---------------------------------------------------------------------------
// 3. `prompt_stream_with` event filtering
// ---------------------------------------------------------------------------

async fn collect_events<S>(
    build: impl FnOnce(AgentBuilder) -> AgentBuilder,
    stream: impl FnOnce(&Agent) -> S,
) -> Vec<SdkEvent>
where
    S: futures_util::Stream<Item = SdkEvent> + Unpin,
{
    let agent = build(AgentBuilder::new())
        .no_session()
        .build()
        .await
        .expect("build agent");
    let mut events = Vec::new();
    let mut stream = stream(&agent);
    while let Some(event) = futures_util::StreamExt::next(&mut stream).await {
        events.push(event);
    }
    events
}

/// The text-answer agent every streaming test builds.
fn text_answer_build(builder: AgentBuilder) -> AgentBuilder {
    builder.with_registry(
        fake_registry(ScriptedProvider::new(vec![assistant_text("hello world")])),
        fake_model("model-a", false),
    )
}

#[tokio::test(flavor = "current_thread")]
async fn prompt_stream_with_filters_events_and_prompt_stream_keeps_all() {
    let filtered = collect_events(text_answer_build, |agent| {
        agent.prompt_stream_with("hi", |event| {
            !matches!(event, SdkEvent::MessageUpdate { .. })
        })
    })
    .await;
    assert!(
        !filtered
            .iter()
            .any(|event| matches!(event, SdkEvent::MessageUpdate { .. })),
        "the filter must drop every MessageUpdate: {filtered:?}"
    );
    assert!(
        filtered
            .iter()
            .any(|event| matches!(event, SdkEvent::MessageStart))
            && filtered
                .iter()
                .any(|event| matches!(event, SdkEvent::MessageEnd { .. })),
        "non-update lifecycle events must survive the filter: {filtered:?}"
    );
    assert!(
        matches!(filtered.last(), Some(SdkEvent::AgentSettled { .. })),
        "the stream still terminates with AgentSettled: {filtered:?}"
    );

    let full = collect_events(text_answer_build, |agent| agent.prompt_stream("hi")).await;
    assert!(
        full.iter().any(
            |event| matches!(event, SdkEvent::MessageUpdate { delta, .. } if delta == "hello world")
        ),
        "prompt_stream stays pass-all (MessageUpdate arrives): {full:?}"
    );
}

// ---------------------------------------------------------------------------
// 4. `set_model` / `set_thinking_level` (+ session-layer model_select)
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn set_model_and_set_thinking_level_switch_session_state() {
    let agent = AgentBuilder::new()
        .with_registry(
            fake_registry(ScriptedProvider::new(vec![assistant_text("ok")])),
            fake_model("model-a", false),
        )
        .no_session()
        .build()
        .await
        .expect("build agent");
    assert_eq!(agent.model().id, "model-a");
    // Build-time capability clamp: the default level (`medium`) is not a
    // level a non-reasoning model supports, so the effective level is `off`
    // (upstream `createAgentSession`'s `clampThinkingLevel`).
    assert_eq!(agent.thinking_level(), ThinkingLevel::Off);

    // Reasoning model: the requested thinking level applies.
    agent
        .set_model(fake_model("model-b", true))
        .await
        .expect("set model");
    assert_eq!(agent.model().id, "model-b");
    let effective = agent
        .set_thinking_level(ThinkingLevel::High)
        .await
        .expect("set thinking level");
    assert_eq!(effective, ThinkingLevel::High);
    assert_eq!(agent.thinking_level(), ThinkingLevel::High);

    // Non-reasoning model: the effective level clamps to `off` as part of
    // the model switch (upstream `setModel` re-applies the thinking level),
    // before any explicit `set_thinking_level` call.
    agent
        .set_model(fake_model("model-a", false))
        .await
        .expect("set model");
    assert_eq!(
        agent.thinking_level(),
        ThinkingLevel::Off,
        "set_model must clamp the applied level to the new model's capability"
    );
    let clamped = agent
        .set_thinking_level(ThinkingLevel::High)
        .await
        .expect("set thinking level");
    assert_eq!(clamped, ThinkingLevel::Off);
    assert_eq!(agent.thinking_level(), ThinkingLevel::Off);
}

#[tokio::test(flavor = "current_thread")]
async fn session_layer_model_select_dispatch_reaches_sdk_on_event_closure() {
    let temp = TempDir::new("model-select");
    let extension = temp.0.join("model_switch.js");
    std::fs::write(
        &extension,
        r#"
        export default function (pi) {
          pi.on("session_start", () => { pi.setModel("model-b"); });
        }
        "#,
    )
    .expect("write extension fixture");

    let selected: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let selected_handler = Arc::clone(&selected);
    let agent = AgentBuilder::new()
        .with_registry(
            fake_registry(ScriptedProvider::new(vec![assistant_text("ok")])),
            fake_model("model-a", false),
        )
        .cwd(temp.0.clone())
        .extension_paths(vec![extension])
        .no_session()
        .on_event("model_select", move |payload| {
            if let Some(id) = payload["model"]["id"].as_str() {
                selected_handler
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .push(id.to_owned());
            }
        })
        .build()
        .await
        .expect("build agent");

    agent.prompt("hi").await.expect("prompt");

    assert_eq!(
        *selected
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner),
        vec!["model-b".to_string()],
        "the session-layer model_select event must reach the on_event closure"
    );
    assert_eq!(agent.model().id, "model-b", "the switch must be applied");
}

// ---------------------------------------------------------------------------
// 5. `Agent::shutdown` tears the JS extension runtime down
// ---------------------------------------------------------------------------

/// Captures the handle the plugin host attaches, so the test can observe the
/// actor's liveness after the SDK shutdown path returns.
struct HandleProbe {
    info: PluginInfo,
    slot: Arc<Mutex<Option<ExtensionHandle>>>,
}

impl AgentPlugin for HandleProbe {
    fn info(&self) -> &PluginInfo {
        &self.info
    }

    fn on_runtime_attached(&self, js: Option<ExtensionHandle>) {
        *self
            .slot
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = js;
    }
}

#[tokio::test(flavor = "current_thread")]
async fn agent_shutdown_runs_js_session_shutdown_and_stops_the_actor() {
    let temp = TempDir::new("agent-shutdown-js");
    let marker = temp.0.join("shutdown.marker");
    let extension = temp.0.join("shutdown.ts");
    let marker_js = serde_json::to_string(&marker.to_string_lossy().to_string())
        .expect("marker path as a JS string");
    std::fs::write(
        &extension,
        format!(
            r#"
        import fs from "node:fs";
        export default function (pi) {{
            pi.on("session_shutdown", (event) => {{
                fs.appendFileSync({marker_js}, event.reason + "\n");
            }});
        }}
        "#
        ),
    )
    .expect("write extension fixture");

    let handle_slot: Arc<Mutex<Option<ExtensionHandle>>> = Arc::new(Mutex::new(None));
    let probe = Arc::new(HandleProbe {
        info: PluginInfo::inline("shutdown-handle-probe"),
        slot: Arc::clone(&handle_slot),
    });
    let agent = AgentBuilder::new()
        .with_registry(
            fake_registry(ScriptedProvider::new(vec![assistant_text("ok")])),
            fake_model("model-a", false),
        )
        .cwd(temp.0.clone())
        .extension_paths(vec![extension])
        .with_plugin(probe)
        .no_session()
        .no_compaction()
        .build()
        .await
        .expect("build agent");

    let runtime = handle_slot
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
        .expect("the plugin host attached the JS runtime handle");
    assert!(runtime.is_ready(), "the actor must be live before shutdown");

    agent.shutdown().await.expect("shutdown");

    assert_eq!(
        std::fs::read_to_string(&marker).expect("JS session_shutdown handler ran"),
        "quit\n",
        "Agent::shutdown must dispatch session_shutdown {{reason: \"quit\"}} to JS"
    );

    let deadline = Instant::now() + Duration::from_secs(5);
    while runtime.is_ready() && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(
        !runtime.is_ready(),
        "Agent::shutdown must stop the pir-ext-actor thread"
    );
    let mut payload = json!({});
    assert!(
        runtime.emit("input", &mut payload).await.is_err(),
        "the actor mailbox must be closed after Agent::shutdown"
    );
}
