//! C1-03 regression tests: per-child budgets and parallel-dispatch
//! cancellation.
//!
//! Covered:
//! 1. `SubagentBudget` defaults + `PIR_SUBAGENT_MAX_TURNS` /
//!    `PIR_SUBAGENT_WALL_CLOCK_MS` overrides (pure lookup, no process env
//!    mutation);
//! 2. a runaway child stops at the turn cap with the agent loop's structured
//!    `StopReason::Error` terminal;
//! 3. a hung provider call is stopped by the wall-clock budget (abort, then
//!    the abort-grace drop for a prompt that never reaches a checkpoint);
//! 4. dropping the outer `run_parallel` future aborts every child: no
//!    provider calls continue and the global child-runtime slots return to
//!    their baseline.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use pir::agent::tool_execution::{AgentTool, ToolContext, ToolRegistry, ToolSessionEnv};
use pir::ai::auth::ApiKeyAuth;
use pir::ai::event_stream::AssistantMessageEventStream;
use pir::ai::provider::{Provider, ProviderRegistry};
use pir::ai::types::*;
use pir::plugins::subagent::{
    active_child_runtime_slots, SubagentBudget, SubagentTool, DEFAULT_SUBAGENT_MAX_TURNS,
    DEFAULT_SUBAGENT_WALL_CLOCK_MS, SUBAGENT_MAX_TURNS_ENV, SUBAGENT_WALL_CLOCK_ENV,
};

// ---------------------------------------------------------------------------
// Fakes (mirrors tests/subagent_builtin.rs)
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

type Responder = Arc<dyn Fn(usize) -> AssistantMessage + Send + Sync>;

struct FakeProvider {
    calls: AtomicUsize,
    respond: Responder,
    /// Return an event stream that never produces an event (a provider call
    /// that never reaches a cancellation checkpoint).
    hang: bool,
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
        if self.hang {
            return AssistantMessageEventStream::new();
        }
        let message = (self.respond)(index);
        let stream = AssistantMessageEventStream::new();
        stream.push(AssistantMessageEventStream::done_event(
            TerminalReason::Stop,
            message,
        ));
        stream
    }
}

fn fake_registry(respond: Responder, hang: bool) -> Arc<ProviderRegistry> {
    let mut registry = ProviderRegistry::default();
    registry.set_provider(Arc::new(FakeProvider {
        calls: AtomicUsize::new(0),
        respond,
        hang,
    }));
    Arc::new(registry)
}

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

struct TempDir(PathBuf);

impl TempDir {
    fn new(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "pir-subagent-budget-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|duration| duration.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&path).expect("create temp dir");
        Self(path)
    }

    fn write_agent(&self, name: &str, frontmatter: &str, body: &str) -> PathBuf {
        let dir = self.0.join("agents");
        std::fs::create_dir_all(&dir).expect("create agents dir");
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

struct EchoTool;

impl AgentTool for EchoTool {
    fn name(&self) -> &'static str {
        "echo"
    }
    fn description(&self) -> &'static str {
        "echo"
    }
    fn run(
        &self,
        _args: &serde_json::Value,
        _ctx: &ToolContext,
    ) -> pir::agent::tool_execution::BoxFuture<'static, pir::agent::tool_batch::ToolResult> {
        Box::pin(async { pir::agent::tool_batch::ToolResult::ok("echo") })
    }
}

/// A tool that yields on every call so a runaway child cannot monopolize a
/// test runtime thread while the cancellation test is trying to abort it.
struct YieldingEchoTool;

impl AgentTool for YieldingEchoTool {
    fn name(&self) -> &'static str {
        "echo"
    }
    fn description(&self) -> &'static str {
        "echo"
    }
    fn run(
        &self,
        _args: &serde_json::Value,
        _ctx: &ToolContext,
    ) -> pir::agent::tool_execution::BoxFuture<'static, pir::agent::tool_batch::ToolResult> {
        Box::pin(async {
            tokio::time::sleep(Duration::from_millis(2)).await;
            pir::agent::tool_batch::ToolResult::ok("echo")
        })
    }
}

fn session_env(
    registry: Arc<ProviderRegistry>,
    user_agents_dir: PathBuf,
    tools: ToolRegistry,
) -> Arc<ToolSessionEnv> {
    Arc::new(ToolSessionEnv {
        read_ledger: std::sync::Arc::new(pir::agent::read_ledger::ReadLedger::default()),
        provider_registry: registry,
        model: fake_model(),
        thinking: ThinkingLevel::Off,
        session_id: None,
        session_file: None,
        base_tools: Arc::new(tools),
        has_ui: false,
        project_trusted: true,
        user_agents_dir: Some(user_agents_dir),
        agent_dir: None,
        credential: None,
        on_update: None,
        live_tools: None,
        active_override: None,
        background: None,
    })
}

fn tool_context(cwd: &Path, env: Arc<ToolSessionEnv>) -> ToolContext {
    ToolContext {
        cwd: cwd.to_path_buf(),
        signal: None,
        nested: None,
        session_env: Some(env),
        held_mutation_keys: None,
        on_update: None,
        background: false,
    }
}

// ---------------------------------------------------------------------------
// 1. Budget resolution
// ---------------------------------------------------------------------------

#[test]
fn budget_defaults_and_env_overrides() {
    let defaults = SubagentBudget::from_lookup(|_| None);
    assert_eq!(defaults.max_turns, DEFAULT_SUBAGENT_MAX_TURNS);
    assert_eq!(defaults.max_turns, 1_000);
    assert_eq!(
        defaults.wall_clock,
        Some(Duration::from_millis(DEFAULT_SUBAGENT_WALL_CLOCK_MS))
    );

    let overridden = SubagentBudget::from_lookup(|name| match name {
        SUBAGENT_MAX_TURNS_ENV => Some("7".to_owned()),
        SUBAGENT_WALL_CLOCK_ENV => Some("1500".to_owned()),
        _ => None,
    });
    assert_eq!(overridden.max_turns, 7);
    assert_eq!(overridden.wall_clock, Some(Duration::from_millis(1_500)));

    // `0` restores the pre-C1-03 unlimited semantics for both knobs.
    let unlimited = SubagentBudget::from_lookup(|name| match name {
        SUBAGENT_MAX_TURNS_ENV | SUBAGENT_WALL_CLOCK_ENV => Some("0".to_owned()),
        _ => None,
    });
    assert_eq!(unlimited.max_turns, usize::MAX);
    assert_eq!(unlimited.wall_clock, None);

    // Junk/negative values keep the safe defaults.
    let junk = SubagentBudget::from_lookup(|name| match name {
        SUBAGENT_MAX_TURNS_ENV | SUBAGENT_WALL_CLOCK_ENV => Some("not-a-number".to_owned()),
        _ => None,
    });
    assert_eq!(junk, defaults);
}

// ---------------------------------------------------------------------------
// 2. Turn cap
// ---------------------------------------------------------------------------

#[tokio::test]
async fn turn_cap_stops_a_runaway_child() {
    let temp = TempDir::new("turn-cap");
    temp.write_agent("looper.md", "name: looper\ndescription: loops\n", "loop");
    let calls = Arc::new(AtomicUsize::new(0));
    let calls_for_provider = Arc::clone(&calls);
    let registry = fake_registry(
        Arc::new(move |index| {
            calls_for_provider.fetch_add(1, Ordering::SeqCst);
            assistant(
                vec![tool_call_block(
                    &format!("call-{index}"),
                    "echo",
                    serde_json::json!({}),
                )],
                StopReason::ToolUse,
            )
        }),
        false,
    );
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(EchoTool));
    let env = session_env(registry, temp.0.join("agents"), tools);
    let ctx = tool_context(&temp.0, env);

    let tool = SubagentTool::default().with_budget(SubagentBudget {
        max_turns: 3,
        wall_clock: None,
        abort_grace: Duration::from_millis(100),
    });
    let result = tool
        .run(
            &serde_json::json!({ "agent": "looper", "task": "loop forever" }),
            &ctx,
        )
        .await;

    assert!(
        result.is_error,
        "expected an error, got: {}",
        result.content
    );
    assert!(
        result
            .content
            .contains("Stopped after the maximum of 3 turns"),
        "turn-cap error missing: {}",
        result.content
    );
    // Exactly three model requests ran; the fourth iteration is the synthetic
    // terminal, not another provider call.
    assert_eq!(calls.load(Ordering::SeqCst), 3);
}

// ---------------------------------------------------------------------------
// 3. Wall-clock budget
// ---------------------------------------------------------------------------

#[tokio::test]
async fn wall_clock_budget_stops_a_hung_child() {
    let temp = TempDir::new("wall-clock");
    temp.write_agent("slow.md", "name: slow\ndescription: hangs\n", "hang");
    let registry = fake_registry(
        Arc::new(|_| assistant(vec![text("unused")], StopReason::Stop)),
        true,
    );
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(EchoTool));
    let env = session_env(registry, temp.0.join("agents"), tools);
    let ctx = tool_context(&temp.0, env);

    let tool = SubagentTool::default().with_budget(SubagentBudget {
        max_turns: 1_000,
        wall_clock: Some(Duration::from_millis(50)),
        abort_grace: Duration::from_millis(50),
    });
    let started = std::time::Instant::now();
    let result = tool
        .run(
            &serde_json::json!({ "agent": "slow", "task": "hang" }),
            &ctx,
        )
        .await;

    assert!(
        result.is_error,
        "expected an error, got: {}",
        result.content
    );
    assert!(
        result
            .content
            .contains("Subagent wall-clock budget exceeded"),
        "wall-clock error missing: {}",
        result.content
    );
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "budget did not bound the run: {:?}",
        started.elapsed()
    );
}

// ---------------------------------------------------------------------------
// 4. Outer cancellation aborts parallel children and releases runtime slots
// ---------------------------------------------------------------------------

static RUNTIME_TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Build a plan-only parent handle. A `builtin:`-only path plan makes
/// [`pir::plugins::subagent::spawn_child_runtime`] take the
/// `ExtensionHandle::start_plan_only` arm: no QuickJS actor is built, but the
/// returned [`pir::plugins::subagent::ChildRuntime`] still owns a
/// `ChildRuntimeSlot`, exactly the resource whose release this test asserts.
fn plan_only_parent() -> pir::extensions::ExtensionHandle {
    let handle =
        pir::extensions::ExtensionHandle::start_plan_only(pir::extensions::ExtensionConfig {
            cwd: std::env::temp_dir(),
            mode: "headless".to_string(),
            has_ui: false,
            model: Some("fake-model".to_string()),
            session_id: Some("c1-03-cancel-test".to_string()),
            system_prompt: String::new(),
            is_idle: true,
            limits: Some(Default::default()),
        });
    handle.set_path_plan(vec![PathBuf::from("builtin:subagent")]);
    handle
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn outer_cancel_aborts_parallel_children_and_releases_runtime_slots() {
    // Child-runtime builds and the global slot counter are process-wide;
    // serialize this file's runtime-spawning tests.
    let _guard = RUNTIME_TEST_LOCK.lock().await;
    let baseline = active_child_runtime_slots();
    assert_eq!(
        baseline, 0,
        "test process must start with no child runtimes"
    );

    let temp = TempDir::new("outer-cancel");
    temp.write_agent("a.md", "name: a\ndescription: loops\n", "loop");
    temp.write_agent("b.md", "name: b\ndescription: loops\n", "loop");
    let parent = plan_only_parent();

    let calls = Arc::new(AtomicUsize::new(0));
    let calls_for_provider = Arc::clone(&calls);
    let registry = fake_registry(
        Arc::new(move |index| {
            calls_for_provider.fetch_add(1, Ordering::SeqCst);
            assistant(
                vec![tool_call_block(
                    &format!("call-{index}"),
                    "echo",
                    serde_json::json!({}),
                )],
                StopReason::ToolUse,
            )
        }),
        false,
    );
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(YieldingEchoTool));
    let env = session_env(registry, temp.0.join("agents"), tools);
    let ctx = tool_context(&temp.0, env);

    let tool = SubagentTool::with_extensions(Some(parent)).with_budget(SubagentBudget {
        max_turns: usize::MAX,
        wall_clock: None,
        abort_grace: Duration::from_millis(100),
    });
    // Poll the tool future like a host task; aborting this task is exactly
    // "the outer tool future was dropped".
    let driver = tokio::spawn(async move {
        tool.run(
            &serde_json::json!({
                "tasks": [
                    { "agent": "a", "task": "loop a" },
                    { "agent": "b", "task": "loop b" },
                ],
            }),
            &ctx,
        )
        .await
    });

    // Both children must have started and each must hold a runtime slot.
    let wait = std::time::Instant::now();
    loop {
        if calls.load(Ordering::SeqCst) >= 2 && active_child_runtime_slots() >= 2 {
            break;
        }
        assert!(
            wait.elapsed() < Duration::from_secs(20),
            "children did not start: calls={}, slots={}",
            calls.load(Ordering::SeqCst),
            active_child_runtime_slots()
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    driver.abort();
    let _ = driver.await;

    // The abort-on-drop JoinSet releases both slot guards. Poll because tokio
    // completes task cancellation asynchronously.
    let wait = std::time::Instant::now();
    while active_child_runtime_slots() > baseline {
        assert!(
            wait.elapsed() < Duration::from_secs(10),
            "child runtime slots leaked: {} > {}",
            active_child_runtime_slots(),
            baseline
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    // No child may continue calling the provider after the outer future was
    // dropped. Sample after the slots have settled, then again later.
    let settled_calls = calls.load(Ordering::SeqCst);
    tokio::time::sleep(Duration::from_millis(250)).await;
    assert_eq!(
        calls.load(Ordering::SeqCst),
        settled_calls,
        "a detached child kept running after the outer future was dropped"
    );
}
