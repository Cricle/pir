//! Tests for prompt-cache warming (`src/cache_warmer.rs` + the
//! `AgentSession` seam): warm requests fired after a successful response,
//! the `cache_warm` usage entry, no warm on error/abort, mode gating, and
//! idle-mode cancellation on a new prompt.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use pir::agent_session::{AgentSession, AgentSessionConfig};
use pir::ai::auth::ApiKeyAuth;
use pir::ai::event_stream::AssistantMessageEventStream;
use pir::ai::provider::{Provider, ProviderRegistry};
use pir::ai::types::*;
use pir::cache_warmer::{CacheWarmingSettings, DEFAULT_IDLE_DELAY};
use pir::config::settings::CacheWarmingMode;
use pir::plugins::{
    AgentPlugin, EventInterest, PluginEvent, PluginEventOutcome, PluginFuture, PluginInfo,
};
use pir::session_manager::{FileEntry, SessionManager};

// ---------------------------------------------------------------------------
// Fakes (same pattern as tests/agent_session.rs)
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

fn assistant_with_usage(
    content: Vec<ContentBlock>,
    stop_reason: StopReason,
    cache_write: u64,
) -> AssistantMessage {
    let mut message = AssistantMessage {
        api: "fake".into(),
        provider: "fake".into(),
        model: "fake-model".into(),
        content,
        stop_reason,
        timestamp: 1,
        ..Default::default()
    };
    message.usage = Usage {
        input: 10,
        output: 1,
        cache_read: 0,
        cache_write,
        cache_write_1h: None,
        reasoning: None,
        total_tokens: 11 + cache_write,
        cost: Cost::default(),
    };
    message
}

/// Recorded provider request: the message texts of the replayed context,
/// the stream options the request carried, and the abort signal it was
/// given.
#[derive(Debug, Clone)]
struct RecordedRequest {
    message_texts: Vec<String>,
    max_tokens: Option<u64>,
    signal: Option<pir::ai::types::AbortSignal>,
}

/// A fake provider that answers conversational turns with a cache-writing
/// assistant message and every later (warm) call with plain text, recording
/// each request.
struct RecordingProvider {
    calls: AtomicUsize,
    requests: Mutex<Vec<RecordedRequest>>,
    /// Stop reason of the FIRST (conversational) response.
    first_stop_reason: StopReason,
}

impl Provider for RecordingProvider {
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
        self.requests.lock().unwrap().push(RecordedRequest {
            message_texts: context
                .messages
                .iter()
                .map(|message| match message {
                    Message::User(user) => user
                        .content
                        .iter()
                        .filter_map(|block| match block {
                            ContentBlock::Text(text) => Some(text.text.clone()),
                            _ => None,
                        })
                        .collect::<Vec<_>>()
                        .join(" "),
                    Message::Assistant(assistant) => {
                        format!("assistant:{:?}", assistant.stop_reason)
                    }
                    Message::ToolResult(result) => format!("tool:{}", result.tool_name),
                    Message::System(_) | Message::Developer(_) => String::new(),
                })
                .collect(),
            max_tokens: options.max_tokens,
            signal: options.signal,
        });
        let message = if index == 0 {
            assistant_with_usage(
                vec![text("Hello!")],
                self.first_stop_reason,
                if matches!(self.first_stop_reason, StopReason::Stop) {
                    100
                } else {
                    0
                },
            )
        } else {
            // Later turns also report a cache write so every conversational
            // turn justifies its own warm request.
            assistant_with_usage(vec![text("warm")], StopReason::Stop, 100)
        };
        let stream = AssistantMessageEventStream::new();
        stream.push(AssistantMessageEventStream::done_event(
            TerminalReason::Stop,
            message,
        ));
        stream
    }
}

/// A provider whose warm responses (identified by the one-token output cap,
/// not the call index) block until a watch gate opens, so a test can observe
/// settle/warm ordering (conversational responses answer immediately).
struct GatedWarmProvider {
    calls: AtomicUsize,
    entered: Arc<AtomicUsize>,
    requests: Mutex<Vec<RecordedRequest>>,
    release: tokio::sync::watch::Receiver<bool>,
}

impl Provider for GatedWarmProvider {
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
        let is_warm = options.max_tokens == Some(1);
        let _ = self.calls.fetch_add(1, Ordering::SeqCst);
        self.requests.lock().unwrap().push(RecordedRequest {
            message_texts: context
                .messages
                .iter()
                .map(|message| match message {
                    Message::User(user) => user
                        .content
                        .iter()
                        .filter_map(|block| match block {
                            ContentBlock::Text(text) => Some(text.text.clone()),
                            _ => None,
                        })
                        .collect::<Vec<_>>()
                        .join(" "),
                    _ => String::new(),
                })
                .collect(),
            max_tokens: options.max_tokens,
            signal: options.signal,
        });
        let stream = AssistantMessageEventStream::new();
        if !is_warm {
            stream.push(AssistantMessageEventStream::done_event(
                TerminalReason::Stop,
                assistant_with_usage(vec![text("Hello!")], StopReason::Stop, 100),
            ));
            return stream;
        }
        self.entered.fetch_add(1, Ordering::SeqCst);
        let mut gate = self.release.clone();
        let pusher = stream.clone();
        tokio::spawn(async move {
            loop {
                if *gate.borrow() {
                    break;
                }
                if tokio::time::timeout(Duration::from_millis(20), gate.changed())
                    .await
                    .is_err()
                {
                    continue;
                }
            }
            pusher.push(AssistantMessageEventStream::done_event(
                TerminalReason::Stop,
                assistant_with_usage(vec![text("warm")], StopReason::Stop, 100),
            ));
        });
        stream
    }
}

/// A provider whose warm responses (identified by the one-token output cap)
/// block until the request's OWN abort signal fires (recording that they
/// observed it), so a test can assert an in-flight warm was actually aborted
/// (not merely skipped); conversational responses answer immediately.
struct AbortObservingProvider {
    calls: AtomicUsize,
    entered: Arc<AtomicUsize>,
    aborted: Arc<AtomicUsize>,
    requests: Mutex<Vec<RecordedRequest>>,
}

impl Provider for AbortObservingProvider {
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
        let is_warm = options.max_tokens == Some(1);
        let _ = self.calls.fetch_add(1, Ordering::SeqCst);
        let request_signal = options.signal.clone();
        self.requests.lock().unwrap().push(RecordedRequest {
            message_texts: context
                .messages
                .iter()
                .map(|message| match message {
                    Message::User(user) => user
                        .content
                        .iter()
                        .filter_map(|block| match block {
                            ContentBlock::Text(text) => Some(text.text.clone()),
                            _ => None,
                        })
                        .collect::<Vec<_>>()
                        .join(" "),
                    _ => String::new(),
                })
                .collect(),
            max_tokens: options.max_tokens,
            signal: options.signal,
        });
        let stream = AssistantMessageEventStream::new();
        if !is_warm {
            stream.push(AssistantMessageEventStream::done_event(
                TerminalReason::Stop,
                assistant_with_usage(vec![text("Hello!")], StopReason::Stop, 100),
            ));
            return stream;
        }
        self.entered.fetch_add(1, Ordering::SeqCst);
        let signal = request_signal;
        let aborted = Arc::clone(&self.aborted);
        let pusher = stream.clone();
        tokio::spawn(async move {
            loop {
                if signal.as_ref().is_some_and(|signal| signal.is_aborted()) {
                    aborted.fetch_add(1, Ordering::SeqCst);
                    pusher.push(AssistantMessageEventStream::done_event(
                        TerminalReason::Stop,
                        assistant_with_usage(vec![text("warm")], StopReason::Aborted, 100),
                    ));
                    return;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        });
        stream
    }
}

fn recording_registry(
    first_stop_reason: StopReason,
) -> (Arc<ProviderRegistry>, Arc<RecordingProvider>) {
    let provider = Arc::new(RecordingProvider {
        calls: AtomicUsize::new(0),
        requests: Mutex::new(Vec::new()),
        first_stop_reason,
    });
    let mut registry = ProviderRegistry::default();
    registry.set_provider(Arc::clone(&provider) as Arc<dyn Provider>);
    (Arc::new(registry), provider)
}

/// Per-test temp session dir.
struct TempDir(std::path::PathBuf);

impl TempDir {
    fn new(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "pir-cache-warmer-{label}-{}-{}",
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

fn session_with_warming(
    registry: Arc<ProviderRegistry>,
    temp: &TempDir,
    warming: Option<CacheWarmingSettings>,
) -> (Arc<AgentSession>, std::path::PathBuf) {
    session_with_warming_model(registry, temp, warming, fake_model(), ThinkingLevel::Off)
}

fn session_with_warming_model(
    registry: Arc<ProviderRegistry>,
    temp: &TempDir,
    warming: Option<CacheWarmingSettings>,
    model: Model,
    thinking: ThinkingLevel,
) -> (Arc<AgentSession>, std::path::PathBuf) {
    let session = SessionManager::create(".", &temp.0).expect("create session");
    let path = session.path().to_path_buf();
    let agent = Arc::new(AgentSession::new(AgentSessionConfig {
        provider_registry: registry,
        model,
        api_key: None,
        thinking,
        tools: pir::agent::tool_execution::ToolRegistry::new(),
        session: Some(session),
        compaction: None,
        max_turns: 4,
        system_prompt: Some("warm test system prompt".into()),
        cwd: Some(temp.0.clone()),
        event_stream: None,
        extensions: None,
        tool_selection: None,
    }));
    agent.set_cache_warming(warming);
    (agent, path)
}

fn usage_kinds(path: &std::path::Path) -> Vec<(String, String)> {
    let session = SessionManager::open(path).expect("open session");
    session
        .entries()
        .into_iter()
        .filter_map(|entry| match entry {
            FileEntry::Usage(usage) => Some((usage.kind, usage.model)),
            _ => None,
        })
        .collect()
}

fn recorded(requests: &Mutex<Vec<RecordedRequest>>) -> Vec<RecordedRequest> {
    requests.lock().unwrap().clone()
}

// ---------------------------------------------------------------------------
// Streaming mode
// ---------------------------------------------------------------------------

#[tokio::test]
async fn streaming_mode_fires_one_warm_request_after_response() {
    let temp = TempDir::new("streaming");
    let (registry, provider) = recording_registry(StopReason::Stop);
    let (agent, session_path) = session_with_warming(
        Arc::clone(&registry),
        &temp,
        Some(CacheWarmingSettings::new(CacheWarmingMode::Streaming)),
    );

    let terminal = agent
        .prompt("hi there", Arc::new(|_| {}))
        .await
        .expect("prompt");
    assert_eq!(terminal.stop_reason, StopReason::Stop);

    // The warm request fires from a DETACHED task: poll for it (prompt()
    // never waits on warming).
    let mut requests = recorded(&provider.requests);
    for _ in 0..100 {
        if requests.len() >= 2 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
        requests = recorded(&provider.requests);
    }
    assert_eq!(requests.len(), 2, "requests: {requests:?}");
    // The warm request replays the projected conversation (user + assistant)
    // with the same model and a one-token output cap.
    assert_eq!(requests[1].max_tokens, Some(1));
    assert!(requests[1]
        .message_texts
        .iter()
        .any(|text| text == "hi there"));

    // The warm usage is recorded as a session usage entry.
    let mut kinds = usage_kinds(&session_path);
    for _ in 0..100 {
        if !kinds.is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
        kinds = usage_kinds(&session_path);
    }
    assert_eq!(kinds.len(), 1, "kinds: {kinds:?}");
    assert_eq!(kinds[0].0, "cache_warm");
    assert_eq!(kinds[0].1, "fake-model");
}

#[tokio::test]
async fn no_warm_on_error_response() {
    let temp = TempDir::new("error");
    let (registry, provider) = recording_registry(StopReason::Error);
    let (agent, session_path) = session_with_warming(
        Arc::clone(&registry),
        &temp,
        Some(CacheWarmingSettings::new(CacheWarmingMode::Streaming)),
    );

    let terminal = agent.prompt("hi", Arc::new(|_| {})).await.expect("prompt");
    assert_eq!(terminal.stop_reason, StopReason::Error);

    // Only the conversational request ran; no usage entry was recorded.
    assert_eq!(recorded(&provider.requests).len(), 1);
    assert!(usage_kinds(&session_path).is_empty());
}

#[tokio::test]
async fn no_warm_on_abort() {
    let temp = TempDir::new("abort");
    let (registry, provider) = recording_registry(StopReason::Stop);
    let (agent, session_path) = session_with_warming(
        Arc::clone(&registry),
        &temp,
        Some(CacheWarmingSettings::new(CacheWarmingMode::Streaming)),
    );

    // Abort the run while it streams: the settled run is aborted, so no warm
    // request may fire (the abort races the first provider call — either the
    // call never happens or it happens exactly once and stays unanswered).
    let aborting = Arc::clone(&agent);
    let _ = agent
        .prompt(
            "hi",
            Arc::new(move |_event| {
                aborting.abort();
            }),
        )
        .await;
    assert!(recorded(&provider.requests).len() <= 1);
    assert!(usage_kinds(&session_path).is_empty());
}

#[tokio::test]
async fn no_warm_without_cacheable_usage() {
    // The gate refuses responses whose usage reports no cacheable input
    // (no cache write and no cache read): the session seam would skip the
    // warm request for such a turn.
    let no_cache = assistant_with_usage(vec![text("x")], StopReason::Stop, 0);
    assert!(!pir::cache_warmer::should_warm(&no_cache, false));
    let cache_write = assistant_with_usage(vec![text("x")], StopReason::Stop, 100);
    assert!(pir::cache_warmer::should_warm(&cache_write, false));
}

#[tokio::test]
async fn warming_off_by_default_and_off_mode_never_warms() {
    let temp = TempDir::new("off");
    let (registry, provider) = recording_registry(StopReason::Stop);
    // No settings installed (direct SDK default): no warm.
    let (agent, session_path) = session_with_warming(Arc::clone(&registry), &temp, None);
    agent.prompt("hi", Arc::new(|_| {})).await.expect("prompt");
    assert_eq!(recorded(&provider.requests).len(), 1);
    assert!(usage_kinds(&session_path).is_empty());

    // Explicit "off" behaves the same.
    let (registry, provider) = recording_registry(StopReason::Stop);
    let (agent, session_path) = session_with_warming(
        Arc::clone(&registry),
        &temp,
        Some(CacheWarmingSettings::new(CacheWarmingMode::Off)),
    );
    agent.prompt("hi", Arc::new(|_| {})).await.expect("prompt");
    assert_eq!(recorded(&provider.requests).len(), 1);
    assert!(usage_kinds(&session_path).is_empty());
}

// ---------------------------------------------------------------------------
// Idle mode
// ---------------------------------------------------------------------------

#[tokio::test]
async fn idle_mode_warms_after_idle_delay() {
    let temp = TempDir::new("idle");
    let (registry, provider) = recording_registry(StopReason::Stop);
    let (agent, session_path) = session_with_warming(
        Arc::clone(&registry),
        &temp,
        Some(CacheWarmingSettings {
            mode: CacheWarmingMode::Idle,
            idle_delay: Duration::from_millis(100),
        }),
    );

    agent.prompt("hi", Arc::new(|_| {})).await.expect("prompt");
    // Nothing warm while the run just settled and the delay has not passed.
    assert_eq!(recorded(&provider.requests).len(), 1);
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(recorded(&provider.requests).len(), 2);
    assert_eq!(recorded(&provider.requests)[1].max_tokens, Some(1));
    assert_eq!(usage_kinds(&session_path).len(), 1);
}

#[tokio::test]
async fn idle_warm_is_cancelled_by_a_new_prompt() {
    let temp = TempDir::new("idle-cancel");
    let (registry, provider) = recording_registry(StopReason::Stop);
    let (agent, session_path) = session_with_warming(
        Arc::clone(&registry),
        &temp,
        Some(CacheWarmingSettings {
            mode: CacheWarmingMode::Idle,
            idle_delay: Duration::from_millis(150),
        }),
    );

    agent
        .prompt("first", Arc::new(|_| {}))
        .await
        .expect("prompt 1");
    // A new prompt before the idle delay: the first turn's warm task is
    // cancelled, the second turn schedules its own.
    agent
        .prompt("second", Arc::new(|_| {}))
        .await
        .expect("prompt 2");
    tokio::time::sleep(Duration::from_millis(700)).await;

    // 2 conversational + exactly 1 idle warm (from the second turn).
    let requests = recorded(&provider.requests);
    assert_eq!(requests.len(), 3, "requests: {requests:?}");
    assert_eq!(requests[2].max_tokens, Some(1));
    // The cancelled warm never ran: no duplicate warm entries.
    assert_eq!(usage_kinds(&session_path).len(), 1);
}

// ---------------------------------------------------------------------------
// Streaming mode: detached task (warming never gates settling)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn streaming_warm_does_not_delay_settle() {
    let temp = TempDir::new("streaming-detached");
    let (gate_tx, gate_rx) = tokio::sync::watch::channel(false);
    let provider = Arc::new(GatedWarmProvider {
        calls: AtomicUsize::new(0),
        entered: Arc::new(AtomicUsize::new(0)),
        requests: Mutex::new(Vec::new()),
        release: gate_rx.clone(),
    });
    let mut registry = ProviderRegistry::default();
    registry.set_provider(Arc::clone(&provider) as Arc<dyn Provider>);
    let (agent, session_path) = session_with_warming(
        Arc::new(registry),
        &temp,
        Some(CacheWarmingSettings::new(CacheWarmingMode::Streaming)),
    );

    let terminal = agent
        .prompt("hi there", Arc::new(|_| {}))
        .await
        .expect("prompt");
    assert_eq!(terminal.stop_reason, StopReason::Stop);

    // prompt() returned while the warm gate is still closed: settling never
    // waits on the warm request (upstream arms an unref'd timer).
    assert!(!*gate_rx.borrow(), "warm gate must be closed at settle");

    // The warm request was issued (and is now parked on the gate).
    let mut entered = false;
    for _ in 0..150 {
        if provider.entered.load(Ordering::SeqCst) >= 1 {
            entered = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(entered, "warm request issued while the gate was closed");
    assert_eq!(recorded(&provider.requests).len(), 2);
    // The incomplete warm recorded no usage yet.
    assert!(usage_kinds(&session_path).is_empty());

    // Releasing the gate lets the warm complete and record its usage.
    gate_tx.send(true).expect("open warm gate");
    let mut kinds = Vec::new();
    for _ in 0..150 {
        kinds = usage_kinds(&session_path);
        if !kinds.is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(kinds.len(), 1, "kinds: {kinds:?}");
    assert_eq!(kinds[0].0, "cache_warm");
}

// ---------------------------------------------------------------------------
// Idle mode: in-flight warm is abortable
// ---------------------------------------------------------------------------

#[tokio::test]
async fn idle_warm_in_flight_is_aborted_by_a_new_prompt() {
    let temp = TempDir::new("idle-abort");
    let provider = Arc::new(AbortObservingProvider {
        calls: AtomicUsize::new(0),
        entered: Arc::new(AtomicUsize::new(0)),
        aborted: Arc::new(AtomicUsize::new(0)),
        requests: Mutex::new(Vec::new()),
    });
    let mut registry = ProviderRegistry::default();
    registry.set_provider(Arc::clone(&provider) as Arc<dyn Provider>);
    let (agent, session_path) = session_with_warming(
        Arc::new(registry),
        &temp,
        Some(CacheWarmingSettings {
            mode: CacheWarmingMode::Idle,
            idle_delay: Duration::from_millis(50),
        }),
    );

    agent
        .prompt("first", Arc::new(|_| {}))
        .await
        .expect("prompt 1");
    // Wait until the idle warm request is in flight (parked in the fake).
    let mut entered = false;
    for _ in 0..150 {
        if provider.entered.load(Ordering::SeqCst) >= 1 {
            entered = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(entered, "idle warm request in flight");

    // Disable further warming so the second prompt schedules nothing: the
    // only warm is the in-flight one.
    agent.set_cache_warming(None);
    agent
        .prompt("second", Arc::new(|_| {}))
        .await
        .expect("prompt 2");

    // The generation bump aborted the in-flight warm: the fake observed the
    // aborted signal (upstream aborts in-flight refreshes on new activity).
    let mut aborted = false;
    for _ in 0..150 {
        if provider.aborted.load(Ordering::SeqCst) >= 1 {
            aborted = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(aborted, "in-flight warm observed its abort signal");
    // The recorded warm request carries the aborted signal and recorded no
    // usage.
    let requests = recorded(&provider.requests);
    assert!(requests[1]
        .signal
        .as_ref()
        .is_some_and(|signal| signal.is_aborted()));
    assert!(usage_kinds(&session_path).is_empty());
}

// ---------------------------------------------------------------------------
// Replayability (upstream `isReplayable`)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn anthropic_budget_thinking_model_is_not_warmed() {
    let temp = TempDir::new("anthropic-thinking");
    let (registry, provider) = recording_registry(StopReason::Stop);
    let mut model = fake_model();
    model.api = "anthropic-messages".into();
    model.reasoning = true;
    let (agent, session_path) = session_with_warming_model(
        Arc::clone(&registry),
        &temp,
        Some(CacheWarmingSettings::new(CacheWarmingMode::Streaming)),
        model,
        ThinkingLevel::Medium,
    );

    agent
        .prompt("hi there", Arc::new(|_| {}))
        .await
        .expect("prompt");
    // Give any (forbidden) detached warm ample time to show up.
    tokio::time::sleep(Duration::from_millis(400)).await;
    // Only the conversational request ran; no warm usage was recorded.
    assert_eq!(recorded(&provider.requests).len(), 1);
    assert!(usage_kinds(&session_path).is_empty());
}

#[tokio::test]
async fn anthropic_adaptive_thinking_model_still_warms() {
    let temp = TempDir::new("anthropic-adaptive");
    let (registry, provider) = recording_registry(StopReason::Stop);
    let mut model = fake_model();
    model.api = "anthropic-messages".into();
    model.reasoning = true;
    model.compat = Some(OpenAiCompat {
        force_adaptive_thinking: Some(true),
        ..OpenAiCompat::default()
    });
    let (agent, session_path) = session_with_warming_model(
        Arc::clone(&registry),
        &temp,
        Some(CacheWarmingSettings::new(CacheWarmingMode::Streaming)),
        model,
        ThinkingLevel::Medium,
    );

    agent
        .prompt("hi there", Arc::new(|_| {}))
        .await
        .expect("prompt");
    // Adaptive thinking derives no per-request budget_tokens: the replay is
    // safe and the warm fires.
    let mut requests = recorded(&provider.requests);
    for _ in 0..100 {
        if requests.len() >= 2 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
        requests = recorded(&provider.requests);
    }
    assert_eq!(requests.len(), 2, "requests: {requests:?}");
    assert_eq!(requests[1].max_tokens, Some(1));
    let _ = session_path;
}

#[test]
fn replayability_gates_anthropic_budget_thinking() {
    let mut anthropic = fake_model();
    anthropic.api = "anthropic-messages".into();
    anthropic.reasoning = true;
    // Budget-based thinking derives budget_tokens from max_tokens: the
    // one-token replay would miss the cache key.
    assert!(!pir::cache_warmer::is_replayable(
        &anthropic,
        Some(ThinkingLevel::Medium)
    ));
    assert!(!pir::cache_warmer::is_replayable(
        &anthropic,
        Some(ThinkingLevel::High)
    ));
    // Adaptive thinking is replayable.
    let mut adaptive = anthropic.clone();
    adaptive.compat = Some(OpenAiCompat {
        force_adaptive_thinking: Some(true),
        ..OpenAiCompat::default()
    });
    assert!(pir::cache_warmer::is_replayable(
        &adaptive,
        Some(ThinkingLevel::Medium)
    ));
    // Non-anthropic APIs are always replayable.
    let mut openai = anthropic.clone();
    openai.api = "openai-completions".into();
    assert!(pir::cache_warmer::is_replayable(
        &openai,
        Some(ThinkingLevel::Medium)
    ));
    // No reasoning option is replayable.
    assert!(pir::cache_warmer::is_replayable(&anthropic, None));
}

// ---------------------------------------------------------------------------
// Unit-level gates
// ---------------------------------------------------------------------------

#[test]
fn should_warm_gates_on_stop_reason_and_signal() {
    let good = assistant_with_usage(vec![text("x")], StopReason::Stop, 100);
    assert!(pir::cache_warmer::should_warm(&good, false));
    assert!(!pir::cache_warmer::should_warm(&good, true));
    let errored = assistant_with_usage(vec![], StopReason::Error, 100);
    assert!(!pir::cache_warmer::should_warm(&errored, false));
    let aborted = assistant_with_usage(vec![], StopReason::Aborted, 100);
    assert!(!pir::cache_warmer::should_warm(&aborted, false));
}

#[test]
fn settings_consumer_resolves_the_mode() {
    // Per-test scratch root: the shared temp root could carry an ambient
    // `.pi/settings.json` written while another test runs.
    let scratch = std::env::temp_dir().join(format!(
        "pir-cache-warmer-settings-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&scratch).expect("create scratch dir");
    let manager = pir::config::settings::SettingsManager::load(&scratch, &scratch, &scratch, false);
    // Upstream default when the setting is unset: "streaming".
    assert_eq!(
        pir::cache_warmer::mode_from_settings(&manager),
        CacheWarmingMode::Streaming
    );
}

#[test]
fn default_idle_delay_is_bounded() {
    assert_eq!(DEFAULT_IDLE_DELAY, Duration::from_secs(60));
    assert!(DEFAULT_IDLE_DELAY < Duration::from_secs(3600));
}

// ---------------------------------------------------------------------------
// cache_warming_decision economics + cache-warm entry_appended
// ---------------------------------------------------------------------------

struct CacheEventRecorder {
    info: PluginInfo,
    events: Arc<Mutex<Vec<(String, serde_json::Value)>>>,
}

impl AgentPlugin for CacheEventRecorder {
    fn info(&self) -> &PluginInfo {
        &self.info
    }

    fn events(&self) -> EventInterest {
        EventInterest::List(vec![
            "cache_warming_decision".to_owned(),
            "entry_appended".to_owned(),
        ])
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
}

#[tokio::test]
async fn decision_payload_carries_economics_and_warm_usage_is_appended() {
    let temp = TempDir::new("economics");
    let (registry, _provider) = recording_registry(StopReason::Stop);
    // Non-zero catalog prices so the economics are observable.
    let mut model = fake_model();
    model.cost.rates = ModelCostRates {
        input: 3.0,
        output: 15.0,
        cache_read: 0.3,
        cache_write: 3.75,
    };
    let session = SessionManager::create(".", &temp.0).expect("create session");
    let events: Arc<Mutex<Vec<(String, serde_json::Value)>>> = Arc::new(Mutex::new(Vec::new()));
    let recorder = Arc::new(CacheEventRecorder {
        info: PluginInfo::inline("cache-economics-recorder"),
        events: Arc::clone(&events),
    });
    let agent = Arc::new(AgentSession::new_with_plugins(
        AgentSessionConfig {
            provider_registry: registry,
            model,
            api_key: None,
            thinking: ThinkingLevel::Off,
            tools: pir::agent::tool_execution::ToolRegistry::new(),
            session: Some(session),
            compaction: None,
            max_turns: 4,
            system_prompt: Some("warm test system prompt".into()),
            cwd: Some(temp.0.clone()),
            event_stream: None,
            extensions: None,
            tool_selection: None,
        },
        vec![recorder as Arc<dyn AgentPlugin>],
    ));
    agent.set_cache_warming(Some(CacheWarmingSettings::new(CacheWarmingMode::Streaming)));

    agent
        .prompt("hi there", Arc::new(|_| {}))
        .await
        .expect("prompt");

    let decision = {
        let events = events.lock().unwrap();
        events
            .iter()
            .find(|(name, _)| name == "cache_warming_decision")
            .map(|(_, payload)| payload.clone())
            .expect("cache_warming_decision dispatched")
    };
    // The terminal usage reports input=10, cacheWrite=100 => promptTokens=110.
    let cache_hit_cost = 0.3 * 110.0 / 1_000_000.0;
    let cache_miss_cost = 3.75 * 110.0 / 1_000_000.0;
    let expected_miss = cache_miss_cost - cache_hit_cost;
    let expected_warm = cache_hit_cost + 15.0 / 1_000_000.0;
    let warm_cost = decision["warmCost"].as_f64().expect("warmCost");
    let miss_cost = decision["missCost"].as_f64().expect("missCost");
    assert!(
        (warm_cost - expected_warm).abs() < 1e-12,
        "warmCost {warm_cost} != {expected_warm}: {decision}"
    );
    assert!(
        (miss_cost - expected_miss).abs() < 1e-12,
        "missCost {miss_cost} != {expected_miss}: {decision}"
    );
    assert_eq!(
        decision["continuationProbability"],
        serde_json::json!(1.0),
        "streaming mode warms while the request is the active run: {decision}"
    );
    assert_eq!(decision["action"], serde_json::json!("warm"), "{decision}");

    // The warm usage entry is persisted and then reported through
    // `entry_appended` (upstream `onWarmed(entry)`).
    let mut warm_entry = None;
    for _ in 0..100 {
        if let Some(entry) = events.lock().unwrap().iter().find_map(|(name, payload)| {
            if name != "entry_appended" {
                return None;
            }
            let entry = &payload["entry"];
            (entry["type"] == serde_json::json!("usage")
                && entry["kind"] == serde_json::json!("cache_warm"))
            .then(|| entry.clone())
        }) {
            warm_entry = Some(entry);
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let warm_entry = warm_entry.expect("entry_appended for the cache_warm usage entry");
    assert_eq!(
        warm_entry["model"],
        serde_json::json!("fake-model"),
        "{warm_entry}"
    );
    assert!(
        warm_entry["id"].as_str().is_some_and(|id| !id.is_empty()),
        "the event carries the persisted entry id: {warm_entry}"
    );
}
