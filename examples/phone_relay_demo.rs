//! End-to-end demo: a real `AgentSession` + `pi-phone` plugin (agent client)
//! connected to `pir-phone-server`, with a scripted local model so no API key
//! is required. Open the printed server URL in a browser, log in with the
//! printed user token, and send a prompt.
//!
//! ```text
//! cargo run --example phone_relay_demo --features phone
//! ```

use std::sync::Arc;
use std::time::Duration;

use pir::agent::tool_execution::ToolRegistry;
use pir::agent_session::{AgentSession, AgentSessionConfig, SharedEventStreamFn};
use pir::ai::event_stream::AssistantMessageEventStream;
use pir::ai::provider::ProviderRegistry;
use pir::ai::types::*;
use pir::plugins::builtin::phone::PhonePlugin;
use pir::plugins::AgentPlugin;
use pir_phone_server::config::Config as ServerConfig;
use pir_phone_server::http as phone_http;
use pir_phone_server::state::AppState as PhoneServerState;
use pir_phone_server::store::Store as PhoneStore;

fn fake_model() -> Model {
    Model {
        id: "demo-model".into(),
        name: "Demo model".into(),
        api: "demo-api".into(),
        provider: "demo".into(),
        base_url: "http://localhost".into(),
        reasoning: true,
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
        context_window: 8192,
        max_tokens: 512,
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

fn scripted_assistant(text: &str) -> AssistantMessage {
    AssistantMessage {
        api: "demo".into(),
        provider: "demo".into(),
        model: "demo-model".into(),
        content: vec![ContentBlock::Text(TextContent {
            text: text.into(),
            text_signature: None,
        })],
        usage: Usage {
            input: 8,
            output: 6,
            total_tokens: 14,
            ..Default::default()
        },
        stop_reason: StopReason::Stop,
        timestamp: 1,
        ..Default::default()
    }
}

fn scripted_stream() -> SharedEventStreamFn {
    Arc::new(|_request| {
        Box::pin(async move {
            let stream = AssistantMessageEventStream::new();
            stream.push(AssistantMessageEventStream::done_event(
                TerminalReason::Stop,
                scripted_assistant("Hello from the real pi-phone agent (scripted model)."),
            ));
            stream
        })
    })
}

fn demo_config() -> AgentSessionConfig {
    AgentSessionConfig {
        provider_registry: Arc::new(ProviderRegistry::default()),
        model: fake_model(),
        api_key: None,
        thinking: ThinkingLevel::Low,
        tools: ToolRegistry::new(),
        session: None,
        compaction: None,
        max_turns: 4,
        performance: Default::default(),
        system_prompt: Some("pi-phone relay demo".into()),
        cwd: std::env::current_dir().ok(),
        event_stream: Some(scripted_stream()),
        extensions: None,
        tool_selection: None,
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let dir = std::env::temp_dir().join(format!("pir-phone-demo-{}", uuid::Uuid::new_v4()));
    let store = PhoneStore::open(&dir)?;
    let user = store.create_user("demo", None)?;
    let (user_token, _) = store.create_token(&user.id, "user", Some("demo"), None)?;
    let device = store.create_device(&user.id, "Demo device", None)?;
    let (device_token, _) =
        store.create_token(&user.id, "device", Some("demo"), Some(&device.id))?;

    let config = ServerConfig::load(dir.clone(), "127.0.0.1:0".parse()?, None, Vec::new())?;
    let state = PhoneServerState::new(config, store);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    tokio::spawn(async move {
        let _ = phone_http::serve(listener, state).await;
    });

    let plugin = Arc::new(PhonePlugin::new());
    let session = AgentSession::new_arc_with_plugins(
        demo_config(),
        vec![plugin.clone() as Arc<dyn AgentPlugin>],
    );
    plugin
        .start_remote_for_test(&format!(
            "ws://{addr} {device_token} --device-name demo --device-id {}",
            device.id
        ))
        .await
        .expect("connect phone relay");

    println!("pi-phone demo");
    println!("  server:       http://{addr}/");
    println!("  user token:   {user_token}");
    println!("  device token: {device_token}");
    println!("Open the server URL, log in with the user token, and send a prompt.");

    tokio::time::sleep(Duration::from_secs(3600)).await;
    drop(session);
    let _ = std::fs::remove_dir_all(dir);
    Ok(())
}
