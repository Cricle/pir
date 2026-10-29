//! `durable::harness::context` — the ten derivation rules of spec §2.1.
//!
//! Minimal fixtures per rule, mirroring upstream `harness-context.test.ts`.
//! Entries are supplied oldest first, as a `scan_entries` page would be.

use pir::ai::types::{
    AssistantMessage, AssistantRole, ContentBlock, Message, StopReason, TextContent, ToolCall,
    ToolResultMessage, ToolResultRole, UserMessage, UserRole,
};
use pir::durable::harness::context::{derive_context, ContextView, MISSING_RESULT_TEXT};
use pir::durable::ids::{id_from_number, EntryId};
use pir::durable::types::{ContextEdit, ContextEditAction, EntryRecord};
use serde_json::{json, Value};

fn id(value: i64) -> EntryId {
    id_from_number(value)
}

fn entry(id_value: i64, kind: &str, messages: Vec<Value>) -> EntryRecord {
    EntryRecord {
        id: id(id_value),
        conversation_id: id_from_number(1),
        kind: kind.to_owned(),
        model: Some(messages),
        data: None,
        head: None,
        edits: None,
        by_task_id: None,
    }
}

fn message_entry(id_value: i64, message: Message) -> EntryRecord {
    entry(id_value, "message", vec![to_value(message)])
}

fn to_value(message: Message) -> Value {
    serde_json::to_value(message).expect("a message is JSON-serializable")
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

fn system(sections: &[(&str, Option<&str>)]) -> Message {
    Message::System(UserMessage {
        role: UserRole::System,
        content: Vec::new(),
        tools_added: None,
        tools_removed: None,
        sections: Some(
            sections
                .iter()
                .map(|(key, value)| ((*key).to_owned(), value.map(str::to_owned)))
                .collect(),
        ),
        timestamp: 4,
    })
}

fn assistant(text: &str, stop_reason: StopReason, calls: &[&str]) -> Message {
    let mut content = vec![ContentBlock::Text(TextContent {
        text: text.to_owned(),
        text_signature: None,
    })];
    content.extend(calls.iter().map(|call| {
        ContentBlock::ToolCall(ToolCall {
            id: (*call).to_owned(),
            name: format!("tool-{call}"),
            arguments: Default::default(),
            thought_signature: None,
            namespace: None,
            arguments_error: None,
        })
    }));
    Message::Assistant(AssistantMessage {
        role: AssistantRole::Assistant,
        content,
        api: "faux".to_owned(),
        provider: "faux".to_owned(),
        model: "faux".to_owned(),
        stop_reason,
        timestamp: 2,
        ..AssistantMessage::default()
    })
}

fn tool_result(id_value: &str, text: &str) -> Message {
    Message::ToolResult(ToolResultMessage {
        role: ToolResultRole::ToolResult,
        tool_call_id: id_value.to_owned(),
        tool_name: format!("tool-{id_value}"),
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

fn describe(message: &Message) -> String {
    match message {
        Message::User(payload) => format!("user:{}", text_of(payload)),
        Message::System(payload) => format!(
            "system:{}",
            payload
                .sections
                .as_ref()
                .map(|sections| sections.keys().cloned().collect::<Vec<_>>().join(","))
                .unwrap_or_default()
        ),
        Message::Developer(payload) => format!("developer:{}", text_of(payload)),
        Message::Assistant(payload) => {
            let text = payload.content.iter().find_map(|block| match block {
                ContentBlock::Text(text) => Some(text.text.clone()),
                _ => None,
            });
            format!("assistant:{}", text.unwrap_or_default())
        }
        Message::ToolResult(payload) => {
            let text = payload.content.iter().find_map(|block| match block {
                ContentBlock::Text(text) => Some(text.text.clone()),
                _ => None,
            });
            format!(
                "result:{}:{}",
                payload.tool_call_id,
                if payload.is_error {
                    "error".to_owned()
                } else {
                    text.unwrap_or_default()
                }
            )
        }
    }
}

fn text_of(message: &UserMessage) -> String {
    message
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text(text) => Some(text.text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("")
}

fn described(view: &ContextView) -> Vec<String> {
    view.messages.iter().map(describe).collect()
}

fn ids(entries: &[EntryRecord]) -> Vec<i64> {
    entries.iter().map(|entry| entry.id.as_i64()).collect()
}

#[test]
fn without_a_head_the_whole_transcript_is_active_and_model_less_entries_contribute_nothing() {
    // Rule 9: a display-only entry stays in `entries`, contributes no messages.
    let entries = vec![
        message_entry(1, user("hi")),
        model_less(2, "note"),
        message_entry(3, assistant("hello", StopReason::Stop, &[])),
    ];
    let view = derive_context(&entries, None);
    assert!(view.head.is_none());
    assert_eq!(ids(&view.entries), vec![1, 2, 3]);
    assert_eq!(described(&view), vec!["user:hi", "assistant:hello"]);
    assert_eq!(view.contributions.len(), 3);
    assert!(view.contributions[1].is_empty());
}

/// A fixture entry with no model messages.
fn model_less(id_value: i64, kind: &str) -> EntryRecord {
    EntryRecord {
        model: None,
        ..entry(id_value, kind, Vec::new())
    }
}

#[test]
fn excludes_aborted_error_and_deferred_assistant_messages_but_keeps_their_entries() {
    // Rule 9.
    let entries = vec![
        message_entry(1, user("q")),
        message_entry(2, assistant("partial", StopReason::Aborted, &[])),
        message_entry(3, assistant("failed", StopReason::Error, &[])),
        message_entry(4, assistant("later", StopReason::Deferred, &[])),
        message_entry(5, assistant("done", StopReason::Length, &[])),
    ];
    let view = derive_context(&entries, None);
    assert_eq!(ids(&view.entries), vec![1, 2, 3, 4, 5]);
    assert_eq!(described(&view), vec!["user:q", "assistant:done"]);
}

#[test]
fn resolves_self_heads_and_uses_the_newest_head_marker_at_or_before_the_cutoff() {
    // Rules 1, 2, 3 and 5.
    let mut reset = message_entry(2, user("fresh start"));
    reset.kind = "pi.reset".to_owned();
    reset.head = Some(id(2));
    let entries = vec![
        message_entry(1, user("old")),
        reset,
        message_entry(3, assistant("after reset", StopReason::Stop, &[])),
        {
            // A compaction summary heads the earlier answer; older head markers drop out.
            let mut summary = entry(4, "pi.compaction", vec![to_value(user("summary"))]);
            summary.head = Some(id(3));
            summary
        },
        message_entry(5, user("next")),
    ];
    let view = derive_context(&entries, None);
    assert_eq!(view.head.as_ref().map(|head| head.id.as_i64()), Some(4));
    assert_eq!(ids(&view.entries), vec![4, 3, 5]);
    assert_eq!(
        described(&view),
        vec!["user:summary", "assistant:after reset", "user:next"]
    );

    // A cutoff before the head marker falls back to the older one.
    let earlier = derive_context(&entries, Some(id(3)));
    assert_eq!(earlier.head.as_ref().map(|head| head.id.as_i64()), Some(2));
    assert_eq!(ids(&earlier.entries), vec![2, 3]);
}

#[test]
fn selects_the_newest_edit_per_target_and_ignores_edits_before_the_active_range() {
    // Rule 4.
    let first = message_entry(1, user("first"));
    let second = message_entry(2, user("second"));
    let mut edit_v2 = model_less(3, "edit");
    edit_v2.edits = Some(vec![ContextEdit {
        target: first.id,
        action: ContextEditAction::Replace {
            messages: vec![to_value(user("first v2"))],
        },
    }]);
    let mut edit_v3 = model_less(4, "edit");
    edit_v3.edits = Some(vec![ContextEdit {
        target: first.id,
        action: ContextEditAction::Replace {
            messages: vec![to_value(user("first v3"))],
        },
    }]);
    let mut edit_omit = model_less(5, "edit");
    edit_omit.edits = Some(vec![ContextEdit {
        target: second.id,
        action: ContextEditAction::Omit,
    }]);
    let entries = vec![first, second, edit_v2, edit_v3, edit_omit];

    let view = derive_context(&entries, None);
    assert_eq!(ids(&view.entries), vec![1, 2, 3, 4, 5]);
    assert_eq!(described(&view), vec!["user:first v3"]);

    // An edit before the active range no longer applies.
    let mut reset = model_less(6, "pi.reset");
    reset.head = Some(id(2));
    let mut entries = entries;
    entries.push(reset);
    let view = derive_context(&entries, None);
    assert_eq!(view.messages, Vec::<Message>::new());

    let mut later_edit = model_less(7, "edit");
    later_edit.edits = Some(vec![ContextEdit {
        target: id(2),
        action: ContextEditAction::Replace {
            messages: vec![to_value(user("second v2"))],
        },
    }]);
    entries.push(later_edit);
    let view = derive_context(&entries, None);
    assert_eq!(described(&view), vec!["user:second v2"]);
}

#[test]
fn keeps_positional_system_messages_and_orders_tool_results_by_call_order() {
    // Rules 6, 7 and 8: results follow their call in call order, ahead of an
    // intervening system message; a result with no call is dropped.
    let entries = vec![
        entry(
            1,
            "pi.system",
            vec![to_value(system(&[("preamble", Some("You help."))]))],
        ),
        message_entry(2, user("run tools")),
        message_entry(3, assistant("calling", StopReason::ToolUse, &["b", "a"])),
        message_entry(4, tool_result("a", "result a")),
        entry(
            5,
            "pi.system",
            vec![to_value(system(&[("cwd", Some("/repo"))]))],
        ),
        message_entry(6, tool_result("b", "result b")),
        message_entry(7, tool_result("zz", "stray")),
        message_entry(8, assistant("done", StopReason::Stop, &[])),
    ];
    let view = derive_context(&entries, None);
    assert_eq!(
        described(&view),
        vec![
            "system:preamble",
            "user:run tools",
            "assistant:calling",
            "result:b:result b",
            "result:a:result a",
            "system:cwd",
            "assistant:done",
        ]
    );
}

#[test]
fn synthesizes_missing_results_and_drops_results_with_no_preceding_call() {
    // Rules 7 and 8.
    let entries = vec![
        message_entry(1, user("go")),
        message_entry(2, assistant("calling", StopReason::ToolUse, &["x", "y"])),
        message_entry(3, tool_result("x", "result x")),
    ];
    let view = derive_context(&entries, None);
    assert_eq!(
        described(&view),
        vec![
            "user:go",
            "assistant:calling",
            "result:x:result x",
            "result:y:error",
        ]
    );
    match &view.messages[3] {
        Message::ToolResult(result) => {
            assert_eq!(result.tool_name, "tool-y");
            assert_eq!(result.details, Some(json!({ "reason": "missing_result" })));
            assert_eq!(text_of_blocks(result), MISSING_RESULT_TEXT);
        }
        other => panic!("expected a synthesized tool result, got {other:?}"),
    }

    // A head between a call and its results leaves stray results that are not sent.
    let mut reset = model_less(4, "pi.reset");
    reset.head = Some(id(3));
    let mut entries = entries;
    entries.push(reset);
    let view = derive_context(&entries, None);
    assert!(view.messages.is_empty());
    assert_eq!(
        view.entries
            .iter()
            .map(|entry| entry.kind.as_str())
            .collect::<Vec<_>>(),
        vec!["pi.reset", "message"]
    );
}

fn text_of_blocks(message: &ToolResultMessage) -> String {
    message
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text(text) => Some(text.text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("")
}

#[test]
fn leads_with_a_system_message_that_only_user_messages_precede() {
    // Rule 10, first clause.
    let entries = vec![
        message_entry(1, user("first")),
        message_entry(2, user("steered")),
        entry(
            3,
            "pi.system",
            vec![to_value(system(&[("preamble", Some("You help."))]))],
        ),
        message_entry(4, assistant("answer", StopReason::Stop, &[])),
        message_entry(5, user("next")),
        entry(
            6,
            "pi.system",
            vec![to_value(system(&[("cwd", Some("/repo"))]))],
        ),
    ];
    let view = derive_context(&entries, None);
    assert_eq!(
        described(&view),
        vec![
            "system:preamble",
            "user:first",
            "user:steered",
            "assistant:answer",
            "user:next",
            "system:cwd",
        ]
    );
    // Stored order and contributions keep the committed order.
    let contributed: Vec<String> = view.contributions.iter().flatten().map(describe).collect();
    assert_eq!(
        &contributed[..3],
        &["user:first", "user:steered", "system:preamble"]
    );
}

#[test]
fn leads_with_the_baseline_after_a_compaction_that_keeps_assistant_messages() {
    // Rule 10, second clause: the baseline's message moves even though an
    // assistant message precedes it.
    let mut summary = entry(6, "pi.compaction", vec![to_value(user("summary"))]);
    summary.head = Some(id(2));
    let entries = vec![
        message_entry(1, user("old")),
        message_entry(2, user("kept")),
        message_entry(3, assistant("calling", StopReason::ToolUse, &["a"])),
        message_entry(4, tool_result("a", "result a")),
        message_entry(5, assistant("answer", StopReason::Stop, &[])),
        summary,
        message_entry(7, user("next")),
        entry(
            8,
            "pi.system",
            vec![to_value(system(&[("preamble", Some("Baseline."))]))],
        ),
    ];
    let view = derive_context(&entries, None);
    assert_eq!(
        described(&view),
        vec![
            "system:preamble",
            "user:summary",
            "user:kept",
            "assistant:calling",
            "result:a:result a",
            "assistant:answer",
            "user:next",
        ]
    );

    // A later system message stays in place.
    let mut entries = entries;
    entries.push(message_entry(9, assistant("done", StopReason::Stop, &[])));
    entries.push(entry(
        10,
        "pi.system",
        vec![to_value(system(&[("cwd", Some("/repo"))]))],
    ));
    let view = derive_context(&entries, None);
    let described_view = described(&view);
    assert_eq!(&described_view[..2], &["system:preamble", "user:summary"]);
    assert_eq!(
        &described_view[described_view.len() - 2..],
        &["assistant:done", "system:cwd"]
    );
}

#[test]
fn leads_with_a_baseline_written_after_the_first_answers_when_none_came_before() {
    // Rule 10: without `H`, the first `pi.system` is the baseline unless an
    // earlier entry carries a system message.
    let entries = vec![
        message_entry(1, user("first")),
        message_entry(2, assistant("answer", StopReason::Stop, &[])),
        message_entry(3, user("next")),
        entry(
            4,
            "pi.system",
            vec![to_value(system(&[("plan", Some("Plan."))]))],
        ),
    ];
    let view = derive_context(&entries, None);
    assert_eq!(
        described(&view),
        vec!["system:plan", "user:first", "assistant:answer", "user:next"]
    );
}

#[test]
fn keeps_a_non_baseline_system_message_in_place_once_an_assistant_precedes_it() {
    // Rule 10: another kind's system message ahead of the baseline is
    // positional, so the baseline does not move ahead of it.
    let entries = vec![
        message_entry(1, user("first")),
        message_entry(2, assistant("answer", StopReason::Stop, &[])),
        entry(
            3,
            "host.note",
            vec![to_value(system(&[("note", Some("From here on."))]))],
        ),
        entry(
            4,
            "pi.system",
            vec![to_value(system(&[("preamble", Some("Baseline."))]))],
        ),
    ];
    let view = derive_context(&entries, None);
    assert_eq!(
        described(&view),
        vec![
            "user:first",
            "assistant:answer",
            "system:note",
            "system:preamble"
        ]
    );

    // After a compaction that keeps the other kind's system message, the new
    // baseline does not move ahead of it either.
    let mut summary = entry(8, "pi.compaction", vec![to_value(user("summary"))]);
    summary.head = Some(id(5));
    let entries = vec![
        entries[0].clone(),
        entries[1].clone(),
        entries[2].clone(),
        entries[3].clone(),
        message_entry(5, user("kept")),
        message_entry(6, assistant("kept answer", StopReason::Stop, &[])),
        entry(
            7,
            "host.note",
            vec![to_value(system(&[("mode", Some("plan"))]))],
        ),
        summary,
        message_entry(9, user("next")),
        entry(
            10,
            "pi.system",
            vec![to_value(system(&[("preamble", Some("Baseline."))]))],
        ),
    ];
    let view = derive_context(&entries, None);
    assert_eq!(
        described(&view),
        vec![
            "user:summary",
            "user:kept",
            "assistant:kept answer",
            "system:mode",
            "user:next",
            "system:preamble",
        ]
    );
}

#[test]
fn does_not_lead_with_a_system_entry_written_after_an_earlier_system_message_even_once_omitted() {
    // Rule 10: the earlier host system message makes the `pi.system` entry no
    // baseline, and it stays positional even when the host message is omitted.
    let host = entry(
        1,
        "host.system",
        vec![to_value(system(&[("legacy", Some("Use legacy."))]))],
    );
    let entries = vec![
        host.clone(),
        message_entry(2, user("first")),
        message_entry(3, assistant("answer", StopReason::Stop, &[])),
        entry(4, "pi.system", vec![to_value(system(&[("legacy", None)]))]),
        {
            let mut edit = model_less(5, "edit");
            edit.edits = Some(vec![ContextEdit {
                target: host.id,
                action: ContextEditAction::Omit,
            }]);
            edit
        },
    ];
    let view = derive_context(&entries, None);
    assert_eq!(
        described(&view),
        vec!["user:first", "assistant:answer", "system:legacy"]
    );

    // The same holds for a system message a replacement edit puts before it,
    // once a later edit replaces that message list again.
    let mut first_replacement = model_less(2, "edit");
    first_replacement.edits = Some(vec![ContextEdit {
        target: id(1),
        action: ContextEditAction::Replace {
            messages: vec![
                to_value(user("first")),
                to_value(system(&[("note", Some("Note."))])),
            ],
        },
    }]);
    let mut second_replacement = model_less(5, "edit");
    second_replacement.edits = Some(vec![ContextEdit {
        target: id(1),
        action: ContextEditAction::Replace {
            messages: vec![to_value(user("first"))],
        },
    }]);
    let entries = vec![
        message_entry(1, user("first")),
        first_replacement,
        message_entry(3, assistant("answer", StopReason::Stop, &[])),
        entry(4, "pi.system", vec![to_value(system(&[("note", None)]))]),
        second_replacement,
    ];
    let view = derive_context(&entries, None);
    assert_eq!(
        described(&view),
        vec!["user:first", "assistant:answer", "system:note"]
    );
}

#[test]
fn does_not_lead_with_a_retained_edited_or_later_system_entry_in_place_of_the_baseline() {
    // Rule 10: a retained `pi.system` entry after a head marker is not the
    // baseline; the baseline generation writes (with an omission) leads, and an
    // omitted or replaced baseline leaves nothing to lead.
    let mut summary = entry(4, "pi.compaction", vec![to_value(user("summary"))]);
    summary.head = Some(id(1));
    let retained = entry(
        3,
        "pi.system",
        vec![to_value(system(&[("cwd", Some("/repo"))]))],
    );
    let mut baseline = entry(
        6,
        "pi.system",
        vec![to_value(system(&[("preamble", Some("Baseline."))]))],
    );
    baseline.edits = Some(vec![ContextEdit {
        target: retained.id,
        action: ContextEditAction::Omit,
    }]);
    let entries = vec![
        message_entry(1, user("kept")),
        message_entry(2, assistant("answer", StopReason::Stop, &[])),
        retained,
        summary,
        message_entry(5, user("next")),
        baseline.clone(),
        message_entry(7, assistant("done", StopReason::Stop, &[])),
        entry(
            8,
            "pi.system",
            vec![to_value(system(&[("preamble", None)]))],
        ),
    ];

    // Before the baseline is written, the retained entry stays in place.
    let view = derive_context(&entries[..5], None);
    assert_eq!(
        described(&view),
        vec![
            "user:summary",
            "user:kept",
            "assistant:answer",
            "system:cwd",
            "user:next",
        ]
    );

    let view = derive_context(&entries, None);
    assert_eq!(
        described(&view),
        vec![
            "system:preamble",
            "user:summary",
            "user:kept",
            "assistant:answer",
            "user:next",
            "assistant:done",
            "system:preamble",
        ]
    );

    // An omitted baseline leaves no message to lead.
    let mut omit_baseline = model_less(9, "edit");
    omit_baseline.edits = Some(vec![ContextEdit {
        target: baseline.id,
        action: ContextEditAction::Omit,
    }]);
    let mut entries = entries;
    entries.push(omit_baseline);
    let view = derive_context(&entries, None);
    assert_eq!(
        described(&view),
        vec![
            "user:summary",
            "user:kept",
            "assistant:answer",
            "user:next",
            "assistant:done",
            "system:preamble",
        ]
    );

    // A replacement of the baseline is not one either.
    let mut replace_baseline = model_less(10, "edit");
    replace_baseline.edits = Some(vec![ContextEdit {
        target: baseline.id,
        action: ContextEditAction::Replace {
            messages: vec![to_value(system(&[("other", Some("Other."))]))],
        },
    }]);
    entries.push(replace_baseline);
    let view = derive_context(&entries, None);
    assert_eq!(
        described(&view),
        vec![
            "user:summary",
            "user:kept",
            "assistant:answer",
            "user:next",
            "system:other",
            "assistant:done",
            "system:preamble",
        ]
    );
}

#[test]
fn a_head_marker_makes_its_own_messages_the_first_contribution() {
    // Rule 5: context entries are `H` followed by non-head entries; `H`'s
    // contribution is its own messages, not the entries it points at.
    let mut summary = entry(3, "pi.compaction", vec![to_value(user("summary"))]);
    summary.head = Some(id(1));
    let entries = vec![
        message_entry(1, user("kept")),
        message_entry(2, assistant("answer", StopReason::Stop, &[])),
        summary,
    ];
    let view = derive_context(&entries, None);
    assert_eq!(ids(&view.entries), vec![3, 1, 2]);
    assert_eq!(
        described(&view),
        vec!["user:summary", "user:kept", "assistant:answer"]
    );
}
