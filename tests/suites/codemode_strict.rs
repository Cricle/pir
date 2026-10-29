//! Strict-verification tests for the `codemode` builtin (upstream
//! `packages/codemode/src/**` + `packages/coding-agent/src/extensions/codemode`):
//! the sandbox edge cases beyond `tests/codemode_builtin.rs`.
//!
//! Contract points, each traced to the upstream source:
//! - a script returning a Promise is awaited (upstream `run()` chains
//!   `promiseThen` over the script's return value);
//! - thrown non-Error values are described with upstream `describeError`:
//!   `throw 42` -> "42", `throw {a:1}` -> `{"a":1}` (not "[object Object]");
//! - nested async tool calls (`await` inside `Promise.all`) work;
//! - a huge nested-JSON tool result flows through the bridge without
//!   crashing or poisoning the runtime;
//! - `text()` coercion matches upstream `outputText`: primitives become
//!   `String(value)` (`NaN` -> "NaN", NOT "null"), objects JSON;
//! - `console.log(Error)` renders "Name: message" (upstream `format`);
//! - `@options.timeout_ms` up to the parse cap is accepted verbatim
//!   (upstream clamps only at `MAX_TIMEOUT_MS` in `parseOptions`);
//! - two concurrent codemode executions run in isolated runtimes;
//! - `deferred`-exposure tools stay callable from scripts and `hidden`
//!   tools are absent (upstream `_getCallableTools`);
//! - console output renders AFTER the text items in one
//!   `<console_output>` block (upstream `formatOutput`).

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use pir::agent::agent_loop::AgentEvent;
use pir::agent::tool_batch::ToolResult;
use pir::agent::tool_execution::{AgentTool, BoxFuture, ToolContext, ToolExposure, ToolRegistry};
use pir::agent_session::{AgentSession, AgentSessionConfig};
use pir::ai::auth::ApiKeyAuth;
use pir::ai::event_stream::AssistantMessageEventStream;
use pir::ai::provider::{Provider, ProviderRegistry};
use pir::ai::types::*;
use pir::session_manager::SessionManager;

// ---------------------------------------------------------------------------
// Fakes (same pattern as tests/codemode_builtin.rs)
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
        context_window: 32768,
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

// ---------------------------------------------------------------------------
// Tools the scripts call
// ---------------------------------------------------------------------------

/// A plain echo tool (direct exposure).
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

/// A tool with `deferred` exposure: not declared to the model, but callable
/// from scripts (upstream `_getCallableTools`).
struct DeferredTool;

impl AgentTool for DeferredTool {
    fn name(&self) -> &'static str {
        "deferred_tool"
    }
    fn description(&self) -> &'static str {
        "Only reachable from scripts"
    }
    fn exposure(&self) -> ToolExposure {
        ToolExposure::Deferred
    }
    fn run(&self, _args: &serde_json::Value, _ctx: &ToolContext) -> BoxFuture<'static, ToolResult> {
        Box::pin(async { ToolResult::ok("deferred-reachable") })
    }
}

/// A tool with `hidden` exposure: absent from the callable set entirely.
struct HiddenTool;

impl AgentTool for HiddenTool {
    fn name(&self) -> &'static str {
        "hidden_tool"
    }
    fn description(&self) -> &'static str {
        "Never callable"
    }
    fn exposure(&self) -> ToolExposure {
        ToolExposure::Hidden
    }
    fn run(&self, _args: &serde_json::Value, _ctx: &ToolContext) -> BoxFuture<'static, ToolResult> {
        Box::pin(async { ToolResult::ok("hidden-leak") })
    }
}

/// A tool returning a large, deeply nested JSON document.
struct NestedJsonTool;

fn nested_json(depth: usize) -> String {
    let mut value = serde_json::Value::String("leaf".to_owned());
    for _ in 0..depth {
        value = serde_json::json!({ "nested": [value, value.clone()] });
    }
    value.to_string()
}

impl AgentTool for NestedJsonTool {
    fn name(&self) -> &'static str {
        "nested_json"
    }
    fn description(&self) -> &'static str {
        "Returns a deeply nested JSON document"
    }
    fn run(&self, _args: &serde_json::Value, _ctx: &ToolContext) -> BoxFuture<'static, ToolResult> {
        Box::pin(async move { ToolResult::ok(nested_json(14)) })
    }
}

// ---------------------------------------------------------------------------
// Harness: drive one codemode call through a real agent session
// ---------------------------------------------------------------------------

struct TempDir(std::path::PathBuf);

impl TempDir {
    fn new(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "pir-codemode-strict-{label}-{}-{}",
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
    tools.register(Arc::new(DeferredTool));
    tools.register(Arc::new(HiddenTool));
    tools.register(Arc::new(NestedJsonTool));
    tools
}

/// Activates `codemode` (plus the suite's direct tools) through the session's
/// live `setActiveTools` cell. `codemode` is upstream `model-only` +
/// `defaultActive: false`; the live override keeps the full merged registry,
/// so the deferred/hidden exposure fixtures below keep their semantics (a
/// `tool_selection` is only the active override — under the F12 rule it never
/// filters the registry, so those tools stay registered and callable).
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

fn codemode_session(registry: Arc<ProviderRegistry>, temp: &TempDir) -> Arc<AgentSession> {
    let tools = codemode_tools();
    let activator = ActivateCodemode::new(&tools);
    Arc::new(AgentSession::new_with_plugins(
        AgentSessionConfig {
            provider_registry: registry,
            model: fake_model(),
            api_key: None,
            thinking: ThinkingLevel::Off,
            tools,
            session: None::<SessionManager>,
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

async fn run_script(code: &str, requested_tools: Option<Vec<String>>) -> String {
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
    let agent = codemode_session(registry, &temp);
    let captured: Arc<Mutex<Vec<serde_json::Value>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&captured);
    agent
        .prompt(
            "compose",
            Arc::new(move |event: AgentEvent| {
                if let AgentEvent::AgentEnd { result } = event {
                    for tool_result in &result.tool_results {
                        if tool_result.tool_name == "codemode" {
                            let content = tool_result
                                .content
                                .iter()
                                .filter_map(|block| match block {
                                    ContentBlock::Text(text) => Some(text.text.clone()),
                                    _ => None,
                                })
                                .collect::<Vec<_>>()
                                .join("\n");
                            sink.lock().unwrap().push(serde_json::json!({
                                "content": content,
                                "isError": tool_result.is_error,
                            }));
                        }
                    }
                }
            }),
        )
        .await
        .expect("prompt");
    let captured = captured.lock().unwrap();
    let first = captured.first().expect("codemode tool result");
    first["content"].as_str().expect("content").to_owned()
}

// ---------------------------------------------------------------------------
// Promise semantics + throws
// ---------------------------------------------------------------------------

#[tokio::test]
async fn script_returning_a_promise_is_awaited() {
    // Upstream `run()` attaches `promiseThen` to the script's return value,
    // so `return Promise.resolve(x)` settles with x.
    let content = run_script("return Promise.resolve('async value');", None).await;
    assert!(
        content.starts_with("Script completed"),
        "content: {content}"
    );
    assert!(content.contains("async value"), "content: {content}");

    // A promise chain that resolves after a nested tool call settles too.
    let content = run_script(
        "return tools.echo({ text: 'chained' }).then(function (v) { return v + '!'; });",
        None,
    )
    .await;
    assert!(content.contains("chained!"), "content: {content}");
}

#[tokio::test]
async fn thrown_non_error_values_are_described_like_upstream() {
    // Upstream describeError(): a non-Error throw becomes
    // { message: format(value) } — strings as is, everything else JSON —
    // and the head defaults to "Error".
    for (script, expected) in [
        ("throw 42;", "Script error:\nError: 42"),
        ("throw { a: 1 };", "Script error:\nError: {\"a\":1}"),
        (
            "throw 'plain string';",
            "Script error:\nError: plain string",
        ),
        ("throw null;", "Script error:\nError: null"),
        ("throw undefined;", "Script error:\nError: undefined"),
        ("throw [1, 2];", "Script error:\nError: [1,2]"),
    ] {
        let content = run_script(script, None).await;
        assert!(
            content.contains(expected),
            "script {script:?} must report {expected:?}, got: {content}"
        );
    }

    // Error subclasses keep their name.
    let content = run_script("throw new RangeError('out of bounds');", None).await;
    assert!(
        content.contains("Script error:\nRangeError: out of bounds"),
        "content: {content}"
    );
}

// ---------------------------------------------------------------------------
// Coercion (upstream outputText / format)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn text_coerces_primitives_like_upstream_output_text() {
    // Primitives become String(value): NaN and Infinity must NOT collapse to
    // JSON's "null".
    let content = run_script(
        "text(42); text(1.5); text(true); text(null); text(undefined); text(NaN); text(Infinity); text(-0); text([1,2]);",
        None,
    )
    .await;
    for expected in [
        "42",
        "1.5",
        "true",
        "null",
        "undefined",
        "NaN",
        "Infinity",
        "0",
        "[1,2]",
    ] {
        assert!(
            content.contains(expected),
            "text() output must contain {expected:?}, got: {content}"
        );
    }
}

#[tokio::test]
async fn console_formats_errors_like_upstream_format() {
    // Upstream console format(): Error instances render "Name: message".
    let content = run_script("console.log(new Error('bad thing'));", None).await;
    assert!(content.contains("Error: bad thing"), "content: {content}");
    assert!(
        !content.contains("{}"),
        "an Error must not render as its empty JSON shape: {content}"
    );

    // Multiple console arguments join with a space, non-strings JSON-ified.
    let content = run_script("console.log('count', 7, { ok: true });", None).await;
    assert!(
        content.contains("count 7 {\"ok\":true}"),
        "content: {content}"
    );
}

#[tokio::test]
async fn console_output_follows_the_text_items_in_one_block() {
    // Upstream formatOutput(): console lines collect into one
    // `<console_output>` block AFTER every other output item.
    let content = run_script(
        "console.log('first-log'); text('item'); console.log('second-log'); text('return soon'); return 'returned';",
        None,
    )
    .await;
    let text_index = content.find("item").expect("text item");
    let returned_index = content.find("returned").expect("returned value");
    let console_index = content.find("<console_output>").expect("console block");
    assert!(
        text_index < returned_index && returned_index < console_index,
        "text items and the return value must precede the console block: {content}"
    );
    let console_block = &content[console_index..];
    let first = console_block.find("first-log").expect("first log");
    let second = console_block.find("second-log").expect("second log");
    assert!(
        first < second,
        "console lines keep call order: {console_block}"
    );
}

// ---------------------------------------------------------------------------
// Nested async calls + big payloads
// ---------------------------------------------------------------------------

#[tokio::test]
async fn nested_async_tool_calls_await_in_parallel() {
    let content = run_script(
        "var single = await tools.echo({ text: 'solo' });\nvar both = await Promise.all([tools.echo({ text: 'p' }), tools.echo({ text: 'q' })]);\nreturn JSON.stringify([single, both]);",
        None,
    )
    .await;
    assert!(
        content.starts_with("Script completed"),
        "content: {content}"
    );
    assert!(
        content.contains("[\"solo\",[\"p\",\"q\"]]"),
        "content: {content}"
    );
}

#[tokio::test]
async fn huge_nested_json_tool_result_flows_through_the_bridge() {
    // A ~300KB deeply nested JSON document crosses the bridge, is measured
    // by the script, and printing the document itself hits the default
    // inline budget's truncation without crashing the sandbox.
    let content = run_script(
        "var doc = await tools.nested_json({});\ntext(doc);\nreturn doc.length > 100000 ? 'big' : 'small';",
        None,
    )
    .await;
    assert!(
        content.starts_with("Script completed"),
        "content: {content}"
    );
    assert!(content.contains("big"), "content: {content}");
    // Upstream truncation keeps the head AND the tail, so the returned
    // value after the huge text item survives.
    assert!(
        content.contains("Warning: truncated output (original token count:"),
        "content: {content}"
    );
    assert!(
        content.trim_end().ends_with("big"),
        "the return value must survive tail-keeping truncation: ...{}",
        &content[content.len().saturating_sub(120)..]
    );
    assert!(content.len() < 14000, "content length: {}", content.len());
}

#[tokio::test]
async fn moderate_recursion_works_inside_the_sandbox() {
    // pir's sandbox caps the QuickJS stack at 1 MB (upstream's worker
    // bounding is the wasm memory cap), so shallow recursion must work and
    // runaway depth fails as a catchable RangeError instead of a crash.
    let content = run_script(
        "function depth(n) { return n <= 0 ? 0 : 1 + depth(n - 1); }\nreturn depth(32);",
        None,
    )
    .await;
    assert!(content.contains("32"), "content: {content}");
}

// ---------------------------------------------------------------------------
// Timeout option cap
// ---------------------------------------------------------------------------

#[tokio::test]
async fn options_timeout_up_to_the_parse_cap_is_accepted_verbatim() {
    // Upstream clamps only in parseOptions (max 2147483647); no further
    // sandbox cap rewrites the value, so a large-but-legal deadline runs a
    // quick script to completion.
    let content = run_script(
        "// @options: {\"timeout_ms\": 2147483647}\nreturn 'within cap';",
        None,
    )
    .await;
    assert!(content.contains("within cap"), "content: {content}");

    // One past the cap fails at parse time with the upstream text.
    let content = run_script("// @options: {\"timeout_ms\": 2147483648}\nreturn 1;", None).await;
    assert!(
        content.contains("@options field `timeout_ms` must be a positive integer up to 2147483647"),
        "content: {content}"
    );
}

// ---------------------------------------------------------------------------
// Concurrency isolation
// ---------------------------------------------------------------------------

#[tokio::test]
async fn two_concurrent_codemode_executions_are_isolated() {
    let temp = TempDir::new("concurrent");
    let make_registry = |tag: &'static str| {
        fake_registry(Arc::new(move |index, _context| {
            if index == 0 {
                let mut arguments = BTreeMap::new();
                arguments.insert(
                    "code".to_owned(),
                    serde_json::Value::String(format!(
                        "var tag = '{tag}';\ntext(tag + '-1');\nvar echo = await tools.echo({{ text: tag }});\nreturn tag + ':' + echo;"
                    )),
                );
                assistant(
                    vec![tool_call("call-1", "codemode", arguments)],
                    StopReason::ToolUse,
                )
            } else {
                assistant(vec![text("done")], StopReason::Stop)
            }
        }))
    };

    let agent_a = codemode_session(make_registry("alpha"), &temp);
    let agent_b = codemode_session(make_registry("beta"), &temp);
    let capture = |sink: Arc<Mutex<Vec<String>>>| {
        Arc::new(move |event: AgentEvent| {
            if let AgentEvent::AgentEnd { result } = event {
                for tool_result in &result.tool_results {
                    if tool_result.tool_name == "codemode" {
                        let content = tool_result
                            .content
                            .iter()
                            .filter_map(|block| match block {
                                ContentBlock::Text(text) => Some(text.text.clone()),
                                _ => None,
                            })
                            .collect::<Vec<_>>()
                            .join("\n");
                        sink.lock().unwrap().push(content);
                    }
                }
            }
        }) as pir::agent_session::EventHandler
    };

    let sink_a: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let sink_b: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let (a, b) = tokio::join!(
        agent_a.prompt("compose", capture(Arc::clone(&sink_a))),
        agent_b.prompt("compose", capture(Arc::clone(&sink_b))),
    );
    a.expect("prompt a");
    b.expect("prompt b");
    let content_a = sink_a.lock().unwrap().join("\n---\n");
    let content_b = sink_b.lock().unwrap().join("\n---\n");
    assert!(
        content_a.contains("alpha-1") && content_a.contains("alpha:alpha"),
        "content a: {content_a}"
    );
    assert!(
        content_b.contains("beta-1") && content_b.contains("beta:beta"),
        "content b: {content_b}"
    );
    assert!(
        !content_a.contains("beta") && !content_b.contains("alpha"),
        "runtimes must not share state: {content_a} | {content_b}"
    );
}

// ---------------------------------------------------------------------------
// Exposure semantics (upstream _getCallableTools)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn deferred_tools_stay_callable_and_hidden_tools_are_absent() {
    // `deferred` exposure: callable from scripts even though it is not
    // declared to the model.
    let content = run_script("return await tools.deferred_tool({});", None).await;
    assert!(content.contains("deferred-reachable"), "content: {content}");

    // `hidden` exposure: absent from the callable set entirely — the proxy
    // call must fail, never leak "hidden-leak".
    let content = run_script("return await tools.hidden_tool({});", None).await;
    assert!(
        !content.contains("hidden-leak"),
        "hidden tools must not be callable: {content}"
    );
    assert!(content.contains("Script failed"), "content: {content}");

    // The `tools` allowlist parameter still applies on top of exposure.
    let content = run_script(
        "return await tools.echo({ text: 'x' });",
        Some(vec!["deferred_tool".to_owned()]),
    )
    .await;
    assert!(content.contains("Script failed"), "content: {content}");
}

// ---------------------------------------------------------------------------
// Upstream header shape
// ---------------------------------------------------------------------------

#[tokio::test]
async fn result_header_carries_the_wall_time_line() {
    let content = run_script("return 'header check';", None).await;
    // Upstream header: "Script completed\nWall time N.N seconds\nOutput:"
    assert!(
        content.starts_with("Script completed\nWall time "),
        "content: {content}"
    );
    assert!(
        content.contains(" seconds\nOutput:\nheader check"),
        "content: {content}"
    );
}
