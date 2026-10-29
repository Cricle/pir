//! Session-command effects on the live `AgentSession` (item 2 of the port
//! stream): `/new`, `/fork`, `/clone`, and `/name` swap or mutate the session
//! storage exactly as upstream `runtimeHost.newSession` / `fork` /
//! `sessionManager.appendSessionInfo` do.
//!
//! The model is faked with a scripted provider (same pattern as
//! `tests/agent_session.rs`); no `~/.pi` configuration is consulted.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use pir::agent_session::{AgentSession, AgentSessionConfig};
use pir::ai::auth::ApiKeyAuth;
use pir::ai::event_stream::AssistantMessageEventStream;
use pir::ai::provider::{Provider, ProviderRegistry};
use pir::ai::types::*;
use pir::session_manager::SessionManager;

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

fn text_block(text: &str) -> ContentBlock {
    ContentBlock::Text(TextContent {
        text: text.into(),
        text_signature: None,
    })
}

fn assistant(content: Vec<ContentBlock>) -> AssistantMessage {
    AssistantMessage {
        api: "fake".into(),
        provider: "fake".into(),
        model: "fake-model".into(),
        content,
        stop_reason: StopReason::Stop,
        timestamp: 1,
        ..Default::default()
    }
}

struct FakeProvider {
    calls: AtomicUsize,
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
        let stream = AssistantMessageEventStream::new();
        stream.push(AssistantMessageEventStream::done_event(
            TerminalReason::Stop,
            assistant(vec![text_block(&format!("reply {index}"))]),
        ));
        stream
    }
}

fn fake_registry() -> Arc<ProviderRegistry> {
    let mut registry = ProviderRegistry::default();
    registry.set_provider(Arc::new(FakeProvider {
        calls: AtomicUsize::new(0),
    }));
    Arc::new(registry)
}

/// Per-test temp session dir.
struct TempDir(std::path::PathBuf);

impl TempDir {
    fn new(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "pir-session-commands-{label}-{}-{}",
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

fn session_with(cwd: &str, dir: &std::path::Path, name: Option<&str>) -> SessionManager {
    let mut session = SessionManager::create(cwd, dir).expect("create session");
    if let Some(name) = name {
        session.set_session_name(Some(name)).expect("seed name");
    }
    session
}

fn agent(session: SessionManager, temp: &TempDir) -> Arc<AgentSession> {
    Arc::new(AgentSession::new(AgentSessionConfig {
        provider_registry: fake_registry(),
        model: fake_model(),
        api_key: None,
        thinking: ThinkingLevel::Off,
        tools: pir::agent::tool_execution::ToolRegistry::new(),
        session: Some(session),
        compaction: None,
        max_turns: 4,
        system_prompt: None,
        cwd: Some(temp.0.clone()),
        event_stream: None,
        extensions: None,
        tool_selection: None,
    }))
}

/// An agent with no durable storage (`--no-session`).
fn ephemeral_agent(temp: &TempDir) -> Arc<AgentSession> {
    Arc::new(AgentSession::new(AgentSessionConfig {
        provider_registry: fake_registry(),
        model: fake_model(),
        api_key: None,
        thinking: ThinkingLevel::Off,
        tools: pir::agent::tool_execution::ToolRegistry::new(),
        session: None,
        compaction: None,
        max_turns: 4,
        system_prompt: None,
        cwd: Some(temp.0.clone()),
        event_stream: None,
        extensions: None,
        tool_selection: None,
    }))
}

/// A no-op event handler (the seams under test never stream).
fn silent() -> pir::agent_session::EventHandler {
    Arc::new(|_| {})
}

// ---------------------------------------------------------------------------
// /name (upstream `handleNameCommand` → `sessionManager.appendSessionInfo`)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn name_command_sets_and_reports_the_session_name() {
    let temp = TempDir::new("name-set");
    let session = session_with(".", &temp.0, None);
    let path = session.path().to_path_buf();
    let agent = agent(session, &temp);

    // Seed a message so the session file exists (pir's file-creation rule:
    // state-only entries buffer until the first user/assistant message —
    // upstream /name runs on a live conversation anyway).
    agent.prompt("hello", silent()).await.expect("prompt");

    let normalized = agent
        .set_session_name("my session")
        .await
        .expect("set name")
        .expect("storage configured");
    assert_eq!(normalized, "my session");

    // The name persists in the session file and the snapshot reports it.
    let reopened = SessionManager::open(&path).expect("reopen");
    assert_eq!(reopened.session_name().as_deref(), Some("my session"));
}

#[tokio::test]
async fn name_command_without_storage_keeps_the_name_in_memory() {
    // Upstream always has a sessionManager, so /name works even without
    // persisted storage; pir keeps the name in memory for the run (the
    // ephemeral field) instead of dropping it.
    let temp = TempDir::new("name-no-storage");
    let agent = ephemeral_agent(&temp);
    let stored = agent.set_session_name("remembered").await.expect("noop");
    assert_eq!(stored.as_deref(), Some("remembered"));
    assert_eq!(agent.session_name().await.as_deref(), Some("remembered"));
}

// ---------------------------------------------------------------------------
// /new (upstream `handleClearCommand` → `runtimeHost.newSession`)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn new_command_swaps_in_fresh_session_storage() {
    let temp = TempDir::new("new-swap");
    let session = session_with(".", &temp.0, Some("original"));
    let original_path = session.path().to_path_buf();
    let agent = agent(session, &temp);
    // Seed a message so the original transcript is non-empty.
    agent.prompt("hello", silent()).await.expect("prompt");

    let fresh = session_with(".", &temp.0, None);
    let fresh_path = fresh.path().to_path_buf();
    assert!(agent.replace_session_storage(fresh).await);

    let state = agent.state();
    assert_eq!(
        state.session_path.as_deref(),
        Some(fresh_path.to_string_lossy().as_ref())
    );
    assert_ne!(fresh_path, original_path);

    // The next prompt reads from the fresh (empty) transcript.
    agent.prompt("after new", silent()).await.expect("prompt");
    let reopened = SessionManager::open(&fresh_path).expect("reopen fresh");
    let texts: Vec<String> = reopened
        .build_context_entries()
        .expect("context")
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
        .collect();
    assert!(
        !texts.iter().any(|text| text == "hello"),
        "the fresh session must not carry the old transcript: {texts:?}"
    );
    assert!(texts.iter().any(|text| text == "after new"), "{texts:?}");
}

#[tokio::test]
async fn new_command_without_storage_reports_failure() {
    let temp = TempDir::new("new-no-storage");
    let agent = ephemeral_agent(&temp);
    let replacement = session_with(".", &temp.0, None);
    assert!(!agent.replace_session_storage(replacement).await);
}

// ---------------------------------------------------------------------------
// /fork and /clone (upstream `runtimeHost.fork(leafId, …)`)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn fork_command_creates_a_linked_copy_and_swaps_it_in() {
    let temp = TempDir::new("fork-swap");
    let session = session_with(".", &temp.0, Some("source"));
    let source_path = session.path().to_path_buf();
    let agent = agent(session, &temp);
    agent.prompt("hello", silent()).await.expect("prompt");

    let forked = SessionManager::fork_from_source(&source_path, ".", &temp.0, None)
        .expect("fork from source");
    let forked_path = forked.path().to_path_buf();
    assert_ne!(forked_path, source_path);
    assert!(agent.replace_session_storage(forked).await);

    // The fork links back to its parent and carries the source transcript.
    let reopened = SessionManager::open(&forked_path).expect("reopen fork");
    let header = reopened.header().expect("header");
    assert_eq!(
        header.parent_session.as_deref(),
        Some(source_path.to_string_lossy().as_ref())
    );
    let messages = reopened.build_context_entries().expect("context");
    assert!(
        messages
            .iter()
            .any(|message| matches!(message, Message::User(_))),
        "the fork must carry the source user message"
    );
    assert_eq!(
        agent.state().session_path.as_deref(),
        Some(forked_path.to_string_lossy().as_ref())
    );
}

#[tokio::test]
async fn clone_command_forks_at_the_current_position() {
    let temp = TempDir::new("clone-swap");
    let session = session_with(".", &temp.0, None);
    let source_path = session.path().to_path_buf();
    let agent = agent(session, &temp);
    agent.prompt("one", silent()).await.expect("prompt");

    // `/clone` forks the current leaf (upstream `handleCloneCommand`).
    let cloned = SessionManager::fork_from_source(&source_path, ".", &temp.0, None)
        .expect("clone from source");
    let cloned_path = cloned.path().to_path_buf();
    assert!(agent.replace_session_storage(cloned).await);

    let state = agent.state();
    assert_eq!(
        state.session_path.as_deref(),
        Some(cloned_path.to_string_lossy().as_ref())
    );
    // The clone's context matches the source at the fork point.
    let reopened = SessionManager::open(&cloned_path).expect("reopen clone");
    let source = SessionManager::open(&source_path).expect("reopen source");
    assert_eq!(
        reopened
            .build_context_entries()
            .expect("clone context")
            .len(),
        source
            .build_context_entries()
            .expect("source context")
            .len()
    );
}

#[tokio::test]
async fn fork_without_storage_reports_failure() {
    let temp = TempDir::new("fork-no-storage");
    let agent = ephemeral_agent(&temp);
    assert!(agent.state().session_path.is_none());
    assert!(
        !agent
            .replace_session_storage(session_with(".", &temp.0, None))
            .await
    );
}

// ---------------------------------------------------------------------------
// Audit §4 #5: `withSession` callbacks (`ctx.newSession` / `ctx.switchSession`
// options) run against the REPLACEMENT session through the host bridge
// ---------------------------------------------------------------------------

/// An extension runtime for a command-ctx fixture: mode `interactive` (an
/// AgentSession-backed host, NOT print), no UI, no pinned session id (the
/// session host pushes the live one).
fn command_extension_config(cwd: &std::path::Path) -> pir::extensions::ExtensionConfig {
    pir::extensions::ExtensionConfig {
        cwd: cwd.to_path_buf(),
        mode: "interactive".to_owned(),
        has_ui: false,
        model: Some("fake-model".to_owned()),
        session_id: None,
        system_prompt: String::new(),
        is_idle: true,
        limits: Some(Default::default()),
    }
}

/// Load one command fixture and return the handle.
async fn load_command_extension(
    temp: &TempDir,
    label: &str,
    source: &str,
) -> pir::extensions::ExtensionHandle {
    let path = temp.0.join(label);
    std::fs::write(&path, source).expect("write extension fixture");
    let handle = pir::extensions::ExtensionHandle::start(command_extension_config(&temp.0))
        .expect("start extension runtime");
    let loaded = handle
        .load(std::slice::from_ref(&path))
        .await
        .expect("load extension fixture");
    assert!(loaded.errors.is_empty(), "load errors: {:?}", loaded.errors);
    handle
}

/// An agent with persistent storage AND an extension runtime.
fn agent_with_extensions(
    temp: &TempDir,
    handle: pir::extensions::ExtensionHandle,
) -> Arc<AgentSession> {
    let session = session_with(".", &temp.0, None);
    Arc::new(AgentSession::new(AgentSessionConfig {
        provider_registry: fake_registry(),
        model: fake_model(),
        api_key: None,
        thinking: ThinkingLevel::Off,
        tools: pir::agent::tool_execution::ToolRegistry::new(),
        session: Some(session),
        compaction: None,
        max_turns: 4,
        system_prompt: None,
        cwd: Some(temp.0.clone()),
        event_stream: None,
        extensions: Some(handle),
        tool_selection: None,
    }))
}

#[tokio::test]
async fn new_session_with_session_callback_runs_against_the_replacement() {
    let temp = TempDir::new("with-session-new");
    let handle = load_command_extension(
        &temp,
        "with_session.ts",
        r#"
        export default function (pi) {
            pi.registerCommand("new-with-session", {
                handler: async (_args, ctx) => {
                    await ctx.newSession({
                        withSession: async (replacement) => {
                            globalThis.__ws_runs = (globalThis.__ws_runs ?? 0) + 1;
                            globalThis.__ws_id = replacement.sessionManager.getSessionId();
                            await replacement.sendUserMessage("hello from withSession");
                        },
                    });
                },
            });
        }
        "#,
    )
    .await;
    let agent = agent_with_extensions(&temp, handle.clone());
    let old_id = agent.session_id().expect("initial session id");
    // The commandAction origin is the host-pushed session id.
    agent.push_session_context_for_test().await;

    let outcome = agent
        .dispatch_plugin_command("new-with-session", "")
        .await
        .expect("dispatch the extension command");
    assert!(
        outcome.ok,
        "withSession command succeeded: {:?}",
        outcome.error
    );

    let new_id = agent.session_id().expect("replacement session id");
    assert_ne!(new_id, old_id, "ctx.newSession replaced the storage");
    assert_eq!(
        handle
            .eval_json("JSON.stringify(globalThis.__ws_runs ?? 0)")
            .await
            .expect("probe callback runs"),
        serde_json::json!(1),
        "the withSession callback ran exactly once"
    );
    assert_eq!(
        handle
            .eval_json("JSON.stringify(globalThis.__ws_id ?? null)")
            .await
            .expect("probe callback session id"),
        serde_json::json!(new_id),
        "the callback's ctx is bound to the REPLACEMENT session"
    );
    // Upstream ReplacedSessionContext also exposes `sendUserMessage` on the
    // callback's ctx: it queues through the same host fn `pi.sendUserMessage`
    // uses (the session applies it on the next drain).
    let sent = handle.take_notifications(|notification| {
        matches!(
            notification,
            pir::extensions::Notification::SendUserMessage { .. }
        )
    });
    assert_eq!(
        sent.len(),
        1,
        "the callback's sendUserMessage reached the host"
    );
}

#[tokio::test]
async fn switch_session_with_session_callback_runs_against_the_opened_session() {
    let temp = TempDir::new("with-session-switch");
    let handle = load_command_extension(
        &temp,
        "with_session_switch.ts",
        r#"
        export default function (pi) {
            pi.registerCommand("switch-with-session", {
                handler: async (sessionPath, ctx) => {
                    await ctx.switchSession(sessionPath, {
                        withSession: async (replacement) => {
                            globalThis.__ws_switch_id = replacement.sessionManager.getSessionId();
                        },
                    });
                },
            });
        }
        "#,
    )
    .await;
    // The destination session the command switches to. It needs one message:
    // an unflushed session has no file on disk yet, and `SessionManager::open`
    // would start a fresh id at that path.
    let mut target = session_with(".", &temp.0, Some("target-session"));
    target
        .append_message(Message::User(UserMessage {
            role: UserRole::User,
            content: vec![text_block("target transcript")],
            tools_added: None,
            tools_removed: None,
            timestamp: pir::ai::util::now_ms(),
            sections: None,
        }))
        .expect("seed target transcript");
    let target_id = target.id().to_owned();
    let target_path = target.path().to_path_buf();

    let agent = agent_with_extensions(&temp, handle.clone());
    let old_id = agent.session_id().expect("initial session id");
    agent.push_session_context_for_test().await;

    let outcome = agent
        .dispatch_plugin_command("switch-with-session", &target_path.display().to_string())
        .await
        .expect("dispatch the extension command");
    assert!(
        outcome.ok,
        "withSession switch succeeded: {:?}",
        outcome.error
    );

    assert_ne!(target_id, old_id);
    assert_eq!(
        agent.session_id().as_deref(),
        Some(target_id.as_str()),
        "ctx.switchSession swapped in the target storage"
    );
    assert_eq!(
        handle
            .eval_json("JSON.stringify(globalThis.__ws_switch_id ?? null)")
            .await
            .expect("probe callback session id"),
        serde_json::json!(target_id),
        "the callback observed the opened session"
    );
}

#[tokio::test]
async fn with_session_callback_failure_is_reported_after_the_replacement() {
    let temp = TempDir::new("with-session-throw");
    let handle = load_command_extension(
        &temp,
        "with_session_throw.ts",
        r#"
        export default function (pi) {
            pi.registerCommand("new-with-throwing-session", {
                handler: async (_args, ctx) => {
                    await ctx.newSession({
                        withSession: async () => { throw new Error("withSession exploded"); },
                    });
                },
            });
        }
        "#,
    )
    .await;
    let agent = agent_with_extensions(&temp, handle);
    let old_id = agent.session_id().expect("initial session id");
    agent.push_session_context_for_test().await;

    let outcome = agent
        .dispatch_plugin_command("new-with-throwing-session", "")
        .await
        .expect("dispatch the extension command");
    assert!(!outcome.ok, "a throwing withSession callback is a failure");
    let error = outcome.error.unwrap_or_default();
    assert!(
        error.contains("withSession callback failed") && error.contains("withSession exploded"),
        "the failure names the callback error: {error}"
    );
    assert_ne!(
        agent.session_id().as_deref(),
        Some(old_id.as_str()),
        "upstream replaces the session and then runs the callback; the \
         replacement is not rolled back"
    );
}

#[tokio::test]
async fn cancelled_replacement_skips_the_with_session_callback() {
    let temp = TempDir::new("with-session-cancel");
    let handle = load_command_extension(
        &temp,
        "with_session_cancel.ts",
        r#"
        export default function (pi) {
            pi.on("session_before_switch", () => ({ cancel: true }));
            pi.registerCommand("new-with-session", {
                handler: async (_args, ctx) => {
                    await ctx.newSession({
                        withSession: async () => {
                            globalThis.__ws_ran_after_cancel =
                                (globalThis.__ws_ran_after_cancel ?? 0) + 1;
                        },
                    });
                },
            });
        }
        "#,
    )
    .await;
    let agent = agent_with_extensions(&temp, handle.clone());
    let old_id = agent.session_id().expect("initial session id");
    agent.push_session_context_for_test().await;

    let outcome = agent
        .dispatch_plugin_command("new-with-session", "")
        .await
        .expect("dispatch the extension command");
    assert!(!outcome.ok, "a cancelled switch fails the action");
    let error = outcome.error.unwrap_or_default();
    assert!(
        error.contains("cancelled"),
        "the failure reports the cancellation: {error}"
    );
    assert_eq!(
        agent.session_id().as_deref(),
        Some(old_id.as_str()),
        "the cancelled switch kept the original session"
    );
    assert_eq!(
        handle
            .eval_json("JSON.stringify(globalThis.__ws_ran_after_cancel ?? 0)")
            .await
            .expect("probe the callback marker"),
        serde_json::json!(0),
        "upstream runs withSession only after a successful replacement"
    );
}
