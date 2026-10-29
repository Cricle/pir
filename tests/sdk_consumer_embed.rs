//! Consumer reality check for the embedded SDK (`pir::sdk`).
//!
//! Written the way an external embedder uses the crate: the public
//! `AgentBuilder`, a mock provider injected through the public `Provider`
//! trait (the same in-memory idiom as `tests/suites/sdk.rs`), one prompt run,
//! and the session file inspected as JSON. No network, no `~/.pi` state.
//!
//! The only feature-gated surface it uses is `sdk` itself (the facade under
//! test), so it compiles against the bare crate plus that one feature; the
//! runtime test (which asserts the on-disk JSONL transcript) additionally
//! requires `session` and is gated on it:
//!
//! ```text
//! cargo check --test sdk_consumer_embed                                   # default (full)
//! cargo check --no-default-features --features sdk --test sdk_consumer_embed
//! cargo test  --no-default-features --features sdk,session --test sdk_consumer_embed
//! ```
//!
//! If a refactor moves any of this consumer path behind another cargo
//! feature, the second command fails a consumer's build instead of their
//! runtime.

// The consumer-shape check (`sdk` only) compiles the runtime test's helpers
// without the `session`-gated test that uses them; keep that shape
// warning-free rather than hiding it behind more `cfg` attributes.
#![cfg_attr(not(feature = "session"), allow(dead_code, unused_imports))]

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use futures_util::StreamExt;

use pir::ai::auth::ApiKeyAuth;
use pir::ai::event_stream::AssistantMessageEventStream;
use pir::ai::provider::{Provider, ProviderRegistry};
use pir::ai::types::{
    AssistantMessage, AssistantMessageEvent, ContentBlock, Context, Message, Model, ModelCost,
    ModelCostRates, StopReason, StreamOptions, TerminalReason, TextContent, ThinkingLevel, Usage,
};
#[cfg(feature = "session")]
use pir::sdk::SessionMode;
use pir::sdk::{AgentBuilder, SdkEvent};

const PROMPT: &str = "summarize the consumer contract";
const REPLY: &str = "consumer turn ok";

// ---------------------------------------------------------------------------
// Mock provider (the `FakeProvider` idiom of `tests/suites/sdk.rs`, trimmed
// to what one consumer turn needs). It answers one request with a streamed
// text message.
// ---------------------------------------------------------------------------

struct MockProvider {
    calls: AtomicUsize,
}

fn mock_model() -> Model {
    Model {
        id: "mock-model".into(),
        name: "Mock model".into(),
        api: "mock-api".into(),
        provider: "mock".into(),
        base_url: "http://127.0.0.1:1".into(),
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
        context_window: 4_096,
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

fn assistant_message(text: &str) -> AssistantMessage {
    AssistantMessage {
        api: "mock".into(),
        provider: "mock".into(),
        model: "mock-model".into(),
        content: vec![ContentBlock::Text(TextContent {
            text: text.to_owned(),
            text_signature: None,
        })],
        stop_reason: StopReason::Stop,
        timestamp: 1,
        usage: Usage::default(),
        ..Default::default()
    }
}

fn assistant_text(message: &AssistantMessage) -> Option<String> {
    message.content.iter().find_map(|block| match block {
        ContentBlock::Text(text) => Some(text.text.clone()),
        _ => None,
    })
}

fn last_user_text(context: &Context) -> Option<String> {
    context
        .messages
        .iter()
        .rev()
        .find_map(|message| match message {
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
}

impl Provider for MockProvider {
    fn id(&self) -> &str {
        "mock"
    }

    fn name(&self) -> &str {
        "Mock provider"
    }

    fn get_models(&self) -> Vec<Model> {
        vec![mock_model()]
    }

    fn api_key_auth(&self) -> Vec<ApiKeyAuth> {
        vec![ApiKeyAuth::new("mock key", vec!["MOCK_TEST_KEY".into()])]
    }

    fn stream(
        &self,
        _model: &Model,
        context: Context,
        _options: StreamOptions,
    ) -> AssistantMessageEventStream {
        let call = self.calls.fetch_add(1, Ordering::SeqCst);
        assert_eq!(call, 0, "one consumer turn = exactly one model request");
        assert_eq!(
            last_user_text(&context).as_deref(),
            Some(PROMPT),
            "the embedder prompt must reach the provider unchanged"
        );
        let message = assistant_message(REPLY);
        let stream = AssistantMessageEventStream::new();
        // One text delta + a terminal event is all a provider must produce
        // for the SDK to observe MessageStart/Update/End.
        stream.push(AssistantMessageEvent::TextDelta {
            content_index: 0,
            delta: REPLY.to_owned(),
            partial: Arc::new(message.clone()),
        });
        stream.push(AssistantMessageEventStream::done_event(
            TerminalReason::Stop,
            message,
        ));
        stream
    }
}

/// Per-test temp session/cwd directory.
struct TempDir(PathBuf);

impl TempDir {
    fn new(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "pir-sdk-consumer-{label}-{}-{}",
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

// ---------------------------------------------------------------------------
// The consumer turn
// ---------------------------------------------------------------------------

// The runtime half needs the `session` feature: `SessionMode::Dir` is
// documented as a no-op without it (the public signature compiles, but no
// transcript file exists), while this test asserts the on-disk JSONL shape.
// Builds with only `sdk` still compile-check the consumer surface above.
#[cfg(feature = "session")]
#[tokio::test]
async fn consumer_builds_runs_one_turn_and_lands_upstream_session_entries() {
    let dir = TempDir::new("embed");
    let mut registry = ProviderRegistry::default();
    registry.set_provider(Arc::new(MockProvider {
        calls: AtomicUsize::new(0),
    }));

    let agent = AgentBuilder::new()
        .with_registry(Arc::new(registry), mock_model())
        .session(SessionMode::Dir(dir.0.clone()))
        .cwd(dir.0.clone())
        .no_compaction()
        .thinking_level(ThinkingLevel::Off)
        .build()
        .await
        .expect("an embedder can build the agent");

    // --- the typed event stream of one consumer turn ----------------------
    let events: Vec<SdkEvent> = agent.prompt_stream(PROMPT).collect::<Vec<_>>().await;
    let labels: Vec<&'static str> = events
        .iter()
        .map(|event| match event {
            SdkEvent::TurnStart { .. } => "turn_start",
            SdkEvent::MessageStart => "message_start",
            SdkEvent::MessageUpdate { .. } => "message_update",
            SdkEvent::MessageDone { .. } => "message_done",
            SdkEvent::MessageEnd { .. } => "message_end",
            SdkEvent::TranscriptMessageStart { .. } => "transcript_message_start",
            SdkEvent::TranscriptMessageEnd { .. } => "transcript_message_end",
            SdkEvent::TurnEnd { .. } => "turn_end",
            SdkEvent::AgentEnd { .. } => "agent_end",
            SdkEvent::EntryAppended { .. } => "entry_appended",
            SdkEvent::AgentSettled { .. } => "settled",
            other => panic!("unexpected event on a plain consumer turn: {other:?}"),
        })
        .collect();
    assert_eq!(
        labels,
        vec![
            "turn_start",
            "transcript_message_start",
            "transcript_message_end",
            "entry_appended",
            "message_start",
            "message_update",
            "message_done",
            "message_end",
            "entry_appended",
            "turn_end",
            "agent_end",
            "settled"
        ],
        "the consumer-visible contract of a tool-less single turn: the turn \
         starts before the starting user message's transcript lifecycle (H4b: \
         agent_start -> turn_start -> message_start/message_end), the user \
         entry is appended after its transcript pair, the assistant stream \
         ends with message_done/message_end, the assistant entry is appended \
         before turn_end, and the low-level run closes with agent_end before \
         the session settles (upstream `entry_appended`/`message_*`/`agent_end`)"
    );

    let user_start = events
        .iter()
        .find_map(|event| match event {
            SdkEvent::TranscriptMessageStart { message, .. } => Some(message),
            _ => None,
        })
        .expect("the starting user message lifecycle is visible");
    assert_eq!(user_start.role(), "user");
    match user_start {
        Message::User(user) => {
            assert_eq!(
                user.content
                    .iter()
                    .filter_map(|block| match block {
                        ContentBlock::Text(text) => Some(text.text.as_str()),
                        _ => None,
                    })
                    .collect::<String>(),
                PROMPT
            );
        }
        other => panic!("expected the user message, got {other:?}"),
    }
    let user_end = events
        .iter()
        .find_map(|event| match event {
            SdkEvent::TranscriptMessageEnd { message, .. } => Some(message),
            _ => None,
        })
        .expect("the starting user message lifecycle is closed");
    assert_eq!(user_end, user_start);

    let delta = events
        .iter()
        .find_map(|event| match event {
            SdkEvent::MessageUpdate { delta, thinking } => Some((delta.as_str(), *thinking)),
            _ => None,
        })
        .expect("a streaming text delta");
    assert_eq!(delta, (REPLY, false));

    let message_done = events
        .iter()
        .find_map(|event| match event {
            SdkEvent::MessageDone {
                reason, message, ..
            } => Some((*reason, message)),
            _ => None,
        })
        .expect("MessageDone carries the provider's terminal frame");
    assert_eq!(message_done.0, TerminalReason::Stop);
    assert_eq!(assistant_text(message_done.1).as_deref(), Some(REPLY));

    let message_end = events
        .iter()
        .find_map(|event| match event {
            SdkEvent::MessageEnd { message } => Some(message),
            _ => None,
        })
        .expect("MessageEnd carries the completed message");
    assert_eq!(assistant_text(message_end).as_deref(), Some(REPLY));

    // The per-run `agent_end` carries THIS run's produced messages (the
    // assistant reply; the starting user message is a preamble, not produced
    // by the run) and never claims a session retry.
    let (agent_end_messages, will_retry) = events
        .iter()
        .find_map(|event| match event {
            SdkEvent::AgentEnd {
                messages,
                will_retry,
            } => Some((messages, *will_retry)),
            _ => None,
        })
        .expect("agent_end is projected");
    assert!(
        !will_retry,
        "a successful one-turn run has no retry pending"
    );
    assert_eq!(agent_end_messages.len(), 1, "{agent_end_messages:?}");
    assert!(matches!(
        agent_end_messages.first(),
        Some(Message::Assistant(message)) if assistant_text(message).as_deref() == Some(REPLY)
    ));

    assert!(matches!(
        events.last(),
        Some(SdkEvent::AgentSettled { aborted: false })
    ));
    assert_eq!(agent.usage_totals().turns, 1);

    // --- the on-disk entry shape (upstream JSONL interop) -----------------
    let path = agent
        .session_path()
        .expect("session_path reports the file after the first persisted message");
    assert!(
        path.starts_with(&dir.0),
        "SessionMode::Dir is honoured: {path:?}"
    );
    let content = std::fs::read_to_string(&path).expect("read the session file");
    let lines: Vec<serde_json::Value> = content
        .lines()
        .map(|line| serde_json::from_str(line).expect("every line is one JSON entry"))
        .collect();
    assert!(
        lines.len() >= 3,
        "header + user + assistant at least: {lines:#?}"
    );

    // Line 1: the session header (upstream CURRENT_SESSION_VERSION = 3).
    assert_eq!(lines[0]["type"], "session");
    assert_eq!(lines[0]["version"], 3);
    assert_eq!(lines[0]["cwd"], dir.0.to_string_lossy().as_ref());

    // Entries form one linear append chain: the root has `parentId: null`,
    // every later entry parents to the previous entry's 8-hex id.
    let mut previous: Option<String> = None;
    for entry in &lines[1..] {
        let id = entry["id"].as_str().expect("entry id");
        assert_eq!(id.len(), 8, "upstream ids are 8 hex chars: {entry}");
        assert!(id.chars().all(|c| c.is_ascii_hexdigit()), "{entry}");
        match &previous {
            None => assert!(entry["parentId"].is_null(), "root entry: {entry}"),
            Some(parent) => assert_eq!(
                entry["parentId"],
                parent.as_str(),
                "the append chain must parent to the previous entry: {entry}"
            ),
        }
        assert!(
            entry["timestamp"]
                .as_str()
                .is_some_and(|timestamp| timestamp.ends_with('Z')),
            "ISO-8601 UTC timestamp: {entry}"
        );
        previous = Some(id.to_owned());
    }

    let user = lines[1..]
        .iter()
        .find(|entry| entry["type"] == "message" && entry["message"]["role"] == "user")
        .expect("the user message entry");
    assert_eq!(user["message"]["content"][0]["type"], "text");
    assert_eq!(user["message"]["content"][0]["text"], PROMPT);

    let assistant = lines[1..]
        .iter()
        .find(|entry| entry["type"] == "message" && entry["message"]["role"] == "assistant")
        .expect("the assistant message entry");
    assert_eq!(assistant["message"]["stopReason"], "stop");
    assert!(
        assistant["message"]["content"]
            .as_array()
            .is_some_and(|blocks| blocks
                .iter()
                .any(|block| block["type"] == "text" && block["text"] == REPLY)),
        "the assistant reply reaches disk: {assistant}"
    );

    agent.shutdown().await.expect("shutdown");
}
