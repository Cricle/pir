//! Integration tests for the production wiring of the QuickJS extension
//! runtime: AgentSession tool merging, `session_start`/`input`/`tool_call`
//! dispatch, the tool gate, and the default discovery-path helper.
//!
//! Fixtures are generated into per-test temp directories (the repository
//! hygiene gate forbids tracked `.ts` files) and runtimes take explicit
//! paths, so no environment variable is mutated by these tests.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use pir::agent::tool_batch::ToolResult;
use pir::agent::tool_execution::{AgentTool, BoxFuture, ToolContext, ToolRegistry};
use pir::agent_session::{AgentSession, AgentSessionConfig};
use pir::ai::auth::ApiKeyAuth;
use pir::ai::event_stream::AssistantMessageEventStream;
use pir::ai::provider::{Provider, ProviderRegistry};
use pir::ai::types::*;
use pir::extensions::{ExtensionConfig, ExtensionHandle};

// ---------------------------------------------------------------------------
// Fixtures and fakes
// ---------------------------------------------------------------------------

/// Per-test temp directory removed on drop.
struct TempDir(PathBuf);

impl TempDir {
    fn new(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "pir-ext-wiring-{label}-{}-{}",
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

fn test_config_at(cwd: &Path) -> ExtensionConfig {
    ExtensionConfig {
        cwd: cwd.to_path_buf(),
        mode: "headless".to_string(),
        has_ui: false,
        model: Some("fake-model".to_string()),
        session_id: Some("wiring-session".to_string()),
        system_prompt: String::new(),
        is_idle: true,
        limits: Some(Default::default()),
    }
}

/// Write `source` as `<dir>/ext.ts` and load it into a fresh runtime actor.
async fn load_runtime(dir: &TempDir, source: &str) -> ExtensionHandle {
    let path = dir.0.join("ext.ts");
    std::fs::write(&path, source).expect("write fixture");
    let runtime = ExtensionHandle::start(test_config_at(&dir.0)).expect("extension runtime");
    let result = runtime
        .load(std::slice::from_ref(&path))
        .await
        .expect("load extensions");
    assert!(result.errors.is_empty(), "{:?}", result.errors);
    assert_eq!(result.loaded.len(), 1, "{:?}", result.errors);
    runtime
}

/// Probe JS globals through a `probe_eval` event registered by a helper
/// extension appended to the fixture source. The dispatch returns the JSON
/// the handler captured; this keeps every assertion on real runtime data.
fn probe_handle<'a>(
    runtime: &'a ExtensionHandle,
    code: &'a str,
) -> impl std::future::Future<Output = serde_json::Value> + 'a {
    let mut payload = serde_json::json!({ "type": "probe", "code": code });
    async move {
        let outcome = runtime
            .emit("before_provider_request", &mut payload)
            .await
            .expect("probe dispatch");
        outcome
            .result
            .get("json")
            .and_then(serde_json::Value::as_str)
            .map(|s| serde_json::from_str::<serde_json::Value>(s).expect("probe json"))
            .expect("probe returned no json")
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

fn tool_call_with_args(id: &str, name: &str, args: serde_json::Value) -> ContentBlock {
    let arguments: BTreeMap<String, serde_json::Value> = match args {
        serde_json::Value::Object(map) => map.into_iter().collect(),
        _ => BTreeMap::new(),
    };
    ContentBlock::ToolCall(ToolCall {
        id: id.into(),
        name: name.into(),
        arguments,
        thought_signature: None,
        namespace: None,
        arguments_error: None,
    })
}

/// Responder signature: (call index, request context) -> assistant message.
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

fn user_texts(context: &Context) -> Vec<String> {
    context
        .messages
        .iter()
        .filter_map(|message| match message {
            Message::User(user) => Some(
                user.content
                    .iter()
                    .filter_map(|block| match block {
                        ContentBlock::Text(text) => Some(text.text.clone()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join(""),
            ),
            _ => None,
        })
        .collect()
}

/// (tool_name, is_error, text) for every tool-result message in a context.
fn tool_results(context: &Context) -> Vec<(String, bool, String)> {
    context
        .messages
        .iter()
        .filter_map(|message| match message {
            Message::ToolResult(result) => {
                let text = result
                    .content
                    .iter()
                    .filter_map(|block| match block {
                        ContentBlock::Text(text) => Some(text.text.clone()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("");
                Some((result.tool_name.clone(), result.is_error, text))
            }
            _ => None,
        })
        .collect()
}

fn no_events() -> pir::agent_session::EventHandler {
    Arc::new(|_| {})
}

// ---------------------------------------------------------------------------
// (a) session_start-registered extension tool is merged and callable
// ---------------------------------------------------------------------------

const SESSION_ECHO_TS: &str = r#"
export default function (pi) {
	pi.on("before_provider_request", (event) => ({ json: eval(event.code) }));
	pi.on("session_start", (event) => {
		globalThis.__session_start_reason = event.reason;
		pi.registerTool({
			name: "echo",
			description: "Echo a message",
			parameters: {
				type: "object",
				properties: { message: { type: "string" } },
				required: ["message"],
			},
			async execute(_toolCallId, params) {
				globalThis.__echo_calls = (globalThis.__echo_calls ?? 0) + 1;
				return { content: [{ type: "text", text: `echoed: ${params.message}` }] };
			},
		});
	});
}
"#;

#[tokio::test(flavor = "current_thread")]
async fn extension_tool_registered_in_session_start_is_callable() {
    let temp = TempDir::new("echo");
    let runtime = load_runtime(&temp, SESSION_ECHO_TS).await;

    let contexts: Arc<Mutex<Vec<Context>>> = Arc::new(Mutex::new(Vec::new()));
    let contexts_for_responder = contexts.clone();
    let registry = fake_registry(Arc::new(move |index, context| {
        contexts_for_responder.lock().unwrap().push(context.clone());
        if index == 0 {
            assistant(
                vec![tool_call_with_args(
                    "call-1",
                    "echo",
                    serde_json::json!({ "message": "hi" }),
                )],
                StopReason::ToolUse,
            )
        } else {
            assistant(vec![text("done")], StopReason::Stop)
        }
    }));

    let agent = AgentSession::new(AgentSessionConfig {
        provider_registry: registry,
        model: fake_model(),
        api_key: None,
        thinking: ThinkingLevel::Off,
        tools: ToolRegistry::new(),
        session: None,
        compaction: None,
        max_turns: 4,
        system_prompt: None,
        cwd: Some(temp.0.clone()),
        event_stream: None,
        extensions: Some(runtime.clone()),
        tool_selection: None,
    });

    let terminal = agent.prompt("go", no_events()).await.expect("prompt");
    assert_eq!(terminal.stop_reason, StopReason::Stop);

    // The model's second request carries the extension tool result, proving
    // the tool was merged into the registry used by the session run.
    {
        let seen = contexts.lock().unwrap();
        assert_eq!(seen.len(), 2, "expected two model requests");
        let results = tool_results(&seen[1]);
        assert!(
            results
                .iter()
                .any(|(name, is_error, text)| name == "echo" && !*is_error && text == "echoed: hi"),
            "extension tool result missing: {results:?}"
        );
    }

    // session_start was delivered before the first prompt and the JS
    // `execute` ran exactly once.
    let probe = probe_handle(
        &runtime,
        r#"JSON.stringify({ reason: globalThis.__session_start_reason ?? null, calls: globalThis.__echo_calls ?? 0 })"#,
    )
    .await;
    assert_eq!(probe["reason"], serde_json::json!("startup"));
    assert_eq!(probe["calls"], serde_json::json!(1));
}

// ---------------------------------------------------------------------------
// (a2) ctx.shutdown is a host-level exit request, never an abort
// ---------------------------------------------------------------------------

const SHUTDOWN_ON_START_TS: &str = r#"
export default function (pi) {
	pi.on("session_start", (_event, ctx) => {
		ctx.shutdown();
	});
}
"#;

/// Upstream `ExtensionContext.shutdown` routes to the host's shutdown
/// handler (`agent-session.ts:3510-3512`), which only records
/// `shutdownRequested`; the run in flight settles normally. The old host fn
/// also pushed `Notification::Abort`, which cancelled the run right after a
/// `session_start` handler asked the session to shut down.
#[tokio::test(flavor = "current_thread")]
async fn ctx_shutdown_requests_a_host_exit_without_aborting_the_run() {
    let temp = TempDir::new("shutdown-request");
    let runtime = load_runtime(&temp, SHUTDOWN_ON_START_TS).await;

    let registry = fake_registry(Arc::new(|_index, _context| {
        assistant(vec![text("completed")], StopReason::Stop)
    }));
    let agent = AgentSession::new(AgentSessionConfig {
        provider_registry: registry,
        model: fake_model(),
        api_key: None,
        thinking: ThinkingLevel::Off,
        tools: ToolRegistry::new(),
        session: None,
        compaction: None,
        max_turns: 4,
        system_prompt: None,
        cwd: Some(temp.0.clone()),
        event_stream: None,
        extensions: Some(runtime),
        tool_selection: None,
    });

    let terminal = agent.prompt("go", no_events()).await.expect("prompt");
    assert_eq!(
        terminal.stop_reason,
        StopReason::Stop,
        "a shutdown request must not abort the in-flight run: {terminal:?}"
    );
    assert!(
        terminal
            .content
            .iter()
            .any(|block| matches!(block, ContentBlock::Text(text) if text.text == "completed")),
        "the run completed its turn: {terminal:?}"
    );

    // The session consumer recorded the host-level request; the host takes it
    // exactly once (the RPC / headless exit seam).
    assert!(
        agent.shutdown_requested(),
        "ctx.shutdown must set the host-level shutdown flag"
    );
    assert!(
        agent.take_shutdown_request(),
        "the host consumes the request at its exit boundary"
    );
    assert!(
        !agent.shutdown_requested(),
        "taking the request clears it (one-shot)"
    );
    assert!(!agent.take_shutdown_request(), "nothing left to take");
}

// ---------------------------------------------------------------------------
// (b) tool_call gate blocks the tool before execution
// ---------------------------------------------------------------------------

const BLOCK_BASH_TS: &str = r#"
export default function (pi) {
	pi.on("before_provider_request", (event) => ({ json: eval(event.code) }));
	pi.on("tool_call", (event) => {
		globalThis.__gate_calls = (globalThis.__gate_calls ?? 0) + 1;
		globalThis.__gate_last_id = event.toolCallId;
		if (event.toolName === "bash") {
			return { block: true, reason: "bash blocked by test extension" };
		}
		return undefined;
	});
}
"#;

/// Records every `run` call; a blocked call must leave the counter at zero.
struct BashStubTool {
    runs: Arc<AtomicUsize>,
}

impl AgentTool for BashStubTool {
    fn name(&self) -> &'static str {
        "bash"
    }
    fn description(&self) -> &'static str {
        "stub bash that must never run in this test"
    }
    fn parameters(&self) -> Option<serde_json::Value> {
        Some(serde_json::json!({ "type": "object" }))
    }
    fn run(&self, _args: &serde_json::Value, _ctx: &ToolContext) -> BoxFuture<'static, ToolResult> {
        self.runs.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { ToolResult::ok("ran") })
    }
}

#[tokio::test(flavor = "current_thread")]
async fn tool_call_handler_blocks_bash_before_execution() {
    let temp = TempDir::new("block-bash");
    let runtime = load_runtime(&temp, BLOCK_BASH_TS).await;

    let runs = Arc::new(AtomicUsize::new(0));
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(BashStubTool { runs: runs.clone() }));

    let contexts: Arc<Mutex<Vec<Context>>> = Arc::new(Mutex::new(Vec::new()));
    let contexts_for_responder = contexts.clone();
    let registry = fake_registry(Arc::new(move |index, context| {
        contexts_for_responder.lock().unwrap().push(context.clone());
        if index == 0 {
            assistant(
                vec![tool_call_with_args("call-1", "bash", serde_json::json!({}))],
                StopReason::ToolUse,
            )
        } else {
            assistant(vec![text("fine")], StopReason::Stop)
        }
    }));

    let agent = AgentSession::new(AgentSessionConfig {
        provider_registry: registry,
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
        extensions: Some(runtime.clone()),
        tool_selection: None,
    });

    let terminal = agent.prompt("go", no_events()).await.expect("prompt");
    assert_eq!(terminal.stop_reason, StopReason::Stop);

    // The underlying tool never ran, and the batch carries the block reason
    // as an error result.
    assert_eq!(runs.load(Ordering::SeqCst), 0, "blocked tool executed");
    {
        let seen = contexts.lock().unwrap();
        let results = tool_results(&seen[1]);
        assert!(
            results.iter().any(|(name, is_error, text)| name == "bash"
                && *is_error
                && text.contains("bash blocked by test extension")),
            "block error result missing: {results:?}"
        );
    }

    // The gate carried the real provider tool-call id.
    let probe = probe_handle(
        &runtime,
        r#"JSON.stringify({ calls: globalThis.__gate_calls ?? 0, id: globalThis.__gate_last_id ?? null })"#,
    )
    .await;
    assert_eq!(probe["calls"], serde_json::json!(1));
    assert_eq!(probe["id"], serde_json::json!("call-1"));
}

// ---------------------------------------------------------------------------
// (c) input event transform rewrites the prompt before the model sees it
// ---------------------------------------------------------------------------

const UPPERCASE_TS: &str = r#"
export default function (pi) {
	pi.on("input", (event) => ({ action: "transform", text: String(event.text).toUpperCase() }));
}
"#;

#[tokio::test(flavor = "current_thread")]
async fn input_handler_transform_replaces_prompt_text() {
    let temp = TempDir::new("input-transform");
    let runtime = load_runtime(&temp, UPPERCASE_TS).await;

    let observed: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let observed_for_responder = observed.clone();
    let registry = fake_registry(Arc::new(move |_index, context| {
        observed_for_responder
            .lock()
            .unwrap()
            .extend(user_texts(context));
        assistant(vec![text("ok")], StopReason::Stop)
    }));

    let agent = AgentSession::new(AgentSessionConfig {
        provider_registry: registry,
        model: fake_model(),
        api_key: None,
        thinking: ThinkingLevel::Off,
        tools: ToolRegistry::new(),
        session: None,
        compaction: None,
        max_turns: 4,
        system_prompt: None,
        cwd: Some(temp.0.clone()),
        event_stream: None,
        extensions: Some(runtime),
        tool_selection: None,
    });

    agent.prompt("hello", no_events()).await.expect("prompt");
    let seen = observed.lock().unwrap();
    assert!(
        seen.iter().any(|text| text == "HELLO"),
        "transformed prompt never reached the model: {seen:?}"
    );
    assert!(
        !seen.iter().any(|text| text == "hello"),
        "original prompt leaked through: {seen:?}"
    );
}

// ---------------------------------------------------------------------------
// (d) tool_call handler can rewrite tool input before execution
// ---------------------------------------------------------------------------

const REWRITE_INPUT_TS: &str = r#"
export default function (pi) {
	pi.on("tool_call", (event) => {
		if (event.toolName === "greet") {
			event.input.message = "rewritten";
		}
	});
}
"#;

struct GreetTool {
    seen_args: Arc<Mutex<Option<serde_json::Value>>>,
}

impl AgentTool for GreetTool {
    fn name(&self) -> &'static str {
        "greet"
    }
    fn description(&self) -> &'static str {
        "records its arguments"
    }
    fn parameters(&self) -> Option<serde_json::Value> {
        Some(serde_json::json!({
            "type": "object",
            "properties": { "message": { "type": "string" } },
            "required": ["message"],
        }))
    }
    fn run(&self, args: &serde_json::Value, _ctx: &ToolContext) -> BoxFuture<'static, ToolResult> {
        *self.seen_args.lock().unwrap() = Some(args.clone());
        let message = args
            .get("message")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_owned();
        Box::pin(async move { ToolResult::ok(format!("greeting {message}")) })
    }
}

#[tokio::test(flavor = "current_thread")]
async fn tool_call_handler_rewrites_tool_input() {
    let temp = TempDir::new("rewrite-input");
    let runtime = load_runtime(&temp, REWRITE_INPUT_TS).await;

    let seen_args: Arc<Mutex<Option<serde_json::Value>>> = Arc::new(Mutex::new(None));
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(GreetTool {
        seen_args: seen_args.clone(),
    }));

    let registry = fake_registry(Arc::new(|index, _context| {
        if index == 0 {
            assistant(
                vec![tool_call_with_args(
                    "call-1",
                    "greet",
                    serde_json::json!({ "message": "original" }),
                )],
                StopReason::ToolUse,
            )
        } else {
            assistant(vec![text("done")], StopReason::Stop)
        }
    }));

    let agent = AgentSession::new(AgentSessionConfig {
        provider_registry: registry,
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
        extensions: Some(runtime),
        tool_selection: None,
    });

    agent.prompt("go", no_events()).await.expect("prompt");
    let args = seen_args.lock().unwrap().clone().expect("tool ran");
    assert_eq!(
        args["message"],
        serde_json::json!("rewritten"),
        "rewritten input did not reach the tool: {args:?}"
    );
}

// ---------------------------------------------------------------------------
// (e) default discovery paths and the trust gate
// ---------------------------------------------------------------------------

#[test]
fn default_extension_paths_respect_the_trust_gate() {
    // Project-local paths come BEFORE user-global (the upstream
    // discovery-order fix).
    let agent_dir = PathBuf::from("/agent");
    let project_dir = PathBuf::from("/project");

    let untrusted =
        pir::extensions::loader::default_extension_paths(&agent_dir, &project_dir, false);
    assert_eq!(untrusted, vec![agent_dir.join("extensions")]);

    let trusted = pir::extensions::loader::default_extension_paths(&agent_dir, &project_dir, true);
    assert_eq!(
        trusted,
        vec![
            project_dir.join(".pi").join("extensions"),
            agent_dir.join("extensions"),
        ]
    );
}

// ---------------------------------------------------------------------------
// C2 — deflated JS shim assets: byte identity + non-gate inflate measurement
// ---------------------------------------------------------------------------

mod shim_deflate {
    use std::collections::BTreeSet;
    use std::fmt::Write as _;
    use std::time::{Duration, Instant};

    use sha2::{Digest, Sha256};

    /// SHA-256 of every raw `.js.txt` source before the C2 deflate tranche.
    /// Captured 2026-10-22 from the unchanged sources; evidence file
    /// `logs/w18-help/q4b/golden-pre.txt` (90 entries, 3,314,994 B total).
    /// The `pi_ai_providers_all_shim.js.txt` entry was refreshed when the
    /// 2026-10-26 catalog update (openai-codex model removal) changed the
    /// pinned manifest/version literals; its blob was regenerated with zlib
    /// level 9 (byte-identical to `scripts/gen_shims_deflate.py`).
    const GOLDEN: [(&str, &str); 90] = [
        (
            "abort_global.js.txt",
            "ab7be26a59e5f548630e45cba5636c649b5b352b1cf9a1cbd96217bf7d49458c",
        ),
        (
            "assert_shim.js.txt",
            "1b79ed2bfb0383af322584d2d497b617d4eb57411fd126bbeda13b985ceb9a3c",
        ),
        (
            "assert_strict_shim.js.txt",
            "9ae725028ed3dec1f09729670e122b6276c754fba88dd05b0ff82ffff30a7214",
        ),
        (
            "async_hooks_shim.js.txt",
            "8f6cce191662c6946bc189f6b7cc0ce3d2e2fc966f7a52c546da62451b893aff",
        ),
        (
            "bcrypt_shim.js.txt",
            "a0efadcb12a5f83f657c38a0e8730e4ea884b77e8a73481039ab56b163b059a1",
        ),
        (
            "better_sqlite3_shim.js.txt",
            "5489d34efe18336953f70f4fee858e4d704fbc034720690f8f1d9116d796ad2d",
        ),
        (
            "buffer_global.js.txt",
            "05a45999acaed4b7b7b9f8e6f23983765f28fc94223639bca1886246b89a641c",
        ),
        (
            "child_process_shim.js.txt",
            "35bbc3403dfdf4188b0e80e71e68c63fe87d495d7f2e4cc78eeeb9c85ea16e4c",
        ),
        (
            "cluster_shim.js.txt",
            "a19befb70380ee3970b84bd035828061fc0c240f50c56db940d95baba6ed928d",
        ),
        (
            "console_shim.js.txt",
            "4ca4d77f71d58f0701a7554edad37f0012ab602c20991c959aecd371f4a8ba27",
        ),
        (
            "constants_shim.js.txt",
            "da511582a6573959db893747519f9d1e137b7e4818a5b6d24e7e6f45d4336b09",
        ),
        (
            "crypto_shim.js.txt",
            "7e5ed94b011b9f7114b0c81ff73ffde9c747d41134b8ed3b28c1a025c29714bb",
        ),
        (
            "dgram_shim.js.txt",
            "e1dc0168a1d1a2e6a5a68c12049e76d35924485db1cf0c0988c387bca6af304f",
        ),
        (
            "diagnostics_channel_shim.js.txt",
            "b7a3262cf301e210a32b42a0a627b1f52a383f3d4c33cd38ca029ebd4fee62ee",
        ),
        (
            "dns_promises_shim.js.txt",
            "af8a90d2730ab689645cc92f30ac4a628ef0768a37f1bbc576be2b9f46e7e896",
        ),
        (
            "dns_shim.js.txt",
            "680cf5e33c49a34468d071fd793a3f947bad8f1fbf6d14fadc13413d78ef4bca",
        ),
        (
            "domain_shim.js.txt",
            "4be9a41cdfb90543c265397025c17f432d930e0eb33ec70c8922a0d550a9cdb3",
        ),
        (
            "events_shim.js.txt",
            "1ed561342ad793ff57bd16526667f6811ef3ad506130d4a3f8d5b83d63e3492a",
        ),
        (
            "fetch_global.js.txt",
            "3da00e6ce6b483e76a1b8fdaf903ad0df4dc2c68718125a6d67271bb01547109",
        ),
        (
            "fs_promises_shim.js.txt",
            "0cd010dc43c5512e989a8a436702e2653fc214f27d21ef699df3713553f3d021",
        ),
        (
            "fs_shim.js.txt",
            "5ed9e10ee1aea9dcb7f5ab8702940cacafe4a4f7650d7f42d0557e574a2aec20",
        ),
        (
            "http2_shim.js.txt",
            "1bed8f38c516887786415a26aad3d9bedb9e704c359a22592046b69aab38c75c",
        ),
        (
            "http_common_shim.js.txt",
            "9fdce91eea031ae36ff856dd4052b59e66afc0d6365975dea0fedf8c95bc1307",
        ),
        (
            "http_incoming_shim.js.txt",
            "5faa298a17295821b12f33b8dc29a02d3e9b34a26cc9819b2571f6cc44a9a928",
        ),
        (
            "http_internal_shim.js.txt",
            "d97b20b4ef6a568cc808bdf7db22f7f715a56d76e471935d34562a0aba63ab93",
        ),
        (
            "http_outgoing_shim.js.txt",
            "fb32cfd474ee21da529f3f61fa0059b1b5efc305da5c0f2ba2941a9e6cf5055e",
        ),
        (
            "http_server_shim.js.txt",
            "6d8fd9121894b5fa09ad0ab668fd9cda3c498cd748326aebb2718c30e67f76fc",
        ),
        (
            "http_shim.js.txt",
            "57e64657df5ecd61890d1a9f3590fe5f80e6ad5b8ee63cee2c66380c5c108d60",
        ),
        (
            "https_shim.js.txt",
            "e652e5b682071092d25134555176646ecfbd1b74bb69a34aac578add5b672ad6",
        ),
        (
            "inspector_promises_shim.js.txt",
            "c0be7d08a026071cce6fb2174a05ea25252fbb7d8b984f0a8fb269677f434d14",
        ),
        (
            "inspector_shim.js.txt",
            "3efa8a3e258558485b7a8d4b5b7d92d2b0fe56a60587f646abdb894650dba9de",
        ),
        (
            "intl_pad.js.txt",
            "1139016f3ad9f4487a3b4a0ab73bb7e98fa54b95d05075cb2c9a257a14e5218f",
        ),
        (
            "module_shim.js.txt",
            "fe36b51a07561cc1c702cfdc36658d50619bcddb9df4180bc152f42e1059e3fa",
        ),
        (
            "net_shim.js.txt",
            "6962266252aa25cd11ccc9fb04cb78f49865f20bf867187baca4a597b8138a82",
        ),
        (
            "node_call_helper.js.txt",
            "a9507416a5b02ea62bd773c8d204a154e4cf8d0f7e8b75362db390876c087309",
        ),
        (
            "node_cjs_runtime.js.txt",
            "44c1e1a25d3157625dfde28205c4b1dd6a1ad50029f9349eec919f43090b203b",
        ),
        (
            "os_shim.js.txt",
            "2449edbff8d21013edd18fa45dc552bd761e2096d759a5324e798174736bb646",
        ),
        (
            "path_shim.js.txt",
            "f948af6fd0c6cdbe46c955f56618c281d43a78b247f1f909a8eb51f2e13ce5cd",
        ),
        (
            "pi_agent_core_shim.js.txt",
            "2ee0603727d2d45030486808ac69f0ea157e150281ee7811b649c3135b342445",
        ),
        (
            "pi_ai_oauth_shim.js.txt",
            "a2e54900d588db4350c35b38cc321db09a1ef96d833c9ed0702b106f68eb057c",
        ),
        (
            "pi_ai_providers_all_shim.js.txt",
            "3a7f240a45f70a6411df12b60dcf0367567cd768fb9bc03737f6b67af232149c",
        ),
        (
            "pi_ai_shim.js.txt",
            "d80861e10393676ab34871b494ad6df25b8aaa9b0826f1b858930bc89e040cf6",
        ),
        (
            "pi_chord_node_shim.js.txt",
            "052732a99f5df9e28f4a7f24175212a3d7477161010e15074061012bd383cf23",
        ),
        (
            "pi_chord_shim.js.txt",
            "82fb8263d03592c6cba0bc6427baf1e520d4fc327f7f7c4b39a8b9b400bf802e",
        ),
        (
            "pi_codemode_shim.js.txt",
            "fd434eb9cb832ea37713e80257a0771f87c4739d899cbbae1eb352fa781452b9",
        ),
        (
            "pi_coding_agent_shim.js.txt",
            "414a7f781bd678546ba339db4c00a5a5173ee9898932241fe9ef9ece2d139a80",
        ),
        (
            "pi_durable_env_shim.js.txt",
            "a09d5fa2b344054645da84540c900f129feb728485ee6bd1f8b3439ec7684f13",
        ),
        (
            "pi_durable_shim.js.txt",
            "86ab7ec512902e7106cd06b462874ffc99c78a6e2eaf49c28755af0aebb0b8ec",
        ),
        (
            "pi_durable_throws_shim.js.txt",
            "1b931cb97bf11c05cfc799f4b34446daa90edc41eb994b8642874ae2f6882028",
        ),
        (
            "pi_env_shim.js.txt",
            "4ce81128e5fae6eb1765c04722a1fe23419f8b4d47a58d5046f3d979e1ae0cb2",
        ),
        (
            "pi_tui_shim.js.txt",
            "c4ae0e5f5e62e95df6e848d0918eb3cfeed03a811af8f4cdfddf412be7f9f679",
        ),
        (
            "prelude.js.txt",
            // 2026-10-25 lane K: full `systemPromptOptions` (mutable
            // sections/tools/skills + per-handler re-render), custom
            // `user_bash` BashOperations execution, ToolDefinition
            // prepareArguments/constrainedSampling/renderCall/renderResult,
            // synchronous `next()` for tool renderer resolvers, JS tool
            // default executionMode "parallel", ctx.compact callbacks,
            // native Provider registration surface, model_select Model
            // objects.
            // Lane Q: `renderShell` on the resolver-chain reply and partial
            // `OverlayHandle` support for `ctx.ui.custom({onHandle})`.
            // Lane V: `withSession` callbacks now ride a runtime token
            // (`__pir_callSessionCallback`) instead of being rejected, and
            // `ctx.reload()` points at the host resource reload.
            // P2-D1: `registerProvider` refreshModels uses the host-pushed
            // ModelsStore entry as `context.stored` and hands
            // `publish({persist})` back to the host on the event outcome.
            // HOST-EDGES: `ctx.shutdown()` no longer fires `notifyAbort()` —
            // upstream `ExtensionContext.shutdown` is a host exit request,
            // not an abort (agent-session.ts:3510-3512).
            "564554f119b20f525d2c247153f2b2250dadc57303cc0b783e0d2919e592f097",
        ),
        (
            "process_global_stub.js.txt",
            "04e058ff5c8b3dea2fbae6632064b7de29c9253d070e225cc5b7d67289142a5b",
        ),
        (
            "process_shim.js.txt",
            "c65bdf64e9d605e173c0786c07eaed7b511b506938777c7866eb119234988e04",
        ),
        (
            "punycode_shim.js.txt",
            "0036f8baefed6cd6a40c182165a19e7e63ac4554768a5db4f88748ce47dccf79",
        ),
        (
            "querystring_shim.js.txt",
            "e3af9a97c5e5731006ea348e643f79e8b303092e8500d775666ec585e06dd165",
        ),
        (
            "readline_promises_shim.js.txt",
            "93fd6b57c124a60523dfc50d6227b716179358034f16975c4c5a00dc0d1c03aa",
        ),
        (
            "readline_shim.js.txt",
            "1568905235f0a2e7557daf69cf6d0a0c2d94b294e958b9445f407e2af37ce9cd",
        ),
        (
            "repl_shim.js.txt",
            "af0da8cbfe2c5571ad6974fb271ffec6af4998b15d58dae33f461fb9463e7aed",
        ),
        (
            "sea_shim.js.txt",
            "c457988fde0ced3752670c70790f799dc0bf4ada5e4f73d875e4345cde5c906a",
        ),
        (
            "sqlite3_shim.js.txt",
            "8ff5ad48e8f810270e9fac1a5c51d2748bd882938021deb38cd69259b926e1b4",
        ),
        (
            "sqlite_shim.js.txt",
            "5c3c2376eb2b856ef86726778abb542cd0359dd6c1ffe7206dc87906c04f6637",
        ),
        (
            "stream_consumers_shim.js.txt",
            "d3d6db7785fd94a369a8a18784e6333176df05fd4ef5a6bbc89bbd4a71e83986",
        ),
        (
            "stream_empty_internals_shim.js.txt",
            "780d3f905fdf07181e72d9e1d0cc24df9b451525a2e31b5521271ac91f5fcea8",
        ),
        (
            "stream_internals_shim.js.txt",
            "09f808bdace71cbe3d5bf1466d7959c4a92692919e4a60194d2b54201bd05b74",
        ),
        (
            "stream_promises_shim.js.txt",
            "0dfaa7b328e5ab4fa90a84891070fe44d4abfb94b3e1d272ac98942ed37de40d",
        ),
        (
            "stream_shim.js.txt",
            "7ec02231b08f5402c125571c85a2ba3bf5c1eb1027f3b2611afbed8146cfd391",
        ),
        (
            "stream_web_shim.js.txt",
            "5c50ce25da7b39a2e495983d4e5d0e71bcdefdb0cd6051f95fac1b6413699573",
        ),
        (
            "string_decoder_shim.js.txt",
            "7be9e1bec39f66b3ba311aa576ce8a8602925f583f8ca2387dbb88b36c9da30a",
        ),
        (
            "sys_shim.js.txt",
            "96c14f8da12ccba6f12f50b825d129269e567122c6bb52636288a8d9d032a4dc",
        ),
        (
            "test_reporters_shim.js.txt",
            "8ab71679a2bf6ba99b2ccc22d8914b4dde0fe4d944bd51bac50193b99b3a6b87",
        ),
        (
            "test_shim.js.txt",
            "5c26a3be89ec90c12cc175fb9ae1ea6dc3cae9d9390ef89ca59660a0b7f9c412",
        ),
        (
            "textcodec_global.js.txt",
            "3110147026a316946d94130b67e1ac785770bd2ab5209a2621758ea76fc43a89",
        ),
        (
            "tls_internals_shim.js.txt",
            "1aa66fa5485a6a1b040824bc1d5b5579b51292ce83090f03d14b738e5fdca6a1",
        ),
        (
            "trace_events_shim.js.txt",
            "0a6aaea16c40f0b7086bf5ceb1722fdba4fe61a47e9a79660fff52389a4d147e",
        ),
        (
            "tty_shim.js.txt",
            "5425a8304adc329a2cde074e8b9637af026321b6fc5de72f401539b0b4cb3ef3",
        ),
        (
            "typebox_compile_shim.js.txt",
            "aad27f612fd620596c343812a021ae266b2d957d2f1df8a0b50819a4efac8fc5",
        ),
        (
            "typebox_shim.js.txt",
            "0c93422b544f03fe804ef8e0deb2a7231c229685ddd765bbb68aedd781d06cd4",
        ),
        (
            "typebox_value_shim.js.txt",
            "bbecad702190d3b897f8ffff40acc6829e2b9c272624a739a264d2df9e018fd8",
        ),
        (
            "url_shim.js.txt",
            "16e06ce718da6231ebc517c08c744c6b5f8520c16951b4e10f75108727ba6687",
        ),
        (
            "util_shim.js.txt",
            "5f45e2e1dc50f0fb0dc30ac7bd8ea17781ed367f02fd7d8f860c69a9eae9ca63",
        ),
        (
            "util_types_shim.js.txt",
            "431fd0b1608ba838c6dd0a031efcfe24da45895fd938f1df44d9f26d7dbb42a9",
        ),
        (
            "v8_shim.js.txt",
            "3e036c2eb29ba8e09b05eef4fd0c9059e491bf266810bf81611365937090eba3",
        ),
        (
            "vm_shim.js.txt",
            "f341502c7fb77c1d0d5d2be7f89bc598160b1f4fdf0368809ddf9e1934868492",
        ),
        (
            "wasi_shim.js.txt",
            "1940fd9dad00d10f7b526f546d816e6189dd7d67e2ea61633c4e16eaa8646ad7",
        ),
        (
            "web_streams.js.txt",
            "7c816f51ac4f83b399755c2ac2aacbafd35dda3da14c30530f601b57b278714a",
        ),
        (
            "webcompat_globals.js.txt",
            "bb17b685713ffcf730706fdb30afb0cff5d92994354db58ec84770843ee5f4bf",
        ),
        (
            "worker_boot.js.txt",
            "03abfb87773e349160352fd226e0f7a8bafb2471401cd1b8299f5419c5fe3236",
        ),
        (
            "worker_threads_shim.js.txt",
            "4d763a31eeb9854ce85c714ed5729debf081bb9cc6b50b49a31c45a6e2f169e6",
        ),
        (
            "zlib_shim.js.txt",
            "79e741c8d42a8c4d73eeac2962b89b0c05ed38678f18e5055b904737bbfc6971",
        ),
    ];

    fn sha256_hex(bytes: &[u8]) -> String {
        let digest = Sha256::digest(bytes);
        let mut hex = String::with_capacity(64);
        for byte in digest.iter() {
            let _ = write!(hex, "{byte:02x}");
        }
        hex
    }

    /// Every embedded shim must inflate to the exact pre-deflate source bytes.
    #[test]
    fn embedded_shims_inflate_to_the_pre_deflate_golden_bytes() {
        let shims = pir::extensions::api::all_shims();
        assert_eq!(shims.len(), GOLDEN.len(), "shim manifest drifted");

        let mut seen: BTreeSet<&str> = BTreeSet::new();
        for (name, shim) in &shims {
            assert_eq!(shim.name(), *name, "manifest name/static mismatch");
            assert!(seen.insert(*name), "duplicate shim {name}");

            let text = shim
                .inflate()
                .unwrap_or_else(|error| panic!("{name}: inflate failed: {error}"));
            let golden = GOLDEN
                .iter()
                .find(|(golden_name, _)| golden_name == name)
                .unwrap_or_else(|| panic!("{name} is not in GOLDEN"))
                .1;
            assert_eq!(
                sha256_hex(text.as_bytes()),
                golden,
                "{name}: decompressed bytes drifted from the pre-deflate source"
            );

            let disk = std::fs::read(format!("src/extensions/api/js/{name}"))
                .unwrap_or_else(|error| panic!("{name}: cannot read source: {error}"));
            // The blobs are generated from canonical LF bytes (see
            // scripts/gen_shims_deflate.py); a Windows checkout with
            // core.autocrlf=true may still hand us CRLF source bytes.
            let disk = String::from_utf8_lossy(&disk)
                .replace("\r\n", "\n")
                .into_bytes();
            assert_eq!(
                text.as_bytes(),
                disk.as_slice(),
                "{name}: blob is stale; run `python scripts/gen_shims_deflate.py`"
            );

            // The OnceLock path must agree and must hand back one allocation.
            assert_eq!(shim.source().as_bytes(), disk.as_slice());
            assert!(
                std::ptr::eq(shim.source(), shim.source()),
                "{name}: source() did not cache its inflation"
            );
        }

        for (name, _) in GOLDEN {
            assert!(seen.contains(name), "{name} is not embedded by the crate");
        }
    }

    /// The inflate gate above walks `all_shims()`; this test pins the other
    /// direction so a newly added `*.js.txt` cannot silently ship without a
    /// blob: every source is either embedded through one of the shim
    /// registries or is the one file `src/main_cli` embeds directly
    /// (`script_boot.js.txt`, `include_str!`, no deflate), and `assets/shims`
    /// holds exactly one blob per embedded shim (no missing or orphan blobs).
    #[test]
    fn shim_sources_and_blobs_are_fully_covered() {
        /// Consumed as UTF-8 by `src/main_cli/js_mode.rs`, outside this
        /// registry (see `scripts/gen_shims_deflate.py`'s SKIP set).
        const DIRECT_SOURCES: &[&str] = &["script_boot.js.txt"];

        let embedded: BTreeSet<&str> = pir::extensions::api::all_shims()
            .into_iter()
            .map(|(name, _)| name)
            .collect();
        assert_eq!(embedded.len(), 90, "embedded shim count");

        let mut sources: BTreeSet<String> = BTreeSet::new();
        for entry in std::fs::read_dir("src/extensions/api/js").expect("read shim source dir") {
            let path = entry.expect("shim source dir entry").path();
            let name = path
                .file_name()
                .and_then(|name| name.to_str())
                .expect("utf-8 shim source name");
            if name.ends_with(".js.txt") {
                sources.insert(name.to_owned());
            }
        }
        assert_eq!(
            sources.len(),
            embedded.len() + DIRECT_SOURCES.len(),
            "source directory has unaccounted `.js.txt` files: {sources:?}"
        );
        for name in &sources {
            assert!(
                embedded.contains(name.as_str()) || DIRECT_SOURCES.contains(&name.as_str()),
                "{name}: source is neither embedded by all_shims() nor an allowlisted direct include"
            );
        }
        for name in &embedded {
            assert!(
                sources.contains(*name),
                "{name}: embedded shim has no source file"
            );
        }
        for name in DIRECT_SOURCES {
            assert!(sources.contains(*name), "direct include {name} is missing");
            assert!(
                !embedded.contains(*name),
                "{name} must not also be deflate-embedded"
            );
        }

        let mut blob_sources: BTreeSet<String> = BTreeSet::new();
        for entry in std::fs::read_dir("assets/shims").expect("read shim blob dir") {
            let path = entry.expect("shim blob dir entry").path();
            let name = path
                .file_name()
                .and_then(|name| name.to_str())
                .expect("utf-8 shim blob name");
            let source = name
                .strip_suffix(".deflate")
                .unwrap_or_else(|| panic!("unexpected non-deflate asset {name}"));
            assert!(
                source.ends_with(".js.txt"),
                "blob {name} does not name a `.js.txt` source"
            );
            blob_sources.insert(source.to_owned());
        }
        let embedded_names: BTreeSet<String> =
            embedded.iter().map(|name| (*name).to_owned()).collect();
        assert_eq!(
            blob_sources, embedded_names,
            "assets/shims must hold exactly one blob per embedded shim (missing or orphan blob)"
        );
    }

    /// Mechanism-level boot cost. Not a gate: the only assertion is a loose
    /// sanity bound, and the numbers are printed for the W19-C startup budget.
    #[test]
    fn measure_shim_inflate_cost_non_gate() {
        let shims = pir::extensions::api::all_shims();
        let mut rows: Vec<(Duration, &str, usize, usize)> = Vec::with_capacity(shims.len());

        let total_start = Instant::now();
        for (name, shim) in &shims {
            let start = Instant::now();
            let text = shim
                .inflate()
                .unwrap_or_else(|error| panic!("{name}: inflate failed: {error}"));
            rows.push((start.elapsed(), name, shim.deflated().len(), text.len()));
        }
        let total = total_start.elapsed();

        let deflated: usize = rows.iter().map(|row| row.2).sum();
        let raw: usize = rows.iter().map(|row| row.3).sum();
        rows.sort_by_key(|row| std::cmp::Reverse(row.0));

        eprintln!(
            "[non-gate] embedded shims: {} assets, {deflated} deflated -> {raw} raw bytes",
            rows.len()
        );
        for (elapsed, name, deflated, raw) in rows.iter().take(10) {
            eprintln!(
                "[non-gate]   {:>9.3?}  {deflated:>8} -> {raw:>8}  {name}",
                elapsed
            );
        }
        eprintln!("[non-gate] fresh inflate of every shim: {total:.3?}");

        // Loose sanity bound only — the real gate lives in W19-C.
        assert!(total < Duration::from_secs(10), "inflate took {total:?}");
    }

    /// Regenerate one embedded shim blob from its checked-in source.
    ///
    /// `scripts/gen_shims_deflate.py` is the canonical generator, but the lane
    /// environment has no Python. This ignored test writes the SAME format
    /// (RFC 1950 zlib, level 9, no dictionary); the gate above only requires
    /// that the blob inflates to the source bytes and that GOLDEN matches the
    /// source SHA-256, so any valid zlib stream is equivalent.
    ///
    /// Run with:
    /// `cargo test --test g_extensions -- --ignored regenerate_shim_deflate`
    #[test]
    #[ignore = "one-shot generator: run after editing a *.js.txt shim source"]
    fn regenerate_shim_deflate() {
        use std::io::Write as _;
        let names: Vec<String> = std::env::var("PIR_REGENERATE_SHIMS")
            .map(|list| list.split(',').map(str::to_owned).collect())
            .unwrap_or_else(|_| vec!["prelude.js.txt".to_owned()]);
        for name in &names {
            let name = name.as_str();
            let source_path = format!("src/extensions/api/js/{name}");
            let source = std::fs::read(&source_path)
                .unwrap_or_else(|error| panic!("{source_path}: {error}"));
            let mut encoder =
                flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::new(9));
            encoder
                .write_all(&source)
                .unwrap_or_else(|error| panic!("{name}: {error}"));
            let blob = encoder
                .finish()
                .unwrap_or_else(|error| panic!("{name}: {error}"));
            let out = format!("assets/shims/{name}.deflate");
            std::fs::write(&out, &blob).unwrap_or_else(|error| panic!("{out}: {error}"));
            eprintln!("[regen] {out}: {} -> {} bytes", source.len(), blob.len());
        }
    }
}
