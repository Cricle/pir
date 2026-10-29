//! Native extension host interface (audit §4 #5, §6 #21, §7 #11) plus the
//! plugin-service `projectTrusted` propagation (Z lane residue).
//!
//! - `NativeSessionHost` replaces the documented no-op
//!   `SessionService::new_session/fork/switch_session` with typed results, a
//!   caller cancellation signal, and an explicit capability probe.
//! - `abort_compaction()` cancels a THRESHOLD compaction independently of the
//!   run signal and does not affect later turns.
//! - native `ctx.ui.custom` carries `overlay: true` + `overlayOptions` on the
//!   existing DialogRequest wire, publishes `getBounds()` through the new
//!   host bounds seam, and keeps the editor-slot default without options.
//! - the live `SessionService::is_project_trusted` reads the SAME resolved
//!   value as the run gate (extension decision + pinned denial).

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{json, Value as JsonValue};

use pir::agent::tool_execution::ToolRegistry;
use pir::agent_session::{
    native_session_host, AgentSession, AgentSessionConfig, NativeCapability, NativeHostError,
    NativeOverlayFactory, NativeSessionAction, NativeSessionHost, SessionEvent,
};
use pir::ai::auth::ApiKeyAuth;
use pir::ai::event_stream::AssistantMessageEventStream;
use pir::ai::provider::{Provider, ProviderRegistry};
use pir::ai::types::{
    AbortSignal, AssistantMessage, ContentBlock, Context, ErrorReason, Message, Model, ModelCost,
    ModelCostRates, StopReason, StreamOptions, TerminalReason, TextContent, ThinkingLevel,
    UserMessage, UserRole,
};
use pir::extensions::{
    ExtensionConfig, ExtensionHandle, NativeOverlayAnchor, NativeOverlayBounds,
    NativeOverlayHandle, NativeOverlayMargin, NativeOverlayOptions, NativeOverlaySize,
    Notification,
};
use pir::plugins::plugin::services::{
    DialogOptions, FactoryRenderContext, ForkOptions, NewSessionOptions, OverlayComponent,
    OverlayComponentFactory, OverlayDone,
};
use pir::plugins::plugin::{AgentPlugin, PluginContext};
use pir::plugins::PluginInfo;
use pir::session_manager::SessionManager;

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

struct ProbePlugin {
    info: PluginInfo,
    contexts: Mutex<Vec<PluginContext>>,
    cancel_switch: bool,
}

impl ProbePlugin {
    fn new(name: &str) -> Arc<Self> {
        Arc::new(Self {
            info: PluginInfo::inline(name),
            contexts: Mutex::new(Vec::new()),
            cancel_switch: false,
        })
    }

    fn cancelling(name: &str) -> Arc<Self> {
        Arc::new(Self {
            info: PluginInfo::inline(name),
            contexts: Mutex::new(Vec::new()),
            cancel_switch: true,
        })
    }

    fn last_context(&self) -> PluginContext {
        self.contexts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .last()
            .cloned()
            .expect("the session bound a context")
    }
}

impl AgentPlugin for ProbePlugin {
    fn info(&self) -> &PluginInfo {
        &self.info
    }

    fn events(&self) -> pir::plugins::plugin::EventInterest {
        if self.cancel_switch {
            pir::plugins::plugin::EventInterest::List(vec!["session_before_switch".to_owned()])
        } else {
            pir::plugins::plugin::EventInterest::None
        }
    }

    fn on_event<'a>(
        &'a self,
        event: pir::plugins::plugin::PluginEvent<'a>,
    ) -> pir::plugins::plugin::PluginFuture<'a, pir::plugins::plugin::PluginEventOutcome> {
        Box::pin(async move {
            if event.name == "session_before_switch" && self.cancel_switch {
                pir::plugins::plugin::PluginEventOutcome::SessionBefore { cancel: true }
            } else {
                pir::plugins::plugin::PluginEventOutcome::None
            }
        })
    }

    fn on_context(&self, ctx: &PluginContext) {
        self.contexts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(ctx.clone());
    }
}

/// Provider used by the replacement/overlay tests: it is never streamed (the
/// prompt path is not exercised there). The compaction tests use
/// [`CompactionProvider`].
struct FakeProvider;

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

    fn stream(
        &self,
        _model: &Model,
        _context: Context,
        _options: StreamOptions,
    ) -> AssistantMessageEventStream {
        AssistantMessageEventStream::new()
    }

    fn api_key_auth(&self) -> Vec<ApiKeyAuth> {
        Vec::new()
    }
}

/// Scripted provider for the compaction tests: summarizer requests are
/// recognized by their system prompt; the FIRST summarizer call stays pending
/// until its request signal aborts (manual path) or is dropped (automatic
/// path), later calls answer immediately.
struct CompactionProvider {
    summarizer_calls: AtomicUsize,
    summarizer_started: AtomicUsize,
    summarizer_aborted: Arc<AtomicUsize>,
}

impl CompactionProvider {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            summarizer_calls: AtomicUsize::new(0),
            summarizer_started: AtomicUsize::new(0),
            summarizer_aborted: Arc::new(AtomicUsize::new(0)),
        })
    }
}

fn is_summarizer(context: &Context) -> bool {
    context.system_prompt.as_deref() == Some(pir::compaction::prompts::SUMMARIZATION_SYSTEM_PROMPT)
}

fn done_stream(text: &str) -> AssistantMessageEventStream {
    let stream = AssistantMessageEventStream::new();
    stream.push(AssistantMessageEventStream::done_event(
        TerminalReason::Stop,
        AssistantMessage {
            stop_reason: StopReason::Stop,
            content: vec![ContentBlock::Text(TextContent {
                text: text.to_owned(),
                text_signature: None,
            })],
            ..AssistantMessage::default()
        },
    ));
    stream
}

impl Provider for CompactionProvider {
    fn id(&self) -> &str {
        "fake"
    }

    fn name(&self) -> &str {
        "Fake provider"
    }

    fn get_models(&self) -> Vec<Model> {
        vec![fake_model()]
    }

    fn stream(
        &self,
        _model: &Model,
        context: Context,
        options: StreamOptions,
    ) -> AssistantMessageEventStream {
        if !is_summarizer(&context) {
            return done_stream("reply");
        }
        let call = self.summarizer_calls.fetch_add(1, Ordering::SeqCst) + 1;
        if call > 1 {
            return done_stream("COMPACT SUMMARY");
        }
        self.summarizer_started.fetch_add(1, Ordering::SeqCst);
        let stream = AssistantMessageEventStream::new();
        let sender = stream.clone();
        let aborted = Arc::clone(&self.summarizer_aborted);
        if let Some(signal) = options.signal.clone() {
            if let Ok(handle) = tokio::runtime::Handle::try_current() {
                // Ends when the signal aborts (manual cancellation) or after a
                // bounded wait (the automatic path drops the request future;
                // the push then finds no receiver and is a no-op).
                drop(handle.spawn(async move {
                    if tokio::time::timeout(Duration::from_secs(5), signal.cancelled())
                        .await
                        .is_ok()
                    {
                        aborted.fetch_add(1, Ordering::SeqCst);
                    }
                    sender.push(AssistantMessageEventStream::error_event(
                        ErrorReason::Aborted,
                        AssistantMessage {
                            stop_reason: StopReason::Aborted,
                            ..AssistantMessage::default()
                        },
                    ));
                }));
            }
        }
        stream
    }

    fn api_key_auth(&self) -> Vec<ApiKeyAuth> {
        Vec::new()
    }
}

fn fake_model() -> Model {
    Model {
        id: "model-a".to_owned(),
        name: "Model A".to_owned(),
        api: "openai-completions".to_owned(),
        provider: "fake".to_owned(),
        base_url: "http://fake.invalid/v1".to_owned(),
        reasoning: false,
        thinking_level_map: None,
        input: vec!["text".to_owned()],
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

fn fake_registry() -> Arc<ProviderRegistry> {
    let mut registry = ProviderRegistry::default();
    registry.set_provider(Arc::new(FakeProvider));
    Arc::new(registry)
}

fn compaction_registry(provider: Arc<CompactionProvider>) -> Arc<ProviderRegistry> {
    let mut registry = ProviderRegistry::default();
    registry.set_provider(provider);
    Arc::new(registry)
}

struct TempDir(PathBuf);

impl TempDir {
    fn new(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "pir-native-host-{label}-{}-{}",
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

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn session_config(
    temp: &TempDir,
    registry: Arc<ProviderRegistry>,
    session: Option<SessionManager>,
    extensions: Option<ExtensionHandle>,
    compaction: Option<pir::compaction::CompactionConfig>,
) -> AgentSessionConfig {
    AgentSessionConfig {
        provider_registry: registry,
        model: fake_model(),
        api_key: None,
        thinking: ThinkingLevel::Off,
        tools: ToolRegistry::new(),
        session,
        compaction,
        max_turns: 4,
        system_prompt: None,
        cwd: Some(temp.path().to_path_buf()),
        event_stream: None,
        extensions,
        tool_selection: None,
    }
}

fn stored_session(temp: &TempDir) -> SessionManager {
    SessionManager::create(
        temp.path().to_str().expect("utf-8 temp path"),
        temp.path().join("sessions"),
    )
    .expect("create session")
}

fn arc_session(temp: &TempDir, plugin: &Arc<ProbePlugin>) -> Arc<AgentSession> {
    AgentSession::new_arc_with_plugins(
        session_config(
            temp,
            fake_registry(),
            Some(stored_session(temp)),
            None,
            None,
        ),
        vec![Arc::clone(plugin) as Arc<dyn AgentPlugin>],
    )
}

fn ui_handle(temp: &TempDir, has_ui: bool) -> ExtensionHandle {
    ExtensionHandle::start_plan_only(ExtensionConfig {
        cwd: temp.path().to_path_buf(),
        mode: "interactive".to_owned(),
        has_ui,
        model: None,
        session_id: None,
        system_prompt: String::new(),
        is_idle: true,
        limits: Some(Default::default()),
    })
}

async fn wait_until(label: &str, mut predicate: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !predicate() {
        assert!(Instant::now() < deadline, "timed out waiting for {label}");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

fn dialog_requests(handle: &ExtensionHandle) -> Vec<(u64, String, JsonValue)> {
    handle
        .take_notifications(|notification| {
            matches!(notification, Notification::DialogRequest { .. })
        })
        .into_iter()
        .filter_map(|notification| match notification {
            Notification::DialogRequest {
                id, kind, options, ..
            } => Some((id, kind, options)),
            _ => None,
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Session replacement: success
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn native_new_session_returns_the_replacement_and_rekeys_the_host() {
    let temp = TempDir::new("new-session");
    let plugin = ProbePlugin::new("new-session-probe");
    let session = arc_session(&temp, &plugin);
    let old_id = session.session_id().expect("initial id");
    let old_path = session.session_path().expect("initial path");

    let ctx = plugin.last_context();
    let host = native_session_host(&ctx).expect("the Arc session registered a native host");
    assert_eq!(
        host.capability(NativeSessionAction::NewSession),
        NativeCapability::Available
    );

    let cancel = AbortSignal::new();
    let replacement = host
        .new_session(NewSessionOptions::default(), cancel)
        .await
        .expect("newSession succeeds");

    assert!(replacement.replaced_storage);
    let new_id = replacement.session_id.clone().expect("replacement id");
    assert_ne!(new_id, old_id);
    assert_eq!(session.session_id().as_deref(), Some(new_id.as_str()));
    assert_eq!(session.session_path(), replacement.session_file);
    assert_ne!(session.session_path(), Some(old_path));
    assert!(replacement.session_file.is_some());

    // The registry re-keyed to the new id: the same (live) context resolves
    // again and reports the replacement session.
    let host_after = native_session_host(&ctx).expect("re-keyed native host");
    assert_eq!(host_after.session_id(), session.session_id());
}

#[tokio::test(flavor = "current_thread")]
async fn native_switch_and_fork_report_typed_results_and_failures() {
    let temp = TempDir::new("switch-fork");
    let plugin = ProbePlugin::new("switch-fork-probe");
    let session = arc_session(&temp, &plugin);
    let host = native_session_host(&plugin.last_context()).expect("native host");

    // A real target session file (the creation rule writes it on the first
    // message).
    let mut target = stored_session(&temp);
    target
        .append_message(Message::User(UserMessage {
            role: UserRole::User,
            content: vec![ContentBlock::Text(TextContent {
                text: "target seed".to_owned(),
                text_signature: None,
            })],
            tools_added: None,
            tools_removed: None,
            timestamp: 1,
            sections: None,
        }))
        .expect("seed target");
    let target_id = target.id().to_owned();
    let target_path = target
        .session_file()
        .expect("persisted target")
        .to_path_buf();
    drop(target);

    let switched = host
        .switch_session(target_path.clone(), AbortSignal::new())
        .await
        .expect("switchSession succeeds");
    assert!(switched.replaced_storage);
    assert_eq!(switched.session_id.as_deref(), Some(target_id.as_str()));
    assert_eq!(session.session_id().as_deref(), Some(target_id.as_str()));
    assert_eq!(switched.session_file, Some(target_path.clone()));

    // Fork at the current position: a new file, same history, new id.
    let forked = host
        .fork(None, ForkOptions::default(), AbortSignal::new())
        .await
        .expect("fork-at-current succeeds");
    assert!(forked.replaced_storage);
    assert_ne!(forked.session_id.as_deref(), Some(target_id.as_str()));
    assert!(forked.session_file.is_some());
    assert_ne!(forked.session_file, Some(target_path));

    // Invalid entry id -> Failed with the upstream text; session untouched.
    let before = session.session_id();
    let error = host
        .fork(
            Some("missing-entry".to_owned()),
            ForkOptions::default(),
            AbortSignal::new(),
        )
        .await
        .expect_err("invalid fork entry");
    match error {
        NativeHostError::Failed { message } => {
            assert!(
                message.contains("Invalid entry ID for forking"),
                "{message}"
            );
        }
        other => panic!("expected Failed, got {other:?}"),
    }
    assert_eq!(session.session_id(), before);
}

// ---------------------------------------------------------------------------
// Session replacement: cancellation
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn native_session_change_honors_the_caller_signal() {
    let temp = TempDir::new("cancel-signal");
    let plugin = ProbePlugin::new("cancel-signal-probe");
    let session = arc_session(&temp, &plugin);
    let before = session.session_id();
    let host = native_session_host(&plugin.last_context()).expect("native host");

    let cancel = AbortSignal::new();
    cancel.abort();
    let error = host
        .new_session(NewSessionOptions::default(), cancel)
        .await
        .expect_err("a pre-aborted call is cancelled");
    assert!(
        matches!(error, NativeHostError::Cancelled { .. }),
        "{error:?}"
    );
    assert_eq!(session.session_id(), before, "the session was not replaced");

    // An extension handler can still refuse (session_before_switch).
    let mut target = stored_session(&temp);
    target
        .append_message(Message::User(UserMessage {
            role: UserRole::User,
            content: vec![ContentBlock::Text(TextContent {
                text: "target".to_owned(),
                text_signature: None,
            })],
            tools_added: None,
            tools_removed: None,
            timestamp: 2,
            sections: None,
        }))
        .expect("seed target");
    let target_path = target.session_file().expect("target file").to_path_buf();
    drop(target);

    let cancelling = ProbePlugin::cancelling("cancel-switch-probe");
    let guarded = AgentSession::new_arc_with_plugins(
        session_config(
            &temp,
            fake_registry(),
            Some(stored_session(&temp)),
            None,
            None,
        ),
        vec![Arc::clone(&cancelling) as Arc<dyn AgentPlugin>],
    );
    let guarded_before = guarded.session_id();
    let guarded_host =
        native_session_host(&cancelling.last_context()).expect("guarded native host");
    let error = guarded_host
        .switch_session(target_path, AbortSignal::new())
        .await
        .expect_err("the session_before_switch handler cancelled");
    assert!(
        matches!(error, NativeHostError::Cancelled { .. }),
        "{error:?}"
    );
    assert_eq!(guarded.session_id(), guarded_before);
}

#[tokio::test(flavor = "current_thread")]
async fn native_session_host_reports_unsupported_without_a_registered_session() {
    let temp = TempDir::new("unsupported");
    let plugin = ProbePlugin::new("unsupported-probe");
    // No storage: no session id, so nothing is registered.
    let ephemeral = AgentSession::new_arc_with_plugins(
        session_config(&temp, fake_registry(), None, None, None),
        vec![Arc::clone(&plugin) as Arc<dyn AgentPlugin>],
    );
    assert_eq!(
        ephemeral.native_session_capability(NativeSessionAction::SwitchSession),
        NativeCapability::Unsupported {
            reason: "no attached session storage; the run stays ephemeral".to_owned(),
        }
    );
    let error = native_session_host(&plugin.last_context())
        .expect_err("no native host is registered for an ephemeral session");
    assert!(
        matches!(error, NativeHostError::Unsupported { .. }),
        "{error:?}"
    );
    // The Arc handle exists (the session was Arc-constructed) but every action
    // reports Unsupported, which is the honest capability answer.
    assert!(ephemeral.native_host().is_some());

    // A by-value session can run the replacement, but cannot hand out a
    // native host (no `Arc`/registry entry): the plugin-facing probe says
    // Unsupported instead of pretending.
    let by_value_plugin = ProbePlugin::new("by-value-probe");
    let by_value = AgentSession::new_with_plugins(
        session_config(
            &temp,
            fake_registry(),
            Some(stored_session(&temp)),
            None,
            None,
        ),
        vec![Arc::clone(&by_value_plugin) as Arc<dyn AgentPlugin>],
    );
    let error = native_session_host(&by_value_plugin.last_context())
        .expect_err("a by-value session has no native host registration");
    assert!(
        matches!(error, NativeHostError::Unsupported { .. }),
        "{error:?}"
    );
    // The direct capability read is still honest.
    assert!(matches!(
        by_value.native_session_capability(NativeSessionAction::NewSession),
        NativeCapability::Available
    ));
}

// ---------------------------------------------------------------------------
// Automatic compaction cancellation (§6 #21)
// ---------------------------------------------------------------------------

fn tiny_window_session(temp: &TempDir, provider: Arc<CompactionProvider>) -> Arc<AgentSession> {
    let mut model = fake_model();
    model.context_window = 64;
    let mut config = session_config(
        temp,
        compaction_registry(provider),
        Some(stored_session(temp)),
        None,
        Some(pir::compaction::CompactionConfig {
            enabled: true,
            reserve_tokens: 1,
            keep_recent_tokens: 8,
        }),
    );
    config.model = model;
    Arc::new(AgentSession::new(config))
}

type CompactionEnds = Arc<Mutex<Vec<(bool, Option<String>)>>>;

/// A cancel of the AUTOMATIC compaction aborts only that summarizer: the turn
/// continues to its model request, reaches a normal stop, and a later turn
/// arms a fresh signal and compacts successfully.
#[tokio::test(flavor = "current_thread")]
async fn abort_compaction_cancels_only_the_automatic_run_and_not_later_turns() {
    let temp = TempDir::new("auto-compaction-cancel");
    let provider = CompactionProvider::new();
    let agent = tiny_window_session(&temp, Arc::clone(&provider));

    let ends: CompactionEnds = Arc::new(Mutex::new(Vec::new()));
    let ends_for = Arc::clone(&ends);
    agent.on_session_event(Arc::new(move |event| {
        if let SessionEvent::CompactionEnd {
            aborted,
            error_message,
            ..
        } = event
        {
            ends_for
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push((aborted, error_message));
        }
    }));

    assert!(!agent.abort_compaction(), "idle: nothing to cancel");

    let noop: pir::agent_session::EventHandler = Arc::new(|_| {});
    let long = |tag: &str| format!("{tag}-{}", "x".repeat(400));
    // Turn 1: the trigger fires but nothing precedes the cut point yet.
    agent
        .prompt(&long("u1"), Arc::clone(&noop))
        .await
        .expect("turn 1");
    assert_eq!(
        provider.summarizer_started.load(Ordering::SeqCst),
        0,
        "no summarizer on the first turn"
    );

    // Turn 2: the threshold compaction starts and stays on the summarizer.
    let turn_two = tokio::spawn({
        let agent = Arc::clone(&agent);
        let text = long("u2");
        async move { agent.prompt(&text, Arc::new(|_| {})).await }
    });
    wait_until("the automatic summarizer to start", || {
        provider.summarizer_started.load(Ordering::SeqCst) == 1
    })
    .await;
    assert!(agent.is_compacting());
    assert!(agent.is_streaming());

    assert!(
        agent.abort_compaction(),
        "the in-flight automatic compaction is cancellable"
    );
    let calls_at_abort = provider.summarizer_calls.load(Ordering::SeqCst);
    let terminal = tokio::time::timeout(Duration::from_secs(10), turn_two)
        .await
        .expect("the turn is not aborted by the compaction cancel")
        .expect("prompt task joins")
        .expect("prompt result");
    assert_eq!(
        terminal.stop_reason,
        StopReason::Stop,
        "the run must continue after cancelling only the compaction"
    );
    assert!(!agent.is_compacting());
    assert!(!agent.is_streaming());

    // Turn 3: a fresh trigger runs a NEW summarizer and compacts for real.
    let terminal = agent
        .prompt(&long("u3"), Arc::clone(&noop))
        .await
        .expect("turn 3");
    assert_eq!(terminal.stop_reason, StopReason::Stop);
    assert!(
        provider.summarizer_calls.load(Ordering::SeqCst) > calls_at_abort,
        "the next turn must arm a fresh compaction signal (calls {} -> {})",
        calls_at_abort,
        provider.summarizer_calls.load(Ordering::SeqCst)
    );

    let ends = ends
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    assert_eq!(
        ends.iter()
            .filter(|(aborted, message)| *aborted && message.is_none())
            .count(),
        1,
        "exactly one cancelled automatic compaction report: {ends:?}"
    );
    assert!(
        ends.iter().any(|(aborted, _)| !*aborted),
        "the later turn's compaction succeeded: {ends:?}"
    );
}

/// The manual path keeps its existing claim + fresh-signal behavior through
/// the SAME `abort_compaction()` entry point, and a cancelled manual
/// compaction does not poison the next one.
#[tokio::test(flavor = "current_thread")]
async fn abort_compaction_cancels_manual_and_allows_the_next_one() {
    let temp = TempDir::new("manual-compaction-cancel");
    let provider = CompactionProvider::new();
    let mut model = fake_model();
    // A huge window keeps automatic compaction idle: only manual runs here.
    model.context_window = 1_000_000;
    let mut config = session_config(
        &temp,
        compaction_registry(Arc::clone(&provider)),
        Some(stored_session(&temp)),
        None,
        Some(pir::compaction::CompactionConfig {
            enabled: true,
            reserve_tokens: 1,
            keep_recent_tokens: 8,
        }),
    );
    config.model = model;
    let agent = Arc::new(AgentSession::new(config));

    let noop: pir::agent_session::EventHandler = Arc::new(|_| {});
    agent
        .prompt(&format!("u1-{}", "x".repeat(400)), Arc::clone(&noop))
        .await
        .expect("seed 1");
    agent
        .prompt(&format!("u2-{}", "y".repeat(400)), Arc::clone(&noop))
        .await
        .expect("seed 2");
    assert!(!agent.is_compacting());

    let compact_task = tokio::spawn({
        let agent = Arc::clone(&agent);
        async move { agent.compact(None).await }
    });
    wait_until("the manual summarizer to start", || {
        provider.summarizer_started.load(Ordering::SeqCst) == 1
    })
    .await;
    assert!(agent.is_compacting());
    assert!(agent.abort_compaction());
    // The cancellation reaches the in-flight summarizer request; the call
    // settles (it must not hang). The terminal shape depends on how the
    // provider reports an aborted stream: an `Err` uses the aborted-error
    // path, while an `Aborted` terminal message is accepted as a partial
    // summary by the history framing (upstream's own contract). Both are
    // non-poisoning.
    let _outcome = tokio::time::timeout(Duration::from_secs(10), compact_task)
        .await
        .expect("the cancelled manual compaction settles")
        .expect("compact task joins");
    assert!(!agent.is_compacting());
    assert!(
        provider.summarizer_aborted.load(Ordering::SeqCst) >= 1,
        "the summarizer request observed the abort signal"
    );

    // A later turn + compaction proves the session signal is usable again.
    agent
        .prompt(&format!("u3-{}", "z".repeat(400)), Arc::clone(&noop))
        .await
        .expect("turn 3");
    let result = agent.compact(None).await.expect("second compact");
    assert_eq!(result.summary, "COMPACT SUMMARY");
    assert!(!agent.is_compacting());
}

// ---------------------------------------------------------------------------
// Native floating custom overlays (§7 #11)
// ---------------------------------------------------------------------------

struct ProbeOverlayComponent;

impl OverlayComponent for ProbeOverlayComponent {
    fn render(&self, _context: &FactoryRenderContext<'_>) -> Vec<String> {
        vec!["probe overlay".to_owned()]
    }
}

struct ProbeOverlayFactory;

impl OverlayComponentFactory for ProbeOverlayFactory {
    fn create(&self, _done: OverlayDone) -> Option<Arc<dyn OverlayComponent>> {
        Some(Arc::new(ProbeOverlayComponent))
    }
}

fn floating_options() -> NativeOverlayOptions {
    NativeOverlayOptions {
        width: Some(NativeOverlaySize::Cells(60)),
        min_width: Some(40),
        max_height: Some(NativeOverlaySize::Percent(50.0)),
        anchor: Some(NativeOverlayAnchor::TopRight),
        offset_x: 2,
        offset_y: -1,
        row: None,
        col: None,
        margin: NativeOverlayMargin::all(1),
        non_capturing: true,
    }
}

struct UiFixture {
    _temp: TempDir,
    session: Arc<AgentSession>,
    extensions: ExtensionHandle,
    plugin: Arc<ProbePlugin>,
}

impl UiFixture {
    fn new(label: &str, has_ui: bool) -> Self {
        let temp = TempDir::new(label);
        let extensions = ui_handle(&temp, has_ui);
        let plugin = ProbePlugin::new(&format!("{label}-probe"));
        let session = AgentSession::new_arc_with_plugins(
            session_config(
                &temp,
                fake_registry(),
                Some(stored_session(&temp)),
                Some(extensions.clone()),
                None,
            ),
            vec![Arc::clone(&plugin) as Arc<dyn AgentPlugin>],
        );
        Self {
            _temp: temp,
            session,
            extensions,
            plugin,
        }
    }

    fn host(&self) -> NativeSessionHost {
        native_session_host(&self.plugin.last_context()).expect("native host")
    }

    fn cell(&self) -> Arc<pir::plugins::host::NativeDialogCell> {
        self.session
            .plugin_host()
            .expect("plugin host")
            .native_dialogs()
    }
}

#[tokio::test(flavor = "current_thread")]
async fn custom_overlay_carries_options_and_publishes_bounds() {
    let fixture = UiFixture::new("overlay-options", true);
    let host = fixture.host();
    assert_eq!(host.ui_capability(), NativeCapability::Available);

    let handle = NativeOverlayHandle::new();
    let pending = host.custom_overlay(
        Arc::new(ProbeOverlayFactory),
        floating_options(),
        handle.clone(),
        None,
    );

    let requests = dialog_requests(&fixture.extensions);
    assert_eq!(requests.len(), 1, "one request per custom call");
    let (id, kind, options) = &requests[0];
    assert_eq!(kind, "custom");
    assert_eq!(options["overlay"], json!(true));
    assert_eq!(
        options["overlayOptions"],
        json!({
            "width": 60,
            "minWidth": 40,
            "maxHeight": "50%",
            "anchor": "top-right",
            "offsetX": 2,
            "offsetY": -1,
            "margin": { "top": 1, "right": 1, "bottom": 1, "left": 1 },
            "nonCapturing": true,
        })
    );
    assert_eq!(handle.id(), Some(*id));

    // The host-side bounds seam fills the native handle's `getBounds()`.
    let bounds = NativeOverlayBounds {
        row: 3,
        col: 7,
        width: 60,
        height: 10,
    };
    assert!(fixture
        .extensions
        .publish_native_overlay_bounds(*id, bounds));
    assert_eq!(handle.get_bounds(), Some(bounds));
    assert!(
        !fixture
            .extensions
            .publish_native_overlay_bounds(id.wrapping_add(9999), bounds),
        "an unregistered id reports no waiter"
    );

    // hide/setHidden ride the reserved status channel with the native key.
    handle.set_hidden(true);
    let hidden: Vec<JsonValue> = fixture
        .extensions
        .take_notifications(|notification| matches!(notification, Notification::Status { .. }))
        .into_iter()
        .filter_map(|notification| match notification {
            Notification::Status { key, text } if key == "__pir_custom_overlay" => {
                text.and_then(|text| serde_json::from_str(&text).ok())
            }
            _ => None,
        })
        .collect();
    assert_eq!(
        hidden,
        vec![json!({
            "component": format!("native:{id}"),
            "op": "hidden",
            "hidden": true,
        })]
    );
    handle.set_hidden(false);
    assert!(!handle.is_hidden());

    // Settle the waiter with the factory's done value.
    assert!(fixture.cell().respond(*id, json!("overlay-result")));
    assert_eq!(
        pending.await.expect("custom resolves"),
        Some(json!("overlay-result"))
    );
    assert!(handle.get_bounds().is_some());
}

#[tokio::test(flavor = "current_thread")]
async fn existing_ui_service_custom_floats_through_the_native_factory_wrapper() {
    let fixture = UiFixture::new("overlay-wrapper", true);
    let ctx = fixture.plugin.last_context();
    let ui = ctx.require_ui().expect("ui service");

    // With the wrapper: `overlay` + `overlayOptions` on the existing trait
    // method's request.
    let handle = NativeOverlayHandle::new();
    let factory = NativeOverlayFactory::new(
        Arc::new(ProbeOverlayFactory),
        floating_options(),
        handle.clone(),
    );
    let pending = ui.custom(factory, DialogOptions::default());
    let requests = dialog_requests(&fixture.extensions);
    assert_eq!(requests.len(), 1);
    let (id, _, options) = &requests[0];
    assert_eq!(options["overlay"], json!(true));
    assert_eq!(options["overlayOptions"]["width"], json!(60));
    assert_eq!(options["overlayOptions"]["anchor"], json!("top-right"));
    assert_eq!(handle.id(), Some(*id));
    assert!(fixture.cell().respond(*id, JsonValue::Null));
    assert_eq!(pending.await.expect("custom resolves"), None);

    // Without the wrapper the documented editor-slot default is unchanged.
    let editor_slot = ui.custom(Arc::new(ProbeOverlayFactory), DialogOptions::default());
    let requests = dialog_requests(&fixture.extensions);
    assert_eq!(requests.len(), 1);
    let (id, _, options) = &requests[0];
    assert!(
        options.get("overlay").is_none(),
        "no options => editor slot: {options}"
    );
    assert!(options.get("overlayOptions").is_none());
    assert!(fixture.cell().respond(*id, JsonValue::Null));
    assert_eq!(editor_slot.await.expect("custom resolves"), None);
}

#[tokio::test(flavor = "current_thread")]
async fn ui_capability_reports_the_missing_sink() {
    let fixture = UiFixture::new("no-ui", false);
    let host = fixture.host();
    match host.ui_capability() {
        NativeCapability::Unsupported { reason } => {
            assert!(reason.contains("without a UI sink"), "{reason}");
        }
        other => panic!("expected Unsupported, got {other:?}"),
    }
    let error = host
        .custom_overlay(
            Arc::new(ProbeOverlayFactory),
            floating_options(),
            NativeOverlayHandle::new(),
            None,
        )
        .await
        .expect_err("no UI sink");
    assert!(
        matches!(error, NativeHostError::Unsupported { .. }),
        "{error:?}"
    );
}

// ---------------------------------------------------------------------------
// The live SessionService's projectTrusted reads the resolved/pinned value
// ---------------------------------------------------------------------------

static TRUST_ENV_LOCK: Mutex<()> = Mutex::new(());

struct TrustEnvGuard {
    trust: Option<OsString>,
}

impl TrustEnvGuard {
    fn new() -> Self {
        let guard = Self {
            trust: std::env::var_os("PIR_TRUST_PROJECT"),
        };
        std::env::remove_var("PIR_TRUST_PROJECT");
        guard
    }
}

impl Drop for TrustEnvGuard {
    fn drop(&mut self) {
        match self.trust.take() {
            Some(value) => std::env::set_var("PIR_TRUST_PROJECT", value),
            None => std::env::remove_var("PIR_TRUST_PROJECT"),
        }
    }
}

#[test]
fn plugin_service_trust_reads_the_extension_decision_and_pinned_denial() {
    use pir::extensions::project_trust::{pin_decision, record_decision, ProjectTrustDecision};

    let _lock = TRUST_ENV_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let _guard = TrustEnvGuard::new();

    // 1. An extension grant recorded before the session is built (the same
    //    state the loader leaves behind) reaches the plugin service.
    let granted = TempDir::new("trust-grant");
    record_decision(granted.path(), ProjectTrustDecision::Yes);
    let grant_plugin = ProbePlugin::new("trust-grant-probe");
    let _grant_session = AgentSession::new_arc_with_plugins(
        session_config(
            &granted,
            fake_registry(),
            Some(
                SessionManager::create(
                    granted.path().to_str().expect("utf-8 path"),
                    granted.path().join("sessions"),
                )
                .expect("session"),
            ),
            None,
            None,
        ),
        vec![Arc::clone(&grant_plugin) as Arc<dyn AgentPlugin>],
    );
    let grant_service = grant_plugin.last_context();
    assert!(
        grant_service
            .session()
            .expect("session service")
            .is_project_trusted(),
        "an extension grant must be visible to the plugin service"
    );

    // 2. A pinned denial (`--no-approve` / an interactive "no") wins over the
    //    trust store and a later extension decision.
    let denied = TempDir::new("trust-denied");
    pin_decision(denied.path(), false);
    record_decision(denied.path(), ProjectTrustDecision::Yes);
    let deny_plugin = ProbePlugin::new("trust-denied-probe");
    let _deny_session = AgentSession::new_arc_with_plugins(
        session_config(
            &denied,
            fake_registry(),
            Some(
                SessionManager::create(
                    denied.path().to_str().expect("utf-8 path"),
                    denied.path().join("sessions"),
                )
                .expect("session"),
            ),
            None,
            None,
        ),
        vec![Arc::clone(&deny_plugin) as Arc<dyn AgentPlugin>],
    );
    let deny_service = deny_plugin.last_context();
    assert!(
        !deny_service
            .session()
            .expect("session service")
            .is_project_trusted(),
        "a pinned denial must be visible to the plugin service"
    );
}
