//! Regression tests for the P2 quality wave on `AgentSession`:
//!
//! - **P2-a**: the steering/follow-up inbox drain/peek degraded explicitly to
//!   an empty snapshot when no tokio runtime is active. Before the fix the
//!   command sat in the channel, the oneshot never resolved, and the caller
//!   hung forever (this test polls the futures with a noop waker: without the
//!   no-runtime guard the poll returns `Pending` forever and would never
//!   terminate).
//! - **P2-b**: the inbox oneshot failure path only adds observability (a
//!   `tracing::warn!`); the runtime-path happy test below guards the
//!   behavior stays "deliver the queued texts".
//! - **P2-e**: `navigate_tree` phase 3 re-validates the leaf against the one
//!   collected in phase 1 (three-phase TOCTOU guard, unit-tested through
//!   `branch_summarization::ensure_leaf_unchanged`) plus an end-to-end
//!   navigation regression test on an in-memory session.
//!
//! Kept standalone (no `tests/common` beyond its sandbox endpoint) so it does
//! not collide with the `g_*` / `suites/` conflict areas.

use std::future::Future;
use std::path::Path;
use std::sync::Arc;
use std::task::Poll;

use pir::agent::tool_execution::ToolRegistry;
use pir::agent_session::{AgentSession, AgentSessionConfig};
use pir::ai::provider::ProviderRegistry;
use pir::ai::types::*;
use pir::compaction::branch_summarization::ensure_leaf_unchanged;
use pir::session_manager::SessionManager;

// ---------------------------------------------------------------------------
// Fixtures (same pattern as tests/bash_abort_set.rs, kept standalone)
// ---------------------------------------------------------------------------

/// Per-test temp working directory.
struct TempDir(std::path::PathBuf);

impl TempDir {
    fn new(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "pir-p2-inbox-toctou-{label}-{}-{}",
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

fn user_message(text: &str) -> Message {
    Message::User(UserMessage {
        role: UserRole::User,
        content: vec![ContentBlock::Text(TextContent {
            text: text.to_owned(),
            text_signature: None,
        })],
        tools_added: None,
        tools_removed: None,
        timestamp: 0,
        sections: None,
    })
}

/// An in-memory session scoped to `cwd` (no provider is ever called).
fn session_in(cwd: &Path) -> Arc<AgentSession> {
    Arc::new(AgentSession::new(AgentSessionConfig {
        provider_registry: Arc::new(ProviderRegistry::default()),
        model: fake_model(),
        api_key: None,
        thinking: ThinkingLevel::Off,
        tools: ToolRegistry::new(),
        session: None,
        compaction: None,
        max_turns: 4,
        system_prompt: None,
        cwd: Some(cwd.to_path_buf()),
        event_stream: None,
        extensions: None,
        tool_selection: None,
    }))
}

/// Like [`session_in`] but backed by an in-memory `SessionManager` so tree
/// navigation has real entries to walk.
fn session_with_storage(cwd: &Path) -> Arc<AgentSession> {
    let storage =
        SessionManager::in_memory(&cwd.to_string_lossy(), None).expect("in-memory session manager");
    Arc::new(AgentSession::new(AgentSessionConfig {
        provider_registry: Arc::new(ProviderRegistry::default()),
        model: fake_model(),
        api_key: None,
        thinking: ThinkingLevel::Off,
        tools: ToolRegistry::new(),
        session: Some(storage),
        compaction: None,
        max_turns: 4,
        system_prompt: None,
        cwd: Some(cwd.to_path_buf()),
        event_stream: None,
        extensions: None,
        tool_selection: None,
    }))
}

// ---------------------------------------------------------------------------
// P2-a: no-runtime inbox operations degrade to empty, never hang
// ---------------------------------------------------------------------------

/// `peek`/`clear_queue` polled OUTSIDE any tokio runtime must return their
/// empty defaults immediately. Polled with a noop waker: a `Pending` result
/// would never be woken, so without the no-runtime guard this test hangs
/// instead of failing (the hang IS the bug).
#[test]
fn inbox_ops_return_empty_defaults_without_runtime() {
    let temp = TempDir::new("no-runtime");
    let agent = session_in(&temp.0);

    // Queue texts first: even WITH pending messages, the no-runtime peek
    // must not block waiting for a consumer that can never exist.
    agent.steer("steered while no runtime");
    agent.follow_up("followed up while no runtime");

    let waker = futures_util::task::noop_waker();
    let mut cx = std::task::Context::from_waker(&waker);

    let mut peek = Box::pin(agent.get_steering_messages());
    match peek.as_mut().poll(&mut cx) {
        Poll::Ready(messages) => assert!(
            messages.is_empty(),
            "no-runtime peek must degrade to an empty snapshot, got {messages:?}"
        ),
        Poll::Pending => panic!("no-runtime peek must not hang (P2-a regression)"),
    }

    let mut peek_follow = Box::pin(agent.get_follow_up_messages());
    match peek_follow.as_mut().poll(&mut cx) {
        Poll::Ready(messages) => assert!(messages.is_empty()),
        Poll::Pending => panic!("no-runtime follow-up peek must not hang (P2-a regression)"),
    }

    let mut clear = Box::pin(agent.clear_queue());
    match clear.as_mut().poll(&mut cx) {
        Poll::Ready(cleared) => {
            assert!(cleared.steering.is_empty());
            assert!(cleared.follow_up.is_empty());
        }
        Poll::Pending => panic!("no-runtime clear_queue must not hang (P2-a regression)"),
    }
}

// ---------------------------------------------------------------------------
// P2-b companion: the runtime path still delivers queued texts unchanged
// ---------------------------------------------------------------------------

/// Inside a runtime the consumer task serves the queue as before (the P2-b
/// warn only covers the dropped-reply path, it must not change delivery).
#[tokio::test]
async fn inbox_peek_and_clear_queue_still_deliver_inside_runtime() {
    let temp = TempDir::new("runtime");
    let agent = session_in(&temp.0);

    agent.steer("first");
    agent.steer("second");
    agent.follow_up("later");

    let steering = agent.get_steering_messages().await;
    assert_eq!(steering, vec!["first".to_owned(), "second".to_owned()]);

    let cleared = agent.clear_queue().await;
    assert_eq!(
        cleared.steering,
        vec!["first".to_owned(), "second".to_owned()]
    );
    assert_eq!(cleared.follow_up, vec!["later".to_owned()]);

    // Everything drained: a second peek is empty.
    assert!(agent.get_steering_messages().await.is_empty());
    assert!(agent.get_follow_up_messages().await.is_empty());
}

// ---------------------------------------------------------------------------
// P2-e: the leaf-staleness guard itself
// ---------------------------------------------------------------------------

#[test]
fn leaf_guard_accepts_unchanged_leaf() {
    assert!(ensure_leaf_unchanged(Some("leaf-1"), Some("leaf-1")).is_ok());
    // Both at "no leaf yet" is also unchanged.
    assert!(ensure_leaf_unchanged(None, None).is_ok());
}

#[test]
fn leaf_guard_rejects_moved_leaf() {
    // Concurrent append moved the leaf elsewhere.
    let error = ensure_leaf_unchanged(Some("new-leaf"), Some("old-leaf"))
        .expect_err("moved leaf must be rejected");
    assert!(
        error.contains("session changed during branch summarization"),
        "unexpected error text: {error}"
    );
    // Leaf appeared (was None at collection time).
    assert!(ensure_leaf_unchanged(Some("new-leaf"), None).is_err());
    // Leaf disappeared.
    assert!(ensure_leaf_unchanged(None, Some("old-leaf")).is_err());
}

// ---------------------------------------------------------------------------
// P2-e: end-to-end navigate_tree on an in-memory session
// ---------------------------------------------------------------------------

/// Build a small linear tree (user message, thinking-level change, user
/// message) and navigate back onto the middle entry. The happy path must
/// still work with the new phase-3 validation (leaf unchanged between the
/// phases, since nothing concurrent touches the session).
#[tokio::test]
async fn navigate_tree_happy_path_still_moves_leaf() {
    let temp = TempDir::new("navigate");
    let agent = session_with_storage(&temp.0);

    let first_id = agent
        .with_session_manager(|session| session.append_message(user_message("first")))
        .await
        .expect("storage present")
        .expect("append first");
    let level_id = agent
        .with_session_manager(|session| session.append_thinking_level_change("high"))
        .await
        .expect("storage present")
        .expect("append thinking level");
    let second_id = agent
        .with_session_manager(|session| session.append_message(user_message("second")))
        .await
        .expect("storage present")
        .expect("append second");

    // Sanity: the leaf sits on the last append.
    let leaf = agent
        .with_session_manager(|session| session.leaf_id().map(str::to_owned))
        .await
        .expect("storage present");
    assert_eq!(leaf.as_deref(), Some(second_id.as_str()));

    // Navigate (no summarize -> no provider call) onto the thinking-level
    // entry: a non-user target selects the target itself as the new leaf.
    let navigation = agent
        .navigate_tree(&level_id, false, None, false, 0)
        .await
        .expect("storage present")
        .expect("navigation succeeds with unchanged leaf");
    assert_eq!(navigation.editor_text, None);
    assert_eq!(navigation.summary_entry_id, None);

    let leaf = agent
        .with_session_manager(|session| session.leaf_id().map(str::to_owned))
        .await
        .expect("storage present");
    assert_eq!(leaf.as_deref(), Some(level_id.as_str()));

    // Already at the target: the no-op early return still fires.
    let navigation = agent
        .navigate_tree(&level_id, false, None, false, 0)
        .await
        .expect("storage present")
        .expect("no-op navigation succeeds");
    assert_eq!(navigation.editor_text, None);
    assert_eq!(navigation.summary_entry_id, None);

    // Unknown target still errors.
    let result = agent
        .navigate_tree("no-such-entry", false, None, false, 0)
        .await
        .expect("storage present");
    assert!(
        result.is_err(),
        "navigating to a missing entry must fail, got {result:?}"
    );

    // Silence the unused-variable lint for the first append (its id only
    // anchors the tree shape).
    let _ = first_id;
}
