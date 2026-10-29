//! Round-2 parity repairs over the session-level event surface and the fork
//! seam (upstream `AgentSessionEvent` additions and
//! `agent-session-runtime.ts` fork):
//!
//! - `queue_update` / `compaction_start` / `compaction_end` /
//!   `bash_execution_update` session events (agent-session.ts:196-237,
//!   1047, 2767/2878/2891/3119/3205/3862) with their upstream camelCase wire
//!   shapes, emitted from the manual/threshold/overflow compaction runs, the
//!   queue mutation sites, and the streaming bash execution;
//! - the `isCompacting` flag (rpc `get_state`, rpc-mode.ts:453);
//! - `session_before_fork` (cancel aborts the fork,
//!   agent-session-runtime.ts:150-162) and `fork(entryId, {position})`
//!   before/at semantics with `selectedText` (agent-session-runtime.ts:
//!   263-346);
//! - the `provider_stream_event` extension dispatch (sdk.ts:396-409).

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

use pir::agent::tool_batch::ToolResult;
use pir::agent::tool_execution::{AgentTool, BoxFuture, ToolContext, ToolRegistry};
use pir::agent_session::{
    AgentSession, AgentSessionConfig, ForkEntryOutcome, ForkPosition, SessionEvent,
};
use pir::ai::auth::ApiKeyAuth;
use pir::ai::event_stream::AssistantMessageEventStream;
use pir::ai::provider::{Provider, ProviderRegistry};
use pir::ai::types::*;
use pir::coding_agent::rpc::{RpcServer, SessionFactory};
use pir::plugins::{
    AgentPlugin, EventInterest, PluginEvent, PluginEventOutcome, PluginFuture, PluginInfo,
};
use pir::session_manager::{FileEntry, SessionManager};

// ---------------------------------------------------------------------------
// Fakes (same pattern as tests/suites/agent_session.rs)
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

/// Provider whose stream exercises the parsed-event hook seam: two raw
/// provider events flow through `options.hooks.on_provider_stream_event`
/// before the terminal message (upstream adapters call the same hook per
/// parsed provider chunk).
struct HookProbeProvider {
    calls: AtomicUsize,
    respond: Responder,
}

impl Provider for HookProbeProvider {
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
        let index = self.calls.fetch_add(1, Ordering::SeqCst);
        if let Some(hook) = &options.hooks.on_provider_stream_event {
            hook(&serde_json::json!({ "type": "message_start", "fake": true }));
            hook(&serde_json::json!({ "type": "text_delta", "fake": index }));
        }
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

/// Provider whose stream fires the transport retry observers exactly the way
/// `request_streaming_with` does for a retried request (one scheduled retry,
/// then success): the session's `auto_retry_start` / `auto_retry_end` wiring
/// is exercised without an HTTP server.
struct RetryProbeProvider {
    calls: AtomicUsize,
    respond: Responder,
}

impl Provider for RetryProbeProvider {
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
        let index = self.calls.fetch_add(1, Ordering::SeqCst);
        if index == 0 {
            // The transport's `auto_retry_start` (`on_retry_start`) fires
            // before the wait and its `auto_retry_end` (`on_retry_end`) once
            // the retried request resolved.
            if let Some(hook) = &options.hooks.on_retry_start {
                hook(&pir::ai::types::AutoRetryStart {
                    attempt: 1,
                    max_attempts: 2,
                    delay_ms: 1,
                    error_message: "HTTP 429".into(),
                });
            }
            if let Some(hook) = &options.hooks.on_retry_end {
                hook(&pir::ai::types::AutoRetryEnd {
                    success: true,
                    attempt: 1,
                    final_error: None,
                });
            }
        }
        let message = (self.respond)(index, &context);
        let stream = AssistantMessageEventStream::new();
        stream.push(AssistantMessageEventStream::done_event(
            TerminalReason::Stop,
            message,
        ));
        stream
    }
}

fn retry_probe_registry(respond: Responder) -> Arc<ProviderRegistry> {
    let mut registry = ProviderRegistry::default();
    registry.set_provider(Arc::new(RetryProbeProvider {
        calls: AtomicUsize::new(0),
        respond,
    }));
    Arc::new(registry)
}

/// Provider whose stream can end WITHOUT a terminal message, so the
/// summarizer's completion closure returns the "stream did not terminate"
/// error the retry wrapper reacts to.
type RespondFn = Arc<dyn Fn(usize, &Context) -> Option<AssistantMessage> + Send + Sync>;

struct MaybeProvider {
    calls: AtomicUsize,
    respond: RespondFn,
}

impl Provider for MaybeProvider {
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
        match message {
            Some(message) => stream.push(AssistantMessageEventStream::done_event(
                TerminalReason::Stop,
                message,
            )),
            // No terminal event: `drain_and_result` yields `None`, which the
            // session's completion closure reports as an error.
            None => stream.end(None),
        }
        stream
    }
}

fn maybe_registry(respond: RespondFn) -> Arc<ProviderRegistry> {
    let mut registry = ProviderRegistry::default();
    registry.set_provider(Arc::new(MaybeProvider {
        calls: AtomicUsize::new(0),
        respond,
    }));
    Arc::new(registry)
}

fn hook_probe_registry(respond: Responder) -> Arc<ProviderRegistry> {
    let mut registry = ProviderRegistry::default();
    registry.set_provider(Arc::new(HookProbeProvider {
        calls: AtomicUsize::new(0),
        respond,
    }));
    Arc::new(registry)
}

struct TempDir(std::path::PathBuf);

impl TempDir {
    fn new(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "pir-session-events-{label}-{}-{}",
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

/// Session-event recorder installed with [`AgentSession::on_session_event`].
type SessionEventLog = Arc<Mutex<Vec<(String, serde_json::Value)>>>;

fn session_event_logger(log: SessionEventLog) -> pir::agent_session::SessionEventHandler {
    Arc::new(move |event: SessionEvent| {
        let json = pir::agent_session::session_event_to_json(&event);
        let kind = json["type"].as_str().unwrap_or_default().to_owned();
        log.lock().unwrap().push((kind, json));
    })
}

fn event_types(log: &SessionEventLog) -> Vec<String> {
    log.lock()
        .unwrap()
        .iter()
        .map(|(kind, _)| kind.clone())
        .collect()
}

fn agent_session_builder(
    registry: Arc<ProviderRegistry>,
    session: Option<SessionManager>,
    compaction: Option<pir::compaction::CompactionConfig>,
    cwd: Option<std::path::PathBuf>,
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
        cwd,
        event_stream: None,
        extensions: None,
        tool_selection: None,
    }
}

fn user_message(message_text: &str) -> Message {
    Message::User(UserMessage {
        role: UserRole::User,
        content: vec![text(message_text)],
        tools_added: None,
        tools_removed: None,
        timestamp: 1,
        sections: None,
    })
}

fn assistant_message(message: AssistantMessage) -> Message {
    Message::Assistant(message)
}

// ---------------------------------------------------------------------------
// Inline plugins (fork-cancel / provider-stream-event recorders)
// ---------------------------------------------------------------------------

struct CancelBeforeForkPlugin {
    info: PluginInfo,
}

impl CancelBeforeForkPlugin {
    fn new() -> Self {
        Self {
            info: PluginInfo::inline("cancel-before-fork"),
        }
    }
}

impl AgentPlugin for CancelBeforeForkPlugin {
    fn info(&self) -> &PluginInfo {
        &self.info
    }
    fn events(&self) -> EventInterest {
        // The fork path emits `session_tree` too; a native plugin that records
        // both must declare both (emission is interest-based).
        EventInterest::List(vec!["session_before_fork".into(), "session_tree".into()])
    }
    fn on_event<'a>(&'a self, event: PluginEvent<'a>) -> PluginFuture<'a, PluginEventOutcome> {
        Box::pin(async move {
            if event.name == "session_before_fork" {
                PluginEventOutcome::SessionBefore { cancel: true }
            } else {
                PluginEventOutcome::None
            }
        })
    }
}

struct RecordBeforeForkPlugin {
    info: PluginInfo,
    seen: Arc<Mutex<Vec<serde_json::Value>>>,
}

impl RecordBeforeForkPlugin {
    fn new(seen: Arc<Mutex<Vec<serde_json::Value>>>) -> Self {
        Self {
            info: PluginInfo::inline("record-before-fork"),
            seen,
        }
    }
}

impl AgentPlugin for RecordBeforeForkPlugin {
    fn info(&self) -> &PluginInfo {
        &self.info
    }
    fn events(&self) -> EventInterest {
        EventInterest::List(vec!["session_before_fork".into(), "session_tree".into()])
    }
    fn on_event<'a>(&'a self, event: PluginEvent<'a>) -> PluginFuture<'a, PluginEventOutcome> {
        let seen = Arc::clone(&self.seen);
        Box::pin(async move {
            seen.lock().unwrap().push(event.payload.clone());
            PluginEventOutcome::None
        })
    }
}

/// Records every `turn_end` boundary payload (upstream `TurnEndEvent`).
struct RecordTurnEndPlugin {
    info: PluginInfo,
    seen: Arc<Mutex<Vec<serde_json::Value>>>,
}

impl RecordTurnEndPlugin {
    fn new(seen: Arc<Mutex<Vec<serde_json::Value>>>) -> Self {
        Self {
            info: PluginInfo::inline("record-turn-end"),
            seen,
        }
    }
}

impl AgentPlugin for RecordTurnEndPlugin {
    fn info(&self) -> &PluginInfo {
        &self.info
    }
    fn events(&self) -> EventInterest {
        EventInterest::List(vec!["turn_end".into()])
    }
    fn on_event<'a>(&'a self, event: PluginEvent<'a>) -> PluginFuture<'a, PluginEventOutcome> {
        let seen = Arc::clone(&self.seen);
        Box::pin(async move {
            if event.name == "turn_end" {
                seen.lock().unwrap().push(event.payload.clone());
            }
            PluginEventOutcome::None
        })
    }
}

/// Records `(event name, turnIndex)` for the turn boundary pair, so one test
/// can assert `turn_start` and `turn_end` name the SAME zero-based turn.
struct RecordTurnIndexPlugin {
    info: PluginInfo,
    seen: Arc<Mutex<Vec<(String, u64)>>>,
}

impl RecordTurnIndexPlugin {
    fn new(seen: Arc<Mutex<Vec<(String, u64)>>>) -> Self {
        Self {
            info: PluginInfo::inline("record-turn-index"),
            seen,
        }
    }
}

impl AgentPlugin for RecordTurnIndexPlugin {
    fn info(&self) -> &PluginInfo {
        &self.info
    }
    fn events(&self) -> EventInterest {
        EventInterest::List(vec!["turn_start".into(), "turn_end".into()])
    }
    fn on_event<'a>(&'a self, event: PluginEvent<'a>) -> PluginFuture<'a, PluginEventOutcome> {
        let seen = Arc::clone(&self.seen);
        Box::pin(async move {
            if matches!(event.name.as_ref(), "turn_start" | "turn_end") {
                let index = event
                    .payload
                    .get("turnIndex")
                    .and_then(serde_json::Value::as_u64)
                    .unwrap_or(u64::MAX);
                seen.lock()
                    .unwrap()
                    .push((event.name.as_ref().to_owned(), index));
            }
            PluginEventOutcome::None
        })
    }
}

struct RecordProviderStreamEventsPlugin {
    info: PluginInfo,
    seen: Arc<Mutex<Vec<serde_json::Value>>>,
}

impl RecordProviderStreamEventsPlugin {
    fn new(seen: Arc<Mutex<Vec<serde_json::Value>>>) -> Self {
        Self {
            info: PluginInfo::inline("record-provider-stream"),
            seen,
        }
    }
}

impl AgentPlugin for RecordProviderStreamEventsPlugin {
    fn info(&self) -> &PluginInfo {
        &self.info
    }
    fn events(&self) -> EventInterest {
        EventInterest::List(vec!["provider_stream_event".into()])
    }
    fn on_event<'a>(&'a self, event: PluginEvent<'a>) -> PluginFuture<'a, PluginEventOutcome> {
        let seen = Arc::clone(&self.seen);
        Box::pin(async move {
            // The unified dispatch hands every event to every plugin; the
            // recorder keeps only the provider_stream_event payloads.
            if event.name == "provider_stream_event" {
                seen.lock().unwrap().push(event.payload.clone());
            }
            PluginEventOutcome::None
        })
    }
}

// ---------------------------------------------------------------------------
// Serialization goldens
// ---------------------------------------------------------------------------

#[test]
fn session_event_json_golden_shapes() {
    let json = pir::agent_session::session_event_to_json(&SessionEvent::QueueUpdate {
        steering: vec!["a".into()],
        follow_up: vec![],
        delivered_steering: vec![],
        delivered_follow_up: vec![],
    });
    assert_eq!(
        json,
        serde_json::json!({ "type": "queue_update", "steering": ["a"], "followUp": [] })
    );

    let json = pir::agent_session::session_event_to_json(&SessionEvent::CompactionStart {
        reason: "manual",
    });
    assert_eq!(
        json,
        serde_json::json!({ "type": "compaction_start", "reason": "manual" })
    );

    let json = pir::agent_session::session_event_to_json(&SessionEvent::CompactionEnd {
        reason: "threshold",
        result: Some(serde_json::json!({
            "summary": "s",
            "firstKeptEntryId": "e1",
            "tokensBefore": 10,
            "estimatedTokensAfter": 4,
        })),
        aborted: false,
        will_retry: true,
        error_message: None,
    });
    assert_eq!(
        json,
        serde_json::json!({
            "type": "compaction_end",
            "reason": "threshold",
            "result": {
                "summary": "s",
                "firstKeptEntryId": "e1",
                "tokensBefore": 10,
                "estimatedTokensAfter": 4,
            },
            "aborted": false,
            "willRetry": true,
        })
    );
    // `errorMessage` rides only when present (upstream `JSON.stringify`
    // omits `undefined`).
    let json = pir::agent_session::session_event_to_json(&SessionEvent::CompactionEnd {
        reason: "overflow",
        result: None,
        aborted: false,
        will_retry: false,
        error_message: Some("Context overflow recovery failed: boom".into()),
    });
    assert_eq!(
        json,
        serde_json::json!({
            "type": "compaction_end",
            "reason": "overflow",
            "result": null,
            "aborted": false,
            "willRetry": false,
            "errorMessage": "Context overflow recovery failed: boom",
        })
    );

    let json = pir::agent_session::session_event_to_json(&SessionEvent::BashExecutionUpdate {
        id: Some("b1".into()),
        delta: "out".into(),
    });
    assert_eq!(
        json,
        serde_json::json!({ "type": "bash_execution_update", "delta": "out", "id": "b1" })
    );
    // `id` stays omitted when absent (upstream `id: options?.id`).
    let json = pir::agent_session::session_event_to_json(&SessionEvent::BashExecutionUpdate {
        id: None,
        delta: "out".into(),
    });
    assert_eq!(
        json,
        serde_json::json!({ "type": "bash_execution_update", "delta": "out" })
    );

    // Transport retry lifecycle: the exact upstream `auto_retry_start` /
    // `auto_retry_end` shapes (agent-session.ts:220-222).
    let json = pir::agent_session::session_event_to_json(&SessionEvent::AutoRetryStart {
        attempt: 2,
        max_attempts: 3,
        delay_ms: 1_500,
        error_message: "HTTP 429".into(),
    });
    assert_eq!(
        json,
        serde_json::json!({
            "type": "auto_retry_start",
            "attempt": 2,
            "maxAttempts": 3,
            "delayMs": 1500,
            "errorMessage": "HTTP 429",
        })
    );
    let json = pir::agent_session::session_event_to_json(&SessionEvent::AutoRetryEnd {
        success: true,
        attempt: 2,
        final_error: None,
    });
    assert_eq!(
        json,
        serde_json::json!({ "type": "auto_retry_end", "success": true, "attempt": 2 })
    );
    // `finalError` rides only when present (upstream `JSON.stringify` omits
    // `undefined`); a cancelled retry reports "Retry cancelled".
    let json = pir::agent_session::session_event_to_json(&SessionEvent::AutoRetryEnd {
        success: false,
        attempt: 1,
        final_error: Some("Retry cancelled".into()),
    });
    assert_eq!(
        json,
        serde_json::json!({
            "type": "auto_retry_end",
            "success": false,
            "attempt": 1,
            "finalError": "Retry cancelled",
        })
    );

    // Summarizer retry lifecycle: upstream `summarization_retry_*` shapes
    // (agent-session.ts:223-236), including the branchSummary/compaction
    // attempt-start union.
    let json =
        pir::agent_session::session_event_to_json(&SessionEvent::SummarizationRetryScheduled {
            attempt: 1,
            max_attempts: 2,
            delay_ms: 500,
            error_message: "stream terminated".into(),
        });
    assert_eq!(
        json,
        serde_json::json!({
            "type": "summarization_retry_scheduled",
            "attempt": 1,
            "maxAttempts": 2,
            "delayMs": 500,
            "errorMessage": "stream terminated",
        })
    );
    let json =
        pir::agent_session::session_event_to_json(&SessionEvent::SummarizationRetryAttemptStart {
            source: pir::agent_session::SummarizationRetrySource::BranchSummary,
        });
    assert_eq!(
        json,
        serde_json::json!({
            "type": "summarization_retry_attempt_start",
            "source": "branchSummary",
        })
    );
    let json =
        pir::agent_session::session_event_to_json(&SessionEvent::SummarizationRetryAttemptStart {
            source: pir::agent_session::SummarizationRetrySource::Compaction { reason: "overflow" },
        });
    assert_eq!(
        json,
        serde_json::json!({
            "type": "summarization_retry_attempt_start",
            "source": "compaction",
            "reason": "overflow",
        })
    );
    let json = pir::agent_session::session_event_to_json(&SessionEvent::SummarizationRetryFinished);
    assert_eq!(
        json,
        serde_json::json!({ "type": "summarization_retry_finished" })
    );
}

/// `background_tool_end` carries the completion's parent attribution, the
/// real result's non-text surfaces, and a handler-reported usage: a nested
/// detach reports the requesting call as `parentToolCallId` (the adoption-time
/// registry value), image / structured payloads travel with the completion
/// instead of being dropped (the completion is the only delivery surface on a
/// store-less host), and `usage` is the detached call's equivalent of a
/// tool-result message's usage field.
#[test]
fn background_tool_end_json_carries_parent_and_result_surfaces() {
    let mut event = SessionEvent::BackgroundToolEnd {
        call_id: "call-parent/0".into(),
        tool_name: "read".into(),
        parent_call_id: Some("call-parent".into()),
        is_error: false,
        duration_ms: Some(7),
        output: "body".into(),
        images: vec![ImageContent {
            data: "aGVsbG8=".into(),
            mime_type: "image/png".into(),
        }],
        details: Some(serde_json::json!({ "path": "a.png" })),
        structured_content: Some(serde_json::json!({ "bytes": 5 })),
        // The `tool_result` handler-reported usage rides the completion (the
        // custom entry has no ToolResultMessage.usage field).
        usage: Some(Usage {
            input: 10,
            output: 5,
            total_tokens: 15,
            ..Default::default()
        }),
    };
    let json = pir::agent_session::session_event_to_json(&event);
    assert_eq!(
        json,
        serde_json::json!({
            "type": "background_tool_end",
            "callId": "call-parent/0",
            "toolName": "read",
            "parentToolCallId": "call-parent",
            "isError": false,
            "output": "body",
            "durationMs": 7,
            "images": [
                { "type": "image", "data": "aGVsbG8=", "mimeType": "image/png" },
            ],
            "details": { "path": "a.png" },
            "structuredContent": { "bytes": 5 },
            "usage": {
                "input": 10,
                "output": 5,
                "cacheRead": 0,
                "cacheWrite": 0,
                "totalTokens": 15,
                "cost": {
                    "input": 0.0,
                    "output": 0.0,
                    "cacheRead": 0.0,
                    "cacheWrite": 0.0,
                    "total": 0.0,
                },
            },
        })
    );

    // A model-issued, text-only completion: every optional key stays absent
    // rather than serializing as null/[].
    let SessionEvent::BackgroundToolEnd {
        parent_call_id,
        images,
        details,
        structured_content,
        usage,
        ..
    } = &mut event
    else {
        unreachable!("constructed above");
    };
    *parent_call_id = None;
    images.clear();
    *details = None;
    *structured_content = None;
    *usage = None;
    let json = pir::agent_session::session_event_to_json(&event);
    assert_eq!(
        json,
        serde_json::json!({
            "type": "background_tool_end",
            "callId": "call-parent/0",
            "toolName": "read",
            "isError": false,
            "output": "body",
            "durationMs": 7,
        })
    );
}

// ---------------------------------------------------------------------------
// turn_end entry ids (upstream `TurnEndEvent.messageEntryId` /
// `toolResultEntryIds`, agent-session.ts:868-874)
// ---------------------------------------------------------------------------

/// A tool that returns one text result so the turn has a tool-result entry.
struct TurnEndEchoTool;

impl AgentTool for TurnEndEchoTool {
    fn name(&self) -> &'static str {
        "echo"
    }
    fn description(&self) -> &'static str {
        "echoes"
    }
    fn run(&self, _args: &serde_json::Value, _ctx: &ToolContext) -> BoxFuture<'static, ToolResult> {
        Box::pin(async { ToolResult::ok("echo output") })
    }
}

fn turn_end_tool_registry() -> ToolRegistry {
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(TurnEndEchoTool));
    tools
}

fn tool_call_message() -> AssistantMessage {
    assistant(
        vec![ContentBlock::ToolCall(ToolCall {
            id: "call-1".into(),
            name: "echo".into(),
            arguments: BTreeMap::new(),
            thought_signature: None,
            namespace: None,
            arguments_error: None,
        })],
        StopReason::ToolUse,
    )
}

/// The extension `turn_end` payload carries the ids of the entries the
/// per-message sink persisted for the turn: `messageEntryId` is the assistant
/// entry, `toolResultEntryIds` the tool-result entries in source order. Both
/// resolve back to real message entries in the session transcript.
#[tokio::test]
async fn turn_end_carries_persisted_message_and_tool_result_entry_ids() {
    let temp = TempDir::new("turn-end-entry-ids");
    let registry = fake_registry(Arc::new(|index, _context| {
        if index == 0 {
            tool_call_message()
        } else {
            assistant(vec![text("done")], StopReason::Stop)
        }
    }));
    let session = SessionManager::create(".", &temp.0).expect("create session");
    let seen: Arc<Mutex<Vec<serde_json::Value>>> = Arc::new(Mutex::new(Vec::new()));
    let agent = AgentSession::new_with_plugins(
        AgentSessionConfig {
            tools: turn_end_tool_registry(),
            ..agent_session_builder(registry, Some(session), None, Some(temp.0.clone()))
        },
        vec![Arc::new(RecordTurnEndPlugin::new(Arc::clone(&seen)))],
    );

    agent
        .prompt("go", Arc::new(|_event| {}))
        .await
        .expect("prompt");

    let seen = seen.lock().unwrap().clone();
    assert_eq!(seen.len(), 2, "one turn_end per completed turn: {seen:?}");

    let tool_turn = &seen[0];
    assert_eq!(tool_turn["type"], serde_json::json!("turn_end"));
    let message_entry_id = tool_turn["messageEntryId"]
        .as_str()
        .expect("the assistant entry id rides turn_end")
        .to_owned();
    let tool_result_entry_ids: Vec<String> = tool_turn["toolResultEntryIds"]
        .as_array()
        .expect("toolResultEntryIds is an array")
        .iter()
        .map(|value| value.as_str().expect("entry id string").to_owned())
        .collect();
    assert_eq!(
        tool_result_entry_ids.len(),
        1,
        "the tool turn has exactly one persisted result: {tool_turn}"
    );

    // Both ids resolve to persisted message entries with the matching roles.
    let resolved = agent
        .with_session_manager(|manager| {
            let mut resolved = Vec::new();
            for (id, expected_role) in [
                (message_entry_id.clone(), "assistant"),
                (tool_result_entry_ids[0].clone(), "toolResult"),
            ] {
                match manager.entry(&id) {
                    Some(FileEntry::Message(entry)) => {
                        assert_eq!(entry.message.role(), expected_role, "entry {id}");
                        assert_eq!(entry.id, id);
                        resolved.push((id, expected_role.to_owned()));
                    }
                    other => panic!("entry {id} must be a message entry, got {other:?}"),
                }
            }
            resolved
        })
        .await
        .expect("session storage");

    // The final (no-tool) turn still carries its assistant entry id and an
    // empty tool-result list.
    let final_turn = &seen[1];
    assert!(
        final_turn["messageEntryId"].is_string(),
        "every persisted assistant turn carries its entry id: {final_turn}"
    );
    assert_eq!(
        final_turn["toolResultEntryIds"],
        serde_json::json!([]),
        "a tool-less turn has no tool-result entries"
    );
    assert_eq!(resolved.len(), 2);
}

/// Store-less sessions have no persisted entries: the ids are OPTIONAL on the
/// wire (the assistant id is omitted, the list is empty), so old consumers and
/// in-memory embedders keep working.
#[tokio::test]
async fn turn_end_entry_ids_are_optional_without_session_storage() {
    let temp = TempDir::new("turn-end-ids-no-store");
    let registry = fake_registry(Arc::new(|_index, _context| {
        assistant(vec![text("done")], StopReason::Stop)
    }));
    let seen: Arc<Mutex<Vec<serde_json::Value>>> = Arc::new(Mutex::new(Vec::new()));
    let agent = AgentSession::new_with_plugins(
        AgentSessionConfig {
            tools: turn_end_tool_registry(),
            ..agent_session_builder(registry, None, None, Some(temp.0.clone()))
        },
        vec![Arc::new(RecordTurnEndPlugin::new(Arc::clone(&seen)))],
    );

    agent
        .prompt("go", Arc::new(|_event| {}))
        .await
        .expect("prompt");

    let seen = seen.lock().unwrap().clone();
    assert_eq!(seen.len(), 1, "{seen:?}");
    assert!(
        seen[0].get("messageEntryId").is_none(),
        "no persisted entry means the field is omitted: {seen:?}"
    );
    assert_eq!(seen[0]["toolResultEntryIds"], serde_json::json!([]));
    // The rest of the upstream payload shape is unchanged.
    assert_eq!(seen[0]["type"], serde_json::json!("turn_end"));
    assert_eq!(seen[0]["outcome"], serde_json::json!("completed"));
    assert!(seen[0]["message"].is_object());
}

/// `turn_start` and the `turn_end` boundary share ONE zero-based turn index
/// (upstream keeps a single `_turnIndex`, reset at agent start and read by
/// both events). The boundary used to run its own 1-based counter, so the
/// first turn reported `turn_start: 0` but `turn_end: 1`.
#[tokio::test]
async fn turn_start_and_turn_end_share_the_zero_based_turn_index() {
    let temp = TempDir::new("turn-index");
    let registry = fake_registry(Arc::new(|index, _context| {
        if index < 2 {
            assistant(
                vec![ContentBlock::ToolCall(ToolCall {
                    id: format!("call-{index}"),
                    name: "echo".into(),
                    arguments: BTreeMap::new(),
                    thought_signature: None,
                    namespace: None,
                    arguments_error: None,
                })],
                StopReason::ToolUse,
            )
        } else {
            assistant(vec![text("done")], StopReason::Stop)
        }
    }));
    let session = SessionManager::create(".", &temp.0).expect("create session");
    let seen: Arc<Mutex<Vec<(String, u64)>>> = Arc::new(Mutex::new(Vec::new()));
    let agent = AgentSession::new_with_plugins(
        AgentSessionConfig {
            tools: turn_end_tool_registry(),
            ..agent_session_builder(registry, Some(session), None, Some(temp.0.clone()))
        },
        vec![Arc::new(RecordTurnIndexPlugin::new(Arc::clone(&seen)))],
    );

    agent
        .prompt("go", Arc::new(|_event| {}))
        .await
        .expect("prompt");

    let seen = seen.lock().unwrap().clone();
    assert_eq!(
        seen,
        vec![
            ("turn_start".to_owned(), 0),
            ("turn_end".to_owned(), 0),
            ("turn_start".to_owned(), 1),
            ("turn_end".to_owned(), 1),
            ("turn_start".to_owned(), 2),
            ("turn_end".to_owned(), 2),
        ],
        "turn_end must report the same 0-based index as the preceding turn_start: {seen:?}"
    );
}

// ---------------------------------------------------------------------------
// Transport retry lifecycle (`auto_retry_start` / `auto_retry_end`)
// ---------------------------------------------------------------------------

/// The session installs the transport retry observers and forwards the
/// lifecycle to `session.subscribe` listeners with the upstream JSON shapes
/// (the RPC session-event bridge and the print JSON host serialize the same
/// events).
#[tokio::test]
async fn transport_retry_lifecycle_reaches_session_event_subscribers() {
    let temp = TempDir::new("retry-events");
    let registry = retry_probe_registry(Arc::new(|_index, _context| {
        assistant(vec![text("done")], StopReason::Stop)
    }));
    let session = SessionManager::create(".", &temp.0).expect("create session");
    let agent = Arc::new(AgentSession::new(agent_session_builder(
        registry,
        Some(session),
        None,
        Some(temp.0.clone()),
    )));

    let log: SessionEventLog = Arc::new(Mutex::new(Vec::new()));
    agent.on_session_event(session_event_logger(Arc::clone(&log)));

    let noop: pir::agent_session::EventHandler = Arc::new(|_event| {});
    agent.prompt("hello", noop).await.expect("prompt");

    let retry: Vec<serde_json::Value> = log
        .lock()
        .unwrap()
        .iter()
        .filter(|(kind, _)| kind.starts_with("auto_retry"))
        .map(|(_, json)| json.clone())
        .collect();
    assert_eq!(
        retry,
        vec![
            serde_json::json!({
                "type": "auto_retry_start",
                "attempt": 1,
                "maxAttempts": 2,
                "delayMs": 1,
                "errorMessage": "HTTP 429",
            }),
            serde_json::json!({
                "type": "auto_retry_end",
                "success": true,
                "attempt": 1,
            }),
        ],
        "the session forwards the transport retry lifecycle to subscribers"
    );
}

/// Summarizer retry lifecycle (upstream `summarization_retry_scheduled` /
/// `_attempt_start` / `_finished`, agent-session.ts:223-236,3781-3804): a
/// dropped summarizer stream is retried once and the session's event channel
/// carries the three upstream-shaped frames with the compaction source/reason.
#[tokio::test]
async fn summarization_retry_lifecycle_reaches_session_event_subscribers() {
    let temp = TempDir::new("summarization-retry-events");
    let summary_calls = Arc::new(AtomicUsize::new(0));
    let summary_calls_for_responder = Arc::clone(&summary_calls);
    let registry = maybe_registry(Arc::new(move |_index, context| {
        if context.system_prompt.as_deref()
            == Some(pir::compaction::prompts::SUMMARIZATION_SYSTEM_PROMPT)
        {
            let call = summary_calls_for_responder.fetch_add(1, Ordering::SeqCst);
            if call == 0 {
                // First summarizer call: the stream ends without a terminal
                // message, so the completion closure errors and the retry
                // wrapper schedules the single retry.
                return None;
            }
            return Some(assistant(vec![text("RECOVERED SUMMARY")], StopReason::Stop));
        }
        Some(assistant(vec![text("reply")], StopReason::Stop))
    }));
    let session = SessionManager::create(".", &temp.0).expect("create session");
    let agent = Arc::new(AgentSession::new(agent_session_builder(
        registry,
        Some(session),
        Some(pir::compaction::CompactionConfig {
            enabled: true,
            reserve_tokens: 1,
            keep_recent_tokens: 8,
        }),
        Some(temp.0.clone()),
    )));
    let log: SessionEventLog = Arc::new(Mutex::new(Vec::new()));
    agent.on_session_event(session_event_logger(Arc::clone(&log)));

    // Seed enough complete turns for a manual cut.
    let noop: pir::agent_session::EventHandler = Arc::new(|_event| {});
    agent
        .prompt(&format!("hello-{}", "x".repeat(400)), Arc::clone(&noop))
        .await
        .expect("seed 1");
    agent
        .prompt(&format!("again-{}", "y".repeat(400)), noop)
        .await
        .expect("seed 2");
    let result = agent.compact(None).await.expect("compaction retries once");
    assert_eq!(result.summary, "RECOVERED SUMMARY");
    assert_eq!(
        summary_calls.load(Ordering::SeqCst),
        2,
        "one dropped summarizer call plus exactly one retry"
    );

    let retry: Vec<serde_json::Value> = log
        .lock()
        .unwrap()
        .iter()
        .filter(|(kind, _)| kind.starts_with("summarization_retry"))
        .map(|(_, json)| json.clone())
        .collect();
    assert_eq!(
        retry,
        vec![
            serde_json::json!({
                "type": "summarization_retry_scheduled",
                "attempt": 1,
                "maxAttempts": 1,
                "delayMs": 0,
                "errorMessage": "summarization stream did not terminate",
            }),
            serde_json::json!({
                "type": "summarization_retry_attempt_start",
                "source": "compaction",
                "reason": "manual",
            }),
            serde_json::json!({ "type": "summarization_retry_finished" }),
        ],
        "the summarization retry lifecycle reaches session.subscribe listeners: {retry:?}"
    );
}

// ---------------------------------------------------------------------------
// Compaction events + isCompacting
// ---------------------------------------------------------------------------

#[tokio::test]
async fn manual_compaction_emits_start_end_and_covers_is_compacting() {
    let temp = TempDir::new("manual-compact-events");
    let registry = fake_registry(Arc::new(|_index, _context| {
        assistant(vec![text("MANUAL SUMMARY")], StopReason::Stop)
    }));
    let session = SessionManager::create(".", &temp.0).expect("create session");
    // A large window keeps the threshold hook idle — only the MANUAL compact
    // below may run.
    let mut model = fake_model();
    model.context_window = 1_000_000;
    let agent = Arc::new(AgentSession::new(agent_session_builder(
        registry,
        Some(session),
        Some(pir::compaction::CompactionConfig {
            enabled: true,
            reserve_tokens: 1,
            keep_recent_tokens: 8,
        }),
        Some(temp.0.clone()),
    )));
    // Pin the model window on the live session too.
    let _ = agent.set_model(model.clone()).await;

    let log: SessionEventLog = Arc::new(Mutex::new(Vec::new()));
    agent.on_session_event(session_event_logger(Arc::clone(&log)));
    let observed_compacting = Arc::new(AtomicBool::new(false));
    let observed_at_end = Arc::new(AtomicBool::new(false));
    let agent_for_handler = Arc::clone(&agent);
    let observed = Arc::clone(&observed_compacting);
    let observed_end = Arc::clone(&observed_at_end);
    agent.on_session_event(Arc::new(move |event: SessionEvent| match event {
        SessionEvent::CompactionStart { reason } => {
            assert_eq!(reason, "manual");
            observed.store(agent_for_handler.is_compacting(), Ordering::SeqCst);
        }
        SessionEvent::CompactionEnd { .. } => {
            observed_end.store(agent_for_handler.is_compacting(), Ordering::SeqCst);
        }
        _ => {}
    }));

    // Idle session: there is no manual compaction to cancel.
    assert!(!agent.abort_compaction());

    // Seed a transcript with complete turns before the cut point.
    let noop: pir::agent_session::EventHandler = Arc::new(|_event| {});
    agent
        .prompt(&format!("u-{}", "x".repeat(400)), Arc::clone(&noop))
        .await
        .expect("seed prompt");
    agent
        .prompt(&format!("u-{}", "y".repeat(400)), noop)
        .await
        .expect("seed prompt 2");

    assert!(!agent.is_compacting(), "idle session is not compacting");
    let result = agent.compact(None).await.expect("compact");
    assert_eq!(result.summary, "MANUAL SUMMARY");

    let kinds = event_types(&log);
    // The compaction events of this test ride among the queue_update frames
    // the prompt runs may emit; only the compaction pair is pinned here.
    let compaction: Vec<serde_json::Value> = log
        .lock()
        .unwrap()
        .iter()
        .filter(|(kind, _)| kind.starts_with("compaction_"))
        .map(|(_, json)| json.clone())
        .collect();
    assert_eq!(
        kinds
            .iter()
            .filter(|kind| kind.starts_with("compaction_"))
            .count(),
        2,
        "exactly one start/end pair for one manual compact: {kinds:?}"
    );
    assert_eq!(compaction[0]["type"], serde_json::json!("compaction_start"));
    assert_eq!(compaction[0]["reason"], serde_json::json!("manual"));
    let end = &compaction[1];
    assert_eq!(end["type"], serde_json::json!("compaction_end"));
    assert_eq!(end["reason"], serde_json::json!("manual"));
    assert_eq!(end["aborted"], serde_json::json!(false));
    assert_eq!(end["willRetry"], serde_json::json!(false));
    assert_eq!(
        end["result"]["summary"],
        serde_json::json!("MANUAL SUMMARY")
    );
    assert!(end["result"]["tokensBefore"].is_u64());
    assert!(end["result"]["estimatedTokensAfter"].is_u64());
    assert!(end.get("errorMessage").is_none());
    drop(compaction);

    // `isCompacting` was true when the run was inside the summarizer call
    // (the compaction_start handler) and false again by compaction_end.
    assert!(
        observed_compacting.load(Ordering::SeqCst),
        "isCompacting must be set during the compaction run"
    );
    assert!(
        !observed_at_end.load(Ordering::SeqCst),
        "isCompacting must be cleared by compaction_end"
    );
    assert!(!agent.is_compacting());
}

#[tokio::test]
async fn failed_manual_compaction_emits_end_with_error_message() {
    let temp = TempDir::new("failed-compact-events");
    let registry = fake_registry(Arc::new(|_index, _context| {
        assistant(vec![text("unused")], StopReason::Stop)
    }));
    let session = SessionManager::create(".", &temp.0).expect("create session");
    let agent = AgentSession::new(agent_session_builder(
        registry,
        Some(session),
        None,
        Some(temp.0.clone()),
    ));
    let log: SessionEventLog = Arc::new(Mutex::new(Vec::new()));
    agent.on_session_event(session_event_logger(Arc::clone(&log)));

    // An empty session has nothing to compact: upstream still emits
    // compaction_start and then compaction_end with an errorMessage.
    let result = agent.compact(None).await;
    assert!(result.is_err(), "nothing to compact on an empty session");

    let kinds = event_types(&log);
    assert_eq!(
        kinds
            .iter()
            .filter(|kind| kind.starts_with("compaction_"))
            .count(),
        2,
        "{kinds:?}"
    );
    let compaction: Vec<serde_json::Value> = log
        .lock()
        .unwrap()
        .iter()
        .filter(|(kind, _)| kind.starts_with("compaction_"))
        .map(|(_, json)| json.clone())
        .collect();
    let end = &compaction[1];
    assert_eq!(end["result"], serde_json::json!(null));
    assert_eq!(end["aborted"], serde_json::json!(false));
    assert_eq!(end["willRetry"], serde_json::json!(false));
    let message = end["errorMessage"].as_str().expect("errorMessage");
    assert!(
        message.contains("Compaction failed:"),
        "manual failure message: {message}"
    );
    assert!(!agent.is_compacting());
}

// ---------------------------------------------------------------------------
// queue_update events
// ---------------------------------------------------------------------------

#[tokio::test]
async fn queue_update_events_fire_on_enqueue_and_clear() {
    let temp = TempDir::new("queue-update");
    let registry = fake_registry(Arc::new(|_index, _context| {
        assistant(vec![text("ok")], StopReason::Stop)
    }));
    let session = SessionManager::create(".", &temp.0).expect("create session");
    let agent = AgentSession::new(agent_session_builder(
        registry,
        Some(session),
        None,
        Some(temp.0.clone()),
    ));
    let log: SessionEventLog = Arc::new(Mutex::new(Vec::new()));
    agent.on_session_event(session_event_logger(Arc::clone(&log)));

    agent.steer("steer one");
    agent.follow_up("follow one");
    agent.steer("steer two");

    {
        let log = log.lock().unwrap();
        let updates: Vec<&serde_json::Value> = log
            .iter()
            .filter(|(kind, _)| kind == "queue_update")
            .map(|(_, json)| json)
            .collect();
        assert_eq!(updates.len(), 3, "one event per enqueue: {updates:?}");
        assert_eq!(
            updates[0],
            &serde_json::json!({
                "type": "queue_update",
                "steering": ["steer one"],
                "followUp": [],
            })
        );
        assert_eq!(
            updates[1],
            &serde_json::json!({
                "type": "queue_update",
                "steering": ["steer one"],
                "followUp": ["follow one"],
            })
        );
        assert_eq!(
            updates[2],
            &serde_json::json!({
                "type": "queue_update",
                "steering": ["steer one", "steer two"],
                "followUp": ["follow one"],
            })
        );
    }

    let cleared = agent.clear_queue().await;
    assert_eq!(cleared.steering.len(), 2);
    assert_eq!(cleared.follow_up.len(), 1);
    let log = log.lock().unwrap();
    let last = &log.last().expect("clear event").1;
    assert_eq!(
        last,
        &serde_json::json!({
            "type": "queue_update",
            "steering": [],
            "followUp": [],
        })
    );
}

#[tokio::test]
async fn queue_update_reflects_drains() {
    // The loop's steering drain (steering_fn) updates the snapshot: steer
    // while a gated run streams, then let the run deliver it.
    let temp = TempDir::new("queue-update-drain");
    let (gate_tx, gate_rx) = tokio::sync::watch::channel(false);
    let entered = Arc::new(AtomicUsize::new(0));

    struct GateTool {
        gate: tokio::sync::watch::Receiver<bool>,
        entered: Arc<AtomicUsize>,
    }
    impl AgentTool for GateTool {
        fn name(&self) -> &'static str {
            "gate"
        }
        fn description(&self) -> &'static str {
            "blocks until released"
        }
        fn parameters(&self) -> Option<serde_json::Value> {
            Some(serde_json::json!({"type": "object"}))
        }
        fn run(
            &self,
            _args: &serde_json::Value,
            ctx: &ToolContext,
        ) -> BoxFuture<'static, ToolResult> {
            self.entered.fetch_add(1, Ordering::SeqCst);
            let mut gate = self.gate.clone();
            let aborted = ctx.signal.clone();
            Box::pin(async move {
                loop {
                    if aborted.as_ref().is_some_and(|signal| signal.is_aborted()) {
                        return ToolResult::error("aborted");
                    }
                    if *gate.borrow() {
                        return ToolResult::ok("gate open");
                    }
                    if tokio::time::timeout(std::time::Duration::from_millis(20), gate.changed())
                        .await
                        .is_err()
                    {
                        continue;
                    }
                }
            })
        }
    }

    let registry = fake_registry(Arc::new(|index, _context| {
        if index == 0 {
            assistant(
                vec![ContentBlock::ToolCall(ToolCall {
                    id: "call-1".into(),
                    name: "gate".into(),
                    arguments: BTreeMap::new(),
                    thought_signature: None,
                    namespace: None,
                    arguments_error: None,
                })],
                StopReason::ToolUse,
            )
        } else {
            assistant(vec![text("after steer")], StopReason::Stop)
        }
    }));
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(GateTool {
        gate: gate_rx,
        entered: Arc::clone(&entered),
    }));
    let session = SessionManager::create(".", &temp.0).expect("create session");
    let agent = Arc::new(AgentSession::new(AgentSessionConfig {
        tools,
        ..agent_session_builder(registry, Some(session), None, Some(temp.0.clone()))
    }));
    let log: SessionEventLog = Arc::new(Mutex::new(Vec::new()));
    agent.on_session_event(session_event_logger(Arc::clone(&log)));

    let prompt_task = tokio::spawn({
        let agent = Arc::clone(&agent);
        async move { agent.prompt("start", Arc::new(|_event| {})).await }
    });
    for _ in 0..2000 {
        if entered.load(Ordering::SeqCst) > 0 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    agent.steer("redirect now");
    let before = log.lock().unwrap().last().expect("enqueue event").1.clone();
    assert_eq!(before["steering"], serde_json::json!(["redirect now"]));

    let _ = gate_tx.send(true);
    prompt_task.await.expect("prompt task").expect("prompt");

    // The drain removed the delivered text from the queue_update snapshot.
    // The settle now also emits `agent_settled` after the drain, so the LAST
    // event is no longer the queue update; find the latest one.
    let last = log
        .lock()
        .unwrap()
        .iter()
        .rev()
        .find(|(_, value)| value["type"] == "queue_update")
        .expect("drain event")
        .1
        .clone();
    assert_eq!(
        last,
        serde_json::json!({
            "type": "queue_update",
            "steering": [],
            "followUp": [],
        })
    );
}

// ---------------------------------------------------------------------------
// bash_execution_update streaming
// ---------------------------------------------------------------------------

#[tokio::test]
async fn bash_execution_update_streams_chunks_with_the_command_id() {
    let temp = TempDir::new("bash-stream-events");
    let registry = fake_registry(Arc::new(|_index, _context| {
        assistant(vec![text("ok")], StopReason::Stop)
    }));
    let session = SessionManager::create(".", &temp.0).expect("create session");
    let agent = AgentSession::new(agent_session_builder(
        registry,
        Some(session),
        None,
        Some(temp.0.clone()),
    ));
    let log: SessionEventLog = Arc::new(Mutex::new(Vec::new()));
    agent.on_session_event(session_event_logger(Arc::clone(&log)));

    // Two writes separated by a sleep produce at least two streamed chunks.
    let result = agent
        .execute_bash_with_id(
            "printf FIRST; sleep 0.4; printf SECOND",
            false,
            Some("b-42"),
            None,
        )
        .await;
    assert!(!result.cancelled);

    let log = log.lock().unwrap();
    let updates: Vec<&serde_json::Value> = log
        .iter()
        .filter(|(kind, _)| kind == "bash_execution_update")
        .map(|(_, json)| json)
        .collect();
    assert!(
        updates.len() >= 2,
        "chunks stream as separate bash_execution_update events: {updates:?}"
    );
    let mut joined = String::new();
    for update in &updates {
        assert_eq!(update["type"], serde_json::json!("bash_execution_update"));
        assert_eq!(update["id"], serde_json::json!("b-42"));
        assert!(update["delta"].is_string());
        joined.push_str(update["delta"].as_str().unwrap_or_default());
    }
    assert!(joined.contains("FIRST"), "joined deltas: {joined}");
    assert!(joined.contains("SECOND"), "joined deltas: {joined}");
}

#[tokio::test]
async fn bash_execution_update_without_id_omits_the_field() {
    let temp = TempDir::new("bash-stream-no-id");
    let registry = fake_registry(Arc::new(|_index, _context| {
        assistant(vec![text("ok")], StopReason::Stop)
    }));
    let session = SessionManager::create(".", &temp.0).expect("create session");
    let agent = AgentSession::new(agent_session_builder(
        registry,
        Some(session),
        None,
        Some(temp.0.clone()),
    ));
    let log: SessionEventLog = Arc::new(Mutex::new(Vec::new()));
    agent.on_session_event(session_event_logger(Arc::clone(&log)));

    let _ = agent
        .execute_bash_with_id("echo hi", false, None, None)
        .await;
    let log = log.lock().unwrap();
    let updates: Vec<&serde_json::Value> = log
        .iter()
        .filter(|(kind, _)| kind == "bash_execution_update")
        .map(|(_, json)| json)
        .collect();
    assert!(!updates.is_empty(), "updates streamed");
    for update in updates {
        assert!(update.get("id").is_none(), "no id field: {update}");
    }
}

// ---------------------------------------------------------------------------
// fork: session_before_fork + position semantics
// ---------------------------------------------------------------------------

fn fork_fixture(dir: &std::path::Path) -> (SessionManager, String, String) {
    let mut session = SessionManager::create(".", dir).expect("create session");
    let first_id = session
        .append_message(user_message("first message"))
        .expect("append first");
    let second_id = session
        .append_message(user_message("second message"))
        .expect("append second");
    (session, first_id, second_id)
}

#[tokio::test]
async fn fork_before_branches_at_the_parent_and_returns_selected_text() {
    let temp = TempDir::new("fork-before");
    let (session, _first_id, second_id) = fork_fixture(&temp.0);
    let registry = fake_registry(Arc::new(|_index, _context| {
        assistant(vec![text("ok")], StopReason::Stop)
    }));
    let events: Arc<Mutex<Vec<serde_json::Value>>> = Arc::new(Mutex::new(Vec::new()));
    let recorder = RecordBeforeForkPlugin::new(Arc::clone(&events));
    let agent = AgentSession::new_with_plugins(
        agent_session_builder(registry, Some(session), None, Some(temp.0.clone())),
        vec![Arc::new(recorder)],
    );

    // Upstream fork(entryId, undefined): position "before" — the entry must
    // be a user message, the fork branches at its PARENT, and the user text
    // is the selectedText.
    let outcome = agent
        .fork_entry(&second_id, ForkPosition::Before)
        .await
        .expect("fork");
    assert_eq!(
        outcome,
        ForkEntryOutcome::Branched {
            selected_text: Some("second message".to_owned()),
        }
    );
    // The leaf now sits on the PARENT of the selected entry (the first
    // message), so the projected context ends at "first message".
    let projected = agent
        .with_session_manager(|sm| {
            sm.build_context_entries()
                .unwrap_or_default()
                .into_iter()
                .filter_map(|message| match message {
                    Message::User(user) => Some(
                        user.content
                            .iter()
                            .filter_map(|block| match block {
                                ContentBlock::Text(text) => Some(text.text.clone()),
                                _ => None,
                            })
                            .collect::<String>(),
                    ),
                    _ => None,
                })
                .collect::<Vec<_>>()
        })
        .await
        .expect("projection");
    assert_eq!(projected, vec!["first message".to_owned()]);

    // session_before_fork fired with the upstream payload, followed by the
    // post-branch session_tree move event.
    let events = events.lock().unwrap();
    assert_eq!(events.len(), 2, "{events:?}");
    assert_eq!(events[0]["type"], serde_json::json!("session_before_fork"));
    assert_eq!(events[0]["entryId"], serde_json::json!(second_id));
    assert_eq!(events[0]["position"], serde_json::json!("before"));
    assert_eq!(events[1]["type"], serde_json::json!("session_tree"));
    assert_eq!(
        events[1]["oldLeafId"],
        serde_json::json!(second_id),
        "the abandoned leaf was the selected entry"
    );
}

#[tokio::test]
async fn fork_at_branches_at_the_entry_without_selected_text() {
    let temp = TempDir::new("fork-at");
    let (session, first_id, _second_id) = fork_fixture(&temp.0);
    let registry = fake_registry(Arc::new(|_index, _context| {
        assistant(vec![text("ok")], StopReason::Stop)
    }));
    let agent = AgentSession::new(agent_session_builder(
        registry,
        Some(session),
        None,
        Some(temp.0.clone()),
    ));

    let outcome = agent
        .fork_entry(&first_id, ForkPosition::At)
        .await
        .expect("fork");
    assert_eq!(
        outcome,
        ForkEntryOutcome::Branched {
            selected_text: None
        }
    );
    // The leaf moved ONTO the entry: the projection ends at the first
    // message (the second message is on the abandoned branch).
    let projected = agent
        .with_session_manager(|sm| sm.build_context_entries().unwrap_or_default().len())
        .await;
    assert_eq!(projected, Some(1));
}

#[tokio::test]
async fn fork_before_the_first_user_message_starts_a_fresh_parent_linked_session() {
    let temp = TempDir::new("fork-root-before");
    let (session, first_id, _second_id) = fork_fixture(&temp.0);
    let parent_path = session.path().to_path_buf();
    let registry = fake_registry(Arc::new(|_index, _context| {
        assistant(vec![text("ok")], StopReason::Stop)
    }));
    let agent = AgentSession::new(agent_session_builder(
        registry,
        Some(session),
        None,
        Some(temp.0.clone()),
    ));

    let outcome = agent
        .fork_entry(&first_id, ForkPosition::Before)
        .await
        .expect("fork");
    assert_eq!(
        outcome,
        ForkEntryOutcome::Branched {
            selected_text: Some("first message".to_owned()),
        }
    );
    // Upstream `SessionManager.create` + `newSession({parentSession})`: a
    // fresh EMPTY session carrying the old one as its parent link.
    let (new_path, has_parent, empty) = agent
        .with_session_manager(|sm| {
            (
                sm.path().to_path_buf(),
                sm.parent_session().is_some(),
                sm.build_context_entries().unwrap_or_default().is_empty(),
            )
        })
        .await
        .expect("storage");
    assert_ne!(new_path, parent_path, "a fresh session file was created");
    assert!(has_parent, "the parent link is recorded");
    assert!(empty, "the fresh session has no messages");
}

#[tokio::test]
async fn fork_rejects_invalid_entries_and_non_user_targets_for_before() {
    let temp = TempDir::new("fork-invalid");
    let registry = fake_registry(Arc::new(|_index, _context| {
        assistant(vec![text("ok")], StopReason::Stop)
    }));
    let mut session = SessionManager::create(".", &temp.0).expect("create session");
    let assistant_id = session
        .append_message(assistant_message(assistant(
            vec![text("model reply")],
            StopReason::Stop,
        )))
        .expect("append assistant");
    let agent = AgentSession::new(agent_session_builder(
        registry,
        Some(session),
        None,
        Some(temp.0.clone()),
    ));

    let error = agent
        .fork_entry("missing-id", ForkPosition::Before)
        .await
        .expect_err("missing entry");
    assert!(error.contains("Invalid entry ID for forking"), "{error}");

    // `before` requires a user message (upstream throws for an assistant
    // entry); `at` accepts it.
    let error = agent
        .fork_entry(&assistant_id, ForkPosition::Before)
        .await
        .expect_err("assistant entry");
    assert!(error.contains("Invalid entry ID for forking"), "{error}");
    let outcome = agent
        .fork_entry(&assistant_id, ForkPosition::At)
        .await
        .expect("fork at assistant entry");
    assert_eq!(
        outcome,
        ForkEntryOutcome::Branched {
            selected_text: None
        }
    );
}

#[tokio::test]
async fn session_before_fork_cancel_aborts_the_fork() {
    let temp = TempDir::new("fork-cancel");
    let registry = fake_registry(Arc::new(|_index, _context| {
        assistant(vec![text("ok")], StopReason::Stop)
    }));
    let (session, _first_id, second_id) = fork_fixture(&temp.0);
    let agent = AgentSession::new_with_plugins(
        agent_session_builder(registry, Some(session), None, Some(temp.0.clone())),
        vec![Arc::new(CancelBeforeForkPlugin::new())],
    );

    let outcome = agent
        .fork_entry(&second_id, ForkPosition::Before)
        .await
        .expect("fork outcome");
    assert_eq!(outcome, ForkEntryOutcome::Cancelled);
    // The leaf is UNCHANGED (the fork was aborted): both messages remain in
    // the projection.
    let projected = agent
        .with_session_manager(|sm| sm.build_context_entries().unwrap_or_default().len())
        .await;
    assert_eq!(projected, Some(2));
}

// ---------------------------------------------------------------------------
// provider_stream_event extension dispatch
// ---------------------------------------------------------------------------

#[tokio::test]
async fn provider_stream_events_dispatch_to_extensions() {
    let temp = TempDir::new("provider-stream-events");
    let registry = hook_probe_registry(Arc::new(|_index, _context| {
        assistant(vec![text("ok")], StopReason::Stop)
    }));
    let session = SessionManager::create(".", &temp.0).expect("create session");
    let seen: Arc<Mutex<Vec<serde_json::Value>>> = Arc::new(Mutex::new(Vec::new()));
    let agent = AgentSession::new_with_plugins(
        agent_session_builder(registry, Some(session), None, Some(temp.0.clone())),
        vec![Arc::new(RecordProviderStreamEventsPlugin::new(Arc::clone(
            &seen,
        )))],
    );

    agent
        .prompt("hi", Arc::new(|_event| {}))
        .await
        .expect("prompt");

    // The per-chunk hook fired with the upstream event shape
    // (sdk.ts:396-409): {type, data, provider, api, model}.
    let seen = seen.lock().unwrap();
    assert!(seen.len() >= 2, "both probe events dispatched: {seen:?}");
    for event in seen.iter() {
        assert_eq!(event["type"], serde_json::json!("provider_stream_event"));
        assert_eq!(event["provider"], serde_json::json!("fake"));
        assert_eq!(event["api"], serde_json::json!("fake-api"));
        assert_eq!(event["model"], serde_json::json!("fake-model"));
        assert!(event["data"].is_object(), "data rides the event: {event}");
    }
    assert_eq!(seen[0]["data"]["type"], serde_json::json!("message_start"));
    assert_eq!(seen[1]["data"]["type"], serde_json::json!("text_delta"));
}

/// Upstream awaits `handleProviderStreamEvent` before consuming more of the
/// provider stream (sdk.ts:388-401). pir's sync hook blocks until the
/// dispatcher finished the handler dispatch, so the provider observes the
/// handler's side effect as soon as `hook(...)` returns — in order, even for a
/// slow handler (backpressure, no loss).
#[tokio::test]
async fn provider_stream_events_are_awaited_before_the_stream_continues() {
    use std::time::Duration;

    /// Records the handler-visible index immediately AFTER each hook call: an
    /// un-awaited (fire-and-forget) dispatch would leave it behind.
    struct AwaitProbeProvider {
        seen: Arc<Mutex<Vec<u64>>>,
        violations: Arc<Mutex<Vec<String>>>,
    }

    impl Provider for AwaitProbeProvider {
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
            options: StreamOptions,
        ) -> AssistantMessageEventStream {
            let stream = AssistantMessageEventStream::new();
            if let Some(hook) = &options.hooks.on_provider_stream_event {
                for index in 0..4u64 {
                    hook(&serde_json::json!({ "type": "delta", "index": index }));
                    let last = self.seen.lock().unwrap().last().copied();
                    if last != Some(index) {
                        self.violations
                            .lock()
                            .unwrap()
                            .push(format!("after hook({index}) the handler had seen {last:?}"));
                    }
                }
            }
            let message = assistant(vec![text("ok")], StopReason::Stop);
            stream.push(AssistantMessageEventStream::done_event(
                TerminalReason::Stop,
                message,
            ));
            stream
        }
    }

    struct SlowProviderEventPlugin {
        info: PluginInfo,
        seen: Arc<Mutex<Vec<u64>>>,
    }

    impl AgentPlugin for SlowProviderEventPlugin {
        fn info(&self) -> &PluginInfo {
            &self.info
        }
        fn events(&self) -> EventInterest {
            EventInterest::List(vec!["provider_stream_event".into()])
        }
        fn on_event<'a>(&'a self, event: PluginEvent<'a>) -> PluginFuture<'a, PluginEventOutcome> {
            let seen = Arc::clone(&self.seen);
            Box::pin(async move {
                if event.name == "provider_stream_event" {
                    let index = event.payload["data"]["index"].as_u64().unwrap_or_default();
                    // Slow handler: the awaited dispatch makes the provider
                    // pay this delay instead of queueing events.
                    tokio::time::sleep(Duration::from_millis(5)).await;
                    seen.lock().unwrap().push(index);
                }
                PluginEventOutcome::None
            })
        }
    }

    let temp = TempDir::new("provider-stream-awaited");
    let seen: Arc<Mutex<Vec<u64>>> = Arc::new(Mutex::new(Vec::new()));
    let violations: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let mut registry = ProviderRegistry::default();
    registry.set_provider(Arc::new(AwaitProbeProvider {
        seen: Arc::clone(&seen),
        violations: Arc::clone(&violations),
    }));
    let agent = AgentSession::new_with_plugins(
        agent_session_builder(Arc::new(registry), None, None, Some(temp.0.clone())),
        vec![Arc::new(SlowProviderEventPlugin {
            info: PluginInfo::inline("slow-provider-stream"),
            seen: Arc::clone(&seen),
        }) as Arc<dyn AgentPlugin>],
    );

    agent
        .prompt("hi", Arc::new(|_event| {}))
        .await
        .expect("prompt");

    assert!(
        violations.lock().unwrap().is_empty(),
        "the hook returned before the handler dispatch completed: {:?}",
        violations.lock().unwrap()
    );
    assert_eq!(
        *seen.lock().unwrap(),
        vec![0, 1, 2, 3],
        "every event reaches the handler exactly once, in order"
    );
}

/// Abort cleanup: once the run's signal fires, later parsed provider events
/// are dropped before dispatch, so the handler never observes them and the
/// hook does not wait for a dispatch that will not happen.
#[tokio::test]
async fn provider_stream_events_after_abort_are_not_dispatched() {
    struct AbortProbeProvider {
        seen: Arc<Mutex<Vec<u64>>>,
        violations: Arc<Mutex<Vec<String>>>,
    }

    impl Provider for AbortProbeProvider {
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
            options: StreamOptions,
        ) -> AssistantMessageEventStream {
            let stream = AssistantMessageEventStream::new();
            if let Some(hook) = &options.hooks.on_provider_stream_event {
                hook(&serde_json::json!({ "type": "delta", "index": 0 }));
                if let Some(signal) = &options.signal {
                    signal.abort();
                }
                for index in 1..=3u64 {
                    hook(&serde_json::json!({ "type": "delta", "index": index }));
                    let last = self.seen.lock().unwrap().last().copied();
                    if last != Some(0) {
                        self.violations.lock().unwrap().push(format!(
                            "post-abort hook({index}) dispatched; handler last saw {last:?}"
                        ));
                    }
                }
            }
            let message = assistant(vec![text("ok")], StopReason::Aborted);
            stream.push(AssistantMessageEventStream::done_event(
                TerminalReason::Stop,
                message,
            ));
            stream
        }
    }

    /// Records the emitted index in the shared `seen` vec.
    struct IndexRecordPlugin {
        info: PluginInfo,
        seen: Arc<Mutex<Vec<u64>>>,
    }

    impl AgentPlugin for IndexRecordPlugin {
        fn info(&self) -> &PluginInfo {
            &self.info
        }
        fn events(&self) -> EventInterest {
            EventInterest::List(vec!["provider_stream_event".into()])
        }
        fn on_event<'a>(&'a self, event: PluginEvent<'a>) -> PluginFuture<'a, PluginEventOutcome> {
            let seen = Arc::clone(&self.seen);
            Box::pin(async move {
                if event.name == "provider_stream_event" {
                    let index = event.payload["data"]["index"].as_u64().unwrap_or_default();
                    seen.lock().unwrap().push(index);
                }
                PluginEventOutcome::None
            })
        }
    }

    let temp = TempDir::new("provider-stream-abort");
    let seen: Arc<Mutex<Vec<u64>>> = Arc::new(Mutex::new(Vec::new()));
    let violations: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let mut registry = ProviderRegistry::default();
    registry.set_provider(Arc::new(AbortProbeProvider {
        seen: Arc::clone(&seen),
        violations: Arc::clone(&violations),
    }));
    let agent = AgentSession::new_with_plugins(
        agent_session_builder(Arc::new(registry), None, None, Some(temp.0.clone())),
        vec![Arc::new(IndexRecordPlugin {
            info: PluginInfo::inline("index-record"),
            seen: Arc::clone(&seen),
        }) as Arc<dyn AgentPlugin>],
    );
    // The aborted run may surface as an error; the dispatch contract is the
    // subject of this test.
    let _ = agent.prompt("hi", Arc::new(|_event| {})).await;

    assert!(
        violations.lock().unwrap().is_empty(),
        "{:?}",
        violations.lock().unwrap()
    );
    assert_eq!(
        *seen.lock().unwrap(),
        vec![0],
        "only the pre-abort event reaches the handler"
    );
}

// ---------------------------------------------------------------------------
// RPC wire level: session events stream BARE on stdout
// ---------------------------------------------------------------------------

struct RpcClient {
    writer: tokio::sync::mpsc::UnboundedSender<String>,
    reader: tokio::sync::mpsc::UnboundedReceiver<String>,
}

impl RpcClient {
    fn send(&self, value: serde_json::Value) {
        self.writer
            .send(serde_json::to_string(&value).expect("serialize frame"))
            .expect("send command");
    }

    async fn next_line(&mut self) -> serde_json::Value {
        let line = tokio::time::timeout(std::time::Duration::from_secs(15), self.reader.recv())
            .await
            .expect("rpc output line within timeout")
            .expect("writer task alive");
        serde_json::from_str(&line).expect("valid json line")
    }

    async fn wait_for(
        &mut self,
        predicate: impl Fn(&serde_json::Value) -> bool,
    ) -> serde_json::Value {
        for _ in 0..1000 {
            let value = self.next_line().await;
            if predicate(&value) {
                return value;
            }
        }
        panic!("expected protocol line never arrived");
    }
}

async fn rpc_pair(label: &str, factory: SessionFactory) -> (RpcClient, TempDir) {
    let temp = TempDir::new(label);
    let server = RpcServer::new(temp.0.clone(), factory);
    let (client_stream, server_stream) = tokio::io::duplex(64 * 1024);
    let (server_read, server_write) = tokio::io::split(server_stream);
    tokio::spawn(async move { server.serve(server_read, server_write).await });

    let (client_read, mut client_write) = tokio::io::split(client_stream);
    let (line_tx, line_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    tokio::spawn(async move {
        let mut lines = BufReader::new(client_read).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            if line_tx.send(line).is_err() {
                break;
            }
        }
    });
    let (write_tx, mut write_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    tokio::spawn(async move {
        while let Some(mut line) = write_rx.recv().await {
            line.push('\n');
            if client_write.write_all(line.as_bytes()).await.is_err() {
                break;
            }
            let _ = client_write.flush().await;
        }
    });
    (
        RpcClient {
            writer: write_tx,
            reader: line_rx,
        },
        temp,
    )
}

fn rpc_factory() -> SessionFactory {
    Arc::new(|session: Option<SessionManager>| {
        let registry = fake_registry(Arc::new(|_index, _context| {
            assistant(vec![text("rpc reply")], StopReason::Stop)
        }));
        Ok(Arc::new(AgentSession::new(agent_session_builder(
            registry, session, None, None,
        ))))
    })
}

#[tokio::test]
async fn rpc_bash_streams_bash_execution_update_frames() {
    let (mut client, temp) = rpc_pair("rpc-bash-stream", rpc_factory()).await;
    client.send(serde_json::json!({ "type": "create" }));
    let _ = client
        .wait_for(|value| value["command"] == "create" && value["success"] == true)
        .await;

    // The command cannot finish before the gate file exists, and the gate is
    // only created after an update carrying FIRST was received. That proves
    // the output is streamed while the command is still running without
    // depending on pipe chunk boundaries or a fixed sleep. `/usr/bin/printf`
    // is used instead of the shell builtin so the FIRST write is flushed by
    // process exit even when stdout is a pipe (a builtin may block-buffer).
    let gate = temp.0.join("bash-stream.gate");
    let command = format!(
        "/usr/bin/printf FIRST; while [ ! -f '{}' ]; do sleep 0.02; done; /usr/bin/printf SECOND",
        gate.to_string_lossy().replace('\\', "/")
    );
    // The RPC request id rides the streamed frames (upstream rpc-mode.ts:
    // 577-586 passes it as executeBash's options.id).
    client.send(serde_json::json!({
        "id": "b-1",
        "type": "bash",
        "command": command,
    }));
    let mut updates: Vec<serde_json::Value> = Vec::new();
    loop {
        let line = client.next_line().await;
        if line["type"] == serde_json::json!("bash_execution_update") {
            assert_eq!(line["id"], serde_json::json!("b-1"));
            updates.push(line);
            let joined: String = updates
                .iter()
                .filter_map(|update| update["delta"].as_str())
                .collect();
            if joined.contains("FIRST") {
                break;
            }
            continue;
        }
        panic!("the bash response arrived before the first streamed update: {line}");
    }
    std::fs::write(&gate, b"go").expect("release the bash gate");

    // Updates and the command response travel over independent bridges, so
    // the response may overtake the final chunk. Collect both instead of
    // assuming every update precedes the response.
    let mut saw_response = false;
    for _ in 0..1_000 {
        let line = client.next_line().await;
        if line["type"] == serde_json::json!("bash_execution_update") {
            assert_eq!(line["id"], serde_json::json!("b-1"));
            updates.push(line);
        } else {
            assert_eq!(
                line["command"],
                serde_json::json!("bash"),
                "unexpected line: {line}"
            );
            saw_response = true;
        }
        let joined: String = updates
            .iter()
            .filter_map(|update| update["delta"].as_str())
            .collect();
        if saw_response && joined.contains("SECOND") {
            break;
        }
    }
    assert!(saw_response, "the bash response never arrived: {updates:?}");
    let joined: String = updates
        .iter()
        .filter_map(|update| update["delta"].as_str())
        .collect();
    assert!(
        joined.contains("FIRST") && joined.contains("SECOND"),
        "{joined}"
    );
}

#[tokio::test]
async fn rpc_steer_and_clear_queue_stream_queue_update_frames() {
    let (mut client, _temp) = rpc_pair("rpc-queue-update", rpc_factory()).await;
    client.send(serde_json::json!({ "type": "create" }));
    let _ = client
        .wait_for(|value| value["command"] == "create" && value["success"] == true)
        .await;

    client.send(serde_json::json!({ "id": "s-1", "type": "steer", "text": "queued line" }));

    // The queue_update session event streams BARE (not wrapped in a
    // response frame), upstream `output(toJsonEvent(event))`; the frame and
    // the steer response arrive on independent bridges, so collect until
    // BOTH are seen.
    let mut saw_update = false;
    let mut saw_response = false;
    for _ in 0..20 {
        let line = client.next_line().await;
        if line["type"] == serde_json::json!("queue_update") {
            assert_eq!(
                line,
                serde_json::json!({
                    "type": "queue_update",
                    "steering": ["queued line"],
                    "followUp": [],
                })
            );
            saw_update = true;
        } else if line["command"] == serde_json::json!("steer")
            && line["id"] == serde_json::json!("s-1")
        {
            assert_eq!(line["data"]["disposition"], serde_json::json!("handled"));
            saw_response = true;
        }
        if saw_update && saw_response {
            break;
        }
    }
    assert!(saw_update && saw_response, "both frames must arrive");

    client.send(serde_json::json!({ "id": "cq-1", "type": "clear_queue" }));
    let mut saw_emptied = false;
    let mut saw_cleared = false;
    for _ in 0..20 {
        let line = client.next_line().await;
        if line["type"] == serde_json::json!("queue_update")
            && line["steering"] == serde_json::json!([])
            && line["followUp"] == serde_json::json!([])
        {
            saw_emptied = true;
        } else if line["command"] == serde_json::json!("clear_queue")
            && line["id"] == serde_json::json!("cq-1")
        {
            assert_eq!(line["success"], serde_json::json!(true));
            saw_cleared = true;
        }
        if saw_emptied && saw_cleared {
            break;
        }
    }
    assert!(saw_emptied && saw_cleared, "both frames must arrive");
}

#[tokio::test]
#[cfg_attr(
    target_os = "linux",
    ignore = "single-worker RPC timing race on shared runner"
)]
async fn rpc_fork_reports_selected_text_and_position_semantics() {
    let (mut client, _temp) = rpc_pair("rpc-fork", rpc_factory()).await;
    client.send(serde_json::json!({ "type": "create" }));
    let _ = client
        .wait_for(|value| value["command"] == "create" && value["success"] == true)
        .await;
    // Materialize two user messages.
    for text in ["first rpc message", "second rpc message"] {
        client.send(serde_json::json!({ "id": "p", "type": "prompt", "text": text }));
        let _ = client.wait_for(|value| value["command"] == "prompt").await;
        // `agent_end` is emitted before the session drops its streaming flag;
        // a prompt sent in between is correctly rejected as "agent is
        // streaming". Wait for the settled event, which is the idle edge.
        let _ = client
            .wait_for(|value| value["type"] == "agent_settled")
            .await;
    }
    client.send(serde_json::json!({ "type": "get_entries" }));
    let entries = client
        .wait_for(|value| value["command"] == "get_entries")
        .await;
    let entry_ids: Vec<String> = entries["data"]["entries"]
        .as_array()
        .expect("entries")
        .iter()
        .filter(|entry| {
            entry.get("message").is_some() && entry["message"]["role"] == serde_json::json!("user")
        })
        .filter_map(|entry| entry["id"].as_str().map(str::to_owned))
        .collect();
    assert!(entry_ids.len() >= 2, "{entries}");
    let second_entry = entry_ids[1].clone();

    // Upstream rpc fork: position "before" — the user text comes back and
    // the branch lands on the PARENT.
    client.send(serde_json::json!({ "id": "fk-1", "type": "fork", "entryId": second_entry }));
    let fork = client
        .wait_for(|value| value["command"] == "fork" && value["id"] == "fk-1")
        .await;
    assert_eq!(fork["success"], serde_json::json!(true));
    assert_eq!(fork["data"]["cancelled"], serde_json::json!(false));
    assert_eq!(
        fork["data"]["text"],
        serde_json::json!("second rpc message")
    );

    // `position: "at"` branches at the entry itself (the pir
    // `runtimeHost.fork(entryId, {position})` surface): no text field
    // (upstream selectedText is undefined).
    client.send(
        serde_json::json!({ "id": "fk-2", "type": "fork", "entryId": second_entry, "position": "at" }),
    );
    let fork = client
        .wait_for(|value| value["command"] == "fork" && value["id"] == "fk-2")
        .await;
    assert_eq!(fork["success"], serde_json::json!(true));
    assert_eq!(fork["data"]["cancelled"], serde_json::json!(false));
    assert!(fork["data"].get("text").is_none(), "{fork}");
}

#[tokio::test]
async fn rpc_get_state_reports_is_compacting() {
    let (mut client, _temp) = rpc_pair("rpc-is-compacting", rpc_factory()).await;
    client.send(serde_json::json!({ "type": "create" }));
    let _ = client
        .wait_for(|value| value["command"] == "create" && value["success"] == true)
        .await;
    client.send(serde_json::json!({ "type": "get_state" }));
    let state = client
        .wait_for(|value| value["command"] == "get_state")
        .await;
    assert_eq!(state["data"]["isCompacting"], serde_json::json!(false));
    assert_eq!(
        state["data"]["isStreaming"],
        serde_json::json!(false),
        "the wire keeps the upstream camelCase state shape"
    );
}
