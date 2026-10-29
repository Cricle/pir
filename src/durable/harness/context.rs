//! Context derivation: the ten rules of spec §2.1.
//!
//! Port of upstream `packages/durable/src/harness/context.ts` (the pure part).
//! The async readers (`captureContextBounds`, `readContext`, `readContextFrom`,
//! `deriveRange`, `extendRange`) belong to the Session/harness glue that scans
//! storage; this module derives the same [`ContextView`] from an already
//! scanned, oldest-first visible range:
//!
//! * the range is every visible entry at or below the cutoff, or just the
//!   entries from `H.head` through it when a head marker `H` applies;
//! * per target, the newest edit in the range wins (`omit` contributes
//!   nothing, `replace` contributes its messages);
//! * entries are `H` followed by the range's non-head entries (the whole range
//!   without `H`);
//! * assistant messages with `aborted`, `error` or `deferred` stop reasons are
//!   dropped, and model-less entries contribute nothing;
//! * each assistant's tool results follow it in tool-call order, a missing
//!   result is synthesized as an error, and a result with no preceding call is
//!   dropped;
//! * the first system message leads when only user messages precede it or when
//!   it is the baseline's (the first `pi.system` after `H`, unless an edit
//!   targets it; without `H`, the first `pi.system` unless an earlier entry
//!   carries a system message).
//!
//! The baseline is identified by *provenance* (which entry's own model list, at
//! which position), not by value: upstream compares object identity, and a
//! replacement edit that happens to produce an identical message is still not
//! the baseline.

use std::collections::BTreeMap;

use serde::Deserialize;
use serde_json::Value;

use crate::ai::types::{
    ContentBlock, Message, StopReason, TextContent, ToolCall, ToolResultMessage, ToolResultRole,
};
use crate::durable::entries::SystemEntry;
use crate::durable::ids::EntryId;
use crate::durable::types::{ContextEdit, ContextEditAction, EntryRecord};

/// The result text of a synthesized tool result: the range ends before the call
/// completed, for example after a fork cut or at an interrupted tail.
pub const MISSING_RESULT_TEXT: &str =
    "Tool result unavailable: history ends before this call completed.";

/// Raw active transcript and derived model context (upstream `ContextView`).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ContextView {
    /// The newest applicable head marker, if any.
    pub head: Option<EntryRecord>,
    /// Raw active entries: the head marker followed by the range's non-head
    /// entries, or the whole range without a marker.
    pub entries: Vec<EntryRecord>,
    /// Per entry of [`Self::entries`], its model messages after edits and
    /// excluded stop reasons, before tool-result ordering.
    pub contributions: Vec<Vec<Message>>,
    /// Model context for the next provider request.
    pub messages: Vec<Message>,
}

impl ContextView {
    /// An empty view: no head, no entries, no messages.
    pub fn empty() -> Self {
        Self::default()
    }
}

/// Where one contributed message came from.
///
/// Upstream's `leadWithSystem()` compares the first system message with the
/// baseline by object identity. A value comparison would mistake a replacement
/// edit that restates the same message for the baseline, so the provenance is
/// carried instead: [`Provenance::Model`] is an entry's own `model` list,
/// [`Provenance::Replacement`] is the message list of a `replace` edit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Provenance {
    /// `active[entry].model[index]`.
    Model { entry: usize, index: usize },
    /// `active[entry]`'s `replace` edit, message `index`.
    Replacement { entry: usize, index: usize },
}

/// One message plus the entry position it came from.
#[derive(Clone, Debug)]
struct Located {
    /// `None` for a synthesized missing-result message.
    provenance: Option<Provenance>,
    message: Message,
}

/// What `find_baseline()` decided about the range's baseline.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Baseline {
    /// The range cannot have a baseline: an entry carries a system message
    /// before the first `pi.system` (without a head marker).
    Cannot,
    /// A later entry may still be the baseline.
    Pending,
    /// The baseline is this message of the active entries.
    Message(Provenance),
}

/// Derive the active transcript and model context of one conversation.
///
/// `entries` is the conversation's visible history in ascending ID order (the
/// caller's `scan_entries` result, oldest first). `cutoff` reads the context as
/// of that visible entry, inclusive; `None` reads the current context. Entries
/// after the cutoff are ignored, and the head marker is the newest one at or
/// before it.
#[must_use]
pub fn derive_context(entries: &[EntryRecord], cutoff: Option<EntryId>) -> ContextView {
    let in_range = |entry: &EntryRecord| cutoff.is_none_or(|cutoff| entry.id <= cutoff);

    // Rule 1: the newest visible head marker at or before the cutoff.
    let head = entries
        .iter()
        .filter(|entry| entry.head.is_some() && in_range(entry))
        .max_by_key(|entry| entry.id)
        .cloned();

    // Rules 2 and 3: from `H.head` (or transcript start) through the cutoff.
    let from = head.as_ref().and_then(|head| head.head);
    let range: Vec<&EntryRecord> = entries
        .iter()
        .filter(|entry| in_range(entry) && from.is_none_or(|from| entry.id >= from))
        .collect();

    // Rule 4: the newest edit per target wins. Edits carried by older head
    // markers in the range count too (they are dropped from `active` below but
    // their edits are still in the range).
    let mut edits: BTreeMap<EntryId, &ContextEdit> = BTreeMap::new();
    for entry in &range {
        for edit in entry.edits.iter().flatten() {
            edits.insert(edit.target, edit);
        }
    }

    // Rule 5: the head marker first, then the range's non-head entries.
    let active: Vec<&EntryRecord> = match &head {
        Some(head) => std::iter::once(head)
            .chain(range.iter().copied().filter(|entry| entry.head.is_none()))
            .collect(),
        None => range,
    };

    // Rules 6-9: contributions before tool-result ordering.
    let contributed: Vec<Vec<(Provenance, Message)>> = active
        .iter()
        .enumerate()
        .map(|(index, entry)| contribute(index, entry, edits.get(&entry.id).copied()))
        .collect();
    let contributions: Vec<Vec<Message>> = contributed
        .iter()
        .map(|messages| {
            messages
                .iter()
                .map(|(_, message)| message.clone())
                .collect()
        })
        .collect();

    // Rules 6-8 and 10: order tool results, then lead with the baseline. The
    // contribution lists already took the copy the view keeps, so the messages
    // move into the flattened sequence rather than being cloned again.
    let flat: Vec<Located> = contributed
        .into_iter()
        .flatten()
        .map(|(provenance, message)| Located {
            provenance: Some(provenance),
            message,
        })
        .collect();
    let baseline = find_baseline(head.as_ref(), &active);
    let messages = lead_with_system(order_tool_results(&flat), &baseline)
        .into_iter()
        .map(|located| located.message)
        .collect();

    // `active` borrows `head`, so it must be consumed before `head` moves out.
    let active_entries: Vec<EntryRecord> = active.into_iter().cloned().collect();

    ContextView {
        head,
        entries: active_entries,
        contributions,
        messages,
    }
}

/// Rule 9: one entry's messages after its edit, without assistant messages later
/// requests must not see.
fn contribute(
    index: usize,
    entry: &EntryRecord,
    edit: Option<&ContextEdit>,
) -> Vec<(Provenance, Message)> {
    match edit.map(|edit| &edit.action) {
        Some(ContextEditAction::Omit) => Vec::new(),
        Some(ContextEditAction::Replace { messages }) => indexed_messages(messages)
            .into_iter()
            .filter(|(_, message)| is_kept(message))
            .map(|(message_index, message)| {
                (
                    Provenance::Replacement {
                        entry: index,
                        index: message_index,
                    },
                    message,
                )
            })
            .collect(),
        None => entry
            .model
            .as_deref()
            .map(indexed_messages)
            .unwrap_or_default()
            .into_iter()
            .filter(|(_, message)| is_kept(message))
            .map(|(message_index, message)| {
                (
                    Provenance::Model {
                        entry: index,
                        index: message_index,
                    },
                    message,
                )
            })
            .collect(),
    }
}

/// Whether a message is kept in model context (rule 9).
fn is_kept(message: &Message) -> bool {
    match message {
        Message::Assistant(assistant) => !matches!(
            assistant.stop_reason,
            StopReason::Aborted | StopReason::Error | StopReason::Deferred
        ),
        _ => true,
    }
}

/// Parse a model list, keeping the raw index for provenance.
///
/// A value that does not deserialize as a [`Message`] cannot be sent to a
/// provider and is dropped, like an unreadable record field in a lenient read.
fn indexed_messages(values: &[Value]) -> Vec<(usize, Message)> {
    values
        .iter()
        .enumerate()
        .filter_map(|(index, value)| {
            Message::deserialize(value)
                .ok()
                .map(|message| (index, message))
        })
        .collect()
}

/// Rule 7 of spec §2.1: place each assistant's tool results directly after it in
/// call order, synthesize a missing one, and drop results with no leading call.
///
/// Upstream splits the sequence at the last assistant message (`settle()`), but
/// orders each part with this same function; ordering the whole sequence at once
/// yields the same result, because a later part always starts with the assistant
/// whose calls cannot reference results in an earlier part.
fn order_tool_results(messages: &[Located]) -> Vec<Located> {
    let mut ordered: Vec<Located> = Vec::with_capacity(messages.len());
    for (index, located) in messages.iter().enumerate() {
        if matches!(&located.message, Message::ToolResult(_)) {
            continue;
        }
        ordered.push(located.clone());
        let Message::Assistant(assistant) = &located.message else {
            continue;
        };
        let calls: Vec<&ToolCall> = assistant
            .content
            .iter()
            .filter_map(|block| match block {
                ContentBlock::ToolCall(call) => Some(call),
                _ => None,
            })
            .collect();
        if calls.is_empty() {
            continue;
        }
        // Results are found before the next assistant message; the first result
        // of a call id wins.
        let mut results: BTreeMap<&str, usize> = BTreeMap::new();
        for (candidate_index, candidate) in messages.iter().enumerate().skip(index + 1) {
            if matches!(&candidate.message, Message::Assistant(_)) {
                break;
            }
            if let Message::ToolResult(result) = &candidate.message {
                results
                    .entry(result.tool_call_id.as_str())
                    .or_insert(candidate_index);
            }
        }
        for call in calls {
            match results.get(call.id.as_str()) {
                Some(&result_index) => ordered.push(messages[result_index].clone()),
                None => ordered.push(Located {
                    provenance: None,
                    message: missing_result(call, assistant.timestamp),
                }),
            }
        }
    }
    ordered
}

/// The synthesized error result of a call whose result is not in the range.
fn missing_result(call: &ToolCall, timestamp: u64) -> Message {
    Message::ToolResult(ToolResultMessage {
        role: ToolResultRole::ToolResult,
        tool_call_id: call.id.clone(),
        tool_name: call.name.clone(),
        content: vec![ContentBlock::Text(TextContent {
            text: MISSING_RESULT_TEXT.to_owned(),
            text_signature: None,
        })],
        details: Some(serde_json::json!({ "reason": "missing_result" })),
        usage: None,
        added_tool_names: Vec::new(),
        structured_content: None,
        nested_calls: None,
        is_error: true,
        duration_ms: None,
        timestamp,
    })
}

/// Rule 10: find the baseline among the active entries, oldest first.
///
/// After a head marker, the baseline is the first `pi.system` entry whose ID is
/// greater than the marker's; retained `pi.system` entries at or before the
/// marker cannot be it. Without a marker, the first `pi.system` entry is the
/// baseline unless an earlier entry already carries a system message (in its
/// model, or in a `replace` edit) — even one a later edit omits — because only
/// then did generation write it with every section and tool in effect.
fn find_baseline(head: Option<&EntryRecord>, active: &[&EntryRecord]) -> Baseline {
    for (index, entry) in active.iter().enumerate() {
        if let Some(head) = head {
            if entry.id <= head.id {
                continue;
            }
        }
        if SystemEntry::is(Some(entry)) {
            return match first_system_index(entry) {
                Some(message_index) => Baseline::Message(Provenance::Model {
                    entry: index,
                    index: message_index,
                }),
                None => Baseline::Cannot,
            };
        }
        if head.is_none() && carries_system_message(entry) {
            return Baseline::Cannot;
        }
    }
    Baseline::Pending
}

/// The raw index of an entry's first system message in its own `model` list.
fn first_system_index(entry: &EntryRecord) -> Option<usize> {
    entry.model.as_deref()?.iter().position(is_system_value)
}

/// Whether an entry's own messages, or the replacement messages of its edits,
/// include a system message.
fn carries_system_message(entry: &EntryRecord) -> bool {
    entry
        .model
        .as_deref()
        .is_some_and(|messages| messages.iter().any(is_system_value))
        || entry.edits.iter().flatten().any(|edit| {
            matches!(
                &edit.action,
                ContextEditAction::Replace { messages }
                    if messages.iter().any(is_system_value)
            )
        })
}

/// Whether a raw message value is a system message.
fn is_system_value(value: &Value) -> bool {
    Message::deserialize(value).is_ok_and(|message| matches!(message, Message::System(_)))
}

/// Rule 10: move the first system message to the front when only user messages
/// precede it, or when it is the baseline's.
///
/// Only the first system message moves, so replaying system messages in order
/// yields the same sections and tools. Any other system message, after an
/// assistant message, stays where it was committed: moving it would change what
/// it means to the model.
fn lead_with_system(mut messages: Vec<Located>, baseline: &Baseline) -> Vec<Located> {
    let Some(index) = messages
        .iter()
        .position(|located| matches!(&located.message, Message::System(_)))
    else {
        return messages;
    };
    if index == 0 {
        return messages;
    }
    let is_baseline = match (baseline, messages[index].provenance) {
        (Baseline::Message(expected), Some(found)) => *expected == found,
        _ => false,
    };
    if !is_baseline
        && messages[..index]
            .iter()
            .any(|located| !matches!(&located.message, Message::User(_)))
    {
        return messages;
    }
    let system = messages.remove(index);
    messages.insert(0, system);
    messages
}
