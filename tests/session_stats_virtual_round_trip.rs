//! M10.4/M10.5: `session_stats` and the virtual-model restore/persist path
//! fold over BORROWED resident entries instead of cloning the whole session.
//!
//! This pins the behavior on a fixture that mixes every stats-bearing entry
//! with a `pi.virtual-model-state` custom entry: stats must count exactly the
//! messages/usage (the custom entry contributes nothing), and opening the
//! session must restore the registration from the custom entry (the
//! value-level round trip; the clone elision is not observable).

use std::sync::Arc;

use pir::agent::tool_execution::ToolRegistry;
use pir::agent_session::{AgentSession, AgentSessionConfig};
use pir::ai::auth::ApiKeyAuth;
use pir::ai::event_stream::AssistantMessageEventStream;
use pir::ai::provider::{Provider, ProviderRegistry};
use pir::ai::types::{
    AssistantMessage, ContentBlock, Context, Cost, Message, Model, ModelCost, ModelCostRates,
    StopReason, StreamOptions, TextContent, ThinkingLevel, ToolCall, Usage, UserMessage, UserRole,
};
use pir::session_manager::{FileEntry, SessionManager};

fn text(content: &str) -> ContentBlock {
    ContentBlock::Text(TextContent {
        text: content.into(),
        text_signature: None,
    })
}

fn user_message(content: &str) -> Message {
    Message::User(UserMessage {
        role: UserRole::User,
        content: vec![text(content)],
        tools_added: None,
        tools_removed: None,
        timestamp: 1,
        sections: None,
    })
}

fn assistant_message(usage: Usage) -> Message {
    Message::Assistant(AssistantMessage {
        api: "fake-api".into(),
        provider: "fake".into(),
        model: "fake-model".into(),
        content: vec![
            text("calling echo"),
            ContentBlock::ToolCall(ToolCall {
                id: "call-1".into(),
                name: "echo".into(),
                arguments: std::collections::BTreeMap::new(),
                thought_signature: None,
                namespace: None,
                arguments_error: None,
            }),
        ],
        stop_reason: StopReason::ToolUse,
        timestamp: 2,
        usage,
        ..AssistantMessage::default()
    })
}

fn usage(input: u64, output: u64, cache_read: u64, cache_write: u64, cost_total: f64) -> Usage {
    Usage {
        input,
        output,
        cache_read,
        cache_write,
        cache_write_1h: None,
        reasoning: None,
        total_tokens: input + output + cache_read + cache_write,
        cost: Cost {
            input: 0.0,
            output: 0.0,
            cache_read: 0.0,
            cache_write: 0.0,
            total: cost_total,
            ..Cost::default()
        },
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

    fn api_key_auth(&self) -> Vec<ApiKeyAuth> {
        vec![ApiKeyAuth::new("fake key", vec!["FAKE_TEST_KEY".into()])]
    }

    fn stream(
        &self,
        _model: &Model,
        _context: Context,
        _options: StreamOptions,
    ) -> AssistantMessageEventStream {
        let stream = AssistantMessageEventStream::new();
        stream.push(AssistantMessageEventStream::done_event(
            pir::ai::types::TerminalReason::Stop,
            AssistantMessage::default(),
        ));
        stream
    }
}

/// Per-test temp session/cwd directory.
struct TempDir(std::path::PathBuf);

impl TempDir {
    fn new(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "pir-m10-{label}-{}-{}",
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

#[tokio::test(flavor = "current_thread")]
async fn stats_ignore_custom_entries_and_virtual_state_restores() {
    let temp = TempDir::new("stats");
    let mut session = SessionManager::create(".", &temp.0).expect("create session");
    session
        .append_message(user_message("hello"))
        .expect("append user");
    // Assistant usage: 10/5/2/3, cost 0.5.
    session
        .append_message(assistant_message(usage(10, 5, 2, 3, 0.5)))
        .expect("append assistant");
    // A standalone usage entry: 1/1/1/1, cost 0.25.
    session
        .append_usage(
            "cache-warm",
            "fake",
            "fake-model",
            usage(1, 1, 1, 1, 0.25),
            None,
        )
        .expect("append usage");
    // Compaction usage: 2/2/2/2, cost 0.125.
    session
        .append_compaction_with_usage(
            "summary",
            None,
            100,
            None,
            Some(usage(2, 2, 2, 2, 0.125)),
            false,
        )
        .expect("append compaction");
    // Branch-summary usage: 3/3/3/3, cost 0.125.
    session
        .branch_with_summary(
            None,
            "branch summary",
            None,
            None,
            Some(usage(3, 3, 3, 3, 0.125)),
        )
        .expect("append branch summary");

    // The virtual-model state entry `persist_virtual_model_state` writes.
    let definition = serde_json::json!({
        "provider": "fake",
        "id": "virtual-x",
        "physical": { "provider": "fake", "id": "fake-model" },
    });
    session
        .append_custom_entry(
            pir::ai::virtual_::VIRTUAL_MODEL_STATE_ENTRY,
            Some(serde_json::json!({
                "provider": "fake",
                "modelId": "virtual-x",
                "state": definition,
            })),
        )
        .expect("append virtual-model state");

    let mut registry = ProviderRegistry::default();
    registry.set_provider(Arc::new(FakeProvider));
    let agent = AgentSession::new(AgentSessionConfig {
        provider_registry: Arc::new(registry),
        model: fake_model(),
        api_key: None,
        thinking: ThinkingLevel::Off,
        tools: ToolRegistry::new(),
        session: Some(session),
        compaction: None,
        max_turns: 4,
        system_prompt: Some("test system prompt".into()),
        cwd: Some(temp.0.clone()),
        event_stream: None,
        extensions: None,
        tool_selection: None,
    });

    // M10.4: the borrowed resident fold produces the same totals, and the
    // custom entry contributes nothing.
    let stats = agent.session_stats().await;
    assert_eq!(stats.user_messages, 1);
    assert_eq!(stats.assistant_messages, 1);
    assert_eq!(stats.tool_calls, 1);
    assert_eq!(stats.tool_results, 0);
    assert_eq!(stats.total_messages, 2);
    assert_eq!(stats.tokens.input, 10 + 1 + 2 + 3);
    assert_eq!(stats.tokens.output, 5 + 1 + 2 + 3);
    assert_eq!(stats.tokens.cache_read, 2 + 1 + 2 + 3);
    assert_eq!(stats.tokens.cache_write, 3 + 1 + 2 + 3);
    assert_eq!(stats.tokens.total, 16 + 11 + 8 + 9);
    assert!(
        (stats.cost - 1.0).abs() < f64::EPSILON,
        "assistant + usage + compaction + branch-summary cost: {}",
        stats.cost
    );

    // M10.5: the restore walk (borrowed resident entries) recovered the
    // registration, and the fixture still holds exactly one state entry.
    let restored = agent.virtual_models();
    assert_eq!(restored.len(), 1, "one virtual registration restored");
    assert_eq!(restored[0].id, "virtual-x");
    assert_eq!(restored[0].provider, "fake");
    assert_eq!(restored[0].source, "session");

    let path = agent.session_path().expect("session path");
    let state_entries = SessionManager::open(&path)
        .expect("reopen session")
        .entries()
        .into_iter()
        .filter(|entry| {
            matches!(
                entry,
                FileEntry::Custom(custom)
                    if custom.custom_type == pir::ai::virtual_::VIRTUAL_MODEL_STATE_ENTRY
            )
        })
        .count();
    assert_eq!(state_entries, 1, "no duplicate state entry");
}
