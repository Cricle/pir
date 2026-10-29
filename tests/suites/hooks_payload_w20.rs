//! H5 hooks/events payload parity (audit #10): the remaining payload fields,
//! event ordering, and in-place handler semantics.
//!
//! Every test drives a real [`AgentSession`] with an inline plugin recorder so
//! the assertions pin the wire payload the extension faces actually observe:
//!
//! - `session_before_compact.preparation` (turnPrefixMessages / previousSummary
//!   / fileOps / settings);
//! - `session_compact_failed` after `compaction_end`;
//! - `resources_discover.cwd`;
//! - `session_before_switch` / `session_shutdown` `targetSessionFile`;
//! - `session_before_fork` `skipConversationRestore`;
//! - `before_provider_request` in-place mutation;
//! - nested native `ui_prompt_start` / `ui_prompt_end`;
//! - `session_start {reason:"reload"}` on the native face.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{json, Value};

use pir::agent::tool_execution::ToolRegistry;
use pir::agent_session::{
    AgentSession, AgentSessionConfig, ForkEntryOutcome, ForkPosition, SessionEvent,
};
use pir::ai::auth::ApiKeyAuth;
use pir::ai::event_stream::AssistantMessageEventStream;
use pir::ai::provider::{Provider, ProviderRegistry};
use pir::ai::types::*;
use pir::extensions::{ExtensionConfig, ExtensionHandle};
use pir::plugins::plugin::{PluginContext, PluginEvent, PluginEventOutcome, PluginFuture};
use pir::plugins::services::{DialogOptions, SelectOption};
use pir::plugins::{AgentPlugin, EventInterest, PluginHost, PluginInfo};
use pir::session_manager::{AppendFailure, FileEntry, SessionManager};

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

struct TempDir(std::path::PathBuf);

impl TempDir {
    fn new(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "pir-hooks-w20-{label}-{}-{}",
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

fn fake_model() -> Model {
    Model {
        id: "w20-model".into(),
        name: "W20 model".into(),
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
        context_window: 1_000_000,
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

fn assistant_text(text: &str) -> AssistantMessage {
    AssistantMessage {
        api: "fake".into(),
        provider: "fake".into(),
        model: "w20-model".into(),
        content: vec![text_block(text)],
        stop_reason: StopReason::Stop,
        timestamp: 1,
        ..Default::default()
    }
}

/// The context-overflow terminal that triggers the durable recovery path.
fn overflow_error_assistant() -> AssistantMessage {
    let mut message = assistant_text("");
    message.content.clear();
    message.stop_reason = StopReason::Error;
    message.error_message = Some("prompt is too long: 200000 tokens > 16384 maximum".into());
    message
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
        "W20 fake provider"
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

fn ok_registry() -> Arc<ProviderRegistry> {
    fake_registry(Arc::new(|_index, _context| assistant_text("ok")))
}

fn session_config(
    cwd: &std::path::Path,
    session: Option<SessionManager>,
    registry: Arc<ProviderRegistry>,
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
        cwd: Some(cwd.to_path_buf()),
        event_stream: None,
        extensions: None,
        tool_selection: None,
    }
}

type Recorded = Arc<Mutex<Vec<(String, Value)>>>;

/// Inline plugin recording every declared event payload.
struct Recorder {
    info: PluginInfo,
    events: Recorded,
    interest: EventInterest,
    contexts: Mutex<Vec<PluginContext>>,
}

impl Recorder {
    fn new(events: Recorded, interest: EventInterest) -> Arc<Self> {
        Arc::new(Self {
            info: PluginInfo::inline("hooks-w20-recorder"),
            events,
            interest,
            contexts: Mutex::new(Vec::new()),
        })
    }

    fn all() -> (Arc<Self>, Recorded) {
        let events: Recorded = Arc::new(Mutex::new(Vec::new()));
        (Self::new(Arc::clone(&events), EventInterest::All), events)
    }

    fn for_events(names: &[&str]) -> (Arc<Self>, Recorded) {
        let events: Recorded = Arc::new(Mutex::new(Vec::new()));
        (
            Self::new(
                Arc::clone(&events),
                EventInterest::List(names.iter().map(|name| (*name).to_owned()).collect()),
            ),
            events,
        )
    }
}

impl AgentPlugin for Recorder {
    fn info(&self) -> &PluginInfo {
        &self.info
    }

    fn events(&self) -> EventInterest {
        self.interest.clone()
    }

    fn on_event<'a>(&'a self, event: PluginEvent<'a>) -> PluginFuture<'a, PluginEventOutcome> {
        Box::pin(async move {
            self.events
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push((event.name.to_string(), event.payload.clone()));
            PluginEventOutcome::None
        })
    }

    fn on_context(&self, ctx: &PluginContext) {
        self.contexts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(ctx.clone());
    }
}

fn long_user(text: &str) -> Message {
    let body = format!("{text}-{}", "x".repeat(400));
    Message::User(UserMessage {
        role: UserRole::User,
        content: vec![text_block(&body)],
        tools_added: None,
        tools_removed: None,
        sections: None,
        timestamp: 1,
    })
}

fn read_tool_call(path: &str) -> ContentBlock {
    ContentBlock::ToolCall(ToolCall {
        id: "call-read".into(),
        name: "read".into(),
        arguments: BTreeMap::from([("path".to_owned(), json!(path))]),
        thought_signature: None,
        namespace: None,
        arguments_error: None,
    })
}

fn payloads(events: &Recorded, name: &str) -> Vec<Value> {
    events
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .iter()
        .filter(|(event, _)| event == name)
        .map(|(_, payload)| payload.clone())
        .collect()
}

// ---------------------------------------------------------------------------
// 1. session_before_compact.preparation
// ---------------------------------------------------------------------------

#[tokio::test]
async fn before_compact_preparation_carries_upstream_fields() {
    let temp = TempDir::new("preparation");
    let (recorder, events) = Recorder::for_events(&["session_before_compact"]);
    let registry = fake_registry(Arc::new(|_index, _context| assistant_text("SUMMARY")));

    let mut session = SessionManager::create(".", &temp.0).expect("create session");
    session
        .append_message(long_user("first"))
        .expect("append first");
    let mut assistant = assistant_text("calling read");
    assistant.content.push(read_tool_call("/tmp/read-a.txt"));
    session
        .append_message(Message::Assistant(assistant))
        .expect("append assistant");
    session
        .append_message(long_user("second"))
        .expect("append second");

    let agent = AgentSession::new_with_plugins(
        session_config(
            &temp.0,
            Some(session),
            registry,
            Some(pir::compaction::CompactionConfig {
                enabled: true,
                reserve_tokens: 1,
                keep_recent_tokens: 8,
            }),
        ),
        vec![Arc::clone(&recorder) as Arc<dyn AgentPlugin>],
    );
    agent.compact(None).await.expect("first compact");

    let first = payloads(&events, "session_before_compact")
        .pop()
        .expect("first preparation payload");
    let preparation = &first["preparation"];
    assert_eq!(
        preparation["turnPrefixMessages"],
        json!([]),
        "turnPrefixMessages present (empty without a split turn): {preparation}"
    );
    assert!(
        preparation.get("previousSummary").is_none(),
        "previousSummary omitted when no previous compaction exists: {preparation}"
    );
    assert_eq!(
        preparation["settings"],
        json!({ "enabled": true, "reserveTokens": 1, "keepRecentTokens": 8 }),
        "upstream CompactionSettings shape: {preparation}"
    );
    assert!(
        preparation["fileOps"]["read"]
            .as_array()
            .is_some_and(|paths| paths.contains(&json!("/tmp/read-a.txt"))),
        "fileOps carries the summarized span's read path: {preparation}"
    );
    assert_eq!(
        preparation["fileOps"]["written"],
        json!([]),
        "{preparation}"
    );
    assert_eq!(preparation["fileOps"]["edited"], json!([]), "{preparation}");

    // A second compaction (after enough new conversation) carries the previous
    // summary and the previous compaction's persisted file lists.
    agent
        .with_session_manager(|sm| {
            sm.append_message(long_user("third"))?;
            sm.append_message(Message::Assistant(assistant_text("third reply")))
        })
        .await
        .expect("append third")
        .expect("third entry");
    agent
        .with_session_manager(|sm| {
            sm.append_message(long_user("fourth"))?;
            sm.append_message(Message::Assistant(assistant_text("fourth reply")))
        })
        .await
        .expect("append fourth")
        .expect("fourth entry");
    agent.compact(None).await.expect("second compact");

    let second = payloads(&events, "session_before_compact")
        .pop()
        .expect("second preparation payload");
    let preparation = &second["preparation"];
    let previous_summary = preparation["previousSummary"]
        .as_str()
        .expect("previousSummary is a string");
    assert!(
        previous_summary.starts_with("SUMMARY"),
        "previousSummary is the prior compaction's summary: {preparation}"
    );
    assert!(
        preparation["fileOps"]["read"]
            .as_array()
            .is_some_and(|paths| paths.contains(&json!("/tmp/read-a.txt"))),
        "the previous compaction's readFiles carry into fileOps: {preparation}"
    );
}

// ---------------------------------------------------------------------------
// 2. session_compact_failed ordering
// ---------------------------------------------------------------------------

struct CancelCompactPlugin {
    info: PluginInfo,
    trace: Arc<Mutex<Vec<String>>>,
}

impl AgentPlugin for CancelCompactPlugin {
    fn info(&self) -> &PluginInfo {
        &self.info
    }

    fn events(&self) -> EventInterest {
        EventInterest::List(vec![
            "session_before_compact".to_owned(),
            "session_compact_failed".to_owned(),
        ])
    }

    fn on_event<'a>(&'a self, event: PluginEvent<'a>) -> PluginFuture<'a, PluginEventOutcome> {
        Box::pin(async move {
            self.trace
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(event.name.to_string());
            if event.name == "session_before_compact" {
                PluginEventOutcome::SessionBefore { cancel: true }
            } else {
                PluginEventOutcome::None
            }
        })
    }
}

#[tokio::test]
async fn manual_compact_failure_emits_end_before_failed() {
    let temp = TempDir::new("compact-order");
    let registry = fake_registry(Arc::new(|_index, _context| assistant_text("SUMMARY")));
    let trace: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let plugin = Arc::new(CancelCompactPlugin {
        info: PluginInfo::inline("cancel-compact"),
        trace: Arc::clone(&trace),
    });

    let mut session = SessionManager::create(".", &temp.0).expect("create session");
    session
        .append_message(long_user("first"))
        .expect("append first");
    session
        .append_message(Message::Assistant(assistant_text("first reply")))
        .expect("append reply");
    session
        .append_message(long_user("second"))
        .expect("append second");
    session
        .append_message(Message::Assistant(assistant_text("second reply")))
        .expect("append reply");

    let agent = AgentSession::new_with_plugins(
        session_config(
            &temp.0,
            Some(session),
            registry,
            Some(pir::compaction::CompactionConfig {
                enabled: true,
                reserve_tokens: 1,
                keep_recent_tokens: 8,
            }),
        ),
        vec![plugin as Arc<dyn AgentPlugin>],
    );
    let trace_events = Arc::clone(&trace);
    agent.on_session_event(Arc::new(move |event| match event {
        SessionEvent::CompactionStart { .. } => trace_events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push("compaction_start".to_owned()),
        SessionEvent::CompactionEnd { .. } => trace_events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push("compaction_end".to_owned()),
        _ => {}
    }));

    let error = agent.compact(None).await.expect_err("handler cancelled");
    assert!(error.to_string().contains("cancelled"), "{error}");

    assert_eq!(
        *trace.lock().unwrap(),
        vec![
            "compaction_start".to_owned(),
            "session_before_compact".to_owned(),
            "compaction_end".to_owned(),
            "session_compact_failed".to_owned(),
        ],
        "upstream catch order: compaction_end before session_compact_failed"
    );
}

// ---------------------------------------------------------------------------
// 2b. overflow recovery uses session_before_compact (cancel / replacement)
// ---------------------------------------------------------------------------

/// Records every compaction-hook payload and returns the configured decision:
/// `cancel` or an extension-provided replacement compaction.
struct OverflowCompactPlugin {
    info: PluginInfo,
    trace: Arc<Mutex<Vec<String>>>,
    payloads: Arc<Mutex<Vec<(String, Value)>>>,
    replacement: bool,
}

impl AgentPlugin for OverflowCompactPlugin {
    fn info(&self) -> &PluginInfo {
        &self.info
    }

    fn events(&self) -> EventInterest {
        EventInterest::List(vec![
            "session_before_compact".to_owned(),
            "session_compact".to_owned(),
            "session_compact_failed".to_owned(),
        ])
    }

    fn on_event<'a>(&'a self, event: PluginEvent<'a>) -> PluginFuture<'a, PluginEventOutcome> {
        Box::pin(async move {
            self.trace
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(event.name.to_string());
            self.payloads
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push((event.name.to_string(), event.payload.clone()));
            if event.name == "session_before_compact" {
                if self.replacement {
                    PluginEventOutcome::Generic(json!({
                        "compaction": {
                            "summary": "EXT SUMMARY",
                            "tokensBefore": 42,
                        }
                    }))
                } else {
                    PluginEventOutcome::SessionBefore { cancel: true }
                }
            } else {
                PluginEventOutcome::None
            }
        })
    }
}

/// Records `session_compact` / `session_compact_failed` names into a shared
/// trace (the same one a session-event subscriber appends to) and keeps every
/// failed payload.
struct CompactEventTracePlugin {
    info: PluginInfo,
    trace: Arc<Mutex<Vec<String>>>,
    failed: Arc<Mutex<Vec<Value>>>,
}

impl AgentPlugin for CompactEventTracePlugin {
    fn info(&self) -> &PluginInfo {
        &self.info
    }

    fn events(&self) -> EventInterest {
        EventInterest::List(vec![
            "session_compact".to_owned(),
            "session_compact_failed".to_owned(),
        ])
    }

    fn on_event<'a>(&'a self, event: PluginEvent<'a>) -> PluginFuture<'a, PluginEventOutcome> {
        Box::pin(async move {
            self.trace
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(event.name.to_string());
            if event.name == "session_compact_failed" {
                self.failed
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .push(event.payload.clone());
            }
            PluginEventOutcome::None
        })
    }
}

/// Seed a transcript large enough for `prepare_compaction_refs` to cut.
fn seed_overflow_transcript(session: &mut SessionManager) {
    for index in 0..4 {
        session
            .append_message(long_user(&format!("u{index}")))
            .expect("append user");
        session
            .append_message(Message::Assistant(assistant_text(&format!(
                "reply-{index}-{}",
                "y".repeat(400)
            ))))
            .expect("append assistant");
    }
}

fn overflow_config() -> pir::compaction::CompactionConfig {
    pir::compaction::CompactionConfig {
        enabled: true,
        reserve_tokens: 1,
        keep_recent_tokens: 20,
    }
}

/// The overflow recovery path dispatches `session_before_compact` with the
/// upstream overflow payload; a handler cancel emits `compaction_end`
/// (aborted) BEFORE `session_compact_failed`, and no summarizer request runs.
#[tokio::test]
async fn overflow_recovery_honors_before_compact_cancel() {
    let temp = TempDir::new("overflow-cancel");
    let calls = Arc::new(AtomicUsize::new(0));
    let calls_for_responder = Arc::clone(&calls);
    let registry = fake_registry(Arc::new(move |index, _context| {
        calls_for_responder.fetch_add(1, Ordering::SeqCst);
        match index {
            0 => {
                let mut message = assistant_text("");
                message.content.clear();
                message.stop_reason = StopReason::Error;
                message.error_message =
                    Some("prompt is too long: 200000 tokens > 16384 maximum".into());
                message
            }
            _ => assistant_text("unexpected retry"),
        }
    }));
    let trace: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let payloads: Arc<Mutex<Vec<(String, Value)>>> = Arc::new(Mutex::new(Vec::new()));
    let plugin = Arc::new(OverflowCompactPlugin {
        info: PluginInfo::inline("overflow-cancel"),
        trace: Arc::clone(&trace),
        payloads: Arc::clone(&payloads),
        replacement: false,
    });

    let mut session = SessionManager::create(".", &temp.0).expect("create session");
    seed_overflow_transcript(&mut session);
    let agent = AgentSession::new_with_plugins(
        session_config(&temp.0, Some(session), registry, Some(overflow_config())),
        vec![plugin as Arc<dyn AgentPlugin>],
    );
    let trace_events = Arc::clone(&trace);
    agent.on_session_event(Arc::new(move |event| match event {
        SessionEvent::CompactionStart { .. } => trace_events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push("compaction_start".to_owned()),
        SessionEvent::CompactionEnd { .. } => trace_events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push("compaction_end".to_owned()),
        _ => {}
    }));

    let terminal = agent
        .prompt("hi", Arc::new(|_event| {}))
        .await
        .expect("prompt settles with the overflow terminal");
    assert_eq!(terminal.stop_reason, StopReason::Error);
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "a cancelled recovery must not call the summarizer"
    );
    assert_eq!(
        *trace.lock().unwrap(),
        vec![
            "compaction_start".to_owned(),
            "session_before_compact".to_owned(),
            "compaction_end".to_owned(),
            "session_compact_failed".to_owned(),
        ],
        "upstream catch order: compaction_end before session_compact_failed"
    );

    let recorded = payloads.lock().unwrap();
    let before = recorded
        .iter()
        .find(|(name, _)| name == "session_before_compact")
        .map(|(_, payload)| payload)
        .expect("before compact payload");
    assert_eq!(before["reason"], json!("overflow"));
    assert_eq!(before["willRetry"], json!(true));
    assert!(
        before["preparation"]["firstKeptEntryId"].is_string()
            || before["preparation"]["firstKeptEntryId"].is_null()
    );
    assert!(
        before["branchEntries"].is_array(),
        "overflow carries the branch view: {before}"
    );
    let failed = recorded
        .iter()
        .find(|(name, _)| name == "session_compact_failed")
        .map(|(_, payload)| payload)
        .expect("failed payload");
    assert_eq!(failed["reason"], json!("overflow"));
    assert_eq!(failed["aborted"], json!(true));
    assert!(failed["errorMessage"].is_null(), "{failed}");
    assert_eq!(failed["fromExtension"], json!(false));
}

/// A `session_before_compact` replacement is used verbatim: no summarizer
/// call, `fromExtension: true` on `session_compact`, and the retry context is
/// rebuilt from the extension-provided summary.
#[tokio::test]
async fn overflow_recovery_uses_before_compact_replacement() {
    let temp = TempDir::new("overflow-replacement");
    let calls = Arc::new(AtomicUsize::new(0));
    let calls_for_responder = Arc::clone(&calls);
    let summary_seen = Arc::new(AtomicBool::new(false));
    let summary_seen_for_responder = Arc::clone(&summary_seen);
    let registry = fake_registry(Arc::new(move |index, context| {
        calls_for_responder.fetch_add(1, Ordering::SeqCst);
        match index {
            0 => {
                let mut message = assistant_text("");
                message.content.clear();
                message.stop_reason = StopReason::Error;
                message.error_message =
                    Some("prompt is too long: 200000 tokens > 16384 maximum".into());
                message
            }
            _ => {
                if context.messages.iter().any(|message| matches!(message,
                    Message::User(user)
                        if user.content.iter().any(|block| matches!(block,
                            ContentBlock::Text(text)
                                if text.text.starts_with(pir::compaction::prompts::COMPACTION_SUMMARY_PREFIX)
                                    && text.text.contains("EXT SUMMARY")
                        ))
                )) {
                    summary_seen_for_responder.store(true, Ordering::SeqCst);
                }
                assistant_text("recovered through extension compaction")
            }
        }
    }));
    let trace: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let payloads: Arc<Mutex<Vec<(String, Value)>>> = Arc::new(Mutex::new(Vec::new()));
    let plugin = Arc::new(OverflowCompactPlugin {
        info: PluginInfo::inline("overflow-replacement"),
        trace: Arc::clone(&trace),
        payloads: Arc::clone(&payloads),
        replacement: true,
    });

    let mut session = SessionManager::create(".", &temp.0).expect("create session");
    seed_overflow_transcript(&mut session);
    let agent = AgentSession::new_with_plugins(
        session_config(&temp.0, Some(session), registry, Some(overflow_config())),
        vec![plugin as Arc<dyn AgentPlugin>],
    );
    let trace_events = Arc::clone(&trace);
    agent.on_session_event(Arc::new(move |event| match event {
        SessionEvent::CompactionStart { .. } => trace_events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push("compaction_start".to_owned()),
        SessionEvent::CompactionEnd { .. } => trace_events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push("compaction_end".to_owned()),
        _ => {}
    }));

    let terminal = agent
        .prompt("hi", Arc::new(|_event| {}))
        .await
        .expect("prompt recovers through the replacement");
    assert!(
        summary_seen.load(Ordering::SeqCst),
        "the replacement summary must head the retry context"
    );
    assert!(
        terminal.content.iter().any(|block| matches!(block,
            ContentBlock::Text(text) if text.text == "recovered through extension compaction"
        )),
        "the retried turn's text: {:?}",
        terminal.content
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        2,
        "failed main request + retried main request; no summarizer call"
    );
    assert_eq!(
        *trace.lock().unwrap(),
        vec![
            "compaction_start".to_owned(),
            "session_before_compact".to_owned(),
            "session_compact".to_owned(),
            "compaction_end".to_owned(),
        ]
    );

    let recorded = payloads.lock().unwrap();
    let compact = recorded
        .iter()
        .find(|(name, _)| name == "session_compact")
        .map(|(_, payload)| payload)
        .expect("session_compact payload");
    assert_eq!(compact["reason"], json!("overflow"));
    assert_eq!(compact["fromExtension"], json!(true));
    assert_eq!(compact["compaction"]["summary"], json!("EXT SUMMARY"));
}

/// A THRESHOLD compaction whose session append fails must not rebuild the
/// context or invalidate the prompt-start snapshot: `compaction_end` (error
/// shape) fires before `session_compact_failed`, the model request continues
/// on the unchanged context, and the NEXT turn retries and persists the
/// compaction.
#[tokio::test]
async fn threshold_append_failure_keeps_context_and_retries_next_turn() {
    let temp = TempDir::new("threshold-append-failure");
    let trace: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let failed: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(Vec::new()));
    let plugin = Arc::new(CompactEventTracePlugin {
        info: PluginInfo::inline("threshold-append-failure"),
        trace: Arc::clone(&trace),
        failed: Arc::clone(&failed),
    });
    let saw_summary = Arc::new(AtomicBool::new(false));
    let saw_summary_for_responder = Arc::clone(&saw_summary);
    let registry = fake_registry(Arc::new(move |_index, context| {
        if context.system_prompt.as_deref()
            == Some(pir::compaction::prompts::SUMMARIZATION_SYSTEM_PROMPT)
        {
            return assistant_text("THRESHOLD SUMMARY");
        }
        if context.messages.iter().any(|message| matches!(message,
            Message::User(user)
                if user.content.iter().any(|block| matches!(block,
                    ContentBlock::Text(text)
                        if text.text.starts_with(pir::compaction::prompts::COMPACTION_SUMMARY_PREFIX)
                ))
        )) {
            saw_summary_for_responder.store(true, Ordering::SeqCst);
        }
        assistant_text("model reply")
    }));

    let mut session = SessionManager::create(".", &temp.0).expect("create session");
    seed_overflow_transcript(&mut session);
    // The prompt-start gate requires an assistant tail; end on a user message
    // so only the run's per-turn threshold hook triggers here.
    session
        .append_message(long_user("tail question"))
        .expect("append tail");

    let mut config = session_config(
        &temp.0,
        Some(session),
        registry,
        Some(pir::compaction::CompactionConfig {
            enabled: true,
            reserve_tokens: 1,
            keep_recent_tokens: 8,
        }),
    );
    config.model.context_window = 64;
    let agent = AgentSession::new_with_plugins(config, vec![plugin as Arc<dyn AgentPlugin>]);
    let trace_events = Arc::clone(&trace);
    agent.on_session_event(Arc::new(move |event| match event {
        SessionEvent::CompactionStart { .. } => trace_events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push("compaction_start".to_owned()),
        SessionEvent::CompactionEnd { .. } => trace_events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push("compaction_end".to_owned()),
        _ => {}
    }));
    agent
        .with_session_manager(|session| {
            session.fail_next_append_for_test(AppendFailure::BeforeWrite)
        })
        .await
        .expect("session storage attached");

    let terminal = agent
        .prompt("first", Arc::new(|_event| {}))
        .await
        .expect("the failed compaction must not abort the prompt");
    assert_eq!(terminal.stop_reason, StopReason::Stop);
    assert!(
        terminal.content.iter().any(|block| matches!(
            block,
            ContentBlock::Text(text) if text.text == "model reply"
        )),
        "the run continues on the unchanged context: {:?}",
        terminal.content
    );
    assert!(
        !saw_summary.load(Ordering::SeqCst),
        "a failed append must not hand the rebuilt (summary-headed) context to the request"
    );
    assert_eq!(
        *trace.lock().unwrap(),
        vec![
            "compaction_start".to_owned(),
            "compaction_end".to_owned(),
            "session_compact_failed".to_owned(),
        ],
        "upstream catch order: compaction_end before session_compact_failed"
    );
    {
        let recorded = failed.lock().unwrap();
        let payload = recorded.last().expect("session_compact_failed payload");
        assert_eq!(payload["reason"], json!("threshold"));
        assert_eq!(payload["aborted"], json!(false));
        assert_eq!(payload["willRetry"], json!(false));
        assert_eq!(payload["fromExtension"], json!(false));
        let message = payload["errorMessage"]
            .as_str()
            .expect("errorMessage is present");
        assert!(
            message.starts_with("Auto-compaction failed:"),
            "upstream threshold failure text: {message}"
        );
    }
    let persisted_compaction = agent
        .with_session_manager(|session| {
            session
                .entries()
                .iter()
                .any(|entry| matches!(entry, FileEntry::Compaction(_)))
        })
        .await
        .expect("session storage attached");
    assert!(
        !persisted_compaction,
        "a failed append must not persist a compaction entry"
    );

    // Next turn: the unchanged context is still over the threshold, so the
    // compaction is retried and now lands in storage.
    trace.lock().unwrap().clear();
    failed.lock().unwrap().clear();
    saw_summary.store(false, Ordering::SeqCst);
    let terminal = agent
        .prompt("second", Arc::new(|_event| {}))
        .await
        .expect("the retry prompt settles");
    assert_eq!(terminal.stop_reason, StopReason::Stop);
    assert!(
        saw_summary.load(Ordering::SeqCst),
        "the retried compaction's summary heads the next request"
    );
    assert_eq!(
        *trace.lock().unwrap(),
        vec![
            "compaction_start".to_owned(),
            "session_compact".to_owned(),
            "compaction_end".to_owned(),
        ],
        "the next turn retries the threshold compaction"
    );
    let summary = agent
        .with_session_manager(|session| {
            session.entries().into_iter().find_map(|entry| match entry {
                FileEntry::Compaction(inner) => Some(inner.summary),
                _ => None,
            })
        })
        .await
        .expect("session storage attached");
    assert_eq!(
        summary.as_deref(),
        Some("THRESHOLD SUMMARY"),
        "the retried compaction is persisted"
    );
}

/// An OVERFLOW recovery whose session append fails must emit the terminal
/// end-then-failed pair and let the prompt settle on its overflow terminal
/// (upstream `_runAutoCompaction` returns false from the catch) instead of
/// aborting the prompt with the storage error and leaving only the guard's
/// synthetic `compaction_end`.
#[tokio::test]
async fn overflow_append_failure_settles_on_the_overflow_terminal() {
    let temp = TempDir::new("overflow-append-failure");
    let calls = Arc::new(AtomicUsize::new(0));
    let calls_for_responder = Arc::clone(&calls);
    let armed = Arc::new(AtomicBool::new(false));
    let armed_for_responder = Arc::clone(&armed);
    // The sync stream responder arms the failure while the summarizer runs:
    // the transcript batch and the recovery-omission edits are already
    // persisted by then, so the NEXT append is the compaction entry.
    let failure_session: Arc<Mutex<Option<Arc<tokio::sync::Mutex<SessionManager>>>>> =
        Arc::new(Mutex::new(None));
    let failure_session_for_responder = Arc::clone(&failure_session);
    let registry = fake_registry(Arc::new(move |_index, context| {
        if context.system_prompt.as_deref()
            == Some(pir::compaction::prompts::SUMMARIZATION_SYSTEM_PROMPT)
        {
            let handle = failure_session_for_responder
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone()
                .expect("session handle installed before the prompt");
            let mut session = handle
                .try_lock()
                .expect("the session lock is free while the summarizer runs");
            session.fail_next_append_for_test(AppendFailure::BeforeWrite);
            armed_for_responder.store(true, Ordering::SeqCst);
            return assistant_text("OVERFLOW SUMMARY");
        }
        calls_for_responder.fetch_add(1, Ordering::SeqCst);
        overflow_error_assistant()
    }));
    let trace: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let failed: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(Vec::new()));
    let plugin = Arc::new(CompactEventTracePlugin {
        info: PluginInfo::inline("overflow-append-failure"),
        trace: Arc::clone(&trace),
        failed: Arc::clone(&failed),
    });

    let mut session = SessionManager::create(".", &temp.0).expect("create session");
    seed_overflow_transcript(&mut session);
    let agent = AgentSession::new_with_plugins(
        session_config(&temp.0, Some(session), registry, Some(overflow_config())),
        vec![plugin as Arc<dyn AgentPlugin>],
    );
    *failure_session
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = agent.session_manager();
    let trace_events = Arc::clone(&trace);
    agent.on_session_event(Arc::new(move |event| match event {
        SessionEvent::CompactionStart { .. } => trace_events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push("compaction_start".to_owned()),
        SessionEvent::CompactionEnd { .. } => trace_events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push("compaction_end".to_owned()),
        _ => {}
    }));

    let terminal = agent
        .prompt("hi", Arc::new(|_event| {}))
        .await
        .expect("the prompt settles on the overflow terminal");
    assert!(
        armed.load(Ordering::SeqCst),
        "the summarizer must have armed the compaction-append failure"
    );
    assert_eq!(terminal.stop_reason, StopReason::Error);
    assert!(
        terminal
            .error_message
            .as_deref()
            .is_some_and(|message| message.contains("prompt is too long")),
        "the original overflow terminal is returned: {terminal:?}"
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "a failed recovery append must not retry the model request"
    );
    assert_eq!(
        *trace.lock().unwrap(),
        vec![
            "compaction_start".to_owned(),
            "compaction_end".to_owned(),
            "session_compact_failed".to_owned(),
        ],
        "upstream catch order: compaction_end before session_compact_failed"
    );
    {
        let recorded = failed.lock().unwrap();
        let payload = recorded.last().expect("session_compact_failed payload");
        assert_eq!(payload["reason"], json!("overflow"));
        assert_eq!(payload["aborted"], json!(false));
        assert_eq!(payload["willRetry"], json!(false));
        assert_eq!(payload["fromExtension"], json!(false));
        let message = payload["errorMessage"]
            .as_str()
            .expect("errorMessage is present");
        assert!(
            message.starts_with("Context overflow recovery failed:"),
            "upstream overflow failure text: {message}"
        );
    }
    let entries = agent
        .with_session_manager(|session| session.entries())
        .await
        .expect("session storage attached");
    assert!(
        !entries
            .iter()
            .any(|entry| matches!(entry, FileEntry::Compaction(_))),
        "a failed append must not persist a compaction entry"
    );
    assert!(
        entries
            .iter()
            .any(|entry| matches!(entry, FileEntry::ContextEdit(_))),
        "the recovery omission stays persisted (the failed attempt is not model-visible)"
    );
}

/// The prompt-start compaction gate reads the PROJECTION tail, not the raw
/// file tail: a user message omitted by a `context_edit` must expose the
/// assistant before it, so the threshold compaction still runs BEFORE
/// `before_agent_start`.
#[tokio::test]
async fn context_edit_omission_keeps_the_prompt_start_compaction_gate() {
    let temp = TempDir::new("prompt-start-edit-omission");
    let trace: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let recorder = Arc::new(OverflowTrailingPlugin {
        info: PluginInfo::inline("prompt-start-edit-omission"),
        trace: Arc::clone(&trace),
    });
    let registry = fake_registry(Arc::new(|_index, context| {
        if context.system_prompt.as_deref()
            == Some(pir::compaction::prompts::SUMMARIZATION_SYSTEM_PROMPT)
        {
            return assistant_text("PRE SUMMARY");
        }
        assistant_text("model reply")
    }));

    let mut session = SessionManager::create(".", &temp.0).expect("create session");
    seed_overflow_transcript(&mut session);
    // The raw file tail is a user message that the projection omits; the
    // projected tail is the assistant before it.
    let omitted = session
        .append_message(long_user("omitted tail question"))
        .expect("append tail");
    session
        .append_context_edit(&omitted, None)
        .expect("omit tail");
    let mut config = session_config(
        &temp.0,
        Some(session),
        registry,
        Some(pir::compaction::CompactionConfig {
            enabled: true,
            reserve_tokens: 10,
            keep_recent_tokens: 20,
        }),
    );
    config.model.context_window = 500;
    let agent = AgentSession::new_with_plugins(config, vec![recorder as Arc<dyn AgentPlugin>]);

    let terminal = agent
        .prompt("hi", Arc::new(|_event| {}))
        .await
        .expect("prompt");
    assert_eq!(terminal.stop_reason, StopReason::Stop);
    assert_eq!(
        *trace.lock().unwrap(),
        vec![
            "session_before_compact".to_owned(),
            "before_agent_start".to_owned(),
        ],
        "the prompt-start compaction must still precede before_agent_start"
    );
}

// ---------------------------------------------------------------------------
// 2c. first compaction runs before before_agent_start (upstream `_prepareRun`)
// ---------------------------------------------------------------------------

/// A resumed transcript already over the threshold compacts BEFORE
/// `before_agent_start` (upstream `_prepareRun`, agent-session.ts:1926-1935):
/// the extension observes `session_before_compact` first, and the run's first
/// request already carries the compaction summary.
#[tokio::test]
async fn first_threshold_compaction_precedes_before_agent_start() {
    let temp = TempDir::new("pre-run-compaction");
    let trace: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let recorder = Arc::new(OverflowTrailingPlugin {
        info: PluginInfo::inline("pre-run-order"),
        trace: Arc::clone(&trace),
    });
    let summary_seen = Arc::new(AtomicBool::new(false));
    let summary_seen_for_responder = Arc::clone(&summary_seen);
    let registry = fake_registry(Arc::new(move |_index, context| {
        if context.system_prompt.as_deref()
            == Some(pir::compaction::prompts::SUMMARIZATION_SYSTEM_PROMPT)
        {
            return assistant_text("PRE SUMMARY");
        }
        if context.messages.iter().any(|message| matches!(message,
            Message::User(user)
                if user.content.iter().any(|block| matches!(block,
                    ContentBlock::Text(text)
                        if text.text.starts_with(pir::compaction::prompts::COMPACTION_SUMMARY_PREFIX)
                            && text.text.contains("PRE SUMMARY")
                ))
        )) {
            summary_seen_for_responder.store(true, Ordering::SeqCst);
        }
        assistant_text("first reply after pre-run compaction")
    }));

    let mut session = SessionManager::create(".", &temp.0).expect("create session");
    seed_overflow_transcript(&mut session);
    let mut config = session_config(
        &temp.0,
        Some(session),
        registry,
        Some(pir::compaction::CompactionConfig {
            enabled: true,
            reserve_tokens: 10,
            keep_recent_tokens: 20,
        }),
    );
    // Small window so the seeded transcript is over the threshold.
    config.model.context_window = 500;
    let agent = AgentSession::new_with_plugins(config, vec![recorder as Arc<dyn AgentPlugin>]);
    let terminal = agent
        .prompt("hi", Arc::new(|_event| {}))
        .await
        .expect("prompt");
    assert_eq!(terminal.stop_reason, StopReason::Stop);
    assert!(
        summary_seen.load(Ordering::SeqCst),
        "the first request must already carry the pre-run summary"
    );

    assert_eq!(
        *trace.lock().unwrap(),
        vec![
            "session_before_compact".to_owned(),
            "before_agent_start".to_owned(),
        ],
        "the first threshold compaction must precede before_agent_start"
    );
}

/// Records `session_before_compact` / `before_agent_start` order only.
struct OverflowTrailingPlugin {
    info: PluginInfo,
    trace: Arc<Mutex<Vec<String>>>,
}

impl AgentPlugin for OverflowTrailingPlugin {
    fn info(&self) -> &PluginInfo {
        &self.info
    }

    fn events(&self) -> EventInterest {
        EventInterest::List(vec![
            "session_before_compact".to_owned(),
            "before_agent_start".to_owned(),
        ])
    }

    fn on_event<'a>(&'a self, event: PluginEvent<'a>) -> PluginFuture<'a, PluginEventOutcome> {
        Box::pin(async move {
            self.trace
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(event.name.to_string());
            PluginEventOutcome::None
        })
    }
}

// ---------------------------------------------------------------------------
// 3. resources_discover.cwd
// ---------------------------------------------------------------------------

#[tokio::test]
async fn resources_discover_carries_cwd() {
    let temp = TempDir::new("resources-cwd");
    let (recorder, events) = Recorder::for_events(&["resources_discover"]);
    let agent = AgentSession::new_with_plugins(
        session_config(&temp.0, None, ok_registry(), None),
        vec![recorder as Arc<dyn AgentPlugin>],
    );

    agent
        .prompt("hello", Arc::new(|_| {}))
        .await
        .expect("prompt");

    let payload = payloads(&events, "resources_discover")
        .pop()
        .expect("resources_discover dispatched");
    assert_eq!(payload["reason"], json!("startup"), "{payload}");
    assert_eq!(
        payload["cwd"],
        json!(temp.0.display().to_string()),
        "upstream ResourcesDiscoverEvent.cwd: {payload}"
    );
}

// ---------------------------------------------------------------------------
// 4. session_before_switch / session_shutdown targetSessionFile
// ---------------------------------------------------------------------------

#[tokio::test]
async fn switch_payloads_carry_target_session_file() {
    let temp = TempDir::new("switch-target");
    let (recorder, events) = Recorder::all();
    let current = SessionManager::create(".", &temp.0).expect("current session");
    let target = SessionManager::create(".", &temp.0).expect("target session");
    let target_file = target.path().display().to_string();
    let agent = AgentSession::new_with_plugins(
        session_config(&temp.0, Some(current), ok_registry(), None),
        vec![recorder as Arc<dyn AgentPlugin>],
    );

    assert!(
        agent.replace_session_storage(target).await,
        "the switch completes"
    );

    let before_switch = payloads(&events, "session_before_switch")
        .pop()
        .expect("session_before_switch dispatched");
    assert_eq!(
        before_switch["targetSessionFile"],
        json!(target_file),
        "upstream SessionBeforeSwitchEvent.targetSessionFile: {before_switch}"
    );

    let shutdown = payloads(&events, "session_shutdown")
        .pop()
        .expect("session_shutdown dispatched on replacement");
    assert_eq!(shutdown["reason"], json!("switch"), "{shutdown}");
    assert_eq!(
        shutdown["targetSessionFile"],
        json!(target_file),
        "upstream SessionShutdownEvent.targetSessionFile: {shutdown}"
    );
}

// ---------------------------------------------------------------------------
// 5. session_before_fork skipConversationRestore
// ---------------------------------------------------------------------------

struct SkipRestorePlugin {
    info: PluginInfo,
}

impl AgentPlugin for SkipRestorePlugin {
    fn info(&self) -> &PluginInfo {
        &self.info
    }

    fn events(&self) -> EventInterest {
        EventInterest::List(vec!["session_before_fork".to_owned()])
    }

    fn on_event<'a>(&'a self, _event: PluginEvent<'a>) -> PluginFuture<'a, PluginEventOutcome> {
        Box::pin(async { PluginEventOutcome::Generic(json!({ "skipConversationRestore": true })) })
    }
}

#[tokio::test]
async fn fork_skip_conversation_restore_starts_an_empty_session() {
    let temp = TempDir::new("fork-skip");
    let mut session = SessionManager::create(".", &temp.0).expect("create session");
    let original_file = session.path().to_path_buf();
    session
        .append_message(long_user("first"))
        .expect("append first");
    let second_id = session
        .append_message(long_user("second"))
        .expect("append second");
    let agent = AgentSession::new_with_plugins(
        session_config(&temp.0, Some(session), ok_registry(), None),
        vec![Arc::new(SkipRestorePlugin {
            info: PluginInfo::inline("skip-restore"),
        }) as Arc<dyn AgentPlugin>],
    );

    let outcome = agent
        .fork_entry(&second_id, ForkPosition::Before)
        .await
        .expect("fork");
    assert_eq!(
        outcome,
        ForkEntryOutcome::Branched {
            selected_text: Some(format!("second-{}", "x".repeat(400))),
        }
    );

    let (new_file, has_parent, empty) = agent
        .with_session_manager(|sm| {
            (
                sm.path().to_path_buf(),
                sm.parent_session().is_some(),
                sm.build_context_entries().unwrap_or_default().is_empty(),
            )
        })
        .await
        .expect("storage");
    assert_ne!(
        new_file, original_file,
        "the skip starts a fresh session file"
    );
    assert!(has_parent, "the fresh session records the parent link");
    assert!(
        empty,
        "skipConversationRestore restores no conversation messages"
    );
}

// ---------------------------------------------------------------------------
// 6. before_provider_request in-place mutation
// ---------------------------------------------------------------------------

struct MutatePayloadPlugin {
    info: PluginInfo,
}

impl AgentPlugin for MutatePayloadPlugin {
    fn info(&self) -> &PluginInfo {
        &self.info
    }

    fn events(&self) -> EventInterest {
        EventInterest::List(vec!["before_provider_request".to_owned()])
    }

    fn on_event<'a>(&'a self, event: PluginEvent<'a>) -> PluginFuture<'a, PluginEventOutcome> {
        Box::pin(async move {
            // The upstream contract mutates the event's payload in place and
            // returns nothing.
            if let Some(body) = event.payload.get_mut("payload") {
                body["inPlace"] = json!(true);
            }
            PluginEventOutcome::None
        })
    }
}

struct ReplacePayloadPlugin {
    info: PluginInfo,
}

impl AgentPlugin for ReplacePayloadPlugin {
    fn info(&self) -> &PluginInfo {
        &self.info
    }

    fn events(&self) -> EventInterest {
        EventInterest::List(vec!["before_provider_request".to_owned()])
    }

    fn on_event<'a>(&'a self, _event: PluginEvent<'a>) -> PluginFuture<'a, PluginEventOutcome> {
        Box::pin(async { PluginEventOutcome::Generic(json!({ "replaced": true })) })
    }
}

#[tokio::test]
async fn before_provider_request_honors_in_place_mutation() {
    let temp = TempDir::new("in-place");
    let agent = AgentSession::new_with_plugins(
        session_config(&temp.0, None, ok_registry(), None),
        vec![Arc::new(MutatePayloadPlugin {
            info: PluginInfo::inline("mutate-payload"),
        }) as Arc<dyn AgentPlugin>],
    );
    let hooks = agent.provider_stream_hooks_for_test();
    let hook = hooks
        .before_provider_request
        .expect("the handler installs the transport hook");
    let returned = hook(json!({ "orig": 1 })).await;
    assert_eq!(
        returned,
        json!({ "orig": 1, "inPlace": true }),
        "an in-place payload mutation must reach the provider request"
    );

    // A returned replacement still wins (upstream chains the next handler on
    // the replacement).
    let agent = AgentSession::new_with_plugins(
        session_config(&temp.0, None, ok_registry(), None),
        vec![Arc::new(ReplacePayloadPlugin {
            info: PluginInfo::inline("replace-payload"),
        }) as Arc<dyn AgentPlugin>],
    );
    let hook = agent
        .provider_stream_hooks_for_test()
        .before_provider_request
        .expect("the handler installs the transport hook");
    assert_eq!(
        hook(json!({ "orig": 1 })).await,
        json!({ "replaced": true })
    );
}

#[cfg(feature = "extensions")]
#[tokio::test(flavor = "current_thread")]
async fn before_provider_request_honors_js_in_place_mutation() {
    let temp = TempDir::new("js-in-place");
    let path = temp.0.join("mutate.ts");
    std::fs::write(
        &path,
        r#"
        export default function (pi) {
            pi.on("before_provider_request", (event) => {
                // Upstream mutation contract: mutate the event payload in
                // place and return nothing.
                event.payload.fromJs = true;
            });
        }
        "#,
    )
    .expect("write extension");
    let handle = ExtensionHandle::start(extension_config(&temp.0)).expect("start extensions");
    let loaded = handle
        .load(std::slice::from_ref(&path))
        .await
        .expect("load");
    assert!(loaded.errors.is_empty(), "{:?}", loaded.errors);

    let mut config = session_config(&temp.0, None, ok_registry(), None);
    config.extensions = Some(handle);
    let agent = AgentSession::new_with_plugins(config, Vec::new());
    let hook = agent
        .provider_stream_hooks_for_test()
        .before_provider_request
        .expect("the JS handler installs the transport hook");
    assert_eq!(
        hook(json!({ "orig": 1 })).await,
        json!({ "orig": 1, "fromJs": true }),
        "a JS handler's in-place payload mutation must reach the provider request"
    );
}

// ---------------------------------------------------------------------------
// 7. nested native ui_prompt_start / ui_prompt_end
// ---------------------------------------------------------------------------

fn extension_config(cwd: &std::path::Path) -> ExtensionConfig {
    ExtensionConfig {
        cwd: cwd.to_path_buf(),
        mode: "interactive".to_owned(),
        has_ui: true,
        model: Some("w20-model".to_owned()),
        session_id: Some("hooks-w20".to_owned()),
        system_prompt: String::new(),
        is_idle: true,
        limits: Some(Default::default()),
    }
}

fn dialog_requests(handle: &ExtensionHandle) -> Vec<u64> {
    handle
        .take_notifications(|notification| {
            matches!(
                notification,
                pir::extensions::Notification::DialogRequest { .. }
            )
        })
        .into_iter()
        .filter_map(|notification| match notification {
            pir::extensions::Notification::DialogRequest { id, .. } => Some(id),
            _ => None,
        })
        .collect()
}

fn poll_once<F: std::future::Future + Unpin>(future: &mut F) -> std::task::Poll<F::Output> {
    let waker = std::task::Waker::noop();
    let mut cx = std::task::Context::from_waker(waker);
    std::pin::Pin::new(future).poll(&mut cx)
}

#[tokio::test(flavor = "current_thread")]
async fn nested_native_dialogs_emit_one_prompt_pair() {
    let temp = TempDir::new("nested-dialog");
    let handle = ExtensionHandle::start_plan_only(extension_config(&temp.0));
    let (recorder, events) = Recorder::all();
    let agent = AgentSession::new_arc_with_plugins(
        {
            let mut config = session_config(&temp.0, None, ok_registry(), None);
            config.extensions = Some(handle.clone());
            config
        },
        vec![recorder as Arc<dyn AgentPlugin>],
    );
    let ui = {
        let recorder = agent_plugin_context(&agent);
        recorder.ui_arc().expect("ui service is bound")
    };

    let mut outer = Box::pin(ui.select(
        "Outer pick",
        &[SelectOption::new("One", "a")],
        DialogOptions::default(),
    ));
    assert!(poll_once(&mut outer).is_pending(), "outer dialog parks");
    let mut inner = Box::pin(ui.confirm("Inner confirm", "sure?", DialogOptions::default()));
    assert!(poll_once(&mut inner).is_pending(), "inner dialog parks");

    let requests = dialog_requests(&handle);
    assert_eq!(requests.len(), 2, "two dialog requests: {requests:?}");
    assert!(agent
        .plugin_host()
        .expect("host")
        .native_dialogs()
        .respond(requests[1], json!(true)));
    inner.await.expect("inner dialog resolves");
    assert_eq!(
        payloads(&events, "ui_prompt_start").len(),
        1,
        "the nested prompt adds no second start"
    );
    assert!(
        payloads(&events, "ui_prompt_end").is_empty(),
        "the outer prompt is still active: no end yet"
    );

    assert!(agent
        .plugin_host()
        .expect("host")
        .native_dialogs()
        .respond(requests[0], json!("a")));
    outer.await.expect("outer dialog resolves");
    let starts = payloads(&events, "ui_prompt_start");
    let ends = payloads(&events, "ui_prompt_end");
    assert_eq!(starts.len(), 1, "exactly one start: {starts:?}");
    assert_eq!(ends.len(), 1, "exactly one end: {ends:?}");
    assert_eq!(starts[0]["kind"], json!("select"), "{starts:?}");
    assert_eq!(starts[0]["title"], json!("Outer pick"), "{starts:?}");
    assert_eq!(
        ends[0]["kind"],
        json!("select"),
        "the end carries the OUTER dialog's identity: {ends:?}"
    );
    assert_eq!(ends[0]["title"], json!("Outer pick"), "{ends:?}");
}

fn agent_plugin_context(agent: &Arc<AgentSession>) -> PluginContext {
    // The inline recorder stored every bound context; find it through the
    // plugin host's bound context (the last bind is the live one).
    agent
        .plugin_host()
        .expect("plugin host")
        .context()
        .expect("bound context")
}

// ---------------------------------------------------------------------------
// 8. session_start {reason: "reload"}
// ---------------------------------------------------------------------------

#[tokio::test]
async fn reload_emits_session_start_on_the_native_face() {
    let temp = TempDir::new("reload-start");
    let (recorder, events) = Recorder::for_events(&["session_start", "session_shutdown"]);
    let agent = AgentSession::new_with_plugins(
        session_config(&temp.0, None, ok_registry(), None),
        vec![recorder as Arc<dyn AgentPlugin>],
    );

    agent.reload().await.expect("reload");

    let names: Vec<String> = events
        .lock()
        .unwrap()
        .iter()
        .map(|(name, _)| name.clone())
        .collect();
    assert_eq!(
        names,
        vec!["session_shutdown".to_owned(), "session_start".to_owned()],
        "reload lifecycle: shutdown then one session_start"
    );
    let start = payloads(&events, "session_start")
        .pop()
        .expect("session_start dispatched");
    assert_eq!(
        start["reason"],
        json!("reload"),
        "upstream reload emits session_start {{reason: \"reload\"}}: {start}"
    );

    // The lazy startup block must not deliver a second session_start later.
    agent
        .prompt("hello", Arc::new(|_| {}))
        .await
        .expect("prompt");
    assert_eq!(
        events
            .lock()
            .unwrap()
            .iter()
            .filter(|(name, _)| name == "session_start")
            .count(),
        1,
        "a reload consumes the lazy startup session_start"
    );
    let _ = Duration::from_millis(1);
}

// ---------------------------------------------------------------------------
// 9. session_start reasons on session replacement (new / resume / fork)
// ---------------------------------------------------------------------------

/// A persisted session with one user message, so the file exists (the RPC/SDK
/// reason classification keys on an existing file + the outgoing `parentSession`
/// link).
fn stored_session(cwd: &str, session_dir: &std::path::Path, seed: &str) -> SessionManager {
    let mut session = SessionManager::create(cwd, session_dir).expect("create session");
    session
        .append_message(long_user(seed))
        .expect("seed session file");
    session
}

#[tokio::test]
async fn session_start_reasons_for_new_resume_and_fork_without_a_duplicate_startup() {
    let temp = TempDir::new("start-reasons");
    let (recorder, events) = Recorder::for_events(&["session_start"]);
    let initial = stored_session(".", &temp.0, "initial");
    let initial_path = initial.path().to_path_buf();
    let agent = AgentSession::new_with_plugins(
        session_config(&temp.0, Some(initial), ok_registry(), None),
        vec![recorder as Arc<dyn AgentPlugin>],
    );

    // A fresh manager (its file is created lazily, so it does not exist yet)
    // is upstream `newSession` => reason "new".
    let fresh = SessionManager::create(".", &temp.0).expect("fresh session");
    let fresh_path = fresh.path().to_path_buf();
    assert!(agent.replace_session_storage(fresh).await);
    let starts = payloads(&events, "session_start");
    assert_eq!(
        starts.len(),
        1,
        "one session_start for the new swap: {starts:?}"
    );
    assert_eq!(starts[0]["reason"], json!("new"), "{starts:?}");
    assert_eq!(
        starts[0]["previousSessionFile"],
        json!(initial_path.display().to_string()),
        "upstream SessionStartEvent.previousSessionFile: {starts:?}"
    );

    // An existing file is upstream `switchSession` => reason "resume".
    let opened = stored_session(".", &temp.0, "resume target");
    let opened_path = opened.path().to_path_buf();
    assert!(agent.replace_session_storage(opened).await);
    let starts = payloads(&events, "session_start");
    assert_eq!(starts.len(), 2, "one more session_start: {starts:?}");
    assert_eq!(starts[1]["reason"], json!("resume"), "{starts:?}");
    assert_eq!(
        starts[1]["previousSessionFile"],
        json!(fresh_path.display().to_string()),
        "{starts:?}"
    );

    // A fork whose header links the OUTGOING file is upstream `fork` =>
    // reason "fork".
    let forked = SessionManager::fork_from_source(&opened_path, ".", &temp.0, None)
        .expect("fork the resumed session");
    assert!(agent.replace_session_storage(forked).await);
    let starts = payloads(&events, "session_start");
    assert_eq!(starts.len(), 3, "one more session_start: {starts:?}");
    assert_eq!(starts[2]["reason"], json!("fork"), "{starts:?}");
    assert_eq!(
        starts[2]["previousSessionFile"],
        json!(opened_path.display().to_string()),
        "{starts:?}"
    );

    // The lazy startup dispatch must NOT deliver a second session_start: the
    // replacement already consumed the lifecycle.
    agent
        .prompt("after the fork", Arc::new(|_| {}))
        .await
        .expect("prompt");
    assert_eq!(
        payloads(&events, "session_start").len(),
        3,
        "a replacement consumes the lazy startup session_start: {:?}",
        payloads(&events, "session_start")
    );
}

// ---------------------------------------------------------------------------
// 10. mcp_servers_change: one event per registry change
// ---------------------------------------------------------------------------

#[cfg(feature = "mcp")]
#[tokio::test]
async fn mcp_servers_change_emits_once_per_registry_change() {
    use pir::plugins::builtin::McpPlugin;

    let temp = TempDir::new("mcp-per-change");
    // Keep the global mcp.json away from the fixture: two extension
    // registrations at host build must be the only changes.
    let _agent_dir = crate::common::AgentDirEnvGuard::scratch("hooks-w20-mcp", None).await;
    let extension_path = temp.0.join("mcp_two_servers.ts");
    std::fs::write(
        &extension_path,
        r#"
        export default function (pi) {
            pi.registerMcpServer("per-change-a", {
                command: "definitely-not-a-real-command-11111",
                enabled: false,
            });
            pi.registerMcpServer("per-change-b", {
                command: "definitely-not-a-real-command-22222",
                enabled: false,
            });
        }
        "#,
    )
    .expect("write extension fixture");
    let Ok(handle) = ExtensionHandle::start(ExtensionConfig {
        cwd: temp.0.clone(),
        ..Default::default()
    }) else {
        return; // build without the extensions feature
    };
    let loaded = handle
        .load(std::slice::from_ref(&extension_path))
        .await
        .expect("load extension");
    assert!(loaded.errors.is_empty(), "{:?}", loaded.errors);

    let (recorder, events) = Recorder::for_events(&["mcp_servers_change"]);
    let host = PluginHost::build(
        Some(handle.clone()),
        vec![
            Arc::new(McpPlugin::with_cwd(&temp.0, &temp.0)) as Arc<dyn AgentPlugin>,
            recorder as Arc<dyn AgentPlugin>,
        ],
    );

    // The two registrations queued at host build each flush as their own
    // event; the old coalescing emitted one.
    let mut payload = json!({ "type": "session_start", "reason": "startup" });
    host.emit("session_start", &mut payload)
        .await
        .expect("emit session_start");
    let changes = payloads(&events, "mcp_servers_change");
    assert_eq!(
        changes.len(),
        2,
        "one event per queued registration: {changes:?}"
    );
    for change in &changes {
        assert_eq!(change["type"], json!("mcp_servers_change"));
        let servers = change["servers"].as_array().expect("servers array");
        let mut names: Vec<&str> = servers
            .iter()
            .filter_map(|server| server["name"].as_str())
            .collect();
        names.sort_unstable();
        assert_eq!(
            names,
            vec!["per-change-a", "per-change-b"],
            "each event carries the current full list: {change}"
        );
    }

    // Removing one server queues one more change and delivers exactly one
    // more event carrying the post-removal list.
    let remove_path = temp.0.join("mcp_remove_one.ts");
    std::fs::write(
        &remove_path,
        r#"
        export default function (pi) {
            pi.unregisterMcpServer("per-change-a");
        }
        "#,
    )
    .expect("write remove fixture");
    let loaded = handle
        .load(std::slice::from_ref(&remove_path))
        .await
        .expect("load remove extension");
    assert!(loaded.errors.is_empty(), "{:?}", loaded.errors);
    let mut payload = json!({ "type": "turn_start" });
    host.emit("turn_start", &mut payload)
        .await
        .expect("emit turn_start");
    let changes = payloads(&events, "mcp_servers_change");
    assert_eq!(
        changes.len(),
        3,
        "the unregister is its own event, not coalesced: {changes:?}"
    );
    let servers = changes[2]["servers"].as_array().expect("servers array");
    let names: Vec<&str> = servers
        .iter()
        .filter_map(|server| server["name"].as_str())
        .collect();
    assert_eq!(
        names,
        vec!["per-change-b"],
        "the removed server is gone from the payload: {}",
        changes[2]
    );
    host.shutdown().await;
}
