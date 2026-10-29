//! Strict-verification tests for the `subagent` builtin (upstream
//! `packages/coding-agent/examples/extensions/subagent`): the edges beyond
//! `tests/subagent_builtin.rs`.
//!
//! Contract points, each traced to the upstream source:
//! - long descriptions (>1024 chars) are accepted (upstream has no cap);
//! - unknown `tools` frontmatter names are dropped silently (upstream passes
//!   `--tools` to the child CLI, whose allowlist matcher simply filters —
//!   `agent-session.ts _isAllowedTool` — and never errors);
//! - `{previous}` substitutes the EMPTY string when the prior step ended
//!   without final text (upstream `getFinalOutput` returns `""`);
//! - per-step `cwd` reaches the child process (upstream `spawn({ cwd })`);
//! - two parallel tasks on the SAME agent run in isolated child sessions;
//! - a parent abort during parallel fails every child with `aborted`;
//! - usage aggregation arithmetic across multi-turn children;
//! - `.pi/agents` walk-up discovery with `both` scope from a nested cwd;
//! - BOM + CRLF agent files parse (upstream `parseFrontmatter`);
//! - `agentScope: "project"` with no project agents: `Available agents: none`;
//! - `details.results[]` carries upstream's `exitCode` (0 / 1 / -1 pending).

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use pir::agent::tool_batch::ToolResult;
use pir::agent::tool_execution::{
    AgentTool, BoxFuture, ToolContext, ToolLoadout, ToolLoadoutChanges, ToolRegistry,
    ToolSessionEnv,
};
use pir::ai::auth::ApiKeyAuth;
use pir::ai::event_stream::AssistantMessageEventStream;
use pir::ai::provider::{Provider, ProviderRegistry};
use pir::ai::types::*;
use pir::plugins::subagent::{discover_agents, parse_agent_markdown, AgentScope, SubagentTool};

// ---------------------------------------------------------------------------
// Fakes (same pattern as tests/subagent_builtin.rs)
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

fn thinking(text: &str) -> ContentBlock {
    ContentBlock::Thinking(ThinkingContent {
        thinking: text.into(),
        thinking_signature: None,
        redacted: false,
    })
}

fn assistant(content: Vec<ContentBlock>, stop_reason: StopReason) -> AssistantMessage {
    AssistantMessage {
        api: "fake".into(),
        provider: "fake".into(),
        model: "fake-model".into(),
        content,
        stop_reason,
        usage: Usage {
            input: 10,
            output: 5,
            total_tokens: 15,
            cost: Cost {
                input: 0.0,
                output: 0.0,
                cache_read: 0.0,
                cache_write: 0.0,
                total: 0.25,
                ..Cost::default()
            },
            ..Usage::default()
        },
        timestamp: 1,
        ..Default::default()
    }
}

fn user_text(context: &Context) -> String {
    context
        .messages
        .iter()
        .rev()
        .filter_map(|message| match message {
            Message::User(user) => user.content.iter().find_map(|block| match block {
                ContentBlock::Text(text) => Some(text.text.clone()),
                _ => None,
            }),
            _ => None,
        })
        .next()
        .unwrap_or_default()
}

fn has_tool_result(context: &Context) -> bool {
    context
        .messages
        .last()
        .is_some_and(|message| matches!(message, Message::ToolResult(_)))
}

fn tool_call_block(id: &str, name: &str, arguments: serde_json::Value) -> ContentBlock {
    ContentBlock::ToolCall(ToolCall {
        id: id.into(),
        name: name.into(),
        arguments: arguments
            .as_object()
            .map(|map| map.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
            .unwrap_or_default(),
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

/// A fake provider whose streams block until the gate opens, then answer with
/// the task text (so concurrent tasks on the SAME agent stay distinguishable).
struct GatedProvider {
    entered: Arc<AtomicUsize>,
    active: Arc<AtomicUsize>,
    max_concurrent: Arc<AtomicUsize>,
    gate: tokio::sync::watch::Receiver<bool>,
}

impl Provider for GatedProvider {
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
        self.entered.fetch_add(1, Ordering::SeqCst);
        let now_active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
        self.max_concurrent.fetch_max(now_active, Ordering::SeqCst);
        let mut gate = self.gate.clone();
        let task = user_text(&context);
        let active = Arc::clone(&self.active);
        let stream = AssistantMessageEventStream::new();
        let pusher = stream.clone();
        tokio::spawn(async move {
            loop {
                if *gate.borrow() {
                    break;
                }
                if tokio::time::timeout(std::time::Duration::from_millis(20), gate.changed())
                    .await
                    .is_err()
                {
                    continue;
                }
            }
            active.fetch_sub(1, Ordering::SeqCst);
            pusher.push(AssistantMessageEventStream::done_event(
                TerminalReason::Stop,
                assistant(vec![text(&format!("done:{task}"))], StopReason::Stop),
            ));
        });
        stream
    }
}

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

struct TempDir(PathBuf);

impl TempDir {
    fn new(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "pir-subagent-strict-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|duration| duration.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&path).expect("create temp dir");
        Self(path)
    }

    fn agents_dir(&self) -> PathBuf {
        let dir = self.0.join("agents");
        std::fs::create_dir_all(&dir).expect("agents dir");
        dir
    }

    fn write_agent_raw(&self, name: &str, bytes: &[u8]) -> PathBuf {
        let path = self.agents_dir().join(name);
        std::fs::write(&path, bytes).expect("write agent");
        path
    }

    fn write_agent(&self, name: &str, frontmatter: &str, body: &str) -> PathBuf {
        self.write_agent_raw(name, format!("---\n{frontmatter}---\n{body}").as_bytes())
    }

    fn write_project_agent(&self, name: &str, frontmatter: &str, body: &str) -> PathBuf {
        let dir = self.0.join(".pi").join("agents");
        std::fs::create_dir_all(&dir).expect("project agents dir");
        let path = dir.join(name);
        std::fs::write(&path, format!("---\n{frontmatter}---\n{body}")).expect("write agent");
        path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn env_with(
    registry: Arc<ProviderRegistry>,
    user_agents_dir: Option<PathBuf>,
    base_tools: ToolRegistry,
) -> Arc<ToolSessionEnv> {
    env_with_updates(registry, user_agents_dir, base_tools, None)
}

fn env_with_updates(
    registry: Arc<ProviderRegistry>,
    user_agents_dir: Option<PathBuf>,
    base_tools: ToolRegistry,
    on_update: Option<pir::agent::tool_execution::ToolUpdateFn>,
) -> Arc<ToolSessionEnv> {
    Arc::new(ToolSessionEnv {
        provider_registry: registry,
        model: fake_model(),
        thinking: ThinkingLevel::Off,
        session_id: None,
        session_file: None,
        base_tools: Arc::new(base_tools),
        has_ui: false,
        project_trusted: true,
        user_agents_dir,
        agent_dir: None,
        credential: None,
        on_update,
        live_tools: None,
        active_override: None,
        background: None,
    })
}

fn tool_context(cwd: &Path, env: Arc<ToolSessionEnv>, signal: Option<AbortSignal>) -> ToolContext {
    ToolContext {
        cwd: cwd.to_path_buf(),
        signal,
        nested: None,
        session_env: Some(env),
        held_mutation_keys: None,
        on_update: None,
        background: false,
    }
}

async fn run_tool(args: serde_json::Value, ctx: &ToolContext) -> ToolResult {
    SubagentTool::default().run(&args, ctx).await
}

/// A base-registry tool that reports the cwd it ran in (the child's cwd).
struct CwdProbe {
    observed: Arc<Mutex<Vec<PathBuf>>>,
}

impl AgentTool for CwdProbe {
    fn name(&self) -> &'static str {
        "cwd_probe"
    }
    fn description(&self) -> &'static str {
        "reports the cwd"
    }
    fn run(&self, _args: &serde_json::Value, ctx: &ToolContext) -> BoxFuture<'static, ToolResult> {
        self.observed.lock().unwrap().push(ctx.cwd.clone());
        Box::pin(async { ToolResult::ok("probed") })
    }
}

// ---------------------------------------------------------------------------
// Frontmatter edge cases
// ---------------------------------------------------------------------------

#[test]
fn descriptions_longer_than_1024_chars_are_accepted() {
    // Upstream discovery has no description length cap: any string
    // frontmatter value is kept verbatim (`agents.ts` loadAgentsFromDir).
    let long = "x".repeat(2000);
    let parsed = parse_agent_markdown(&format!("---\nname: long\ndescription: {long}\n---\nbody"))
        .expect("parses");
    assert_eq!(parsed.description.map(|d| d.len()), Some(2000));

    let temp = TempDir::new("long-desc");
    temp.write_agent(
        "long.md",
        &format!("name: long\ndescription: {long}\n"),
        "b",
    );
    let discovery = discover_agents(&temp.0, AgentScope::User, &temp.agents_dir());
    assert_eq!(discovery.agents.len(), 1);
    assert_eq!(discovery.agents[0].description.len(), 2000);
}

#[test]
fn bom_and_crlf_frontmatter_parse() {
    // Upstream `parseFrontmatter` reads files as UTF-8 and tolerates a BOM;
    // CRLF line endings are the Windows default.
    let bom_crlf: Vec<u8> = {
        let mut bytes = vec![0xEF, 0xBB, 0xBF];
        bytes.extend_from_slice(b"---\r\nname: crlf\r\ndescription: windows file\r\ntools: echo, bash\r\n---\r\n\r\nBe brief.\r\n");
        bytes
    };
    let parsed = parse_agent_markdown(&String::from_utf8(bom_crlf).expect("utf8"))
        .expect("BOM + CRLF frontmatter parses");
    assert_eq!(parsed.name.as_deref(), Some("crlf"));
    assert_eq!(parsed.description.as_deref(), Some("windows file"));
    assert_eq!(
        parsed.tools,
        Some(vec!["echo".to_owned(), "bash".to_owned()])
    );

    // And through the on-disk discovery path.
    let temp = TempDir::new("bom-crlf");
    let mut bytes = vec![0xEF, 0xBB, 0xBF];
    bytes.extend_from_slice(b"---\r\nname: bomby\r\ndescription: d\r\n---\r\nbody");
    temp.write_agent_raw("bomby.md", &bytes);
    let discovery = discover_agents(&temp.0, AgentScope::User, &temp.agents_dir());
    assert_eq!(discovery.agents.len(), 1);
    assert_eq!(discovery.agents[0].name, "bomby");
}

#[tokio::test]
async fn unknown_tool_names_in_frontmatter_are_dropped_like_the_upstream_allowlist() {
    // Upstream dispatches `--tools echo,ghost`; the child CLI's allowlist
    // matcher (`agent-session.ts _isAllowedTool`) silently keeps only the
    // tools that exist — it does NOT error. The child runs with the
    // surviving tools.
    let temp = TempDir::new("unknown-tools");
    temp.write_agent(
        "mixed.md",
        "name: mixed\ndescription: d\ntools: cwd_probe, ghost_tool\n",
        "b",
    );
    let observed: Arc<Mutex<Vec<PathBuf>>> = Arc::new(Mutex::new(Vec::new()));
    let mut base = ToolRegistry::new();
    base.register(Arc::new(CwdProbe {
        observed: Arc::clone(&observed),
    }));
    let registry = fake_registry(Arc::new(|_, context| {
        if has_tool_result(context) {
            assistant(vec![text("child done")], StopReason::Stop)
        } else {
            assistant(
                vec![tool_call_block("c1", "cwd_probe", serde_json::json!({}))],
                StopReason::ToolUse,
            )
        }
    }));
    let env = env_with(registry, Some(temp.agents_dir()), base);
    let ctx = tool_context(&temp.0, env, None);
    let result = run_tool(serde_json::json!({ "agent": "mixed", "task": "t" }), &ctx).await;
    assert!(
        !result.is_error,
        "unknown frontmatter tool names must not fail the child: {}",
        result.content
    );
    // The tool call executed, proving the surviving tool stayed registered.
    assert!(!observed.lock().unwrap().is_empty(), "cwd_probe ran");
}

// ---------------------------------------------------------------------------
// Chain semantics
// ---------------------------------------------------------------------------

#[tokio::test]
async fn previous_substitutes_empty_when_the_prior_step_has_no_final_text() {
    // Upstream `previousOutput = getFinalOutput(result.messages)` returns ""
    // when the last assistant message carries no text block (here: thinking
    // only), so `{previous}` is replaced with the empty string.
    let temp = TempDir::new("no-final-text");
    temp.write_agent("a.md", "name: a\ndescription: d\n", "b");
    temp.write_agent("b.md", "name: b\ndescription: d\n", "b");
    let registry = fake_registry(Arc::new(|_, context| {
        let last_user = user_text(context);
        if last_user == "step two: <>" {
            assistant(vec![text("SECOND")], StopReason::Stop)
        } else if last_user == "step one" {
            // Ends without any text block.
            assistant(vec![thinking("pondering")], StopReason::Stop)
        } else {
            assistant(vec![text("unused")], StopReason::Stop)
        }
    }));
    let env = env_with(registry, Some(temp.agents_dir()), ToolRegistry::new());
    let ctx = tool_context(&temp.0, env, None);
    let result = run_tool(
        serde_json::json!({
            "chain": [
                { "agent": "a", "task": "step one" },
                { "agent": "b", "task": "step two: <{previous}>" },
            ]
        }),
        &ctx,
    )
    .await;
    assert!(!result.is_error, "content: {}", result.content);
    assert_eq!(result.content, "SECOND");
    let details = result.details.expect("details");
    assert_eq!(details["results"][1]["task"], "step two: <>");
}

#[tokio::test]
async fn chain_and_single_cwd_reach_the_child_process() {
    // Upstream spawns each child with `cwd: step.cwd ?? defaultCwd`; pir
    // passes the same directory into the child session, observable through a
    // base tool's ToolContext.cwd.
    let temp = TempDir::new("cwd");
    temp.write_agent("a.md", "name: a\ndescription: d\n", "b");
    let subdir = temp.0.join("custom").join("dir");
    std::fs::create_dir_all(&subdir).expect("subdir");
    let observed: Arc<Mutex<Vec<PathBuf>>> = Arc::new(Mutex::new(Vec::new()));
    let mut base = ToolRegistry::new();
    base.register(Arc::new(CwdProbe {
        observed: Arc::clone(&observed),
    }));
    let registry = fake_registry(Arc::new(|_, context| {
        if has_tool_result(context) {
            assistant(vec![text("done")], StopReason::Stop)
        } else {
            assistant(
                vec![tool_call_block("c1", "cwd_probe", serde_json::json!({}))],
                StopReason::ToolUse,
            )
        }
    }));
    let env = env_with(registry, Some(temp.agents_dir()), base);

    // Chain step cwd.
    let ctx = tool_context(&temp.0, Arc::clone(&env), None);
    let result = run_tool(
        serde_json::json!({ "chain": [{ "agent": "a", "task": "t", "cwd": subdir.to_string_lossy() }] }),
        &ctx,
    )
    .await;
    assert!(!result.is_error, "content: {}", result.content);
    let chain_cwds = observed.lock().unwrap().clone();
    assert_eq!(chain_cwds.last(), Some(&subdir), "chain step cwd");

    // Single-mode cwd.
    observed.lock().unwrap().clear();
    let ctx = tool_context(&temp.0, env, None);
    let result = run_tool(
        serde_json::json!({ "agent": "a", "task": "t", "cwd": subdir.to_string_lossy() }),
        &ctx,
    )
    .await;
    assert!(!result.is_error, "content: {}", result.content);
    let single_cwds = observed.lock().unwrap().clone();
    assert_eq!(single_cwds.last(), Some(&subdir), "single mode cwd");
}

// ---------------------------------------------------------------------------
// Parallel semantics
// ---------------------------------------------------------------------------

#[tokio::test]
async fn parallel_tasks_on_the_same_agent_run_in_isolated_children() {
    let temp = TempDir::new("same-agent");
    temp.write_agent("echoer.md", "name: echoer\ndescription: d\n", "b");
    let entered = Arc::new(AtomicUsize::new(0));
    let active = Arc::new(AtomicUsize::new(0));
    let max_concurrent = Arc::new(AtomicUsize::new(0));
    let (gate_tx, gate_rx) = tokio::sync::watch::channel(false);
    let mut registry = ProviderRegistry::default();
    registry.set_provider(Arc::new(GatedProvider {
        entered: Arc::clone(&entered),
        active: Arc::clone(&active),
        max_concurrent: Arc::clone(&max_concurrent),
        gate: gate_rx.clone(),
    }));
    let env = env_with(
        Arc::new(registry),
        Some(temp.agents_dir()),
        ToolRegistry::new(),
    );
    let ctx = tool_context(&temp.0, env, None);
    let run_task = tokio::spawn(async move {
        run_tool(
            serde_json::json!({
                "tasks": [
                    { "agent": "echoer", "task": "alpha" },
                    { "agent": "echoer", "task": "beta" },
                    { "agent": "echoer", "task": "gamma" },
                ]
            }),
            &ctx,
        )
        .await
    });

    // All three tasks enter before the gate opens: concurrent, isolated.
    for _ in 0..4000 {
        if entered.load(Ordering::SeqCst) >= 3 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    assert_eq!(
        entered.load(Ordering::SeqCst),
        3,
        "all three same-agent tasks must be in flight concurrently"
    );
    let _ = gate_tx.send(true);
    let result = run_task.await.expect("tool task");
    assert!(!result.is_error, "content: {}", result.content);
    assert!(result.content.starts_with("Parallel: 3/3 succeeded"));
    assert!(result.content.contains("done:alpha"));
    assert!(result.content.contains("done:beta"));
    assert!(result.content.contains("done:gamma"));

    // Isolation: each result carries its OWN task text (no cross-talk) and
    // its own single-turn usage.
    let details = result.details.expect("details");
    let results = details["results"].as_array().expect("results");
    assert_eq!(results.len(), 3);
    for (index, expected) in ["alpha", "beta", "gamma"].iter().enumerate() {
        assert_eq!(results[index]["task"], *expected);
        assert_eq!(results[index]["exitCode"], 0);
        assert_eq!(results[index]["usage"]["turns"], 1);
        assert_eq!(results[index]["usage"]["input"], 10);
    }
}

#[tokio::test]
async fn abort_during_parallel_fails_every_child_with_aborted() {
    let temp = TempDir::new("parallel-abort");
    temp.write_agent("a.md", "name: a\ndescription: d\n", "b");
    temp.write_agent("b.md", "name: b\ndescription: d\n", "b");
    let entered = Arc::new(AtomicUsize::new(0));
    let active = Arc::new(AtomicUsize::new(0));
    let max_concurrent = Arc::new(AtomicUsize::new(0));
    let (gate_tx, gate_rx) = tokio::sync::watch::channel(false);
    let mut registry = ProviderRegistry::default();
    registry.set_provider(Arc::new(GatedProvider {
        entered: Arc::clone(&entered),
        active: Arc::clone(&active),
        max_concurrent: Arc::clone(&max_concurrent),
        gate: gate_rx.clone(),
    }));
    let env = env_with(
        Arc::new(registry),
        Some(temp.agents_dir()),
        ToolRegistry::new(),
    );
    let signal = AbortSignal::new();
    let ctx = tool_context(&temp.0, env, Some(signal.clone()));
    let run_task = tokio::spawn(async move {
        run_tool(
            serde_json::json!({
                "tasks": [
                    { "agent": "a", "task": "one" },
                    { "agent": "b", "task": "two" },
                ]
            }),
            &ctx,
        )
        .await
    });
    for _ in 0..4000 {
        if entered.load(Ordering::SeqCst) >= 2 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    assert_eq!(entered.load(Ordering::SeqCst), 2, "children in flight");
    // Release the gate with an aborted answer (the parent abort propagated
    // into both child sessions cooperatively).
    let _ = gate_tx.send(true);
    signal.abort();
    let result = run_task.await.expect("tool task");
    assert!(result.is_error, "content: {}", result.content);
    assert_eq!(result.content, "Subagent was aborted");
    let details = result.details.expect("details");
    let results = details["results"].as_array().expect("results");
    assert_eq!(results.len(), 2);
    for entry in results {
        assert_eq!(entry["stopReason"], "aborted", "entry: {entry}");
        assert_eq!(entry["exitCode"], 1, "failed entry exitCode");
    }
}

// ---------------------------------------------------------------------------
// Usage aggregation arithmetic
// ---------------------------------------------------------------------------

#[tokio::test]
async fn usage_aggregates_across_multi_turn_children() {
    // A two-turn child: usage sums every assistant turn; contextTokens is the
    // LAST turn's total; turns counts assistant messages. Second turn's usage
    // differs so the arithmetic is observable.
    let temp = TempDir::new("usage");
    temp.write_agent("a.md", "name: a\ndescription: d\n", "b");
    let registry = fake_registry(Arc::new(|_, context| {
        let mut message = if has_tool_result(context) {
            assistant(vec![text("final")], StopReason::Stop)
        } else {
            assistant(
                vec![tool_call_block(
                    "c1",
                    "echo",
                    serde_json::json!({ "text": "x" }),
                )],
                StopReason::ToolUse,
            )
        };
        if has_tool_result(context) {
            message.usage = Usage {
                input: 20,
                output: 7,
                total_tokens: 35,
                cost: Cost {
                    total: 0.5,
                    ..Cost::default()
                },
                ..Usage::default()
            };
            message.timestamp = 2;
        }
        message
    }));
    let mut base = ToolRegistry::new();
    base.register(Arc::new(EchoTool));
    let updates: Arc<Mutex<Vec<serde_json::Value>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&updates);
    let env = env_with_updates(
        registry,
        Some(temp.agents_dir()),
        base,
        Some(Arc::new(move |value: serde_json::Value| {
            sink.lock().unwrap().push(value);
        })),
    );
    let ctx = tool_context(&temp.0, env, None);
    let result = run_tool(serde_json::json!({ "agent": "a", "task": "t" }), &ctx).await;
    assert!(!result.is_error, "content: {}", result.content);
    assert_eq!(result.content, "final");

    let details = result.details.expect("details");
    let entry = &details["results"][0];
    assert_eq!(entry["usage"]["turns"], 2);
    assert_eq!(entry["usage"]["input"], 30, "10 + 20");
    assert_eq!(entry["usage"]["output"], 12, "5 + 7");
    assert_eq!(entry["usage"]["cost"], 0.75, "0.25 + 0.5");
    assert_eq!(entry["usage"]["contextTokens"], 35, "last turn's total");
    assert_eq!(entry["exitCode"], 0);

    // The running placeholder in the live updates carries exitCode -1
    // (upstream's still-running marker).
    let updates = updates.lock().unwrap().clone();
    assert!(
        updates
            .iter()
            .any(|update| update["details"]["results"][0]["exitCode"] == -1),
        "live update must mark the running child with exitCode -1: {updates:?}"
    );
}

struct EchoTool;

impl AgentTool for EchoTool {
    fn name(&self) -> &'static str {
        "echo"
    }
    fn description(&self) -> &'static str {
        "echo"
    }
    fn run(&self, _args: &serde_json::Value, _ctx: &ToolContext) -> BoxFuture<'static, ToolResult> {
        Box::pin(async { ToolResult::ok("echo") })
    }
}

// ---------------------------------------------------------------------------
// Discovery edge cases
// ---------------------------------------------------------------------------

#[tokio::test]
async fn project_agents_discover_from_a_parent_dir_with_both_scope() {
    // `.pi/agents` in a PARENT directory of the dispatch cwd; `both` scope
    // keeps the user agents too.
    let temp = TempDir::new("walkup-both");
    temp.write_agent("user_only.md", "name: user_only\ndescription: u\n", "b");
    temp.write_project_agent(
        "shared.md",
        "name: shared\ndescription: project\n",
        "PROJECT BODY",
    );
    let nested = temp.0.join("deep").join("deeper");
    std::fs::create_dir_all(&nested).expect("nested");
    let registry = fake_registry(Arc::new(|_, context| {
        if user_text(context) == "run shared" {
            assistant(vec![text("PROJECT RAN")], StopReason::Stop)
        } else {
            assistant(vec![text("unused")], StopReason::Stop)
        }
    }));
    let env = env_with(registry, Some(temp.agents_dir()), ToolRegistry::new());
    let ctx = tool_context(&nested, env, None);
    let result = run_tool(
        serde_json::json!({
            "agent": "shared",
            "task": "run shared",
            "agentScope": "both",
        }),
        &ctx,
    )
    .await;
    assert!(!result.is_error, "content: {}", result.content);
    assert_eq!(result.content, "PROJECT RAN");
    let details = result.details.expect("details");
    assert_eq!(details["agentScope"], "both");
    assert_eq!(
        details["projectAgentsDir"],
        serde_json::Value::String(
            temp.0
                .join(".pi")
                .join("agents")
                .to_string_lossy()
                .into_owned()
        )
    );
}

#[tokio::test]
async fn project_scope_without_project_agents_reports_none() {
    // Upstream `discoverAgents(cwd, "project")` with no `.pi/agents` anywhere
    // returns an empty list; a dispatch then fails with
    // `Available agents: none.`
    let temp = TempDir::new("empty-project");
    // A USER agent exists, but project scope must not load it.
    temp.write_agent("user.md", "name: user\ndescription: u\n", "b");
    let registry = fake_registry(Arc::new(|_, _| {
        assistant(vec![text("unused")], StopReason::Stop)
    }));
    let env = env_with(registry, Some(temp.agents_dir()), ToolRegistry::new());
    let ctx = tool_context(&temp.0, env, None);
    let result = run_tool(
        serde_json::json!({ "agent": "user", "task": "t", "agentScope": "project" }),
        &ctx,
    )
    .await;
    assert!(result.is_error);
    assert_eq!(
        result.content,
        "Agent error: Unknown agent: \"user\". Available agents: none."
    );

    // Zero modes reports the same empty list.
    let result = run_tool(serde_json::json!({ "agentScope": "project" }), &ctx).await;
    assert!(
        result.content.contains("Available agents: none"),
        "content: {}",
        result.content
    );
}

// ---------------------------------------------------------------------------
// Child system prompt = the child's DECLARED tool set (D6 for subagents)
// ---------------------------------------------------------------------------

/// A base-registry tool that joins the prompt's `<tools>` section through its
/// `promptSnippet` (the section lists snippet-carrying tools only) and can
/// hide declarations through the same `prepareLoadout` hook `codemode` uses.
struct PromptProbe {
    name: &'static str,
    snippet: &'static str,
    hidden_declarations: &'static [&'static str],
    default_active: bool,
}

impl AgentTool for PromptProbe {
    fn name(&self) -> &str {
        self.name
    }
    fn description(&self) -> &str {
        "prompt probe"
    }
    fn prompt_snippet(&self) -> Option<String> {
        Some(self.snippet.to_owned())
    }
    fn default_active(&self) -> bool {
        self.default_active
    }
    fn prepare_loadout(&self, _loadout: &ToolLoadout) -> Option<ToolLoadoutChanges> {
        if self.hidden_declarations.is_empty() {
            return None;
        }
        Some(ToolLoadoutChanges {
            hidden_declarations: self
                .hidden_declarations
                .iter()
                .map(|name| (*name).to_owned())
                .collect(),
            ..Default::default()
        })
    }
    fn run(&self, _args: &serde_json::Value, _ctx: &ToolContext) -> BoxFuture<'static, ToolResult> {
        Box::pin(async { ToolResult::ok("probe") })
    }
}

/// Run one child through the real dispatcher and return the system prompt its
/// provider request carried. The child prompt is built by child.rs only when
/// the agent body is non-empty and context discovery produced a base prompt,
/// so every caller writes `AGENTS.md` into the child cwd (asserted below by
/// the marker checks).
async fn child_system_prompt(temp: &TempDir, agent: &str, base: ToolRegistry) -> String {
    std::fs::write(
        temp.0.join("AGENTS.md"),
        "# project context\nPROMPT_CONTEXT_MARKER\n",
    )
    .expect("write context file");
    let observed: Arc<Mutex<Vec<Option<String>>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&observed);
    let registry = fake_registry(Arc::new(move |_, context| {
        sink.lock().unwrap().push(context.system_prompt.clone());
        assistant(vec![text("done")], StopReason::Stop)
    }));
    let env = env_with(registry, Some(temp.agents_dir()), base);
    let ctx = tool_context(&temp.0, env, None);
    let result = run_tool(serde_json::json!({ "agent": agent, "task": "t" }), &ctx).await;
    assert!(!result.is_error, "content: {}", result.content);
    let prompt = observed
        .lock()
        .unwrap()
        .iter()
        .rev()
        .find_map(Clone::clone)
        .expect("the child request carries the child system prompt");
    assert!(
        prompt.contains("PROMPT_CONTEXT_MARKER"),
        "the context-derived base prompt must have been built: {prompt}"
    );
    prompt
}

#[tokio::test]
async fn child_prompt_lists_only_the_allowlisted_tools() {
    // D6 for subagents (upstream `_rebuildSystemPrompt`'s
    // `selectedTools: validToolNames`, agent-session.ts:1705-1731): an agent
    // whose frontmatter says `tools: probe_listed` must advertise exactly that
    // tool, never another registered child tool.
    let temp = TempDir::new("prompt-allowlist");
    temp.write_agent(
        "strict.md",
        "name: strict\ndescription: d\ntools: probe_listed\n",
        "STRICT_BODY_MARKER",
    );
    let mut base = ToolRegistry::new();
    base.register(Arc::new(PromptProbe {
        name: "probe_listed",
        snippet: "LISTED_SNIPPET_MARKER",
        hidden_declarations: &[],
        default_active: true,
    }));
    base.register(Arc::new(PromptProbe {
        name: "probe_unlisted",
        snippet: "UNLISTED_SNIPPET_MARKER",
        hidden_declarations: &[],
        default_active: true,
    }));
    let prompt = child_system_prompt(&temp, "strict", base).await;
    assert!(
        prompt.contains("LISTED_SNIPPET_MARKER"),
        "the allowlisted tool must be listed: {prompt}"
    );
    assert!(
        !prompt.contains("UNLISTED_SNIPPET_MARKER"),
        "a registered but un-allowlisted tool must not be advertised: {prompt}"
    );
    assert!(
        prompt.contains("STRICT_BODY_MARKER"),
        "the agent body still appends: {prompt}"
    );
}

#[tokio::test]
async fn child_prompt_applies_prepare_loadout_hidden_declarations() {
    // The prompt applies the declared tools' Rust `prepareLoadout` hidden
    // declarations (upstream `hiddenTools: [..._hiddenDeclarations]`,
    // agent-session.ts:1727): `probe_hider` stays active/callable but leaves
    // the `<tools>`/`<rules>` sections, exactly like the child session's
    // request declarations. Before the fix the prompt rendered the full child
    // registry and advertised it.
    let temp = TempDir::new("prompt-hidden");
    temp.write_agent(
        "hider.md",
        "name: hider\ndescription: d\ntools: probe_listed, probe_hider\n",
        "HIDER_BODY_MARKER",
    );
    let mut base = ToolRegistry::new();
    base.register(Arc::new(PromptProbe {
        name: "probe_listed",
        snippet: "LISTED_SNIPPET_MARKER",
        hidden_declarations: &[],
        default_active: true,
    }));
    base.register(Arc::new(PromptProbe {
        name: "probe_hider",
        snippet: "HIDER_SNIPPET_MARKER",
        hidden_declarations: &["probe_hider"],
        default_active: true,
    }));
    let prompt = child_system_prompt(&temp, "hider", base).await;
    assert!(
        prompt.contains("LISTED_SNIPPET_MARKER"),
        "the non-hidden declared tool stays listed: {prompt}"
    );
    assert!(
        !prompt.contains("HIDER_SNIPPET_MARKER"),
        "a tool hidden by prepareLoadout must not be advertised: {prompt}"
    );
}

#[tokio::test]
async fn child_prompt_without_allowlist_uses_registration_active_tools() {
    // No `tools:` frontmatter means the child runs the registration-active
    // default set (`active_names(None)`), not every registered tool: a
    // `default_active: false` tool is never declared by the child session, so
    // the prompt must not advertise it either.
    let temp = TempDir::new("prompt-defaults");
    temp.write_agent("plain.md", "name: plain\ndescription: d\n", "PLAIN_BODY");
    let mut base = ToolRegistry::new();
    base.register(Arc::new(PromptProbe {
        name: "probe_active",
        snippet: "ACTIVE_SNIPPET_MARKER",
        hidden_declarations: &[],
        default_active: true,
    }));
    base.register(Arc::new(PromptProbe {
        name: "probe_inactive",
        snippet: "INACTIVE_SNIPPET_MARKER",
        hidden_declarations: &[],
        default_active: false,
    }));
    let prompt = child_system_prompt(&temp, "plain", base).await;
    assert!(
        prompt.contains("ACTIVE_SNIPPET_MARKER"),
        "a registration-active tool is listed: {prompt}"
    );
    assert!(
        !prompt.contains("INACTIVE_SNIPPET_MARKER"),
        "a default-inactive tool is never declared, so it must not be advertised: {prompt}"
    );
}
