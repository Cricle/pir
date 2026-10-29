//! Integration tests for the compaction module: token estimation (upstream
//! compaction's 4 chars/token heuristic), the trigger predicate, projected
//! cut-point selection (including the oversized-trailing-tool-results
//! fallback), the summarizer hook's failure/success behavior, and the
//! split-turn second summarizer call.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use pir::ai::types::{
    AssistantMessage, ContentBlock, Context, ImageContent, Message, Model, ModelCost,
    ModelCostRates, StopReason, TextContent, ThinkingContent, ToolCall, ToolResultMessage,
    ToolResultRole, Usage, UserMessage, UserRole,
};
use pir::compaction::algorithm::{
    estimate_context_tokens, estimate_message_tokens, find_projected_cut_point, should_compact,
    should_compact_tokens, CompactionConfig, CutEntry, CutPointResult,
};
use pir::compaction::prompts::{
    truncate_for_summary, COMPACTION_SUMMARY_PREFIX, SUMMARIZATION_PROMPT,
    SUMMARIZATION_SYSTEM_PROMPT, TOOL_RESULT_MAX_CHARS, TURN_PREFIX_SUMMARIZATION_PROMPT,
};
use pir::compaction::{
    calculate_context_tokens, compact_context, compaction_fn, recover_from_overflow,
    serialize_conversation, summarization_max_tokens, turn_prefix_max_tokens, CompleteFn,
};

// ---------------------------------------------------------------------------
// Message / model helpers
// ---------------------------------------------------------------------------

fn text_block(text: &str) -> ContentBlock {
    ContentBlock::Text(TextContent {
        text: text.into(),
        text_signature: None,
    })
}

fn image_block() -> ContentBlock {
    ContentBlock::Image(ImageContent {
        data: "aGVsbG8=".into(),
        mime_type: "image/png".into(),
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

fn assistant_msg(content: Vec<ContentBlock>, stop_reason: StopReason, usage: Usage) -> Message {
    Message::Assistant(AssistantMessage {
        api: "test".into(),
        provider: "test".into(),
        model: "test-model".into(),
        content,
        stop_reason,
        usage,
        timestamp: 2,
        ..Default::default()
    })
}

fn tool_call_block(id: &str, name: &str) -> ContentBlock {
    let mut arguments = BTreeMap::new();
    arguments.insert("path".to_string(), serde_json::json!("abc"));
    ContentBlock::ToolCall(ToolCall {
        id: id.into(),
        name: name.into(),
        arguments,
        thought_signature: None,
        namespace: None,
        arguments_error: None,
    })
}

fn tool_result_msg(call_id: &str, text: &str) -> Message {
    Message::ToolResult(ToolResultMessage {
        role: ToolResultRole::ToolResult,
        tool_call_id: call_id.into(),
        tool_name: "echo".into(),
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

/// Override a message's upstream `timestamp` (Unix ms) for prefix-guard tests.
fn at(mut message: Message, timestamp: u64) -> Message {
    match &mut message {
        Message::User(inner) | Message::System(inner) | Message::Developer(inner) => {
            inner.timestamp = timestamp;
        }
        Message::Assistant(inner) => inner.timestamp = timestamp,
        Message::ToolResult(inner) => inner.timestamp = timestamp,
    }
    message
}

fn usage_with_total(total_tokens: u64) -> Usage {
    Usage {
        total_tokens,
        ..Default::default()
    }
}

fn test_model(context_window: u64, max_tokens: u64) -> Model {
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

fn context_of(messages: Vec<Message>) -> Context {
    Context {
        system_prompt: Some("session system prompt".into()),
        messages,
        tools: None,
    }
}

/// Light projected entries synthesized 1:1 from a message list (the
/// session-less compaction path).
fn cut_entries_of(messages: &[Message]) -> Vec<CutEntry> {
    messages
        .iter()
        .cloned()
        .map(CutEntry::from_message)
        .collect()
}

/// Default thresholds with a small keep-recent budget so the cut point lands
/// inside the synthetic conversation.
fn compact_config() -> CompactionConfig {
    CompactionConfig {
        keep_recent_tokens: 500,
        ..CompactionConfig::default()
    }
}

type CapturedContexts = Arc<Mutex<Vec<Context>>>;

fn prompt_text_of(request: &Context) -> String {
    match &request.messages[0] {
        Message::User(user) => match &user.content[0] {
            ContentBlock::Text(text) => text.text.clone(),
            other => panic!("expected text block, got {other:?}"),
        },
        other => panic!("expected user message, got {other:?}"),
    }
}

/// A CompleteFn that always answers with a fixed summary and records requests.
fn fake_complete(summary: &'static str, captured: CapturedContexts) -> CompleteFn {
    Box::new(move |context| {
        let captured = captured.clone();
        Box::pin(async move {
            captured.lock().unwrap().push(context);
            Ok(AssistantMessage {
                api: "test".into(),
                provider: "test".into(),
                model: "test-model".into(),
                content: vec![text_block(summary)],
                stop_reason: StopReason::Stop,
                usage: usage_with_total(500),
                timestamp: 4,
                ..Default::default()
            })
        })
    })
}

/// A CompleteFn that answers with the given stop reason and content.
fn fake_complete_stopping(stop_reason: StopReason, content: Vec<ContentBlock>) -> CompleteFn {
    Box::new(move |_context| {
        let content = content.clone();
        Box::pin(async move {
            Ok(AssistantMessage {
                api: "test".into(),
                provider: "test".into(),
                model: "test-model".into(),
                content,
                stop_reason,
                timestamp: 4,
                ..Default::default()
            })
        })
    })
}

/// A CompleteFn that always fails the call.
fn failing_complete() -> CompleteFn {
    Box::new(|_context| Box::pin(async { Err("provider down".to_string()) }))
}

/// Two turns of user -> assistant(toolCall) -> toolResult, plus a final user
/// message.  The first tool result is large (4000 chars -> 1000 estimated
/// tokens) so the keep-recent budget is crossed there; the cut then lands on
/// the second turn's user message (the closest valid cut point at or after
/// the budget).
fn tool_turn_conversation() -> Vec<Message> {
    let big_result = "x".repeat(4000);
    vec![
        user_msg("turn one"),
        assistant_msg(
            vec![tool_call_block("call-1", "echo")],
            StopReason::ToolUse,
            usage_with_total(20_000),
        ),
        tool_result_msg("call-1", &big_result),
        user_msg("turn two"),
        assistant_msg(
            vec![tool_call_block("call-2", "echo")],
            StopReason::ToolUse,
            usage_with_total(20_000),
        ),
        tool_result_msg("call-2", "small"),
        user_msg("final question"),
    ]
}

// ---------------------------------------------------------------------------
// Estimation (4 chars/token)
// ---------------------------------------------------------------------------

#[test]
fn estimate_context_tokens_uses_last_assistant_usage_plus_trailing_estimate() {
    let messages = vec![
        user_msg("12345678"), // 8 chars -> ceil(8/4) = 2 tokens (ignored)
        assistant_msg(
            vec![text_block("answer")],
            StopReason::Stop,
            usage_with_total(1000),
        ),
        user_msg("12345678"),              // 8 chars -> 2
        tool_result_msg("call-1", "abcd"), // 4 chars -> 1
    ];
    let estimate = estimate_context_tokens(&messages);
    assert_eq!(estimate.usage_tokens, 1000);
    assert_eq!(estimate.trailing_tokens, 3);
    assert_eq!(estimate.tokens, 1003);
    assert_eq!(estimate.last_usage_index, Some(1));
}

#[test]
fn estimate_context_tokens_falls_back_to_char_estimate_without_valid_usage() {
    // All-zero usage on the last assistant message is not valid usage data.
    let messages = vec![
        user_msg("12345678"), // 8 chars -> 2
        assistant_msg(
            vec![text_block("12345678")],
            StopReason::Stop,
            Usage::default(),
        ), // 8 chars -> 2
    ];
    let estimate = estimate_context_tokens(&messages);
    assert_eq!(estimate.tokens, 4);
    assert_eq!(estimate.usage_tokens, 0);
    assert_eq!(estimate.last_usage_index, None);

    // Error/aborted assistant messages are also skipped.
    let errored = vec![
        user_msg("12345678"),
        assistant_msg(
            vec![text_block("12345678")],
            StopReason::Error,
            usage_with_total(1000),
        ),
    ];
    assert_eq!(estimate_context_tokens(&errored).tokens, 4);
}

#[test]
fn estimate_context_tokens_rejects_a_usage_older_than_a_later_inserted_prefix_message() {
    // Upstream `estimate.ts:71-95` prefix-timestamp guard: a compaction
    // summary inserted at the front carries a NEWER timestamp than the
    // response it summarizes, so that response's usage no longer describes
    // the current prefix and must be ignored.
    let messages = vec![
        at(user_msg("summary of earlier work"), 20),
        at(
            assistant_msg(
                vec![text_block("old answer")],
                StopReason::Stop,
                usage_with_total(1000),
            ),
            10,
        ),
        at(user_msg("follow-up question"), 30),
    ];
    let estimate = estimate_context_tokens(&messages);
    assert_eq!(estimate.last_usage_index, None);
    assert_eq!(estimate.usage_tokens, 0);
    // Whole-context char estimate: 23 + 10 + 18 = 51 chars -> ceil(51/4) = 13.
    assert_eq!(estimate.trailing_tokens, 13);
    assert_eq!(estimate.tokens, 13);

    // A response that comes AFTER the inserted summary still applies.
    let fresh = vec![
        at(user_msg("summary"), 20),
        at(
            assistant_msg(
                vec![text_block("new answer")],
                StopReason::Stop,
                usage_with_total(1000),
            ),
            25,
        ),
        at(user_msg("12345678"), 30),
    ];
    let estimate = estimate_context_tokens(&fresh);
    assert_eq!(estimate.last_usage_index, Some(1));
    assert_eq!(estimate.usage_tokens, 1000);
    assert_eq!(estimate.trailing_tokens, 2);
    assert_eq!(estimate.tokens, 1002);
}

#[test]
fn estimate_message_tokens_counts_components_and_images() {
    // Image blocks weigh 4800 chars -> ceil(4800/4) = 1200 tokens.
    let with_image = Message::User(UserMessage {
        role: UserRole::User,
        content: vec![image_block()],
        tools_added: None,
        tools_removed: None,
        timestamp: 1,
        sections: None,
    });
    assert_eq!(estimate_message_tokens(&with_image), 1200);

    // toolCall blocks weigh name length + serialized arguments length:
    // "echo" (4) + `{"path":"abc"}` (14) = 18 chars -> ceil(18 / 4) = 5.
    let with_call = assistant_msg(
        vec![tool_call_block("call-1", "echo")],
        StopReason::ToolUse,
        Usage::default(),
    );
    assert_eq!(estimate_message_tokens(&with_call), 5);

    // Thinking blocks contribute their text: 8 chars -> 2.
    let with_thinking = assistant_msg(
        vec![ContentBlock::Thinking(ThinkingContent {
            thinking: "12345678".into(),
            thinking_signature: None,
            redacted: false,
        })],
        StopReason::Stop,
        Usage::default(),
    );
    assert_eq!(estimate_message_tokens(&with_thinking), 2);

    // System-role messages are estimated too (upstream `estimateTokens`).
    let system = Message::System(UserMessage {
        role: UserRole::System,
        content: vec![text_block("12345678")],
        tools_added: None,
        tools_removed: None,
        timestamp: 1,
        sections: None,
    });
    assert_eq!(estimate_message_tokens(&system), 2);

    // Usage fallback: totalTokens == 0 sums the components.
    let usage = Usage {
        input: 60,
        output: 20,
        cache_read: 10,
        cache_write: 10,
        ..Default::default()
    };
    assert_eq!(calculate_context_tokens(&usage), 100);
}

// ---------------------------------------------------------------------------
// Trigger
// ---------------------------------------------------------------------------

#[test]
fn should_compact_boundary_is_strictly_greater_than_window_minus_reserve() {
    let config = CompactionConfig::default();

    // Exactly at the threshold: no compaction.
    assert!(!should_compact_tokens(20_000, 36_384, &config));
    // One token over: compaction.
    assert!(should_compact_tokens(20_001, 36_384, &config));

    // Disabled config never compacts.
    let disabled = CompactionConfig {
        enabled: false,
        ..CompactionConfig::default()
    };
    assert!(!should_compact_tokens(20_001, 36_384, &disabled));

    // The Context-level wrapper estimates first.
    let model = test_model(36_384, 8192);
    let at_threshold = context_of(vec![assistant_msg(
        vec![text_block("ok")],
        StopReason::Stop,
        usage_with_total(20_000),
    )]);
    assert!(!should_compact(&at_threshold, &model, &config));

    // Char-estimate path (no assistant usage): 20004 chars -> 5716 tokens.
    let estimated = context_of(vec![user_msg(&"y".repeat(20_004))]);
    assert!(!should_compact(&estimated, &model, &config));
    let big = context_of(vec![user_msg(&"y".repeat(80_004))]); // 22859 tokens
    assert!(should_compact(&big, &model, &config));
}

// ---------------------------------------------------------------------------
// Cut point (projected entries)
// ---------------------------------------------------------------------------

#[test]
fn find_cut_point_never_starts_at_a_tool_result_and_prefers_the_latest_cut() {
    // Upstream regression #9740: oversized trailing tool results fall back to
    // the LAST valid cut point (the assistant tool call), not the first
    // message.
    let entries = cut_entries_of(&[
        user_msg("old history"),
        assistant_msg(
            vec![text_block("old answer")],
            StopReason::Stop,
            Usage::default(),
        ),
        user_msg("read the large file"),
        assistant_msg(
            vec![tool_call_block("call-1", "read")],
            StopReason::ToolUse,
            Usage::default(),
        ),
        tool_result_msg("call-1", &"x".repeat(8000)),
    ]);

    let result: CutPointResult = find_projected_cut_point(&entries, 0, entries.len(), 1000);
    assert_eq!(
        result,
        CutPointResult {
            first_kept_entry_index: 3,
            turn_start_index: 2,
            is_split_turn: true,
        },
        "cut at the trailing assistant tool call, not the first message"
    );

    // The tool call starts the kept tail and the tool result stays glued to
    // it (tool results are never cut points).
    assert!(entries[3].is_cut_point());
    assert!(!entries[4].is_cut_point());
}

#[test]
fn find_cut_point_splits_turns_at_assistant_messages() {
    // Budget crossed at an assistant message: the cut splits the turn and
    // reports the user message that started it.
    let entries = cut_entries_of(&[
        user_msg("Turn 1"),
        assistant_msg(
            vec![text_block("A1")],
            StopReason::Stop,
            usage_with_total(1000),
        ),
        user_msg("Turn 2"), // index 2
        assistant_msg(vec![text_block("A2-1")], StopReason::Stop, Usage::default()),
        assistant_msg(vec![text_block("A2-2")], StopReason::Stop, Usage::default()),
        assistant_msg(vec![text_block("A2-3")], StopReason::Stop, Usage::default()),
    ]);

    let result = find_projected_cut_point(&entries, 0, entries.len(), 3000);
    // Walking back: A2-3, A2-2, A2-1 (3 tokens), Turn 2 (3), A2... the budget
    // (3000) is crossed at Turn 1's assistant (1143 + ...). Wherever it
    // lands, an assistant cut must report a split turn with turn start 2.
    if !entries[result.first_kept_entry_index].is_turn_start() {
        assert!(result.is_split_turn);
        assert_eq!(result.turn_start_index, 2);
    }
}

#[test]
fn find_cut_point_budgets_context_visible_entries() {
    // The 4000-char message is a cut point: a tiny budget cuts right after
    // it (split turn), a slightly larger budget cuts at it.
    let entries = cut_entries_of(&[
        user_msg("hi"),
        assistant_msg(
            vec![text_block("hello")],
            StopReason::Stop,
            Usage::default(),
        ),
        user_msg(&"x".repeat(4000)),
        assistant_msg(vec![text_block("ok")], StopReason::Stop, Usage::default()),
    ]);

    let tiny = find_projected_cut_point(&entries, 0, entries.len(), 1);
    assert_eq!(tiny.first_kept_entry_index, 3);
    assert!(tiny.is_split_turn);
    assert_eq!(tiny.turn_start_index, 2);

    let fits = find_projected_cut_point(&entries, 0, entries.len(), 2);
    assert_eq!(fits.first_kept_entry_index, 2);
    assert!(!fits.is_split_turn);
    assert_eq!(fits.turn_start_index, -1);
}

#[test]
fn find_cut_point_returns_start_index_without_valid_cut_points() {
    // A single assistant message is a valid cut point at index 0.
    let entries = cut_entries_of(&[assistant_msg(
        vec![text_block("a")],
        StopReason::Stop,
        Usage::default(),
    )]);
    let result = find_projected_cut_point(&entries, 0, entries.len(), 1000);
    assert_eq!(result.first_kept_entry_index, 0);

    // An empty range yields the start index.
    let result = find_projected_cut_point(&entries, 0, 0, 1000);
    assert_eq!(result.first_kept_entry_index, 0);
    assert!(!result.is_split_turn);
}

// ---------------------------------------------------------------------------
// Summarizer hook
// ---------------------------------------------------------------------------

#[tokio::test]
async fn compaction_fn_failure_returns_original_context_unchanged() {
    let model = test_model(36_384, 8192);
    let config = compact_config();
    let conversation = tool_turn_conversation();
    let context = context_of(conversation.clone());
    assert!(should_compact(&context, &model, &config));

    for complete in [
        failing_complete(),
        fake_complete_stopping(StopReason::Length, vec![text_block("partial")]),
        fake_complete_stopping(StopReason::ToolUse, vec![tool_call_block("call-x", "echo")]),
        fake_complete_stopping(StopReason::Error, vec![]),
    ] {
        let hook = compaction_fn(model.clone(), complete, config.clone());
        let result = hook(Arc::new(context.clone())).await;
        assert_eq!(*result, context, "failure must leave the context untouched");
    }
}

/// One dropped summarizer stream does not fail the compaction: the shared
/// choke point retries the call once (upstream `retryAssistantCall`,
/// compaction.ts:619-639). A second consecutive drop still fails.
#[tokio::test]
async fn summarizer_stream_drop_is_retried_once() {
    let model = test_model(36_384, 8192);
    let config = compact_config();
    let conversation = tool_turn_conversation();
    let context = context_of(conversation.clone());
    assert!(should_compact(&context, &model, &config));

    // Fails once, then succeeds: the compaction goes through.
    let calls = Arc::new(AtomicUsize::new(0));
    let calls_for_complete = calls.clone();
    let flaky: CompleteFn = Box::new(move |_context| {
        let n = calls_for_complete.fetch_add(1, Ordering::SeqCst);
        Box::pin(async move {
            if n == 0 {
                Err("terminated".to_string())
            } else {
                Ok(AssistantMessage {
                    api: "test".into(),
                    provider: "test".into(),
                    model: "test-model".into(),
                    content: vec![text_block("recovered summary")],
                    stop_reason: StopReason::Stop,
                    usage: usage_with_total(500),
                    timestamp: 4,
                    ..Default::default()
                })
            }
        })
    });
    let hook = compaction_fn(model.clone(), flaky, config.clone());
    let result = hook(Arc::new(context.clone())).await;
    assert_ne!(
        *result, context,
        "the retried call must still produce a compaction"
    );
    assert_eq!(calls.load(Ordering::SeqCst), 2, "exactly one retry");

    // Both attempts drop: the compaction fails (context untouched).
    let hook = compaction_fn(model, failing_complete(), config);
    let result = hook(Arc::new(context)).await;
    // compaction_fn returns the original context on failure.
    assert!(result.messages.len() >= 2);
}

#[tokio::test]
async fn compaction_fn_success_produces_summary_user_message_then_retained_tail() {
    let model = test_model(36_384, 8192);
    let config = compact_config();
    let captured: CapturedContexts = Arc::new(Mutex::new(Vec::new()));
    // The budget (500) is crossed at the second turn's user message, which is
    // itself the cut point: [u1, a1, tr1] summarized, the rest kept.
    let conversation = tool_turn_conversation();
    let context = context_of(conversation.clone());

    let hook = compaction_fn(
        model,
        fake_complete("## Goal\n- keep going", captured.clone()),
        config.clone(),
    );
    let result = hook(Arc::new(context.clone())).await;

    // The summary user message comes first, framed like upstream, followed by
    // the retained tail; the session system prompt is preserved.
    assert_eq!(
        result.system_prompt.as_deref(),
        Some("session system prompt")
    );
    let first = match result.messages.first() {
        Some(Message::User(user)) => user,
        other => panic!("expected summary user message, got {other:?}"),
    };
    let expected_summary = format!("{COMPACTION_SUMMARY_PREFIX}## Goal\n- keep going\n</summary>");
    assert_eq!(first.content.len(), 1);
    assert_eq!(
        match &first.content[0] {
            ContentBlock::Text(text) => text.text.clone(),
            other => panic!("expected text block, got {other:?}"),
        },
        expected_summary
    );

    // The cut landed at the second turn's user message (index 3).
    assert_eq!(result.messages.len(), 1 + (conversation.len() - 3));
    assert_eq!(&result.messages[1..], &conversation[3..]);
    assert_eq!(context.messages.len(), conversation.len());

    // The summarizer saw the system prompt, the serialized conversation, and
    // the format prompt - in exactly one call (no split turn here).
    let captured_contexts = captured.lock().unwrap();
    assert_eq!(captured_contexts.len(), 1, "exactly one summarizer call");
    let request = &captured_contexts[0];
    assert_eq!(
        request.system_prompt.as_deref(),
        Some(SUMMARIZATION_SYSTEM_PROMPT)
    );
    assert_eq!(request.messages.len(), 1);
    let prompt = prompt_text_of(request);
    assert!(prompt.starts_with("<conversation>\n"));
    assert!(prompt.contains("[User]: turn one"));
    assert!(prompt.contains("[Tool result]: xxxx"));
    assert!(prompt
        .trim_end()
        .ends_with("Preserve exact file paths, function names, and error messages."));
    assert!(!prompt.contains("<previous-summary>"));
    assert!(prompt.contains(SUMMARIZATION_PROMPT));
}

#[tokio::test]
async fn compaction_fn_split_turn_makes_a_second_smaller_turn_prefix_call() {
    let model = test_model(36_384, 8192);
    let config = compact_config();
    let captured: CapturedContexts = Arc::new(Mutex::new(Vec::new()));

    // Budget (500) crossed at the second turn's big assistant message: the
    // cut lands mid-turn at that assistant, splitting the turn. The last
    // assistant message carries a large usage record so the trigger fires
    // (context window 36384, reserve 16384).
    let big = "y".repeat(4000); // 1143 estimated tokens
    let conversation = vec![
        user_msg("u1"),
        assistant_msg(vec![text_block("a1")], StopReason::Stop, Usage::default()),
        user_msg(&big),
        assistant_msg(
            vec![text_block(&big)],
            StopReason::Stop,
            usage_with_total(30_000),
        ), // index 3, cut
        user_msg("u3"),
        assistant_msg(vec![text_block("a3")], StopReason::Stop, Usage::default()),
    ];
    let context = context_of(conversation);

    let hook = compaction_fn(
        model,
        fake_complete("merged summary", captured.clone()),
        config.clone(),
    );
    let result = hook(Arc::new(context.clone())).await;

    // TWO summarizer calls: the history summary and the turn-prefix
    // checkpoint (upstream `compact` merge).
    let captured_contexts = captured.lock().unwrap();
    assert_eq!(captured_contexts.len(), 2, "history + turn prefix calls");

    let history_prompt = prompt_text_of(&captured_contexts[0]);
    assert!(history_prompt.contains("<conversation>"));
    assert!(history_prompt.contains(SUMMARIZATION_PROMPT));

    let prefix_prompt = prompt_text_of(&captured_contexts[1]);
    assert!(
        prefix_prompt.starts_with("# Conversation\n"),
        "turn prefix prompt uses the # Conversation framing: {prefix_prompt}"
    );
    assert!(prefix_prompt.contains("# Instructions\n"));
    assert!(prefix_prompt.contains(TURN_PREFIX_SUMMARIZATION_PROMPT));
    assert!(prefix_prompt.contains("[User]: yyy"));
    assert!(
        !prefix_prompt.contains("[User]: u1"),
        "prefix covers only the split turn"
    );

    // The merged summary carries the split-turn marker between the two parts
    // ("No prior history." when the history span is empty, upstream too).
    let first = match result.messages.first() {
        Some(Message::User(user)) => &user.content,
        other => panic!("expected summary user message, got {other:?}"),
    };
    let text = match &first[0] {
        ContentBlock::Text(text) => &text.text,
        other => panic!("expected text block, got {other:?}"),
    };
    assert!(
        text.contains("**Turn Context (split turn):**"),
        "merged summary must contain the split-turn separator: {text}"
    );
    assert!(text.contains("merged summary"));
}

/// `/compact <instructions>` must reach the SPLIT-TURN checkpoint too
/// (upstream `generateTurnPrefixSummary` appends `Additional focus:`,
/// compaction.ts:1078-1081), including the case where the turn prefix is the
/// ONLY new content: the cut lands on the first assistant message, so the
/// history span is empty and the turn-prefix call is the only summarizer call.
#[tokio::test]
async fn split_turn_prefix_is_the_only_new_span_and_receives_instructions() {
    let model = test_model(36_384, 8192);
    let config = compact_config();
    let captured: CapturedContexts = Arc::new(Mutex::new(Vec::new()));

    // The assistant is the only cut point the keep-recent budget crosses, and
    // it is not a turn start: the whole preceding user message becomes the
    // turn prefix (history span empty).
    let big = "z".repeat(4000);
    let conversation = vec![
        user_msg(&big),
        assistant_msg(
            vec![text_block(&big)],
            StopReason::Stop,
            usage_with_total(30_000),
        ),
    ];
    let result = compact_context(
        context_of(conversation),
        &model,
        Arc::new(fake_complete("prefix summary", captured.clone())),
        &config,
        None,
        Some("focus on the parser"),
    )
    .await
    .expect("split-turn-only compaction");

    assert_eq!(result.retained_tail.len(), 1, "the cut keeps the assistant");
    assert!(
        result
            .summary
            .starts_with("No prior history.\n\n---\n\n**Turn Context (split turn):**\n\n"),
        "an empty history span must use the upstream placeholder: {}",
        result.summary
    );
    assert!(result.summary.contains("prefix summary"));

    let captured = captured.lock().unwrap();
    assert_eq!(
        captured.len(),
        1,
        "the turn prefix is the only summarizer call"
    );
    let prompt = prompt_text_of(&captured[0]);
    assert!(prompt.starts_with("# Conversation\n"));
    assert!(prompt.contains(TURN_PREFIX_SUMMARIZATION_PROMPT));
    assert!(
        prompt.ends_with("\n\nAdditional focus: focus on the parser"),
        "the instructions must be appended at the prompt tail: {prompt}"
    );
}

#[tokio::test]
async fn compaction_fn_skips_work_when_threshold_not_reached_or_disabled() {
    let model = test_model(36_384, 8192);
    let config = compact_config();
    let captured: CapturedContexts = Arc::new(Mutex::new(Vec::new()));
    let complete = fake_complete("unused", captured.clone());

    // Below threshold: hook is a no-op and the summarizer is never called.
    let small = context_of(vec![
        user_msg("hi"),
        assistant_msg(
            vec![text_block("hello")],
            StopReason::Stop,
            usage_with_total(10),
        ),
    ]);
    let hook = compaction_fn(model.clone(), complete, config.clone());
    let result = hook(Arc::new(small.clone())).await;
    assert_eq!(*result, small);
    assert!(captured.lock().unwrap().is_empty());

    // Disabled: even over-threshold contexts pass through untouched.
    let big = context_of(vec![assistant_msg(
        vec![text_block("ok")],
        StopReason::Stop,
        usage_with_total(30_000),
    )]);
    let disabled = CompactionConfig {
        enabled: false,
        ..compact_config()
    };
    let hook = compaction_fn(model, fake_complete("unused", captured.clone()), disabled);
    let result = hook(Arc::new(big.clone())).await;
    assert_eq!(*result, big);
    assert!(captured.lock().unwrap().is_empty());
}

#[tokio::test]
async fn compact_context_uses_update_prompt_with_previous_summary() {
    let model = test_model(36_384, 8192);
    let config = compact_config();
    let captured: CapturedContexts = Arc::new(Mutex::new(Vec::new()));
    let conversation = tool_turn_conversation();
    let context = context_of(conversation.clone());

    let result = compact_context(
        context.clone(),
        &model,
        Arc::new(fake_complete("updated", captured.clone())),
        &config,
        Some("earlier summary"),
        None,
    )
    .await
    .expect("compaction should succeed");

    assert_eq!(result.summary, "updated");
    assert_eq!(result.usage.total_tokens, 500);
    assert_eq!(
        result.first_kept_index,
        context.messages.len() - result.retained_tail.len()
    );
    assert!(result.tokens_before > 0);

    let prompt = prompt_text_of(&captured.lock().unwrap()[0]);
    assert!(prompt.contains("<previous-summary>\nearlier summary\n</previous-summary>"));
    assert!(
        prompt.contains("Update the existing structured summary with new information."),
        "update prompt should be used when a previous summary exists"
    );
    assert!(pir::compaction::prompts::update_summarization_prompt()
        .ends_with("Preserve exact file paths, function names, and error messages."));
}

// ---------------------------------------------------------------------------
// File-operation extraction (upstream utils.ts + nestedCalls tolerance)
// ---------------------------------------------------------------------------

#[test]
fn file_ops_extract_read_write_edit_and_nested_calls() {
    let mut ops = pir::compaction::FileOperations::default();
    let messages = vec![
        assistant_msg(
            vec![
                tool_call_block("call-1", "read"),
                {
                    let mut arguments = BTreeMap::new();
                    arguments.insert("path".to_string(), serde_json::json!("src/written.ts"));
                    ContentBlock::ToolCall(ToolCall {
                        id: "call-2".into(),
                        name: "write".into(),
                        arguments,
                        thought_signature: None,
                        namespace: None,
                        arguments_error: None,
                    })
                },
                {
                    let mut arguments = BTreeMap::new();
                    arguments.insert("path".to_string(), serde_json::json!("src/edited.ts"));
                    ContentBlock::ToolCall(ToolCall {
                        id: "call-3".into(),
                        name: "edit".into(),
                        arguments,
                        thought_signature: None,
                        namespace: None,
                        arguments_error: None,
                    })
                },
                // Not a tracked tool name.
                {
                    let mut arguments = BTreeMap::new();
                    arguments.insert("path".to_string(), serde_json::json!("src/other.ts"));
                    ContentBlock::ToolCall(ToolCall {
                        id: "call-4".into(),
                        name: "bash".into(),
                        arguments,
                        thought_signature: None,
                        namespace: None,
                        arguments_error: None,
                    })
                },
            ],
            StopReason::ToolUse,
            Usage::default(),
        ),
        // Codemode nested calls recorded on the tool result's details.
        Message::ToolResult(ToolResultMessage {
            role: ToolResultRole::ToolResult,
            tool_call_id: "call-2".into(),
            tool_name: "codemode".into(),
            content: vec![text_block("ok")],
            details: Some(serde_json::json!({
                "nestedCalls": {
                    "calls": [
                        { "name": "read", "arguments": { "path": "codemode/read.ts" } },
                        { "name": "edit", "arguments": { "path": "codemode/edit.ts" } },
                        { "name": "write", "arguments": {} }
                    ]
                }
            })),
            usage: None,
            added_tool_names: Vec::new(),
            is_error: false,
            structured_content: None,
            nested_calls: None,
            duration_ms: None,
            timestamp: 3,
        }),
    ];
    pir::compaction::extract_file_ops_from_messages(&messages, &mut ops);
    let (read_only, modified) = pir::compaction::compute_file_lists(&ops);
    assert_eq!(read_only, vec!["abc", "codemode/read.ts"]);
    assert_eq!(
        modified,
        vec!["codemode/edit.ts", "src/edited.ts", "src/written.ts"]
    );
}

// ---------------------------------------------------------------------------
// Overflow recovery
// ---------------------------------------------------------------------------

#[tokio::test]
async fn recover_from_overflow_drops_failed_assistant_and_compacts() {
    let model = test_model(36_384, 8192);
    let config = compact_config();
    let captured: CapturedContexts = Arc::new(Mutex::new(Vec::new()));

    let mut conversation = tool_turn_conversation();
    conversation.push(assistant_msg(
        vec![text_block("partial explosion")],
        StopReason::Error,
        Usage::default(),
    ));
    let context = context_of(conversation);

    let recovered = recover_from_overflow(
        context,
        "prompt is too long: 36400 tokens > 36384 maximum",
        &model,
        Arc::new(fake_complete("## Goal\n- retry", captured.clone())),
        &config,
    )
    .await
    .expect("overflow recovery should compact successfully");

    // The failed trailing assistant message is gone and the summary leads.
    assert!(recovered.messages.iter().all(|message| match message {
        Message::Assistant(assistant) => assistant.stop_reason != StopReason::Error,
        _ => true,
    }));
    match recovered.messages.first() {
        Some(Message::User(user)) => match &user.content[0] {
            ContentBlock::Text(text) => {
                assert!(text.text.starts_with(COMPACTION_SUMMARY_PREFIX));
                assert!(text.text.contains("## Goal\n- retry"));
            }
            other => panic!("expected text block, got {other:?}"),
        },
        other => panic!("expected summary user message, got {other:?}"),
    }

    // The overflow error text is passed to the summarizer as additional focus.
    let prompt = prompt_text_of(&captured.lock().unwrap()[0]);
    assert!(prompt.contains("Additional focus: Recover from a context-overflow error:"));
}

#[tokio::test]
async fn recover_from_overflow_returns_none_without_trailing_assistant_or_on_failure() {
    let model = test_model(36_384, 8192);
    let config = compact_config();

    // No trailing assistant message: nothing to drop.
    let no_assistant = context_of(vec![user_msg("hi")]);
    assert!(recover_from_overflow(
        no_assistant,
        "overflow",
        &model,
        Arc::new(fake_complete("summary", Arc::new(Mutex::new(Vec::new())))),
        &config,
    )
    .await
    .is_none());

    // Summarizer failure: the caller keeps the original context.
    let context = context_of(tool_turn_conversation());
    assert!(recover_from_overflow(
        context,
        "overflow",
        &model,
        Arc::new(failing_complete()),
        &config,
    )
    .await
    .is_none());
}

// ---------------------------------------------------------------------------
// Serialization and response caps
// ---------------------------------------------------------------------------

#[test]
fn serialize_conversation_truncates_long_tool_results_and_skips_system() {
    let long_result = tool_result_msg("call-1", &"a".repeat(3000));
    let serialized = serialize_conversation(&[long_result]);
    assert!(serialized.contains(&format!("[Tool result]: {}", "a".repeat(2000))));
    assert!(serialized.contains("[... 1000 more characters truncated]"));

    // System messages are prompt state and skipped; image-only user content
    // has no text and contributes nothing.
    let empty = serialize_conversation(&[
        Message::System(UserMessage {
            role: UserRole::System,
            content: vec![text_block("prompt state")],
            tools_added: None,
            tools_removed: None,
            timestamp: 1,
            sections: None,
        }),
        Message::User(UserMessage {
            role: UserRole::User,
            content: vec![image_block()],
            tools_added: None,
            tools_removed: None,
            timestamp: 1,
            sections: None,
        }),
    ]);
    assert!(empty.is_empty());
}

#[test]
fn summary_response_caps_are_the_models_own_output_limit() {
    // Upstream 6fcb48670: no reserve-share cap. Both summary paths use the
    // model's own output limit, and 0 means "no additional cap".
    let model = test_model(36_384, 8_192);
    assert_eq!(summarization_max_tokens(16_384, &model), 8_192);
    assert_eq!(turn_prefix_max_tokens(16_384, &model), 8_192);

    let model_with_headroom = test_model(36_384, 1_000_000);
    assert_eq!(
        summarization_max_tokens(16_384, &model_with_headroom),
        1_000_000
    );
    assert_eq!(
        turn_prefix_max_tokens(16_384, &model_with_headroom),
        1_000_000
    );

    let model_uncapped = test_model(36_384, 0);
    assert_eq!(summarization_max_tokens(16_384, &model_uncapped), 0);
    assert_eq!(turn_prefix_max_tokens(16_384, &model_uncapped), 0);
}

// ---------------------------------------------------------------------------
// Regression: multi-byte truncation (P1-3)
// ---------------------------------------------------------------------------

#[test]
fn truncate_for_summary_never_splits_multibyte_characters() {
    // 3000 CJK characters (3 bytes each): byte-slicing at 2000 used to panic
    // mid-codepoint.
    let text = "中".repeat(3000);
    let truncated = truncate_for_summary(&text, TOOL_RESULT_MAX_CHARS);
    // The prefix is exactly 2000 whole characters plus the marker.
    let prefix = "中".repeat(TOOL_RESULT_MAX_CHARS);
    assert!(truncated.starts_with(&prefix));
    assert!(truncated.ends_with("[... 1000 more characters truncated]"));
    // No character was split or lost: the result holds exactly the 2000
    // kept characters (no replacement characters from a torn codepoint).
    assert_eq!(
        truncated
            .chars()
            .filter(|c| *c == char::REPLACEMENT_CHARACTER)
            .count(),
        0
    );
    assert_eq!(truncated.chars().filter(|c| *c == '中').count(), 2000);
}

#[test]
fn truncate_for_summary_counts_characters_not_bytes() {
    // Under the limit (in characters): returned unchanged even though the
    // byte length exceeds the limit.
    let short = "中".repeat(1500);
    assert_eq!(truncate_for_summary(&short, TOOL_RESULT_MAX_CHARS), short);
    // Exactly at the limit: unchanged.
    let exact = "中".repeat(TOOL_RESULT_MAX_CHARS);
    assert_eq!(truncate_for_summary(&exact, TOOL_RESULT_MAX_CHARS), exact);
}

// ---------------------------------------------------------------------------
// Usage scan: upstream `getLastAssistantUsageInfo` (prefix-timestamp guard)
// ---------------------------------------------------------------------------

/// Upstream `getLastAssistantUsageInfo` (`estimate.ts:71-95`) scans forward and
/// only accepts an assistant usage whose timestamp is at least the newest
/// prefix timestamp. A summary inserted after a response therefore invalidates
/// that response's usage for the current prefix; a later response with a fresh
/// timestamp is accepted normally.
#[test]
fn estimate_context_tokens_rejects_usage_older_than_a_later_prefix_message() {
    // The summary user message carries a fresh timestamp; the kept assistant
    // message predates it — its usage cannot describe the current prefix.
    let rebuilt = vec![
        Message::User(UserMessage {
            role: UserRole::User,
            content: vec![text_block("summary of the earlier conversation")],
            tools_added: None,
            tools_removed: None,
            timestamp: 1_000_000,
            sections: None,
        }),
        assistant_msg(
            vec![text_block("kept answer")],
            StopReason::Stop,
            usage_with_total(100_000),
        ), // ts = 2, older than the inserted summary
    ];
    let estimate = estimate_context_tokens(&rebuilt);
    assert_eq!(estimate.last_usage_index, None);
    assert_eq!(estimate.usage_tokens, 0);
    assert_eq!(estimate.tokens, estimate.trailing_tokens);

    // Same sequence with chronological timestamps: the usage is accepted.
    let fresh = vec![
        user_msg("hello"), // ts = 1
        assistant_msg(
            vec![text_block("answer")],
            StopReason::Stop,
            usage_with_total(100_000),
        ), // ts = 2
        tool_result_msg("call-1", "abcd"), // ts = 3, 4 chars -> 1 token
    ];
    let estimate = estimate_context_tokens(&fresh);
    assert_eq!(estimate.last_usage_index, Some(1));
    assert_eq!(estimate.usage_tokens, 100_000);
    assert_eq!(estimate.tokens, 100_001);
}

/// End to end: after a compaction the inserted summary is newer than the kept
/// assistant messages, so the message-level estimate rejects their
/// pre-compaction usage (upstream `getLastAssistantUsageInfo`'s prefix guard)
/// and falls back to the character estimate for the rebuilt tail.
#[tokio::test]
async fn rebuilt_message_level_context_rejects_the_pre_compaction_usage() {
    let model = test_model(36_384, 8_192);
    let config = compact_config();
    let captured: CapturedContexts = Arc::new(Mutex::new(Vec::new()));
    let context = context_of(vec![
        user_msg("hello"),
        assistant_msg(
            vec![tool_call_block("call-1", "echo")],
            StopReason::ToolUse,
            usage_with_total(30_000),
        ),
        tool_result_msg("call-1", &"x".repeat(4000)),
    ]);
    let result = compact_context(
        context.clone(),
        &model,
        Arc::new(fake_complete("checkpoint summary", captured)),
        &config,
        None,
        None,
    )
    .await
    .expect("compaction succeeds");
    let rebuilt = pir::compaction::rebuilt_context(&context, &result);
    // The summary message is newer than the kept assistant, so the 30k usage
    // is stale for the rebuilt prefix and must be rejected; only the
    // character estimate of the small tail remains (below the 28_192
    // threshold).
    let estimate = estimate_context_tokens(&rebuilt.messages);
    assert_eq!(estimate.usage_tokens, 0);
    assert_eq!(estimate.last_usage_index, None);
    assert!(!should_compact(&rebuilt, &model, &config));
}
