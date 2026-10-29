//! Tests for the `codemode` builtin plugin: the sandboxed JS tool
//! (nested tool calls, sandbox isolation, timeout kill), settings wiring,
//! and the inline output budget.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use pir::agent::agent_loop::AgentEvent;
use pir::agent::tool_batch::ToolResult;
use pir::agent::tool_execution::{AgentTool, BoxFuture, ToolContext, ToolExposure, ToolRegistry};
use pir::agent_session::{agent_event_to_json, AgentSession, AgentSessionConfig};
use pir::ai::auth::ApiKeyAuth;
use pir::ai::event_stream::AssistantMessageEventStream;
use pir::ai::provider::{Provider, ProviderRegistry};
use pir::ai::types::*;
use pir::session_manager::SessionManager;

// ---------------------------------------------------------------------------
// Fakes (same pattern as tests/agent_session.rs)
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

/// A fake provider that answers every stream with a scripted message and
/// records the request contexts it received.
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
struct EchoTool {
    calls: AtomicUsize,
}

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
        self.calls.fetch_add(1, Ordering::SeqCst);
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
            "pir-codemode-{label}-{}-{}",
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
    tools.register(Arc::new(EchoTool {
        calls: AtomicUsize::new(0),
    }));
    tools
}

/// Activates `codemode` (plus the fixture's direct tools) through the
/// session's live `setActiveTools` cell. `codemode` is upstream `model-only`
/// with `defaultActive: false`, so the builtin's behavior tests must activate
/// it explicitly; the live override keeps the full merged registry, so
/// deferred and codemode-exposure fixtures keep their exposure semantics.
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
    session: Option<SessionManager>,
) -> Arc<AgentSession> {
    let activator = ActivateCodemode::new(&tools);
    Arc::new(AgentSession::new_with_plugins(
        AgentSessionConfig {
            provider_registry: registry,
            model: fake_model(),
            api_key: None,
            thinking: ThinkingLevel::Off,
            tools,
            session,
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

/// Event recorder capturing `AgentEnd` results (the event carries the run's
/// shared `LoopResult`; the log keeps the `Arc` and unwraps on read).
type EndLog = Arc<Mutex<Vec<Arc<pir::agent::agent_loop::LoopResult>>>>;

fn end_recorder() -> EndLog {
    Arc::new(Mutex::new(Vec::new()))
}

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
    let code_owned = code.to_owned();
    let mut arguments = BTreeMap::new();
    arguments.insert(
        "code".to_owned(),
        serde_json::Value::String(code_owned.clone()),
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
    let agent = codemode_session(registry, codemode_tools(), &temp, None);
    let log = end_recorder();
    agent
        .prompt("compose", end_handler(log.clone()))
        .await
        .expect("prompt");
    let result = log.lock().unwrap().pop().expect("agent end");
    Arc::unwrap_or_clone(result)
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
// Sandbox execution + nested calls
// ---------------------------------------------------------------------------

#[tokio::test]
async fn script_calls_registered_tool_as_nested_call() {
    let result = run_codemode_script(
        "const out = await tools.echo({ text: 'hi from script' });\nreturn out;",
        None,
    )
    .await;
    let codemode = codemode_result(&result);
    let content = text_of(codemode);
    assert!(
        content.starts_with("Script completed"),
        "content: {content}"
    );
    assert!(content.contains("hi from script"), "content: {content}");
    // The script's tool call surfaced as a nested call with a
    // `<parentToolCallId>/<n>` id and an ok status.
    let nested = codemode.nested_calls.as_ref().expect("nested calls record");
    assert_eq!(nested.calls.len(), 1);
    assert_eq!(nested.calls[0].name, "echo");
    assert!(
        nested.calls[0].id.starts_with("call-1/"),
        "id: {}",
        nested.calls[0].id
    );
    assert_eq!(
        nested.calls[0].status,
        pir::ai::types::NestedToolCallStatus::Ok
    );
    assert!(nested.complete);
}

#[tokio::test]
async fn nested_events_carry_parent_tool_call_id() {
    let temp = TempDir::new("events");
    let mut arguments = BTreeMap::new();
    arguments.insert(
        "code".to_owned(),
        serde_json::Value::String("return await tools.echo({ text: 'x' });".into()),
    );
    let registry = fake_registry(Arc::new(move |index, _context| {
        if index == 0 {
            assistant(
                vec![tool_call("call-7", "codemode", arguments.clone())],
                StopReason::ToolUse,
            )
        } else {
            assistant(vec![text("done")], StopReason::Stop)
        }
    }));
    let agent = codemode_session(registry, codemode_tools(), &temp, None);
    let events: Arc<Mutex<Vec<serde_json::Value>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&events);
    agent
        .prompt(
            "compose",
            Arc::new(move |event| {
                let json = agent_event_to_json(&event);
                if json.get("type").and_then(serde_json::Value::as_str)
                    == Some("tool_execution_start")
                {
                    sink.lock().unwrap().push(json);
                }
            }),
        )
        .await
        .expect("prompt");
    let starts = events.lock().unwrap().clone();
    let echo_start = starts
        .iter()
        .find(|payload| payload.get("toolName").and_then(serde_json::Value::as_str) == Some("echo"))
        .expect("echo nested start event");
    assert_eq!(
        echo_start
            .get("parentToolCallId")
            .and_then(serde_json::Value::as_str),
        Some("call-7")
    );
    assert!(
        echo_start
            .get("toolCallId")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|id| id.starts_with("call-7/")),
        "nested id: {echo_start}"
    );
}

#[tokio::test]
async fn text_console_and_return_items_render() {
    let result = run_codemode_script(
        "text('first');\nconsole.log('logged', 42);\ntext('second');\nreturn { value: 7 };",
        None,
    )
    .await;
    let codemode = codemode_result(&result);
    let content = text_of(codemode);
    assert!(
        content.starts_with("Script completed"),
        "content: {content}"
    );
    assert!(content.contains("==> text 1/3 <=="), "content: {content}");
    assert!(content.contains("==> text 2/3 <=="), "content: {content}");
    assert!(content.contains("==> text 3/3 <=="), "content: {content}");
    assert!(content.contains("first"), "content: {content}");
    assert!(content.contains("second"), "content: {content}");
    assert!(content.contains("<console_output>"), "content: {content}");
    assert!(content.contains("logged 42"), "content: {content}");
    assert!(content.contains("{\"value\":7}"), "content: {content}");
}

#[tokio::test]
async fn script_error_keeps_partial_output_and_fails_result() {
    let result =
        run_codemode_script("text('partial output');\nthrow new Error('boom');", None).await;
    let codemode = codemode_result(&result);
    let content = text_of(codemode);
    assert!(content.starts_with("Script failed"), "content: {content}");
    assert!(content.contains("Wall time"), "content: {content}");
    assert!(content.contains("partial output"), "content: {content}");
    // Upstream error text: "Script error:\n{head}" with head "{name}: {message}".
    assert!(
        content.contains("Script error:\nError: boom"),
        "content: {content}"
    );
    assert!(codemode.is_error);
}

#[tokio::test]
async fn unknown_tool_call_fails_inside_the_script() {
    let result = run_codemode_script("await tools.does_not_exist({});", None).await;
    let codemode = codemode_result(&result);
    assert!(codemode.is_error);
    let content = text_of(codemode);
    assert!(content.contains("Script failed"), "content: {content}");
}

#[tokio::test]
async fn tools_allowlist_blocks_unlisted_tools() {
    let result = run_codemode_script(
        "await tools.echo({ text: 'x' });",
        Some(vec!["other".to_owned()]),
    )
    .await;
    let codemode = codemode_result(&result);
    assert!(codemode.is_error);
}

#[tokio::test]
async fn scripts_cannot_call_codemode_itself() {
    let result = run_codemode_script("await tools.codemode({ code: '1' });", None).await;
    let codemode = codemode_result(&result);
    assert!(codemode.is_error);
}

// ---------------------------------------------------------------------------
// Sandbox isolation + timeout
// ---------------------------------------------------------------------------

#[tokio::test]
async fn sandbox_has_no_host_capabilities() {
    let result = run_codemode_script(
        "return JSON.stringify([typeof require, typeof process, typeof fetch, typeof fs, typeof importScripts]);",
        None,
    )
    .await;
    let codemode = codemode_result(&result);
    assert!(
        text_of(codemode)
            .contains("[\"undefined\",\"undefined\",\"undefined\",\"undefined\",\"undefined\"]"),
        "content: {}",
        text_of(codemode)
    );
}

#[tokio::test]
async fn sandbox_cannot_reach_host_module_loader() {
    // No module loader is attached: dynamic import is rejected.
    let result = run_codemode_script(
        "try { await import('node:fs'); return 'import worked'; } catch (e) { return 'import rejected'; }",
        None,
    )
    .await;
    let codemode = codemode_result(&result);
    assert!(text_of(codemode).contains("import rejected"));
}

#[tokio::test]
async fn timeout_kills_runaway_loop_and_session_stays_usable() {
    // Per-script deadline via the @options preamble (no process-global env:
    // this binary's tests run in parallel and share the environment).
    let result =
        run_codemode_script("// @options: {\"timeout_ms\": 400}\nwhile (true) {}", None).await;
    let codemode = codemode_result(&result);
    let content = text_of(codemode);
    assert!(codemode.is_error, "content: {content}");
    assert!(content.contains("Script failed"), "content: {content}");
    // The runtime is torn down per execution: a follow-up call works.
    let after = run_codemode_script("return 'recovered';", None).await;
    let after_codemode = codemode_result(&after);
    assert!(text_of(after_codemode).contains("recovered"));
}

// ---------------------------------------------------------------------------
// Settings wiring + output budget
// ---------------------------------------------------------------------------

#[test]
fn settings_mode_and_inline_budget_are_consumed() {
    // The settings-wiring seam is the pure `codemode_config_from` helper over
    // the same public SettingsManager the hosts use (env-var-based loading
    // races other tests in the shared process).
    let temp = TempDir::new("settings");
    std::fs::write(
        temp.0.join("settings.json"),
        serde_json::json!({
            "codemode": { "mode": "only", "inlineBudget": 77 }
        })
        .to_string(),
    )
    .expect("write settings");
    let manager =
        pir::config::settings::SettingsManager::load(&temp.0, &temp.0.join(".pi"), &temp.0, false);
    let config = pir::plugins::codemode::codemode_config_from(&manager);
    assert_eq!(config.mode, pir::config::settings::CodemodeMode::Only);
    assert_eq!(config.inline_budget, 77);
}

#[test]
fn settings_default_to_on_and_upstream_default_budget() {
    let temp = TempDir::new("settings-default");
    let manager =
        pir::config::settings::SettingsManager::load(&temp.0, &temp.0.join(".pi"), &temp.0, false);
    let config = pir::plugins::codemode::codemode_config_from(&manager);
    // No `codemode.mode` setting: the consumer default is upstream's `"on"`.
    assert_eq!(config.mode, pir::config::settings::CodemodeMode::On);
    assert_eq!(
        config.inline_budget,
        pir::plugins::codemode::DEFAULT_INLINE_BUDGET
    );
}

#[tokio::test]
async fn output_beyond_the_inline_budget_is_truncated() {
    let result = run_codemode_script(
        "var s = ''; for (var i = 0; i < 20000; i++) { s += 'x'; } text(s); return 'ok';",
        None,
    )
    .await;
    let codemode = codemode_result(&result);
    let content = text_of(codemode);
    assert!(content.contains("Script completed"), "content: {content}");
    assert!(
        content.contains("Warning: truncated output (original token count:"),
        "content: {content}"
    );
    // The truncated output keeps head AND tail (upstream truncateOutput) and
    // stays near the default budget (3000 tokens * 4 chars/token) plus the
    // header and the notice.
    assert!(content.len() < 13000, "content length: {}", content.len());
}

// ---------------------------------------------------------------------------
// Output caps (upstream MAX_OUTPUT_CHARS / MAX_OUTPUT_ITEMS)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn text_output_caps_fail_the_script_without_oom() {
    // A script pushing text() past the default character cap (upstream
    // MAX_OUTPUT_CHARS, 16MB) must FAIL with the upstream cap error instead
    // of growing host memory unbounded. The caps are exercised at their
    // defaults because the cap overrides are process-global env reads and
    // the tests in this binary run in parallel.
    // A few LARGE items instead of many small ones: the total bytes past the
    // cap are the same, but the prelude's per-item JS accounting scans every
    // char, and thousands of small items make the script exceed the default
    // 30s sandbox deadline before the cap trips (the deadline interrupt then
    // wins with a generic QuickJS abort instead of the cap error).
    let result = run_codemode_script(
        "var s = 'x'.repeat(1048576); for (var i = 0; i < 20; i++) { text(s); } return 'done';",
        None,
    )
    .await;
    let codemode = codemode_result(&result);
    let content = text_of(codemode);
    assert!(codemode.is_error, "content: {content}");
    assert!(
        content.contains(&format!(
            "script output exceeded the limit of {} characters",
            pir::plugins::codemode::MAX_OUTPUT_CHARS
        )),
        "content: {content}"
    );
    // Bounded: partial output + the cap error, all inline-budget truncated.
    assert!(content.len() < 14000, "content length: {}", content.len());

    // The per-execution teardown keeps the session usable afterwards.
    let after = run_codemode_script("return 'recovered';", None).await;
    assert!(text_of(codemode_result(&after)).contains("recovered"));
}

#[tokio::test]
async fn console_output_counts_toward_the_caps_too() {
    // A script spamming console.log past the default item cap (upstream
    // MAX_OUTPUT_ITEMS, 100_000) must FAIL with the cap error; the character
    // total stays tiny, so this proves the item cap bites on its own.
    let result = run_codemode_script(
        "for (var i = 0; i < 150000; i++) { console.log('i'); } return 'done';",
        None,
    )
    .await;
    let codemode = codemode_result(&result);
    let content = text_of(codemode);
    assert!(codemode.is_error, "content: {content}");
    assert!(
        content.contains(&format!(
            "or {} text(), image(), and console calls",
            pir::plugins::codemode::MAX_OUTPUT_ITEMS
        )),
        "content: {content}"
    );
}

// ---------------------------------------------------------------------------
// `// @options:` preamble (upstream parseCodemodeSource)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn options_preamble_applies_max_output_tokens_and_timeout() {
    // max_output_tokens overrides the inline budget for this run.
    let result = run_codemode_script(
        "// @options: {\"max_output_tokens\": 100}\nvar s = ''; for (var i = 0; i < 20000; i++) { s += 'x'; } text(s); return 'ok';",
        None,
    )
    .await;
    let codemode = codemode_result(&result);
    let content = text_of(codemode);
    assert!(content.contains("Script completed"), "content: {content}");
    assert!(
        content.contains("Warning: truncated output (original token count:"),
        "content: {content}"
    );
    // 100 tokens * 4 chars/token of head+tail output plus header + notice.
    assert!(content.len() < 900, "content length: {}", content.len());

    // timeout_ms overrides the default timeout: a runaway loop fails fast
    // (well below the 30s default), not after the ambient timeout.
    let started = std::time::Instant::now();
    let result =
        run_codemode_script("// @options: {\"timeout_ms\": 300}\nwhile (true) {}", None).await;
    let elapsed = started.elapsed();
    let codemode = codemode_result(&result);
    assert!(codemode.is_error, "content: {}", text_of(codemode));
    assert!(text_of(codemode).contains("Script failed"));
    assert!(
        elapsed < std::time::Duration::from_secs(10),
        "elapsed: {elapsed:?}"
    );
}

#[tokio::test]
async fn invalid_options_fail_the_tool_with_upstream_text() {
    let result = run_codemode_script("// @options: {\"bogus\": 1}\nreturn 1;", None).await;
    let codemode = codemode_result(&result);
    assert!(codemode.is_error);
    assert_eq!(
        text_of(codemode),
        "@options only supports `max_output_tokens` and `timeout_ms`; got `bogus`"
    );

    // An options line with no following source is an error too.
    let result = run_codemode_script("// @options: {\"timeout_ms\": 100}", None).await;
    let codemode = codemode_result(&result);
    assert!(codemode.is_error);
    assert_eq!(
        text_of(codemode),
        "The @options line must be followed by JavaScript source on subsequent lines"
    );
}

#[test]
fn parse_codemode_source_error_texts_match_upstream() {
    let parse = pir::plugins::codemode::parse_codemode_source;
    // Empty input.
    assert_eq!(
        parse("   ").unwrap_err(),
        "Expected JavaScript source text (non-empty). Provide JS only, optionally with a first line `// @options: {\"max_output_tokens\": 1000}`."
    );
    // Bare prefix (empty directive).
    assert_eq!(
        parse("// @options:\nreturn 1;").unwrap_err(),
        "@options must be a JSON object with supported fields `max_output_tokens` and `timeout_ms`"
    );
    // Invalid JSON.
    let error = parse("// @options: not json\nreturn 1;").unwrap_err();
    assert!(
        error.starts_with(
            "@options must be valid JSON with supported fields `max_output_tokens` and `timeout_ms`: "
        ),
        "error: {error}"
    );
    // Non-object JSON.
    assert_eq!(
        parse("// @options: 7\nreturn 1;").unwrap_err(),
        "@options must be a JSON object with supported fields `max_output_tokens` and `timeout_ms`"
    );
    // Bad timeout values.
    assert_eq!(
        parse("// @options: {\"timeout_ms\": 0}\nreturn 1;").unwrap_err(),
        "@options field `timeout_ms` must be a positive integer up to 2147483647"
    );
    assert_eq!(
        parse("// @options: {\"timeout_ms\": 99999999999}\nreturn 1;").unwrap_err(),
        "@options field `timeout_ms` must be a positive integer up to 2147483647"
    );
    // Bad token budgets.
    assert_eq!(
        parse("// @options: {\"max_output_tokens\": -1}\nreturn 1;").unwrap_err(),
        "@options field `max_output_tokens` must be a non-negative safe integer"
    );
    assert_eq!(
        parse("// @options: {\"max_output_tokens\": 1.5}\nreturn 1;").unwrap_err(),
        "@options field `max_output_tokens` must be a non-negative safe integer"
    );
}

#[test]
fn parse_codemode_source_strips_options_and_preserves_lines() {
    let parse = pir::plugins::codemode::parse_codemode_source;
    let (code, options) = parse(
        "// @options: {\"max_output_tokens\": 500, \"timeout_ms\": 1234}\nreturn 1;\nreturn 2;",
    )
    .expect("parses");
    assert_eq!(options.max_output_tokens, Some(500));
    assert_eq!(options.timeout_ms, Some(1234));
    // The options line becomes an empty line, so line numbers are unchanged.
    assert!(code.starts_with('\n'), "code: {code:?}");
    assert_eq!(code.lines().count(), 3);

    // No options line: the code passes through verbatim.
    let (code, options) = parse("return 1;").expect("parses plain");
    assert_eq!(code, "return 1;");
    assert_eq!(
        options,
        pir::plugins::codemode::CodemodeSourceOptions::default()
    );

    // \r\n handling on the options line.
    let (code, options) =
        parse("// @options: {\"timeout_ms\": 5}\r\nreturn 1;").expect("parses crlf");
    assert_eq!(options.timeout_ms, Some(5));
    assert!(code.starts_with('\n'));
}

// ---------------------------------------------------------------------------
// Abort mid-script (interrupt handler honors the run signal)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn abort_stops_a_running_script_promptly() {
    // Long per-script deadline via the @options preamble (no process-global
    // env: this binary's tests run in parallel and share the environment):
    // only the abort signal can stop this loop.
    let temp = TempDir::new("abort");
    let mut arguments = BTreeMap::new();
    arguments.insert(
        "code".to_owned(),
        serde_json::Value::String("// @options: {\"timeout_ms\": 60000}\nwhile (true) {}".into()),
    );
    let registry = fake_registry(Arc::new(move |index, _context| {
        if index == 0 {
            assistant(
                vec![tool_call("call-9", "codemode", arguments.clone())],
                StopReason::ToolUse,
            )
        } else {
            assistant(vec![text("done")], StopReason::Stop)
        }
    }));
    let agent = codemode_session(registry, codemode_tools(), &temp, None);
    let aborter = Arc::clone(&agent);
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        aborter.abort();
    });
    let started = std::time::Instant::now();
    let log = end_recorder();
    let _ = agent.prompt("compose", end_handler(log.clone())).await;
    let elapsed = started.elapsed();
    // The interrupt handler stopped the compute-bound loop promptly on
    // abort; without the signal check the script would burn CPU until the
    // 60s deadline.
    assert!(
        elapsed < std::time::Duration::from_secs(20),
        "elapsed: {elapsed:?}"
    );
    let result = log.lock().unwrap().pop().expect("agent end");
    let codemode = codemode_result(&result);
    let content = text_of(codemode);
    assert!(codemode.is_error, "content: {content}");
    assert!(content.contains("Script failed"), "content: {content}");
}

// ---------------------------------------------------------------------------
// Registration
// ---------------------------------------------------------------------------

#[test]
fn codemode_builtin_is_registered_and_replaceable() {
    let def = pir::plugins::builtin_extension_def("codemode").expect("codemode builtin");
    assert!(def.replaceable);
    let plugin = (def.factory)();
    assert!(plugin.info().replaceable);
    assert_eq!(plugin.info().name, "codemode");
}

#[tokio::test]
async fn codemode_tool_joins_registry_through_plugin_host() {
    let plugin = pir::plugins::codemode::new_codemode_plugin();
    let mut registrations = pir::plugins::PluginRegistrations::default();
    let mut ctx = pir::plugins::RegistrationContext::new(&mut registrations);
    plugin.register(&mut ctx).expect("register");
    let names: Vec<String> = registrations
        .tools
        .iter()
        .map(|(def, _)| def.name.clone())
        .collect();
    // The codemode plugin registers the `codemode` tool plus the `models.*`
    // namespace tools (image generation and classification) that route to the
    // image/classifier model families.
    assert_eq!(
        names,
        vec![
            "codemode".to_owned(),
            "models.generateImages".to_owned(),
            "models.classify".to_owned(),
        ]
    );
    let (def, _) = &registrations.tools[0];
    let parameters = def.parameters.clone();
    let schema = parameters
        .get("properties")
        .and_then(|props| props.get("code"))
        .cloned();
    assert_eq!(
        schema.and_then(|schema| schema.get("type").cloned()),
        Some(serde_json::Value::String("string".into()))
    );
    assert_eq!(
        def.parameters
            .get("required")
            .and_then(serde_json::Value::as_array)
            .map(|required| required.len()),
        Some(1)
    );
}

#[tokio::test]
async fn tool_run_requires_a_session_nested_context() {
    use pir::agent::tool_execution::ToolContext;
    let tool = pir::plugins::codemode::CodemodeTool::default();
    let ctx = ToolContext::new(std::env::temp_dir());
    let result = tool
        .run(&serde_json::json!({ "code": "return 1;" }), &ctx)
        .await;
    assert!(result.is_error);
    assert!(result.content.contains("nested tool execution"));
}

/// A minimal direct tool the loadout tests register alongside codemode.
struct LoadoutEchoTool;

impl AgentTool for LoadoutEchoTool {
    fn name(&self) -> &'static str {
        "echo"
    }
    fn description(&self) -> &'static str {
        "echoes"
    }
    fn run(&self, _args: &serde_json::Value, _ctx: &ToolContext) -> BoxFuture<'static, ToolResult> {
        Box::pin(async { ToolResult::ok("ok") })
    }
}

fn loadout_with(tool: Arc<dyn AgentTool>) -> pir::agent::tool_execution::ToolLoadout {
    let mut tools: Vec<Arc<dyn AgentTool>> = vec![tool];
    tools.push(Arc::new(LoadoutEchoTool));
    pir::agent::tool_execution::ToolLoadout {
        declared: tools.clone(),
        callable: tools.clone(),
        registered: tools,
        // An empty exposure map means every tool is `direct` (the default).
        exposures: Default::default(),
        namespaces: Default::default(),
    }
}

fn registered_codemode_tool(mode: pir::config::settings::CodemodeMode) -> Arc<dyn AgentTool> {
    use pir::plugins::plugin::{PluginRegistrations, RegistrationContext};
    use pir::plugins::{AgentPlugin, CodemodeConfig, CodemodePlugin};
    let mut registrations = PluginRegistrations::default();
    CodemodePlugin::with_config(CodemodeConfig {
        mode,
        inline_budget: pir::plugins::codemode::DEFAULT_INLINE_BUDGET,
        sandbox_memory_bytes: Some(pir::plugins::codemode::default_sandbox_memory_bytes()),
    })
    .register(&mut RegistrationContext::new(&mut registrations))
    .expect("register the codemode plugin");
    let tool = match registrations.tools[0].1.clone() {
        pir::plugins::ToolBackend::Rust(tool) => tool,
        pir::plugins::ToolBackend::Js { .. } => {
            panic!("codemode must register a Rust tool")
        }
    };
    assert_eq!(tool.name(), "codemode");
    tool
}

/// Regression: `codemode.mode: "only"` is wired through the tool-exposure
/// loadout (upstream `prepareCodemodeLoadout` `hiddenDeclarations`) — the
/// declarations of active direct tools are left out while the tools stay
/// active and callable from scripts. The default mode (`"on"`) leaves the
/// loadout untouched, and codemode never hides its own declaration.
#[test]
fn mode_only_hides_direct_declarations_through_the_loadout() {
    use pir::config::settings::CodemodeMode;

    let loadout = loadout_with(registered_codemode_tool(CodemodeMode::Only));
    let changes = loadout
        .declared
        .iter()
        .find(|tool| tool.name() == "codemode")
        .expect("codemode registered")
        .prepare_loadout(&loadout)
        .expect("mode only must change the loadout");
    assert_eq!(changes.hidden_declarations, vec!["echo".to_owned()]);
    assert!(changes.descriptions.is_empty());

    let loadout = loadout_with(registered_codemode_tool(CodemodeMode::On));
    let changes = loadout
        .declared
        .iter()
        .find(|tool| tool.name() == "codemode")
        .expect("codemode registered")
        .prepare_loadout(&loadout);
    assert_eq!(changes, None, "mode on must leave the loadout untouched");
}
