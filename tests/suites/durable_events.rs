//! `durable::harness::events` — the coding-agent event stream (spec §9.4).
//!
//! Port of upstream `packages/durable/test/harness-events.test.ts`'s pure
//! halves: one synthetic run's ordered batches, delta correctness (the changes
//! rebuild the committed partial), tool output trims/appends, and the bounded
//! backlog collapsing to one fresh snapshot.
//!
//! Tests live here as an integration suite rather than inline in the module:
//! the crate forbids inline tests.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use serde_json::{json, Value};

use pir::durable::delta::{Op, Seg};
use pir::durable::harness::events::{
    message_changes, snapshot_of, tool_update, translate, watch_events, AgentEvent, MessageChange,
    NestedCallState, NestedToolExecutionResult, SnapshotEvent, ToolOutputChange, ViewPort,
};
use pir::durable::harness::live::{
    GenerationState, LiveState, NestedToolSlot, RunState, SlotProgress, SlotStatus, ToolDiagnostic,
    ToolSlot,
};
use pir::durable::harness::{ConversationViews, LIVE_KIND};
use pir::durable::ids::{ConversationId, EntryId, Seq, SubmissionId, TaskId};
use pir::durable::session::observation::WatchEnd;
use pir::durable::session::publication::{CommitChange, CommitPublication, TableCommitChange};
use pir::durable::session::transaction::EntryDraft;
use pir::durable::session::{ConversationOwnership, Session, SessionOptions};
use pir::durable::types::{
    DocumentCommitChange, DocumentRecord, DocumentScope, EntryRecord, Storage, SubmissionRecord,
    SubmissionStatus, SubmissionType, TaskOutcome, TaskRecord, TaskState,
};
use pir::durable::{Context, MemoryStorage, StorageError};

// ── fixtures ───────────────────────────────────────────────────────────────

const CONVERSATION: i64 = 1;

fn conversation() -> ConversationId {
    ConversationId::from_raw(CONVERSATION)
}

fn usage() -> Value {
    json!({
        "input": 1,
        "output": 2,
        "cacheRead": 0,
        "cacheWrite": 0,
        "totalTokens": 3,
        "cost": {"input": 0.0, "output": 0.0, "cacheRead": 0.0, "cacheWrite": 0.0, "total": 0.0}
    })
}

fn assistant(content: Value) -> Value {
    json!({
        "role": "assistant",
        "content": content,
        "api": "faux",
        "provider": "faux",
        "model": "faux-1",
        "usage": usage(),
        "stopReason": "pending",
        "timestamp": 0
    })
}

fn text_block(text: &str) -> Value {
    json!({"type": "text", "text": text})
}

fn thinking_block(thinking: &str) -> Value {
    json!({"type": "thinking", "thinking": thinking})
}

fn tool_call_block(id: &str, name: &str, arguments: Value) -> Value {
    json!({"type": "toolCall", "id": id, "name": name, "arguments": arguments})
}

fn user_message(text: &str) -> Value {
    json!({"role": "user", "content": [text_block(text)], "timestamp": 0})
}

fn tool_result_message(call_id: &str, name: &str) -> Value {
    json!({
        "role": "toolResult",
        "toolCallId": call_id,
        "toolName": name,
        "content": [],
        "isError": false,
        "timestamp": 0
    })
}

fn entry(id: i64, kind: &str, model: Option<Vec<Value>>) -> EntryRecord {
    EntryRecord {
        id: EntryId::from_raw(id),
        conversation_id: conversation(),
        kind: kind.to_owned(),
        model,
        data: None,
        head: None,
        edits: None,
        by_task_id: None,
    }
}

fn task(id: i64, kind: &str, state: TaskState) -> TaskRecord {
    TaskRecord {
        trace: None,
        links: Vec::new(),
        id: TaskId::from_raw(id),
        conversation_id: conversation(),
        kind: kind.to_owned(),
        version: 1,
        input: json!({}),
        owner: None,
        background: false,
        abort_requested: false,
        abort_reason: None,
        abandon_on_restart: None,
        started_at: None,
        ended_at: None,
        restarts: None,
        state,
        memos: None,
    }
}

fn submission(id: i64, status: SubmissionStatus) -> SubmissionRecord {
    SubmissionRecord {
        trace: None,
        id: SubmissionId::from_raw(id),
        conversation_id: conversation(),
        request_id: None,
        submission_type: SubmissionType::Input,
        status,
        entry: None,
        answer: None,
        reason: None,
        detail: None,
    }
}

fn publication(seq: i64, changes: Vec<CommitChange>) -> CommitPublication {
    CommitPublication {
        seq: Seq::from_raw(seq),
        changes,
    }
}

fn entry_change(entry: EntryRecord) -> CommitChange {
    CommitChange::Table(TableCommitChange::Entry(entry))
}

fn task_change(task: TaskRecord) -> CommitChange {
    CommitChange::Table(TableCommitChange::Task(task))
}

fn submission_change(record: SubmissionRecord) -> CommitChange {
    CommitChange::Table(TableCommitChange::Submission(record))
}

fn view(entries: Vec<EntryRecord>, live: LiveState) -> ViewPort {
    ViewPort {
        entries,
        live,
        inbox: None,
        agent: None,
        usage: None,
    }
}

fn key(name: &str) -> Seg {
    Seg::Key(name.to_owned())
}

/// `docs/pi.live/<tail...>`.
fn live_path(tail: &[Seg]) -> Vec<Seg> {
    let mut path = vec![key("docs"), key(LIVE_KIND)];
    path.extend(tail.iter().cloned());
    path
}

fn content_path(index: usize, field: &str) -> Vec<Seg> {
    live_path(&[
        key("generation"),
        key("message"),
        key("content"),
        Seg::Index(index),
        key(field),
    ])
}

fn run(task_id: i64, inputs: Vec<i64>) -> RunState {
    RunState {
        task_id: TaskId::from_raw(task_id),
        inputs: inputs.into_iter().map(SubmissionId::from_raw).collect(),
    }
}

fn generation(attempt: i64, message: Option<Value>) -> GenerationState {
    GenerationState {
        attempt,
        message,
        retry: None,
        deferred: None,
    }
}

fn tool_slot(call_id: &str, name: &str, task_id: i64, status: SlotStatus) -> ToolSlot {
    let mut progress = SlotProgress::new(call_id, name);
    progress.status = status;
    ToolSlot {
        progress,
        task_id: Some(TaskId::from_raw(task_id)),
        entry: None,
    }
}

/// Collect one commit's event kinds, and the events themselves.
fn kinds(events: &[AgentEvent]) -> Vec<&'static str> {
    events.iter().map(AgentEvent::kind).collect()
}

// ── message deltas ─────────────────────────────────────────────────────────

/// Apply message changes to a copy of `message`, as an events-only consumer
/// would.
fn apply_changes(message: &Value, changes: &[MessageChange]) -> Value {
    let mut next = message.clone();
    for change in changes {
        match change {
            MessageChange::Message { message } => {
                next = serde_json::to_value(message).expect("assistant message serializes");
            }
            MessageChange::Block {
                content_index,
                block,
            } => {
                next["content"][content_index] =
                    serde_json::to_value(block).expect("block serializes");
            }
            MessageChange::TextStart {
                content_index,
                block,
            }
            | MessageChange::ThinkingStart {
                content_index,
                block,
            }
            | MessageChange::ToolcallStart {
                content_index,
                block,
            } => {
                let block = serde_json::to_value(block).expect("block serializes");
                let content = next["content"].as_array_mut().expect("content array");
                let index = (*content_index).min(content.len());
                content.insert(index, block);
            }
            MessageChange::TextDelta {
                content_index,
                delta,
            } => {
                let text = next["content"][content_index]["text"]
                    .as_str()
                    .unwrap_or_default()
                    .to_owned();
                next["content"][content_index]["text"] = json!(format!("{text}{delta}"));
            }
            MessageChange::ThinkingDelta {
                content_index,
                delta,
            } => {
                let thinking = next["content"][content_index]["thinking"]
                    .as_str()
                    .unwrap_or_default()
                    .to_owned();
                next["content"][content_index]["thinking"] = json!(format!("{thinking}{delta}"));
            }
            MessageChange::ToolcallDelta {
                content_index,
                path,
                delta,
            } => {
                let mut target = &mut next["content"][content_index]["arguments"];
                for segment in &path[..path.len().saturating_sub(1)] {
                    target = match segment {
                        Seg::Key(name) => &mut target[name],
                        Seg::Index(index) => &mut target[index],
                    };
                }
                let (last, _) = path.split_last().expect("a non-empty path");
                let current = match last {
                    Seg::Key(name) => target[name].as_str().unwrap_or_default().to_owned(),
                    Seg::Index(index) => target[index].as_str().unwrap_or_default().to_owned(),
                };
                let value = json!(format!("{current}{delta}"));
                match last {
                    Seg::Key(name) => target[name] = value,
                    Seg::Index(index) => target[index] = value,
                }
            }
        }
    }
    next
}

#[test]
fn text_and_thinking_deltas_rebuild_the_committed_partial() {
    let partial = assistant(json!([
        text_block("Hel"),
        thinking_block("hm"),
        tool_call_block("c1", "print", json!({"path": "src/"}))
    ]));
    let message = match serde_json::from_value::<pir::ai::types::Message>(partial.clone()) {
        Ok(pir::ai::types::Message::Assistant(message)) => message,
        other => panic!("assistant message decodes: {other:?}"),
    };

    let ops = vec![
        Op::Append(content_path(0, "text"), "lo".to_owned()),
        Op::Append(content_path(1, "thinking"), "m".to_owned()),
        Op::Append(
            {
                let mut path = content_path(2, "arguments");
                path.push(key("path"));
                path
            },
            "main.rs".to_owned(),
        ),
        // A usage-only operation sends no changes.
        Op::Set(
            live_path(&[key("generation"), key("message"), key("usage")]),
            usage(),
        ),
    ];

    let changes = message_changes(&ops, &message);
    assert_eq!(
        changes,
        vec![
            MessageChange::TextDelta {
                content_index: 0,
                delta: "lo".to_owned()
            },
            MessageChange::ThinkingDelta {
                content_index: 1,
                delta: "m".to_owned()
            },
            MessageChange::ToolcallDelta {
                content_index: 2,
                path: vec![key("path")],
                delta: "main.rs".to_owned()
            },
        ]
    );

    let rebuilt = apply_changes(&partial, &changes);
    let committed = assistant(json!([
        text_block("Hello"),
        thinking_block("hmm"),
        tool_call_block("c1", "print", json!({"path": "src/main.rs"}))
    ]));
    assert_eq!(rebuilt, committed);
}

#[test]
fn block_starts_and_whole_message_replacements_are_reported() {
    let partial = assistant(json!([text_block("a")]));
    let message = match serde_json::from_value::<pir::ai::types::Message>(partial.clone()) {
        Ok(pir::ai::types::Message::Assistant(message)) => message,
        other => panic!("assistant message decodes: {other:?}"),
    };

    // Inserting a thinking block and a tool-call block in one splice.
    let ops = vec![Op::Splice {
        path: live_path(&[key("generation"), key("message"), key("content")]),
        index: 1,
        remove: 0,
        items: vec![
            thinking_block("th"),
            tool_call_block("c1", "print", json!({})),
        ],
    }];
    assert_eq!(
        message_changes(&ops, &message),
        vec![
            MessageChange::ThinkingStart {
                content_index: 1,
                block: serde_json::from_value(thinking_block("th")).unwrap()
            },
            MessageChange::ToolcallStart {
                content_index: 2,
                block: serde_json::from_value(tool_call_block("c1", "print", json!({}))).unwrap()
            },
        ]
    );

    // A field change no delta describes sends the block whole.
    let signed = assistant(json!([{
        "type": "text",
        "text": "a",
        "textSignature": "sig"
    }]));
    let signed_message = match serde_json::from_value::<pir::ai::types::Message>(signed) {
        Ok(pir::ai::types::Message::Assistant(message)) => message,
        other => panic!("assistant message decodes: {other:?}"),
    };
    let ops = vec![Op::Set(content_path(0, "textSignature"), json!("sig"))];
    assert_eq!(
        message_changes(&ops, &signed_message),
        vec![MessageChange::Block {
            content_index: 0,
            block: serde_json::from_value(json!({
                "type": "text",
                "text": "a",
                "textSignature": "sig"
            }))
            .unwrap()
        }]
    );

    // A whole-document replacement or a message replacement sends the message.
    for ops in [
        vec![Op::Set(
            live_path(&[key("generation")]),
            json!({"attempt": 1}),
        )],
        vec![Op::Set(
            live_path(&[key("generation"), key("message")]),
            partial.clone(),
        )],
    ] {
        assert_eq!(
            message_changes(&ops, &message),
            vec![MessageChange::Message {
                message: message.clone()
            }]
        );
    }
}

#[test]
fn message_changes_keep_the_upstream_wire_tags() {
    let value = serde_json::to_value(MessageChange::TextDelta {
        content_index: 2,
        delta: "hi".to_owned(),
    })
    .expect("a delta serializes");
    assert_eq!(
        value,
        json!({"type": "text_delta", "contentIndex": 2, "delta": "hi"})
    );

    let value = serde_json::to_value(MessageChange::ToolcallDelta {
        content_index: 0,
        path: vec![key("path")],
        delta: "a".to_owned(),
    })
    .expect("a tool-call delta serializes");
    assert_eq!(
        value,
        json!({"type": "toolcall_delta", "contentIndex": 0, "path": ["path"], "delta": "a"})
    );

    // The tool events flatten the call identity, as upstream's intersection.
    let value = serde_json::to_value(AgentEvent::ToolExecutionStart {
        call: pir::durable::harness::events::ToolEventCall {
            tool_call_id: "c1".to_owned(),
            tool_name: "print".to_owned(),
            task_id: Some(TaskId::from_raw(20)),
            parent_tool_call_id: None,
            parent_task_id: None,
        },
        args: json!({"n": 1}).as_object().cloned().unwrap(),
    })
    .expect("a start serializes");
    assert_eq!(
        value,
        json!({
            "type": "tool_execution_start",
            "toolCallId": "c1",
            "toolName": "print",
            "taskId": 20,
            "args": {"n": 1}
        })
    );

    // A removed `details` sends `null`; unchanged ones are omitted.
    let value = serde_json::to_value(AgentEvent::ToolExecutionUpdate {
        call: pir::durable::harness::events::ToolEventCall {
            tool_call_id: "c1".to_owned(),
            tool_name: "print".to_owned(),
            task_id: None,
            parent_tool_call_id: None,
            parent_task_id: None,
        },
        output: None,
        details: pir::durable::harness::events::Tri::Null,
        diagnostics: pir::durable::harness::events::Tri::Absent,
    })
    .expect("an update serializes");
    assert_eq!(
        value,
        json!({
            "type": "tool_execution_update",
            "toolCallId": "c1",
            "toolName": "print",
            "details": null
        })
    );
}

#[test]
fn tool_updates_report_appends_trims_replacements_and_cleared_progress() {
    let mut before = SlotProgress::new("c1", "print");
    before.status = SlotStatus::Running;
    before.output = Some("one\n".to_owned());
    before.details = Some(json!({"n": 1}));
    before.diagnostics = Some(Vec::new());

    // Front trim and then append of the retained window.
    let mut after = before.clone();
    after.output = Some("two\nthree\n".to_owned());
    let ops = vec![
        Op::Truncate(live_path(&[key("tools"), Seg::Index(0), key("output")]), 4),
        Op::Append(
            live_path(&[key("tools"), Seg::Index(0), key("output")]),
            "three\n".to_owned(),
        ),
    ];
    let update = tool_update(&ops, "tools", 0, &after, &before).expect("an update");
    assert_eq!(
        update.output,
        Some(ToolOutputChange::Window {
            trim_start: Some(4),
            append: Some("three\n".to_owned())
        })
    );

    // Replacement of the retained window.
    let mut replaced = before.clone();
    replaced.output = Some("whole".to_owned());
    let ops = vec![Op::Set(
        live_path(&[key("tools"), Seg::Index(0), key("output")]),
        json!("whole"),
    )];
    let update = tool_update(&ops, "tools", 0, &replaced, &before).expect("an update");
    assert_eq!(
        update.output,
        Some(ToolOutputChange::Set {
            set: "whole".to_owned()
        })
    );

    // A safe replay clears progress: removed details send null, removed
    // diagnostics send [].
    let mut cleared = before.clone();
    cleared.details = None;
    cleared.diagnostics = None;
    let update = tool_update(&[], "tools", 0, &cleared, &before).expect("an update");
    assert_eq!(update.details, pir::durable::harness::events::Tri::Null);
    assert_eq!(
        update.diagnostics,
        pir::durable::harness::events::Tri::Value(Vec::new())
    );
    assert_eq!(update.output, None);

    // No change at all.
    assert_eq!(tool_update(&[], "tools", 0, &before, &before), None);
}

// ── one synthetic run ──────────────────────────────────────────────────────

#[test]
fn one_run_translates_into_the_ordered_coding_agent_batches() {
    let mut held: HashSet<TaskId> = HashSet::new();
    let mut flat: Vec<AgentEvent> = Vec::new();
    let mut batches: Vec<Vec<&'static str>> = Vec::new();

    let user = entry(2, "pi.user", Some(vec![user_message("hi")]));
    let mut before = view(Vec::new(), LiveState::new());

    // 1. Submit: the user entry, the submission, the run and its generation
    //    task all land in one commit.
    let mut after = before.clone();
    after.entries.push(user.clone());
    after.live.run = Some(run(10, vec![5]));
    let events = translate(
        conversation(),
        &before,
        &after,
        &[],
        &publication(
            1,
            vec![
                entry_change(user.clone()),
                submission_change(submission(5, SubmissionStatus::Placed)),
                task_change(task(
                    10,
                    "pi.generation",
                    TaskState::Running {
                        checkpoint: json!({}),
                    },
                )),
            ],
        ),
        &mut held,
    );
    batches.push(kinds(&events));
    flat.extend(events.iter().cloned());
    before = after;

    // 2. The first committed partial starts the answer.
    let partial_one = assistant(json!([text_block("He")]));
    let mut after = before.clone();
    after.live.generation = Some(generation(1, Some(partial_one.clone())));
    let events = translate(
        conversation(),
        &before,
        &after,
        &[Op::Set(
            live_path(&[key("generation")]),
            json!({"attempt": 1, "message": partial_one}),
        )],
        &publication(2, Vec::new()),
        &mut held,
    );
    batches.push(kinds(&events));
    flat.extend(events.iter().cloned());
    before = after;

    // 3. Text is appended: a delta, not the whole message.
    let partial_two = assistant(json!([text_block("Hello")]));
    let mut after = before.clone();
    after.live.generation = Some(generation(1, Some(partial_two.clone())));
    let events = translate(
        conversation(),
        &before,
        &after,
        &[Op::Append(content_path(0, "text"), "llo".to_owned())],
        &publication(3, Vec::new()),
        &mut held,
    );
    batches.push(kinds(&events));
    flat.extend(events.iter().cloned());
    before = after;

    // 4. A thinking block is inserted: a start change.
    let partial_three = assistant(json!([text_block("Hello"), thinking_block("hm")]));
    let mut after = before.clone();
    after.live.generation = Some(generation(1, Some(partial_three)));
    let events = translate(
        conversation(),
        &before,
        &after,
        &[Op::Splice {
            path: live_path(&[key("generation"), key("message"), key("content")]),
            index: 1,
            remove: 0,
            items: vec![thinking_block("hm")],
        }],
        &publication(4, Vec::new()),
        &mut held,
    );
    batches.push(kinds(&events));
    flat.extend(events.iter().cloned());
    before = after;

    // 5. The tool round begins: the assistant entry is committed, the partial
    //    leaves `pi.live`, and a running slot starts with its intent args.
    let assistant_tool_call = entry(
        3,
        "pi.assistant",
        Some(vec![assistant(json!([tool_call_block(
            "c1",
            "print",
            json!({"n": 1})
        )]))]),
    );
    let mut after = before.clone();
    after.entries.push(assistant_tool_call.clone());
    after.live.generation = None;
    after.live.tools = Some(vec![tool_slot("c1", "print", 20, SlotStatus::Running)]);
    let events = translate(
        conversation(),
        &before,
        &after,
        &[Op::Delete(live_path(&[key("generation")]))],
        &publication(
            5,
            vec![
                entry_change(assistant_tool_call),
                task_change(task(
                    20,
                    "pi.tool",
                    TaskState::Running {
                        checkpoint: json!({"phase": "execute", "arguments": {"n": 1}}),
                    },
                )),
            ],
        ),
        &mut held,
    );
    batches.push(kinds(&events));
    flat.extend(events.iter().cloned());
    assert_eq!(
        events[0],
        AgentEvent::ToolExecutionStart {
            call: pir::durable::harness::events::ToolEventCall {
                tool_call_id: "c1".to_owned(),
                tool_name: "print".to_owned(),
                task_id: Some(TaskId::from_raw(20)),
                parent_tool_call_id: None,
                parent_task_id: None,
            },
            args: json!({"n": 1}).as_object().cloned().unwrap(),
        }
    );
    before = after;

    // 6. Output is appended.
    let mut after = before.clone();
    after.live.tools.as_mut().unwrap()[0].progress.output = Some("one\n".to_owned());
    let events = translate(
        conversation(),
        &before,
        &after,
        &[Op::Append(
            live_path(&[key("tools"), Seg::Index(0), key("output")]),
            "one\n".to_owned(),
        )],
        &publication(6, Vec::new()),
        &mut held,
    );
    batches.push(kinds(&events));
    flat.extend(events.iter().cloned());
    assert_eq!(
        events[0],
        AgentEvent::ToolExecutionUpdate {
            call: pir::durable::harness::events::ToolEventCall {
                tool_call_id: "c1".to_owned(),
                tool_name: "print".to_owned(),
                task_id: Some(TaskId::from_raw(20)),
                parent_tool_call_id: None,
                parent_task_id: None,
            },
            output: Some(ToolOutputChange::Window {
                trim_start: None,
                append: Some("one\n".to_owned()),
            }),
            details: pir::durable::harness::events::Tri::Absent,
            diagnostics: pir::durable::harness::events::Tri::Absent,
        }
    );
    before = after;

    // 7. The retained window slides: a trim and then an append.
    let mut after = before.clone();
    after.live.tools.as_mut().unwrap()[0].progress.output = Some("two\nthree\n".to_owned());
    let events = translate(
        conversation(),
        &before,
        &after,
        &[
            Op::Truncate(live_path(&[key("tools"), Seg::Index(0), key("output")]), 4),
            Op::Append(
                live_path(&[key("tools"), Seg::Index(0), key("output")]),
                "three\n".to_owned(),
            ),
        ],
        &publication(7, Vec::new()),
        &mut held,
    );
    batches.push(kinds(&events));
    flat.extend(events.iter().cloned());
    assert_eq!(
        events[0],
        AgentEvent::ToolExecutionUpdate {
            call: pir::durable::harness::events::ToolEventCall {
                tool_call_id: "c1".to_owned(),
                tool_name: "print".to_owned(),
                task_id: Some(TaskId::from_raw(20)),
                parent_tool_call_id: None,
                parent_task_id: None,
            },
            output: Some(ToolOutputChange::Window {
                trim_start: Some(4),
                append: Some("three\n".to_owned()),
            }),
            details: pir::durable::harness::events::Tri::Absent,
            diagnostics: pir::durable::harness::events::Tri::Absent,
        }
    );
    before = after;

    // 8. The call ends with its result entry, which follows directly.
    let result = entry(
        4,
        "pi.tool-result",
        Some(vec![tool_result_message("c1", "print")]),
    );
    let mut after = before.clone();
    after.entries.push(result.clone());
    after.live.tools.as_mut().unwrap()[0].progress.status = SlotStatus::Done;
    after.live.tools.as_mut().unwrap()[0].entry = Some(result.id);
    let events = translate(
        conversation(),
        &before,
        &after,
        &[],
        &publication(
            8,
            vec![
                entry_change(result.clone()),
                task_change(task(
                    20,
                    "pi.tool",
                    TaskState::Terminal {
                        outcome: TaskOutcome::Completed {
                            result: json!(null),
                        },
                    },
                )),
            ],
        ),
        &mut held,
    );
    batches.push(kinds(&events));
    flat.extend(events.iter().cloned());
    assert_eq!(
        events[0],
        AgentEvent::ToolExecutionEnd {
            call: pir::durable::harness::events::ToolEventCall {
                tool_call_id: "c1".to_owned(),
                tool_name: "print".to_owned(),
                task_id: Some(TaskId::from_raw(20)),
                parent_tool_call_id: None,
                parent_task_id: None,
            },
            entry: Some(result),
            result: None,
        }
    );
    before = after;

    // 9. The generation holds its outcome: the turn ends; then the run ends.
    let completing = task(
        10,
        "pi.generation",
        TaskState::Completing {
            outcome: TaskOutcome::Completed {
                result: json!(null),
            },
        },
    );
    let mut after = before.clone();
    after.live.run = None;
    let events = translate(
        conversation(),
        &before,
        &after,
        &[],
        &publication(9, vec![task_change(completing)]),
        &mut held,
    );
    batches.push(kinds(&events));
    flat.extend(events.iter().cloned());
    before = after;

    // 10. The generation goes terminal; its turn already ended, and the usage
    //     ledger changed.
    let mut after = before.clone();
    after.usage = Some(pir::durable::harness::usage::UsageState {
        models: std::collections::BTreeMap::new(),
        tools: std::collections::BTreeMap::new(),
    });
    let events = translate(
        conversation(),
        &before,
        &after,
        &[],
        &publication(
            10,
            vec![task_change(task(
                10,
                "pi.generation",
                TaskState::Terminal {
                    outcome: TaskOutcome::Completed {
                        result: json!(null),
                    },
                },
            ))],
        ),
        &mut held,
    );
    batches.push(kinds(&events));
    flat.extend(events.iter().cloned());

    assert_eq!(
        batches,
        vec![
            vec![
                "message_start",
                "message_end",
                "submission",
                "run_start",
                "turn_start"
            ],
            vec!["message_start"],
            vec!["message_update"],
            vec!["message_update"],
            vec!["tool_execution_start", "message_end"],
            vec!["tool_execution_update"],
            vec!["tool_execution_update"],
            vec!["tool_execution_end", "message_start", "message_end"],
            vec!["turn_end", "run_end"],
            vec!["usage_changed"],
        ]
    );

    // The deltas rebuild every committed partial.
    let mut rebuilt = flat
        .iter()
        .find_map(|event| match event {
            AgentEvent::MessageStart {
                message: pir::ai::types::Message::Assistant(message),
            } if message.content.len() == 1
                && serde_json::to_value(message).unwrap()["content"][0]["text"] == json!("He") =>
            {
                Some(serde_json::to_value(message).unwrap())
            }
            _ => None,
        })
        .expect("the assistant partial started");
    for event in &flat {
        if let AgentEvent::MessageUpdate { changes, .. } = event {
            rebuilt = apply_changes(&rebuilt, changes);
        }
    }
    assert_eq!(
        rebuilt["content"],
        json!([text_block("Hello"), thinking_block("hm")])
    );
}

// ── nested calls ───────────────────────────────────────────────────────────

#[test]
fn a_nested_call_ends_with_the_result_its_task_document_stored() {
    let mut before = view(Vec::new(), LiveState::new());
    before.live.nested_tools = Some(vec![NestedToolSlot {
        progress: {
            let mut progress = SlotProgress::new("c1/sub", "read");
            progress.status = SlotStatus::Running;
            progress
        },
        parent_call_id: "c1".to_owned(),
        parent_task_id: TaskId::from_raw(20),
        task_id: TaskId::from_raw(21),
        arguments: json!({"path": "a"}).as_object().cloned().unwrap(),
        summary: None,
    }]);

    let state = NestedCallState {
        task_id: Some(TaskId::from_raw(21)),
        result: Some(NestedToolExecutionResult {
            task_id: TaskId::from_raw(21),
            structured_output: Some(json!("done")),
            is_error: false,
            details: None,
            diagnostics: vec![ToolDiagnostic {
                severity: "warn".to_owned(),
                message: "careful".to_owned(),
                code: None,
            }],
            usage: None,
            duration_ms: Some(5),
        }),
    };
    let mut after = before.clone();
    after.live.nested_tools.as_mut().unwrap()[0].progress.status = SlotStatus::Done;

    let record = DocumentRecord {
        id: pir::durable::ids::DocumentId::from_raw(30),
        kind: pir::durable::harness::events::NESTED_CALL_KIND.to_owned(),
        key: Some("sub".to_owned()),
        created_at: Seq::from_raw(1),
        retired_at: None,
        scope: DocumentScope::Task {
            task_id: TaskId::from_raw(20),
        },
        history: None,
        fork: None,
    };
    let publication = publication(
        1,
        vec![CommitChange::Document(DocumentCommitChange::Document {
            record,
            conversation_id: Some(conversation()),
            version: Some(1),
            value: Some(
                serde_json::to_value(&state)
                    .unwrap()
                    .as_object()
                    .cloned()
                    .unwrap(),
            ),
            ops: Vec::new(),
        })],
    );

    let mut held = HashSet::new();
    let events = translate(
        conversation(),
        &before,
        &after,
        // The slot's status change is a view operation, which is what makes
        // the batch non-empty.
        &[Op::Set(
            live_path(&[key("nestedTools"), Seg::Index(0), key("status")]),
            json!("done"),
        )],
        &publication,
        &mut held,
    );
    assert_eq!(kinds(&events), vec!["tool_execution_end"]);
    assert_eq!(
        events[0],
        AgentEvent::ToolExecutionEnd {
            call: pir::durable::harness::events::ToolEventCall {
                tool_call_id: "c1/sub".to_owned(),
                tool_name: "read".to_owned(),
                task_id: Some(TaskId::from_raw(21)),
                parent_tool_call_id: Some("c1".to_owned()),
                parent_task_id: Some(TaskId::from_raw(20)),
            },
            entry: None,
            result: state.result,
        }
    );
}

// ── snapshots ──────────────────────────────────────────────────────────────

#[test]
fn the_snapshot_carries_the_whole_state_and_reads_retired_documents_as_initial() {
    let mut live = LiveState::new();
    live.run = Some(run(10, vec![5]));
    live.generation = Some(generation(2, Some(assistant(json!([text_block("hi")])))));
    live.tools = Some(vec![tool_slot("c1", "print", 20, SlotStatus::Running)]);
    let view = ViewPort {
        entries: vec![entry(2, "pi.user", Some(vec![user_message("hi")]))],
        live,
        inbox: Some(pir::durable::harness::inbox::InboxState {
            items: vec![pir::durable::harness::inbox::InboxItem::Input {
                id: SubmissionId::from_raw(5),
                mode: pir::durable::harness::inbox::InputMode::FollowUp,
                content: json!("later"),
                entry: None,
            }],
        }),
        agent: None,
        usage: None,
    };
    let snapshot = snapshot_of(&view);
    assert_eq!(snapshot.entries.len(), 1);
    assert_eq!(
        snapshot.run,
        Some(pir::durable::harness::events::RunInputs {
            inputs: vec![SubmissionId::from_raw(5)]
        })
    );
    assert_eq!(snapshot.generation.as_ref().map(|g| g.attempt), Some(2));
    assert_eq!(snapshot.tools.len(), 1);
    assert_eq!(snapshot.inbox.len(), 1);
    assert_eq!(
        snapshot.inbox[0].mode,
        pir::durable::harness::events::QueuedMode::FollowUp
    );
    // Absent documents read as their initial values.
    assert_eq!(
        snapshot.agent,
        pir::durable::harness::agent::AgentState::default()
    );
    assert_eq!(
        snapshot.usage,
        pir::durable::harness::usage::UsageState::default()
    );

    // The wire event carries the tag.
    let event = AgentEvent::Snapshot(snapshot.clone());
    let value = serde_json::to_value(&event).expect("snapshot serializes");
    assert_eq!(value["type"], json!("snapshot"));
    assert_eq!(value["entries"].as_array().unwrap().len(), 1);
}

// ── the stream ─────────────────────────────────────────────────────────────

fn open_session() -> Session {
    let storage: Arc<dyn Storage> = Arc::new(MemoryStorage::new());
    Session::open(storage, SessionOptions::default())
}

async fn create_conversation(session: &Session) -> ConversationId {
    session
        .create_conversation(
            ConversationOwnership::Ownerless,
            None,
            Context::background(),
        )
        .await
        .expect("create conversation")
        .id
}

fn note_draft(text: &str) -> EntryDraft {
    EntryDraft {
        kind: "note".to_owned(),
        model: None,
        data: Some(json!({"text": text})),
        head: None,
        edits: None,
    }
}

async fn append_note(session: &Session, conversation: ConversationId, text: &str) -> EntryRecord {
    session
        .append_entry(conversation, note_draft(text), Context::background())
        .await
        .expect("append entry")
}

#[tokio::test(flavor = "current_thread")]
async fn a_backlog_of_one_hundred_batches_collapses_to_one_fresh_snapshot() {
    let session = open_session();
    let conversation = create_conversation(&session).await;
    let views = Arc::new(
        ConversationViews::open(session.clone())
            .await
            .expect("open view adapter"),
    );
    let stream = watch_events(&views, conversation, Context::background())
        .await
        .expect("attach event stream");
    assert!(stream.snapshot().entries.is_empty(), "the fresh transcript");

    // 101 commits with no listener yet: the 101st push collapses the 100
    // undelivered batches into one snapshot of the newest state.
    for index in 0..101 {
        append_note(&session, conversation, &format!("note {index}")).await;
    }
    assert!(
        stream.overflowed(),
        "the undelivered backlog collapsed at the 101st batch"
    );

    let batches: Arc<Mutex<Vec<Vec<AgentEvent>>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = batches.clone();
    stream
        .start(move |events, _context| {
            sink.lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(events.to_vec());
        })
        .expect("start the stream");
    stream.drain();

    assert!(stream.overflowed(), "the backlog overflowed");
    {
        let batches = batches.lock().unwrap();
        assert_eq!(batches.len(), 1, "one collapsing batch");
        assert_eq!(batches[0].len(), 1, "one event: the snapshot");
        match &batches[0][0] {
            AgentEvent::Snapshot(SnapshotEvent { entries, .. }) => {
                assert_eq!(entries.len(), 101, "the newest state, not the backlog")
            }
            other => panic!("expected a snapshot, got {}", other.kind()),
        }
    }

    assert_eq!(stream.stop(), WatchEnd::Stopped);
    session.close().await.expect("close");
}

#[tokio::test(flavor = "current_thread")]
async fn the_stream_starts_from_a_snapshot_and_ends_with_the_session() {
    let session = open_session();
    let conversation = create_conversation(&session).await;
    // An entry committed before attachment is part of the snapshot.
    let early = append_note(&session, conversation, "early").await;

    let views = Arc::new(
        ConversationViews::open(session.clone())
            .await
            .expect("open view adapter"),
    );
    let stream = watch_events(&views, conversation, Context::background())
        .await
        .expect("attach event stream");
    assert_eq!(stream.snapshot().entries, vec![early.clone()]);
    assert_eq!(
        serde_json::to_value(stream.snapshot_event()).unwrap()["type"],
        json!("snapshot")
    );

    let batches: Arc<Mutex<Vec<Vec<AgentEvent>>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = batches.clone();
    stream
        .start(move |events, _context| {
            sink.lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(events.to_vec());
        })
        .expect("start the stream");
    // Starting twice fails, like upstream's watch.
    assert!(stream.start(|_, _| {}).is_err());

    let later = append_note(&session, conversation, "later").await;
    stream.drain();
    {
        let batches = batches.lock().unwrap();
        assert_eq!(batches.len(), 1);
        assert_eq!(
            batches[0],
            vec![AgentEvent::EntryAppended {
                entry: later.clone()
            }]
        );
    }

    session.close().await.expect("close");
    assert_eq!(stream.closed().await, WatchEnd::SessionClosed);
    assert_eq!(stream.reason(), Some(WatchEnd::SessionClosed));
}

#[tokio::test(flavor = "current_thread")]
async fn retired_documents_read_as_their_initial_values() {
    let session = open_session();
    let conversation = create_conversation(&session).await;
    let views = Arc::new(
        ConversationViews::open(session.clone())
            .await
            .expect("open view adapter"),
    );
    let stream = watch_events(&views, conversation, Context::background())
        .await
        .expect("attach event stream");
    let batches: Arc<Mutex<Vec<Vec<AgentEvent>>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = batches.clone();
    stream
        .start(move |events, _context| {
            sink.lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(events.to_vec());
        })
        .expect("start");

    // Write and then retire `pi.usage`; the event reports the initial ledger.
    let definition = pir::durable::harness::USAGE_DEFINITION.clone();
    let address = pir::durable::harness::harness::usage_address(conversation);
    session
        .doc(
            address.clone(),
            definition,
            Context::background(),
            |_draft| {},
        )
        .await
        .expect("create pi.usage");
    stream.drain();
    batches
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clear();

    session
        .retire_doc(address, Context::background())
        .await
        .expect("retire pi.usage");
    stream.drain();
    {
        let batches = batches.lock().unwrap();
        assert_eq!(batches.len(), 1);
        assert_eq!(
            batches[0],
            vec![AgentEvent::UsageChanged {
                usage: pir::durable::harness::usage::UsageState::default()
            }]
        );
    }

    session.close().await.expect("close");
}

#[tokio::test(flavor = "current_thread")]
async fn an_unknown_conversation_rejects_the_attachment() {
    let session = open_session();
    let views = Arc::new(
        ConversationViews::open(session.clone())
            .await
            .expect("open view adapter"),
    );
    let error: StorageError = watch_events(
        &views,
        ConversationId::from_raw(900_000),
        Context::background(),
    )
    .await
    .expect_err("an unknown conversation is rejected");
    // The mount reports the conversation does not exist.
    assert!(!error.to_string().is_empty());
    session.close().await.expect("close");
}
