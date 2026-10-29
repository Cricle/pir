//! P0-3: `tool_search` matches become model-visible declarations.
//!
//! Upstream's `tool_search` loads inactive `codemode`/`deferred` tools into
//! the active set, and the loadout declares every active non-hidden tool, so
//! the matches are declared on the next model call
//! (`extensions/tool-search/tool.js:147-162`,
//! `dist/core/agent-session.js:1155-1206`). pir used to declare only
//! `direct`/`model-only` tools, which left the default-enabled discovery
//! feature inert: matches became callable but never entered the request's
//! tool list.
//!
//! The tests drive a real session with an inline plugin surface (the
//! `tool_search` builtin + fake deferred tools), emit a `tool_search` call
//! from the fake provider, and read the tool names of each request. The
//! remaining "next prompt, not next call" window is asserted explicitly: the
//! run's request list is fixed when the run starts, so the second request of
//! the same run still lacks the match; the next prompt declares it.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use serde_json::{json, Value};

use pir::agent::tool_batch::ToolResult;
use pir::agent::tool_execution::{AgentTool, BoxFuture, ToolContext, ToolExposure, ToolRegistry};
use pir::agent_session::{AgentSession, AgentSessionConfig};
use pir::ai::auth::ApiKeyAuth;
use pir::ai::event_stream::AssistantMessageEventStream;
use pir::ai::provider::{Provider, ProviderRegistry};
use pir::ai::types::{
    AssistantMessage, ContentBlock, Context, Message, Model, ModelCost, ModelCostRates, StopReason,
    StreamOptions, TerminalReason, TextContent, ThinkingLevel, ToolCall, ToolResultMessage, Usage,
};
use pir::coding_agent::ToolSelection;
use pir::plugins::builtin::ToolSearchPlugin;
use pir::plugins::plugin::{AgentPlugin, RegistrationContext};
use pir::plugins::PluginInfo;

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

struct TempDir(PathBuf);

impl TempDir {
    fn new(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "pir-tool-search-declaration-{label}-{}-{}",
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

/// A fake tool with a fixed exposure: the stand-in for an MCP/extension tool
/// registered with `deferred` (or `direct`) exposure.
struct FakeTool {
    name: &'static str,
    description: &'static str,
    exposure: ToolExposure,
    default_active: bool,
}

impl AgentTool for FakeTool {
    fn name(&self) -> &'static str {
        self.name
    }
    fn description(&self) -> &'static str {
        self.description
    }
    fn parameters(&self) -> Option<Value> {
        // Deliberately generic: the searchable terms live in the name and
        // description, so `unrelated_gizmo` cannot match through a property
        // name shared with `deferred_widget`.
        Some(json!({
            "type": "object",
            "properties": {
                "input": {
                    "type": "string",
                    "description": "input value"
                }
            }
        }))
    }
    fn exposure(&self) -> ToolExposure {
        self.exposure
    }
    fn default_active(&self) -> bool {
        self.default_active
    }
    /// Every fake tool contributes a snippet so the structured system prompt's
    /// `<tools>` section lists it (upstream lists snippet-carrying tools only,
    /// `system_prompt::render_tools_section`). The second description line is
    /// the tool-search matcher's own convention and is not part of the snippet.
    fn prompt_snippet(&self) -> Option<String> {
        self.description.lines().next().map(str::to_owned)
    }
    fn run(&self, _args: &Value, _ctx: &ToolContext) -> BoxFuture<'static, ToolResult> {
        Box::pin(async { ToolResult::ok("ran") })
    }
}

/// Registers one default-active direct tool and two deferred tools. Only
/// `deferred_widget` matches the search query used below.
struct DeferredToolsPlugin;

impl AgentPlugin for DeferredToolsPlugin {
    fn info(&self) -> &PluginInfo {
        static INFO: std::sync::OnceLock<PluginInfo> = std::sync::OnceLock::new();
        INFO.get_or_init(|| PluginInfo::inline("tool-search-declaration-test"))
    }

    fn register(&self, ctx: &mut RegistrationContext<'_>) -> anyhow::Result<()> {
        ctx.register_tool(Arc::new(FakeTool {
            name: "core_helper",
            description: "Always available helper",
            exposure: ToolExposure::Direct,
            default_active: true,
        }));
        ctx.register_tool(Arc::new(FakeTool {
            name: "deferred_widget",
            description: "Manages deferred widgets\nSecond line is ignored",
            exposure: ToolExposure::Deferred,
            default_active: false,
        }));
        ctx.register_tool(Arc::new(FakeTool {
            name: "unrelated_gizmo",
            description: "Unrelated helper for something else",
            exposure: ToolExposure::Deferred,
            default_active: false,
        }));
        Ok(())
    }
}

/// One scripted prompt: what the provider answers on the first request of a
/// prompt. The continuation requests of that prompt always answer "done", so
/// one scripted turn produces the same two-request shape the run's fixed tool
/// list gives.
enum Turn {
    /// Emit one `tool_search` call for `deferred widget`.
    Search,
    /// Emit `count` `tool_search` calls in ONE assistant message: the batch
    /// scheduler runs them together, the upstream repeated-search shape.
    RepeatedSearches(usize),
    /// Emit a top-level call to another tool (the D11 executable-set probe).
    Call { name: String, arguments: Value },
}

/// A provider that answers each prompt with the next scripted turn and records
/// the declared tool names of every request it sees.
struct ScriptedProvider {
    /// One entry per prompt, consumed on that prompt's first request.
    turns: Mutex<VecDeque<Turn>>,
    /// The tool names of every request, in request order.
    captured: Mutex<Vec<Vec<String>>>,
    call_seq: AtomicUsize,
}

impl ScriptedProvider {
    /// `searches` prompts that each start with a `tool_search` call.
    fn new(searches: usize) -> Self {
        Self::scripted((0..searches).map(|_| Turn::Search).collect())
    }

    fn scripted(turns: Vec<Turn>) -> Self {
        Self {
            turns: Mutex::new(turns.into()),
            captured: Mutex::new(Vec::new()),
            call_seq: AtomicUsize::new(0),
        }
    }

    fn declared(&self) -> Vec<Vec<String>> {
        self.captured
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

/// A `ToolUse` assistant message carrying one call.
fn tool_call_message(name: &str, arguments: Value, sequence: usize) -> AssistantMessage {
    let mut message = tool_call_messages(&[(name, arguments)], sequence);
    message.content.truncate(1);
    message
}

/// A `ToolUse` assistant message carrying one call per `(name, arguments)`
/// entry, numbered from `sequence`.
fn tool_call_messages(calls: &[(&str, Value)], sequence: usize) -> AssistantMessage {
    let content = calls
        .iter()
        .enumerate()
        .map(|(offset, (name, arguments))| {
            ContentBlock::ToolCall(ToolCall {
                id: format!("call-{}", sequence + offset),
                name: (*name).into(),
                arguments: arguments
                    .as_object()
                    .map(|map| {
                        map.iter()
                            .map(|(key, value)| (key.clone(), value.clone()))
                            .collect()
                    })
                    .unwrap_or_default(),
                thought_signature: None,
                namespace: None,
                arguments_error: None,
            })
        })
        .collect();
    AssistantMessage {
        api: "fake-api".into(),
        provider: "fake".into(),
        model: "fake-model".into(),
        content,
        stop_reason: StopReason::ToolUse,
        timestamp: 1,
        usage: Usage::default(),
        ..AssistantMessage::default()
    }
}

fn done_message() -> AssistantMessage {
    AssistantMessage {
        api: "fake-api".into(),
        provider: "fake".into(),
        model: "fake-model".into(),
        content: vec![ContentBlock::Text(TextContent {
            text: "done".into(),
            text_signature: None,
        })],
        stop_reason: StopReason::Stop,
        timestamp: 1,
        usage: Usage::default(),
        ..AssistantMessage::default()
    }
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
        self.captured
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(
                context
                    .tools
                    .as_ref()
                    .map(|tools| tools.iter().map(|tool| tool.name.clone()).collect())
                    .unwrap_or_default(),
            );
        // A continuation request (after a tool result) ends the prompt; only a
        // fresh prompt consumes a scripted turn.
        let new_prompt = !matches!(context.messages.last(), Some(Message::ToolResult(_)));
        let turn = if new_prompt {
            self.turns
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .pop_front()
        } else {
            None
        };
        let message = match turn {
            Some(Turn::Search) => {
                let sequence = self.call_seq.fetch_add(1, Ordering::SeqCst);
                tool_call_message(
                    "tool_search",
                    json!({ "query": "deferred widget" }),
                    sequence,
                )
            }
            Some(Turn::RepeatedSearches(count)) => {
                let sequence = self.call_seq.fetch_add(count, Ordering::SeqCst);
                let calls: Vec<(&str, Value)> = (0..count)
                    .map(|_| ("tool_search", json!({ "query": "deferred widget" })))
                    .collect();
                tool_call_messages(&calls, sequence)
            }
            Some(Turn::Call { name, arguments }) => {
                let sequence = self.call_seq.fetch_add(1, Ordering::SeqCst);
                tool_call_message(&name, arguments, sequence)
            }
            None => done_message(),
        };
        let stream = AssistantMessageEventStream::new();
        stream.push(AssistantMessageEventStream::done_event(
            TerminalReason::Stop,
            message,
        ));
        stream
    }
}

struct Fixture {
    session: AgentSession,
    provider: Arc<ScriptedProvider>,
    _dir: TempDir,
}

/// The selection every owning test uses: `tool_search` is registered
/// `model-only` + `default_active: false` (upstream
/// `createToolSearchExtension`), so it is never declared unless the host
/// activates it. Activating it here is what makes a model-issued `tool_search`
/// call a DECLARED call: since W16-D11 the top-level loop resolves calls
/// against the declared set, not the registry, and refuses an undeclared
/// model-only tool exactly like an absent one (upstream
/// `agent-loop.ts:713-723`). `core_helper` rides along because an explicit
/// selection replaces the exposure defaults entirely.
fn activated_selection() -> ToolSelection {
    ToolSelection {
        enabled: vec!["tool_search".to_owned(), "core_helper".to_owned()],
        ..ToolSelection::default()
    }
}

fn fixture(label: &str, searches: usize) -> Fixture {
    fixture_with(
        label,
        ScriptedProvider::new(searches),
        Some(activated_selection()),
    )
}

fn fixture_with(
    label: &str,
    provider: ScriptedProvider,
    tool_selection: Option<ToolSelection>,
) -> Fixture {
    let dir = TempDir::new(label);
    let provider = Arc::new(provider);
    let mut registry = ProviderRegistry::default();
    registry.set_provider(provider.clone());
    let session = AgentSession::new_with_plugins(
        AgentSessionConfig {
            provider_registry: Arc::new(registry),
            model: fake_model(),
            api_key: None,
            thinking: ThinkingLevel::Off,
            tools: ToolRegistry::new(),
            session: None,
            compaction: None,
            max_turns: 4,
            system_prompt: Some("base system prompt".into()),
            cwd: Some(dir.0.clone()),
            event_stream: None,
            extensions: None,
            tool_selection,
        },
        vec![
            Arc::new(ToolSearchPlugin::new()) as Arc<dyn AgentPlugin>,
            Arc::new(DeferredToolsPlugin),
        ],
    );
    Fixture {
        session,
        provider,
        _dir: dir,
    }
}

fn count(list: &[String], name: &str) -> usize {
    list.iter().filter(|entry| entry.as_str() == name).count()
}

async fn prompt(fixture: &Fixture) {
    fixture
        .session
        .prompt("go", Arc::new(|_| {}))
        .await
        .expect("prompt");
}

/// The recorded tool results for `name`, in transcript order.
async fn tool_results(fixture: &Fixture, name: &str) -> Vec<ToolResultMessage> {
    fixture
        .session
        .transcript()
        .await
        .iter()
        .filter_map(|message| match message {
            Message::ToolResult(result) if result.tool_name == name => Some(result.clone()),
            _ => None,
        })
        .collect()
}

/// A tool result's model-facing text (the concatenated text blocks).
fn result_text(result: &ToolResultMessage) -> String {
    result
        .content
        .iter()
        .map(|block| match block {
            ContentBlock::Text(text) => text.text.as_str(),
            _ => "",
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Declaration behavior
// ---------------------------------------------------------------------------

/// (a) A deferred tool matched by `tool_search` is declared on the next
/// loadout (and (c) a default-active tool is unaffected).
#[tokio::test(flavor = "current_thread")]
async fn activated_deferred_tool_is_declared_on_the_next_prompt() {
    let fixture = fixture("activated", 1);

    prompt(&fixture).await;
    let captured = fixture.provider.declared();
    // Prompt 1 has two requests: the tool-search call and the final answer.
    // The match is not declared before the search, and the run's request list
    // is fixed when the run starts (the documented one-request window).
    assert_eq!(captured.len(), 2, "two requests in the search prompt");
    assert!(
        !captured[0].iter().any(|name| name == "deferred_widget"),
        "the deferred tool is not declared before tool_search runs"
    );
    assert!(
        !captured[1].iter().any(|name| name == "deferred_widget"),
        "the in-flight run keeps its request tool list (next prompt, not next call)"
    );
    assert!(
        captured[0].iter().any(|name| name == "core_helper"),
        "the default-active tool is declared before the search"
    );

    prompt(&fixture).await;
    let captured = fixture.provider.declared();
    let next_loadout = &captured[2];
    assert!(
        next_loadout.iter().any(|name| name == "deferred_widget"),
        "the activated deferred tool is declared on the next loadout: {next_loadout:?}"
    );
    assert!(
        next_loadout.iter().any(|name| name == "core_helper"),
        "the default-active tool stays declared after activation: {next_loadout:?}"
    );
}

/// (b) A deferred tool the query does not match stays undeclared in every
/// loadout.
#[tokio::test(flavor = "current_thread")]
async fn non_matching_deferred_tool_stays_undeclared() {
    let fixture = fixture("non-matching", 1);

    prompt(&fixture).await;
    prompt(&fixture).await;

    for (index, declared) in fixture.provider.declared().iter().enumerate() {
        assert_eq!(
            count(declared, "unrelated_gizmo"),
            0,
            "request {index} must not declare a non-matching deferred tool: {declared:?}"
        );
    }
}

/// (b2) `--exclude-tools` outranks tool_search: an excluded deferred tool is
/// neither returned by the search nor activated into the next loadout. The
/// exclusion is a default-tools `-name` modifier (the resolver carries
/// `--exclude-tools` the same way), and the search's additive union must not
/// re-add the name the user deleted.
#[tokio::test(flavor = "current_thread")]
async fn an_excluded_deferred_tool_is_never_loaded() {
    let selection = ToolSelection {
        enabled: vec!["tool_search".to_owned(), "core_helper".to_owned()],
        modifiers: vec!["-deferred_widget".to_owned()],
        uses_default_tools: true,
    };
    let fixture = fixture_with("excluded", ScriptedProvider::new(1), Some(selection));

    prompt(&fixture).await;
    prompt(&fixture).await;

    let results = tool_results(&fixture, "tool_search").await;
    assert_eq!(results.len(), 1, "one search ran");
    let text = result_text(&results[0]);
    assert!(
        !text.contains("deferred_widget"),
        "the excluded tool must not be returned: {text}"
    );
    for (index, declared) in fixture.provider.declared().iter().enumerate() {
        assert_eq!(
            count(declared, "deferred_widget"),
            0,
            "request {index} must not declare an excluded tool: {declared:?}"
        );
    }
}

/// (c) A default-active tool is unaffected by the activation: it is declared
/// before and after, exactly once.
#[tokio::test(flavor = "current_thread")]
async fn default_active_tool_is_unaffected_by_the_activation() {
    let fixture = fixture("default-active", 1);

    prompt(&fixture).await;
    prompt(&fixture).await;

    for (index, declared) in fixture.provider.declared().iter().enumerate() {
        assert_eq!(
            count(declared, "core_helper"),
            1,
            "request {index} declares the default-active helper exactly once: {declared:?}"
        );
    }
}

/// (d) Repeated searches for the same tool do not duplicate declarations.
///
/// Both searches run in ONE batch (the upstream repeated-search shape) and
/// both execute: the second finds nothing because the match is already
/// active. The same run's next request declares the match once, and the next
/// prompt still declares it exactly once.
#[tokio::test(flavor = "current_thread")]
async fn repeated_searches_do_not_duplicate_declarations() {
    let fixture = fixture_with(
        "repeated",
        ScriptedProvider::scripted(vec![Turn::RepeatedSearches(2)]),
        Some(activated_selection()),
    );

    prompt(&fixture).await;
    prompt(&fixture).await;

    let results = tool_results(&fixture, "tool_search").await;
    assert_eq!(results.len(), 2, "both batch searches returned a result");
    assert_eq!(
        result_text(&results[0]),
        "Loaded 1 tool. They are available from your next call:\n- deferred_widget: Manages deferred widgets"
    );
    assert_eq!(
        result_text(&results[1]),
        "No matching tools found.",
        "the second search runs and finds nothing new: {:?}",
        results.iter().map(result_text).collect::<Vec<_>>()
    );

    let captured = fixture.provider.declared();
    for (index, declared) in captured.iter().enumerate() {
        assert_eq!(
            count(declared, "deferred_widget"),
            usize::from(index >= 2),
            "request {index} declares the match at most once, from the next loadout on: {captured:?}"
        );
    }
}

/// (e) The result text keeps upstream's shape
/// (`tool.js:179-190`): count with correct plural, the next-call sentence,
/// and `- name: first description line` per match.
///
/// This is also the W19-CL15 regression: with an activated `tool_search`, the
/// first search succeeds and the match is declared, and the SECOND search
/// must still execute. The write-back used to compute its baseline from the
/// exposure defaults instead of the session's resolved active set, so the
/// loadout after the first activation was `[core_helper, deferred_widget]` —
/// `tool_search` ITSELF was gone — and the second search was refused with
/// `Tool tool_search not found`. Upstream keeps it
/// (`setActiveTools([...getActiveTools(), ...matches])` starts from the active
/// set that still contains `tool_search`); the builtin now reads
/// `getActiveTools()` through its bound plugin context.
#[tokio::test(flavor = "current_thread")]
async fn tool_search_result_text_matches_upstream_shape() {
    let fixture = fixture("result-text", 2);

    prompt(&fixture).await;
    prompt(&fixture).await;

    let transcript = fixture.session.transcript().await;
    let results: Vec<&ToolResultMessage> = transcript
        .iter()
        .filter_map(|message| match message {
            Message::ToolResult(result) if result.tool_name == "tool_search" => Some(result),
            _ => None,
        })
        .collect();
    assert_eq!(results.len(), 2, "one tool result per search");

    assert_eq!(
        result_text(results[0]),
        "Loaded 1 tool. They are available from your next call:\n- deferred_widget: Manages deferred widgets"
    );
    assert_eq!(
        results[0].details,
        Some(json!({ "loaded": ["deferred_widget"] }))
    );

    // Upstream `searchAndLoad` writes `[...getActiveTools(), ...matches]`, so
    // the second search still has `tool_search` active and executes, finding
    // nothing new. The loadout after the activation must therefore still carry
    // `tool_search`; the resolved-active baseline keeps it (W19-CL15).
    assert_eq!(
        result_text(results[1]),
        "No matching tools found.",
        "the second search must execute; declarations per request: {:?}",
        fixture.provider.declared()
    );
    assert_eq!(
        results[1].details,
        Some(json!({ "loaded": [] })),
        "the second search executes and finds nothing new; declared requests: {:?}",
        fixture.provider.declared()
    );

    fixture.session.shutdown().await;
}

/// W19-CL15 regression: a successful search must never un-declare
/// `tool_search` itself across prompts.
///
/// Prompt 1: a search loads `deferred_widget`. Prompt 2: the declarations
/// carry BOTH `tool_search` (the tool that performed the search stays active,
/// upstream `[...getActiveTools(), ...matches]`) and `deferred_widget`; the
/// second search EXECUTES and returns `No matching tools found.`; and the
/// top-level call of the match still works in that same prompt (the D11 gate
/// admits it because it is declared).
#[tokio::test(flavor = "current_thread")]
async fn tool_search_stays_declared_across_prompts_after_a_successful_search() {
    let fixture = fixture_with(
        "stays-declared",
        ScriptedProvider::scripted(vec![
            Turn::Search,
            Turn::Search,
            Turn::Call {
                name: "deferred_widget".to_owned(),
                arguments: json!({ "input": "x" }),
            },
        ]),
        Some(activated_selection()),
    );

    // Prompt 1: the search loads the match.
    prompt(&fixture).await;
    let first = tool_results(&fixture, "tool_search").await;
    assert_eq!(first.len(), 1);
    assert_eq!(
        result_text(&first[0]),
        "Loaded 1 tool. They are available from your next call:\n- deferred_widget: Manages deferred widgets"
    );

    // Prompt 2: the loadout carries the search tool AND its match.
    prompt(&fixture).await;
    let captured = fixture.provider.declared();
    let second_loadout = &captured[2];
    assert!(
        second_loadout.iter().any(|name| name == "tool_search"),
        "the search tool stays declared after loading a match: {second_loadout:?}"
    );
    assert!(
        second_loadout.iter().any(|name| name == "deferred_widget"),
        "the loaded match is declared on the next prompt: {second_loadout:?}"
    );

    // The second search EXECUTES (not refused) and finds nothing new.
    let searches = tool_results(&fixture, "tool_search").await;
    assert_eq!(searches.len(), 2, "both searches produced a result");
    assert!(
        !searches[1].is_error,
        "the second search executes instead of being refused: {searches:?}"
    );
    assert_eq!(result_text(&searches[1]), "No matching tools found.");
    assert_eq!(searches[1].details, Some(json!({ "loaded": [] })));

    // Prompt 3: the top-level call of the still-declared match works.
    prompt(&fixture).await;
    let calls = tool_results(&fixture, "deferred_widget").await;
    assert_eq!(calls.len(), 1, "the top-level call produced one result");
    assert!(
        !calls[0].is_error,
        "the declared match executes at the top level: {calls:?}"
    );
    assert_eq!(result_text(&calls[0]), "ran");

    fixture.session.shutdown().await;
}

// ---------------------------------------------------------------------------
// W16-D11 executable-set boundary
// ---------------------------------------------------------------------------

/// The fixture registers `tool_search` exactly as upstream does: `model-only`
/// exposure, `default_active: false`. Without a host selection it is
/// REGISTERED but never DECLARED, and since W16-D11 a model-issued call
/// resolves against the declared set, not the registry — so it fails with the
/// scheduler's not-found shape, the same result an unregistered name gets
/// (upstream `agent-loop.ts:713-723` looks the call up in `context.tools`).
#[tokio::test(flavor = "current_thread")]
async fn unactivated_tool_search_is_refused_like_an_unknown_tool() {
    let fixture = fixture_with(
        "unactivated",
        ScriptedProvider::scripted(vec![Turn::Search]),
        None,
    );

    prompt(&fixture).await;

    let captured = fixture.provider.declared();
    assert!(
        captured
            .iter()
            .all(|declared| !declared.iter().any(|name| name == "tool_search")),
        "an un-activated model-only tool is never declared: {captured:?}"
    );

    let results = tool_results(&fixture, "tool_search").await;
    assert_eq!(results.len(), 1, "one result for the refused call");
    assert!(
        results[0].is_error,
        "the refused call is an error result: {results:?}"
    );
    assert_eq!(result_text(&results[0]), "Tool tool_search not found");
    assert_eq!(
        results[0].details, None,
        "the scheduler's not-found result carries no tool details"
    );

    fixture.session.shutdown().await;
}

/// The activated flow end to end: the search executes, the match is declared
/// on the next prompt, and a top-level call to the match runs inside that same
/// prompt's run (multi-turn fake provider).
///
/// Turn 1 searches for `deferred_widget`; turn 2 (the next prompt) calls it at
/// the top level. The declaration must reach `context.tools` and the D11 gate
/// must let the call through.
#[tokio::test(flavor = "current_thread")]
async fn activated_search_then_top_level_call_of_the_match_executes() {
    let fixture = fixture_with(
        "activated-end-to-end",
        ScriptedProvider::scripted(vec![
            Turn::Search,
            Turn::Call {
                name: "deferred_widget".to_owned(),
                arguments: json!({ "input": "x" }),
            },
        ]),
        Some(activated_selection()),
    );

    // Prompt 1: the search runs and loads the match.
    prompt(&fixture).await;
    let searches = tool_results(&fixture, "tool_search").await;
    assert_eq!(searches.len(), 1, "the activated search executes");
    assert!(
        !searches[0].is_error,
        "the search is not refused: {searches:?}"
    );
    assert_eq!(
        result_text(&searches[0]),
        "Loaded 1 tool. They are available from your next call:\n- deferred_widget: Manages deferred widgets"
    );
    assert_eq!(
        searches[0].details,
        Some(json!({ "loaded": ["deferred_widget"] }))
    );

    // Prompt 2: the match is declared, and the top-level call executes.
    prompt(&fixture).await;
    let captured = fixture.provider.declared();
    let second_prompt = captured
        .last()
        .expect("the second prompt's request")
        .clone();
    assert!(
        second_prompt.iter().any(|name| name == "deferred_widget"),
        "the match is declared on the next prompt: {second_prompt:?}"
    );

    let calls = tool_results(&fixture, "deferred_widget").await;
    assert_eq!(calls.len(), 1, "the top-level call produced one result");
    assert!(
        !calls[0].is_error,
        "the declared match executes at the top level (W16-D11 admits it): {calls:?}"
    );
    assert_eq!(result_text(&calls[0]), "ran");

    fixture.session.shutdown().await;
}

// ---------------------------------------------------------------------------
// W19-CL19 print-host regression: the resolved-active baseline
// ---------------------------------------------------------------------------
//
// Print drives `agent_loop` without an `AgentSession`, so `tool_search` had no
// session service to ask for `getActiveTools()` and fell back to the exposure
// defaults (`active_names(None)`). A `--tools ...,tool_search` selection is
// replaced by those defaults in the write-back, so a successful search
// un-declared `tool_search` itself and the next search was refused.
//
// These tests drive `run_print`'s production sequence against a real print
// plugin host (`print_plugin_host` over an inline Rust plugin) — the loadout,
// the session env `run_print` builds per prompt, and the per-prompt request
// declarations — so the seeding (`print_tool_session_env`) and the rebuild
// (`print_request_definitions`) are the code under test, not a copy.

/// The print fixture's tools: `tool_search` comes from the real builtin, an
/// extra direct tool rides along, and a `deferred` tool is the search match.
/// The selection activates `tool_search` + `core_helper` and never names the
/// deferred tool, exactly like `pir print --tools tool_search,core_helper`.
struct PrintFixture {
    registry: ToolRegistry,
    cell: Arc<std::sync::RwLock<Option<Vec<String>>>>,
    active: Vec<String>,
    /// The loadout's `hiddenTools` input (registered names the declared set
    /// leaves out, plus hidden declarations): what the pre-cache prompt build
    /// passed to `build_system_prompt`.
    hidden: Vec<String>,
    first_build: Vec<pir::ai::types::Tool>,
    _dir: TempDir,
}

fn print_fixture(label: &str) -> PrintFixture {
    let dir = TempDir::new(label);
    let mut registry = ToolRegistry::new();
    registry.register(Arc::new(FakeTool {
        name: "core_helper",
        description: "Always available helper",
        exposure: ToolExposure::Direct,
        default_active: true,
    }));
    registry.register(Arc::new(FakeTool {
        name: "deferred_widget",
        description: "Manages deferred widgets\nSecond line is ignored",
        exposure: ToolExposure::Deferred,
        default_active: false,
    }));
    // The REAL builtin, bound through the same host construction print uses.
    let host = pir::coding_agent::print::print_plugin_host(None).or_else(|| {
        Some(Arc::new(pir::plugins::PluginHost::build(
            None,
            vec![Arc::new(ToolSearchPlugin::new()) as Arc<dyn AgentPlugin>],
        )))
    });
    let host = host.expect("a print plugin host");
    pir::agent::tool_execution::merge_plugin_tools(&mut registry, &host, None);

    let loadout = pir::coding_agent::print::print_tool_loadout(
        registry,
        None,
        &["tool_search".to_owned(), "core_helper".to_owned()],
        &[],
        false,
        false,
    )
    .expect("the fixture selection resolves");
    let active = loadout.active.clone();
    assert!(
        active.iter().any(|name| name == "tool_search"),
        "the selection activates the search tool: {active:?}"
    );
    let first_build = loadout.definitions();
    let hidden = loadout.hidden.clone();
    // `run_print` seeds the env cell with the resolved active set.
    let cell = Arc::new(std::sync::RwLock::new(Some(active.clone())));

    PrintFixture {
        registry: loadout.registry,
        cell,
        active,
        hidden,
        first_build,
        _dir: dir,
    }
}

/// A print-style scripted provider: one prompt's requests, driven by the
/// turn list. It records the declared tool names and the system prompt of
/// EVERY request it sees.
/// Print installs its provider through `LoopDeps::stream`, so this is a
/// stream closure rather than a `Provider` impl (same shape `run_print`
/// fills).
struct PrintProvider {
    turns: Mutex<VecDeque<Turn>>,
    declared: Mutex<Vec<Vec<String>>>,
    system_prompts: Mutex<Vec<String>>,
    call_seq: AtomicUsize,
}

impl PrintProvider {
    fn new(turns: Vec<Turn>) -> Self {
        Self {
            turns: Mutex::new(turns.into()),
            declared: Mutex::new(Vec::new()),
            system_prompts: Mutex::new(Vec::new()),
            call_seq: AtomicUsize::new(0),
        }
    }

    fn declared(&self) -> Vec<Vec<String>> {
        self.declared
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// The system prompt of every request, in request order.
    fn system_prompts(&self) -> Vec<String> {
        self.system_prompts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Record this request's declarations and system prompt, then answer it:
    /// a fresh prompt consumes the next scripted turn, a continuation request
    /// ends the run.
    fn respond(&self, request: pir::agent::agent_loop::ProviderRequest) -> AssistantMessage {
        let context = request.context;
        self.declared
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(
                context
                    .tools
                    .as_ref()
                    .map(|tools| tools.iter().map(|tool| tool.name.clone()).collect())
                    .unwrap_or_default(),
            );
        self.system_prompts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(context.system_prompt.clone().unwrap_or_default());
        let new_prompt = !matches!(context.messages.last(), Some(Message::ToolResult(_)));
        let turn = if new_prompt {
            self.turns
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .pop_front()
        } else {
            None
        };
        match turn {
            Some(Turn::Search) => {
                let sequence = self.call_seq.fetch_add(1, Ordering::SeqCst);
                tool_call_message(
                    "tool_search",
                    json!({ "query": "deferred widget" }),
                    sequence,
                )
            }
            Some(Turn::Call { name, arguments }) => {
                let sequence = self.call_seq.fetch_add(1, Ordering::SeqCst);
                tool_call_message(&name, arguments, sequence)
            }
            Some(Turn::RepeatedSearches(count)) => {
                let sequence = self.call_seq.fetch_add(count, Ordering::SeqCst);
                let calls: Vec<(&str, Value)> = (0..count)
                    .map(|_| ("tool_search", json!({ "query": "deferred widget" })))
                    .collect();
                tool_call_messages(&calls, sequence)
            }
            None => done_message(),
        }
    }
}

/// The `<tools>` section of a structured system prompt, or `""` when the
/// prompt has none (a preamble override suppresses it).
fn tools_section(prompt: &str) -> &str {
    let Some(start) = prompt.find("<tools>") else {
        return "";
    };
    let start = start + "<tools>".len();
    let Some(end) = prompt[start..].find("</tools>") else {
        return "";
    };
    &prompt[start..start + end]
}

/// `run_print`'s prompt cache for the fixture: no `--system-prompt` override,
/// no context files, no skills, the fixture's cwd — the same shape `run_print`
/// captures before its first prompt.
fn print_prompt_cache(fixture: &PrintFixture) -> pir::coding_agent::print::PrintPromptCache {
    pir::coding_agent::print::PrintPromptCache::new(
        None,
        Vec::new(),
        Vec::new(),
        Vec::new(),
        fixture._dir.0.clone(),
        None,
    )
}

/// Run one print prompt through `agent_loop` with the print host's own env,
/// the per-prompt declarations and the per-prompt system prompt — `run_print`'s
/// sequence, minus the CLI process. Returns the tool-result messages the run
/// appended.
async fn run_print_prompt(
    fixture: &PrintFixture,
    provider: Arc<PrintProvider>,
    prompt_cache: &mut pir::coding_agent::print::PrintPromptCache,
) -> Vec<ToolResultMessage> {
    let mut registry = ToolRegistry::new();
    for tool in fixture.registry.iter() {
        registry.register(Arc::clone(tool));
    }
    let providers = Arc::new(ProviderRegistry::default());

    let mut deps = pir::coding_agent::print::print_loop_deps(
        Arc::new(registry.clone()),
        pir::agent::tool_scheduler::ToolScheduler::new(Default::default()),
        fixture._dir.0.clone(),
        None,
        None,
        None,
        ThinkingLevel::Off,
    );
    deps.stream = Box::new(move |request: pir::agent::agent_loop::ProviderRequest| {
        let message = provider.respond(request);
        Box::pin(async move { message })
    });
    // The print env exactly as `run_print` builds it: the seeded active-set
    // cell is what `tool_search` reads.
    deps.session_env = Some(pir::coding_agent::print::print_tool_session_env(
        Arc::clone(&providers),
        &fake_model(),
        ThinkingLevel::Off,
        Arc::new(registry),
        Arc::clone(&fixture.cell),
        None,
    ));
    // The session's nested-callable view (print hands the loop its loadout's
    // callable set, active direct tools + every codemode/deferred tool).
    deps.nested_callable = Some(Arc::new(
        pir::agent::tool_execution::callable_tools(&fixture.registry, &fixture.active)
            .iter()
            .map(|tool| tool.name().to_owned())
            .collect(),
    ));
    deps.max_turns = 4;

    let declared_now = fixture
        .cell
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    let tools = pir::coding_agent::print::print_request_definitions(
        &fixture.registry,
        &fixture.first_build,
        &fixture.active,
        declared_now.as_deref(),
    );
    // The system prompt follows the same cell (D6): the first prompt while the
    // declared set is unchanged, a re-render from the cached discovery inputs
    // once `tool_search` has loaded a match.
    let system_prompt =
        prompt_cache.sync(&fixture.registry, &fixture.active, declared_now.as_deref());
    let result = pir::agent::agent_loop::run_with_options(
        deps,
        Context {
            system_prompt: Some(system_prompt),
            messages: vec![Message::User(pir::ai::types::UserMessage {
                role: pir::ai::types::UserRole::User,
                content: vec![ContentBlock::Text(TextContent {
                    text: "go".into(),
                    text_signature: None,
                })],
                tools_added: None,
                tools_removed: None,
                sections: None,
                timestamp: 1,
            })],
            tools: Some(tools),
        },
        None,
        pir::agent::agent_loop::LoopOptions::default(),
    )
    .await;
    result.tool_results
}

/// W19-CL19 + W19-Q14b: a print-mode search keeps `tool_search` declared and
/// loads the match into the NEXT prompt's declarations AND `<tools>` section.
///
/// Three prompts: (1) a search loads `deferred_widget`; (2) the declarations
/// carry BOTH `tool_search` and `deferred_widget`, the system prompt's
/// `<tools>` section mentions the match, and the second search EXECUTES (`No
/// matching tools found.`); (3) a top-level call of the loaded match executes.
#[tokio::test(flavor = "current_thread")]
async fn print_search_keeps_tool_search_declared_and_loads_the_match() {
    let fixture = print_fixture("print-resolved-active");
    let provider = Arc::new(PrintProvider::new(vec![
        Turn::Search,
        Turn::Search,
        Turn::Call {
            name: "deferred_widget".to_owned(),
            arguments: json!({ "input": "x" }),
        },
    ]));
    let mut prompt_cache = print_prompt_cache(&fixture);

    // Prompt 1: the search runs against the SEEDED baseline and loads the
    // match without dropping `tool_search` from the cell.
    let results = run_print_prompt(&fixture, Arc::clone(&provider), &mut prompt_cache).await;
    let searches: Vec<&ToolResultMessage> = results
        .iter()
        .filter(|result| result.tool_name == "tool_search")
        .collect();
    assert_eq!(searches.len(), 1, "the first prompt's search ran");
    assert!(
        !searches[0].is_error,
        "the selected search tool executes: {searches:?}"
    );
    assert_eq!(
        result_text(searches[0]),
        "Loaded 1 tool. They are available from your next call:\n- deferred_widget: Manages deferred widgets"
    );
    {
        let cell = fixture
            .cell
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
            .expect("the search wrote the active set");
        assert!(
            cell.iter().any(|name| name == "tool_search"),
            "the write-back keeps the search tool (the resolved-active baseline, \
             not the exposure defaults): {cell:?}"
        );
        assert!(
            cell.iter().any(|name| name == "core_helper"),
            "the write-back keeps the other selected tool: {cell:?}"
        );
        assert!(
            cell.iter().any(|name| name == "deferred_widget"),
            "the write-back adds the match: {cell:?}"
        );
    }
    // Prompt 1's system prompt is the first build (the cell was the seed):
    // fixed for both requests of the run, and it does not list the tool the
    // model has not loaded yet.
    let prompts = provider.system_prompts();
    assert_eq!(prompts.len(), 2, "two requests in the search prompt");
    assert_eq!(
        prompts[0], prompts[1],
        "a prompt run's system prompt is fixed for the run"
    );
    assert!(
        !tools_section(&prompts[0]).contains("deferred_widget"),
        "prompt 1's <tools> does not list the unloaded match: {}",
        tools_section(&prompts[0])
    );

    // Prompt 2: the next loadout declares the search tool AND its match; the
    // second search executes and finds nothing new.
    let results = run_print_prompt(&fixture, Arc::clone(&provider), &mut prompt_cache).await;
    let searches: Vec<&ToolResultMessage> = results
        .iter()
        .filter(|result| result.tool_name == "tool_search")
        .collect();
    assert_eq!(searches.len(), 1, "the second prompt's search ran");
    assert!(
        !searches[0].is_error,
        "the second search executes instead of being refused: {searches:?}"
    );
    assert_eq!(result_text(searches[0]), "No matching tools found.");
    assert_eq!(searches[0].details, Some(json!({ "loaded": [] })));

    let declared = provider.declared();
    let second_prompt = declared
        .iter()
        .rev()
        .find(|names| names.iter().any(|name| name == "deferred_widget"))
        .unwrap_or_else(|| {
            panic!("no request declared the loaded match; every request: {declared:?}")
        });
    for expected in ["tool_search", "deferred_widget", "core_helper"] {
        assert!(
            second_prompt.iter().any(|name| name == expected),
            "the post-search loadout declares `{expected}`: {second_prompt:?}"
        );
    }

    // The system prompt follows the declarations (W19-Q14b): prompt 2's
    // `<tools>` section lists the loaded match, from the same discovered
    // inputs (no re-read), and the prompt run keeps it fixed.
    let prompts = provider.system_prompts();
    assert_eq!(prompts.len(), 4, "two requests in each prompt");
    assert_eq!(
        prompts[2], prompts[3],
        "a prompt run's system prompt is fixed for the run"
    );
    let second_tools = tools_section(&prompts[2]);
    for expected in ["tool_search", "core_helper", "deferred_widget"] {
        assert!(
            second_tools.contains(expected),
            "prompt 2's <tools> lists `{expected}` after the load: {second_tools}"
        );
    }
    assert_ne!(
        prompts[0], prompts[2],
        "the declared set moved, so the prompt was rebuilt"
    );

    // Prompt 3: the loaded match is declared and callable at the top level.
    let results = run_print_prompt(&fixture, Arc::clone(&provider), &mut prompt_cache).await;
    let calls: Vec<&ToolResultMessage> = results
        .iter()
        .filter(|result| result.tool_name == "deferred_widget")
        .collect();
    assert_eq!(calls.len(), 1, "the top-level call produced one result");
    assert!(
        !calls[0].is_error,
        "the loaded match executes at the top level: {calls:?}"
    );
    assert_eq!(result_text(calls[0]), "ran");

    // Prompt 3's declared set is unchanged from prompt 2, so the prompt is
    // reused byte-for-byte (the cache's steady state).
    let prompts = provider.system_prompts();
    assert_eq!(prompts.len(), 6, "two requests in each prompt");
    assert_eq!(
        prompts[2], prompts[4],
        "an unchanged declared set reuses the rebuilt prompt byte-for-byte"
    );
    assert_eq!(
        prompts[4], prompts[5],
        "a prompt run's system prompt is fixed for the run"
    );
}

/// The first prompt's declarations ARE the loadout's first build (the cell is
/// still exactly the selection), so the rebuild seam adds nothing before a
/// search has run.
#[test]
fn print_declarations_are_the_first_build_until_a_search_writes_the_cell() {
    let fixture = print_fixture("print-first-build");
    let names = |tools: &[pir::ai::types::Tool]| -> Vec<String> {
        tools.iter().map(|tool| tool.name.clone()).collect()
    };
    let first = names(&fixture.first_build);
    assert!(first.iter().any(|name| name == "tool_search"), "{first:?}");
    assert!(
        !first.iter().any(|name| name == "deferred_widget"),
        "a deferred tool is not declared before a search loads it: {first:?}"
    );

    let unchanged = pir::coding_agent::print::print_request_definitions(
        &fixture.registry,
        &fixture.first_build,
        &fixture.active,
        Some(&fixture.active),
    );
    assert_eq!(
        names(&unchanged),
        first,
        "an unchanged cell reuses the first build verbatim"
    );

    // A load is ADDITIVE: the rebuild carries the selection plus the match.
    let mut loaded = fixture.active.clone();
    loaded.push("deferred_widget".to_owned());
    let rebuilt = pir::coding_agent::print::print_request_definitions(
        &fixture.registry,
        &fixture.first_build,
        &fixture.active,
        Some(&loaded),
    );
    let rebuilt = names(&rebuilt);
    for expected in ["tool_search", "core_helper", "deferred_widget"] {
        assert!(
            rebuilt.iter().any(|name| name == expected),
            "the rebuild declares `{expected}`: {rebuilt:?}"
        );
    }
}

// ---------------------------------------------------------------------------
// W19-Q14b print-host regression: the system prompt follows the declaration set
// ---------------------------------------------------------------------------
//
// `run_print` resolved its one loadout before the prompt loop, so the system
// prompt was built once from the FIRST loadout: a tool loaded by `tool_search`
// (or activated by MCP discovery) was declared in the next request body but
// absent from that run's `<tools>` section. The prompt cache re-renders from
// the SAME discovered inputs when the declared set moves, and reuses the first
// prompt byte-for-byte when it does not.

/// The rebuild seam itself: a `tool_search` load (the active-set cell gains
/// the match) re-renders `<tools>` from the cached discovery inputs, a
/// steady declared set reuses the bytes, and the FIRST prompt — what the
/// pre-fix host would have sent for every run — does not mention the match.
#[test]
fn print_prompt_follows_the_declared_set_after_a_load() {
    let fixture = print_fixture("print-prompt-rebuild");
    let mut cache = print_prompt_cache(&fixture);

    let first = cache.sync(&fixture.registry, &fixture.active, None);
    let first_tools = tools_section(&first);
    assert!(first_tools.contains("tool_search"), "{first_tools}");
    assert!(first_tools.contains("core_helper"), "{first_tools}");
    // Negative control: reusing the first prompt (the pre-fix behavior) sends
    // a `<tools>` section without the tool the next request declares.
    assert!(
        !first_tools.contains("deferred_widget"),
        "the first prompt lists only the selected tools: {first_tools}"
    );

    // The cell after a successful search: the selection plus the match.
    let mut loaded = fixture.active.clone();
    loaded.push("deferred_widget".to_owned());
    let rebuilt = cache.sync(&fixture.registry, &fixture.active, Some(&loaded));
    let rebuilt_tools = tools_section(&rebuilt);
    for expected in ["tool_search", "core_helper", "deferred_widget"] {
        assert!(
            rebuilt_tools.contains(expected),
            "the loaded match's prompt lists `{expected}`: {rebuilt_tools}"
        );
    }
    assert_ne!(
        first, rebuilt,
        "the pre-fix prompt (reused first build) would omit the loaded match"
    );

    // Steady state: the same declared set is served from the cache
    // byte-identically.
    assert_eq!(
        rebuilt,
        cache.sync(&fixture.registry, &fixture.active, Some(&loaded))
    );

    // The cache key is the DECLARED surface: an active-set entry the registry
    // does not declare (a stale cell name) does not re-render.
    let mut ghost = loaded.clone();
    ghost.push("ghost_tool".to_owned());
    assert_eq!(
        rebuilt,
        cache.sync(&fixture.registry, &fixture.active, Some(&ghost))
    );

    // A cell that reverts to the seed re-renders the first prompt's bytes
    // (the discovery inputs are fixed and the render is deterministic).
    assert_eq!(
        first,
        cache.sync(&fixture.registry, &fixture.active, Some(&fixture.active))
    );
}

/// The first prompt is the SAME bytes the pre-cache build sent: the active
/// set's `ToolInfo`s and the loadout's `hidden` list (every registered name
/// the declared set leaves out), rendered from the same discovery inputs.
/// The cache only changes what happens AFTER the declared set moves.
#[test]
fn print_first_prompt_keeps_the_pre_cache_build_bytes() {
    let fixture = print_fixture("print-prompt-first-bytes");
    let mut cache = print_prompt_cache(&fixture);
    let rendered = cache.sync(&fixture.registry, &fixture.active, None);

    // The build `run_print` performed before the cache existed: `prompt_tools`
    // from the ACTIVE names and `hidden_tools` from `PrintToolLoadout::hidden`.
    let tools: Vec<pir::system_prompt::ToolInfo> = fixture
        .active
        .iter()
        .filter_map(|name| fixture.registry.get(name))
        .map(|tool| pir::system_prompt::ToolInfo::from_agent_tool(tool.as_ref()))
        .collect();
    let expected =
        pir::system_prompt::build_system_prompt(&pir::system_prompt::SystemPromptInput {
            preamble_override: None,
            append: Vec::new(),
            tools: &tools,
            hidden_tools: &fixture.hidden,
            project_files: &[],
            skills: &[],
            cwd: &fixture._dir.0,
            package_dir: None,
            custom_sections: Vec::new(),
        });
    assert_eq!(
        rendered, expected,
        "the first prompt is byte-identical to the pre-cache build"
    );
}

/// A no-search run never changes the declared set, so every prompt sends the
/// first prompt's system prompt byte-for-byte, and that prompt lists only the
/// tools the selection activated.
#[tokio::test(flavor = "current_thread")]
async fn print_no_search_run_keeps_the_first_prompt_byte_identical() {
    let fixture = print_fixture("print-prompt-identity");
    // No scripted turns: each fresh prompt answers "done" on its first
    // request, so the run issues exactly one request per prompt.
    let provider = Arc::new(PrintProvider::new(Vec::new()));
    let mut prompt_cache = print_prompt_cache(&fixture);

    run_print_prompt(&fixture, Arc::clone(&provider), &mut prompt_cache).await;
    run_print_prompt(&fixture, Arc::clone(&provider), &mut prompt_cache).await;

    let prompts = provider.system_prompts();
    assert_eq!(prompts.len(), 2, "one request per prompt");
    assert_eq!(
        prompts[0], prompts[1],
        "an unchanged declared set reuses the first prompt byte-for-byte"
    );
    let tools = tools_section(&prompts[0]);
    assert!(tools.contains("tool_search"), "{tools}");
    assert!(tools.contains("core_helper"), "{tools}");
    assert!(
        !tools.contains("deferred_widget"),
        "the deferred tool is not in the prompt before a search loads it: {tools}"
    );
}
