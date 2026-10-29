//! Nesting-depth guard for the in-process `subagent` builtin (builtin audit
//! P1-6): entering a child at the limit is refused with an explicit error,
//! parallel siblings keep independent per-session depths, an aborted child
//! leaves no depth behind, and a three-level chain completes.
//!
//! The three-level chain builds three full in-process child sessions one
//! inside the other, so it runs on a thread with an explicit stack (see
//! [`CHAIN_STACK_BYTES`]) instead of libtest's 2 MiB default: the child
//! session machinery alone needs ~1.3 MiB and each nesting level adds its
//! share, which overflowed the default stack. The stack is a property of the
//! test harness, not of the depth guard — the bound and the refusal
//! assertions are unchanged.
//!
//! The model is faked with a scripted provider (same pattern as
//! `tests/subagent_builtin.rs`); no `~/.pi` configuration is consulted.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use pir::agent::tool_execution::{AgentTool, ToolContext, ToolRegistry, ToolSessionEnv};
use pir::ai::auth::ApiKeyAuth;
use pir::ai::event_stream::AssistantMessageEventStream;
use pir::ai::provider::{Provider, ProviderRegistry};
use pir::ai::types::*;
use pir::plugins::subagent::{
    install_subagent_depth, subagent_depth, SubagentTool, MAX_SUBAGENT_DEPTH,
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

fn text(value: &str) -> ContentBlock {
    ContentBlock::Text(TextContent {
        text: value.into(),
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

/// Text of every `tool_name` tool result present in `context`.
fn tool_result_texts(context: &Context, tool_name: &str) -> Vec<String> {
    context
        .messages
        .iter()
        .filter_map(|message| match message {
            Message::ToolResult(result) if result.tool_name == tool_name => Some(
                result
                    .content
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
        .collect()
}

/// Responder signature: (call index, request context) -> assistant message.
type Responder = Arc<dyn Fn(usize, &Context) -> AssistantMessage + Send + Sync>;

/// A fake provider that answers every stream with a scripted message.
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
/// an abortable "gated answer".
struct GatedProvider {
    entered: Arc<AtomicUsize>,
    gate: tokio::sync::watch::Receiver<bool>,
    release_aborted: Arc<AtomicBool>,
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
        _context: Context,
        _options: StreamOptions,
    ) -> AssistantMessageEventStream {
        self.entered.fetch_add(1, Ordering::SeqCst);
        let mut gate = self.gate.clone();
        let release_aborted = Arc::clone(&self.release_aborted);
        let stream = AssistantMessageEventStream::new();
        let pusher = stream.clone();
        tokio::spawn(async move {
            while !*gate.borrow() {
                if tokio::time::timeout(std::time::Duration::from_millis(20), gate.changed())
                    .await
                    .is_err()
                {
                    continue;
                }
            }
            let stop_reason = if release_aborted.load(Ordering::SeqCst) {
                StopReason::Aborted
            } else {
                StopReason::Stop
            };
            pusher.push(AssistantMessageEventStream::done_event(
                TerminalReason::Stop,
                assistant(vec![text("gated answer")], stop_reason),
            ));
        });
        stream
    }
}

/// A gated provider that answers with a scripted responder once the gate is
/// open. Used by the parallel-siblings test so both depth-1 siblings are
/// in-flight before either dispatches its nested child.
struct GatedScriptProvider {
    calls: AtomicUsize,
    entered: Arc<AtomicUsize>,
    gate: tokio::sync::watch::Receiver<bool>,
    respond: Responder,
}

impl Provider for GatedScriptProvider {
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
        self.entered.fetch_add(1, Ordering::SeqCst);
        let mut gate = self.gate.clone();
        let respond = Arc::clone(&self.respond);
        let stream = AssistantMessageEventStream::new();
        let pusher = stream.clone();
        tokio::spawn(async move {
            while !*gate.borrow() {
                if tokio::time::timeout(std::time::Duration::from_millis(20), gate.changed())
                    .await
                    .is_err()
                {
                    continue;
                }
            }
            let message = respond(index, &context);
            pusher.push(AssistantMessageEventStream::done_event(
                TerminalReason::Stop,
                message,
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
            "pir-subagent-depth-{label}-{}-{}",
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

/// A session env whose base registry carries `depth` plus the `subagent` tool,
/// i.e. exactly the state a child session of that depth runs under.
fn env_at_depth(
    registry: Arc<ProviderRegistry>,
    user_agents_dir: &Path,
    depth: usize,
) -> Arc<ToolSessionEnv> {
    let mut base = ToolRegistry::new();
    base.register(Arc::new(SubagentTool::default()));
    install_subagent_depth(&mut base, depth);
    Arc::new(ToolSessionEnv {
        provider_registry: registry,
        model: fake_model(),
        thinking: ThinkingLevel::Off,
        session_id: None,
        session_file: None,
        base_tools: Arc::new(base),
        has_ui: false,
        project_trusted: true,
        user_agents_dir: Some(user_agents_dir.to_path_buf()),
        agent_dir: None,
        credential: None,
        on_update: None,
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

async fn run_tool(
    args: serde_json::Value,
    ctx: &ToolContext,
) -> pir::agent::tool_batch::ToolResult {
    SubagentTool::default().run(&args, ctx).await
}

/// Nested child sessions discover agents through the PROCESS agent dir: a
/// loop-built [`ToolSessionEnv`] always carries `user_agents_dir: None`
/// (`AgentSession::prompt` builds it that way), so `subagent` falls back to
/// `PIR_AGENT_DIR/agents`. The chain/parallel tests scope that variable to
/// their own fixture; every test in this file takes this lock so a scoped
/// value is never visible to a concurrent reader (and the two scoping tests
/// cannot overwrite each other's value).
static TEST_ENV_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

struct AgentDirGuard {
    previous: Option<std::ffi::OsString>,
}

impl AgentDirGuard {
    fn set(path: &Path) -> Self {
        let previous = std::env::var_os("PIR_AGENT_DIR");
        std::env::set_var("PIR_AGENT_DIR", path);
        Self { previous }
    }
}

impl Drop for AgentDirGuard {
    fn drop(&mut self) {
        match self.previous.take() {
            Some(value) => std::env::set_var("PIR_AGENT_DIR", value),
            None => std::env::remove_var("PIR_AGENT_DIR"),
        }
    }
}

// ---------------------------------------------------------------------------
// (a) Test seam: entering at the limit refuses, depth 2 allows, state stays
//     at the dispatcher's own level after success and after failure.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn entering_at_the_depth_limit_refuses_and_leaves_no_state_behind() {
    let _env_lock = TEST_ENV_LOCK.lock().await;
    let temp = TempDir::new("seam");
    temp.write_agent("scout.md", "name: scout\ndescription: d\n", "b");
    let registry = fake_registry(Arc::new(|_, _| {
        assistant(vec![text("OK")], StopReason::Stop)
    }));

    // Root session (depth 0): the depth state reads 0 and dispatch works.
    let root = env_at_depth(Arc::clone(&registry), &temp.0.join("agents"), 0);
    assert_eq!(subagent_depth(&root.base_tools), 0);

    // The depth is NOT a registered tool: model-facing views never see it,
    // whatever exposure filter they apply (the session build strips `Hidden`
    // tools from the base registry; an earlier marker registered that way was
    // deleted there and the guard read 0 forever).
    assert!(
        !root.base_tools.names().contains(&"__pir_subagent_depth"),
        "depth marker must not be a registered tool: {:?}",
        root.base_tools.names()
    );
    let depth_two_registry = env_at_depth(Arc::clone(&registry), &temp.0.join("agents"), 2);
    let mut stripped = (*depth_two_registry.base_tools).clone();
    stripped.remove_exposure(pir::agent::tool_execution::ToolExposure::Hidden);
    assert_eq!(
        subagent_depth(&stripped),
        2,
        "depth must survive the session build's hidden-tool strip"
    );

    // Dispatcher at depth 1 enters a child at depth 2: allowed.
    let depth_one = env_at_depth(Arc::clone(&registry), &temp.0.join("agents"), 1);
    let ctx = tool_context(&temp.0, Arc::clone(&depth_one), None);
    let result = run_tool(
        serde_json::json!({ "agent": "scout", "task": "allowed" }),
        &ctx,
    )
    .await;
    assert!(!result.is_error, "content: {}", result.content);
    assert_eq!(result.content, "OK");
    // Success did not change (or leak) the dispatcher's own level.
    assert_eq!(subagent_depth(&depth_one.base_tools), 1);

    // Dispatcher at depth 2 would enter a child at depth 3 (the limit):
    // refused with the explicit limit message, no panic.
    let depth_two = env_at_depth(Arc::clone(&registry), &temp.0.join("agents"), 2);
    let ctx = tool_context(&temp.0, Arc::clone(&depth_two), None);
    let result = run_tool(
        serde_json::json!({ "agent": "scout", "task": "refused" }),
        &ctx,
    )
    .await;
    assert!(result.is_error, "content: {}", result.content);
    assert!(
        result.content.contains(&format!(
            "Subagent nesting depth limit ({MAX_SUBAGENT_DEPTH}) reached"
        )),
        "content: {}",
        result.content
    );
    // The refused entry consumed nothing and changed nothing.
    assert_eq!(subagent_depth(&depth_two.base_tools), 2);

    // A fresh root run after the refusal is not falsely limited, and the root
    // still reads depth 0 (the "counter returns to 0" invariant).
    assert_eq!(subagent_depth(&root.base_tools), 0);
    let ctx = tool_context(&temp.0, Arc::clone(&root), None);
    let result = run_tool(
        serde_json::json!({ "agent": "scout", "task": "again" }),
        &ctx,
    )
    .await;
    assert!(!result.is_error, "content: {}", result.content);
    assert_eq!(subagent_depth(&root.base_tools), 0);
}

// ---------------------------------------------------------------------------
// (b) Three-level chain: deepest call returns the limit error and the whole
//     run completes (no stack overflow).
// ---------------------------------------------------------------------------

/// Stack for the three-level chain: three nested in-process child sessions.
///
/// Measured on the depth guard's own path (the marker intact): the chain
/// needs ~2.9 MiB, of which the child session machinery alone is ~1.3 MiB at
/// ONE level. That is product cost outside the nesting recursion itself
/// (session build, prompt assembly, the stream), so the chain runs on an
/// explicit 8 MiB thread rather than libtest's 2 MiB default. 8 MiB leaves
/// ~2.7x headroom; the depth BOUND and every assertion below are unchanged.
const CHAIN_STACK_BYTES: usize = 8 * 1024 * 1024;

/// Run one async test body on a thread with [`CHAIN_STACK_BYTES`] and a
/// current-thread tokio runtime (same shape as the other big-stack tests in
/// this repo, e.g. `tests/subagent_builtin.rs`), instead of relying on the
/// runner's `RUST_MIN_STACK`.
fn run_with_chain_stack<F, T>(body: F) -> T
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    std::thread::Builder::new()
        .stack_size(CHAIN_STACK_BYTES)
        .spawn(body)
        .expect("spawn chain-stack thread")
        .join()
        .expect("chain-stack thread panicked")
}

#[test]
fn three_level_chain_refuses_the_next_level() {
    // The test env lock and the process-wide `PIR_AGENT_DIR` scoping are set
    // INSIDE the spawned thread and held for the whole body, exactly as the
    // former async body did.
    run_with_chain_stack(|| {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        runtime.block_on(three_level_chain_body());
    });
}

async fn three_level_chain_body() {
    let _env_lock = TEST_ENV_LOCK.lock().await;
    let temp = TempDir::new("chain");
    for agent in ["a", "b", "c"] {
        temp.write_agent(
            &format!("{agent}.md"),
            &format!("name: {agent}\ndescription: d\n"),
            "b",
        );
    }
    // Nested children resolve their agent dir from the process env.
    let _agent_dir = AgentDirGuard::set(&temp.0);

    let observed_results: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let observed_for_responder = Arc::clone(&observed_results);
    let observed_prompts: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let prompts_for_responder = Arc::clone(&observed_prompts);
    let registry = fake_registry(Arc::new(move |_, context| {
        let last_user = user_text(context);
        prompts_for_responder
            .lock()
            .unwrap()
            .push(last_user.clone());
        let tool_results = tool_result_texts(context, "subagent");
        if !tool_results.is_empty() {
            observed_for_responder
                .lock()
                .unwrap()
                .extend(tool_results.clone());
        }
        if has_tool_result(context) {
            // After the (successful or refused) nested call: finish this level.
            return assistant(vec![text(&format!("done:{last_user}"))], StopReason::Stop);
        }
        // Root -> a (depth 1) -> b (depth 2) -> c (depth 3, refused).
        let (next_agent, next_task) = match last_user.as_str() {
            "T1" => ("b", "T2"),
            "T2" => ("c", "T3"),
            other => {
                return assistant(vec![text(&format!("UNEXPECTED:{other}"))], StopReason::Stop)
            }
        };
        assistant(
            vec![tool_call_block(
                "nested",
                "subagent",
                serde_json::json!({ "agent": next_agent, "task": next_task }),
            )],
            StopReason::ToolUse,
        )
    }));

    // The chain runs on the explicit chain-stack thread; the depth bound is
    // what must stop it, not the stack (see [`CHAIN_STACK_BYTES`]).
    let env = env_at_depth(registry, &temp.0.join("agents"), 0);
    let ctx = tool_context(&temp.0, env, None);
    let result = run_tool(serde_json::json!({ "agent": "a", "task": "T1" }), &ctx).await;
    assert!(!result.is_error, "content: {}", result.content);
    assert_eq!(result.content, "done:T1");

    // Exactly one nested call hit the limit (the one dispatched from the
    // depth-2 child); the whole chain still completed.
    let observed = observed_results.lock().unwrap().clone();
    let refusals: Vec<&String> = observed
        .iter()
        .filter(|text| text.contains("Subagent nesting depth limit (3) reached"))
        .collect();
    assert_eq!(refusals.len(), 1, "observed: {observed:?}");
    assert!(
        observed.iter().any(|text| text == "done:T2"),
        "observed: {observed:?}"
    );
    // The refused level never ran a model request (no silent truncation of
    // the error: it came from the depth guard before any child work).
    let prompts = observed_prompts.lock().unwrap().clone();
    assert!(
        !prompts.iter().any(|prompt| prompt == "T3"),
        "prompts: {prompts:?}"
    );
}

// ---------------------------------------------------------------------------
// (c) Parallel siblings at depth 1 do not affect each other's depth.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn parallel_siblings_at_depth_one_keep_independent_depths() {
    let _env_lock = TEST_ENV_LOCK.lock().await;
    let temp = TempDir::new("parallel");
    temp.write_agent("worker.md", "name: worker\ndescription: d\n", "b");
    // Nested children resolve their agent dir from the process env.
    let _agent_dir = AgentDirGuard::set(&temp.0);

    let entered = Arc::new(AtomicUsize::new(0));
    let (gate_tx, gate_rx) = tokio::sync::watch::channel(false);
    let registry = {
        let mut registry = ProviderRegistry::default();
        registry.set_provider(Arc::new(GatedScriptProvider {
            calls: AtomicUsize::new(0),
            entered: Arc::clone(&entered),
            gate: gate_rx,
            respond: Arc::new(|_, context| {
                let last_user = user_text(context);
                if has_tool_result(context) {
                    return assistant(vec![text(&format!("done:{last_user}"))], StopReason::Stop);
                }
                match last_user.as_str() {
                    "P1" => assistant(
                        vec![tool_call_block(
                            "nested",
                            "subagent",
                            serde_json::json!({ "agent": "worker", "task": "N1" }),
                        )],
                        StopReason::ToolUse,
                    ),
                    "P2" => assistant(
                        vec![tool_call_block(
                            "nested",
                            "subagent",
                            serde_json::json!({ "agent": "worker", "task": "N2" }),
                        )],
                        StopReason::ToolUse,
                    ),
                    "N1" | "N2" => {
                        assistant(vec![text(&format!("ok:{last_user}"))], StopReason::Stop)
                    }
                    other => {
                        assistant(vec![text(&format!("UNEXPECTED:{other}"))], StopReason::Stop)
                    }
                }
            }),
        }));
        Arc::new(registry)
    };

    // Open the gate only after BOTH depth-1 siblings are in flight, so their
    // nested dispatches overlap: a shared/global active-run counter would be
    // inflated to 2 here and falsely refuse the depth-2 children.
    let entered_watch = Arc::clone(&entered);
    tokio::spawn(async move {
        for _ in 0..2000 {
            if entered_watch.load(Ordering::SeqCst) >= 2 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        let _ = gate_tx.send(true);
    });

    let env = env_at_depth(registry, &temp.0.join("agents"), 0);
    let ctx = tool_context(&temp.0, env, None);
    let result = run_tool(
        serde_json::json!({
            "tasks": [
                { "agent": "worker", "task": "P1" },
                { "agent": "worker", "task": "P2" },
            ],
        }),
        &ctx,
    )
    .await;
    assert!(!result.is_error, "content: {}", result.content);
    assert!(
        result.content.starts_with("Parallel: 2/2 succeeded\n\n"),
        "content: {}",
        result.content
    );
    assert!(
        result.content.contains("done:P1"),
        "content: {}",
        result.content
    );
    assert!(
        result.content.contains("done:P2"),
        "content: {}",
        result.content
    );
    assert!(
        !result.content.contains("nesting depth limit"),
        "siblings corrupted each other's depth: {}",
        result.content
    );
}

// ---------------------------------------------------------------------------
// (d) An aborted child leaves no depth behind (no false refusal on rerun).
// ---------------------------------------------------------------------------

#[tokio::test]
async fn aborted_child_leaves_no_depth_behind() {
    let _env_lock = TEST_ENV_LOCK.lock().await;
    let temp = TempDir::new("abort");
    temp.write_agent("scout.md", "name: scout\ndescription: d\n", "b");

    let entered = Arc::new(AtomicUsize::new(0));
    let release_aborted = Arc::new(AtomicBool::new(false));
    let (gate_tx, gate_rx) = tokio::sync::watch::channel(false);
    let registry: Arc<ProviderRegistry> = {
        let mut registry = ProviderRegistry::default();
        registry.set_provider(Arc::new(GatedProvider {
            entered: Arc::clone(&entered),
            gate: gate_rx,
            release_aborted: Arc::clone(&release_aborted),
        }));
        Arc::new(registry)
    };

    // Dispatcher at depth 1: a child at depth 2 is allowed.
    let env = env_at_depth(registry, &temp.0.join("agents"), 1);
    let signal = AbortSignal::new();
    let ctx = tool_context(&temp.0, Arc::clone(&env), Some(signal.clone()));
    let run_task = tokio::spawn(async move {
        run_tool(serde_json::json!({ "agent": "scout", "task": "t" }), &ctx).await
    });

    for _ in 0..2000 {
        if entered.load(Ordering::SeqCst) > 0 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    assert!(entered.load(Ordering::SeqCst) > 0, "child never started");
    signal.abort();
    release_aborted.store(true, Ordering::SeqCst);
    let _ = gate_tx.send(true);

    let result = run_task.await.expect("tool task");
    assert!(result.is_error, "content: {}", result.content);
    assert_eq!(result.content, "Subagent was aborted");
    // The aborted run owned no shared counter slot: the dispatcher's depth is
    // untouched.
    assert_eq!(subagent_depth(&env.base_tools), 1);

    // Rerun at the same depth: allowed, no false refusal.
    release_aborted.store(false, Ordering::SeqCst);
    let ctx = tool_context(&temp.0, Arc::clone(&env), None);
    let result = run_tool(
        serde_json::json!({ "agent": "scout", "task": "again" }),
        &ctx,
    )
    .await;
    assert!(!result.is_error, "content: {}", result.content);
    assert_eq!(result.content, "gated answer");
}
