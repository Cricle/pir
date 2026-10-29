//! P0/P1 SDK fix lane: boxing, secret redaction, agent-dir isolation, and the
//! prompt-stream sink admission race.
//!
//! Every runtime test is hermetic: the model is a scripted in-process
//! [`Provider`] (no network, no `~/.pi` configuration) and temp directories
//! are removed on drop.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use futures_util::StreamExt;

use pir::ai::auth::ApiKeyAuth;
use pir::ai::event_stream::AssistantMessageEventStream;
use pir::ai::provider::{Provider, ProviderRegistry};
use pir::ai::types::{
    AssistantMessage, AssistantMessageEvent, ContentBlock, Context, Model, StopReason,
    StreamOptions, TerminalReason, TextContent, ThinkingLevel, Usage,
};
use pir::sdk::{
    AgentBuilder, ApiKind, CreateAgentSessionOptions, Endpoint, ModelRegistry, SdkEvent,
};

// ---------------------------------------------------------------------------
// Scripted provider (same shape as tests/sdk_extended.rs, kept standalone)
// ---------------------------------------------------------------------------

struct ScriptedProvider {
    calls: AtomicUsize,
    answer: String,
}

impl Provider for ScriptedProvider {
    fn id(&self) -> &str {
        "scripted"
    }

    fn name(&self) -> &str {
        "Scripted provider"
    }

    fn get_models(&self) -> Vec<Model> {
        vec![scripted_model()]
    }

    fn api_key_auth(&self) -> Vec<ApiKeyAuth> {
        // `with_registry` streams unchecked, so no key source is required.
        Vec::new()
    }

    fn stream(
        &self,
        _model: &Model,
        _context: Context,
        _options: StreamOptions,
    ) -> AssistantMessageEventStream {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let message = scripted_message(&self.answer);
        let stream = AssistantMessageEventStream::new();
        if let Some(ContentBlock::Text(text)) = message.content.first() {
            stream.push(AssistantMessageEvent::TextDelta {
                content_index: 0,
                delta: text.text.clone(),
                partial: Arc::new(message.clone()),
            });
        }
        stream.push(AssistantMessageEventStream::done_event(
            TerminalReason::Stop,
            message,
        ));
        stream
    }
}

fn scripted_model() -> Model {
    pir::ai::providers::construction::model(
        "scripted",
        "scripted-model",
        "Scripted model",
        "scripted-api",
        "http://127.0.0.1:1",
        8_192,
        512,
        vec!["text"],
    )
}

fn scripted_message(text: &str) -> AssistantMessage {
    AssistantMessage {
        api: "scripted".into(),
        provider: "scripted".into(),
        model: "scripted-model".into(),
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

async fn scripted_agent(answer: &str) -> pir::sdk::Agent {
    let provider = Arc::new(ScriptedProvider {
        calls: AtomicUsize::new(0),
        answer: answer.to_owned(),
    });
    let mut registry = ProviderRegistry::default();
    registry.set_provider(provider);
    AgentBuilder::new()
        .with_registry(Arc::new(registry), scripted_model())
        .no_session()
        .no_compaction()
        .thinking_level(ThinkingLevel::Off)
        .build()
        .await
        .expect("scripted agent builds")
}

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

/// Per-test temp directory removed on drop.
struct TempDir(PathBuf);

impl TempDir {
    fn new(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "pir-sdk-lane-f-{label}-{}-{}",
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

fn write_models_json(agent_dir: &Path, provider: &str, model_id: &str, api_key: &str) {
    let contents = format!(
        r#"{{
            "providers": {{
                "{provider}": {{
                    "name": "Lane F explicit provider",
                    "baseUrl": "http://127.0.0.1:1/v1",
                    "api": "openai-completions",
                    "apiKey": "{api_key}",
                    "model": "{model_id}",
                    "models": [{{
                        "id": "{model_id}",
                        "name": "{model_id}",
                        "api": "openai-completions",
                        "baseUrl": "http://127.0.0.1:1/v1",
                        "contextWindow": 4096,
                        "maxTokens": 512,
                        "input": ["text"]
                    }}]
                }}
            }}
        }}"#
    );
    std::fs::write(agent_dir.join("models.json"), contents).expect("write models.json");
}

// ---------------------------------------------------------------------------
// 1. Boxed prompt future
// ---------------------------------------------------------------------------

#[tokio::test]
async fn prompt_future_is_boxed_on_a_small_thread_stack() {
    let agent = scripted_agent("hello back").await;
    let moved = agent.clone();
    let handle = std::thread::Builder::new()
        .name("sdk-small-stack".to_owned())
        // Far below the 1 MiB Windows main-thread stack: constructing the
        // prompt future must not build the whole run state machine here.
        .stack_size(64 * 1024)
        .spawn(move || {
            let future = moved.prompt("hello");
            let size = std::mem::size_of_val(&future);
            assert!(
                size <= 2 * std::mem::size_of::<usize>() * 8,
                "Agent::prompt must return a boxed future, got {size} bytes"
            );
            drop(future);
        })
        .expect("spawn small-stack thread");
    handle.join().expect("small-stack thread must not overflow");
    agent.shutdown().await.expect("shutdown");
}

#[tokio::test]
async fn prompt_boxed_runs_through_the_prompt_path() {
    let agent = scripted_agent("boxed answer").await;
    let outcome = agent.prompt_boxed("hello").await.expect("prompt");
    assert_eq!(outcome.message_text().as_deref(), Some("boxed answer"));
    agent.shutdown().await.expect("shutdown");
}

// ---------------------------------------------------------------------------
// 2. Endpoint Debug redaction
// ---------------------------------------------------------------------------

#[test]
fn endpoint_debug_redacts_the_api_key() {
    let mut endpoint = Endpoint::new(
        "http://127.0.0.1:1/v1",
        ApiKind::OpenAiCompletions,
        "lane-f-model",
    );
    endpoint.api_key = Some("sk-lane-f-super-secret".to_owned());

    let debug = format!("{endpoint:?}");
    assert!(
        !debug.contains("sk-lane-f-super-secret"),
        "Debug must not print the API key: {debug}"
    );
    assert!(debug.contains("redacted"), "{debug}");
    assert!(debug.contains("lane-f-model"), "{debug}");
    assert!(debug.contains("http://127.0.0.1:1/v1"), "{debug}");

    endpoint.api_key = None;
    let debug_none = format!("{endpoint:?}");
    assert!(debug_none.contains("api_key: None"), "{debug_none}");
}

// ---------------------------------------------------------------------------
// 3. Explicit agent_dir isolation for the runtime model registry
// ---------------------------------------------------------------------------

#[tokio::test]
async fn explicit_agent_dir_drives_the_runtime_model_registry() {
    let cwd = TempDir::new("cwd");
    let agent_a = TempDir::new("agent-a");
    let agent_b = TempDir::new("agent-b");
    write_models_json(agent_a.path(), "lane-f-a", "model-a", "sk-lane-a");
    write_models_json(agent_b.path(), "lane-f-b", "model-b", "sk-lane-b");
    std::fs::write(
        agent_a.path().join("auth.json"),
        r#"{"lane-f-auth":{"type":"api_key","key":"sk-lane-f-auth"}}"#,
    )
    .expect("write auth.json");

    let registry_a = ModelRegistry::from_environment_in(agent_a.path(), cwd.path())
        .expect("explicit environment A");
    assert!(registry_a.get_model("lane-f-a", "model-a").is_some());
    assert!(registry_a.get_model("lane-f-b", "model-b").is_none());
    assert_eq!(
        registry_a.configured_api_key("lane-f-a").as_deref(),
        Some("sk-lane-a")
    );
    assert!(
        registry_a.has_configured_auth("lane-f-auth"),
        "auth.json must be read from the explicit agent directory"
    );

    let registry_b = ModelRegistry::from_environment_in(agent_b.path(), cwd.path())
        .expect("explicit environment B");
    assert!(registry_b.get_model("lane-f-b", "model-b").is_some());
    assert!(registry_b.get_model("lane-f-a", "model-a").is_none());
    assert_eq!(
        registry_b.configured_api_key("lane-f-b").as_deref(),
        Some("sk-lane-b")
    );
    assert!(
        !registry_b.has_configured_auth("lane-f-auth"),
        "directory B must not see directory A's stored credential"
    );

    // The runtime factory must use the explicit directory, not the process
    // environment: selecting A must expose A's provider and never B's.
    let services = pir::sdk::create_agent_session_services(CreateAgentSessionOptions {
        cwd: Some(cwd.path().to_path_buf()),
        agent_dir: Some(agent_a.path().to_path_buf()),
        no_session: true,
        ..CreateAgentSessionOptions::default()
    })
    .expect("services resolve");
    assert!(services
        .model_registry
        .get_model("lane-f-a", "model-a")
        .is_some());
    assert!(services
        .model_registry
        .get_model("lane-f-b", "model-b")
        .is_none());
}

// ---------------------------------------------------------------------------
// 4. Prompt-stream sink admission race
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn concurrent_prompt_stream_is_rejected_before_attaching() {
    let agent = scripted_agent("first answer").await;

    // Both calls are synchronous: the first claims the per-agent stream slot
    // before the spawned task can run, so the second must be rejected up front
    // instead of attaching the sink and racing a session prompt.
    let first = agent.prompt_stream("first");
    let second = agent.prompt_stream("second");

    let second_events: Vec<SdkEvent> = second.collect().await;
    assert_eq!(
        second_events.len(),
        2,
        "a concurrent stream gets exactly the terminal rejection pair: {second_events:?}"
    );
    match &second_events[0] {
        SdkEvent::PromptRejected { message } => {
            assert!(
                message.contains("already streaming"),
                "unexpected rejection: {message}"
            );
        }
        other => panic!("expected PromptRejected, got {other:?}"),
    }
    assert!(matches!(
        &second_events[1],
        SdkEvent::AgentSettled { aborted: false }
    ));

    // The winner keeps its sink and delivers its own lifecycle, settle once.
    let first_events: Vec<SdkEvent> = first.collect().await;
    assert!(
        !first_events
            .iter()
            .any(|event| matches!(event, SdkEvent::PromptRejected { .. })),
        "the winning stream must not be rejected: {first_events:?}"
    );
    assert!(first_events
        .iter()
        .any(|event| matches!(event, SdkEvent::TurnStart { .. })));
    assert!(first_events
        .iter()
        .any(|event| matches!(event, SdkEvent::MessageEnd { .. })));
    assert!(matches!(
        first_events.last(),
        Some(SdkEvent::AgentSettled { .. })
    ));

    // The guard releases admission after the terminal pair; a later stream is
    // admitted normally.
    for _ in 0..8 {
        tokio::task::yield_now().await;
    }
    let third_events: Vec<SdkEvent> = agent.prompt_stream("third").collect().await;
    assert!(
        !third_events
            .iter()
            .any(|event| matches!(event, SdkEvent::PromptRejected { .. })),
        "admission must be released when the previous run settles: {third_events:?}"
    );
    assert!(matches!(
        third_events.last(),
        Some(SdkEvent::AgentSettled { .. })
    ));

    agent.shutdown().await.expect("shutdown");
}
