//! Tests for branch summarization (upstream
//! `packages/coding-agent/src/core/compaction/branch-summarization.ts` +
//! `navigateTree`): entry collection over the session tree, token-budget
//! truncation, file-op carry-over from nested branch summaries, the
//! summarizer call (fake `CompleteFn`, same pattern as the compaction suite),
//! and the `AgentSession::navigate_tree` wiring (fake provider registry).

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;

use pir::agent::tool_execution::ToolRegistry;
use pir::agent_session::{AgentSession, AgentSessionConfig};
use pir::ai::auth::ApiKeyAuth;
use pir::ai::event_stream::AssistantMessageEventStream;
use pir::ai::provider::{Provider, ProviderRegistry};
use pir::ai::types::*;
use pir::compaction::branch_summarization::{
    branch_summary_max_tokens, collect_entries_for_branch_summary, generate_branch_summary,
    prepare_branch_entries, BRANCH_SUMMARY_PROMPT,
};
use pir::compaction::prompts::SUMMARIZATION_SYSTEM_PROMPT;
use pir::compaction::{compute_file_lists, CompleteFn};
use pir::session_manager::{
    BranchSummaryEntry, CompactionEntry, FileEntry, MessageEntry, SessionManager, StoredMessage,
};

// ---------------------------------------------------------------------------
// Fakes (same pattern as tests/suites/compaction.rs and agent_session.rs)
// ---------------------------------------------------------------------------

fn fake_model(context_window: u64, max_tokens: u64) -> Model {
    Model {
        id: "test-model".into(),
        name: "Test Model".into(),
        api: "openai-completions".into(),
        provider: "test".into(),
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
        context_window,
        max_tokens,
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

fn text_block(text: &str) -> ContentBlock {
    ContentBlock::Text(TextContent {
        text: text.into(),
        text_signature: None,
    })
}

fn user_msg(text: &str) -> Message {
    Message::User(UserMessage {
        role: UserRole::User,
        content: vec![text_block(text)],
        tools_added: None,
        tools_removed: None,
        timestamp: 1,
        sections: None,
    })
}

fn assistant_text(text: &str, stop_reason: StopReason) -> AssistantMessage {
    AssistantMessage {
        api: "test".into(),
        provider: "test".into(),
        model: "test-model".into(),
        content: vec![text_block(text)],
        stop_reason,
        timestamp: 2,
        ..Default::default()
    }
}

/// An assistant message with one `read`/`edit` tool call carrying `path`.
fn assistant_with_tool_call(id: &str, name: &str, path: &str) -> AssistantMessage {
    let mut arguments = BTreeMap::new();
    arguments.insert("path".to_string(), serde_json::json!(path));
    AssistantMessage {
        api: "test".into(),
        provider: "test".into(),
        model: "test-model".into(),
        content: vec![ContentBlock::ToolCall(ToolCall {
            id: id.into(),
            name: name.into(),
            arguments,
            thought_signature: None,
            namespace: None,
            arguments_error: None,
        })],
        stop_reason: StopReason::ToolUse,
        timestamp: 2,
        ..Default::default()
    }
}

fn tool_result_msg(call_id: &str, text: &str) -> Message {
    Message::ToolResult(ToolResultMessage {
        role: ToolResultRole::ToolResult,
        tool_call_id: call_id.into(),
        tool_name: "read".into(),
        content: vec![text_block(text)],
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

fn message_entry(id: &str, parent: Option<&str>, message: Message) -> FileEntry {
    FileEntry::Message(MessageEntry {
        id: id.into(),
        parent_id: parent.map(str::to_owned),
        timestamp: "2026-01-01T00:00:00.000Z".into(),
        message: StoredMessage::from(message),
    })
}

fn compaction_entry(id: &str, parent: Option<&str>, summary: &str) -> FileEntry {
    FileEntry::Compaction(CompactionEntry {
        id: id.into(),
        parent_id: parent.map(str::to_owned),
        timestamp: "2026-01-01T00:00:00.000Z".into(),
        summary: summary.into(),
        first_kept_entry_id: id.into(),
        first_kept_entry_index: None,
        tokens_before: 0,
        details: None,
        usage: None,
        from_hook: None,
        system_message: None,
    })
}

fn branch_summary_entry(
    id: &str,
    parent: Option<&str>,
    summary: &str,
    details: Option<serde_json::Value>,
    from_hook: bool,
) -> FileEntry {
    FileEntry::BranchSummary(BranchSummaryEntry {
        id: id.into(),
        parent_id: parent.map(str::to_owned),
        timestamp: "2026-01-01T00:00:00.000Z".into(),
        from_id: "old".into(),
        summary: summary.into(),
        details,
        usage: None,
        from_hook: if from_hook { Some(true) } else { None },
    })
}

/// A `CompleteFn` that returns a scripted result and records the request
/// contexts it was called with.
fn complete_of(
    response: Result<AssistantMessage, String>,
    contexts: Arc<std::sync::Mutex<Vec<Context>>>,
) -> Arc<CompleteFn> {
    Arc::new(Box::new(move |context: Context| {
        if let Ok(mut slots) = contexts.lock() {
            slots.push(context);
        }
        let response = response.clone();
        Box::pin(async move { response })
    }))
}

// ---------------------------------------------------------------------------
// collectEntriesForBranchSummary
// ---------------------------------------------------------------------------

/// A linear session `u1 -> a1` plus a sibling branch `u1 -> u2 -> u3`.
fn branched_session() -> (SessionManager, Vec<String>) {
    let mut session = SessionManager::in_memory(".", None).expect("in-memory session");
    let u1 = session
        .append_message(user_msg("first"))
        .expect("append u1");
    let a1 = session
        .append_message(Message::Assistant(assistant_text(
            "answer",
            StopReason::Stop,
        )))
        .expect("append a1");
    session.branch(&u1).expect("branch");
    let u2 = session
        .append_message(user_msg("second"))
        .expect("append u2");
    let u3 = session
        .append_message(user_msg("third"))
        .expect("append u3");
    (session, vec![u1, a1, u2, u3])
}

#[test]
fn collect_walks_the_abandoned_branch_to_the_common_ancestor() {
    let (session, ids) = branched_session();
    let [u1, a1, u2, u3] = [
        ids[0].clone(),
        ids[1].clone(),
        ids[2].clone(),
        ids[3].clone(),
    ];

    // From the branch leaf back to the trunk root: the abandoned path is
    // u2 -> u3 in chronological order, with u1 as the common ancestor.
    let result = collect_entries_for_branch_summary(&session, Some(&u3), &u1);
    assert_eq!(
        result
            .entries
            .iter()
            .map(|entry| entry.id().unwrap_or_default())
            .collect::<Vec<_>>(),
        vec![u2, u3.clone()]
    );
    assert_eq!(result.common_ancestor_id.as_deref(), Some(u1.as_str()));

    // Navigating to the sibling trunk assistant: everything after the fork
    // point is abandoned. The common ancestor is the DEEPEST entry shared by
    // both paths (upstream `collectEntriesForBranchSummary`): a1 is on the
    // target path, but u3's path only shares u1 — the ancestor is u1, so
    // the abandoned walk keeps the two entries after it.
    let result = collect_entries_for_branch_summary(&session, Some(&u3), &a1);
    assert_eq!(result.common_ancestor_id.as_deref(), Some(u1.as_str()));
    assert_eq!(result.entries.len(), 2);

    // No old position: nothing to summarize.
    let result = collect_entries_for_branch_summary(&session, None, &u1);
    assert!(result.entries.is_empty());
    assert!(result.common_ancestor_id.is_none());
}

// ---------------------------------------------------------------------------
// prepareBranchEntries: budget truncation + file ops
// ---------------------------------------------------------------------------

#[test]
fn prepare_keeps_the_most_recent_messages_within_the_token_budget() {
    // Each user message is 400 chars => 100 estimated tokens (chars/4).
    let long = "x".repeat(400);
    let entries = vec![
        message_entry("m1", None, user_msg(&long)),
        message_entry("m2", Some("m1"), user_msg(&long)),
        message_entry("m3", Some("m2"), user_msg(&long)),
    ];

    // Budget 150: the newest message (100) fits; the next one (200 > 150)
    // stops the walk, so only m3 survives.
    let preparation = prepare_branch_entries(&entries, 150);
    assert_eq!(preparation.messages.len(), 1);
    assert_eq!(preparation.total_tokens, 100);
    match &preparation.messages[0] {
        Message::User(user) => match &user.content[0] {
            ContentBlock::Text(text) => assert_eq!(text.text, long),
            other => panic!("expected a text block, got {other:?}"),
        },
        other => panic!("expected a user message, got {other:?}"),
    }

    // Budget 0 = no limit.
    let preparation = prepare_branch_entries(&entries, 0);
    assert_eq!(preparation.messages.len(), 3);
    assert_eq!(preparation.total_tokens, 300);
}

#[test]
fn prepare_still_fits_summary_entries_under_ninety_percent_of_the_budget() {
    // Budget 110 (90% = 99). The compaction summary (~250 tokens) alone
    // exceeds the budget, but a summary entry still fits while the running
    // total (0) is under 90% of the budget; the following over-budget
    // non-summary user message then breaks the walk (upstream
    // `prepareBranchEntries`: the 90% allowance applies to
    // compaction/branch_summary entries only).
    let long = "x".repeat(400);
    let entries = vec![
        message_entry("m1", None, user_msg(&long)),
        compaction_entry("c1", Some("m1"), &"s".repeat(1000)),
    ];
    let preparation = prepare_branch_entries(&entries, 110);
    assert_eq!(preparation.messages.len(), 1);
    match &preparation.messages[0] {
        Message::User(user) => match &user.content[0] {
            ContentBlock::Text(text) => {
                assert!(text.text.contains(&"s".repeat(1000)));
                assert!(text.text.starts_with(
                    "The conversation history before this point was compacted into the following summary:"
                ));
            }
            other => panic!("expected the framed compaction summary, got {other:?}"),
        },
        other => panic!("expected a user message, got {other:?}"),
    }
}

#[test]
fn prepare_skips_tool_results_and_carries_file_ops_from_nested_summaries() {
    let entries = vec![
        message_entry(
            "m1",
            None,
            Message::Assistant(assistant_with_tool_call("call-1", "read", "src/lib.rs")),
        ),
        message_entry("m2", Some("m1"), tool_result_msg("call-1", "file body")),
        // pi-generated nested summary: its tracking carries over.
        branch_summary_entry(
            "b1",
            Some("m2"),
            "nested summary",
            Some(serde_json::json!({
                "readFiles": ["docs/a.md"],
                "modifiedFiles": ["src/main.rs"],
            })),
            false,
        ),
        // Extension-generated (fromHook): its tracking is ignored.
        branch_summary_entry(
            "b2",
            Some("b1"),
            "extension summary",
            Some(serde_json::json!({"readFiles": ["skipped.rs"]})),
            true,
        ),
    ];

    let preparation = prepare_branch_entries(&entries, 0);
    // Tool results are skipped; branch summaries contribute their framed
    // message regardless of fromHook (upstream `getMessageFromEntry` has no
    // fromHook filter on the message path — only the file-ops pass ignores
    // hook-generated summaries): assistant + b1 + b2.
    assert_eq!(preparation.messages.len(), 3);
    assert!(matches!(preparation.messages[0], Message::Assistant(_)));
    assert!(matches!(preparation.messages[1], Message::User(_)));
    assert!(matches!(preparation.messages[2], Message::User(_)));

    let (read_files, modified_files) = compute_file_lists(&preparation.file_ops);
    assert_eq!(
        read_files,
        vec!["docs/a.md".to_string(), "src/lib.rs".to_string()]
    );
    assert_eq!(modified_files, vec!["src/main.rs".to_string()]);
}

// ---------------------------------------------------------------------------
// generateBranchSummary (fake CompleteFn)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn generate_builds_the_upstream_prompt_and_wraps_the_output() {
    let long = "y".repeat(200);
    let entries = vec![
        message_entry(
            "m1",
            None,
            Message::Assistant(assistant_with_tool_call("call-1", "edit", "src/mod.rs")),
        ),
        message_entry("m2", Some("m1"), user_msg(&long)),
    ];
    let contexts = Arc::new(std::sync::Mutex::new(Vec::new()));
    let complete = complete_of(
        Ok(assistant_text("## Goal\nbranch work", StopReason::Stop)),
        contexts.clone(),
    );

    let result = generate_branch_summary(
        &entries,
        &fake_model(128_000, 4096),
        complete,
        None,
        false,
        16_384,
    )
    .await;

    assert!(result.error.is_none());
    let summary = result.summary.expect("summary text");
    assert!(
        summary.starts_with(
            "The user explored a different conversation branch before returning here.\nSummary of that exploration:\n\n## Goal\nbranch work"
        ),
        "unexpected summary: {summary}"
    );
    // File lists from the abandoned branch's tool calls are appended.
    assert!(summary.contains("<modified-files>\nsrc/mod.rs\n</modified-files>"));

    // The request carries the upstream system prompt and the serialized
    // conversation ending with the verbatim branch-summary prompt.
    let slots = contexts.lock().unwrap();
    assert_eq!(slots.len(), 1);
    assert_eq!(
        slots[0].system_prompt.as_deref(),
        Some(SUMMARIZATION_SYSTEM_PROMPT)
    );
    assert_eq!(slots[0].messages.len(), 1);
    let prompt = match &slots[0].messages[0] {
        Message::User(user) => match &user.content[0] {
            ContentBlock::Text(text) => text.text.clone(),
            other => panic!("expected a text block, got {other:?}"),
        },
        other => panic!("expected a user message, got {other:?}"),
    };
    assert!(prompt.starts_with("<conversation>\n"));
    assert!(prompt.contains("[Assistant tool calls]: edit(path=\"src/mod.rs\")"));
    assert!(
        prompt.ends_with(BRANCH_SUMMARY_PROMPT),
        "prompt must end with the verbatim upstream template"
    );
}

#[tokio::test]
async fn generate_appends_custom_focus_and_honors_replace_instructions() {
    let entries = vec![message_entry("m1", None, user_msg("hello"))];

    // Appended focus.
    let contexts = Arc::new(std::sync::Mutex::new(Vec::new()));
    let complete = complete_of(
        Ok(assistant_text("ok", StopReason::Stop)),
        Arc::clone(&contexts),
    );
    let result = generate_branch_summary(
        &entries,
        &fake_model(128_000, 4096),
        complete,
        Some("the refactoring"),
        false,
        16_384,
    )
    .await;
    assert!(result.error.is_none());
    let prompt = match &contexts.lock().unwrap()[0].messages[0] {
        Message::User(user) => match &user.content[0] {
            ContentBlock::Text(text) => text.text.clone(),
            other => panic!("expected a text block, got {other:?}"),
        },
        other => panic!("expected a user message, got {other:?}"),
    };
    assert!(prompt.ends_with(&format!(
        "{BRANCH_SUMMARY_PROMPT}\n\nAdditional focus: the refactoring"
    )));

    // replace_instructions swaps the template out entirely.
    let contexts = Arc::new(std::sync::Mutex::new(Vec::new()));
    let complete = complete_of(Ok(assistant_text("ok", StopReason::Stop)), contexts.clone());
    let result = generate_branch_summary(
        &entries,
        &fake_model(128_000, 4096),
        complete,
        Some("custom only"),
        true,
        16_384,
    )
    .await;
    assert!(result.error.is_none());
    let prompt = match &contexts.lock().unwrap()[0].messages[0] {
        Message::User(user) => match &user.content[0] {
            ContentBlock::Text(text) => text.text.clone(),
            other => panic!("expected a text block, got {other:?}"),
        },
        other => panic!("expected a user message, got {other:?}"),
    };
    assert_eq!(
        prompt,
        "<conversation>\n[User]: hello\n</conversation>\n\ncustom only"
    );
}

#[tokio::test]
async fn generate_surfaces_failures_without_panicking() {
    // Empty message set: the static upstream fallback text, no error.
    let result = generate_branch_summary(
        &[],
        &fake_model(128_000, 4096),
        complete_of(
            Ok(assistant_text("x", StopReason::Stop)),
            Arc::new(std::sync::Mutex::new(Vec::new())),
        ),
        None,
        false,
        16_384,
    )
    .await;
    assert_eq!(result.summary.as_deref(), Some("No content to summarize"));
    assert!(result.error.is_none());

    // Error stop reason.
    let result = generate_branch_summary(
        &[message_entry("m1", None, user_msg("hello"))],
        &fake_model(128_000, 4096),
        complete_of(
            Ok(assistant_text("", StopReason::Error)),
            Arc::new(std::sync::Mutex::new(Vec::new())),
        ),
        None,
        false,
        16_384,
    )
    .await;
    assert!(result
        .error
        .is_some_and(|error| error.starts_with("Branch summarization failed:")));

    // Transport failure (the retry also fails).
    let result = generate_branch_summary(
        &[message_entry("m1", None, user_msg("hello"))],
        &fake_model(128_000, 4096),
        complete_of(
            Err("boom".into()),
            Arc::new(std::sync::Mutex::new(Vec::new())),
        ),
        None,
        false,
        16_384,
    )
    .await;
    assert_eq!(
        result.error.as_deref(),
        Some("Branch summarization failed: boom")
    );
    assert!(result.summary.is_none());

    // A tool-call attempt in the summarizer response is rejected.
    let result = generate_branch_summary(
        &[message_entry("m1", None, user_msg("hello"))],
        &fake_model(128_000, 4096),
        complete_of(
            Ok(assistant_with_tool_call("call-1", "read", "x")),
            Arc::new(std::sync::Mutex::new(Vec::new())),
        ),
        None,
        false,
        16_384,
    )
    .await;
    assert_eq!(
        result.error.as_deref(),
        Some("Branch summarization attempted to call a tool")
    );
}

#[tokio::test]
async fn generate_uses_the_context_window_budget_and_the_response_cap() {
    // Budget = contextWindow - reserveTokens.
    let long = "z".repeat(400); // 100 tokens per message
    let entries: Vec<FileEntry> = (0..30)
        .map(|index| message_entry(&format!("m{index}"), None, user_msg(&long)))
        .collect();
    let contexts = Arc::new(std::sync::Mutex::new(Vec::new()));
    let complete = complete_of(
        Ok(assistant_text("ok", StopReason::Stop)),
        Arc::clone(&contexts),
    );
    let model = fake_model(1_000, 0);
    let result = generate_branch_summary(&entries, &model, complete, None, false, 400).await;
    assert!(result.error.is_none());
    // VERDICT (test premise was stale, implementation right): 1000 - 400 = 600
    // tokens => the PREPARATION keeps 6 x 100-token messages (asserted below),
    // but that prepared span no longer fits ONE summarizer REQUEST: the chunk
    // budget is 3/4 x (window - reserve) = 450 tokens = 1800 chars, while the
    // span serializes to ~2448 chars (~612 tokens) and the response cap is 4096
    // tokens on a 1000-token window. Sending it as one request would exceed the
    // window, so the chunked runner splits it — the pre-chunking single call is
    // exactly the behavior this suite must NOT pin anymore. The preparation
    // budget and the response caps (the subject of this test) are unchanged.
    let calls = contexts.lock().unwrap().len();
    assert!(calls >= 2, "the span must chunk: {calls} calls");
    for (index, context) in contexts.lock().unwrap().iter().enumerate() {
        let chars: usize = context
            .messages
            .iter()
            .map(|message| match message {
                Message::User(user) => user
                    .content
                    .iter()
                    .map(|block| match block {
                        ContentBlock::Text(text) => text.text.chars().count(),
                        _ => 0,
                    })
                    .sum(),
                _ => 0,
            })
            .sum();
        assert!(
            chars <= 1_000 * 4,
            "request {index} must stay inside the window budget: {chars} chars"
        );
    }
    let preparation = prepare_branch_entries(&entries, 600);
    assert_eq!(preparation.total_tokens, 600);

    // Upstream 6fcb48670: the branch summary uses the model's own output
    // limit; there is no fixed 4096 cap (0 means "no additional cap").
    assert_eq!(branch_summary_max_tokens(&fake_model(1_000, 256)), 256);
    assert_eq!(branch_summary_max_tokens(&fake_model(1_000, 0)), 0);
    assert_eq!(branch_summary_max_tokens(&fake_model(1_000, 9_999)), 9_999);
}

// ---------------------------------------------------------------------------
// AgentSession::navigate_tree (fake provider registry, upstream `navigateTree`)
// ---------------------------------------------------------------------------

type FakeRespond = Arc<dyn Fn(usize, &Context) -> AssistantMessage + Send + Sync>;

fn fake_registry_for(respond: FakeRespond) -> Arc<ProviderRegistry> {
    struct FakeProvider {
        calls: AtomicUsize,
        respond: FakeRespond,
    }
    impl Provider for FakeProvider {
        fn id(&self) -> &str {
            // Must match the `provider` on the test models: the session
            // resolves the summarization model through the provider registry
            // keyed by `model.provider`.
            "test"
        }
        fn name(&self) -> &str {
            "Fake provider"
        }
        fn get_models(&self) -> Vec<Model> {
            vec![fake_model(4096, 256)]
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
    let mut registry = ProviderRegistry::default();
    registry.set_provider(Arc::new(FakeProvider {
        calls: AtomicUsize::new(0),
        respond,
    }));
    Arc::new(registry)
}

struct TempDir(std::path::PathBuf);

impl TempDir {
    fn new(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "pir-branch-summary-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|duration| duration.as_nanos())
                .unwrap_or(0)
        ));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("create temp dir");
        TempDir(path)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn agent_with_session(
    session: SessionManager,
    registry: Arc<ProviderRegistry>,
    dir: &std::path::Path,
) -> Arc<AgentSession> {
    Arc::new(AgentSession::new(AgentSessionConfig {
        provider_registry: registry,
        model: fake_model(4096, 256),
        api_key: None,
        thinking: ThinkingLevel::Off,
        tools: ToolRegistry::new(),
        session: Some(session),
        compaction: None,
        max_turns: 4,
        system_prompt: None,
        cwd: Some(dir.to_path_buf()),
        event_stream: None,
        extensions: None,
        tool_selection: None,
    }))
}

#[tokio::test]
async fn navigate_tree_generates_and_persists_the_branch_summary() {
    let temp = TempDir::new("navigate-summary");
    let (session, ids) = branched_session();
    let [u1, _a1, _u2, u3] = [
        ids[0].clone(),
        ids[1].clone(),
        ids[2].clone(),
        ids[3].clone(),
    ];
    let agent = agent_with_session(
        session,
        fake_registry_for(Arc::new(|_index, _context| {
            assistant_text("## Goal\nbranch work", StopReason::Stop)
        })),
        &temp.0,
    );

    // Navigate from the branch leaf (u3) to the trunk user message u1: the
    // abandoned branch is summarized and attached at the target position
    // (u1's parent = root), and the target's text becomes the editor text.
    let navigation = agent
        .navigate_tree(
            &u1,
            true,
            None,
            false,
            pir::compaction::DEFAULT_BRANCH_SUMMARY_RESERVE_TOKENS,
        )
        .await
        .expect("storage configured")
        .expect("navigation");
    assert_eq!(navigation.editor_text.as_deref(), Some("first"));
    assert!(navigation.summary_entry_id.is_some());

    let (leaf, summaries) = agent
        .with_session_manager(|manager| {
            (
                manager.leaf_id().map(str::to_owned),
                manager
                    .entries()
                    .into_iter()
                    .filter_map(|entry| match entry {
                        FileEntry::BranchSummary(inner) => Some(inner),
                        _ => None,
                    })
                    .collect::<Vec<_>>(),
            )
        })
        .await
        .expect("session manager");
    assert_eq!(summaries.len(), 1);
    let summary = &summaries[0];
    assert_eq!(
        navigation.summary_entry_id.as_deref(),
        Some(summary.id.as_str())
    );
    // Summary attached at the target position (u1's parent = root), capturing
    // the abandoned path's from-id.
    assert_eq!(summary.parent_id, None);
    assert_eq!(summary.from_id, u3);
    assert!(
        summary
            .summary
            .contains("The user explored a different conversation branch before returning here."),
        "summary must carry the preamble: {}",
        summary.summary
    );
    assert!(summary.summary.contains("## Goal\nbranch work"));
    assert_eq!(
        summary.details.as_ref().map(|details| details["readFiles"]
            .as_array()
            .map(|items| items.len())
            .unwrap_or_default()),
        Some(0)
    );
    // With a summary, the attached summary entry becomes the new leaf
    // (upstream `branchWithSummary(newLeafId, ...)` — the same on the
    // no-summary path only a user-message target's parent reset applies).
    assert_eq!(
        leaf.as_deref(),
        navigation.summary_entry_id.as_deref(),
        "the attached summary entry is the new leaf"
    );
}

#[tokio::test]
async fn navigate_tree_without_summary_branches_plainly() {
    let temp = TempDir::new("navigate-plain");
    let (session, ids) = branched_session();
    let [u1, _a1, _u2, _u3] = [
        ids[0].clone(),
        ids[1].clone(),
        ids[2].clone(),
        ids[3].clone(),
    ];
    let agent = agent_with_session(
        session,
        fake_registry_for(Arc::new(|_index, _context| {
            panic!("no summarizer call is expected without summarize")
        })),
        &temp.0,
    );

    let navigation = agent
        .navigate_tree(&u1, false, None, false, 16_384)
        .await
        .expect("storage configured")
        .expect("navigation");
    assert_eq!(navigation.editor_text.as_deref(), Some("first"));
    assert!(navigation.summary_entry_id.is_none());

    let (leaf, summary_count) = agent
        .with_session_manager(|manager| {
            (
                manager.leaf_id().map(str::to_owned),
                manager
                    .entries()
                    .iter()
                    .filter(|entry| matches!(entry, FileEntry::BranchSummary(_)))
                    .count(),
            )
        })
        .await
        .expect("session manager");
    assert_eq!(leaf, None);
    assert_eq!(summary_count, 0);
}

#[tokio::test]
async fn navigate_tree_failure_leaves_the_leaf_unchanged() {
    let temp = TempDir::new("navigate-failure");
    let (session, ids) = branched_session();
    let [u1, _a1, _u2, _u3] = [
        ids[0].clone(),
        ids[1].clone(),
        ids[2].clone(),
        ids[3].clone(),
    ];
    let agent = agent_with_session(
        session,
        fake_registry_for(Arc::new(|_index, _context| {
            assistant_text("", StopReason::Error)
        })),
        &temp.0,
    );

    let outcome = agent
        .navigate_tree(&u1, true, None, false, 16_384)
        .await
        .expect("storage configured");
    assert!(outcome.is_err());

    // The leaf is still the old branch leaf and no summary was stored.
    let (leaf, summary_count) = agent
        .with_session_manager(|manager| {
            (
                manager.leaf_id().map(str::to_owned),
                manager
                    .entries()
                    .iter()
                    .filter(|entry| matches!(entry, FileEntry::BranchSummary(_)))
                    .count(),
            )
        })
        .await
        .expect("session manager");
    assert_eq!(leaf.as_deref(), Some(ids[3].as_str()));
    assert_eq!(summary_count, 0);

    // Unknown target: explicit error, no panic.
    let outcome = agent
        .navigate_tree("no-such-id", true, None, false, 16_384)
        .await
        .expect("storage configured");
    assert_eq!(outcome.err().as_deref(), Some("Entry no-such-id not found"));
}

// ---------------------------------------------------------------------------
// `navigate_tree` Esc-during-summary abort (MAPPING.md follow-up, upstream
// `abortBranchSummary` + `{cancelled: true, aborted: true}`)
// ---------------------------------------------------------------------------

/// A summarizer provider that only settles once the request's abort signal
/// fires — the `/tree` Esc path (upstream `abortBranchSummary()` aborts the
/// summarizer stream mid-flight). `started` records that the request reached
/// the provider; `aborted` that the cancellation arrived.
fn abort_aware_registry(
    started: Arc<AtomicBool>,
    aborted: Arc<AtomicBool>,
) -> Arc<ProviderRegistry> {
    struct AbortAwareProvider {
        started: Arc<AtomicBool>,
        aborted: Arc<AtomicBool>,
    }
    impl Provider for AbortAwareProvider {
        fn id(&self) -> &str {
            "test"
        }
        fn name(&self) -> &str {
            "Abort-aware fake provider"
        }
        fn get_models(&self) -> Vec<Model> {
            vec![fake_model(4096, 256)]
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
            let publisher = stream.clone();
            let signal = options.signal.clone();
            let started = Arc::clone(&self.started);
            let aborted = Arc::clone(&self.aborted);
            std::thread::spawn(move || {
                started.store(true, Ordering::SeqCst);
                if let Some(signal) = signal {
                    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
                    while !signal.is_aborted() && std::time::Instant::now() < deadline {
                        std::thread::sleep(std::time::Duration::from_millis(2));
                    }
                }
                aborted.store(true, Ordering::SeqCst);
                let mut message = assistant_text("partial branch text", StopReason::Aborted);
                message.error_message = Some("Request was aborted".into());
                publisher.push(AssistantMessageEventStream::error_event(
                    ErrorReason::Aborted,
                    message,
                ));
            });
            stream
        }
    }
    let mut registry = ProviderRegistry::default();
    registry.set_provider(Arc::new(AbortAwareProvider { started, aborted }));
    Arc::new(registry)
}

#[tokio::test]
async fn navigate_tree_abort_cancels_the_summary_and_keeps_the_leaf() {
    let temp = TempDir::new("navigate-abort");
    let (session, ids) = branched_session();
    let [u1, _a1, _u2, u3] = [
        ids[0].clone(),
        ids[1].clone(),
        ids[2].clone(),
        ids[3].clone(),
    ];
    let started = Arc::new(AtomicBool::new(false));
    let aborted = Arc::new(AtomicBool::new(false));
    let agent = agent_with_session(
        session,
        abort_aware_registry(Arc::clone(&started), Arc::clone(&aborted)),
        &temp.0,
    );

    // The summarization runs on a task; the test waits until the summarizer
    // request is in flight and then fires the cancellation, exactly like the
    // TUI's Esc handler (which reaches the latch out of band).
    let task_agent = Arc::clone(&agent);
    let target = u1.clone();
    let navigation = tokio::spawn(async move {
        task_agent
            .navigate_tree(
                &target,
                true,
                None,
                false,
                pir::compaction::DEFAULT_BRANCH_SUMMARY_RESERVE_TOKENS,
            )
            .await
    });
    while !started.load(Ordering::SeqCst) {
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    agent.abort_branch_summary();
    let outcome = navigation
        .await
        .expect("navigation task")
        .expect("storage configured")
        .expect("aborted navigation is not an error");

    assert!(
        aborted.load(Ordering::SeqCst),
        "the signal reached the request"
    );
    assert!(outcome.aborted, "upstream cancelled+aborted result");
    assert_eq!(outcome.summary_entry_id, None);
    assert_eq!(outcome.editor_text, None);

    // Post-abort state (upstream keeps the previous position): the leaf is
    // still the abandoned branch's leaf and no summary was persisted.
    let (leaf, summary_count) = agent
        .with_session_manager(|manager| {
            (
                manager.leaf_id().map(str::to_owned),
                manager
                    .entries()
                    .iter()
                    .filter(|entry| matches!(entry, FileEntry::BranchSummary(_)))
                    .count(),
            )
        })
        .await
        .expect("session manager");
    assert_eq!(leaf.as_deref(), Some(u3.as_str()));
    assert_eq!(summary_count, 0);
}

#[test]
fn branch_summary_abort_latch_arms_fresh_signals_and_ignores_stale_cancels() {
    let abort = pir::agent_session::BranchSummaryAbort::default();
    assert!(!abort.is_armed());

    // A stale cancel (Esc from a settled navigation) is a no-op: the next
    // navigation's fresh signal stays live.
    abort.cancel();
    let signal = abort.arm();
    assert!(abort.is_armed());
    assert!(!signal.is_aborted());
    assert!(!abort.is_cancelled());

    // Firing the latch aborts the armed signal, which the summarizer request
    // observes.
    abort.cancel();
    assert!(signal.is_aborted());
    assert!(abort.is_cancelled());

    // The next navigation re-arms a fresh, live signal.
    let next = abort.arm();
    assert!(!next.is_aborted());
    assert!(!abort.is_cancelled());
}
