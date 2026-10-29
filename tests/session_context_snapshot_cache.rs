//! `ctx.sessionManager` snapshot push cache (`agent_session/session_ctx.rs`).
//!
//! The snapshot push runs before every prompt and again after it settles. The
//! second push used to rebuild the whole session-derived payload — context
//! entries (a full projection materialization + serialization), the branch, the
//! full entry tree, the per-entry projection copy, the label map, and the
//! session-directory listing — even when nothing had changed. The push now
//! fingerprints the live state (cheap borrowed session reads + the scalar
//! snapshot cells) and skips the rebuild when the runtime already holds the
//! identical snapshot.
//!
//! The tests pin:
//! - two consecutive pushes with no session mutation leave the runtime snapshot
//!   byte-identical and do not push it a second time to plugins, and the second
//!   push reads NO session file for the listing (`session_list_file_reads`);
//! - appending a session entry invalidates the fingerprint: the next push
//!   rebuilds and the new entry is visible in the snapshot.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use pir::agent_session::{AgentSession, AgentSessionConfig};
use pir::ai::provider::ProviderRegistry;
use pir::ai::types::*;
use pir::extensions::{ExtensionConfig, ExtensionHandle, SessionContextSnapshot};
use pir::plugins::{AgentPlugin, PluginInfo};
use pir::session_manager::{
    reset_session_list_file_reads, session_list_file_reads, SessionManager,
};

struct TempDir(PathBuf);

impl TempDir {
    fn new(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "pir-ctx-snapshot-cache-{label}-{}-{}",
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

fn user_message(text: &str) -> Message {
    Message::User(UserMessage {
        role: UserRole::User,
        content: vec![ContentBlock::Text(TextContent {
            text: text.to_owned(),
            text_signature: None,
        })],
        tools_added: None,
        tools_removed: None,
        timestamp: 1,
        sections: None,
    })
}

fn assistant_message(text: &str) -> Message {
    Message::Assistant(AssistantMessage {
        api: "fake".into(),
        provider: "fake".into(),
        model: "fake-model".into(),
        content: vec![ContentBlock::Text(TextContent {
            text: text.to_owned(),
            text_signature: None,
        })],
        stop_reason: StopReason::Stop,
        timestamp: 2,
        ..Default::default()
    })
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

fn extension_config(cwd: &Path) -> ExtensionConfig {
    ExtensionConfig {
        cwd: cwd.to_path_buf(),
        mode: "headless".to_string(),
        has_ui: false,
        model: None,
        session_id: None,
        system_prompt: String::new(),
        is_idle: true,
        limits: Some(Default::default()),
    }
}

fn agent_config(cwd: &Path, session: SessionManager) -> AgentSessionConfig {
    AgentSessionConfig {
        provider_registry: Arc::new(ProviderRegistry::default()),
        model: fake_model(),
        api_key: None,
        thinking: ThinkingLevel::Off,
        tools: pir::agent::tool_execution::ToolRegistry::new(),
        session: Some(session),
        compaction: None,
        max_turns: 1,
        system_prompt: None,
        cwd: Some(cwd.to_path_buf()),
        event_stream: None,
        extensions: None,
        tool_selection: None,
    }
}

/// Inline plugin recording every `on_session_context` push: a cache hit must
/// not send the (identical) snapshot through the plugin fan-out again.
struct SnapshotRecorder {
    info: PluginInfo,
    pushes: Arc<Mutex<Vec<SessionContextSnapshot>>>,
}

impl SnapshotRecorder {
    fn new() -> (Self, Arc<Mutex<Vec<SessionContextSnapshot>>>) {
        let pushes = Arc::new(Mutex::new(Vec::new()));
        (
            Self {
                info: PluginInfo::inline("snapshot-recorder"),
                pushes: Arc::clone(&pushes),
            },
            pushes,
        )
    }
}

impl AgentPlugin for SnapshotRecorder {
    fn info(&self) -> &PluginInfo {
        &self.info
    }

    fn on_session_context(&self, snapshot: &SessionContextSnapshot) {
        self.pushes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(snapshot.clone());
    }
}

async fn snapshot_json(handle: &ExtensionHandle) -> serde_json::Value {
    handle
        .eval_json("__pir_host.getSessionManagerJson()")
        .await
        .expect("read the pushed snapshot")
}

#[tokio::test(flavor = "current_thread")]
async fn unchanged_push_reuses_the_previous_snapshot() {
    let temp = TempDir::new("reuse");
    let cwd = temp.path().to_string_lossy().into_owned();
    let mut manager = SessionManager::create(&cwd, temp.path()).expect("session");
    manager
        .append_message(user_message("first turn"))
        .expect("append user");
    manager
        .append_message(assistant_message("first reply"))
        .expect("append assistant");

    let (recorder, pushes) = SnapshotRecorder::new();
    let handle = ExtensionHandle::start(extension_config(temp.path())).expect("extension handle");
    let mut config = agent_config(temp.path(), manager);
    config.extensions = Some(handle.clone());
    let recorder: Arc<dyn AgentPlugin> = Arc::new(recorder);
    let agent = AgentSession::new_with_plugins(config, vec![recorder]);

    reset_session_list_file_reads();
    assert!(
        agent.push_session_context_for_test().await,
        "the first push has no previous snapshot and must build"
    );
    let first = snapshot_json(&handle).await;
    assert_eq!(
        pushes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len(),
        1,
        "the first push reaches the plugin fan-out"
    );
    assert_eq!(
        first["entries"].as_array().map(Vec::len),
        Some(2),
        "the snapshot carries the projected context entries: {first}"
    );
    assert_eq!(
        session_list_file_reads(),
        1,
        "the build lists the one session file exactly once"
    );

    // No session mutation between the two pushes: the runtime already holds
    // this exact snapshot, so the second push must be a no-op.
    reset_session_list_file_reads();
    assert!(
        !agent.push_session_context_for_test().await,
        "an unchanged push must not rebuild"
    );
    let second = snapshot_json(&handle).await;
    assert_eq!(first, second, "the runtime snapshot is unchanged");
    assert_eq!(
        pushes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len(),
        1,
        "a cache hit must not fan the identical snapshot out again"
    );
    assert_eq!(
        session_list_file_reads(),
        0,
        "a skipped push must not re-read the session directory"
    );

    // A new entry invalidates the fingerprint.
    let manager = agent.session_manager().expect("persisted session");
    manager
        .lock()
        .await
        .append_message(user_message("second turn"))
        .expect("append user");
    assert!(
        agent.push_session_context_for_test().await,
        "a new session entry must invalidate the cache"
    );
    let third = snapshot_json(&handle).await;
    assert_ne!(first, third, "the new entry must reach the snapshot");
    assert_eq!(
        third["entries"].as_array().map(Vec::len),
        Some(3),
        "the snapshot carries the new projected entry: {third}"
    );
    assert_eq!(
        pushes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len(),
        2,
        "the rebuilt snapshot is pushed to the plugin fan-out"
    );
}
