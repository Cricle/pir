//! Regression tests for the prompt main-path integration fixes:
//!
//! - **P1-1**: the extension command-context action queue (`ctx.newSession` /
//!   `ctx.fork` / ...) is stamped with a per-host [`SessionOrigin`]; a consumer
//!   claims only its own actions (a print host can never drain another
//!   session's actions, and an `AgentSession` host ignores foreign ones).
//! - **P1-3**: the runtime's `ctx.scopedModels` snapshot serializes the LIVE
//!   session-only model scope (upstream `ScopedModel[]`) instead of a
//!   permanently empty list.
//! - **P2-1**: the overflow-recovery trigger and the per-message persistence
//!   predicate read the THRESHOLD model (`AgentSession::threshold_model_for`,
//!   the routed physical model under a virtual selection) — a raw virtual
//!   model's window 0 would fire the durable compact-retry on every prompt and
//!   skip every persistence.
//! - **P2-4**: the cache-warm replay context carries THIS run's (possibly
//!   `before_agent_start`-overridden) system prompt, not the base one —
//!   otherwise the warm request replays a different prefix and misses the
//!   provider cache.
//!
//! Fixtures follow the standalone pattern of `tests/p2_inbox_toctou.rs`: fake
//! in-memory `SessionManager` storage plus a static `Provider` behind the
//! registry — no network, no JS runtime.

use std::path::Path;
use std::sync::{Arc, Mutex as StdMutex};

use pir::agent::tool_execution::ToolRegistry;
use pir::agent_session::{
    record_session_command_action, serialize_scoped_models, take_session_command_actions,
    take_session_command_actions_for, AgentSession, AgentSessionConfig, SessionCommandAction,
    SessionOrigin,
};
use pir::ai::auth::ApiKeyAuth;
use pir::ai::event_stream::AssistantMessageEventStream;
use pir::ai::provider::{Provider, ProviderRegistry};
use pir::ai::providers::ScopedModel;
use pir::ai::types::{
    AssistantMessage, Context, Model, ModelCost, ModelCostRates, StopReason, StreamOptions,
    ThinkingLevel, Usage,
};
use pir::ai::util::is_context_overflow;
use pir::ai::virtual_::{PhysicalModelRef, VirtualModelRegistration};
use pir::session_manager::SessionManager;

/// The command-action queue is process-global: only one test may hold a
/// record+claim window at a time.
static QUEUE_WINDOW: StdMutex<()> = StdMutex::new(());

/// Per-test temp directory removed at the end of the test body.
struct TempDir(std::path::PathBuf);

impl TempDir {
    fn new(label: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "pir-prompt-path-fixes-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|duration| duration.as_nanos())
                .unwrap_or(0)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        TempDir(dir)
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

/// An in-memory session scoped to `cwd` (no provider is ever called).
fn session_with_storage(cwd: &Path, model: Model) -> Arc<AgentSession> {
    let storage = SessionManager::in_memory(&cwd.to_string_lossy(), None).unwrap();
    Arc::new(AgentSession::new(AgentSessionConfig {
        provider_registry: Arc::new(ProviderRegistry::default()),
        model,
        api_key: None,
        thinking: ThinkingLevel::Off,
        tools: ToolRegistry::new(),
        session: Some(storage),
        compaction: None,
        max_turns: 4,
        system_prompt: Some("base prompt".into()),
        cwd: Some(cwd.to_path_buf()),
        event_stream: None,
        extensions: None,
        tool_selection: None,
    }))
}

/// A stop assistant message with the given input usage (silently overflowed
/// per the z.ai shape when the window is exceeded).
fn stop_assistant(input_tokens: u64) -> AssistantMessage {
    AssistantMessage {
        api: "fake".into(),
        provider: "fake".into(),
        model: "fake-model".into(),
        stop_reason: StopReason::Stop,
        usage: Usage {
            input: input_tokens,
            ..Usage::default()
        },
        ..AssistantMessage::default()
    }
}

// ---------------------------------------------------------------------------
// P1-1: command-context actions are origin-scoped
// ---------------------------------------------------------------------------

/// A print host must claim only the actions stamped `Print`; actions stamped
/// with another host's identity stay queued for their owner (before the fix
/// the queue was anonymous, so print drained and applied other hosts'
/// leftover actions while AgentSession hosts had zero consumers).
#[test]
fn print_host_never_claims_other_hosts_actions() {
    let _window = QUEUE_WINDOW
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    // Defensive cleanup: a panicked sibling must not poison this window.
    let _ = take_session_command_actions();

    let print_origin = SessionOrigin::Print;
    let other_origin = SessionOrigin::AgentSession {
        session_id: Some("sess-b".into()),
    };
    record_session_command_action(
        print_origin.clone(),
        SessionCommandAction::NewSession {
            parent_session: None,
            with_session: None,
        },
    );
    record_session_command_action(
        other_origin,
        SessionCommandAction::Fork {
            entry_id: None,
            position: None,
            with_session: None,
        },
    );
    record_session_command_action(
        SessionOrigin::AgentSession {
            session_id: Some("sess-a".into()),
        },
        SessionCommandAction::Reload,
    );

    let claimed = take_session_command_actions_for(&print_origin);
    assert_eq!(
        claimed.len(),
        1,
        "print claims exactly its own action, got {claimed:?}"
    );
    assert!(matches!(
        claimed[0].action,
        SessionCommandAction::NewSession { .. }
    ));

    // The other hosts' actions are NOT in print's claim and NOT dropped:
    // each owner still finds its own.
    let a_claim = take_session_command_actions_for(&SessionOrigin::AgentSession {
        session_id: Some("sess-a".into()),
    });
    assert_eq!(a_claim.len(), 1);
    assert!(matches!(a_claim[0].action, SessionCommandAction::Reload));

    let remaining = take_session_command_actions();
    assert_eq!(
        remaining.len(),
        1,
        "sess-b's action must stay queued for its owner"
    );
    assert!(matches!(remaining[0], SessionCommandAction::Fork { .. }));
}

/// An `AgentSession` host claims (and applies) only the actions stamped with
/// its own session id; a foreign action stays queued untouched.
#[tokio::test]
#[allow(clippy::await_holding_lock)] // the queue lock must span the runtime awaits
async fn agent_session_applies_only_its_own_actions() {
    let _window = QUEUE_WINDOW
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let _ = take_session_command_actions();

    let temp = TempDir::new("apply-filter");
    let agent = session_with_storage(&temp.0, fake_model());

    // A foreign session's action: not claimed, not dropped.
    record_session_command_action(
        SessionOrigin::AgentSession {
            session_id: Some("some-other-session".into()),
        },
        SessionCommandAction::Reload,
    );
    let outcomes = agent.apply_session_command_actions().await;
    assert!(
        outcomes.is_empty(),
        "an AgentSession host must not claim another session's actions, got {outcomes:?}"
    );
    assert_eq!(
        take_session_command_actions().len(),
        1,
        "the foreign action must stay queued for its owner"
    );

    // This session's own action IS claimed and applied. The session has no
    // extension/plugin surface, so `ctx.reload` is a successful no-op
    // (nothing to reload) — the Ok is the observable proof that the action
    // ran through the host's reload seam.
    record_session_command_action(
        SessionOrigin::AgentSession {
            session_id: agent.session_id(),
        },
        SessionCommandAction::Reload,
    );
    let outcomes = agent.apply_session_command_actions().await;
    assert_eq!(outcomes.len(), 1, "the host claims its own action");
    assert!(
        outcomes[0].1.is_ok(),
        "reload is applied through AgentSession::reload (a session with no \
         extension/plugin surface has nothing to reload), got {:?}",
        outcomes[0]
    );
    assert!(take_session_command_actions().is_empty(), "queue drained");
}

// ---------------------------------------------------------------------------
// P1-3: ctx.scopedModels serializes the live scope
// ---------------------------------------------------------------------------

#[test]
fn scoped_models_snapshot_serializes_the_live_scope() {
    // Empty scope → empty array (an unset scope removes the restriction).
    let empty = serialize_scoped_models(&[]);
    assert!(empty.as_array().unwrap().is_empty());

    let scoped = serialize_scoped_models(&[ScopedModel {
        model: fake_model(),
        thinking_level: Some(ThinkingLevel::Off),
    }]);
    let entries = scoped.as_array().expect("scoped snapshot is an array");
    assert_eq!(entries.len(), 1, "the set scope must be non-empty");
    assert_eq!(entries[0]["model"]["id"], "fake-model");
    assert_eq!(entries[0]["model"]["provider"], "fake");
    assert_eq!(entries[0]["thinkingLevel"], "off");

    // An unset thinking level is omitted (upstream `thinkingLevel?`), never
    // a misleading null that would shadow a real level.
    let bare = serialize_scoped_models(&[ScopedModel {
        model: fake_model(),
        thinking_level: None,
    }]);
    assert!(bare[0].get("thinkingLevel").is_none());
    assert_eq!(bare[0]["model"]["id"], "fake-model");
}

// ---------------------------------------------------------------------------
// P2-1: overflow trigger reads the THRESHOLD model
// ---------------------------------------------------------------------------

/// Static provider donating one physical model to the registry (never
/// streamed in these tests).
struct StaticProvider {
    id: &'static str,
    model: Model,
}

impl Provider for StaticProvider {
    fn id(&self) -> &str {
        self.id
    }

    fn name(&self) -> &str {
        self.id
    }

    fn get_models(&self) -> Vec<Model> {
        vec![self.model.clone()]
    }

    fn stream(
        &self,
        _model: &Model,
        _context: Context,
        _options: StreamOptions,
    ) -> AssistantMessageEventStream {
        let output = AssistantMessageEventStream::new();
        output.end(None);
        output
    }

    fn api_key_auth(&self) -> Vec<ApiKeyAuth> {
        Vec::new()
    }
}

/// Under a virtual selection the threshold model is the ROUTED physical model
/// (window 4096), not the raw virtual entry (window 0): a window-0 threshold
/// would classify every non-empty assistant message as an overflow and fire
/// the durable compact-retry / skip persistence on every prompt.
#[test]
fn threshold_model_resolves_routed_physical_window() {
    let mut registry = ProviderRegistry::default();
    registry.set_provider(Arc::new(StaticProvider {
        id: "fake",
        model: fake_model(),
    }));
    let registration = VirtualModelRegistration {
        id: "v-model".into(),
        provider: "fake".into(),
        name: None,
        physical: PhysicalModelRef {
            provider: "fake".into(),
            id: "fake-model".into(),
        },
        reasoning: None,
        system_prompt: None,
        model_override: None,
        source: "test".into(),
    };
    registry
        .register_virtual_model(registration.clone())
        .unwrap();

    let temp = TempDir::new("threshold");
    let storage = SessionManager::in_memory(&temp.0.to_string_lossy(), None).unwrap();
    let agent = Arc::new(AgentSession::new(AgentSessionConfig {
        provider_registry: Arc::new(registry),
        // The raw virtual entry as the session's current model: the catalog
        // fallback without a resolved physical carries window 0.
        model: registration.catalog_model(None),
        api_key: None,
        thinking: ThinkingLevel::Off,
        tools: ToolRegistry::new(),
        session: Some(storage),
        compaction: None,
        max_turns: 4,
        system_prompt: None,
        cwd: Some(temp.0.clone()),
        event_stream: None,
        extensions: None,
        tool_selection: None,
    }));

    let raw = agent.model();
    assert_eq!(
        raw.context_window, 0,
        "the raw virtual entry's window is 0 (the pathological input)"
    );
    let resolved = agent.threshold_model_for(&raw);
    assert_eq!(
        resolved.context_window, 4096,
        "the threshold model must carry the routed physical window"
    );
    assert_eq!(
        resolved.max_tokens, 256,
        "the threshold model must carry the routed physical output cap"
    );

    // A physical selection resolves to itself.
    let physical = fake_model();
    let unchanged = agent.threshold_model_for(&physical);
    assert_eq!(unchanged, physical);

    // The pathology: with the RAW model's window 0 every non-empty response
    // is misclassified as an overflow (the pre-fix trigger fired the durable
    // compact-retry and skipped persistence on every prompt); with the
    // threshold model's window the same response is a normal stop.
    let assistant = stop_assistant(100);
    assert!(
        is_context_overflow(&assistant, Some(0)),
        "window 0 misfires the overflow predicate (the bug's mechanism)"
    );
    assert!(
        !is_context_overflow(&assistant, Some(4096)),
        "the routed physical window classifies the same response correctly"
    );
}

// ---------------------------------------------------------------------------
// P2-4: the warm replay carries THIS run's system prompt
// ---------------------------------------------------------------------------

#[tokio::test]
async fn warm_context_snapshot_uses_the_run_system_prompt() {
    let temp = TempDir::new("warm-prefix");
    let agent = session_with_storage(&temp.0, fake_model());

    // `before_agent_start` overrode the system prompt for the settled run:
    // the warm replay must carry the OVERRIDDEN prompt — warming with the
    // base prompt would replay a different prefix and miss the cache.
    let snapshot = agent.warm_context_snapshot(Some("run prompt".into())).await;
    assert_eq!(
        snapshot.system_prompt.as_deref(),
        Some("run prompt"),
        "the warm prefix must be this run's system prompt, not the base one"
    );

    // Runs without an override warm with `None` exactly like they ran.
    let snapshot = agent.warm_context_snapshot(None).await;
    assert_eq!(snapshot.system_prompt, None);
}

// ---------------------------------------------------------------------------
// `--system-prompt` / `--append-system-prompt` resolution (upstream
// `resolvePromptInput`, resource-loader.ts:167-182)
// ---------------------------------------------------------------------------

#[test]
fn prompt_inputs_resolve_files_and_keep_literals() {
    let dir = std::env::temp_dir().join(format!("pir-print-prompt-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("prompt.md");
    std::fs::write(&file, "\u{feff}You are a test.").unwrap();

    let file_arg = file.to_string_lossy().to_string();
    assert_eq!(
        pir::coding_agent::print::resolve_prompt_input(&file_arg),
        "You are a test."
    );
    assert_eq!(
        pir::coding_agent::print::resolve_text_or_file(&file_arg).unwrap(),
        "You are a test."
    );

    let literal = "plain text prompt";
    assert_eq!(
        pir::coding_agent::print::resolve_prompt_input(literal),
        literal
    );
    assert_eq!(
        pir::coding_agent::print::resolve_text_or_file(literal).unwrap(),
        literal
    );

    // An existing path that cannot be read (a directory): the file branch
    // is attempted, fails, and the literal value still stands.
    let dir_arg = dir.to_string_lossy().to_string();
    assert_eq!(
        pir::coding_agent::print::resolve_prompt_input(&dir_arg),
        dir_arg
    );

    std::fs::remove_dir_all(&dir).unwrap();
}
