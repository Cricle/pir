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
use std::ops::Deref;
use std::sync::{Arc, OnceLock};

use serde::Deserialize;
use serde_json::Value;

use crate::ai::types::{
    ContentBlock, Message, StopReason, TextContent, ToolResultMessage, ToolResultRole,
};
use crate::durable::entries::SystemEntry;
use crate::durable::ids::EntryId;
use crate::durable::types::{ContextEdit, ContextEditAction, EntryRecord};

/// The result text of a synthesized tool result: the range ends before the call
/// completed, for example after a fork cut or at an interrupted tail.
pub const MISSING_RESULT_TEXT: &str =
    "Tool result unavailable: history ends before this call completed.";

/// Raw active transcript and derived model context (upstream `ContextView`).
///
/// A derived view keeps one parsed copy of the messages it sends
/// ([`Self::messages`]) plus the raw entries in one shared [`Arc`] slice.
/// [`Self::contributions`] is a lightweight per-entry index that parses its
/// message lists only when a compaction path reads them, so an ordinary
/// `read_context` does not retain a second copy of the whole transcript.
/// [`Self::messages`] is likewise shared and derived on first access, so a
/// context read that only needs the raw transcript does not parse every model
/// message into a second full copy.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ContextView {
    /// The newest applicable head marker, if any.
    pub head: Option<EntryRecord>,
    /// Raw active entries: the head marker followed by the range's non-head
    /// entries, or the whole range without a marker.
    pub entries: Arc<[EntryRecord]>,
    /// Per entry of [`Self::entries`], its model messages after edits and
    /// excluded stop reasons, before tool-result ordering.
    ///
    /// The lists are derived from [`Self::entries`] on first access and kept
    /// in this view's shared cache; iterating or indexing the value materializes
    /// them. An empty view has no entries and therefore no contributions.
    pub contributions: Contributions,
    /// Model context for the next provider request.
    ///
    /// The ordered message list is derived from [`Self::entries`], the winning
    /// edits and the baseline on first access. Cloning the view shares the
    /// materialized list instead of deep-cloning every message.
    pub messages: Messages,
}

impl ContextView {
    /// An empty view: no head, no entries, no messages.
    pub fn empty() -> Self {
        Self::default()
    }

    /// Consume the view and return its ordered model messages.
    ///
    /// Materializes the lazy list once and moves it out of the shared cache
    /// when this view is the only owner, so provider handoff does not deep
    /// clone the transcript a second time.
    #[must_use]
    pub fn into_messages(self) -> Vec<Message> {
        self.messages.into_vec()
    }
}

/// Ordered model messages of a [`ContextView`], materialized lazily.
///
/// The wrapper dereferences to `[Message]`, so existing callers can keep using
/// `len()`, indexing, slicing and iteration. The ordered list (rule ordering,
/// tool-result placement and the baseline move) is derived from the view's
/// shared entries on first access and then cached in the same `Arc` as the
/// view. A context read that never sends a provider request pays neither the
/// second message copy nor its CPU.
#[derive(Clone, Default)]
pub struct Messages {
    inner: Arc<MessagesInner>,
}

#[derive(Default)]
struct MessagesInner {
    /// Active entries in [`ContextView::entries`] order.
    entries: Arc<[EntryRecord]>,
    /// The winning edit per active entry, if any (shared with
    /// [`Contributions`]).
    edits: Arc<[Option<ContextEdit>]>,
    /// The baseline decision used by rule 10.
    baseline: Baseline,
    /// Ordered messages, built on first access.
    messages: OnceLock<Vec<Message>>,
}

impl Messages {
    fn new(
        entries: Arc<[EntryRecord]>,
        edits: Arc<[Option<ContextEdit>]>,
        baseline: Baseline,
    ) -> Self {
        Self {
            inner: Arc::new(MessagesInner {
                entries,
                edits,
                baseline,
                messages: OnceLock::new(),
            }),
        }
    }

    fn materialized(&self) -> &[Message] {
        self.inner.messages.get_or_init(|| {
            derive_messages(&self.inner.entries, &self.inner.edits, &self.inner.baseline)
        })
    }

    /// Iterate the ordered messages, materializing them on first use.
    pub fn iter(&self) -> std::slice::Iter<'_, Message> {
        self.materialized().iter()
    }

    /// The ordered messages as a slice, materializing them on first use.
    #[must_use]
    pub fn as_slice(&self) -> &[Message] {
        self.materialized()
    }

    /// Test seam: whether the ordered message list was already built.
    ///
    /// A freshly derived view must answer `false`; only a provider request,
    /// compaction or an explicit slice/index access materializes it.
    #[doc(hidden)]
    #[must_use]
    pub fn is_materialized(&self) -> bool {
        self.inner.messages.get().is_some()
    }

    /// Test seam: whether this list shares `view`'s entry allocation.
    #[doc(hidden)]
    #[must_use]
    pub fn shares_entries_with(&self, view: &ContextView) -> bool {
        Arc::ptr_eq(&self.inner.entries, &view.entries)
    }

    /// Test seam: whether two lists share the same cached message vector.
    #[doc(hidden)]
    #[must_use]
    pub fn shares_messages_with(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.inner, &other.inner)
    }

    /// Clone the ordered messages into a fresh vector.
    #[must_use]
    pub fn to_vec(&self) -> Vec<Message> {
        self.materialized().to_vec()
    }

    /// Consume the list, returning the cached vector without cloning it when
    /// this [`Messages`] is the only owner.
    #[must_use]
    pub fn into_vec(self) -> Vec<Message> {
        if self.inner.messages.get().is_none() {
            let _ = self.materialized();
        }
        match Arc::try_unwrap(self.inner) {
            Ok(inner) => inner.messages.into_inner().unwrap_or_default(),
            Err(inner) => inner.messages.get().cloned().unwrap_or_default(),
        }
    }
}

impl Deref for Messages {
    type Target = [Message];

    fn deref(&self) -> &Self::Target {
        self.materialized()
    }
}

impl AsRef<[Message]> for Messages {
    fn as_ref(&self) -> &[Message] {
        self.materialized()
    }
}

impl<'a> IntoIterator for &'a Messages {
    type Item = &'a Message;
    type IntoIter = std::slice::Iter<'a, Message>;

    fn into_iter(self) -> Self::IntoIter {
        self.materialized().iter()
    }
}

impl IntoIterator for Messages {
    type Item = Message;
    type IntoIter = std::vec::IntoIter<Message>;

    fn into_iter(self) -> Self::IntoIter {
        self.into_vec().into_iter()
    }
}

impl PartialEq for Messages {
    fn eq(&self, other: &Self) -> bool {
        self.materialized() == other.materialized()
    }
}

impl PartialEq<Vec<Message>> for Messages {
    fn eq(&self, other: &Vec<Message>) -> bool {
        self.materialized() == other.as_slice()
    }
}

impl std::fmt::Debug for Messages {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Do not materialize (and thus do not parse messages) for formatting.
        f.debug_struct("Messages")
            .field("entries", &self.inner.entries.len())
            .field("materialized", &self.inner.messages.get().is_some())
            .finish()
    }
}

/// Per-entry contributed messages of a [`ContextView`], materialized lazily.
///
/// The wrapper dereferences to `[Vec<Message>]`, so existing callers can keep
/// using `len()`, indexing, slicing and iteration. The backing messages are
/// parsed from the view's shared entries the first time any of those are used
/// and then cached in the same `Arc` as the view; a request that never runs
/// compaction pays neither the second message copy nor its CPU.
#[derive(Clone, Default)]
pub struct Contributions {
    inner: Arc<ContributionsInner>,
}

#[derive(Default)]
struct ContributionsInner {
    /// Active entries in [`ContextView::entries`] order.
    entries: Arc<[EntryRecord]>,
    /// The winning edit per active entry, if any (shared with [`Messages`]).
    edits: Arc<[Option<ContextEdit>]>,
    /// Parsed per-entry messages, built on first access.
    messages: OnceLock<Vec<Vec<Message>>>,
}

impl Contributions {
    fn new(entries: Arc<[EntryRecord]>, edits: Arc<[Option<ContextEdit>]>) -> Self {
        Self {
            inner: Arc::new(ContributionsInner {
                entries,
                edits,
                messages: OnceLock::new(),
            }),
        }
    }

    fn materialized(&self) -> &[Vec<Message>] {
        self.inner.messages.get_or_init(|| {
            self.inner
                .entries
                .iter()
                .enumerate()
                .map(|(index, entry)| {
                    contribute(
                        index,
                        entry,
                        self.inner.edits.get(index).and_then(Option::as_ref),
                    )
                    .into_iter()
                    .map(|(_, message)| message)
                    .collect()
                })
                .collect()
        })
    }

    /// The number of active entries (the same value as [`Self::deref`]'s
    /// length), without materializing the messages.
    #[must_use]
    pub fn len(&self) -> usize {
        self.inner.entries.len()
    }

    /// Whether the view has no active entries, without materializing.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.inner.entries.is_empty()
    }

    /// Iterate the per-entry messages, materializing them on first use.
    pub fn iter(&self) -> std::slice::Iter<'_, Vec<Message>> {
        self.materialized().iter()
    }

    /// Test seam: whether the per-entry message lists were already built.
    ///
    /// A freshly derived view must answer `false`; only a compaction path (or
    /// an explicit slice/index access) materializes them.
    #[doc(hidden)]
    #[must_use]
    pub fn is_materialized(&self) -> bool {
        self.inner.messages.get().is_some()
    }

    /// Test seam: whether this index shares `view`'s entry allocation.
    ///
    /// `true` proves the view does not own a second copy of the transcript.
    #[doc(hidden)]
    #[must_use]
    pub fn shares_entries_with(&self, view: &ContextView) -> bool {
        Arc::ptr_eq(&self.inner.entries, &view.entries)
    }
}

impl Deref for Contributions {
    type Target = [Vec<Message>];

    fn deref(&self) -> &Self::Target {
        self.materialized()
    }
}

impl PartialEq for Contributions {
    fn eq(&self, other: &Self) -> bool {
        self.materialized() == other.materialized()
    }
}

impl std::fmt::Debug for Contributions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Do not materialize (and thus do not clone messages) for formatting.
        f.debug_struct("Contributions")
            .field("entries", &self.inner.entries.len())
            .finish()
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
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum Baseline {
    /// The range cannot have a baseline: an entry carries a system message
    /// before the first `pi.system` (without a head marker).
    Cannot,
    /// A later entry may still be the baseline.
    #[default]
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
    let selection = select_active(entries, cutoff);
    // The borrowed entry point cannot move records out of its input, so the
    // active entries are cloned once here. `derive_context_owned` is the
    // harness path and moves them instead.
    let active_entries = selection
        .indices
        .iter()
        .map(|&index| entries[index].clone())
        .collect();
    build_view(active_entries, selection.edits)
}

/// [`derive_context`] over owned entries.
///
/// The harness scan owns the `EntryRecord`s it read; this variant moves the
/// active ones into the view instead of cloning a second full transcript.
#[must_use]
pub(crate) fn derive_context_owned(
    entries: Vec<EntryRecord>,
    cutoff: Option<EntryId>,
) -> ContextView {
    let selection = select_active(&entries, cutoff);
    let head_index = selection
        .indices
        .first()
        .copied()
        .filter(|&index| entries[index].head.is_some());
    let mut selected = vec![false; entries.len()];
    for &index in &selection.indices {
        selected[index] = true;
    }
    let mut tail = Vec::with_capacity(selection.indices.len());
    let mut head_entry = None;
    for (index, entry) in entries.into_iter().enumerate() {
        if Some(index) == head_index {
            head_entry = Some(entry);
        } else if selected[index] {
            tail.push(entry);
        }
    }
    let mut active_entries = Vec::with_capacity(selection.indices.len());
    if let Some(head) = head_entry {
        active_entries.push(head);
    }
    active_entries.extend(tail);
    build_view(active_entries, selection.edits)
}

/// The active-entry positions and winning edits of one derivation.
struct Selection {
    /// Active entry indices: the head marker first, then non-head entries in
    /// transcript order.
    indices: Vec<usize>,
    /// The winning edit per active entry, aligned with `indices`.
    edits: Vec<Option<ContextEdit>>,
}

/// Rules 1-5: choose the active entries and the edit that applies to each.
fn select_active(entries: &[EntryRecord], cutoff: Option<EntryId>) -> Selection {
    let in_range = |entry: &EntryRecord| cutoff.is_none_or(|cutoff| entry.id <= cutoff);

    // Rule 1: the newest visible head marker at or before the cutoff.
    let head_index = entries
        .iter()
        .enumerate()
        .filter(|(_, entry)| entry.head.is_some() && in_range(entry))
        .max_by_key(|(_, entry)| entry.id)
        .map(|(index, _)| index);

    // Rules 2 and 3: from `H.head` (or transcript start) through the cutoff.
    let from = head_index.and_then(|index| entries[index].head);

    // Rule 4: the newest edit per target wins. Edits carried by older head
    // markers in the range count too (they are dropped from `active` below but
    // their edits are still in the range).
    let mut edits: BTreeMap<EntryId, &ContextEdit> = BTreeMap::new();
    for entry in entries {
        if !in_range(entry) || from.is_some_and(|from| entry.id < from) {
            continue;
        }
        for edit in entry.edits.iter().flatten() {
            edits.insert(edit.target, edit);
        }
    }

    // Rule 5: the head marker first, then the range's non-head entries.
    let mut indices = Vec::new();
    if let Some(index) = head_index {
        indices.push(index);
    }
    for (index, entry) in entries.iter().enumerate() {
        if in_range(entry) && from.is_none_or(|from| entry.id >= from) && entry.head.is_none() {
            indices.push(index);
        }
    }
    let edit_list = indices
        .iter()
        .map(|&index| edits.get(&entries[index].id).map(|edit| (*edit).clone()))
        .collect();
    Selection {
        indices,
        edits: edit_list,
    }
}

/// Rules 6-10 over the selected entries: shared entry storage plus the
/// lazily derived contribution index and ordered message list.
fn build_view(active_entries: Vec<EntryRecord>, edits: Vec<Option<ContextEdit>>) -> ContextView {
    // The head marker always leads when present, so the view's `head` is the
    // first active entry in that case.
    let head = active_entries
        .first()
        .filter(|entry| entry.head.is_some())
        .cloned();
    let active: Vec<&EntryRecord> = active_entries.iter().collect();
    let baseline = find_baseline(head.as_ref(), &active);

    let entries: Arc<[EntryRecord]> = Arc::from(active_entries);
    let edits: Arc<[Option<ContextEdit>]> = Arc::from(edits);
    let contributions = Contributions::new(Arc::clone(&entries), Arc::clone(&edits));
    let messages = Messages::new(entries.clone(), edits, baseline);

    ContextView {
        head,
        entries,
        contributions,
        messages,
    }
}

/// Rules 6-10: order tool results, then lead with the baseline.
///
/// Derived on first access by [`Messages`]; the flattened messages move into
/// `order_tool_results`, which consumes them instead of cloning each one.
fn derive_messages(
    entries: &[EntryRecord],
    edits: &[Option<ContextEdit>],
    baseline: &Baseline,
) -> Vec<Message> {
    let flat: Vec<Located> = entries
        .iter()
        .enumerate()
        .flat_map(|(index, entry)| {
            contribute(index, entry, edits.get(index).and_then(Option::as_ref)).into_iter()
        })
        .map(|(provenance, message)| Located {
            provenance: Some(provenance),
            message,
        })
        .collect();
    lead_with_system(order_tool_results(flat), baseline)
        .into_iter()
        .map(|located| located.message)
        .collect()
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
fn order_tool_results(messages: Vec<Located>) -> Vec<Located> {
    let mut slots: Vec<Option<Located>> = messages.into_iter().map(Some).collect();
    let mut ordered: Vec<Located> = Vec::with_capacity(slots.len());
    for index in 0..slots.len() {
        let Some(located) = slots[index].as_ref() else {
            continue;
        };
        if matches!(&located.message, Message::ToolResult(_)) {
            continue;
        }
        // Copy the call ids/names and timestamp before taking the message out:
        // the assistant itself moves into `ordered` without being cloned.
        let (calls, timestamp) = match &located.message {
            Message::Assistant(assistant) => (
                assistant
                    .content
                    .iter()
                    .filter_map(|block| match block {
                        ContentBlock::ToolCall(call) => Some((call.id.clone(), call.name.clone())),
                        _ => None,
                    })
                    .collect::<Vec<_>>(),
                assistant.timestamp,
            ),
            _ => (Vec::new(), 0),
        };
        let Some(message) = slots[index].take() else {
            continue;
        };
        ordered.push(message);
        if calls.is_empty() {
            continue;
        }
        // Results are found before the next assistant message; the first result
        // of a call id wins.
        let mut results: BTreeMap<&str, usize> = BTreeMap::new();
        for (candidate_index, candidate) in slots.iter().enumerate().skip(index + 1) {
            match candidate {
                Some(Located {
                    message: Message::Assistant(_),
                    ..
                }) => break,
                Some(Located {
                    message: Message::ToolResult(result),
                    ..
                }) => {
                    results
                        .entry(result.tool_call_id.as_str())
                        .or_insert(candidate_index);
                }
                _ => {}
            }
        }
        let targets: Vec<Option<usize>> = calls
            .iter()
            .map(|(id, _)| results.get(id.as_str()).copied())
            .collect();
        // A result is moved out of `slots` on its last use and cloned for the
        // earlier ones, so duplicate call ids keep the duplicates they had.
        let mut uses: BTreeMap<usize, usize> = BTreeMap::new();
        for target in targets.iter().flatten() {
            *uses.entry(*target).or_default() += 1;
        }
        for ((id, name), target) in calls.into_iter().zip(targets) {
            match target {
                Some(result_index) => {
                    let last = uses.get_mut(&result_index).is_some_and(|remaining| {
                        *remaining -= 1;
                        *remaining == 0
                    });
                    let found = if last {
                        slots[result_index].take()
                    } else {
                        slots[result_index].clone()
                    };
                    if let Some(found) = found {
                        ordered.push(found);
                    }
                }
                None => ordered.push(Located {
                    provenance: None,
                    message: missing_result(&id, &name, timestamp),
                }),
            }
        }
    }
    ordered
}

/// The synthesized error result of a call whose result is not in the range.
fn missing_result(tool_call_id: &str, tool_name: &str, timestamp: u64) -> Message {
    Message::ToolResult(ToolResultMessage {
        role: ToolResultRole::ToolResult,
        tool_call_id: tool_call_id.to_owned(),
        tool_name: tool_name.to_owned(),
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
///
/// The cheap role check first keeps the baseline scan from parsing every
/// non-system message; a message whose tag is not `system` can never
/// deserialize to [`Message::System`], so the two paths agree.
fn is_system_value(value: &Value) -> bool {
    if value.get("role").and_then(Value::as_str) != Some("system") {
        return false;
    }
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
