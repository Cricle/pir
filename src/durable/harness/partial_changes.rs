//! What a streamed answer changed in its partial message between two partial
//! commits, so a commit costs the new text.
//!
//! Port of upstream `packages/durable/src/harness/partial-changes.ts`:
//!
//! * text and thinking deltas are collected per content block and committed as
//!   an [`AssignPlan::Append`], so a growing string is appended rather than
//!   reassigned whole;
//! * tool-call arguments are parsed incrementally from the raw
//!   `toolcall_delta` text ([`StreamingJsonMirror`]), and the commit applies
//!   those changes to the committed arguments in order — the arguments are
//!   left out of the message assignment — so the committed arguments are
//!   exactly the parsed text so far, without re-parsing the accumulated string
//!   every flush;
//! * a failed commit puts its tool-call changes back, so the next commit
//!   applies them first.

use std::collections::BTreeMap;

use serde_json::{Map, Value};

use crate::ai::api::partial_mirror::streaming_json::JsonPathSegment;
use crate::ai::api::partial_mirror::{JsonChange, StreamingJsonMirror};
use crate::ai::types::{AssistantMessage, AssistantMessageEvent};
use crate::durable::types::JsonObject;

use super::json::{assign_json_with_plan, AssignPlan, PlanKey};

/// A partial message as `pi.live.generation.message` holds it.
pub type PartialMessage = JsonObject;

/// What one partial commit writes besides the partial message itself; from
/// [`PartialChanges::take`].
#[derive(Debug)]
pub(crate) struct PartialCommit {
    /// The plan [`assign_json_with_plan`] writes the message with: text
    /// appends, and the arguments it leaves to [`PartialChanges::commit`].
    pub plan: AssignPlan,
    /// Each tracked tool call by content index: its argument changes since the
    /// last commit, in order, and its snapshot.
    pub tool_calls: BTreeMap<usize, ToolCallCommit>,
    /// The text/thinking deltas this commit consumed. Kept so a failed commit
    /// can put them back: a later flush reads the last COMMITTED message as
    /// its base, so dropped deltas would otherwise be lost forever.
    pub(crate) text: BTreeMap<usize, TextDelta>,
}

/// One tracked tool call's share of a [`PartialCommit`].
#[derive(Debug)]
pub(crate) struct ToolCallCommit {
    /// Argument changes since the last commit, in order.
    pub(crate) changes: Vec<JsonChange>,
    /// The arguments the partial message carried when the commit took it.
    pub(crate) snapshot: Value,
}

/// Which string a text event extends.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TextField {
    Text,
    Thinking,
}

impl TextField {
    fn key(self) -> &'static str {
        match self {
            Self::Text => "text",
            Self::Thinking => "thinking",
        }
    }
}

/// Text accumulated for one block since the last commit.
#[derive(Clone, Debug)]
pub(crate) struct TextDelta {
    field: TextField,
    text: String,
}

/// What a tracked tool call changed since the last commit, and whether any
/// commit took its changes yet.
struct ToolCallState {
    parser: StreamingJsonMirror,
    changes: Vec<JsonChange>,
    committed: bool,
}

/// Collects, from a response's events, what changed in its partial message
/// since the last commit.
///
/// See the module docs for the rules (upstream `PartialChanges`).
#[derive(Default)]
pub struct PartialChanges {
    text: BTreeMap<usize, TextDelta>,
    tool_calls: BTreeMap<usize, ToolCallState>,
    /// First block content per content index, captured on the block START
    /// events. A later flush reads the last COMMITTED message as its base, so
    /// a block that did not exist there yet must be seeded from the event's
    /// partial snapshot.
    block_seeds: BTreeMap<usize, Value>,
    /// A block's final content (text/thinking end, tool-call end). The provider
    /// may replace the accumulated text there; replacing the block keeps the
    /// committed base exact without serializing the whole message again.
    block_overrides: BTreeMap<usize, Value>,
    /// Whether `pi.live.generation.message` already holds a base this tracker
    /// can incrementally update. False until the first successful flush; a
    /// reset/missing committed message flips it back.
    has_base: bool,
}

impl PartialChanges {
    /// An empty tracker.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether the next commit needs a full message serialization because no
    /// committed base exists yet (first flush, or the document's message slot
    /// disappeared).
    pub(crate) fn needs_full_snapshot(&self) -> bool {
        !self.has_base
    }

    /// Record that the last commit landed in `pi.live.generation.message`.
    pub(crate) fn mark_base_committed(&mut self) {
        self.has_base = true;
    }

    /// The committed base disappeared (document reset by another writer);
    /// fall back to one full snapshot on the next flush.
    pub(crate) fn invalidate_base(&mut self) {
        self.has_base = false;
    }

    /// Apply the pending text deltas to a committed message base, creating
    /// blocks the base does not carry yet. O(new text): no full message walk
    /// and no JSON serialization.
    pub(crate) fn apply_pending_to_message(&self, message: &mut PartialMessage) {
        let content = message
            .entry("content".to_owned())
            .or_insert_with(|| Value::Array(Vec::new()));
        let Some(content) = content.as_array_mut() else {
            return;
        };
        for (index, block) in &self.block_seeds {
            ensure_block(content, *index, block.clone());
        }
        for (index, delta) in &self.text {
            let field = delta.field.key();
            let missing = content.get(*index).is_none_or(|block| !block.is_object());
            if missing {
                let mut block = JsonObject::new();
                block.insert("type".to_owned(), Value::String(field.to_owned()));
                block.insert(field.to_owned(), Value::String(String::new()));
                ensure_block(content, *index, Value::Object(block));
            }
            if let Some(Value::String(text)) = content
                .get_mut(*index)
                .and_then(Value::as_object_mut)
                .and_then(|block| block.get_mut(field))
            {
                text.push_str(&delta.text);
            }
        }
        for (index, block) in &self.block_overrides {
            if *index >= content.len() {
                content.resize(*index + 1, Value::Null);
            }
            content[*index] = block.clone();
        }
    }

    /// Record what `event` changed.
    pub fn observe(&mut self, event: &AssistantMessageEvent) {
        match event {
            AssistantMessageEvent::TextDelta {
                content_index,
                delta,
                ..
            } => self.push_text(*content_index, TextField::Text, delta),
            AssistantMessageEvent::ThinkingDelta {
                content_index,
                delta,
                ..
            } => self.push_text(*content_index, TextField::Thinking, delta),
            AssistantMessageEvent::TextEnd {
                content_index,
                partial,
                ..
            }
            | AssistantMessageEvent::ThinkingEnd {
                content_index,
                partial,
                ..
            } => {
                // Providers may replace the text when a block ends; the final
                // commit assigns what they ended with.
                self.text.remove(content_index);
                if let Some(block) = block_value(partial, *content_index) {
                    self.block_overrides.insert(*content_index, block);
                }
            }
            AssistantMessageEvent::TextStart {
                content_index,
                partial,
            }
            | AssistantMessageEvent::ThinkingStart {
                content_index,
                partial,
            } => {
                if let Some(block) = block_value(partial, *content_index) {
                    self.block_seeds.insert(*content_index, block);
                }
            }
            AssistantMessageEvent::ToolCallStart {
                content_index,
                partial,
            } => {
                if let Some(block) = block_value(partial, *content_index) {
                    self.block_seeds.insert(*content_index, block);
                }
                self.tool_calls.insert(
                    *content_index,
                    ToolCallState {
                        parser: StreamingJsonMirror::new(),
                        changes: Vec::new(),
                        committed: false,
                    },
                );
            }
            AssistantMessageEvent::ToolCallDelta {
                content_index,
                delta,
                ..
            } => {
                let mut drop_call = false;
                if let Some(call) = self.tool_calls.get_mut(content_index) {
                    for change in call.parser.append(delta) {
                        // Arguments are an object: any other root shows as no
                        // arguments, so the snapshot is committed instead.
                        if change_path(&change).is_empty() && !root_is_record(&change) {
                            drop_call = true;
                            break;
                        }
                        call.changes.push(change);
                    }
                }
                if drop_call {
                    self.tool_calls.remove(content_index);
                }
            }
            AssistantMessageEvent::ToolCallEnd {
                content_index,
                partial,
                ..
            } => {
                // The call's last commit assigns its final arguments.
                self.tool_calls.remove(content_index);
                if let Some(block) = block_value(partial, *content_index) {
                    self.block_overrides.insert(*content_index, block);
                }
            }
            AssistantMessageEvent::Start { .. }
            | AssistantMessageEvent::Done { .. }
            | AssistantMessageEvent::Error { .. } => {}
        }
    }

    fn push_text(&mut self, index: usize, field: TextField, delta: &str) {
        match self.text.get_mut(&index) {
            Some(pending) if pending.field == field => pending.text.push_str(delta),
            _ => {
                self.text.insert(
                    index,
                    TextDelta {
                        field,
                        text: delta.to_owned(),
                    },
                );
            }
        }
    }

    /// Take what changed since the last call, for a commit of `message`, a
    /// copy of the partial. Clears the arguments of tracked tool calls in
    /// `message`: the commit writes them from the changes. Call [`Self::restore`]
    /// if the commit fails.
    pub(crate) fn take(&mut self, message: &mut PartialMessage) -> PartialCommit {
        let mut blocks: BTreeMap<PlanKey, AssignPlan> = BTreeMap::new();
        let text = std::mem::take(&mut self.text);
        for (index, delta) in &text {
            let mut fields = BTreeMap::new();
            fields.insert(
                PlanKey::Key(delta.field.key().to_owned()),
                AssignPlan::Append {
                    text: delta.text.clone(),
                },
            );
            blocks.insert(PlanKey::Index(*index), AssignPlan::Fields(fields));
        }
        let mut tool_calls = BTreeMap::new();
        for (index, call) in &mut self.tool_calls {
            let Some(block) = message
                .get_mut("content")
                .and_then(Value::as_array_mut)
                .and_then(|content| content.get_mut(*index))
                .and_then(Value::as_object_mut)
            else {
                continue;
            };
            if block.get("type").and_then(Value::as_str) != Some("toolCall") {
                continue;
            }
            let snapshot = block
                .get("arguments")
                .cloned()
                .unwrap_or_else(|| Value::Object(Map::new()));
            block.insert("arguments".to_owned(), Value::Object(Map::new()));
            let mut fields = BTreeMap::new();
            fields.insert(PlanKey::Key("arguments".to_owned()), AssignPlan::Keep);
            blocks.insert(PlanKey::Index(*index), AssignPlan::Fields(fields));
            tool_calls.insert(
                *index,
                ToolCallCommit {
                    changes: std::mem::take(&mut call.changes),
                    snapshot,
                },
            );
        }
        let mut fields = BTreeMap::new();
        fields.insert(
            PlanKey::Key("content".to_owned()),
            AssignPlan::Fields(blocks),
        );
        PartialCommit {
            plan: AssignPlan::Fields(fields),
            tool_calls,
            text,
        }
    }

    /// Write `message` with what `commit` took into `generation["message"]`:
    /// the message leaf by leaf with the plan, then the argument changes. A
    /// tracked call whose block is new here gets its arguments from its
    /// changes, which then start at its first piece; when the committed
    /// message lost a call whose changes were already committed (another
    /// writer reset it), the call falls back to its snapshot from now on.
    pub(crate) fn commit(
        &mut self,
        generation: &mut JsonObject,
        mut message: PartialMessage,
        commit: &PartialCommit,
        committed_blocks: usize,
    ) {
        let mut apply: Vec<(usize, &[JsonChange])> = Vec::new();
        for (index, taken) in &commit.tool_calls {
            let Some(tracked) = self.tool_calls.get(index) else {
                continue;
            };
            let is_tool_call = message
                .get("content")
                .and_then(Value::as_array)
                .and_then(|content| content.get(*index))
                .and_then(|block| block.get("type"))
                .and_then(Value::as_str)
                == Some("toolCall");
            if *index >= committed_blocks && tracked.committed && is_tool_call {
                // The earlier changes are not in the committed message: commit
                // the snapshot and stop tracking the call.
                self.tool_calls.remove(index);
                if let Some(block) = message
                    .get_mut("content")
                    .and_then(Value::as_array_mut)
                    .and_then(|content| content.get_mut(*index))
                    .and_then(Value::as_object_mut)
                {
                    block.insert("arguments".to_owned(), taken.snapshot.clone());
                }
                continue;
            }
            if let Some(tracked) = self.tool_calls.get_mut(index) {
                tracked.committed = true;
            }
            apply.push((*index, &taken.changes));
        }
        assign_json_with_plan(
            generation,
            "message",
            Value::Object(message),
            Some(&commit.plan),
        );
        let Some(content) = generation
            .get_mut("message")
            .and_then(|message| message.get_mut("content"))
            .and_then(Value::as_array_mut)
        else {
            return;
        };
        for (index, changes) in apply {
            let Some(block) = content.get_mut(index) else {
                continue;
            };
            if block.get("type").and_then(Value::as_str) != Some("toolCall") {
                continue;
            }
            for change in changes {
                apply_change(block, change);
            }
        }
    }

    /// Put back what a failed commit took, so the next commit applies it first.
    pub(crate) fn restore(&mut self, commit: &PartialCommit) {
        for (index, taken) in &commit.tool_calls {
            let Some(tracked) = self.tool_calls.get_mut(index) else {
                continue;
            };
            let mut merged = taken.changes.clone();
            merged.append(&mut tracked.changes);
            tracked.changes = merged;
        }
        // Put the consumed text back IN FRONT of anything accumulated since
        // (normally nothing: the stream awaits this commit).
        for (index, taken) in &commit.text {
            match self.text.get_mut(index) {
                Some(pending) if pending.field == taken.field => {
                    let mut merged = taken.text.clone();
                    merged.push_str(&pending.text);
                    pending.text = merged;
                }
                _ => {
                    self.text.insert(*index, taken.clone());
                }
            }
        }
    }
}

/// Serialize one content block from a partial snapshot (small: START seeds are
/// empty, END overrides are produced once per block).
fn block_value(partial: &AssistantMessage, index: usize) -> Option<Value> {
    partial
        .content
        .get(index)
        .and_then(|block| serde_json::to_value(block).ok())
}

/// Ensure `content[index]` exists and is an object, seeding it when absent.
fn ensure_block(content: &mut Vec<Value>, index: usize, seed: Value) {
    if index >= content.len() {
        content.resize(index + 1, Value::Null);
    }
    if !content[index].is_object() {
        content[index] = seed;
    }
}

/// Apply one argument change to the committed arguments of `block`.
fn apply_change(block: &mut Value, change: &JsonChange) {
    let Some(arguments) = block
        .as_object_mut()
        .and_then(|block| block.get_mut("arguments"))
    else {
        return;
    };
    let Some((last, rest)) = change_path(change).split_last() else {
        // Only an object root is tracked (`observe` drops any other), and only
        // `set` starts it.
        if let JsonChange::Set { value, .. } = change {
            if value.is_object() {
                *arguments = value.clone();
            }
        }
        return;
    };
    let mut parent = arguments;
    for segment in rest {
        let next = match segment {
            JsonPathSegment::Key(key) => parent.get_mut(key),
            JsonPathSegment::Index(index) => parent.get_mut(*index),
        };
        let Some(next) = next else {
            return;
        };
        parent = next;
    }
    match change {
        JsonChange::Set { value, .. } => match last {
            JsonPathSegment::Key(key) => {
                if let Some(map) = parent.as_object_mut() {
                    map.insert(key.clone(), value.clone());
                }
            }
            JsonPathSegment::Index(index) => {
                if let Some(items) = parent.as_array_mut() {
                    if *index < items.len() {
                        items[*index] = value.clone();
                    } else if *index == items.len() {
                        items.push(value.clone());
                    }
                }
            }
        },
        JsonChange::Append { text, .. } => {
            let target = match last {
                JsonPathSegment::Key(key) => {
                    parent.as_object_mut().and_then(|map| map.get_mut(key))
                }
                JsonPathSegment::Index(index) => parent
                    .as_array_mut()
                    .and_then(|items| items.get_mut(*index)),
            };
            if let Some(Value::String(string)) = target {
                string.push_str(text);
            }
        }
    }
}

fn change_path(change: &JsonChange) -> &[JsonPathSegment] {
    match change {
        JsonChange::Set { path, .. } | JsonChange::Append { path, .. } => path,
    }
}

fn root_is_record(change: &JsonChange) -> bool {
    matches!(change, JsonChange::Set { value, .. } if value.is_object())
}
