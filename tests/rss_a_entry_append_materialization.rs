//! RSS-A: `entry_appended` payload materialization follows subscriber
//! interest. A TUI-only session asks for user entries and must not clone or
//! serialize the assistant/toolResult tail; the default `on_session_event`
//! registration keeps the historical "all entries" semantics for
//! print/RPC/SDK.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use pir::agent::agent_loop::AgentEvent;
use pir::agent::tool_execution::ToolRegistry;
use pir::agent_session::{
    entry_appended_wire_builds, reset_entry_appended_wire_builds, AgentSession, AgentSessionConfig,
    SessionEvent, SessionEventInterest,
};
use pir::ai::event_stream::AssistantMessageEventStream;
use pir::ai::provider::{Provider, ProviderRegistry};
use pir::ai::types::*;
use pir::session_manager::SessionManager;

const TEST_ROOT: &str = env!("CARGO_TARGET_TMPDIR");

struct TempDir(std::path::PathBuf);

impl TempDir {
    fn new(name: &str) -> Self {
        let path =
            std::path::Path::new(TEST_ROOT).join(format!("rss-a-{name}-{}", std::process::id()));
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
    fn api_key_auth(&self) -> Vec<pir::ai::auth::ApiKeyAuth> {
        Vec::new()
    }
    fn stream(
        &self,
        _model: &Model,
        _context: Context,
        _options: StreamOptions,
    ) -> AssistantMessageEventStream {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let message = AssistantMessage {
            api: "fake".into(),
            provider: "fake".into(),
            model: "fake-model".into(),
            content: vec![text("done")],
            stop_reason: StopReason::Stop,
            timestamp: 1,
            ..Default::default()
        };
        let stream = AssistantMessageEventStream::new();
        stream.push(AssistantMessageEventStream::done_event(
            TerminalReason::Stop,
            message,
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

fn agent(session: SessionManager) -> AgentSession {
    AgentSession::new_with_plugins(
        AgentSessionConfig {
            performance: Default::default(),
            provider_registry: fake_registry(),
            model: fake_model(),
            api_key: None,
            thinking: ThinkingLevel::Off,
            tools: ToolRegistry::new(),
            session: Some(session),
            compaction: None,
            max_turns: 1,
            system_prompt: None,
            cwd: Some(".".into()),
            event_stream: None,
            extensions: None,
            tool_selection: None,
        },
        Vec::new(),
    )
}

fn noop_events() -> pir::agent_session::EventHandler {
    Arc::new(|_event: AgentEvent| {})
}

fn entry_roles(entries: &Arc<Mutex<Vec<serde_json::Value>>>) -> Vec<String> {
    entries
        .lock()
        .unwrap()
        .iter()
        .filter_map(|entry| {
            entry
                .get("message")
                .and_then(|message| message.get("role"))
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
        })
        .collect()
}

#[tokio::test]
async fn tui_only_interest_skips_assistant_payloads_but_keeps_user_entries() {
    let entries: Arc<Mutex<Vec<serde_json::Value>>> = Arc::new(Mutex::new(Vec::new()));
    let captured = Arc::clone(&entries);
    let seen: pir::agent_session::SessionEventHandler = Arc::new(move |event| {
        if let SessionEvent::EntryAppended { entry } = event {
            captured.lock().unwrap().push(entry);
        }
    });

    // (1) TUI interest: user entries only.
    let temp = TempDir::new("entry-append");
    let session = SessionManager::create(".", &temp.0).expect("create session");
    let tui_agent = agent(session);
    tui_agent.on_session_event_with_interest(seen, SessionEventInterest::EntryAppendedUserOnly);

    reset_entry_appended_wire_builds();
    tui_agent
        .prompt("rss-a user prompt", noop_events())
        .await
        .expect("prompt");
    let tui_builds = entry_appended_wire_builds();
    let tui_roles = entry_roles(&entries);
    assert_eq!(
        tui_builds, 1,
        "a TUI-only session must materialize exactly the user entry, not the assistant: {tui_roles:?}"
    );
    assert_eq!(
        tui_roles,
        vec!["user".to_owned()],
        "the user entry must still reach the TUI bridge"
    );

    // (2) Default registration keeps the historical all-entries semantics.
    entries.lock().unwrap().clear();
    let captured = Arc::clone(&entries);
    let all: pir::agent_session::SessionEventHandler = Arc::new(move |event| {
        if let SessionEvent::EntryAppended { entry } = event {
            captured.lock().unwrap().push(entry);
        }
    });
    let temp = TempDir::new("entry-append");
    let session = SessionManager::create(".", &temp.0).expect("create session");
    let default_agent = agent(session);
    default_agent.on_session_event(all);

    reset_entry_appended_wire_builds();
    default_agent
        .prompt("rss-a default prompt", noop_events())
        .await
        .expect("prompt");
    let default_builds = entry_appended_wire_builds();
    let default_roles = entry_roles(&entries);
    assert!(
        default_builds >= 2,
        "the default subscriber must still receive user AND assistant entries: {default_roles:?}"
    );
    assert!(
        default_roles.iter().any(|role| role == "user")
            && default_roles.iter().any(|role| role == "assistant"),
        "default semantics changed: {default_roles:?}"
    );
}

#[test]
fn tui_bridge_forwards_user_entries_and_drops_assistant_entries() {
    use pir::tui::terminal::{SessionBridgeEvent, SessionEventBridgeProbe};

    let mut probe = SessionEventBridgeProbe::new();
    let user = serde_json::json!({
        "id": "e1",
        "message": { "role": "user", "content": [{ "type": "text", "text": "hello" }] },
    });
    probe.handle(SessionEvent::EntryAppended {
        entry: user.clone(),
    });
    assert_eq!(
        probe.take_events(),
        vec![SessionBridgeEvent::EntryAppended { entry: user }],
        "the user entry must still reach the TUI bridge"
    );

    for entry in [
        serde_json::json!({
            "id": "e2",
            "message": { "role": "assistant", "content": [{ "type": "text", "text": "done" }] },
        }),
        serde_json::json!({
            "id": "e3",
            "message": { "role": "toolResult", "toolCallId": "c1", "content": [] },
        }),
    ] {
        probe.handle(SessionEvent::EntryAppended { entry });
    }
    assert!(
        probe.take_events().is_empty(),
        "assistant/toolResult entries must not produce TUI frames"
    );
}
