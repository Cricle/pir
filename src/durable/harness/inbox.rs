//! The built-in `pi.inbox` document and boundary selection.
//!
//! Ports upstream `harness/inbox.ts`. One `pi.inbox` document per conversation
//! holds the submissions that wait for a boundary, as an ordered list of tagged
//! items (spec §6):
//!
//! ```json
//! {"items": [
//!   {"id": 12, "mode": "steer", "content": "stop", "entry": {"kind": "app.author", "data": {"name": "Ada"}}},
//!   {"id": 13, "mode": "followUp", "content": "and then?"},
//!   {"id": 14, "mode": "write", "entry": {"kind": "app.note", "data": {"text": "..."}}}
//! ]}
//! ```
//!
//! Items are kept in ID order: ids are minted in ascending order, so appending
//! at the tail keeps the invariant. Selection is positional and deterministic —
//! `steering_mode`/`follow_up_mode` decide how many items of a mode a boundary
//! takes, and the selected items are removed by position afterwards, which
//! preserves the order of the retained items.
//!
//! # The boundary table (spec §6)
//!
//! | boundary | write | steer | follow-up |
//! |---|---|---|---|
//! | `postTools` | all | first/all by mode | none |
//! | `final` | all | first/all by mode | first/all by mode |
//!
//! A queued reset (`head: "self"`) turns a `postTools` boundary into `final`
//! behaviour: the run's context is about to be cut, so the follow-ups queued
//! behind the reset belong to the next run. Selected writes come first, in ID
//! order, then the selected user items, in ID order, so a user item queued
//! before a reset or summary lands after it in the new context.
//!
//! # Staleness
//!
//! A head write whose target is older than the start of the active range (the
//! newest head marker's `head`) is stale: placing it would bring back history
//! that head cut. `head: "self"` is never stale. Heads placed earlier in the
//! same boundary count: once a queued reset was placed, the active range starts
//! at the reset's new entry, so a summary targeting an older entry is stale
//! too. See [`is_stale`] and [`is_stale_entry`].
//!
//! # What this module does not do
//!
//! It performs no storage access. The facade reads the document, calls
//! [`boundary`], appends the entries, settles the submissions (including stale
//! writes as `unanswered`/`stale`) and then calls [`remove_selected`] on the
//! draft. [`withdraw_inputs`] is the inbox half of a conversation abort.

use std::sync::LazyLock;

use serde::de::Error as _;
use serde::ser::SerializeMap;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value;

use crate::durable::documents::{AnyDocDefinition, CommonDocDefinition};
use crate::durable::harness::settings::{QueueMode, Settings};
use crate::durable::ids::{EntryId, SubmissionId};
use crate::durable::types::{DocumentFork, JsonObject};

/// Persisted document kind of the inbox (spec §6).
pub const INBOX_KIND: &str = "pi.inbox";

/// Persisted document version of the inbox.
pub const INBOX_VERSION: i64 = 1;

/// Settlement reason of a queued item withdrawn by an abort.
pub const REASON_ABORTED: &str = "aborted";

/// Settlement reason of a write whose head target is older than the active
/// range.
pub const REASON_STALE: &str = "stale";

/// How an input item waits for a boundary.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum InputMode {
    /// Steering input: placed at a `postTools` or `final` boundary.
    Steer,
    /// Follow-up input: placed at a `final` boundary.
    FollowUp,
}

/// The entry an input is placed as, instead of `pi.user`.
///
/// Only `kind` and `data` are stored; the model message is still the input's
/// user message. Admission rejects a kind that is empty or starts with `pi.`
/// (see [`check_input_entry`]).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct InputEntry {
    /// Application-defined entry kind.
    pub kind: String,
    /// Payload the transcript shows.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

/// One queued submission in the inbox.
///
/// The wire shape matches upstream exactly: every item carries `id` and a
/// `mode` discriminator (`steer`, `followUp` or `write`); inputs carry
/// `content` and an optional `entry`, writes carry the `EntryDraft` as plain
/// JSON in `entry`. `mode: "write"` is an upstream field, so the enum cannot
/// use serde's internally tagged representation (the `Input` variant's `mode`
/// field would collide with the tag); [`Serialize`]/[`Deserialize`] are written
/// by hand to keep the exact upstream bytes.
#[derive(Clone, Debug, PartialEq)]
pub enum InboxItem {
    /// User input waiting for a boundary.
    Input {
        /// Submission identifier.
        id: SubmissionId,
        /// Whether the item waits for a `postTools` or only a `final` boundary.
        mode: InputMode,
        /// The user input, as given by the host.
        content: Value,
        /// The entry it is placed as; absent means `pi.user`.
        entry: Option<InputEntry>,
    },
    /// An entry write waiting for a boundary.
    Write {
        /// Submission identifier.
        id: SubmissionId,
        /// The `EntryDraft`, stored as plain JSON.
        entry: JsonObject,
    },
}

impl InboxItem {
    /// The submission this item queues.
    pub fn id(&self) -> SubmissionId {
        match self {
            Self::Input { id, .. } | Self::Write { id, .. } => *id,
        }
    }

    /// Whether this item is user input (steer or follow-up).
    pub fn is_user(&self) -> bool {
        matches!(self, Self::Input { .. })
    }

    /// Whether this item is a passive entry write.
    pub fn is_write(&self) -> bool {
        matches!(self, Self::Write { .. })
    }

    /// The input mode, for an input item.
    pub fn input_mode(&self) -> Option<InputMode> {
        match self {
            Self::Input { mode, .. } => Some(*mode),
            Self::Write { .. } => None,
        }
    }

    /// The item's entry draft, for a write item.
    pub fn write_entry(&self) -> Option<&JsonObject> {
        match self {
            Self::Write { entry, .. } => Some(entry),
            Self::Input { .. } => None,
        }
    }
}

/// Raw wire shape of one item, used only by the hand-written `Deserialize`.
#[derive(Deserialize)]
struct RawInboxItem {
    id: SubmissionId,
    mode: String,
    #[serde(default)]
    content: Option<Value>,
    #[serde(default)]
    entry: Option<Value>,
}

impl Serialize for InboxItem {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Input {
                id,
                mode,
                content,
                entry,
            } => {
                let mut map = serializer.serialize_map(None)?;
                map.serialize_entry("id", id)?;
                map.serialize_entry("mode", mode)?;
                map.serialize_entry("content", content)?;
                if let Some(entry) = entry {
                    map.serialize_entry("entry", entry)?;
                }
                map.end()
            }
            Self::Write { id, entry } => {
                let mut map = serializer.serialize_map(None)?;
                map.serialize_entry("id", id)?;
                map.serialize_entry("mode", "write")?;
                map.serialize_entry("entry", entry)?;
                map.end()
            }
        }
    }
}

impl<'de> Deserialize<'de> for InboxItem {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = RawInboxItem::deserialize(deserializer)?;
        match raw.mode.as_str() {
            "write" => match raw.entry {
                Some(Value::Object(entry)) => Ok(Self::Write { id: raw.id, entry }),
                Some(_) => Err(D::Error::custom(
                    "an inbox write item's entry must be a JSON object",
                )),
                None => Err(D::Error::missing_field("entry")),
            },
            "steer" | "followUp" => {
                let mode = if raw.mode == "steer" {
                    InputMode::Steer
                } else {
                    InputMode::FollowUp
                };
                let entry = raw
                    .entry
                    .map(serde_json::from_value::<InputEntry>)
                    .transpose()
                    .map_err(D::Error::custom)?;
                Ok(Self::Input {
                    id: raw.id,
                    mode,
                    content: raw.content.unwrap_or(Value::Null),
                    entry,
                })
            }
            other => Err(D::Error::unknown_variant(
                other,
                &["steer", "followUp", "write"],
            )),
        }
    }
}

/// The value of the `pi.inbox` document.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct InboxState {
    /// Waiting items, in ID order.
    #[serde(default)]
    pub items: Vec<InboxItem>,
}

impl InboxState {
    /// An empty inbox.
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether no submission waits.
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }
}

/// Initial value of a new `pi.inbox` incarnation: `{ items: [] }`.
fn inbox_initial() -> JsonObject {
    let mut value = JsonObject::new();
    value.insert("items".to_owned(), Value::Array(Vec::new()));
    value
}

/// `pi.inbox`'s checkpoint predicate: a complete base whenever `items` is
/// empty. A missing `items` key reads as empty (the initial value).
fn inbox_checkpoint_when(value: &JsonObject) -> bool {
    value
        .get("items")
        .and_then(Value::as_array)
        .is_none_or(Vec::is_empty)
}

/// The built-in `pi.inbox` definition (spec §6).
///
/// Kind `pi.inbox`, version 1, conversation scope, `history: "latest"`,
/// `fork: "initial"`, initial `{ items: [] }`, checkpoint whenever the inbox is
/// empty.
///
/// A `LazyLock` because the definition carries function pointers and building
/// it validates the version, which is not a `const` operation.
pub static INBOX_DEFINITION: LazyLock<AnyDocDefinition> = LazyLock::new(|| {
    AnyDocDefinition::define(
        CommonDocDefinition::conversation_latest(INBOX_KIND, INBOX_VERSION, DocumentFork::Initial)
            .expect("the pi.inbox definition version is valid"),
        inbox_initial,
        None,
        Some(inbox_checkpoint_when),
    )
});

/// Whether a draft is display-only: it has no `model`, `head` or `edits`
/// (present counts, even when empty), so it changes nothing the model sees and
/// needs no boundary.
pub fn is_display_only(entry: &JsonObject) -> bool {
    !entry.contains_key("model") && !entry.contains_key("head") && !entry.contains_key("edits")
}

/// Whether a reset (`head: "self"` write) is queued.
///
/// Entries appended before a reset leave the active range when it lands, so a
/// display-only write also queues behind a queued reset, and a `postTools`
/// boundary that selects the reset behaves as `final`.
pub fn has_queued_reset(state: &InboxState) -> bool {
    state.items.iter().any(|item| {
        item.write_entry()
            .is_some_and(|entry| entry.get("head").is_some_and(|head| head == "self"))
    })
}

/// Whether an entry draft's head target is older than the active range start.
///
/// A `head: "self"` write is never stale, and neither is a write with no head
/// (or a non-numeric head). The caller passes the newest head marker's `head`
/// as `active_range_start`; when there is no head marker, nothing is stale and
/// this is not called.
///
/// Heads placed earlier in the same boundary count: after placing a write with
/// a numeric `head`, the caller advances `active_range_start` to that target
/// (and after a reset, to the reset's new entry) before checking the next
/// selected write. That is how a summary queued behind a reset becomes stale
/// (spec §6).
pub fn is_stale_entry(entry: &JsonObject, active_range_start: EntryId) -> bool {
    match entry.get("head") {
        Some(Value::Number(target)) => target
            .as_i64()
            .is_some_and(|target| target < active_range_start.as_i64()),
        _ => false,
    }
}

/// Whether a queued write item is stale against the active range start.
///
/// Input items are never stale: queued user items are placed into whatever
/// context the boundary selects.
pub fn is_stale(item: &InboxItem, active_range_start: EntryId) -> bool {
    item.write_entry()
        .is_some_and(|entry| is_stale_entry(entry, active_range_start))
}

/// Reject an input entry whose kind is empty or reserved (`pi.` prefixed).
///
/// Upstream throws a `TypeError` during admission, before any write. The
/// harness calls this on a draft's `entry` before [`admit`](super::submissions::admit).
pub fn check_input_entry(entry: &InputEntry) -> Result<(), InvalidInputEntry> {
    if entry.kind.is_empty() || entry.kind.starts_with("pi.") {
        return Err(InvalidInputEntry {
            kind: entry.kind.clone(),
        });
    }
    Ok(())
}

/// An input entry whose kind admission must reject.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("An input entry's kind must be a non-empty string not starting with \"pi.\", got {kind:?}")]
pub struct InvalidInputEntry {
    /// The rejected kind.
    pub kind: String,
}

/// Which boundary is being applied.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Boundary {
    /// After a tool round: writes and steer, no follow-ups.
    PostTools,
    /// The end of a run: writes, steer and follow-ups.
    Final,
}

/// Which half of the queue an item belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SelectedKind {
    /// A passive entry write.
    Write,
    /// A steer or follow-up input.
    User,
}

/// One item a boundary selected, by its position in the inbox it read.
///
/// Positions are the index in [`InboxState::items`] at the moment [`boundary`]
/// ran; [`remove_selected`] removes exactly those positions. The id is carried
/// so the caller can settle the submission without re-reading the state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Selected {
    /// Position in the inbox that was read.
    pub index: usize,
    /// Submission the item queues.
    pub id: SubmissionId,
    /// Write or user item.
    pub kind: SelectedKind,
}

/// Select the items a boundary places, in placement order (spec §6).
///
/// Every queued write is selected (the caller still settles a stale one as
/// `unanswered`). Steer items are selected by `settings.steering_mode`; at a
/// `final` boundary — or a `postTools` boundary whose inbox holds a queued
/// reset — follow-ups are selected by `settings.follow_up_mode`. `OneAtATime`
/// takes the first item of a mode; `All` takes every item.
///
/// Writes come first in ID order, then every selected steer in ID order, then
/// every selected follow-up in ID order (upstream `take`). Selection is
/// positional: it never removes anything itself, so the caller can check
/// staleness against the ids it is about to append.
pub fn boundary(state: &InboxState, at: Boundary, settings: &Settings) -> Vec<Selected> {
    let final_boundary = at == Boundary::Final || has_queued_reset(state);

    let mut selected = Vec::new();
    for (index, item) in state.items.iter().enumerate() {
        if item.is_write() {
            selected.push(Selected {
                index,
                id: item.id(),
                kind: SelectedKind::Write,
            });
        }
    }

    let mut user_indexes = Vec::new();
    user_indexes.extend(pick_mode(state, InputMode::Steer, settings.steering_mode));
    if final_boundary {
        user_indexes.extend(pick_mode(
            state,
            InputMode::FollowUp,
            settings.follow_up_mode,
        ));
    }

    for index in user_indexes {
        selected.push(Selected {
            index,
            id: state.items[index].id(),
            kind: SelectedKind::User,
        });
    }
    selected
}

/// Positions of the items of `mode`, limited by `queue_mode`, in ID order.
fn pick_mode(state: &InboxState, mode: InputMode, queue_mode: QueueMode) -> Vec<usize> {
    let indexes: Vec<usize> = state
        .items
        .iter()
        .enumerate()
        .filter(|(_, item)| item.input_mode() == Some(mode))
        .map(|(index, _)| index)
        .collect();
    match queue_mode {
        QueueMode::All => indexes,
        QueueMode::OneAtATime => indexes.into_iter().take(1).collect(),
    }
}

/// Remove the selected items positionally, preserving the order of the rest.
///
/// `selected` must come from a [`boundary`] call over the same state. Removal
/// goes from the highest position down, so earlier positions stay valid and
/// retained items keep their relative order.
pub fn remove_selected(state: &mut InboxState, selected: &[Selected]) {
    let mut indexes: Vec<usize> = selected.iter().map(|item| item.index).collect();
    indexes.sort_unstable();
    indexes.dedup();
    for index in indexes.into_iter().rev() {
        if index < state.items.len() {
            state.items.remove(index);
        }
    }
}

/// Withdraw every queued steer/follow-up item; queued writes stay.
///
/// This is the inbox half of `Conversation.abort()` and abort cascades. The
/// returned ids are in ID order; the caller settles each `unanswered` with
/// [`REASON_ABORTED`] (or its own reason) and stages the settlement.
pub fn withdraw_inputs(state: &mut InboxState) -> Vec<SubmissionId> {
    let withdrawn = state
        .items
        .iter()
        .filter(|item| item.is_user())
        .map(InboxItem::id)
        .collect();
    state.items.retain(InboxItem::is_write);
    withdrawn
}
