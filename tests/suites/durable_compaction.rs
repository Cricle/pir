//! `durable::harness::compaction` — the built-in `pi.compaction` task (spec
//! §8.7).
//!
//! Pure coverage of range selection, trigger thresholds and the summarization
//! text pipeline, plus end-to-end runs of [`compaction_task`] through a real
//! [`TaskScheduler`] and [`MemoryStorage`]: hook decline and supply, stale
//! settlement, retry backoff, and failure presentation in `pi.live`.
//!
//! The task's harness services (`agent`, `model`, `context`, `complete`,
//! `place`) are a [`CompactionHost`]; the tests implement that seam directly so
//! every durable assertion below is about committed state.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::json;

use pir::ai::types::{
    AssistantMessage, AssistantRole, CacheRetention, ContentBlock, Message, StopReason,
    TextContent, ThinkingLevel, ToolCall, ToolResultMessage, ToolResultRole, Usage, UserMessage,
    UserRole,
};
use pir::durable::documents::{DocOwner, DocumentSemantics, ResolvedAddress};
use pir::durable::harness::agent::ModelRef;
use pir::durable::harness::compaction::{
    active_range_start, add_compaction_status, calculate_context_tokens, compaction_ownership,
    compaction_policy, compaction_request_id, compaction_status_of, compaction_task,
    estimate_context, estimate_message_tokens, fail_compaction_status, overflow_compaction,
    placement_action, remove_compaction_status, retry_until, select_cut, serialize_conversation,
    summarized_messages, summary_entry, summary_entry_draft, summary_failure, summary_is_stale,
    summary_max_tokens, summary_prompt, summary_request_options, summary_text,
    threshold_compaction, wrap_summary, CompactionAgent, CompactionCheckpoint,
    CompactionCompletion, CompactionDecision, CompactionDraft, CompactionHooks, CompactionHost,
    CompactionInput, CompactionModel, CompactionReason, CompactionResult, CompactionTaskError,
    CompactionTrigger, NoCompactionHooks, PlacedSummary, PlacementAction, SummaryPlacement,
    SummaryRequest, COMPACTION_TASK_NAME, COMPACTION_TASK_VERSION, SUMMARY_PREFIX, SUMMARY_SUFFIX,
    TOOL_RESULT_MAX_CHARS,
};
use pir::durable::harness::context::derive_context;
use pir::durable::harness::live::{live_from_object, live_to_object, LIVE_DEFINITION, LIVE_KIND};
use pir::durable::harness::scheduler::{TaskRuntimeError, TaskScheduler, TaskSchedulerOptions};
use pir::durable::harness::settings::Settings;
use pir::durable::harness::task::BoxFuture;
use pir::durable::ids::{id_from_number, ConversationId, EntryId, SubmissionId, TaskId};
use pir::durable::session::facade::ConversationOwnership;
use pir::durable::session::transaction::{HeadTarget, TaskOptions, TaskOwnership};
use pir::durable::storage::memory::MemoryStorage;
use pir::durable::types::{
    ConversationRecord, DocumentPoint, EntryRecord, Storage, StorageWrite, TaskOutcome, TaskRecord,
    TaskState,
};
use pir::durable::Context;

// ── fixtures ───────────────────────────────────────────────────────────────

fn entry_id(value: i64) -> EntryId {
    id_from_number(value)
}

fn user(text: &str) -> Message {
    Message::User(UserMessage {
        role: UserRole::User,
        content: vec![ContentBlock::Text(TextContent {
            text: text.to_owned(),
            text_signature: None,
        })],
        tools_added: None,
        tools_removed: None,
        sections: None,
        timestamp: 1,
    })
}

fn assistant(text: &str) -> Message {
    assistant_with_calls(text, &[])
}

fn assistant_with_calls(text: &str, calls: &[&str]) -> Message {
    let mut content = vec![ContentBlock::Text(TextContent {
        text: text.to_owned(),
        text_signature: None,
    })];
    content.extend(calls.iter().map(|call| {
        ContentBlock::ToolCall(ToolCall {
            id: (*call).to_owned(),
            name: "read".to_owned(),
            arguments: Default::default(),
            thought_signature: None,
            namespace: None,
            arguments_error: None,
        })
    }));
    Message::Assistant(AssistantMessage {
        role: AssistantRole::Assistant,
        content,
        api: "test".to_owned(),
        provider: "test".to_owned(),
        model: "model".to_owned(),
        usage: Usage::default(),
        stop_reason: StopReason::Stop,
        timestamp: 1,
        ..AssistantMessage::default()
    })
}

fn tool_result(call: &str, text: &str) -> Message {
    Message::ToolResult(ToolResultMessage {
        role: ToolResultRole::ToolResult,
        tool_call_id: call.to_owned(),
        tool_name: "read".to_owned(),
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
        timestamp: 1,
    })
}

fn entry(id: i64, kind: &str, messages: Vec<Message>, head: Option<EntryId>) -> EntryRecord {
    EntryRecord {
        id: entry_id(id),
        conversation_id: id_from_number(1),
        kind: kind.to_owned(),
        model: Some(
            messages
                .iter()
                .map(|message| serde_json::to_value(message).expect("message serializes"))
                .collect(),
        ),
        data: None,
        head,
        edits: None,
        by_task_id: None,
    }
}

fn view(entries: Vec<EntryRecord>) -> pir::durable::harness::context::ContextView {
    derive_context(&entries, None)
}

fn span(tokens: u64) -> String {
    // chars_per_token is 3.5, so an even token count is `tokens * 7 / 2` chars.
    "x".repeat(usize::try_from(tokens.saturating_mul(7) / 2).unwrap_or(0))
}

fn live_address(conversation: ConversationId) -> ResolvedAddress {
    ResolvedAddress::new(
        LIVE_KIND,
        DocumentSemantics::ConversationLatest {
            fork: pir::durable::types::DocumentFork::Initial,
        },
        Some(DocOwner::Conversation(conversation)),
        None,
    )
    .expect("a conversation addresses pi.live")
}

// ── range selection ────────────────────────────────────────────────────────

#[test]
fn cut_matches_the_spec_example() {
    // 1 user, 2 assistant(read), 3 tool result 30k tokens, 4 assistant, 5 user,
    // 6 assistant. keepRecentTokens 20k: the walk reaches 20k at 3, and the
    // first candidate at or after 3 is 4.
    let entries = vec![
        entry(1, "pi.user", vec![user("go")], None),
        entry(
            2,
            "pi.assistant",
            vec![assistant_with_calls("read", &["c1"])],
            None,
        ),
        entry(
            3,
            "pi.tool-result",
            vec![tool_result("c1", &span(30_000))],
            None,
        ),
        entry(4, "pi.assistant", vec![assistant("done")], None),
        entry(5, "pi.user", vec![user("next")], None),
        entry(6, "pi.assistant", vec![assistant("answer")], None),
    ];
    let view = view(entries);
    let cut = select_cut(&view, 20_000).expect("a cut exists");
    assert_eq!(cut, 3, "the summary covers entries 1-3");
    assert_eq!(view.entries[cut].id, entry_id(4), "first kept is entry 4");
}

#[test]
fn cut_is_none_below_the_keep_threshold() {
    let entries = vec![
        entry(1, "pi.user", vec![user("hello")], None),
        entry(2, "pi.assistant", vec![assistant("hi")], None),
    ];
    let view = view(entries);
    assert_eq!(select_cut(&view, 20_000), None);
}

#[test]
fn cut_skips_a_user_entry_whose_tool_result_follows() {
    // Entry 3 is a user entry that a result of entry 2's call still follows
    // before the next assistant, so it is not a candidate. The walk stops at
    // the heavy entry 3, and the cut is the first candidate at or after it:
    // entry 5. Were entry 3 a candidate, the cut would be 3.
    let entries = vec![
        entry(1, "pi.user", vec![user("go")], None),
        entry(
            2,
            "pi.assistant",
            vec![assistant_with_calls("read", &["c1"])],
            None,
        ),
        entry(3, "pi.user", vec![user(&span(30_000))], None),
        entry(4, "pi.tool-result", vec![tool_result("c1", "done")], None),
        entry(5, "pi.assistant", vec![assistant("after")], None),
        entry(6, "pi.user", vec![user("next")], None),
    ];
    let view = view(entries);
    assert_eq!(select_cut(&view, 20_000), Some(4));
}

#[test]
fn cut_starts_after_a_head_marker() {
    // The head marker itself is never a candidate, and nothing precedes it, so
    // a marker plus one entry has nothing to compact.
    let entries = vec![
        entry(
            2,
            "pi.compaction",
            vec![user("earlier summary")],
            Some(entry_id(2)),
        ),
        entry(3, "pi.assistant", vec![assistant("after")], None),
    ];
    let view = view(entries);
    assert_eq!(select_cut(&view, 1), None);
    assert_eq!(active_range_start(&view), Some(entry_id(2)));
}

#[test]
fn summarized_messages_order_tool_results_and_synthesize_missing_ones() {
    let entries = vec![
        entry(
            1,
            "pi.assistant",
            vec![assistant_with_calls("read", &["c1", "c2"])],
            None,
        ),
        entry(2, "pi.tool-result", vec![tool_result("c2", "second")], None),
    ];
    let view = view(entries);
    let messages = summarized_messages(&view, 2);
    assert_eq!(messages.len(), 3, "the assistant and both results");
    match &messages[1] {
        Message::ToolResult(result) => assert_eq!(result.tool_call_id, "c1"),
        other => panic!("expected the synthesized c1 result, got {other:?}"),
    }
    match &messages[2] {
        Message::ToolResult(result) => assert_eq!(result.tool_call_id, "c2"),
        other => panic!("expected the c2 result, got {other:?}"),
    }
}

// ── triggers ───────────────────────────────────────────────────────────────

fn trigger_view() -> pir::durable::harness::context::ContextView {
    let entries = vec![
        entry(1, "pi.user", vec![user(&span(10))], None),
        entry(2, "pi.assistant", vec![assistant(&span(10))], None),
        entry(3, "pi.user", vec![user(&span(10))], None),
        entry(4, "pi.assistant", vec![assistant(&span(10))], None),
    ];
    view(entries)
}

fn policy(
    reserve: u64,
    background: u64,
    keep: u64,
) -> pir::durable::harness::settings::CompactionPolicy {
    pir::durable::harness::settings::CompactionPolicy {
        reserve_tokens: reserve,
        background_tokens: background,
        keep_recent_tokens: keep,
        ..Default::default()
    }
}

#[test]
fn threshold_compaction_reports_blocking_and_background() {
    let view = trigger_view();
    let tokens = estimate_context(&view, &[]);
    assert!(tokens > 0);

    // Above the blocking threshold (window - reserve).
    let window = tokens + 50;
    let blocking = policy(100, 50, 1);
    assert_eq!(
        threshold_compaction(&view, &[], window, &blocking),
        Some(CompactionTrigger::Blocking)
    );

    // Between background and blocking: tokens > blocking - backgroundTokens.
    // Choose a window so `blocking` sits above the estimate.
    let window = tokens + 100;
    let between = policy(10, 1_000, 1);
    assert_eq!(
        threshold_compaction(&view, &[], window, &between),
        Some(CompactionTrigger::Background)
    );

    // Below both thresholds.
    let window = tokens + 100_000;
    assert_eq!(threshold_compaction(&view, &[], window, &blocking), None);
}

#[test]
fn threshold_compaction_requires_enabled_and_a_cut() {
    let view = trigger_view();
    let window = estimate_context(&view, &[]) + 1;

    let mut disabled = policy(10, 0, 1);
    disabled.enabled = false;
    assert_eq!(threshold_compaction(&view, &[], window, &disabled), None);

    assert_eq!(threshold_compaction(&view, &[], 0, &policy(10, 0, 1)), None);

    // `backgroundTokens: 0` disables background compaction.
    let background_off = policy(10, 0, 1);
    let window = estimate_context(&view, &[]) + 1_000;
    assert_eq!(
        threshold_compaction(&view, &[], window, &background_off),
        None
    );

    // No cut: keep more than the whole context.
    assert_eq!(
        threshold_compaction(&view, &[], 1, &policy(100, 50, u64::MAX)),
        None
    );
}

#[test]
fn overflow_compaction_needs_an_enabled_policy_and_a_cut() {
    let view = trigger_view();
    assert!(overflow_compaction(&view, &policy(100, 50, 1)));
    let mut disabled = policy(100, 50, 1);
    disabled.enabled = false;
    assert!(!overflow_compaction(&view, &disabled));
    assert!(!overflow_compaction(&view, &policy(100, 50, u64::MAX)));
}

#[test]
fn trigger_arithmetic_does_not_saturate() {
    // contextWindow < reserveTokens yields a negative blocking threshold in
    // upstream's JavaScript arithmetic; any positive estimate is over it.
    let view = trigger_view();
    assert_eq!(
        threshold_compaction(&view, &[], 5, &policy(1_000, 0, 1)),
        Some(CompactionTrigger::Blocking)
    );
}

// ── estimation ─────────────────────────────────────────────────────────────

#[test]
fn estimate_tokens_uses_pi_ai_semantics() {
    assert_eq!(estimate_message_tokens(&user("1234567")), 2); // 7 / 3.5
    assert_eq!(estimate_message_tokens(&user("1")), 1);
    assert_eq!(estimate_message_tokens(&assistant("12345678901234")), 4);
    assert_eq!(calculate_context_tokens(&Usage::default()), 0);
    let usage = Usage {
        input: 10,
        output: 2,
        cache_read: 3,
        cache_write: 1,
        total_tokens: 0,
        ..Usage::default()
    };
    assert_eq!(calculate_context_tokens(&usage), 16);
    let usage = Usage {
        input: 10,
        total_tokens: 10,
        ..Usage::default()
    };
    assert_eq!(calculate_context_tokens(&usage), 10, "totalTokens wins");
}

#[test]
fn estimate_context_uses_usage_and_trailing_messages() {
    let usage = Usage {
        input: 500,
        total_tokens: 500,
        ..Usage::default()
    };
    let mut measured = match assistant("abcdefg") {
        Message::Assistant(message) => message,
        Message::User(_) | Message::System(_) | Message::Developer(_) | Message::ToolResult(_) => {
            unreachable!()
        }
    };
    measured.usage = usage;
    let entries = vec![
        entry(1, "pi.user", vec![user("hello")], None),
        entry(2, "pi.assistant", vec![Message::Assistant(measured)], None),
        entry(3, "pi.user", vec![user("abcdefg")], None),
    ];
    let view = view(entries);
    let base = estimate_context(&view, &[]);
    assert_eq!(base, 500 + 2, "usage plus the trailing 7-char user message");
    let extra = estimate_context(&view, &[user("1234567")]);
    assert_eq!(extra, base + 2);
}

#[test]
fn summary_budget_and_stream_options() {
    assert_eq!(summary_max_tokens(100, 0), 80);
    assert_eq!(summary_max_tokens(100, 50), 50);
    assert_eq!(summary_max_tokens(0, 50), 0);

    let stream = pir::durable::harness::settings::ConversationStreamOptions {
        deferred: Some(pir::durable::harness::settings::StreamDeferred::Enabled(
            true,
        )),
        cache_retention: Some(CacheRetention::Long),
        ..Default::default()
    };
    let options = summary_request_options(&stream);
    assert!(options.deferred.is_none(), "deferred is stripped");
    assert_eq!(options.cache_retention, Some(CacheRetention::None));
}

// ── summaries and serialization ────────────────────────────────────────────

#[test]
fn summary_text_and_failure_classification() {
    let stopped = match assistant("  the summary  ") {
        Message::Assistant(message) => message,
        _ => unreachable!(),
    };
    assert_eq!(summary_text(&stopped).as_deref(), Some("the summary"));

    let mut tool_call = stopped.clone();
    tool_call.content.push(ContentBlock::ToolCall(ToolCall {
        id: "c1".to_owned(),
        name: "read".to_owned(),
        arguments: Default::default(),
        thought_signature: None,
        namespace: None,
        arguments_error: None,
    }));
    assert_eq!(summary_text(&tool_call), None);
    assert_eq!(
        summary_failure(&tool_call),
        "Summarization attempted to call a tool"
    );

    let mut length = stopped.clone();
    length.stop_reason = StopReason::Length;
    assert_eq!(summary_text(&length), None);
    assert_eq!(
        summary_failure(&length),
        "Summarization hit the token limit; the summary is incomplete"
    );

    let mut error = stopped.clone();
    error.stop_reason = StopReason::Error;
    error.error_message = Some("boom".to_owned());
    assert_eq!(summary_failure(&error), "Summarization failed: boom");

    let empty = AssistantMessage {
        stop_reason: StopReason::Stop,
        ..AssistantMessage::default()
    };
    assert_eq!(summary_text(&empty), None);
    assert_eq!(summary_failure(&empty), "Summarization produced no text");
}

#[test]
fn serialize_conversation_omits_system_and_thinking_and_truncates_results() {
    let messages = vec![
        Message::System(UserMessage {
            role: UserRole::System,
            content: vec![ContentBlock::Text(TextContent {
                text: "system prompt".to_owned(),
                text_signature: None,
            })],
            tools_added: None,
            tools_removed: None,
            sections: None,
            timestamp: 1,
        }),
        user("hello"),
        assistant_with_calls("thinking out loud", &["c1"]),
        tool_result("c1", &"r".repeat(2_500)),
    ];
    let text = serialize_conversation(&messages);
    assert!(text.starts_with("[User]: hello"), "{text}");
    assert!(text.contains("[Assistant]: thinking out loud"));
    assert!(text.contains("[Assistant tool calls]: read()"), "{text}");
    assert!(text.contains("[Tool result]: "));
    assert!(text.contains("more characters truncated]"), "{text}");
    assert!(!text.contains("system prompt"));
    assert!(
        text.contains(&"r".repeat(TOOL_RESULT_MAX_CHARS)),
        "the kept head is full length"
    );
}

#[test]
fn summary_prompt_wraps_the_conversation_and_adds_focus() {
    let prompt = summary_prompt(&[user("hello")], None);
    assert!(prompt.starts_with("<conversation>\n[User]: hello\n</conversation>\n\n"));
    assert!(prompt.contains("## Goal"));
    let focused = summary_prompt(&[user("hello")], Some("be brief"));
    assert!(focused.ends_with("\n\nAdditional focus: be brief"));
}

#[test]
fn summary_entry_is_headed_and_wrapped() {
    let now = 42;
    let draft = summary_entry_draft(entry_id(7), "the gist", CompactionReason::Threshold, now);
    assert_eq!(draft.kind, "pi.compaction");
    assert_eq!(draft.head, Some(HeadTarget::Entry(entry_id(7))));
    assert_eq!(
        draft.data,
        Some(json!({ "reason": "threshold" })),
        "the entry carries the reason"
    );
    let model = draft.model.expect("a user message");
    assert_eq!(model.len(), 1);
    assert_eq!(
        model[0]["content"][0]["text"],
        json!(wrap_summary("the gist"))
    );
    assert_eq!(
        wrap_summary("the gist"),
        format!("{SUMMARY_PREFIX}the gist{SUMMARY_SUFFIX}")
    );

    let object = summary_entry(entry_id(7), "the gist", CompactionReason::Overflow, now);
    assert_eq!(object["kind"], json!("pi.compaction"));
    assert_eq!(object["head"], json!(7));
    assert_eq!(object["data"]["reason"], json!("overflow"));
}

#[test]
fn checkpoint_round_trips_the_upstream_shape() {
    let request = SummaryRequest {
        attempt: 2,
        model: ModelRef::new("p", "m"),
        thinking_level: ThinkingLevel::Low,
        stream_options: Default::default(),
        max_tokens: 1_000,
        tail: entry_id(9),
        first_kept: entry_id(4),
    };
    let summarize = CompactionCheckpoint::Summarize(request.clone());
    let value = summarize.to_value();
    assert_eq!(value["phase"], json!("summarize"));
    assert_eq!(value["attempt"], json!(2));
    assert_eq!(value["model"]["provider"], json!("p"));
    assert_eq!(value["model"]["modelId"], json!("m"));
    assert_eq!(value["thinkingLevel"], json!("low"));
    assert_eq!(value["maxTokens"], json!(1_000));
    assert_eq!(value["tail"], json!(9));
    assert_eq!(value["firstKept"], json!(4));
    assert_eq!(CompactionCheckpoint::from_value(&value), Ok(summarize));

    let retry = CompactionCheckpoint::Retry {
        request,
        until: 5_000,
    };
    let value = retry.to_value();
    assert_eq!(value["phase"], json!("retry"));
    assert_eq!(value["until"], json!(5_000));
    assert_eq!(CompactionCheckpoint::from_value(&value), Ok(retry));

    let select = CompactionCheckpoint::Select;
    assert_eq!(select.to_value(), json!({ "phase": "select" }));
    assert_eq!(
        CompactionCheckpoint::from_value(&json!({ "phase": "select" })),
        Ok(select)
    );
    assert!(CompactionCheckpoint::from_value(&json!({ "phase": "nope" })).is_err());
    assert!(CompactionCheckpoint::from_value(&json!(3)).is_err());
}

#[test]
fn retry_backoff_follows_the_policy() {
    let mut policy = pir::durable::harness::settings::ConversationRetryPolicy {
        enabled: true,
        max_retries: 3,
        base_delay_ms: 2_000,
        max_agent_delay_ms: 60_000,
    };

    let mut retryable = AssistantMessage {
        stop_reason: StopReason::Error,
        error_message: Some("rate limit exceeded".to_owned()),
        ..AssistantMessage::default()
    };
    assert_eq!(retry_until(&retryable, &policy, 1, 1_000), Some(3_000));
    assert_eq!(retry_until(&retryable, &policy, 2, 1_000), Some(5_000));
    assert_eq!(
        retry_until(&retryable, &policy, 4, 1_000),
        None,
        "budget spent"
    );

    retryable.error_message = Some("invalid request".to_owned());
    assert_eq!(retry_until(&retryable, &policy, 1, 1_000), None);

    policy.enabled = false;
    retryable.error_message = Some("rate limit exceeded".to_owned());
    assert_eq!(retry_until(&retryable, &policy, 1, 1_000), None);
}

#[test]
fn policy_overrides_are_field_by_field() {
    let mut settings = Settings::default();
    settings.compaction.reserve_tokens = 1_000;
    settings.compaction.keep_recent_tokens = 2_000;
    settings.compaction.model_overrides.insert(
        "p/m".to_owned(),
        pir::durable::harness::settings::PartialCompactionPolicy {
            reserve_tokens: Some(10),
            ..Default::default()
        },
    );
    let policy = compaction_policy(&settings, &ModelRef::new("p", "m"));
    assert_eq!(policy.reserve_tokens, 10);
    assert_eq!(
        policy.keep_recent_tokens, 2_000,
        "unset fields keep the default"
    );
}

#[test]
fn live_status_helpers_keep_order_and_record_failures() {
    let mut live = pir::durable::harness::live::LiveState::default();
    let first = TaskId::from_raw(1);
    let second = TaskId::from_raw(2);
    add_compaction_status(
        &mut live,
        compaction_status_of(first, CompactionReason::Manual, false),
    );
    add_compaction_status(
        &mut live,
        compaction_status_of(second, CompactionReason::Overflow, true),
    );
    let statuses = live.compactions.as_ref().expect("listed");
    assert_eq!(statuses[0].task_id, first);
    assert_eq!(statuses[1].task_id, second);
    assert_eq!(statuses[0].reason, "manual");
    assert!(statuses[1].blocking);

    fail_compaction_status(&mut live, first, "boom");
    let failure = live.compaction_failure.as_ref().expect("failure recorded");
    assert_eq!(failure.task_id, first);
    assert_eq!(failure.reason, "manual");
    assert_eq!(failure.error, "boom");
    assert_eq!(live.compactions.as_ref().map(Vec::len), Some(1));

    remove_compaction_status(&mut live, second);
    assert!(live.compactions.is_none(), "the list is dropped when empty");
}

#[test]
fn placement_actions_and_staleness() {
    assert_eq!(placement_action(true, false), PlacementAction::Append);
    assert_eq!(placement_action(true, true), PlacementAction::Append);
    assert_eq!(placement_action(false, false), PlacementAction::Admit);
    assert_eq!(placement_action(false, true), PlacementAction::SettleStale);

    assert!(summary_is_stale(entry_id(3), Some(entry_id(4))));
    assert!(!summary_is_stale(entry_id(4), Some(entry_id(4))));
    assert!(!summary_is_stale(entry_id(3), None));

    let input = CompactionInput::new(CompactionReason::Threshold);
    assert_eq!(
        compaction_ownership(&input, None),
        (TaskOwnership::Conversation, true)
    );
    assert_eq!(
        compaction_ownership(&CompactionInput::new(CompactionReason::Manual), None),
        (TaskOwnership::Conversation, false)
    );
    assert_eq!(
        compaction_ownership(&input, Some(TaskId::from_raw(9))),
        (TaskOwnership::Task(TaskId::from_raw(9)), false)
    );
    assert_eq!(compaction_request_id(TaskId::from_raw(9)), "compaction:9");
    assert_eq!(
        CompactionReason::parse("overflow"),
        Some(CompactionReason::Overflow)
    );
    assert_eq!(CompactionReason::parse("x"), None);
    let _: Vec<Arc<dyn CompactionHooks>> = vec![Arc::new(NoCompactionHooks)];
}

#[test]
fn result_serializes_camel_case_and_skips_absent_ids() {
    let nothing = CompactionResult::default();
    assert_eq!(nothing.to_value(), json!({}));
    let result = CompactionResult {
        entry_id: Some(entry_id(4)),
        submission_id: Some(SubmissionId::from_raw(9)),
    };
    assert_eq!(
        result.to_value(),
        json!({ "entryId": 4, "submissionId": 9 })
    );
    assert_eq!(CompactionResult::from_value(&result.to_value()), Ok(result));
}

// ── end-to-end harness ─────────────────────────────────────────────────────

/// A fake clock the test drives.
#[derive(Clone, Debug)]
struct Clock(Arc<AtomicI64>);

impl Clock {
    fn new(at: i64) -> Self {
        Self(Arc::new(AtomicI64::new(at)))
    }

    fn now(&self) -> i64 {
        self.0.load(Ordering::SeqCst)
    }

    fn advance(&self, by: i64) {
        self.0.fetch_add(by, Ordering::SeqCst);
    }
}

/// The harness services the compaction task needs, backed by test values.
struct TestHost {
    settings: Settings,
    agent: Option<CompactionAgent>,
    model: Option<CompactionModel>,
    view: pir::durable::harness::context::ContextView,
    active_start: Option<EntryId>,
    completions: Mutex<VecDeque<AssistantMessage>>,
    completions_seen: Mutex<Vec<CompactionCompletion>>,
    placements: Mutex<Vec<SummaryPlacement>>,
}

impl TestHost {
    fn new(view: pir::durable::harness::context::ContextView) -> Self {
        let mut settings = Settings::default();
        settings.compaction.keep_recent_tokens = 1;
        Self {
            settings,
            agent: Some(CompactionAgent {
                model: Some(ModelRef::new("test", "model")),
                thinking_level: ThinkingLevel::Off,
            }),
            model: Some(CompactionModel { max_tokens: 8_000 }),
            view,
            active_start: None,
            completions: Mutex::new(VecDeque::new()),
            completions_seen: Mutex::new(Vec::new()),
            placements: Mutex::new(Vec::new()),
        }
    }

    fn reply(&self, message: AssistantMessage) {
        self.completions.lock().unwrap().push_back(message);
    }

    fn placements(&self) -> Vec<SummaryPlacement> {
        self.placements.lock().unwrap().clone()
    }
}

impl CompactionHost for TestHost {
    fn settings(&self) -> Settings {
        self.settings.clone()
    }

    fn agent(&self, _conversation_id: ConversationId) -> Option<CompactionAgent> {
        self.agent.clone()
    }

    fn model(&self, _reference: &ModelRef) -> Option<CompactionModel> {
        self.model
    }

    fn context<'a>(
        &'a self,
        _conversation_id: ConversationId,
        _at: Option<EntryId>,
        _context: Context,
    ) -> BoxFuture<'a, Result<pir::durable::harness::context::ContextView, String>> {
        let view = self.view.clone();
        Box::pin(async move { Ok(view) })
    }

    fn active_range_start<'a>(
        &'a self,
        _conversation_id: ConversationId,
        _context: Context,
    ) -> BoxFuture<'a, Result<Option<EntryId>, String>> {
        let start = self.active_start;
        Box::pin(async move { Ok(start) })
    }

    fn complete<'a>(
        &'a self,
        request: CompactionCompletion,
        _context: Context,
    ) -> BoxFuture<'a, Result<AssistantMessage, String>> {
        self.completions_seen.lock().unwrap().push(request);
        let next = self.completions.lock().unwrap().pop_front();
        Box::pin(async move { next.ok_or_else(|| "no scripted completion".to_owned()) })
    }

    fn place<'a>(
        &'a self,
        _conversation_id: ConversationId,
        placement: &'a SummaryPlacement,
        _context: Context,
    ) -> BoxFuture<'a, Result<PlacedSummary, String>> {
        self.placements.lock().unwrap().push(placement.clone());
        let placed = if placement.blocking {
            PlacedSummary {
                entry_id: Some(EntryId::from_raw(5_000)),
                submission_id: None,
            }
        } else {
            PlacedSummary {
                entry_id: None,
                submission_id: Some(SubmissionId::from_raw(7_000)),
            }
        };
        Box::pin(async move { Ok(placed) })
    }
}

/// A hook that declines every compaction.
struct DecliningHook;

impl CompactionHooks for DecliningHook {
    fn before_compact<'a>(
        &'a self,
        _compaction: &'a CompactionDraft,
        _context: &'a Context,
    ) -> BoxFuture<'a, Result<Option<CompactionDecision>, String>> {
        Box::pin(async { Ok(Some(CompactionDecision::Decline)) })
    }
}

/// A hook that supplies its own summary.
struct SupplyingHook(&'static str);

impl CompactionHooks for SupplyingHook {
    fn before_compact<'a>(
        &'a self,
        _compaction: &'a CompactionDraft,
        _context: &'a Context,
    ) -> BoxFuture<'a, Result<Option<CompactionDecision>, String>> {
        let summary = self.0.to_owned();
        Box::pin(async move { Ok(Some(CompactionDecision::Supply(summary))) })
    }
}

/// A world: storage, a scheduler holding one `pi.compaction` definition, a fake
/// clock and one conversation.
struct World {
    storage: Arc<MemoryStorage>,
    scheduler: TaskScheduler,
    clock: Clock,
    conversation: ConversationId,
    host: Arc<TestHost>,
}

impl World {
    async fn new(host: Arc<TestHost>, hooks: Vec<Arc<dyn CompactionHooks>>) -> Self {
        let storage = Arc::new(MemoryStorage::new());
        let clock = Clock::new(1_000);
        let clock_for_now = clock.clone();
        let settings = Arc::new(host.settings.clone());
        let task = compaction_task(host.clone(), hooks).expect("a valid compaction definition");
        let options = TaskSchedulerOptions {
            now: Some(Arc::new(move || clock_for_now.now())),
            settings: Some(Arc::new(move || settings.as_ref().clone())),
            ..TaskSchedulerOptions::default()
        };
        let scheduler = TaskScheduler::open_with(options, storage.clone(), vec![task])
            .await
            .expect("open scheduler");
        let conversation = scheduler
            .session()
            .create_conversation(
                ConversationOwnership::Ownerless,
                None,
                Context::background(),
            )
            .await
            .expect("create conversation")
            .id;
        Self {
            storage,
            scheduler,
            clock,
            conversation,
            host,
        }
    }

    /// Create the compaction task and seed its live status, as
    /// `createCompaction` does in one commit.
    async fn start(&self, reason: CompactionReason) -> TaskId {
        let definition = self
            .scheduler
            .catalogue()
            .snapshot()
            .task(COMPACTION_TASK_NAME)
            .expect("installed");
        let task_id = self
            .scheduler
            .create_task(
                &definition,
                serde_json::to_value(CompactionInput::new(reason)).expect("input"),
                TaskOptions {
                    trace: None,
                    conversation_id: Some(self.conversation),
                    ownership: TaskOwnership::Conversation,
                    background: false,
                    abandon_on_restart: false,
                    submission_ids: Vec::new(),
                },
            )
            .await
            .expect("create compaction task");
        self.seed_status(task_id, reason).await;
        self.scheduler.resume();
        task_id
    }

    /// Open a world over a directly seeded `pi.compaction` record, so a test
    /// can hand the task a stored input or checkpoint no public API creates
    /// (the corrupt-record failure paths).
    async fn seeded(
        host: Arc<TestHost>,
        input: serde_json::Value,
        checkpoint: serde_json::Value,
    ) -> (Self, TaskId) {
        let storage = Arc::new(MemoryStorage::new());
        let conversation = ConversationId::from_raw(1);
        storage
            .commit(vec![StorageWrite::Conversation {
                value: ConversationRecord {
                    id: conversation,
                    parent: None,
                    owner: None,
                },
            }])
            .await
            .expect("seed conversation");
        let task_id = TaskId::from_raw(9_500);
        storage
            .commit(vec![StorageWrite::Task {
                value: TaskRecord {
                    trace: None,
                    links: Vec::new(),
                    id: task_id,
                    conversation_id: conversation,
                    kind: COMPACTION_TASK_NAME.to_owned(),
                    version: COMPACTION_TASK_VERSION,
                    input,
                    owner: None,
                    background: false,
                    abort_requested: false,
                    abort_reason: None,
                    abandon_on_restart: None,
                    started_at: None,
                    ended_at: None,
                    restarts: None,
                    state: TaskState::Pending { checkpoint },
                    memos: None,
                },
            }])
            .await
            .expect("seed compaction task");

        let clock = Clock::new(1_000);
        let clock_for_now = clock.clone();
        let settings = Arc::new(host.settings.clone());
        let task =
            compaction_task(host.clone(), Vec::new()).expect("a valid compaction definition");
        let options = TaskSchedulerOptions {
            now: Some(Arc::new(move || clock_for_now.now())),
            settings: Some(Arc::new(move || settings.as_ref().clone())),
            ..TaskSchedulerOptions::default()
        };
        let scheduler = TaskScheduler::open_with(options, storage.clone(), vec![task])
            .await
            .expect("open scheduler");
        let world = Self {
            storage,
            scheduler,
            clock,
            conversation,
            host,
        };
        world.seed_status(task_id, CompactionReason::Manual).await;
        world.scheduler.resume();
        (world, task_id)
    }

    async fn seed_status(&self, task_id: TaskId, reason: CompactionReason) {
        let resolved = live_address(self.conversation);
        let definition = LIVE_DEFINITION.clone();
        self.scheduler
            .session()
            .doc(resolved, definition, Context::background(), move |value| {
                let mut live = live_from_object(value);
                add_compaction_status(&mut live, compaction_status_of(task_id, reason, false));
                *value = live_to_object(&live);
            })
            .await
            .expect("seed pi.live status");
    }

    async fn terminal(&self, id: TaskId) -> pir::durable::types::TaskRecord {
        tokio::time::timeout(Duration::from_secs(5), self.scheduler.wait_for_task(id))
            .await
            .unwrap_or_else(|_| panic!("task {id} did not settle"))
            .expect("wait for task")
    }

    async fn record(&self, id: TaskId) -> pir::durable::types::TaskRecord {
        self.scheduler
            .get_task(id)
            .await
            .expect("read task")
            .expect("task exists")
    }

    async fn live(&self) -> pir::durable::harness::live::LiveState {
        // Read committed storage directly: `Session::snapshot` may answer from
        // its document cache, which this test drives past in `seed_status`.
        let address = live_address(self.conversation).address;
        let record = self
            .storage
            .find_document(address, DocumentPoint::Current)
            .await
            .expect("read pi.live record")
            .expect("pi.live exists");
        let stored = self
            .storage
            .document(record.id, DocumentPoint::Current)
            .await
            .expect("read pi.live value")
            .expect("pi.live value exists");
        live_from_object(&stored.value)
    }
}

fn successful_summary(text: &str) -> AssistantMessage {
    AssistantMessage {
        role: AssistantRole::Assistant,
        content: vec![ContentBlock::Text(TextContent {
            text: text.to_owned(),
            text_signature: None,
        })],
        api: "test".to_owned(),
        provider: "test".to_owned(),
        model: "model".to_owned(),
        usage: Usage {
            input: 100,
            output: 10,
            total_tokens: 110,
            ..Usage::default()
        },
        stop_reason: StopReason::Stop,
        timestamp: 1,
        ..AssistantMessage::default()
    }
}

fn failed_summary(stop_reason: StopReason, error: Option<&str>) -> AssistantMessage {
    AssistantMessage {
        role: AssistantRole::Assistant,
        content: Vec::new(),
        api: "test".to_owned(),
        provider: "test".to_owned(),
        model: "model".to_owned(),
        usage: Usage {
            input: 100,
            total_tokens: 100,
            ..Usage::default()
        },
        stop_reason,
        error_message: error.map(str::to_owned),
        timestamp: 1,
        ..AssistantMessage::default()
    }
}

fn small_view() -> pir::durable::harness::context::ContextView {
    view(vec![
        entry(1, "pi.user", vec![user("hello")], None),
        entry(2, "pi.assistant", vec![assistant("hi")], None),
        entry(3, "pi.user", vec![user("again")], None),
    ])
}

fn outcome_of(record: &pir::durable::types::TaskRecord) -> TaskOutcome {
    match &record.state {
        TaskState::Terminal { outcome } | TaskState::Completing { outcome } => outcome.clone(),
        other => panic!("expected a settled record, got {other:?}"),
    }
}

#[tokio::test]
async fn hook_decline_completes_without_placing() {
    let host = Arc::new(TestHost::new(small_view()));
    let world = World::new(host.clone(), vec![Arc::new(DecliningHook)]).await;
    let id = world.start(CompactionReason::Manual).await;
    let record = world.terminal(id).await;

    match outcome_of(&record) {
        TaskOutcome::Completed { result } => assert_eq!(result, json!({})),
        other => panic!("expected completed, got {other:?}"),
    }
    assert!(world.host.placements().is_empty(), "nothing was placed");
    assert!(world.host.completions_seen.lock().unwrap().is_empty());
    assert!(
        world.live().await.compactions.is_none(),
        "the status is removed"
    );
}

#[tokio::test]
async fn hook_supplied_summary_is_placed_headed_at_the_first_kept_entry() {
    let host = Arc::new(TestHost::new(small_view()));
    let world = World::new(host.clone(), vec![Arc::new(SupplyingHook("hook summary"))]).await;
    let id = world.start(CompactionReason::Manual).await;
    let record = world.terminal(id).await;

    match outcome_of(&record) {
        TaskOutcome::Completed { result } => {
            assert_eq!(result, json!({ "submissionId": 7_000 }))
        }
        other => panic!("expected completed, got {other:?}"),
    }
    let placements = host.placements();
    assert_eq!(placements.len(), 1);
    let placement = &placements[0];
    assert_eq!(placement.first_kept, entry_id(3), "cut at the last entry");
    assert_eq!(placement.entry.head, Some(HeadTarget::Entry(entry_id(3))));
    let model = placement.entry.model.as_ref().expect("a user message");
    assert_eq!(
        model[0]["content"][0]["text"],
        json!(wrap_summary("hook summary"))
    );
    assert_eq!(placement.entry.data, Some(json!({ "reason": "manual" })));
    assert!(
        !placement.blocking,
        "a conversation-owned compaction submits"
    );
    assert!(!placement.stale);
    assert!(
        host.completions_seen.lock().unwrap().is_empty(),
        "a supplied summary never asks the model"
    );
    assert!(world.live().await.compactions.is_none());
}

#[tokio::test]
async fn summarize_places_the_summary_and_removes_the_status() {
    let host = Arc::new(TestHost::new(small_view()));
    host.reply(successful_summary("model summary"));
    let world = World::new(host.clone(), Vec::new()).await;
    let id = world.start(CompactionReason::Threshold).await;
    let record = world.terminal(id).await;

    match outcome_of(&record) {
        TaskOutcome::Completed { result } => {
            assert_eq!(result, json!({ "submissionId": 7_000 }))
        }
        other => panic!("expected completed, got {other:?}"),
    }
    let seen = {
        let seen = host.completions_seen.lock().unwrap();
        seen.clone()
    };
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].model, ModelRef::new("test", "model"));
    assert_eq!(seen[0].max_tokens, 8_000, "min(0.8*16384, 8000)");
    assert_eq!(seen[0].thinking_level, ThinkingLevel::Off);
    assert_eq!(
        seen[0].stream_options.cache_retention,
        Some(CacheRetention::None)
    );
    assert_eq!(seen[0].messages.len(), 2);
    match &seen[0].messages[0] {
        Message::System(message) => {
            assert!(matches!(&message.content[0], ContentBlock::Text(text)
                if text.text.contains("context summarization assistant")))
        }
        other => panic!("expected the system prompt, got {other:?}"),
    }
    match &seen[0].messages[1] {
        Message::User(message) => {
            assert!(matches!(&message.content[0], ContentBlock::Text(text)
                if text.text.contains("<conversation>")))
        }
        other => panic!("expected the user prompt, got {other:?}"),
    }
    assert_eq!(host.placements().len(), 1);
    assert!(world.live().await.compactions.is_none());
}

#[tokio::test]
async fn a_summary_cutting_before_the_active_range_settles_stale() {
    // The host reports an active range that starts after the selected cut.
    let mut host = TestHost::new(small_view());
    host.active_start = Some(entry_id(100));
    let host = Arc::new(host);
    host.reply(successful_summary("late summary"));
    let world = World::new(host.clone(), Vec::new()).await;
    let id = world.start(CompactionReason::Manual).await;
    let record = world.terminal(id).await;

    match outcome_of(&record) {
        TaskOutcome::Completed { result } => {
            assert_eq!(result, json!({ "submissionId": 7_000 }))
        }
        other => panic!("expected completed, got {other:?}"),
    }
    let placements = host.placements();
    assert_eq!(placements.len(), 1);
    assert!(
        placements[0].stale,
        "the summary cuts before the range start"
    );
    assert_eq!(
        placement_action(placements[0].blocking, placements[0].stale),
        PlacementAction::SettleStale
    );
    assert_eq!(
        placements[0].entry.head,
        Some(HeadTarget::Entry(entry_id(3))),
        "the entry still heads the first kept entry"
    );
}

#[tokio::test]
async fn a_failed_summarization_writes_compaction_failure() {
    let host = Arc::new(TestHost::new(small_view()));
    host.reply(failed_summary(StopReason::Length, None));
    let world = World::new(host.clone(), Vec::new()).await;
    let id = world.start(CompactionReason::Threshold).await;
    let record = world.terminal(id).await;

    match outcome_of(&record) {
        TaskOutcome::Failed { error, .. } => {
            assert_eq!(
                error.message,
                "Summarization hit the token limit; the summary is incomplete"
            );
            assert_eq!(error.detail, Some(json!({ "reason": "model_error" })));
        }
        other => panic!("expected failed, got {other:?}"),
    }
    let live = world.live().await;
    assert!(live.compactions.is_none(), "the status is removed");
    let failure = live.compaction_failure.expect("the failure is presented");
    assert_eq!(failure.task_id, id);
    assert_eq!(failure.reason, "threshold");
    assert_eq!(
        failure.error,
        "Summarization hit the token limit; the summary is incomplete"
    );
    assert!(host.placements().is_empty());
}

#[tokio::test]
async fn a_missing_model_fails_with_no_model() {
    let mut host = TestHost::new(small_view());
    host.agent = Some(CompactionAgent::default());
    let host = Arc::new(host);
    let world = World::new(host, Vec::new()).await;
    let id = world.start(CompactionReason::Manual).await;
    let record = world.terminal(id).await;

    match outcome_of(&record) {
        TaskOutcome::Failed { error, .. } => {
            assert_eq!(error.message, "No model is configured");
            assert_eq!(error.detail, Some(json!({ "reason": "no_model" })));
        }
        other => panic!("expected failed, got {other:?}"),
    }
    let failure = world
        .live()
        .await
        .compaction_failure
        .expect("the failure is presented");
    assert_eq!(failure.error, "No model is configured");
}

/// A stored input that is not a `CompactionInput` settles the task as a
/// structured `failed` outcome (`invalid_input`), not as a panic/fault.
#[tokio::test]
async fn a_corrupt_input_settles_failed_with_invalid_input() {
    let host = Arc::new(TestHost::new(small_view()));
    let (world, id) = World::seeded(
        host.clone(),
        json!({ "reason": "not-a-known-reason" }),
        json!({ "phase": "select" }),
    )
    .await;
    let record = world.terminal(id).await;

    match outcome_of(&record) {
        TaskOutcome::Failed { error, .. } => {
            assert_eq!(error.detail, Some(json!({ "reason": "invalid_input" })));
            assert!(
                error.message.contains("has an invalid input"),
                "{}",
                error.message
            );
        }
        other => panic!("expected a failed outcome, got {other:?}"),
    }
    let live = world.live().await;
    assert!(live.compactions.is_none(), "the status is removed");
    let failure = live.compaction_failure.expect("the failure is presented");
    assert_eq!(failure.task_id, id);
    assert_eq!(failure.reason, "manual");
    assert!(host.placements().is_empty());
    assert!(host.completions_seen.lock().unwrap().is_empty());
}

/// A malformed stored checkpoint settles the task as `invalid_checkpoint`,
/// distinguishable from the input class.
#[tokio::test]
async fn a_corrupt_checkpoint_settles_failed_with_invalid_checkpoint() {
    let host = Arc::new(TestHost::new(small_view()));
    let (world, id) = World::seeded(
        host.clone(),
        json!({ "reason": "threshold" }),
        json!({ "phase": "summarize", "attempt": "one" }),
    )
    .await;
    let record = world.terminal(id).await;

    match outcome_of(&record) {
        TaskOutcome::Failed { error, .. } => {
            assert_eq!(
                error.detail,
                Some(json!({ "reason": "invalid_checkpoint" }))
            );
            assert!(
                error.message.contains("has an invalid checkpoint"),
                "{}",
                error.message
            );
        }
        other => panic!("expected a failed outcome, got {other:?}"),
    }
    let live = world.live().await;
    assert!(live.compactions.is_none(), "the status is removed");
    assert_eq!(
        live.compaction_failure
            .expect("the failure is presented")
            .task_id,
        id
    );
    assert!(host.placements().is_empty());
    assert!(host.completions_seen.lock().unwrap().is_empty());
}

#[tokio::test]
async fn nothing_to_compact_completes_without_a_summary() {
    let view = view(vec![
        entry(1, "pi.user", vec![user("hello")], None),
        entry(2, "pi.assistant", vec![assistant("hi")], None),
    ]);
    let mut host = TestHost::new(view);
    host.settings.compaction.keep_recent_tokens = 1_000_000;
    let host = Arc::new(host);
    let world = World::new(host.clone(), Vec::new()).await;
    let id = world.start(CompactionReason::Manual).await;
    let record = world.terminal(id).await;

    match outcome_of(&record) {
        TaskOutcome::Completed { result } => assert_eq!(result, json!({})),
        other => panic!("expected completed, got {other:?}"),
    }
    assert!(host.placements().is_empty());
    assert!(host.completions_seen.lock().unwrap().is_empty());
    assert!(world.live().await.compactions.is_none());
}

#[tokio::test]
async fn a_retryable_error_sleeps_and_retries_the_pinned_request() {
    let host = Arc::new(TestHost::new(small_view()));
    host.reply(failed_summary(
        StopReason::Error,
        Some("rate limit exceeded"),
    ));
    host.reply(successful_summary("after the retry"));
    let world = World::new(host.clone(), Vec::new()).await;
    let id = world.start(CompactionReason::Overflow).await;

    // The task sleeps until `now + retryDelayMs(policy, 1)` = 1000 + 2000.
    let record = wait_for_state(&world, id, |state| {
        matches!(state, TaskState::Sleeping { .. })
    })
    .await;
    let TaskState::Sleeping { until, .. } = record.state else {
        unreachable!()
    };
    assert_eq!(until, 3_000);
    let live = world.live().await;
    let status = live
        .compactions
        .as_ref()
        .and_then(|statuses| statuses.first())
        .expect("the status is listed while sleeping");
    let retry = status.retry.as_ref().expect("a retry is presented");
    assert_eq!(retry.at, 3_000);
    assert_eq!(retry.error, "rate limit exceeded");

    world.clock.advance(2_000);
    world.scheduler.resume();
    let record = world.terminal(id).await;
    match outcome_of(&record) {
        TaskOutcome::Completed { result } => {
            assert_eq!(result, json!({ "submissionId": 7_000 }))
        }
        other => panic!("expected completed, got {other:?}"),
    }
    let seen = {
        let seen = host.completions_seen.lock().unwrap();
        seen.clone()
    };
    assert_eq!(seen.len(), 2);
    assert_eq!(seen[0].model, seen[1].model, "the model stays pinned");
    assert_eq!(
        seen[0].max_tokens, seen[1].max_tokens,
        "the request stays pinned across retries"
    );
    let prompt_text = |request: &CompactionCompletion| match &request.messages[1] {
        Message::User(message) => match &message.content[0] {
            ContentBlock::Text(text) => text.text.clone(),
            _ => panic!("expected a text prompt"),
        },
        other => panic!("expected the user prompt, got {other:?}"),
    };
    assert_eq!(
        prompt_text(&seen[0]),
        prompt_text(&seen[1]),
        "the serialized transcript and prompt are unchanged"
    );
    assert_eq!(seen[0].stream_options, seen[1].stream_options);
    assert_eq!(seen[1].messages.len(), 2);
    assert_eq!(host.placements().len(), 1);
    let live = world.live().await;
    assert!(live.compactions.is_none());
}

async fn wait_for_state<F>(
    world: &World,
    id: TaskId,
    predicate: F,
) -> pir::durable::types::TaskRecord
where
    F: Fn(&TaskState) -> bool,
{
    for _ in 0..500 {
        let record = world.record(id).await;
        if predicate(&record.state) {
            return record;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!(
        "condition did not hold for task {id}; last record: {:?}",
        world.record(id).await
    );
}

/// The `detail.reason` taxonomy callers switch on is fixed.
#[test]
fn phase_error_reasons_name_the_failure_class() {
    let task = TaskId::from_raw(1);
    let cases = [
        (
            CompactionTaskError::InvalidInput {
                task,
                detail: "bad input".to_owned(),
            },
            "invalid_input",
        ),
        (
            CompactionTaskError::InvalidCheckpoint {
                task,
                detail: "bad checkpoint".to_owned(),
            },
            "invalid_checkpoint",
        ),
        (
            CompactionTaskError::Runtime(TaskRuntimeError::Closed),
            "runtime_error",
        ),
        (CompactionTaskError::host("host said no"), "host_error"),
        (
            CompactionTaskError::Document {
                kind: "pi.live",
                detail: "read failed".to_owned(),
            },
            "document_read_error",
        ),
        (CompactionTaskError::Aborted { task }, "aborted"),
        (
            CompactionTaskError::InternalInvariant {
                detail: "index out of bounds".to_owned(),
            },
            "internal_error",
        ),
    ];
    for (error, reason) in cases {
        assert_eq!(error.reason(), reason);
        assert!(!error.to_string().is_empty(), "{reason} renders a message");
    }
    assert_eq!(
        CompactionTaskError::host("host said no").to_string(),
        "host said no",
        "a host failure keeps the host's message verbatim"
    );
    assert_eq!(
        CompactionTaskError::Document {
            kind: "pi.live",
            detail: "read failed".to_owned(),
        }
        .to_string(),
        "compaction: cannot read pi.live: read failed"
    );
}

#[test]
fn task_metadata_matches_upstream() {
    let host = Arc::new(TestHost::new(small_view()));
    let task = compaction_task(host, Vec::new()).expect("valid task");
    let definition = task.definition();
    assert_eq!(definition.name, COMPACTION_TASK_NAME);
    assert_eq!(definition.name, "pi.compaction");
    assert_eq!(definition.version, COMPACTION_TASK_VERSION);
    let phases: Vec<&str> = definition.phases.keys().map(String::as_str).collect();
    assert_eq!(phases, vec!["retry", "select", "summarize"]);
    assert!(definition.abort.is_some());
    assert_eq!(
        (definition.initial)(&json!(null)),
        json!({ "phase": "select" })
    );
}

#[test]
fn value_helpers_are_exported_through_both_paths() {
    // Keep the imports above honest: each helper is reachable through the
    // module and through the harness re-exports.
    use pir::durable::harness::compaction as module;
    assert_eq!(module::COMPACTION_TASK_NAME, COMPACTION_TASK_NAME);
    assert_eq!(
        pir::durable::harness::COMPACTION_TASK_NAME,
        COMPACTION_TASK_NAME
    );
    let mut live = pir::durable::harness::live::LiveState::default();
    module::add_compaction_status(
        &mut live,
        module::compaction_status_of(TaskId::from_raw(1), CompactionReason::Manual, false),
    );
    assert!(module::compaction_status(&live, TaskId::from_raw(1)).is_some());
}
