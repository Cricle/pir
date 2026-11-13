//! Tests for the `subagent` builtin plugin (upstream
//! `packages/coding-agent/examples/extensions/subagent`): discovery, mode
//! validation, single/parallel/chain runs against a fake provider, the
//! parallel caps + aggregate format, chain `{previous}` + stop-at-failure,
//! output-cap truncation, the details shape, and cancellation.
//!
//! The model is faked with a scripted provider (same pattern as
//! `tests/agent_session.rs`); no `~/.pi` configuration is consulted — the
//! user-agent directory is injected through the session env.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use pir::agent::agent_loop::AgentEvent;
use pir::agent::tool_execution::{
    AgentTool, ToolContext, ToolCredential, ToolRegistry, ToolSessionEnv, ToolUpdateFn,
};
use pir::agent_session::{AgentSession, AgentSessionConfig};
use pir::ai::auth::ApiKeyAuth;
use pir::ai::event_stream::AssistantMessageEventStream;
use pir::ai::provider::{Provider, ProviderRegistry};
use pir::ai::types::*;
use pir::plugins::subagent::{
    aggregate_parallel_text, apply_previous, discover_agents, get_final_output, is_failed,
    load_agents_from_dir, parse_agent_markdown, result_output, truncate_parallel_output,
    AgentScope, SubagentTool,
};
use pir::plugins::{builtin_extension_def, instantiate_builtin};

// ---------------------------------------------------------------------------
// Fakes
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

/// A fake provider whose streams block until the gate opens, then answer.
struct GatedProvider {
    entered: Arc<AtomicUsize>,
    gate: tokio::sync::watch::Receiver<bool>,
    /// When set before the gate opens, the released message is aborted.
    release_aborted: Arc<std::sync::atomic::AtomicBool>,
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

/// Recorded child request: the request's last user text and its `reasoning`
/// (thinking) option.
type ObservedThinking = (String, Option<ThinkingLevel>);

/// A fake provider that records the `reasoning` option of every request
/// together with the request's last user text, so a CHILD session's thinking
/// level is observable.
struct ThinkingRecordingProvider {
    calls: AtomicUsize,
    observed: Arc<Mutex<Vec<ObservedThinking>>>,
}

impl Provider for ThinkingRecordingProvider {
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
        options: StreamOptions,
    ) -> AssistantMessageEventStream {
        let _ = self.calls.fetch_add(1, Ordering::SeqCst);
        self.observed
            .lock()
            .unwrap()
            .push((user_text(&context), options.reasoning));
        let stream = AssistantMessageEventStream::new();
        stream.push(AssistantMessageEventStream::done_event(
            TerminalReason::Stop,
            assistant(vec![text("thinking answer")], StopReason::Stop),
        ));
        stream
    }
}

/// A fake provider that records the `api_key` of every request together with
/// the request's last user text, so a CHILD session's inherited credential
/// (and the agent directory it resolved from) is observable. `respond` scripts
/// the answer (the loop test turns the first parent request into a subagent
/// call).
type Observed = Arc<Mutex<Vec<(String, Option<String>)>>>;

struct KeyRecordingProvider {
    model: Model,
    observed: Observed,
    respond: Responder,
    calls: AtomicUsize,
}

impl Provider for KeyRecordingProvider {
    // The fake provider's id is its configured provider name; the `model.id`
    // field is the model name and is not what this trait method returns.
    #[allow(clippy::misnamed_getters)]
    fn id(&self) -> &str {
        &self.model.provider
    }
    fn name(&self) -> &str {
        "Key recording provider"
    }
    fn get_models(&self) -> Vec<Model> {
        vec![self.model.clone()]
    }
    fn api_key_auth(&self) -> Vec<ApiKeyAuth> {
        vec![ApiKeyAuth::new("fake key", vec!["FAKE_TEST_KEY".into()])]
    }
    fn stream(
        &self,
        _model: &Model,
        context: Context,
        options: StreamOptions,
    ) -> AssistantMessageEventStream {
        self.observed
            .lock()
            .unwrap()
            .push((user_text(&context), options.api_key));
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

fn other_model() -> Model {
    let mut model = fake_model();
    model.id = "other-model".into();
    model.name = "Other model".into();
    model.provider = "other".into();
    model
}

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

struct TempDir(PathBuf);

impl TempDir {
    fn new(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "pir-subagent-{label}-{}-{}",
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

    fn write_project_agent(&self, name: &str, frontmatter: &str, body: &str) -> PathBuf {
        let dir = self.0.join(".pi").join("agents");
        std::fs::create_dir_all(&dir).expect("create project agents dir");
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

fn session_env(
    registry: Arc<ProviderRegistry>,
    user_agents_dir: Option<PathBuf>,
) -> Arc<ToolSessionEnv> {
    env_with(registry, user_agents_dir, false, true)
}

fn env_with(
    registry: Arc<ProviderRegistry>,
    user_agents_dir: Option<PathBuf>,
    has_ui: bool,
    project_trusted: bool,
) -> Arc<ToolSessionEnv> {
    Arc::new(ToolSessionEnv {
        live_model: None,
        live_thinking: None,
        read_ledger: std::sync::Arc::new(pir::agent::read_ledger::ReadLedger::default()),
        provider_registry: registry,
        model: fake_model(),
        thinking: ThinkingLevel::Off,
        session_id: None,
        session_file: None,
        base_tools: Arc::new(ToolRegistry::new()),
        has_ui,
        project_trusted,
        user_agents_dir,
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

// ---------------------------------------------------------------------------
// Discovery (upstream agents.ts)
// ---------------------------------------------------------------------------

#[test]
fn parses_frontmatter_with_string_and_array_tools() {
    let parsed = parse_agent_markdown(
        "---\nname: scout\ndescription: Fast recon\ntools: read, bash\n---\n\nBe brief.\n",
    )
    .expect("parses");
    assert_eq!(parsed.name.as_deref(), Some("scout"));
    assert_eq!(parsed.description.as_deref(), Some("Fast recon"));
    assert_eq!(
        parsed.tools,
        Some(vec!["read".to_owned(), "bash".to_owned()])
    );
    assert_eq!(parsed.body.trim(), "Be brief.");

    let parsed = parse_agent_markdown(
        "---\nname: a\ndescription: b\ntools:\n  - read\n  - grep\nmodel: fake/x\n---\nbody",
    )
    .expect("parses array tools");
    assert_eq!(
        parsed.tools,
        Some(vec!["read".to_owned(), "grep".to_owned()])
    );
    assert_eq!(parsed.model.as_deref(), Some("fake/x"));
}

#[test]
fn files_without_required_frontmatter_are_skipped() {
    assert!(parse_agent_markdown("no frontmatter").is_none());
    assert!(parse_agent_markdown("---\nname: only-name\n---\nbody").is_none());
    assert!(parse_agent_markdown("---\ndescription: only-desc\n---\nbody").is_none());
}

#[test]
fn discovery_reads_scopes_and_project_overrides_user() {
    let temp = TempDir::new("discovery");
    temp.write_agent(
        "scout.md",
        "name: scout\ndescription: user scout\n",
        "user body",
    );
    temp.write_agent("shared.md", "name: shared\ndescription: user shared\n", "u");
    temp.write_agent("broken.md", "description: no name\n", "x");
    temp.write_agent("notes.txt", "name: ignored\ndescription: not md\n", "y");
    temp.write_project_agent(
        "scout.md",
        "name: scout\ndescription: project scout\n",
        "project body",
    );

    let user_dir = temp.0.join("agents");
    let empty = TempDir::new("discovery-empty");

    // user scope: project agents are not loaded.
    let user = discover_agents(&empty.0, AgentScope::User, &user_dir);
    assert_eq!(user.agents.len(), 2);
    assert!(user.agents.iter().all(|a| a.source.as_str() == "user"));

    // project scope: only project agents, from the nearest .pi/agents walk-up.
    let project = discover_agents(&temp.0, AgentScope::Project, &user_dir);
    assert_eq!(project.agents.len(), 1);
    assert_eq!(project.agents[0].description, "project scout");
    assert_eq!(
        project.project_agents_dir,
        Some(temp.0.join(".pi").join("agents"))
    );

    // both scope: project agents override same-named user agents.
    let both = discover_agents(&temp.0, AgentScope::Both, &user_dir);
    assert_eq!(both.agents.len(), 2);
    let scout = both
        .agents
        .iter()
        .find(|a| a.name == "scout")
        .expect("scout");
    assert_eq!(scout.description, "project scout");
    assert_eq!(scout.source.as_str(), "project");
    let shared = both
        .agents
        .iter()
        .find(|a| a.name == "shared")
        .expect("shared");
    assert_eq!(shared.source.as_str(), "user");
}

#[test]
fn discovery_rereads_on_every_call() {
    let temp = TempDir::new("reread");
    let user_dir = temp.0.join("agents");
    let empty = TempDir::new("reread-empty");
    let first = discover_agents(&empty.0, AgentScope::User, &user_dir);
    // No user/project files anywhere: the shipped built-in agents are offered
    // so a fresh install can use `subagent` out of the box.
    let mut first_names: Vec<&str> = first.agents.iter().map(|a| a.name.as_str()).collect();
    first_names.sort_unstable();
    assert_eq!(first_names, vec!["planner", "reviewer", "scout", "worker"]);
    temp.write_agent("late.md", "name: late\ndescription: added later\n", "b");
    let second = discover_agents(&empty.0, AgentScope::User, &user_dir);
    assert_eq!(second.agents.len(), 1);
    assert_eq!(second.agents[0].name, "late");
}

#[test]
fn builtin_agents_are_offered_only_when_no_agent_files_exist() {
    let temp = TempDir::new("builtin-defaults");
    let user_dir = temp.0.join("agents");
    let empty = TempDir::new("builtin-empty");
    let builtins = discover_agents(&empty.0, AgentScope::User, &user_dir);
    assert_eq!(builtins.agents.len(), 4);
    assert!(builtins
        .agents
        .iter()
        .all(|agent| agent.model.is_none() && agent.file_path.starts_with("<builtin>")));

    temp.write_agent("scout.md", "name: scout\ndescription: mine\n", "b");
    let configured = discover_agents(&empty.0, AgentScope::User, &user_dir);
    assert_eq!(configured.agents.len(), 1);
    assert_eq!(configured.agents[0].description, "mine");
}

#[test]
fn project_agents_dir_walks_up_to_nearest() {
    let temp = TempDir::new("walkup");
    temp.write_project_agent("a.md", "name: a\ndescription: d\n", "b");
    let nested = temp.0.join("deep").join("deeper");
    std::fs::create_dir_all(&nested).expect("nested dirs");
    let found = pir::plugins::subagent::find_nearest_project_agents_dir(&nested);
    assert_eq!(found, Some(temp.0.join(".pi").join("agents")));
    let none = TempDir::new("walkup-none");
    assert!(pir::plugins::subagent::find_nearest_project_agents_dir(&none.0).is_none());
}

#[test]
fn load_agents_from_dir_skips_unreadable_and_invalid() {
    let temp = TempDir::new("loaddir");
    let dir = temp.0.join("agents");
    std::fs::create_dir_all(&dir).expect("dir");
    std::fs::write(dir.join("ok.md"), "---\nname: ok\ndescription: d\n---\nb").expect("ok");
    std::fs::write(dir.join("bad.md"), "plain text").expect("bad");
    let agents = load_agents_from_dir(&dir, pir::plugins::subagent::AgentSource::User);
    assert_eq!(agents.len(), 1);
    assert_eq!(agents[0].name, "ok");
}

// ---------------------------------------------------------------------------
// Pure helpers
// ---------------------------------------------------------------------------

#[test]
fn previous_placeholder_is_replaced_everywhere() {
    assert_eq!(
        apply_previous("use {previous} and {previous} again", "OUT"),
        "use OUT and OUT again"
    );
    assert_eq!(apply_previous("no placeholder", "OUT"), "no placeholder");
}

#[test]
fn parallel_output_is_truncated_at_50kb_utf8_boundary() {
    let small = "short output";
    assert_eq!(truncate_parallel_output(small), small);

    // Multibyte characters must not be split.
    let multibyte = "ä".repeat(20_000); // 2 bytes each = 40_000 bytes... 50KB cap splits at 25_000 bytes
    let long = format!("{multibyte}{}", "x".repeat(60_000));
    let truncated = truncate_parallel_output(&long);
    assert!(truncated.len() < long.len());
    assert!(truncated.contains("[Output truncated:"));
    assert!(truncated.is_char_boundary(truncated.len() - "[Output truncated:".len() - 2));

    let ascii = "a".repeat(60_000);
    let truncated = truncate_parallel_output(&ascii);
    assert!(truncated.starts_with(&"a".repeat(1000)));
    assert!(truncated.contains(&format!(
        "[Output truncated: {} bytes omitted.",
        60_000 - 50 * 1024
    )));
}

#[test]
fn aggregate_parallel_text_formats_status_blocks() {
    use pir::plugins::subagent::SingleResult;
    let ok = SingleResult {
        agent: "scout".into(),
        agent_source: "user",
        task: "t".into(),
        messages: vec![Message::Assistant(assistant(
            vec![text("found it")],
            StopReason::Stop,
        ))],
        pending: false,
        ..SingleResult::placeholder("scout", "t")
    };
    let mut bad = SingleResult::placeholder("planner", "t2");
    bad.pending = false;
    bad.stop_reason = Some("aborted".into());
    let text = aggregate_parallel_text(&[ok, bad]);
    assert!(text.starts_with("Parallel: 1/2 succeeded\n\n"));
    assert!(text.contains("### [scout] completed\n\nfound it"));
    assert!(text.contains("### [planner] failed (aborted)"));
    assert!(text.contains("\n\n---\n\n"));
}

#[test]
fn failure_detection_and_output_precedence() {
    use pir::plugins::subagent::SingleResult;
    let mut result = SingleResult::placeholder("a", "t");
    result.pending = false;
    assert!(!is_failed(&result));

    // errorMessage wins over stderr and final output.
    result.error_message = Some("model exploded".into());
    result.stderr = Some("run error".into());
    result.messages = vec![Message::Assistant(assistant(
        vec![text("final text")],
        StopReason::Error,
    ))];
    assert!(is_failed(&result));
    assert_eq!(result_output(&result), "model exploded");

    // stderr next.
    result.error_message = None;
    assert_eq!(result_output(&result), "run error");

    // final output next.
    result.stderr = None;
    assert_eq!(result_output(&result), "final text");

    // "(no output)" last.
    result.messages = Vec::new();
    assert_eq!(result_output(&result), "(no output)");
}

#[test]
fn final_output_picks_the_last_assistant_text() {
    let messages = vec![
        Message::Assistant(assistant(vec![text("first")], StopReason::Stop)),
        Message::Assistant(assistant(vec![text("second")], StopReason::Stop)),
    ];
    assert_eq!(get_final_output(&messages).as_deref(), Some("second"));
    assert_eq!(get_final_output(&[]), None);
}

// ---------------------------------------------------------------------------
// Mode validation
// ---------------------------------------------------------------------------

#[tokio::test]
async fn zero_or_multiple_modes_error_with_available_agents() {
    let temp = TempDir::new("modes");
    temp.write_agent("scout.md", "name: scout\ndescription: d\n", "b");
    let registry = fake_registry(Arc::new(|_, _| {
        assistant(vec![text("unused")], StopReason::Stop)
    }));
    let env = session_env(registry, Some(temp.0.join("agents")));
    let ctx = tool_context(&temp.0, env, None);

    // Zero modes.
    let result = run_tool(serde_json::json!({}), &ctx).await;
    assert!(!result.is_error);
    assert!(result
        .content
        .contains("Invalid parameters. Provide exactly one mode."));
    assert!(result.content.contains("Available agents: scout (user)"));
    assert!(result.details.is_some());

    // Half a single mode.
    let result = run_tool(serde_json::json!({ "agent": "scout" }), &ctx).await;
    assert!(result.content.contains("Invalid parameters"));

    // Two modes at once.
    let result = run_tool(
        serde_json::json!({
            "agent": "scout",
            "task": "t",
            "tasks": [{ "agent": "scout", "task": "t" }],
        }),
        &ctx,
    )
    .await;
    assert!(!result.is_error);
    assert!(result.content.contains("Provide exactly one mode"));

    // Three modes at once.
    let result = run_tool(
        serde_json::json!({
            "agent": "scout",
            "task": "t",
            "tasks": [{ "agent": "scout", "task": "t" }],
            "chain": [{ "agent": "scout", "task": "t" }],
        }),
        &ctx,
    )
    .await;
    assert!(result.content.contains("Provide exactly one mode"));
}

#[tokio::test]
async fn invalid_agent_scope_is_rejected() {
    let temp = TempDir::new("scope");
    let registry = fake_registry(Arc::new(|_, _| {
        assistant(vec![text("unused")], StopReason::Stop)
    }));
    let env = session_env(registry, Some(temp.0.join("agents")));
    let ctx = tool_context(&temp.0, env, None);
    let result = run_tool(
        serde_json::json!({ "agent": "a", "task": "t", "agentScope": "galaxy" }),
        &ctx,
    )
    .await;
    assert!(result.is_error);
    assert!(result.content.contains("Invalid agentScope"));
}

#[tokio::test]
async fn too_many_parallel_tasks_is_rejected() {
    let temp = TempDir::new("caps");
    let registry = fake_registry(Arc::new(|_, _| {
        assistant(vec![text("unused")], StopReason::Stop)
    }));
    let env = session_env(registry, Some(temp.0.join("agents")));
    let ctx = tool_context(&temp.0, env, None);
    let tasks: Vec<_> = (0..9)
        .map(|i| serde_json::json!({ "agent": "a", "task": i.to_string() }))
        .collect();
    let result = run_tool(serde_json::json!({ "tasks": tasks }), &ctx).await;
    assert!(!result.is_error);
    assert_eq!(
        result.content,
        "Too many parallel tasks (9). Max is 8.".to_owned()
    );
}

// ---------------------------------------------------------------------------
// Single mode against the fake flow
// ---------------------------------------------------------------------------

#[tokio::test]
async fn single_mode_runs_an_isolated_child_and_returns_final_output() {
    let temp = TempDir::new("single");
    temp.write_agent(
        "scout.md",
        "name: scout\ndescription: recon\nmodel: fake/fake-model\n",
        "SCOUT SYSTEM PROMPT",
    );
    let registry = fake_registry(Arc::new(|_, context| {
        let last_user = user_text(context);
        if has_tool_result(context) {
            // Parent turn 2: after the tool result.
            assistant(vec![text("parent done")], StopReason::Stop)
        } else if last_user == "find the auth code" {
            // Child run: the task is the last user message.
            assistant(vec![text("CHILD_RESULT")], StopReason::Stop)
        } else {
            // Parent turn 1: dispatch the subagent call.
            assistant(
                vec![tool_call_block(
                    "call-1",
                    "subagent",
                    serde_json::json!({ "agent": "scout", "task": "find the auth code" }),
                )],
                StopReason::ToolUse,
            )
        }
    }));
    let env = session_env(registry, Some(temp.0.join("agents")));

    // Direct tool-level run: the child session is fully in-process.
    let ctx = tool_context(&temp.0, Arc::clone(&env), None);
    let result = run_tool(
        serde_json::json!({ "agent": "scout", "task": "find the auth code" }),
        &ctx,
    )
    .await;
    assert!(!result.is_error, "content: {}", result.content);
    assert_eq!(result.content, "CHILD_RESULT");

    // Details shape (contract item 9).
    let details = result.details.expect("details");
    assert_eq!(details["mode"], "single");
    assert_eq!(details["agentScope"], "user");
    assert_eq!(details["projectAgentsDir"], serde_json::Value::Null);
    let results = details["results"].as_array().expect("results");
    assert_eq!(results.len(), 1);
    let entry = &results[0];
    assert_eq!(entry["agent"], "scout");
    assert_eq!(entry["agentSource"], "user");
    assert_eq!(entry["task"], "find the auth code");
    let messages = entry["messages"].as_array().expect("messages");
    assert!(
        messages.len() >= 2,
        "child transcript should include user + assistant"
    );
    assert_eq!(messages[0]["role"], "user");
    let usage = &entry["usage"];
    assert_eq!(usage["turns"], 1);
    assert_eq!(usage["input"], 10);
    assert_eq!(usage["output"], 5);
    assert_eq!(usage["cost"], 0.25);
    assert_eq!(usage["contextTokens"], 15);
    assert_eq!(entry["model"], "fake-model");
    assert_eq!(entry["stopReason"], "stop");
}

#[tokio::test]
async fn single_mode_agent_body_is_appended_to_system_prompt() {
    let temp = TempDir::new("sysprompt");
    temp.write_agent(
        "scout.md",
        "name: scout\ndescription: recon\n",
        "SCOUT_SYSTEM_PROMPT_MARKER",
    );
    let observed: Arc<Mutex<Vec<Option<String>>>> = Arc::new(Mutex::new(Vec::new()));
    let observed_for_responder = observed.clone();
    let registry = fake_registry(Arc::new(move |_, context| {
        observed_for_responder
            .lock()
            .unwrap()
            .push(context.system_prompt.clone());
        assistant(vec![text("done")], StopReason::Stop)
    }));
    let env = session_env(registry, Some(temp.0.join("agents")));
    let ctx = tool_context(&temp.0, env, None);
    let result = run_tool(serde_json::json!({ "agent": "scout", "task": "t" }), &ctx).await;
    assert!(!result.is_error, "content: {}", result.content);
    let prompts = observed.lock().unwrap().clone();
    let prompt = prompts
        .last()
        .expect("child request")
        .clone()
        .expect("system prompt");
    assert!(
        prompt.contains("SCOUT_SYSTEM_PROMPT_MARKER"),
        "prompt: {prompt}"
    );
}

#[tokio::test]
async fn bare_workspace_child_still_gets_the_default_preamble() {
    // Audit M5: upstream always gives the child the default prompt, with the
    // agent body as `appendSystemPrompt`, so a bare workspace — no context
    // files, no skills, no MCP — must not collapse the child's system prompt
    // to the agent body alone.
    let temp = TempDir::new("bare-sysprompt");
    // The skill scan also reads `~/.agents/skills` (home-anchored, NOT gated
    // by the agent dir), so point `HOME` at an empty dir for this test —
    // otherwise a developer machine's global skills make the workspace
    // non-bare and the body-only arm below is never exercised. Restored on
    // drop so the other tests keep their ambient environment.
    struct HomeGuard(Option<std::ffi::OsString>);
    impl Drop for HomeGuard {
        fn drop(&mut self) {
            match self.0.take() {
                Some(home) => std::env::set_var("HOME", home),
                None => std::env::remove_var("HOME"),
            }
        }
    }
    let empty_home = temp.0.join("home");
    std::fs::create_dir_all(&empty_home).expect("create empty home");
    let _home = HomeGuard(std::env::var_os("HOME"));
    std::env::set_var("HOME", &empty_home);
    temp.write_agent(
        "scout.md",
        "name: scout\ndescription: recon\n",
        "SCOUT_SYSTEM_PROMPT_MARKER",
    );
    let observed: Arc<Mutex<Vec<Option<String>>>> = Arc::new(Mutex::new(Vec::new()));
    let observed_for_responder = observed.clone();
    let registry = fake_registry(Arc::new(move |_, context| {
        observed_for_responder
            .lock()
            .unwrap()
            .push(context.system_prompt.clone());
        assistant(vec![text("done")], StopReason::Stop)
    }));
    // `agent_dir` points at the same (otherwise empty) temp dir so the
    // prompt's global skill/context discovery finds nothing either.
    let env = Arc::new(ToolSessionEnv {
        live_model: None,
        live_thinking: None,
        read_ledger: std::sync::Arc::new(pir::agent::read_ledger::ReadLedger::default()),
        provider_registry: registry,
        model: fake_model(),
        thinking: ThinkingLevel::Off,
        session_id: None,
        session_file: None,
        base_tools: Arc::new(ToolRegistry::new()),
        has_ui: false,
        project_trusted: true,
        user_agents_dir: Some(temp.0.join("agents")),
        agent_dir: Some(temp.0.clone()),
        credential: None,
        on_update: None,
        live_tools: None,
        active_override: None,
        background: None,
    });
    let ctx = tool_context(&temp.0, env, None);
    let result = run_tool(serde_json::json!({ "agent": "scout", "task": "t" }), &ctx).await;
    assert!(!result.is_error, "content: {}", result.content);
    let prompts = observed.lock().unwrap().clone();
    let prompt = prompts
        .last()
        .expect("child request")
        .clone()
        .expect("system prompt");
    assert!(
        prompt.starts_with("You are an expert coding assistant"),
        "prompt: {prompt}"
    );
    assert!(prompt.contains("<cwd>"), "prompt: {prompt}");
    assert!(prompt.contains("<tools>"), "prompt: {prompt}");
    assert!(prompt.contains("<rules>"), "prompt: {prompt}");
    assert!(
        prompt.ends_with("SCOUT_SYSTEM_PROMPT_MARKER"),
        "prompt: {prompt}"
    );
}

#[tokio::test]
async fn unknown_agent_fails_with_available_list() {
    let temp = TempDir::new("unknown-agent");
    temp.write_agent("scout.md", "name: scout\ndescription: d\n", "b");
    temp.write_agent("planner.md", "name: planner\ndescription: d\n", "b");
    let registry = fake_registry(Arc::new(|_, _| {
        assistant(vec![text("unused")], StopReason::Stop)
    }));
    let env = session_env(registry, Some(temp.0.join("agents")));
    let ctx = tool_context(&temp.0, env, None);
    let result = run_tool(serde_json::json!({ "agent": "ghost", "task": "t" }), &ctx).await;
    assert!(result.is_error);
    // Upstream's unknown-agent failure quotes the agent names only (the
    // `name (source)` format stays in the invalid-parameters message).
    assert_eq!(
        result.content,
        "Agent error: Unknown agent: \"ghost\". Available agents: \"planner\", \"scout\"."
    );
    let details = result.details.expect("details");
    assert_eq!(details["results"][0]["agentSource"], "unknown");
    assert_eq!(
        details["results"][0]["errorMessage"],
        "Unknown agent: \"ghost\". Available agents: \"planner\", \"scout\"."
    );
}

#[tokio::test]
async fn child_error_stop_reason_becomes_tool_error() {
    let temp = TempDir::new("child-error");
    temp.write_agent("scout.md", "name: scout\ndescription: d\n", "b");
    let registry = fake_registry(Arc::new(|_, context| {
        if user_text(context) == "t" {
            assistant(vec![text("boom")], StopReason::Error)
        } else {
            assistant(vec![text("unused")], StopReason::Stop)
        }
    }));
    let env = session_env(registry, Some(temp.0.join("agents")));
    let ctx = tool_context(&temp.0, env, None);
    let result = run_tool(serde_json::json!({ "agent": "scout", "task": "t" }), &ctx).await;
    assert!(result.is_error);
    assert_eq!(result.content, "Agent error: boom");
}

#[tokio::test]
async fn agent_with_own_model_uses_the_default_thinking_level() {
    // Upstream passes no `--thinking` when an agent declares its own model:
    // the child falls back to its default thinking level (medium) instead of
    // silently losing thinking; an agent WITHOUT its own model inherits the
    // dispatch (parent) thinking level.
    let temp = TempDir::new("own-model-thinking");
    temp.write_agent(
        "scout.md",
        "name: scout\ndescription: d\nmodel: fake/fake-model\n",
        "b",
    );
    temp.write_agent("inherited.md", "name: inherited\ndescription: d\n", "b");
    let observed: Arc<Mutex<Vec<ObservedThinking>>> = Arc::new(Mutex::new(Vec::new()));
    let provider = Arc::new(ThinkingRecordingProvider {
        calls: AtomicUsize::new(0),
        observed: Arc::clone(&observed),
    });
    let mut registry = ProviderRegistry::default();
    registry.set_provider(Arc::clone(&provider) as Arc<dyn Provider>);
    // Dispatch (parent) thinking level: high.
    let env = Arc::new(ToolSessionEnv {
        live_model: None,
        live_thinking: None,
        read_ledger: std::sync::Arc::new(pir::agent::read_ledger::ReadLedger::default()),
        provider_registry: Arc::new(registry),
        model: fake_model(),
        thinking: ThinkingLevel::High,
        session_id: None,
        session_file: None,
        base_tools: Arc::new(ToolRegistry::new()),
        has_ui: false,
        project_trusted: true,
        user_agents_dir: Some(temp.0.join("agents")),
        agent_dir: None,
        credential: None,
        on_update: None,
        live_tools: None,
        active_override: None,
        background: None,
    });
    let ctx = tool_context(&temp.0, env, None);

    let result = run_tool(serde_json::json!({ "agent": "scout", "task": "own" }), &ctx).await;
    assert!(!result.is_error, "content: {}", result.content);
    let result = run_tool(
        serde_json::json!({ "agent": "inherited", "task": "inherited" }),
        &ctx,
    )
    .await;
    assert!(!result.is_error, "content: {}", result.content);

    let observed = observed.lock().unwrap().clone();
    // Both child models here are `reasoning: false` (`fake_model` /
    // `other_model`), so `getSupportedThinkingLevels` yields only `off` and
    // PIR-BIN-074's construction-time clamp (upstream
    // `createAgentSession`) makes BOTH children send `off` — the own-model
    // child would otherwise have taken the default `medium` and the inheriting
    // child the parent's `high`. The distinction this test used to observe is
    // not expressible on a non-reasoning model any more; what it still pins is
    // that both dispatch paths build a child session and send a request.
    assert!(
        observed
            .iter()
            .any(|(task, thinking)| task == "own" && *thinking == Some(ThinkingLevel::Off)),
        "observed: {observed:?}"
    );
    assert!(
        observed
            .iter()
            .any(|(task, thinking)| task == "inherited" && *thinking == Some(ThinkingLevel::Off)),
        "observed: {observed:?}"
    );
}

#[tokio::test]
async fn child_resolves_models_json_from_the_parent_agent_dir() {
    // P1-I: a child session must use the ENCLOSING session's agent directory,
    // not the process `~/.pi/agent`: a `models.json` provider key that exists
    // only in the propagated directory must reach the child request.
    let temp = TempDir::new("child-agent-dir");
    temp.write_agent("scout.md", "name: scout\ndescription: d\n", "b");
    std::fs::write(
        temp.0.join("models.json"),
        r#"{ "providers": { "fake": { "apiKey": "models-json-key" } } }"#,
    )
    .expect("write models.json");

    let observed: Observed = Arc::new(Mutex::new(Vec::new()));
    let mut registry = ProviderRegistry::default();
    registry.set_provider(Arc::new(KeyRecordingProvider {
        model: fake_model(),
        observed: Arc::clone(&observed),
        respond: Arc::new(|_, _| assistant(vec![text("keyed answer")], StopReason::Stop)),
        calls: AtomicUsize::new(0),
    }));
    let env = Arc::new(ToolSessionEnv {
        live_model: None,
        live_thinking: None,
        read_ledger: std::sync::Arc::new(pir::agent::read_ledger::ReadLedger::default()),
        provider_registry: Arc::new(registry),
        model: fake_model(),
        thinking: ThinkingLevel::Off,
        session_id: None,
        session_file: None,
        base_tools: Arc::new(ToolRegistry::new()),
        has_ui: false,
        project_trusted: true,
        user_agents_dir: Some(temp.0.join("agents")),
        agent_dir: Some(temp.0.clone()),
        credential: None,
        on_update: None,
        live_tools: None,
        active_override: None,
        background: None,
    });
    let ctx = tool_context(&temp.0, env, None);
    let result = run_tool(serde_json::json!({ "agent": "scout", "task": "t" }), &ctx).await;
    assert!(!result.is_error, "content: {}", result.content);

    let observed = observed.lock().unwrap().clone();
    assert_eq!(observed.len(), 1, "observed: {observed:?}");
    assert_eq!(observed[0].1.as_deref(), Some("models-json-key"));
}

#[tokio::test]
async fn child_inherits_only_matching_provider_credentials() {
    // P1-I: the parent's explicit credential (`.endpoint(..).api_key(..)` /
    // `.api_key(..)` / `AgentSessionConfig::api_key`) is provider-scoped. A
    // child that inherits the parent model receives it; a child whose agent
    // declares a model on ANOTHER provider never does.
    let temp = TempDir::new("child-credential");
    temp.write_agent("inherited.md", "name: inherited\ndescription: d\n", "b");
    temp.write_agent(
        "foreign.md",
        "name: foreign\ndescription: d\nmodel: other/other-model\n",
        "b",
    );

    let observed: Observed = Arc::new(Mutex::new(Vec::new()));
    let mut registry = ProviderRegistry::default();
    registry.set_provider(Arc::new(KeyRecordingProvider {
        model: fake_model(),
        observed: Arc::clone(&observed),
        respond: Arc::new(|_, _| assistant(vec![text("keyed answer")], StopReason::Stop)),
        calls: AtomicUsize::new(0),
    }));
    registry.set_provider(Arc::new(KeyRecordingProvider {
        model: other_model(),
        observed: Arc::clone(&observed),
        respond: Arc::new(|_, _| assistant(vec![text("keyed answer")], StopReason::Stop)),
        calls: AtomicUsize::new(0),
    }));
    let credential = ToolCredential::new("fake", "endpoint-scoped-key");
    assert!(format!("{credential:?}").contains("[redacted]"));
    assert!(!format!("{credential:?}").contains("endpoint-scoped-key"));
    let env = Arc::new(ToolSessionEnv {
        live_model: None,
        live_thinking: None,
        read_ledger: std::sync::Arc::new(pir::agent::read_ledger::ReadLedger::default()),
        provider_registry: Arc::new(registry),
        model: fake_model(),
        thinking: ThinkingLevel::Off,
        session_id: None,
        session_file: None,
        base_tools: Arc::new(ToolRegistry::new()),
        has_ui: false,
        project_trusted: true,
        user_agents_dir: Some(temp.0.join("agents")),
        // No models.json in this directory: the only source of a key is the
        // env credential below.
        agent_dir: Some(temp.0.clone()),
        credential: Some(credential),
        on_update: None,
        live_tools: None,
        active_override: None,
        background: None,
    });
    let ctx = tool_context(&temp.0, env, None);

    let result = run_tool(
        serde_json::json!({ "agent": "inherited", "task": "inherited" }),
        &ctx,
    )
    .await;
    assert!(!result.is_error, "content: {}", result.content);
    let result = run_tool(
        serde_json::json!({ "agent": "foreign", "task": "foreign" }),
        &ctx,
    )
    .await;
    assert!(!result.is_error, "content: {}", result.content);

    let observed = observed.lock().unwrap().clone();
    let key_for = |task: &str| {
        observed
            .iter()
            .find(|(observed_task, _)| observed_task == task)
            .and_then(|(_, key)| key.clone())
    };
    assert_eq!(
        key_for("inherited").as_deref(),
        Some("endpoint-scoped-key"),
        "observed: {observed:?}"
    );
    assert_eq!(
        key_for("foreign"),
        None,
        "a foreign provider must not receive the parent credential: {observed:?}"
    );
}

#[tokio::test]
async fn chain_live_updates_show_completed_plus_one_running_entry() {
    // Upstream shows the completed steps plus exactly ONE running entry per
    // update: the latest snapshot REPLACES the running step's earlier
    // partials, so the details array never grows with the child turn count.
    let temp = TempDir::new("chain-live");
    temp.write_agent("a.md", "name: a\ndescription: d\n", "b");
    // Each child turn asks for the echo tool, so every child runs two turns
    // (two TurnEnd updates per chain step).
    let registry = fake_registry(Arc::new(|_, context| {
        if has_tool_result(context) {
            assistant(
                vec![text(format!("DONE:{}", user_text(context)).as_str())],
                StopReason::Stop,
            )
        } else {
            assistant(
                vec![tool_call_block(
                    "c1",
                    "echo",
                    serde_json::json!({ "text": "x" }),
                )],
                StopReason::ToolUse,
            )
        }
    }));
    let mut base = ToolRegistry::new();
    base.register(Arc::new(EchoTool));
    let updates: Arc<Mutex<Vec<serde_json::Value>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&updates);
    let env = Arc::new(ToolSessionEnv {
        live_model: None,
        live_thinking: None,
        read_ledger: std::sync::Arc::new(pir::agent::read_ledger::ReadLedger::default()),
        provider_registry: registry,
        model: fake_model(),
        thinking: ThinkingLevel::Off,
        session_id: None,
        session_file: None,
        base_tools: Arc::new(base),
        has_ui: false,
        project_trusted: true,
        user_agents_dir: Some(temp.0.join("agents")),
        agent_dir: None,
        credential: None,
        on_update: Some(Arc::new(move |value| {
            sink.lock().unwrap().push(value);
        })),
        live_tools: None,
        active_override: None,
        background: None,
    });
    let ctx = tool_context(&temp.0, env, None);
    let result = run_tool(
        serde_json::json!({
            "chain": [
                { "agent": "a", "task": "step-one" },
                { "agent": "a", "task": "step-two" },
            ]
        }),
        &ctx,
    )
    .await;
    assert!(!result.is_error, "content: {}", result.content);

    let updates = updates.lock().unwrap().clone();
    // Each step emits one placeholder as soon as it starts plus one update per
    // child turn; every snapshot shows the completed steps plus exactly one
    // running entry: lengths 1,1,1,2,2,2 (PIR-TUI-005: the step-start entry
    // must be present before the first child event).
    let lengths: Vec<usize> = updates
        .iter()
        .map(|update| {
            update["details"]["results"]
                .as_array()
                .map(|results| results.len())
                .unwrap_or(0)
        })
        .collect();
    assert!(
        updates.len() >= 6,
        "expected step-start + 2 turn updates per step, got {}: {updates:?}",
        updates.len()
    );
    assert!(
        lengths.iter().all(|length| *length <= 2),
        "live details arrays must stay bounded per step: {lengths:?}"
    );
    assert_eq!(lengths[..6], [1, 1, 1, 2, 2, 2], "lengths: {lengths:?}");
    let first = updates[0]["details"]["results"][0].clone();
    assert_eq!(first["step"], 1, "first update is step 1: {first}");
    assert_eq!(
        first["pending"], true,
        "step-start entry is running: {first}"
    );
    assert_eq!(first["agent"], "a", "first update: {first}");
}

#[tokio::test]
async fn tool_allowlist_limits_child_registry() {
    let temp = TempDir::new("allowlist");
    // Agent allows only "echo"; the base registry has "echo" + "other".
    temp.write_agent(
        "narrow.md",
        "name: narrow\ndescription: d\ntools: echo\n",
        "b",
    );
    let mut base = ToolRegistry::new();
    base.register(Arc::new(EchoTool));
    base.register(Arc::new(OtherTool));
    let registry = fake_registry(Arc::new(|_, context| {
        // The child's request advertises the child tool registry.
        let tool_names: Vec<String> = context
            .tools
            .as_ref()
            .map(|tools| tools.iter().map(|tool| tool.name.clone()).collect())
            .unwrap_or_default();
        let content = format!("TOOLS:{}", tool_names.join(","));
        assistant(vec![text(&content)], StopReason::Stop)
    }));
    let mut env = ToolSessionEnv {
        live_model: None,
        live_thinking: None,
        read_ledger: std::sync::Arc::new(pir::agent::read_ledger::ReadLedger::default()),
        provider_registry: registry,
        model: fake_model(),
        thinking: ThinkingLevel::Off,
        session_id: None,
        session_file: None,
        base_tools: Arc::new(base),
        has_ui: false,
        project_trusted: true,
        user_agents_dir: Some(temp.0.join("agents")),
        agent_dir: None,
        credential: None,
        on_update: None,
        live_tools: None,
        active_override: None,
        background: None,
    };
    let ctx = tool_context(&temp.0, Arc::new(env.clone()), None);
    let result = run_tool(serde_json::json!({ "agent": "narrow", "task": "t" }), &ctx).await;
    assert_eq!(result.content, "TOOLS:echo");

    // Empty/absent allowlist = the pir default set (the base registry as-is).
    temp.write_agent("wide.md", "name: wide\ndescription: d\n", "b");
    env.user_agents_dir = Some(temp.0.join("agents"));
    let ctx = tool_context(&temp.0, Arc::new(env), None);
    let result = run_tool(serde_json::json!({ "agent": "wide", "task": "t" }), &ctx).await;
    let content = result.content.clone();
    assert!(content.contains("echo"), "content: {content}");
    assert!(content.contains("other"), "content: {content}");
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

struct OtherTool;
impl AgentTool for OtherTool {
    fn name(&self) -> &'static str {
        "other"
    }
    fn description(&self) -> &'static str {
        "other"
    }
    fn run(
        &self,
        _args: &serde_json::Value,
        _ctx: &ToolContext,
    ) -> pir::agent::tool_execution::BoxFuture<'static, pir::agent::tool_batch::ToolResult> {
        Box::pin(async { pir::agent::tool_batch::ToolResult::ok("other") })
    }
}

// ---------------------------------------------------------------------------
// Parallel mode
// ---------------------------------------------------------------------------

#[tokio::test]
async fn parallel_mode_aggregates_and_emits_live_updates() {
    let temp = TempDir::new("parallel");
    temp.write_agent("a.md", "name: a\ndescription: d\n", "b");
    temp.write_agent("b.md", "name: b\ndescription: d\n", "b");
    let registry = fake_registry(Arc::new(|_, context| {
        let last_user = user_text(context);
        if last_user == "task b" {
            assistant(vec![text("B_RESULT")], StopReason::Stop)
        } else {
            assistant(vec![text(&format!("out:{last_user}"))], StopReason::Stop)
        }
    }));
    let updates: Arc<Mutex<Vec<serde_json::Value>>> = Arc::new(Mutex::new(Vec::new()));
    let updates_for_env = updates.clone();
    let mut env = ToolSessionEnv {
        live_model: None,
        live_thinking: None,
        read_ledger: std::sync::Arc::new(pir::agent::read_ledger::ReadLedger::default()),
        provider_registry: registry,
        model: fake_model(),
        thinking: ThinkingLevel::Off,
        session_id: None,
        session_file: None,
        base_tools: Arc::new(ToolRegistry::new()),
        has_ui: false,
        project_trusted: true,
        user_agents_dir: Some(temp.0.join("agents")),
        agent_dir: None,
        credential: None,
        on_update: None,
        live_tools: None,
        active_override: None,
        background: None,
    };
    let update_fn: ToolUpdateFn = Arc::new(move |partial: serde_json::Value| {
        updates_for_env.lock().unwrap().push(partial);
    });
    env.on_update = Some(update_fn);

    let ctx = tool_context(&temp.0, Arc::new(env), None);
    let result = run_tool(
        serde_json::json!({
            "tasks": [
                { "agent": "a", "task": "one" },
                { "agent": "b", "task": "task b" },
                { "agent": "a", "task": "three" },
            ],
        }),
        &ctx,
    )
    .await;
    assert!(!result.is_error, "content: {}", result.content);
    assert!(result.content.starts_with("Parallel: 3/3 succeeded\n\n"));
    assert!(result.content.contains("### [a] completed\n\nout:one"));
    assert!(result.content.contains("### [b] completed\n\nB_RESULT"));
    assert!(result.content.contains("\n\n---\n\n"));

    // Live updates: at least the initial running placeholder and one per
    // completion, with details carrying all results.
    let updates = updates.lock().unwrap().clone();
    assert!(updates.len() >= 4, "updates: {}", updates.len());
    assert!(
        updates.iter().any(|update| {
            update["content"][0]["text"]
                .as_str()
                .is_some_and(|text| text.contains("3/3 done, 0 running"))
        }),
        "final aggregate update missing"
    );
    let last_details = updates.last().expect("update").clone();
    assert_eq!(last_details["details"]["mode"], "parallel");
    assert_eq!(
        last_details["details"]["results"]
            .as_array()
            .map(|r| r.len()),
        Some(3)
    );
}

#[tokio::test]
async fn parallel_mode_reports_failures_in_the_aggregate() {
    let temp = TempDir::new("parallel-fail");
    temp.write_agent("a.md", "name: a\ndescription: d\n", "b");
    temp.write_agent("ghost.md", "name: ghost\ndescription: d\n", "b");
    let registry = fake_registry(Arc::new(|_, context| {
        assistant(
            vec![text(&format!("out:{}", user_text(context)))],
            StopReason::Stop,
        )
    }));
    let env = session_env(registry, Some(temp.0.join("agents")));
    let ctx = tool_context(&temp.0, env, None);
    let result = run_tool(
        serde_json::json!({
            "tasks": [
                { "agent": "a", "task": "one" },
                { "agent": "missing", "task": "two" },
            ],
        }),
        &ctx,
    )
    .await;
    assert!(!result.is_error);
    assert!(result.content.starts_with("Parallel: 1/2 succeeded\n\n"));
    assert!(result.content.contains("### [a] completed"));
    assert!(result
        .content
        .contains("### [missing] failed (error)\n\nUnknown agent: \"missing\""));
}

// ---------------------------------------------------------------------------
// Chain mode
// ---------------------------------------------------------------------------

#[tokio::test]
async fn chain_mode_substitutes_previous_and_returns_last_output() {
    let temp = TempDir::new("chain");
    temp.write_agent("scout.md", "name: scout\ndescription: d\n", "b");
    temp.write_agent("planner.md", "name: planner\ndescription: d\n", "b");
    let registry = fake_registry(Arc::new(|_, context| {
        let last_user = user_text(context);
        if last_user.starts_with("plan this: FINDING:") {
            assistant(vec![text("THE_PLAN")], StopReason::Stop)
        } else {
            assistant(vec![text("FINDING:rust")], StopReason::Stop)
        }
    }));
    let env = session_env(registry, Some(temp.0.join("agents")));
    let ctx = tool_context(&temp.0, env, None);
    let result = run_tool(
        serde_json::json!({
            "chain": [
                { "agent": "scout", "task": "scout the code" },
                { "agent": "planner", "task": "plan this: {previous}" },
            ],
        }),
        &ctx,
    )
    .await;
    assert!(!result.is_error, "content: {}", result.content);
    assert_eq!(result.content, "THE_PLAN");

    let details = result.details.expect("details");
    assert_eq!(details["mode"], "chain");
    let results = details["results"].as_array().expect("results");
    assert_eq!(results.len(), 2);
    assert_eq!(results[0]["step"], 1);
    assert_eq!(results[1]["step"], 2);
    // Step 2's task carried the substituted prior output.
    assert_eq!(results[1]["task"], "plan this: FINDING:rust");
}

#[tokio::test]
async fn chain_mode_stops_at_first_failure() {
    let temp = TempDir::new("chain-stop");
    temp.write_agent("scout.md", "name: scout\ndescription: d\n", "b");
    let registry = fake_registry(Arc::new(|_, _| {
        assistant(vec![text("unused")], StopReason::Stop)
    }));
    let env = session_env(registry, Some(temp.0.join("agents")));
    let ctx = tool_context(&temp.0, env, None);
    let result = run_tool(
        serde_json::json!({
            "chain": [
                { "agent": "scout", "task": "ok step" },
                { "agent": "missing", "task": "never runs {previous}" },
                { "agent": "scout", "task": "never reached" },
            ],
        }),
        &ctx,
    )
    .await;
    assert!(result.is_error);
    // Upstream's unknown-agent failure quotes the agent names only.
    assert_eq!(
        result.content,
        "Chain stopped at step 2 (missing): Unknown agent: \"missing\". Available agents: \"scout\"."
    );
    let details = result.details.expect("details");
    let results = details["results"].as_array().expect("results");
    assert_eq!(results.len(), 2, "failed step result is included");
    assert_eq!(results[1]["stopReason"], "error");
}

// ---------------------------------------------------------------------------
// Project-agent trust gate
// ---------------------------------------------------------------------------

#[tokio::test]
async fn project_agents_require_approval_when_ui_and_untrusted() {
    let temp = TempDir::new("trust-gate");
    temp.write_project_agent(
        "risky.md",
        "name: risky\ndescription: repo-controlled\n",
        "b",
    );
    let registry = fake_registry(Arc::new(|_, _| {
        assistant(vec![text("unused")], StopReason::Stop)
    }));
    let env = env_with(registry, Some(temp.0.join("agents")), true, false);
    let ctx = tool_context(&temp.0, env, None);
    let result = run_tool(
        serde_json::json!({
            "agent": "risky",
            "task": "t",
            "agentScope": "both",
        }),
        &ctx,
    )
    .await;
    // SAFE path: non-error text asking for approval; nothing ran.
    assert!(!result.is_error);
    assert!(
        result
            .content
            .starts_with("Project-local agents require approval before running."),
        "content: {}",
        result.content
    );
    assert!(result.content.contains("Agents: risky"));
    assert!(result.content.contains("Source:"));
    assert!(result.details.is_some());
}

#[tokio::test]
async fn trusted_project_skips_the_gate() {
    let temp = TempDir::new("trusted");
    temp.write_project_agent("ok.md", "name: ok\ndescription: d\n", "b");
    let registry = fake_registry(Arc::new(|_, context| {
        if user_text(context) == "t" {
            assistant(vec![text("PROJECT_RAN")], StopReason::Stop)
        } else {
            assistant(vec![text("unused")], StopReason::Stop)
        }
    }));
    let env = env_with(registry, Some(temp.0.join("agents")), true, true);
    let ctx = tool_context(&temp.0, env, None);
    let result = run_tool(
        serde_json::json!({ "agent": "ok", "task": "t", "agentScope": "both" }),
        &ctx,
    )
    .await;
    assert!(!result.is_error, "content: {}", result.content);
    assert_eq!(result.content, "PROJECT_RAN");
}

#[tokio::test]
async fn confirm_project_agents_false_skips_the_gate() {
    let temp = TempDir::new("confirm-false");
    temp.write_project_agent("ok.md", "name: ok\ndescription: d\n", "b");
    let registry = fake_registry(Arc::new(|_, context| {
        if user_text(context) == "t" {
            assistant(vec![text("RAN")], StopReason::Stop)
        } else {
            assistant(vec![text("unused")], StopReason::Stop)
        }
    }));
    let env = env_with(registry, Some(temp.0.join("agents")), true, false);
    let ctx = tool_context(&temp.0, env, None);
    let result = run_tool(
        serde_json::json!({
            "agent": "ok",
            "task": "t",
            "agentScope": "both",
            "confirmProjectAgents": false,
        }),
        &ctx,
    )
    .await;
    assert!(!result.is_error, "content: {}", result.content);
    assert_eq!(result.content, "RAN");
}

#[tokio::test]
async fn headless_runs_skip_the_gate_like_upstream_print_mode() {
    let temp = TempDir::new("headless");
    temp.write_project_agent("ok.md", "name: ok\ndescription: d\n", "b");
    let registry = fake_registry(Arc::new(|_, context| {
        if user_text(context) == "t" {
            assistant(vec![text("RAN")], StopReason::Stop)
        } else {
            assistant(vec![text("unused")], StopReason::Stop)
        }
    }));
    let env = env_with(registry, Some(temp.0.join("agents")), false, false);
    let ctx = tool_context(&temp.0, env, None);
    let result = run_tool(
        serde_json::json!({ "agent": "ok", "task": "t", "agentScope": "both" }),
        &ctx,
    )
    .await;
    assert!(!result.is_error, "content: {}", result.content);
    assert_eq!(result.content, "RAN");
}

// ---------------------------------------------------------------------------
// Cancellation
// ---------------------------------------------------------------------------

#[tokio::test]
async fn pre_aborted_signal_returns_subagent_aborted_error() {
    let temp = TempDir::new("pre-abort");
    temp.write_agent("scout.md", "name: scout\ndescription: d\n", "b");
    let registry = fake_registry(Arc::new(|_, _| {
        assistant(vec![text("never")], StopReason::Stop)
    }));
    let env = session_env(registry, Some(temp.0.join("agents")));
    let signal = AbortSignal::new();
    signal.abort();
    let ctx = tool_context(&temp.0, env, Some(signal));
    let result = run_tool(serde_json::json!({ "agent": "scout", "task": "t" }), &ctx).await;
    assert!(result.is_error);
    assert_eq!(result.content, "Subagent was aborted");
}

#[tokio::test]
async fn parent_abort_during_run_fails_the_tool_with_aborted_stop() {
    let temp = TempDir::new("mid-abort");
    temp.write_agent("scout.md", "name: scout\ndescription: d\n", "b");
    let entered = Arc::new(AtomicUsize::new(0));
    let release_aborted = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let (gate_tx, gate_rx) = tokio::sync::watch::channel(false);
    let registry: Arc<ProviderRegistry> = {
        let mut registry = ProviderRegistry::default();
        registry.set_provider(Arc::new(GatedProvider {
            entered: Arc::clone(&entered),
            gate: gate_rx.clone(),
            release_aborted: Arc::clone(&release_aborted),
        }));
        Arc::new(registry)
    };
    let env = session_env(registry, Some(temp.0.join("agents")));
    let signal = AbortSignal::new();
    let ctx = tool_context(&temp.0, env, Some(signal.clone()));

    let run_task = tokio::spawn(async move {
        run_tool(serde_json::json!({ "agent": "scout", "task": "t" }), &ctx).await
    });

    // Wait for the child run to be in flight, then abort the parent.
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
    let details = result.details.expect("details");
    assert_eq!(details["results"][0]["stopReason"], "aborted");
}

// ---------------------------------------------------------------------------
// Full session wiring: the builtin through the agent loop
// ---------------------------------------------------------------------------

#[test]
fn subagent_runs_through_the_agent_loop_with_session_env() {
    // The parent loop polls the subagent tool, which polls the CHILD agent loop
    // INLINE: the poll chain is deep but finite, and it overflows libtest's
    // default test-thread stack (measured: STATUS_STACK_OVERFLOW aborts the
    // whole binary; the same run passes in 0.06 s on a 32 MiB thread). Give
    // the run a real stack instead of relying on a layout-dependent default.
    std::thread::Builder::new()
        .stack_size(32 * 1024 * 1024)
        .spawn(|| {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("runtime");
            runtime.block_on(async move {
                let temp = TempDir::new("loop");
                temp.write_agent("scout.md", "name: scout\ndescription: d\n", "b");
                let registry = fake_registry(Arc::new(|_, context| {
                    let last_user = user_text(context);
                    if has_tool_result(context) {
                        assistant(vec![text("parent done")], StopReason::Stop)
                    } else if last_user == "kick off" {
                        // Parent turn 1: call the subagent tool.
                        assistant(
                            vec![tool_call_block(
                                "call-1",
                                "subagent",
                                serde_json::json!({ "agent": "scout", "task": "delegate me" }),
                            )],
                            StopReason::ToolUse,
                        )
                    } else {
                        // Child run (task text differs from the parent prompt).
                        assistant(vec![text("CHILD_VIA_LOOP")], StopReason::Stop)
                    }
                }));

                let mut tools = ToolRegistry::new();
                tools.register(Arc::new(SubagentTool::default()));
                // The loop-built session env resolves the user-agent directory from the
                // process agent dir; point it at the fixture for this test. (The other
                // tests in this binary inject the directory through the env struct and
                // never read the process env, so this is race-free here.)
                let previous_agent_dir = std::env::var("PIR_AGENT_DIR").ok();
                std::env::set_var("PIR_AGENT_DIR", &temp.0);
                let agent = Arc::new(AgentSession::new(AgentSessionConfig {
                    performance: Default::default(),
                    provider_registry: Arc::clone(&registry),
                    model: fake_model(),
                    api_key: None,
                    thinking: ThinkingLevel::Off,
                    tools,
                    session: None,
                    compaction: None,
                    max_turns: 4,
                    system_prompt: None,
                    cwd: Some(temp.0.clone()),
                    event_stream: None,
                    extensions: None,
                    tool_selection: None,
                }));

                let tool_results: Arc<Mutex<Vec<serde_json::Value>>> =
                    Arc::new(Mutex::new(Vec::new()));
                let tool_results_for_handler = tool_results.clone();
                let handler: pir::agent_session::EventHandler =
                    Arc::new(move |event: AgentEvent| {
                        if let AgentEvent::AgentEnd { result } = event {
                            for tool_result in &result.tool_results {
                                if tool_result.tool_name != "subagent" {
                                    continue;
                                }
                                let content = tool_result
                                    .content
                                    .iter()
                                    .filter_map(|block| match block {
                                        ContentBlock::Text(text) => Some(text.text.clone()),
                                        _ => None,
                                    })
                                    .collect::<Vec<_>>()
                                    .join("\n");
                                tool_results_for_handler
                                    .lock()
                                    .unwrap()
                                    .push(serde_json::json!({
                                        "content": content,
                                        "isError": tool_result.is_error,
                                        "details": tool_result.details,
                                    }));
                            }
                        }
                    });

                let terminal = agent.prompt("kick off", handler).await.expect("prompt");
                match previous_agent_dir {
                    Some(value) => std::env::set_var("PIR_AGENT_DIR", value),
                    None => std::env::remove_var("PIR_AGENT_DIR"),
                }
                assert_eq!(
                    get_final_output(&[Message::Assistant(terminal)]).as_deref(),
                    Some("parent done")
                );
                let results = tool_results.lock().unwrap().clone();
                assert_eq!(results.len(), 1);
                assert_eq!(results[0]["content"], "CHILD_VIA_LOOP");
                assert_eq!(results[0]["isError"], false);
                assert_eq!(results[0]["details"]["mode"], "single");
            });
        })
        .expect("spawn the subagent loop thread")
        .join()
        .expect("the subagent loop must not panic");
}

/// P1-I end-to-end through the SESSION path: the loop builds
/// `ToolSessionEnv` from the enclosing `AgentSession`, so an explicit
/// `AgentSessionConfig::api_key` (the SDK `.endpoint(..).api_key(..)` /
/// `.api_key(..)` shape) must reach the child request, and an explicit
/// `agent_dir` must drive child discovery/context (no process env read).
#[test]
fn session_env_propagates_the_explicit_credential_to_child_runs() {
    std::thread::Builder::new()
        .stack_size(32 * 1024 * 1024)
        .spawn(|| {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("runtime");
            runtime.block_on(async move {
                let temp = TempDir::new("session-key-propagation");
                temp.write_agent("scout.md", "name: scout\ndescription: d\n", "b");
                let observed: Observed = Arc::new(Mutex::new(Vec::new()));
                let mut registry = ProviderRegistry::default();
                registry.set_provider(Arc::new(KeyRecordingProvider {
                    model: fake_model(),
                    observed: Arc::clone(&observed),
                    respond: Arc::new(|index, _context| {
                        if index == 0 {
                            assistant(
                                vec![tool_call_block(
                                    "call-1",
                                    "subagent",
                                    serde_json::json!({ "agent": "scout", "task": "child-task" }),
                                )],
                                StopReason::ToolUse,
                            )
                        } else {
                            assistant(vec![text("child done")], StopReason::Stop)
                        }
                    }),
                    calls: AtomicUsize::new(0),
                }));

                let mut tools = ToolRegistry::new();
                tools.register(Arc::new(SubagentTool::default()));
                // Explicit agent dir: the session (and therefore the child)
                // must resolve `<dir>/agents` and never the process dir.
                let agent = Arc::new(AgentSession::new_with_agent_dir(
                    AgentSessionConfig {
                        performance: Default::default(),
                        provider_registry: Arc::new(registry),
                        model: fake_model(),
                        api_key: Some("session-explicit-key".into()),
                        thinking: ThinkingLevel::Off,
                        tools,
                        session: None,
                        compaction: None,
                        max_turns: 4,
                        system_prompt: None,
                        cwd: Some(temp.0.clone()),
                        event_stream: None,
                        extensions: None,
                        tool_selection: None,
                    },
                    &temp.0,
                ));

                let handler: pir::agent_session::EventHandler = Arc::new(|_event| {});
                agent.prompt("kick off", handler).await.expect("prompt");

                let observed = observed.lock().unwrap().clone();
                let key_for = |task: &str| {
                    observed
                        .iter()
                        .find(|(observed_task, _)| observed_task == task)
                        .and_then(|(_, key)| key.clone())
                };
                assert_eq!(
                    key_for("kick off").as_deref(),
                    Some("session-explicit-key"),
                    "the parent request carries its explicit key: {observed:?}"
                );
                assert_eq!(
                    key_for("child-task").as_deref(),
                    Some("session-explicit-key"),
                    "the child request inherits the provider-scoped key: {observed:?}"
                );
            });
        })
        .expect("spawn the subagent loop thread")
        .join()
        .expect("the subagent loop must not panic");
}

// ---------------------------------------------------------------------------
// Builtin registration + loader default-enable
// ---------------------------------------------------------------------------

#[test]
fn subagent_builtin_is_registered_and_replaceable() {
    let def = builtin_extension_def("subagent").expect("subagent builtin exists");
    assert!(def.replaceable);
    let plugin = instantiate_builtin("subagent").expect("instantiates");
    assert_eq!(plugin.info().name, "subagent");
    assert_eq!(plugin.info().path, "builtin:subagent");
    assert!(plugin.info().replaceable);

    let mut registrations = pir::plugins::PluginRegistrations::default();
    let mut ctx = pir::plugins::RegistrationContext::new(&mut registrations);
    plugin.register(&mut ctx).expect("register");
    assert_eq!(registrations.tools.len(), 1);
    assert_eq!(registrations.tools[0].0.name, "subagent");
    let schema = registrations.tools[0].0.parameters.clone();
    assert!(!schema.is_null(), "parameters schema present");
    assert_eq!(schema["properties"]["agent"]["type"], "string");
    assert_eq!(schema["properties"]["tasks"]["type"], "array");
    assert_eq!(schema["properties"]["chain"]["type"], "array");
    assert_eq!(schema["properties"]["agentScope"]["default"], "user");
    assert_eq!(
        schema["properties"]["confirmProjectAgents"]["default"],
        true
    );
}

#[test]
fn loader_enables_builtins_by_default_with_settings_exclusion() {
    // Default: every factory builtin is on the path plan without -e.
    let ordered = pir::plugins::order_extension_paths(&[], &[], false);
    assert!(ordered.contains(&PathBuf::from("builtin:subagent")));
    assert!(ordered.contains(&PathBuf::from("builtin:mcp")));

    // Exclusion: disabledBuiltinExtensions (the --no-mcp pattern) removes it
    // even when the default enablement added it.
    let mut disabled = std::collections::BTreeSet::new();
    disabled.insert("subagent".to_owned());
    let filtered = pir::plugins::filter_disabled_builtins(&ordered, &disabled);
    assert!(!filtered.contains(&PathBuf::from("builtin:subagent")));
    assert!(filtered.contains(&PathBuf::from("builtin:mcp")));

    // --no-extensions keeps only CLI paths (an explicit -e still loads).
    let cli_only =
        pir::plugins::order_extension_paths(&[PathBuf::from("builtin:subagent")], &[], true);
    assert_eq!(cli_only, vec![PathBuf::from("builtin:subagent")]);
}

// ---------------------------------------------------------------------------
// Child extension tools (the parent's ExtensionHandle reaches the child)
// ---------------------------------------------------------------------------

/// A JS extension registers `js-probe`; the handle is attached to the
/// `subagent` builtin the way `PluginHost::build` does; the CHILD session
/// must then execute `js-probe` through the shared QuickJS actor (upstream's
/// child is a full `pi` with the complete tool surface). Also proves the
/// JS→Rust→JS re-entry (`child tool execution` → `call_tool`) does not
/// deadlock.
#[tokio::test(flavor = "current_thread")]
async fn child_executes_extension_tools_through_the_parent_handle() {
    let temp = TempDir::new("ext-child");
    temp.write_agent(
        "scout.md",
        "name: scout\ndescription: JS tool user\ntools: js-probe\n",
        "Use the js-probe tool.",
    );

    let ext_path = temp.0.join("ext.ts");
    std::fs::write(
        &ext_path,
        r#"
        export default function (pi) {
            pi.registerTool({
                name: "js-probe",
                description: "js probe tool",
                parameters: { type: "object" },
                execute: async () => ({ content: [{ type: "text", text: "FROM_JS" }] }),
            });
        }
    "#,
    )
    .expect("write extension");

    let handle = pir::extensions::ExtensionHandle::start(pir::extensions::ExtensionConfig {
        cwd: temp.0.clone(),
        mode: "headless".to_string(),
        has_ui: false,
        model: Some("fake-model".to_string()),
        session_id: Some("subagent-ext-test".to_string()),
        system_prompt: String::new(),
        is_idle: true,
        limits: Some(Default::default()),
    })
    .expect("runtime start");
    let load = handle.load(&[ext_path]).await.expect("load extension");
    assert!(load.errors.is_empty(), "{:?}", load.errors);

    // Wire the tool exactly as the plugin host does: attach the handle to the
    // plugin, then register the tool from the plugin.
    let plugin = instantiate_builtin("subagent").expect("instantiates");
    let subagent_plugin = plugin
        .as_any()
        .and_then(|any| any.downcast_ref::<pir::plugins::SubagentPlugin>())
        .expect("subagent plugin downcast");
    subagent_plugin.attach_js(Some(handle));
    let mut registrations = pir::plugins::PluginRegistrations::default();
    let mut registration = pir::plugins::RegistrationContext::new(&mut registrations);
    plugin.register(&mut registration).expect("register");
    let backend = registrations
        .tools
        .iter()
        .find(|(def, _)| def.name == "subagent")
        .map(|(_, backend)| backend.clone())
        .expect("registered subagent tool");
    let pir::plugins::plugin::ToolBackend::Rust(tool) = backend else {
        panic!("subagent must register a Rust tool backend");
    };

    // The child model: turn 0 calls the JS tool, turn 1 echoes whether the
    // tool result reached the child's context (proving the child executed it
    // through the shared actor).
    let registry = fake_registry(Arc::new(|index, context| {
        if index == 0 {
            assistant(
                vec![tool_call_block("call-1", "js-probe", serde_json::json!({}))],
                StopReason::ToolUse,
            )
        } else {
            let saw_from_js = context.messages.iter().any(|message| {
                matches!(message, Message::ToolResult(result)
                    if result.tool_name == "js-probe"
                        && result.content.iter().any(|block| matches!(block,
                            ContentBlock::Text(text) if text.text.contains("FROM_JS"))))
            });
            assistant(
                vec![text(if saw_from_js {
                    "ECHO:FROM_JS"
                } else {
                    "MISSING"
                })],
                StopReason::Stop,
            )
        }
    }));

    let env = session_env(registry, Some(temp.0.join("agents")));
    let tool_ctx = tool_context(&temp.0, env, None);
    let result = tool
        .run(
            &serde_json::json!({ "agent": "scout", "task": "run the probe" }),
            &tool_ctx,
        )
        .await;
    assert!(!result.is_error, "{}", result.content);
    assert_eq!(result.content, "ECHO:FROM_JS");
}

// ---------------------------------------------------------------------------
// tool_search activation isolation: parent session vs subagent child session
// ---------------------------------------------------------------------------

/// A deferred tool used to verify that `tool_search` activation in a child
/// session writes only the child's own override cell.
#[cfg(feature = "tool-search")]
mod tool_search_isolation {
    use super::*;
    use pir::agent::tool_batch::ToolResult;
    use pir::agent::tool_execution::{BoxFuture, ToolExposure};
    use pir::coding_agent::ToolSelection;
    use pir::plugins::builtin::ToolSearchPlugin;
    use pir::session_manager::SessionManager;

    struct DeferredProbe {
        name: &'static str,
        description: &'static str,
    }

    impl AgentTool for DeferredProbe {
        fn name(&self) -> &str {
            self.name
        }
        fn description(&self) -> &str {
            self.description
        }
        fn parameters(&self) -> Option<serde_json::Value> {
            Some(serde_json::json!({
                "type": "object",
                "properties": { "service": { "type": "string" } },
            }))
        }
        fn exposure(&self) -> ToolExposure {
            ToolExposure::Deferred
        }
        fn run(
            &self,
            _args: &serde_json::Value,
            _ctx: &ToolContext,
        ) -> BoxFuture<'static, ToolResult> {
            let text = format!("{} ok", self.name);
            Box::pin(async move { ToolResult::ok(text) })
        }
    }

    fn deferred_registry() -> ToolRegistry {
        let mut tools = ToolRegistry::new();
        tools.register(Arc::new(DeferredProbe {
            name: "deploy_status",
            description: "Report the production deployment status for a service",
        }));
        tools.register(Arc::new(DeferredProbe {
            name: "release_notes",
            description: "Show changelog entries for a published version",
        }));
        tools.register(Arc::new(DeferredProbe {
            name: "rollback_status",
            description: "Undo a previously shipped release",
        }));
        tools
    }

    fn search_message(query: &str) -> AssistantMessage {
        assistant(
            vec![tool_call_block(
                "call-search",
                "tool_search",
                serde_json::json!({ "query": query, "limit": 5 }),
            )],
            StopReason::ToolUse,
        )
    }

    fn done() -> AssistantMessage {
        assistant(vec![text("done")], StopReason::Stop)
    }

    fn declared(context: &Context) -> Vec<String> {
        context
            .tools
            .as_ref()
            .map(|tools| tools.iter().map(|tool| tool.name.clone()).collect())
            .unwrap_or_default()
    }

    /// Real `AgentSession` for the parent, real `SubagentTool` + plan-only
    /// `builtin:tool-search` runtime for the child.
    ///
    /// 1. Parent tool_search loads A (`deploy_status`).
    /// 2. Child tool_search loads B (`release_notes`); the child must not
    ///    inherit A and must not throw B into the parent's active set.
    /// 3. Parent tool_search loads C (`rollback_status`) afterwards: parent is
    ///    A ∪ C (never B); the child declared only B in its own run.
    #[tokio::test]
    async fn child_tool_search_activation_does_not_pollute_parent_or_siblings() {
        let temp = TempDir::new("tool-search-isolation");
        temp.write_agent(
            "scout.md",
            "name: scout\ndescription: search isolation probe\ntools: tool_search\n",
            "Use tool_search when needed.",
        );
        let ext_path = temp.0.join("deferred-tools.ts");
        std::fs::write(
            &ext_path,
            r#"
            export default function (pi) {
                const tool = (name, description) => ({
                    name,
                    description,
                    exposure: "deferred",
                    parameters: { type: "object", properties: { service: { type: "string" } } },
                    execute: async () => ({ content: [{ type: "text", text: name + " ok" }] }),
                });
                pi.registerTool(tool("deploy_status", "Report the production deployment status for a service"));
                pi.registerTool(tool("release_notes", "Show changelog entries for a published version"));
                pi.registerTool(tool("rollback_status", "Undo a previously shipped release"));
            }
            "#,
        )
        .expect("write extension");

        let requests: Arc<Mutex<Vec<(String, Vec<String>, String)>>> =
            Arc::new(Mutex::new(Vec::new()));
        let requests_for_responder = Arc::clone(&requests);
        let providers = fake_registry(Arc::new(move |_index, context: &Context| {
            let last_user = user_text(context);
            requests_for_responder.lock().unwrap().push((
                last_user.clone(),
                declared(context),
                context
                    .messages
                    .iter()
                    .map(|message| match message {
                        Message::User(_) => "U",
                        Message::System(_) => "S",
                        Message::Developer(_) => "D",
                        Message::Assistant(_) => "A",
                        Message::ToolResult(_) => "T",
                    })
                    .collect::<String>(),
            ));
            if has_tool_result(context) {
                return done();
            }
            if last_user.contains("PARENT-A") {
                search_message("production deployment")
            } else if last_user.contains("CHILD-B") {
                search_message("changelog entries")
            } else if last_user.contains("PARENT-C") {
                search_message("undo shipped")
            } else {
                done()
            }
        }));

        let base_tools = deferred_registry();
        let parent = AgentSession::new_with_plugins(
            AgentSessionConfig {
                performance: Default::default(),
                provider_registry: Arc::clone(&providers),
                model: fake_model(),
                api_key: None,
                thinking: ThinkingLevel::Off,
                tools: base_tools.clone(),
                session: None,
                compaction: None,
                max_turns: 8,
                system_prompt: Some("base system prompt".into()),
                cwd: Some(temp.0.clone()),
                event_stream: None,
                extensions: None,
                tool_selection: Some(ToolSelection {
                    enabled: vec!["tool_search".to_owned()],
                    modifiers: Vec::new(),
                    uses_default_tools: false,
                }),
            },
            vec![Arc::new(ToolSearchPlugin::new()) as Arc<dyn pir::plugins::plugin::AgentPlugin>],
        );

        parent
            .prompt(
                "PARENT-A: find the deployment status tool",
                Arc::new(|_| {}),
            )
            .await
            .expect("parent prompt A");

        // Child: independent AgentSession built by `run_child` from the
        // plan-only handle; no `tools` allowlist, so deferred tools stay
        // registered but inactive until the child's own tool_search.
        let handle = pir::extensions::ExtensionHandle::start(pir::extensions::ExtensionConfig {
            cwd: temp.0.clone(),
            mode: "headless".to_string(),
            has_ui: false,
            model: Some("fake-model".to_string()),
            session_id: Some("tool-search-isolation".to_string()),
            system_prompt: String::new(),
            is_idle: true,
            limits: Some(Default::default()),
        })
        .expect("extension runtime");
        let load = handle
            .load(&[ext_path.clone()])
            .await
            .expect("load deferred tools");
        assert!(load.errors.is_empty(), "{:?}", load.errors);
        handle.set_path_plan(vec![PathBuf::from("builtin:tool-search"), ext_path]);
        let env = Arc::new(ToolSessionEnv {
            live_model: None,
            live_thinking: None,
            read_ledger: Arc::new(pir::agent::read_ledger::ReadLedger::default()),
            provider_registry: Arc::clone(&providers),
            model: fake_model(),
            thinking: ThinkingLevel::Off,
            session_id: None,
            session_file: None,
            base_tools: Arc::new(base_tools),
            has_ui: false,
            project_trusted: true,
            user_agents_dir: Some(temp.0.join("agents")),
            agent_dir: Some(temp.0.clone()),
            credential: None,
            on_update: None,
            live_tools: None,
            active_override: None,
            background: None,
        });
        let tool_ctx = tool_context(&temp.0, env, None);
        let result = SubagentTool::with_extensions(Some(handle))
            .run(
                &serde_json::json!({
                    "agent": "scout",
                    "task": "CHILD-B: find the release notes tool",
                }),
                &tool_ctx,
            )
            .await;
        assert!(!result.is_error, "child failed: {}", result.content);
        assert_eq!(result.content, "done");

        parent
            .prompt("PARENT-C: find the rollback status tool", Arc::new(|_| {}))
            .await
            .expect("parent prompt C");

        let requests = requests
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let parent_a: Vec<&Vec<String>> = requests
            .iter()
            .filter(|(user, _, _)| user.contains("PARENT-A"))
            .map(|(_, declared, _)| declared)
            .collect();
        let child_b: Vec<&Vec<String>> = requests
            .iter()
            .filter(|(user, _, _)| user.contains("CHILD-B"))
            .map(|(_, declared, _)| declared)
            .collect();
        let parent_c: Vec<&Vec<String>> = requests
            .iter()
            .filter(|(user, _, _)| user.contains("PARENT-C"))
            .map(|(_, declared, _)| declared)
            .collect();

        assert!(!parent_a.is_empty(), "parent A requests: {requests:?}");
        assert!(
            !parent_a[0].iter().any(|name| name == "deploy_status"),
            "deferred A must start inactive: {:?}",
            parent_a[0]
        );
        assert!(
            parent_a
                .iter()
                .any(|names| names.iter().any(|name| name == "deploy_status")),
            "parent tool_search must load A: {parent_a:?}"
        );

        assert!(!child_b.is_empty(), "child B requests: {requests:?}");
        assert!(
            !child_b[0].iter().any(|name| name == "release_notes"),
            "deferred B must start inactive in the child: {:?}",
            child_b[0]
        );
        assert!(
            child_b
                .iter()
                .any(|names| names.iter().any(|name| name == "release_notes")),
            "child tool_search must load B: {child_b:?}"
        );
        assert!(
            child_b
                .iter()
                .all(|names| !names.iter().any(|name| name == "deploy_status")),
            "the child must NOT inherit the parent's active A: {child_b:?}"
        );

        assert!(!parent_c.is_empty(), "parent C requests: {requests:?}");
        assert!(
            parent_c[0].iter().any(|name| name == "deploy_status"),
            "the child run must not clear the parent's A: {:?}",
            parent_c[0]
        );
        assert!(
            parent_c
                .last()
                .is_some_and(|names| names.iter().any(|name| name == "rollback_status")
                    && names.iter().any(|name| name == "deploy_status")),
            "parent after C must be A ∪ C: {parent_c:?}"
        );
        assert!(
            parent_c
                .iter()
                .all(|names| !names.iter().any(|name| name == "release_notes")),
            "the parent must NOT see the child's B: {parent_c:?}"
        );
        assert!(
            requests
                .iter()
                .filter(|(user, _, _)| user.contains("PARENT"))
                .all(|(_, names, _)| !names.iter().any(|name| name == "release_notes")),
            "no parent request may declare the child-only B: {requests:?}"
        );
    }

    // -----------------------------------------------------------------------
    // Parallel siblings: four children run concurrently, each tool_search
    // loads a different deferred tool (or nothing for the no-match child).
    // -----------------------------------------------------------------------

    #[derive(Debug, Clone)]
    struct ParallelRequest {
        last_user: String,
        declared: Vec<String>,
        tool_results: Vec<String>,
    }

    /// A tool result AFTER the current (last) user message marks a post-search
    /// turn. (The loadout refresh inserts a trailing `system` `toolsAdded`
    /// message, so "last message is a tool result" is false on precisely the
    /// turn after a load; a plain "any tool result" wrongly matches the second
    /// parent prompt, whose history still contains prompt A's tool result.)
    fn has_any_tool_result(context: &Context) -> bool {
        let last_user = context
            .messages
            .iter()
            .rposition(|message| matches!(message, Message::User(_)))
            .unwrap_or(0);
        context.messages[last_user..]
            .iter()
            .any(|message| matches!(message, Message::ToolResult(_)))
    }

    fn query_for(last_user: &str) -> Option<&'static str> {
        if last_user.contains("PARENT-A") || last_user.contains("CHILD-A") {
            Some("production deployment")
        } else if last_user.contains("CHILD-B") {
            Some("changelog entries")
        } else if last_user.contains("PARENT-C") || last_user.contains("CHILD-C") {
            Some("undo shipped")
        } else if last_user.contains("CHILD-N") {
            // Deliberately unlike any registered tool: the child's search
            // returns "No matching tools found." and activates nothing.
            Some("zzz quantum flux capacitor")
        } else {
            None
        }
    }

    /// Scripted provider whose FIRST child request blocks until all four
    /// sibling children have reached the provider. That is only reachable if
    /// the parallel dispatcher really has all children in flight at once;
    /// sequential dispatch trips `timed_out` after the deadline.
    struct ParallelBarrierProvider {
        entered: Arc<AtomicUsize>,
        expected: usize,
        timed_out: Arc<std::sync::atomic::AtomicBool>,
        requests: Arc<Mutex<Vec<ParallelRequest>>>,
    }

    impl Provider for ParallelBarrierProvider {
        fn id(&self) -> &str {
            "fake"
        }
        fn name(&self) -> &str {
            "Fake parallel barrier provider"
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
            let last_user = user_text(&context);
            let tool_results = context
                .messages
                .iter()
                .filter_map(|message| match message {
                    Message::ToolResult(result) => Some(
                        result
                            .content
                            .iter()
                            .filter_map(|block| match block {
                                ContentBlock::Text(text) => Some(text.text.clone()),
                                _ => None,
                            })
                            .collect::<String>(),
                    ),
                    _ => None,
                })
                .collect::<Vec<_>>();
            self.requests
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(ParallelRequest {
                    last_user: last_user.clone(),
                    declared: declared(&context),
                    tool_results,
                });

            let stream = AssistantMessageEventStream::new();
            let pusher = stream.clone();
            let first_child_turn = !has_any_tool_result(&context) && last_user.contains("CHILD-");
            if first_child_turn {
                self.entered.fetch_add(1, Ordering::SeqCst);
                let entered = Arc::clone(&self.entered);
                let expected = self.expected;
                let timed_out = Arc::clone(&self.timed_out);
                let message = query_for(&last_user)
                    .map(search_message)
                    .unwrap_or_else(done);
                tokio::spawn(async move {
                    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
                    loop {
                        if entered.load(Ordering::SeqCst) >= expected {
                            break;
                        }
                        if tokio::time::Instant::now() >= deadline {
                            timed_out.store(true, Ordering::SeqCst);
                            break;
                        }
                        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                    }
                    pusher.push(AssistantMessageEventStream::done_event(
                        TerminalReason::Stop,
                        message,
                    ));
                });
            } else {
                let message = if has_any_tool_result(&context) {
                    done()
                } else {
                    query_for(&last_user)
                        .map(search_message)
                        .unwrap_or_else(done)
                };
                pusher.push(AssistantMessageEventStream::done_event(
                    TerminalReason::Stop,
                    message,
                ));
            }
            stream
        }
    }

    fn deferred_names_in(requests: &[&ParallelRequest]) -> std::collections::BTreeSet<String> {
        requests
            .iter()
            .flat_map(|request| request.declared.iter().cloned())
            .filter(|name| {
                name == "deploy_status" || name == "release_notes" || name == "rollback_status"
            })
            .collect()
    }

    fn requests_for<'a>(requests: &'a [ParallelRequest], marker: &str) -> Vec<&'a ParallelRequest> {
        requests
            .iter()
            .filter(|request| request.last_user.contains(marker))
            .collect()
    }

    /// Parent A loaded before the parallel dispatch; four children (A/B/C plus
    /// a no-match child) each run their own tool_search inside their own child
    /// runtime; parent C loaded afterwards. Siblings must not see each other's
    /// active tools, the parent's active set is untouched by all children, and
    /// the no-match child must not disturb its siblings.
    #[tokio::test]
    async fn parallel_child_tool_search_activations_stay_isolated_across_siblings() {
        let temp = TempDir::new("parallel-tool-search-isolation");
        temp.write_agent(
            "scout.md",
            "name: scout\ndescription: parallel search isolation probe\ntools: tool_search\n",
            "Use tool_search when needed.",
        );
        let ext_path = temp.0.join("deferred-tools.ts");
        std::fs::write(
            &ext_path,
            r#"
            export default function (pi) {
                const tool = (name, description) => ({
                    name,
                    description,
                    exposure: "deferred",
                    parameters: { type: "object", properties: { service: { type: "string" } } },
                    execute: async () => ({ content: [{ type: "text", text: name + " ok" }] }),
                });
                pi.registerTool(tool("deploy_status", "Report the production deployment status for a service"));
                pi.registerTool(tool("release_notes", "Show changelog entries for a published version"));
                pi.registerTool(tool("rollback_status", "Undo a previously shipped release"));
            }
            "#,
        )
        .expect("write extension");

        let requests: Arc<Mutex<Vec<ParallelRequest>>> = Arc::new(Mutex::new(Vec::new()));
        let entered = Arc::new(AtomicUsize::new(0));
        let timed_out = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let mut provider_registry = ProviderRegistry::default();
        provider_registry.set_provider(Arc::new(ParallelBarrierProvider {
            entered: Arc::clone(&entered),
            expected: 4,
            timed_out: Arc::clone(&timed_out),
            requests: Arc::clone(&requests),
        }));
        let providers = Arc::new(provider_registry);

        let base_tools = deferred_registry();
        let parent_session =
            SessionManager::create(&temp.0.display().to_string(), &temp.0).expect("create session");
        let parent_session_path = parent_session.path().to_path_buf();
        let parent = AgentSession::new_with_plugins(
            AgentSessionConfig {
                performance: Default::default(),
                provider_registry: Arc::clone(&providers),
                model: fake_model(),
                api_key: None,
                thinking: ThinkingLevel::Off,
                tools: base_tools.clone(),
                session: Some(parent_session),
                compaction: None,
                max_turns: 8,
                system_prompt: Some("base system prompt".into()),
                cwd: Some(temp.0.clone()),
                event_stream: None,
                extensions: None,
                tool_selection: Some(ToolSelection {
                    enabled: vec!["tool_search".to_owned()],
                    modifiers: Vec::new(),
                    uses_default_tools: false,
                }),
            },
            vec![Arc::new(ToolSearchPlugin::new()) as Arc<dyn pir::plugins::plugin::AgentPlugin>],
        );

        parent
            .prompt(
                "PARENT-A: find the deployment status tool",
                Arc::new(|_| {}),
            )
            .await
            .expect("parent prompt A");

        let handle = pir::extensions::ExtensionHandle::start(pir::extensions::ExtensionConfig {
            cwd: temp.0.clone(),
            mode: "headless".to_string(),
            has_ui: false,
            model: Some("fake-model".to_string()),
            session_id: Some("parallel-tool-search-isolation".to_string()),
            system_prompt: String::new(),
            is_idle: true,
            limits: Some(Default::default()),
        })
        .expect("extension runtime");
        let load = handle
            .load(&[ext_path.clone()])
            .await
            .expect("load deferred tools");
        assert!(load.errors.is_empty(), "{:?}", load.errors);
        handle.set_path_plan(vec![PathBuf::from("builtin:tool-search"), ext_path]);
        let env = Arc::new(ToolSessionEnv {
            live_model: None,
            live_thinking: None,
            read_ledger: Arc::new(pir::agent::read_ledger::ReadLedger::default()),
            provider_registry: Arc::clone(&providers),
            model: fake_model(),
            thinking: ThinkingLevel::Off,
            session_id: None,
            session_file: None,
            base_tools: Arc::new(base_tools),
            has_ui: false,
            project_trusted: true,
            user_agents_dir: Some(temp.0.join("agents")),
            agent_dir: Some(temp.0.clone()),
            credential: None,
            on_update: None,
            live_tools: None,
            active_override: None,
            background: None,
        });
        let updates: Arc<Mutex<Vec<serde_json::Value>>> = Arc::new(Mutex::new(Vec::new()));
        let updates_sink = Arc::clone(&updates);
        let mut tool_ctx = tool_context(&temp.0, env, None);
        tool_ctx.on_update = Some(Arc::new(move |value| {
            updates_sink
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(value);
        }));

        let task_texts = [
            "CHILD-A: find the deployment status tool",
            "CHILD-B: find the release notes tool",
            "CHILD-C: find the rollback status tool",
            "CHILD-N: find the nonexistent quantum tool",
        ];
        let result = SubagentTool::with_extensions(Some(handle))
            .run(
                &serde_json::json!({
                    "tasks": task_texts
                        .iter()
                        .map(|task| serde_json::json!({ "agent": "scout", "task": task }))
                        .collect::<Vec<_>>(),
                }),
                &tool_ctx,
            )
            .await;
        assert!(!result.is_error, "parallel run failed: {}", result.content);
        assert!(
            !timed_out.load(Ordering::SeqCst),
            "children did not reach the provider concurrently (sequential dispatch?)"
        );
        assert_eq!(
            entered.load(Ordering::SeqCst),
            4,
            "all four children must have issued their first request"
        );

        parent
            .prompt("PARENT-C: find the rollback status tool", Arc::new(|_| {}))
            .await
            .expect("parent prompt C");

        let details = result.details.clone().expect("parallel details");
        assert_eq!(details["mode"], "parallel");
        let results = details["results"].as_array().expect("results array");
        assert_eq!(results.len(), task_texts.len());
        for (index, task) in task_texts.iter().enumerate() {
            let entry = &results[index];
            assert_eq!(
                entry["task"], *task,
                "result order must match input: {details}"
            );
            assert_eq!(entry["exitCode"], 0, "child {index} failed: {entry}");
            assert_eq!(entry["stopReason"], "stop", "child {index}: {entry}");
            // Two turns per child: tool_search + final text. The shared fake
            // assistant usage is 10 input / 5 output / 0.25 per turn.
            assert_eq!(entry["usage"]["turns"], 2, "child {index} usage: {entry}");
            assert_eq!(entry["usage"]["input"], 20, "child {index} usage: {entry}");
            assert_eq!(entry["usage"]["output"], 10, "child {index} usage: {entry}");
            assert_eq!(entry["usage"]["cost"], 0.5, "child {index} usage: {entry}");
            // Each child's transcript is its own full transcript: user task,
            // tool_search call/result, the loadout refresh, final answer. It
            // may only name its own loaded tool (or, for CHILD-N, none).
            let messages = entry["messages"].as_array().expect("messages");
            assert!(messages.len() >= 4, "child {index} transcript: {entry}");
            let transcript = serde_json::to_string(messages).expect("transcript json");
            if let Some(own) = ["deploy_status", "release_notes", "rollback_status"].get(index) {
                assert!(
                    transcript.contains(&format!("- {own}:")),
                    "child {index} must see its own load result: {transcript}"
                );
                for other in ["deploy_status", "release_notes", "rollback_status"] {
                    if other != *own {
                        assert!(
                            !transcript.contains(&format!("- {other}:")),
                            "child {index} must not see sibling {other}: {transcript}"
                        );
                    }
                }
            } else {
                assert!(
                    transcript.contains("No matching tools found."),
                    "no-match child transcript: {transcript}"
                );
            }
        }

        let requests = requests
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();

        // Sibling isolation: each child's declared set contains its OWN
        // deferred tool and none of the other two.
        for (marker, own) in [
            ("CHILD-A", "deploy_status"),
            ("CHILD-B", "release_notes"),
            ("CHILD-C", "rollback_status"),
        ] {
            let child_requests = requests_for(&requests, marker);
            assert!(!child_requests.is_empty(), "no requests for {marker}");
            assert!(
                !child_requests[0].declared.iter().any(|name| name == own),
                "{marker} must start with {own} inactive: {:?}",
                child_requests[0]
            );
            assert!(
                child_requests
                    .iter()
                    .any(|request| request.declared.iter().any(|name| name == own)),
                "{marker} tool_search must load {own}: {child_requests:#?}"
            );
            let names = deferred_names_in(&child_requests);
            assert_eq!(
                names,
                std::collections::BTreeSet::from([own.to_owned()]),
                "{marker} must only activate its own deferred tool: {names:?}"
            );
            assert!(
                child_requests.iter().any(|request| request
                    .tool_results
                    .iter()
                    .any(|text| text.contains(&format!("Loaded 1 tool")) && text.contains(own))),
                "{marker} must see its own successful tool_search result: {child_requests:#?}"
            );
        }

        // No-match child: no deferred activation, no error, own transcript,
        // and the sibling toolset never bleeds in.
        let nomatch = requests_for(&requests, "CHILD-N");
        assert!(!nomatch.is_empty(), "no requests for CHILD-N");
        assert!(
            deferred_names_in(&nomatch).is_empty(),
            "no-match child must not activate any deferred tool: {nomatch:#?}"
        );
        assert!(
            nomatch.iter().any(|request| request
                .tool_results
                .iter()
                .any(|text| text.contains("No matching tools found."))),
            "CHILD-N must observe the no-match result: {nomatch:#?}"
        );
        assert!(
            results[3]["messages"]
                .as_array()
                .is_some_and(|messages| !messages.is_empty()),
            "no-match child transcript must exist: {}",
            results[3]
        );

        // Parent active set: A before, A ∪ C afterwards, never B; the four
        // child runs neither clear A nor inject their own tools.
        let parent_requests = requests_for(&requests, "PARENT");
        let parent_a = requests_for(&requests, "PARENT-A");
        let parent_c = requests_for(&requests, "PARENT-C");
        assert!(!parent_a.is_empty() && !parent_c.is_empty());
        assert!(
            !parent_a[0]
                .declared
                .iter()
                .any(|name| name == "deploy_status"),
            "parent A starts inactive: {:?}",
            parent_a[0]
        );
        assert!(
            parent_c[0]
                .declared
                .iter()
                .any(|name| name == "deploy_status"),
            "child runs must not clear the parent's A: {:?}",
            parent_c[0]
        );
        assert!(
            parent_c.last().is_some_and(|request| request
                .declared
                .iter()
                .any(|name| name == "deploy_status")
                && request
                    .declared
                    .iter()
                    .any(|name| name == "rollback_status")),
            "parent after C must be A ∪ C: {parent_c:#?}"
        );
        assert_eq!(
            deferred_names_in(&parent_requests),
            std::collections::BTreeSet::from([
                "deploy_status".to_owned(),
                "rollback_status".to_owned()
            ]),
            "parent must never declare a child-only deferred tool"
        );

        // Live aggregate events: every snapshot keeps the input task order,
        // starts with all placeholders pending, and ends with all four done.
        let updates = updates
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        assert!(
            updates.len() >= 5,
            "expected live aggregate updates: {updates:#?}"
        );
        let snapshot_results = |update: &serde_json::Value| {
            update["details"]["results"]
                .as_array()
                .expect("update results")
                .clone()
        };
        let first = snapshot_results(&updates[0]);
        assert_eq!(first.len(), 4, "initial snapshot must show every slot");
        assert!(
            first.iter().all(|entry| entry["pending"] == true),
            "initial snapshot must be all placeholders: {first:#?}"
        );
        for update in &updates {
            let slots = snapshot_results(update);
            assert_eq!(slots.len(), 4, "snapshot slot count: {update}");
            for (index, task) in task_texts.iter().enumerate() {
                assert_eq!(slots[index]["task"], *task, "snapshot order: {update}");
            }
        }
        let last = snapshot_results(updates.last().expect("last update"));
        assert!(
            last.iter().all(|entry| entry.get("pending").is_none()),
            "final snapshot must have no pending slot (the flag is omitted when false): {last:#?}"
        );
        assert!(
            last.iter().all(|entry| entry["usage"]["turns"] == 2),
            "final snapshot per-task usage: {last:#?}"
        );

        // Parent session JSONL: both parent prompts are persisted; no child
        // prompt text is written as a parent user turn (children are
        // in-memory `--no-session` runs; their transcript lives in details).
        let jsonl = std::fs::read_to_string(&parent_session_path).expect("read parent session");
        let mut parent_user_texts = Vec::new();
        for line in jsonl.lines() {
            let value: serde_json::Value = serde_json::from_str(line).expect("jsonl line");
            if value["type"] == "message" && value["message"]["role"] == "user" {
                let text = value["message"]["content"]
                    .as_array()
                    .map(|blocks| {
                        blocks
                            .iter()
                            .filter_map(|block| block["text"].as_str())
                            .collect::<String>()
                    })
                    .unwrap_or_default();
                parent_user_texts.push(text);
            }
        }
        assert!(
            parent_user_texts
                .iter()
                .any(|text| text.contains("PARENT-A"))
                && parent_user_texts
                    .iter()
                    .any(|text| text.contains("PARENT-C")),
            "parent prompts must be persisted: {parent_user_texts:?}"
        );
        assert!(
            parent_user_texts
                .iter()
                .all(|text| !text.contains("CHILD-")),
            "child prompts must not leak into the parent session JSONL: {parent_user_texts:?}"
        );
    }
}
