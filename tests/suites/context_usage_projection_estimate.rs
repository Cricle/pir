//! `AgentSession::context_usage` estimates over the projection metadata
//! instead of materializing a transcript clone (`agent_session/stats.rs`),
//! with upstream `getContextUsage`'s invalidation rules:
//!
//! - a usage strictly newer than the latest compaction/context edit is the
//!   authoritative count (trailing messages estimated), matching
//!   `compaction::estimate_context_tokens` on the materialized projection;
//! - after a compaction with no projected post-compaction assistant, the
//!   count is UNKNOWN (`tokens`/`percent` `None`), never the pre-compaction
//!   size;
//! - a usage at/before the newest context edit (or the absence of any usage)
//!   re-estimates the whole projection by characters instead of trusting the
//!   stale record;
//! - repeated compactions invalidate every usage from before the newest one.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use pir::agent_session::{AgentSession, AgentSessionConfig};
use pir::ai::provider::ProviderRegistry;
use pir::ai::types::*;
use pir::compaction;
use pir::session_manager::SessionManager;
use serde_json::json;

struct TempDir(PathBuf);

impl TempDir {
    fn new(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "pir-context-usage-{label}-{}-{}",
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
    user_message_at(text, 1)
}

fn user_message_at(text: &str, timestamp: u64) -> Message {
    Message::User(UserMessage {
        role: UserRole::User,
        content: vec![ContentBlock::Text(TextContent {
            text: text.to_owned(),
            text_signature: None,
        })],
        tools_added: None,
        tools_removed: None,
        timestamp,
        sections: None,
    })
}

fn assistant_message(text: &str, input_tokens: u64) -> Message {
    assistant_message_at(text, input_tokens, 2)
}

fn assistant_message_at(text: &str, input_tokens: u64, timestamp: u64) -> Message {
    Message::Assistant(AssistantMessage {
        api: "fake".into(),
        provider: "fake".into(),
        model: "fake-model".into(),
        content: vec![ContentBlock::Text(TextContent {
            text: text.to_owned(),
            text_signature: None,
        })],
        stop_reason: StopReason::Stop,
        usage: Usage {
            input: input_tokens,
            ..Usage::default()
        },
        timestamp,
        ..Default::default()
    })
}

fn tool_result_message(text: &str) -> Message {
    Message::ToolResult(ToolResultMessage {
        role: ToolResultRole::ToolResult,
        tool_call_id: "call-1".into(),
        tool_name: "probe".into(),
        content: vec![ContentBlock::Text(TextContent {
            text: text.to_owned(),
            text_signature: None,
        })],
        details: None,
        usage: None,
        added_tool_names: Vec::new(),
        structured_content: None,
        nested_calls: None,
        is_error: false,
        duration_ms: None,
        timestamp: 3,
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

fn agent_for(cwd: &Path, session: SessionManager) -> AgentSession {
    AgentSession::new(AgentSessionConfig {
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
    })
}

/// The no-invalidation path is still callable (`transcript()` returns the
/// projection's materialized messages): fixtures whose newest usage is
/// trustworthy compare against it directly.
async fn assert_matches_old_path(agent: &AgentSession) -> u64 {
    let transcript = agent.transcript().await;
    let expected = compaction::estimate_context_tokens(&transcript).tokens;
    let usage = agent.context_usage().await.expect("a usage report");
    assert_eq!(
        usage.tokens,
        Some(expected),
        "the metadata estimate must equal the materialized-projection estimate"
    );
    assert_eq!(
        usage.percent,
        Some(expected as f64 / usage.context_window as f64 * 100.0)
    );
    expected
}

#[tokio::test(flavor = "current_thread")]
async fn usage_record_mid_transcript_matches_the_materialized_projection() {
    let temp = TempDir::new("mid-usage");
    let cwd = temp.path().to_string_lossy().into_owned();
    let mut manager = SessionManager::create(&cwd, temp.path()).expect("session");
    manager
        .append_message(user_message("user one"))
        .expect("append");
    manager
        .append_message(assistant_message("assistant one", 500))
        .expect("append");
    manager
        .append_message(user_message("after usage"))
        .expect("append");
    manager
        .append_message(tool_result_message("result"))
        .expect("append");
    manager
        .append_message(assistant_message("ok", 0))
        .expect("append");
    let agent = agent_for(temp.path(), manager);

    let expected = assert_matches_old_path(&agent).await;
    // 500 reported tokens (usage.total_tokens == 0 -> input + output + cache)
    // + ceil(11/4) for "after usage" + ceil(6/4) for "result" + ceil(2/4) for
    // "ok"; the usage message itself and everything before it is not
    // re-estimated.
    assert_eq!(expected, 500 + 3 + 2 + 1);
}

#[tokio::test(flavor = "current_thread")]
async fn transcript_without_usage_matches_the_materialized_projection() {
    let temp = TempDir::new("no-usage");
    let cwd = temp.path().to_string_lossy().into_owned();
    let mut manager = SessionManager::create(&cwd, temp.path()).expect("session");
    manager
        .append_message(user_message("user one"))
        .expect("append");
    manager
        .append_message(assistant_message("assistant one", 0))
        .expect("append");
    manager
        .append_message(user_message("user two"))
        .expect("append");
    let agent = agent_for(temp.path(), manager);

    let expected = assert_matches_old_path(&agent).await;
    assert!(expected > 0, "every message is estimated: {expected}");
}

#[tokio::test(flavor = "current_thread")]
async fn context_edit_newer_than_usage_reestimates_the_projection() {
    let temp = TempDir::new("context-edit-stale");
    let cwd = temp.path().to_string_lossy().into_owned();
    let mut manager = SessionManager::create(&cwd, temp.path()).expect("session");
    let edited_id = manager
        .append_message(user_message("original question with a long body"))
        .expect("append");
    manager
        .append_message(assistant_message("assistant one", 300))
        .expect("append");
    manager
        .append_message(user_message("tail question"))
        .expect("append");
    manager
        .append_context_edit(&edited_id, Some(json!("short")))
        .expect("context edit");
    let agent = agent_for(temp.path(), manager);

    // Upstream `getContextUsage` -> `estimateProjectedContextTokens`: the
    // context edit is newer than the assistant usage, so the reported usage
    // is stale and the whole projection is re-estimated by characters:
    // "short" (5 -> 2) + "assistant one" (13 -> 4) + "tail question" (13 -> 4).
    let usage = agent.context_usage().await.expect("a usage report");
    assert_eq!(usage.tokens, Some(2 + 4 + 4));
    assert_eq!(
        usage.percent,
        Some(10.0 / usage.context_window as f64 * 100.0)
    );
}

#[tokio::test(flavor = "current_thread")]
async fn context_edit_older_than_usage_keeps_the_reported_usage() {
    let temp = TempDir::new("context-edit-fresh");
    let cwd = temp.path().to_string_lossy().into_owned();
    let mut manager = SessionManager::create(&cwd, temp.path()).expect("session");
    let edited_id = manager
        .append_message(user_message("original question with a long body"))
        .expect("append");
    manager
        .append_message(user_message("a second question"))
        .expect("append");
    manager
        .append_message(assistant_message("assistant one", 300))
        .expect("append");
    manager
        .append_context_edit(&edited_id, Some(json!("short")))
        .expect("context edit");
    manager
        .append_message(user_message("tail question"))
        .expect("append");
    manager
        .append_message(assistant_message("reply", 400))
        .expect("append");
    let agent = agent_for(temp.path(), manager);

    // The newest assistant responded after the context edit, so its usage is
    // authoritative: 400 reported tokens, nothing trailing it.
    let usage = agent.context_usage().await.expect("a usage report");
    assert_eq!(usage.tokens, Some(400));
}

#[tokio::test(flavor = "current_thread")]
async fn compaction_with_retained_tail_reports_unknown_usage() {
    let temp = TempDir::new("compaction-tail");
    let cwd = temp.path().to_string_lossy().into_owned();
    let mut manager = SessionManager::create(&cwd, temp.path()).expect("session");
    let kept_id = manager
        .append_message(user_message("user one"))
        .expect("append");
    manager
        .append_message(assistant_message("assistant one", 500))
        .expect("append");
    manager
        .append_message(user_message(&"x".repeat(1000)))
        .expect("append");
    manager
        .append_compaction("compaction summary", Some(&kept_id), 750, None)
        .expect("compaction");
    let agent = agent_for(temp.path(), manager);

    // Upstream `getContextUsage`: after a compaction the last assistant usage
    // describes the PRE-compaction context, and no assistant responded after
    // the compaction, so the token count is unknown. Before the fix the
    // metadata fast path still trusted that usage and reported 500 + 250
    // (the pre-compaction estimate) even though the projection had shrunk.
    let usage = agent.context_usage().await.expect("a usage report");
    assert_eq!(usage.tokens, None);
    assert_eq!(usage.percent, None);
    assert_eq!(usage.context_window, 4096);
    let stats = agent.session_stats().await;
    assert_eq!(stats.context_usage.expect("stats usage").tokens, None);
}

#[tokio::test(flavor = "current_thread")]
async fn post_compaction_assistant_response_reports_its_usage() {
    let temp = TempDir::new("compaction-fresh");
    let cwd = temp.path().to_string_lossy().into_owned();
    let mut manager = SessionManager::create(&cwd, temp.path()).expect("session");
    let kept_id = manager
        .append_message(user_message("user one"))
        .expect("append");
    manager
        .append_message(assistant_message("assistant one", 500))
        .expect("append");
    manager
        .append_compaction("compaction summary", Some(&kept_id), 500, None)
        .expect("compaction");
    // Timestamps advance past the compaction entry: a real post-compaction
    // response always carries a newer prefix timestamp than the summary.
    manager
        .append_message(user_message_at("next question", u64::MAX - 1))
        .expect("append");
    manager
        .append_message(assistant_message_at("reply", 200, u64::MAX))
        .expect("append");
    let agent = agent_for(temp.path(), manager);

    // The post-compaction response is the authoritative usage: 200 reported
    // tokens (the pre-compaction 500 must not leak through).
    let expected = assert_matches_old_path(&agent).await;
    assert_eq!(expected, 200);
    assert_eq!(
        agent.context_usage().await.expect("usage").tokens,
        Some(200)
    );
}

#[tokio::test(flavor = "current_thread")]
async fn repeated_compactions_report_unknown_until_the_next_response() {
    let temp = TempDir::new("compaction-repeated");
    let cwd = temp.path().to_string_lossy().into_owned();
    let mut manager = SessionManager::create(&cwd, temp.path()).expect("session");
    let first_kept = manager
        .append_message(user_message("user one"))
        .expect("append");
    manager
        .append_message(assistant_message("assistant one", 500))
        .expect("append");
    manager
        .append_compaction("summary one", Some(&first_kept), 500, None)
        .expect("compaction one");
    let second_kept = manager
        .append_message(user_message("second question"))
        .expect("append");
    manager
        .append_message(assistant_message("second reply", 700))
        .expect("append");
    manager
        .append_compaction("summary two", Some(&second_kept), 700, None)
        .expect("compaction two");
    let agent = agent_for(temp.path(), manager);

    // The 700-token usage predates the second compaction; nothing responded
    // after it, so the count is unknown (not the pre-compaction 700).
    assert_eq!(
        agent.context_usage().await.expect("usage").tokens,
        None,
        "a usage between two compactions must not survive the newer compaction"
    );

    // The next assistant response re-establishes a trustworthy usage.
    let session = agent.session_manager().expect("session");
    session
        .lock()
        .await
        .append_message(assistant_message_at("third reply", 100, u64::MAX))
        .expect("append");
    let usage = agent.context_usage().await.expect("usage");
    assert_eq!(usage.tokens, Some(100));
    assert_eq!(
        usage.percent,
        Some(100.0 / usage.context_window as f64 * 100.0)
    );
}

#[tokio::test(flavor = "current_thread")]
async fn pre_compaction_estimate_is_unchanged() {
    let temp = TempDir::new("pre-compaction");
    let cwd = temp.path().to_string_lossy().into_owned();
    let mut manager = SessionManager::create(&cwd, temp.path()).expect("session");
    manager
        .append_message(user_message("user one"))
        .expect("append");
    manager
        .append_message(assistant_message("assistant one", 500))
        .expect("append");
    manager
        .append_message(user_message(&"x".repeat(1000)))
        .expect("append");
    let agent = agent_for(temp.path(), manager);

    // No compaction: the usage record plus the trailing 250-token message.
    let expected = assert_matches_old_path(&agent).await;
    assert_eq!(expected, 500 + 250);
}
