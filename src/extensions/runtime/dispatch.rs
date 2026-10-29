use super::*;

use std::sync::Arc;

use crate::ai::types::{
    AssistantMessage, AssistantMessageEvent, AssistantRole, ContentBlock, DeferredHandle,
    DiagnosticInfo, StopReason, ThinkingLevel, Usage,
};

// ---------------------------------------------------------------------------
// `message_update` streaming wire
// ---------------------------------------------------------------------------

/// Internal wire key carrying the AUTHORITATIVE content block of one per-delta
/// event (the partial's block at `content_index`). It is how the JS-side
/// accumulator stays exact without ever receiving the cumulative snapshot:
/// see [`MessageUpdateEventWire`]. Deleted by the JS read-only entry before any
/// handler runs (the accumulator consumes it and `delete`s it), so
/// `assistantMessageEvent` keeps exactly the reference keys.
const MESSAGE_UPDATE_BLOCK_KEY: &str = "block";

/// Internal wire key carrying the O(delta) patch of one tool-call block's
/// `arguments` object (`{"key", "tail"}`: `arguments[key]` gained `tail`). It
/// replaces the reply-sized `block` helper for the common "one string argument
/// grows" case; see [`tool_argument_patch`]. Deleted by the JS entry (the
/// accumulator applies it and `delete`s it) exactly like `block`.
const MESSAGE_UPDATE_PATCH_KEY: &str = "patch";

/// Internal top-level wire key marking a DELTA line: the per-delta view
/// (`message` without the accumulated `content`, the raw event without
/// `partial`, plus the internal helpers) rather than the reference payload. It
/// is how the JS read-only entry tells the two routes apart
/// ([`message_update_stream_json`] classifies them on the host). Deleted before
/// any handler runs, so the payload keeps exactly the reference keys.
const MESSAGE_UPDATE_DELTA_KEY: &str = "__pirDelta";

/// Internal top-level wire key carrying the id of the stream the event belongs
/// to (one per assistant-message stream, assigned by
/// [`crate::agent::agent_loop::MessageUpdateDispatcher`]). The JS side keys its
/// per-stream live message on it, so two interleaved streams sharing one
/// runtime (subagent children reuse the parent handle) cannot clobber each
/// other's accumulator. Deleted before any handler runs.
const MESSAGE_UPDATE_STREAM_KEY: &str = "__pirStream";

/// The `message` value the reference construction produces for an event that
/// carries no cumulative snapshot (its `{"role": "assistant"}` fallback).
const MESSAGE_UPDATE_ROLE_ONLY: &str = r#"{"role":"assistant"}"#;

/// The shared empty `content` seed of the per-delta wire (the JS accumulator
/// owns the array; each delta appends to it).
const EMPTY_CONTENT: &[ContentBlock] = &[];

/// Which wire one `message_update` dispatch travels as.
///
/// The classification is HOST-side (the JS side never asks the host for a
/// snapshot): the host decides once, when it serializes the wire, and the wire
/// says which route it took via [`MESSAGE_UPDATE_DELTA_KEY`]. The two sets must
/// stay exactly in sync with the per-delta type set the JS accumulator knows
/// (`prelude.js.txt`, `applyMessageUpdateDelta`): a variant the JS side cannot
/// apply must classify as [`Self::Reference`] — the full reference payload —
/// never as a delta line the accumulator would silently fail on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageUpdateStreamShape {
    /// A per-delta line the JS accumulator can apply exactly: the message's
    /// O(1) scalars plus the delta itself, with the authoritative `block` (or
    /// the O(delta) `patch`) helper for every event whose block the JS side
    /// cannot derive from the delta alone.
    Delta,
    /// The reference payload
    /// ([`crate::agent::agent_loop::message_update_payload`]): the full
    /// `message` value. Terminal events (`done` / `error`) have no `partial`
    /// and no derivable content, so they are the only members today; a future
    /// non-terminal shape the JS accumulator does not know joins them rather
    /// than drifting.
    Reference,
}

/// Classify one event; see [`MessageUpdateStreamShape`]. The match is
/// exhaustive on purpose — a new event variant must be routed explicitly (and
/// the JS accumulator taught it) before it can ship.
pub fn message_update_stream_shape(event: &AssistantMessageEvent) -> MessageUpdateStreamShape {
    match event {
        AssistantMessageEvent::Start { .. }
        | AssistantMessageEvent::TextStart { .. }
        | AssistantMessageEvent::TextDelta { .. }
        | AssistantMessageEvent::TextEnd { .. }
        | AssistantMessageEvent::ThinkingStart { .. }
        | AssistantMessageEvent::ThinkingDelta { .. }
        | AssistantMessageEvent::ThinkingEnd { .. }
        | AssistantMessageEvent::ToolCallStart { .. }
        | AssistantMessageEvent::ToolCallDelta { .. }
        | AssistantMessageEvent::ToolCallEnd { .. } => MessageUpdateStreamShape::Delta,
        AssistantMessageEvent::Done { .. } | AssistantMessageEvent::Error { .. } => {
            MessageUpdateStreamShape::Reference
        }
    }
}

/// Wire form of one `message_update` dispatch for the read-only streaming
/// path (see [`ExtensionRuntime::emit_read_only`]).
///
/// The wire carries the DELTA, never the cumulative snapshot: the object holds
/// the reference keys in the reference order (`type`, `message`,
/// `assistantMessageEvent`), but
///
/// - `assistantMessageEvent` is the raw event without its `partial` (plus the
///   internal `block` helper, deleted by the JS entry), and
/// - `message` is the cumulative message MINUS its `content` — every other
///   field is O(1)-sized and rides its value verbatim, so the JS side can keep
///   a live message (upstream hands the live object to a handler) without any
///   per-delta snapshot: the JS accumulator appends the deltas to `content`
///   and applies the scalars, and the read-only entry aliases
///   `assistantMessageEvent.partial` to that same live object.
///
/// Cost: O(delta) per dispatch. The previous construction serialized the whole
/// accumulated reply once per delta (O(reply) per delta, O(reply²) per reply)
/// even when no handler looked at it — the cost this wire removes.
///
/// Terminal events (`done` / `error`) carry no `partial` and no derivable
/// content, so their wire IS the reference construction
/// ([`crate::agent::agent_loop::message_update_payload`]) plus the internal
/// stream key: one pass, once per stream, and byte-identical by construction.
///
/// The wire is built on the PROVIDER-STREAM thread (the agent loop's enqueue),
/// before the event crosses into the extension mailbox: the event — and with it
/// its `partial` `Arc` — is dropped the moment the wire exists. That is what
/// keeps the provider parser's copy-on-write accumulator unique (zero copies
/// per delta): a single retained `Arc` across the next parse would force one
/// whole-reply deep copy per delta (the ~630 MB of churn this lane removed).
pub fn message_update_stream_json(
    event: &AssistantMessageEvent,
    stream: u64,
    tool_arguments: &mut crate::agent::agent_loop::ToolArgumentBaselines,
) -> String {
    match message_update_stream_shape(event) {
        MessageUpdateStreamShape::Reference => {
            // The reference payload with the internal stream key appended
            // (key order: the reference keys stay exactly as the reference
            // construction emitted them; the marker rides last and the JS
            // entry deletes it before any handler runs).
            let payload = crate::agent::agent_loop::message_update_payload(event);
            let mut payload = match payload {
                JsonValue::Object(map) => map,
                other => return serde_json::to_string(&other).unwrap_or_else(|_| "{}".to_owned()),
            };
            payload.insert(
                MESSAGE_UPDATE_STREAM_KEY.to_string(),
                JsonValue::from(stream),
            );
            serde_json::to_string(&JsonValue::Object(payload)).unwrap_or_else(|_| "{}".to_owned())
        }
        MessageUpdateStreamShape::Delta => {
            if let AssistantMessageEvent::ToolCallDelta {
                content_index,
                partial,
                ..
            } = event
            {
                let index = *content_index;
                // The patch needs the PREVIOUS arguments of this content index:
                // take the baseline (moved out, so the new value can be stored
                // without a second live copy) and diff it against the partial's
                // value (O(args) comparison, no allocation).
                let previous = tool_arguments.take(index);
                let current = block_arguments(partial, index);
                let patch = match (previous.as_ref(), current) {
                    (Some(previous), Some(current)) => tool_argument_patch(previous, current),
                    _ => None,
                };
                let wire = serde_json::to_string(&MessageUpdateStreamWire {
                    event,
                    stream,
                    patch,
                })
                .unwrap_or_else(|_| "{}".to_owned());
                // The baseline is refreshed AFTER the wire is built (the patch
                // borrows it): the JS side now holds the new value, which is
                // what the next delta's patch is computed against.
                match current {
                    Some(current) => tool_arguments.put(index, current.clone()),
                    None => tool_arguments.forget(index),
                }
                return wire;
            }
            let wire = serde_json::to_string(&MessageUpdateStreamWire {
                event,
                stream,
                patch: None,
            })
            .unwrap_or_else(|_| "{}".to_owned());
            // Every other delta line ships the block whole (or none): the JS
            // accumulator's block is the shipped value from here on, so a
            // stale argument baseline for that index must not survive.
            match event {
                AssistantMessageEvent::Start { .. } => tool_arguments.clear(),
                AssistantMessageEvent::ToolCallStart {
                    content_index,
                    partial,
                } => match block_arguments(partial, *content_index) {
                    Some(current) => tool_arguments.put(*content_index, current.clone()),
                    None => tool_arguments.forget(*content_index),
                },
                AssistantMessageEvent::TextStart { content_index, .. }
                | AssistantMessageEvent::ThinkingStart { content_index, .. }
                | AssistantMessageEvent::TextEnd { content_index, .. }
                | AssistantMessageEvent::ThinkingEnd { content_index, .. }
                | AssistantMessageEvent::ToolCallEnd { content_index, .. } => {
                    tool_arguments.forget(*content_index)
                }
                AssistantMessageEvent::TextDelta { .. }
                | AssistantMessageEvent::ThinkingDelta { .. }
                | AssistantMessageEvent::ToolCallDelta { .. }
                | AssistantMessageEvent::Done { .. }
                | AssistantMessageEvent::Error { .. } => {}
            }
            wire
        }
    }
}

/// The parsed `arguments` of the tool-call block at `index` (the value the JS
/// accumulator holds for that block), when the partial carries one there.
fn block_arguments(
    partial: &AssistantMessage,
    index: usize,
) -> Option<&std::collections::BTreeMap<String, JsonValue>> {
    match partial.content.get(index) {
        Some(ContentBlock::ToolCall(call)) => Some(&call.arguments),
        _ => None,
    }
}

/// One O(delta) patch of a tool-call block's `arguments` object: the value of
/// `key` gained exactly the `tail` suffix, and no other key changed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ToolArgumentPatch<'a> {
    pub key: &'a str,
    pub tail: &'a str,
}

/// The parsed `arguments` of one tool-call block on two consecutive deltas:
/// see [`ToolArgumentPatch`]. `previous` is the value the JS side already holds,
/// `current` the one the partial now carries. Every change that is not exactly
/// "one string value gained a suffix, everything else deep-equal" returns
/// `None` (the caller ships the whole block instead), so a patch can never
/// misdescribe the block.
///
/// Cost: O(args) comparison and no allocation; the two sides are compared
/// structurally (map entries, `Value` equality), never as raw JSON text, so a
/// fragment that only changes the escaping of a value still compares equal.
pub fn tool_argument_patch<'a>(
    previous: &'a std::collections::BTreeMap<String, JsonValue>,
    current: &'a std::collections::BTreeMap<String, JsonValue>,
) -> Option<ToolArgumentPatch<'a>> {
    let mut previous_entries = previous.iter();
    let mut grew: Option<ToolArgumentPatch<'a>> = None;
    for (key, current_value) in current {
        // Lockstep iteration over the two sorted maps pins the key SETS equal
        // (an added or removed key is a change the patch cannot express).
        let (previous_key, previous_value) = previous_entries.next()?;
        if previous_key != key {
            return None;
        }
        if previous_value == current_value {
            continue;
        }
        // The one changed key must be a string the new value extends.
        let previous_text = previous_value.as_str()?;
        let current_text = current_value.as_str()?;
        let tail = current_text.strip_prefix(previous_text)?;
        if grew.is_some() {
            return None;
        }
        grew = Some(ToolArgumentPatch {
            key: key.as_str(),
            tail,
        });
    }
    if previous_entries.next().is_some() {
        return None;
    }
    grew
}

/// The authoritative cumulative snapshot of one `message_update` event: the
/// exact value the reference construction puts under `message`
/// ([`crate::agent::agent_loop::message_update_payload`], whose `message` slot
/// is `to_value(event)["partial"]`).
///
/// NOT part of the streaming path: the per-delta wire never materializes it
/// (that was the O(reply)-per-delta cost this lane removed), and the JS
/// read-only entry no longer has a host function to ask for one — a shape the
/// host cannot express as a delta line travels as the full reference payload
/// instead ([`MessageUpdateStreamShape`]). Kept as the reference serializer the
/// wire tests compare against (and re-exported by
/// [`crate::extensions::runtime`]).
pub fn message_update_snapshot_json(event: &AssistantMessageEvent) -> String {
    match assistant_event_partial_arc(event) {
        // Spelled through the runtime re-export (the wire removed the host
        // function that used to be the only caller, and `mod.rs` owns that
        // re-export): this keeps the re-export live for the wire tests.
        Some(partial) => {
            crate::extensions::runtime::message_update_snapshot_json_for_partial(&partial)
        }
        None => MESSAGE_UPDATE_ROLE_ONLY.to_owned(),
    }
}

/// Serialize one cumulative snapshot (`Arc<AssistantMessage>` serializes
/// transparently, so this is byte for byte the reference's `message` slot).
/// The reference serializer behind [`message_update_snapshot_json`].
pub(crate) fn message_update_snapshot_json_for_partial(partial: &AssistantMessage) -> String {
    serde_json::to_string(partial).unwrap_or_else(|_| MESSAGE_UPDATE_ROLE_ONLY.to_owned())
}

/// The in-flight assistant snapshot carried by an event, as the SAME `Arc` the
/// event holds (cloning one is an O(1) refcount bump; the snapshot registry
/// must never deep-copy the reply).
fn assistant_event_partial_arc(event: &AssistantMessageEvent) -> Option<Arc<AssistantMessage>> {
    match event {
        AssistantMessageEvent::Start { partial }
        | AssistantMessageEvent::TextStart { partial, .. }
        | AssistantMessageEvent::TextDelta { partial, .. }
        | AssistantMessageEvent::TextEnd { partial, .. }
        | AssistantMessageEvent::ThinkingStart { partial, .. }
        | AssistantMessageEvent::ThinkingDelta { partial, .. }
        | AssistantMessageEvent::ThinkingEnd { partial, .. }
        | AssistantMessageEvent::ToolCallStart { partial, .. }
        | AssistantMessageEvent::ToolCallDelta { partial, .. }
        | AssistantMessageEvent::ToolCallEnd { partial, .. } => Some(partial.clone()),
        AssistantMessageEvent::Done { .. } | AssistantMessageEvent::Error { .. } => None,
    }
}

/// `{type: "message_update", message, assistantMessageEvent, __pirDelta,
/// __pirStream}` — the per-delta streaming wire; see
/// [`message_update_stream_json`].
struct MessageUpdateStreamWire<'a> {
    event: &'a AssistantMessageEvent,
    stream: u64,
    patch: Option<ToolArgumentPatch<'a>>,
}

impl serde::Serialize for MessageUpdateStreamWire<'_> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        let mut map = serializer.serialize_map(Some(5))?;
        map.serialize_entry("type", "message_update")?;
        // The stream's `start` event carries the initial partial's content
        // (nothing before it exists to rebuild it from); every other per-delta
        // event ships the empty seed the JS accumulator appends to.
        let content = match self.event {
            AssistantMessageEvent::Start { partial } => partial.content.as_slice(),
            _ => EMPTY_CONTENT,
        };
        let partial = match self.event {
            AssistantMessageEvent::Start { partial }
            | AssistantMessageEvent::TextStart { partial, .. }
            | AssistantMessageEvent::TextDelta { partial, .. }
            | AssistantMessageEvent::TextEnd { partial, .. }
            | AssistantMessageEvent::ThinkingStart { partial, .. }
            | AssistantMessageEvent::ThinkingDelta { partial, .. }
            | AssistantMessageEvent::ThinkingEnd { partial, .. }
            | AssistantMessageEvent::ToolCallStart { partial, .. }
            | AssistantMessageEvent::ToolCallDelta { partial, .. }
            | AssistantMessageEvent::ToolCallEnd { partial, .. } => partial,
            AssistantMessageEvent::Done { .. } | AssistantMessageEvent::Error { .. } => {
                unreachable!("terminal events serialize through the reference construction")
            }
        };
        map.serialize_entry("message", &MessageUpdateMessageWire::new(partial, content))?;
        map.serialize_entry(
            "assistantMessageEvent",
            &MessageUpdateEventWire {
                event: self.event,
                patch: self.patch,
            },
        )?;
        // The internal markers ride LAST: the JS read-only entry deletes them
        // (and the `block`/`patch` helpers) before any handler runs, so the
        // payload a handler sees keeps exactly the reference keys and order.
        map.serialize_entry(MESSAGE_UPDATE_DELTA_KEY, &true)?;
        map.serialize_entry(MESSAGE_UPDATE_STREAM_KEY, &self.stream)?;
        map.end()
    }
}

/// The live-message view the wire carries: every field of the cumulative
/// assistant message with its value verbatim EXCEPT `content`, which is the
/// array the JS accumulator owns (the seed `[]`, or the initial partial's
/// content on the stream's `start` event).
///
/// Field order and `skip_serializing_if` mirror the `AssistantMessage` derive
/// one for one (that is what makes the live object the JS side builds
/// key-for-key equal to the reference `message`); the drift test
/// `message_update_live_object_matches_the_reference_at_every_delta` fails if a
/// field is added upstream without being added here.
#[derive(serde::Serialize)]
struct MessageUpdateMessageWire<'a> {
    role: &'a AssistantRole,
    content: &'a [ContentBlock],
    api: &'a crate::ai::types::ApiId,
    provider: &'a crate::ai::types::ProviderId,
    model: &'a str,
    #[serde(rename = "responseModel", skip_serializing_if = "Option::is_none")]
    response_model: Option<&'a str>,
    #[serde(rename = "responseId", skip_serializing_if = "Option::is_none")]
    response_id: Option<&'a str>,
    #[serde(skip_serializing_if = "slice_is_empty")]
    diagnostics: &'a [DiagnosticInfo],
    usage: &'a Usage,
    #[serde(rename = "stopReason")]
    stop_reason: StopReason,
    #[serde(skip_serializing_if = "Option::is_none")]
    deferred: Option<&'a DeferredHandle>,
    #[serde(rename = "errorMessage", skip_serializing_if = "Option::is_none")]
    error_message: Option<&'a str>,
    #[serde(rename = "rawStopReason", skip_serializing_if = "Option::is_none")]
    raw_stop_reason: Option<&'a str>,
    #[serde(rename = "endTurn", skip_serializing_if = "Option::is_none")]
    end_turn: Option<bool>,
    #[serde(
        rename = "providerThinkingLevel",
        skip_serializing_if = "Option::is_none"
    )]
    provider_thinking_level: Option<&'a str>,
    #[serde(rename = "thinkingLevel", skip_serializing_if = "Option::is_none")]
    thinking_level: Option<&'a ThinkingLevel>,
    #[serde(rename = "durationMs", skip_serializing_if = "Option::is_none")]
    duration_ms: Option<u64>,
    timestamp: u64,
}

impl<'a> MessageUpdateMessageWire<'a> {
    fn new(partial: &'a AssistantMessage, content: &'a [ContentBlock]) -> Self {
        Self {
            role: &partial.role,
            content,
            api: &partial.api,
            provider: &partial.provider,
            model: partial.model.as_str(),
            response_model: partial.response_model.as_deref(),
            response_id: partial.response_id.as_deref(),
            diagnostics: &partial.diagnostics,
            usage: &partial.usage,
            stop_reason: partial.stop_reason,
            deferred: partial.deferred.as_ref(),
            error_message: partial.error_message.as_deref(),
            raw_stop_reason: partial.raw_stop_reason.as_deref(),
            end_turn: partial.end_turn,
            provider_thinking_level: partial.provider_thinking_level.as_deref(),
            thinking_level: partial.thinking_level.as_ref(),
            duration_ms: partial.duration_ms,
            timestamp: partial.timestamp,
        }
    }
}

/// `skip_serializing_if` predicate for the borrowed diagnostics slice
/// (`std::slice::is_empty` takes `&[T]`; serde passes `&&[T]`).
fn slice_is_empty<T>(value: &&[T]) -> bool {
    value.is_empty()
}

/// The per-delta `assistantMessageEvent`: the raw event's own fields in the
/// reference order, WITHOUT the cumulative `partial` (the JS entry aliases
/// `partial` to its live message object) and with the internal `block` helper
/// the accumulator needs: the authoritative block at `content_index`, shipped
/// when the JS side cannot derive it from the delta.
///
/// `TextStart`/`ThinkingStart`/`ToolCallStart` and every `*_end` ship it (a
/// block is created/destroyed there and the partial is the only authority for
/// its shape); a `ToolCallDelta` ships it (or, for the common "one string
/// argument grew" case, the O(delta) `patch` instead — the provider MIRRORS
/// PARSED JSON arguments into the block, so the raw fragment is not the block);
/// a `TextDelta`/`ThinkingDelta` ships it only when the delta IS the block's
/// whole content — the provider opened the block with this very delta and the
/// JS side has no `*_start` to create it from (`coding_agent/json.rs` hoists
/// the same kind of derived data for the session wire).
struct MessageUpdateEventWire<'a> {
    event: &'a AssistantMessageEvent,
    patch: Option<ToolArgumentPatch<'a>>,
}

impl serde::Serialize for MessageUpdateEventWire<'_> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        let mut map = serializer.serialize_map(None)?;
        match self.event {
            AssistantMessageEvent::Start { .. } => {
                map.serialize_entry("type", "start")?;
            }
            AssistantMessageEvent::TextStart {
                content_index,
                partial,
            } => {
                map.serialize_entry("type", "text_start")?;
                map.serialize_entry("content_index", content_index)?;
                serialize_block(&mut map, partial, *content_index)?;
            }
            AssistantMessageEvent::TextDelta {
                content_index,
                delta,
                partial,
            } => {
                map.serialize_entry("type", "text_delta")?;
                map.serialize_entry("content_index", content_index)?;
                map.serialize_entry("delta", delta)?;
                if text_of(partial.content.get(*content_index)) == Some(delta.as_str()) {
                    serialize_block(&mut map, partial, *content_index)?;
                }
            }
            AssistantMessageEvent::TextEnd {
                content_index,
                content,
                partial,
            } => {
                map.serialize_entry("type", "text_end")?;
                map.serialize_entry("content_index", content_index)?;
                map.serialize_entry("content", content)?;
                serialize_block(&mut map, partial, *content_index)?;
            }
            AssistantMessageEvent::ThinkingStart {
                content_index,
                partial,
            } => {
                map.serialize_entry("type", "thinking_start")?;
                map.serialize_entry("content_index", content_index)?;
                serialize_block(&mut map, partial, *content_index)?;
            }
            AssistantMessageEvent::ThinkingDelta {
                content_index,
                delta,
                partial,
            } => {
                map.serialize_entry("type", "thinking_delta")?;
                map.serialize_entry("content_index", content_index)?;
                map.serialize_entry("delta", delta)?;
                if thinking_of(partial.content.get(*content_index)) == Some(delta.as_str()) {
                    serialize_block(&mut map, partial, *content_index)?;
                }
            }
            AssistantMessageEvent::ThinkingEnd {
                content_index,
                content,
                partial,
            } => {
                map.serialize_entry("type", "thinking_end")?;
                map.serialize_entry("content_index", content_index)?;
                map.serialize_entry("content", content)?;
                serialize_block(&mut map, partial, *content_index)?;
            }
            AssistantMessageEvent::ToolCallStart {
                content_index,
                partial,
            } => {
                map.serialize_entry("type", "toolcall_start")?;
                map.serialize_entry("content_index", content_index)?;
                serialize_block(&mut map, partial, *content_index)?;
            }
            AssistantMessageEvent::ToolCallDelta {
                content_index,
                delta,
                partial,
            } => {
                map.serialize_entry("type", "toolcall_delta")?;
                map.serialize_entry("content_index", content_index)?;
                map.serialize_entry("delta", delta)?;
                // The authoritative value of this delta is either the O(delta)
                // patch (one string argument gained `tail`) or the whole block
                // (the raw fragment is not the block: the provider mirrors the
                // PARSED in-flight JSON). See `tool_argument_patch`.
                match self.patch {
                    Some(patch) => map.serialize_entry(
                        MESSAGE_UPDATE_PATCH_KEY,
                        &ToolArgumentPatchWire {
                            key: patch.key,
                            tail: patch.tail,
                        },
                    )?,
                    None => serialize_block(&mut map, partial, *content_index)?,
                }
            }
            AssistantMessageEvent::ToolCallEnd {
                content_index,
                tool_call,
                partial,
            } => {
                map.serialize_entry("type", "toolcall_end")?;
                map.serialize_entry("content_index", content_index)?;
                map.serialize_entry("tool_call", tool_call)?;
                serialize_block(&mut map, partial, *content_index)?;
            }
            AssistantMessageEvent::Done { .. } | AssistantMessageEvent::Error { .. } => {
                unreachable!("terminal events serialize through the reference construction")
            }
        }
        map.end()
    }
}

/// Write the `block` helper when the partial carries a block at `index`.
fn serialize_block<S: serde::ser::SerializeMap>(
    map: &mut S,
    partial: &AssistantMessage,
    index: usize,
) -> Result<(), S::Error> {
    if let Some(block) = partial.content.get(index) {
        map.serialize_entry(MESSAGE_UPDATE_BLOCK_KEY, block)?;
    }
    Ok(())
}

/// The `patch` helper of one `toolcall_delta`: `arguments[key]` gained `tail`.
#[derive(serde::Serialize)]
struct ToolArgumentPatchWire<'a> {
    key: &'a str,
    tail: &'a str,
}

/// `TextContent::text` of a block, when it is a text block.
fn text_of(block: Option<&ContentBlock>) -> Option<&str> {
    match block {
        Some(ContentBlock::Text(text)) => Some(text.text.as_str()),
        _ => None,
    }
}

/// `ThinkingContent::thinking` of a block, when it is a thinking block.
fn thinking_of(block: Option<&ContentBlock>) -> Option<&str> {
    match block {
        Some(ContentBlock::Thinking(thinking)) => Some(thinking.thinking.as_str()),
        _ => None,
    }
}

impl ExtensionRuntime {
    /// Dispatch `event` through the reply-free JS entry
    /// (`__pir_dispatchEventReadOnly`).
    ///
    /// The JS side runs the exact same handler loop as [`Self::emit`]
    /// (`dispatchEventToHandlers`), but builds no `{outcome, payload, errors}`
    /// reply. That is the whole contract: this entry is for events whose
    /// handler return value the host already ignores (`message_update`, and
    /// the informational `session_tree`).
    ///
    /// For a payload that can be a deep tree the difference is stack safety:
    /// the normal entry `JSON.stringify`s the (possibly mutated) payload, and
    /// this side then re-parses, clones and drops that reply — `serde_json`
    /// recurses one stack frame per nesting level on all three. A
    /// `session_tree` reply echo of a long linear session overran a 2 MiB
    /// thread even with the payload itself built iteratively. With no reply
    /// there is no deep `Value` on this path at all: the caller hands over
    /// pre-rendered JSON text (`json_depth::write_value_iteratively` /
    /// `TreeNode::forest_to_json_iterative`). For the per-delta
    /// `message_update` wire, `payload_json` is O(delta): the caller
    /// ([`message_update_stream_json`]) serialized it on the provider-stream
    /// thread before the event was dropped, so neither this future nor the
    /// extension mailbox ever holds the event's `partial` `Arc`, and the
    /// provider parser's copy-on-write accumulator stays unique (zero copies
    /// per delta — a retained `Arc` would force one whole-reply deep copy per
    /// delta, the ~630 MB churn this lane removed).
    ///
    /// Handler semantics are [`Self::emit`]'s: same registration order, same
    /// payload object, same per-handler error isolation, same post-dispatch
    /// registry publication. The payload reaches every handler exactly once;
    /// only the reply is gone.
    ///
    /// Abandonment semantics are also [`Self::emit`]'s: the dispatch's epoch
    /// is allocated by the CALLER ([`ExtensionHandle::emit_read_only`], the
    /// same shared counter the reply-carrying `emit` path uses) and handed
    /// here, so a caller-side timeout can mark it abandoned even when the
    /// actor never observes its own deadline. The dispatch runs inside
    /// [`with_dispatch_epoch`], which the JS entry passes to
    /// `dispatchEventToHandlers`. A handler that outlives this future's
    /// deadline keeps tagging its `pi.*` / `ctx.ui.*` side effects with the
    /// abandoned epoch, so a late continuation is dropped instead of being
    /// applied to a later run.
    pub(crate) async fn emit_read_only(
        &self,
        event: &str,
        payload_json: String,
        epoch: u64,
    ) -> anyhow::Result<()> {
        let call_timeout = self.call_timeout();
        let token_deadline = self.arm_deadline_for(call_timeout);
        // Keep the epoch on the thread for the WHOLE dispatch, recovery
        // included: `recover_after_interrupt` drains pending jobs and may
        // resume a handler continuation, whose side effects must still be
        // tagged with this epoch and dropped once it is abandoned.
        let result = with_dispatch_epoch(epoch, async {
            let result = tokio::time::timeout(
                call_timeout,
                self.invoke_str3(
                    "__pir_dispatchEventReadOnly",
                    event.to_string(),
                    payload_json,
                    epoch.to_string(),
                ),
            )
            .await;
            if result.is_err() {
                // Actor-side deadline elapsed: this dispatch is abandoned even
                // if the caller's reply channel already timed out. Mark the
                // epoch BEFORE recovery so a continuation that resumes below
                // drops its side effects (`exitDispatch` clears the marker
                // when the handler's JS `finally` finally runs).
                self.mark_abandoned_dispatch(epoch);
            }
            if !matches!(result, Ok(Ok(_))) {
                self.recover_after_interrupt().await;
            }
            result
        })
        .await;
        self.disarm_deadline(token_deadline);
        // A no-op unless the dispatch was abandoned above.
        self.purge_abandoned_dispatch(epoch);
        match result {
            Ok(Ok(_)) => {}
            Ok(Err(js_error)) => return Err(js_error.into()),
            Err(_elapsed) => {
                return Err(anyhow::anyhow!("event `{event}` dispatch timed out"));
            }
        }
        // A handler may register tools/handlers while it runs (`pi.register*`
        // inside a handler): the same publication `emit` performs, and a
        // dirty-check no-op on the hot path when nothing changed.
        self.refresh_registry().await?;
        Ok(())
    }

    /// Record `epoch` as abandoned: side effects the dispatch queues from now
    /// on are dropped instead of being applied to a later run. Shared by
    /// [`Self::emit_with_budget`] and [`Self::emit_read_only`].
    fn mark_abandoned_dispatch(&self, epoch: u64) {
        if epoch == 0 {
            return;
        }
        let state = lock_state(&self.state);
        let mut abandoned = state
            .abandoned_dispatch_epochs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        const MAX_ABANDONED: usize = 256;
        if abandoned.len() >= MAX_ABANDONED {
            abandoned.pop_first();
        }
        abandoned.insert(epoch);
    }

    /// Parse the shared `{ok, error?}` JSON shape of a slash-command /
    /// shortcut dispatch reply.
    fn parse_command_outcome(parsed: &JsonValue) -> CommandOutcome {
        CommandOutcome {
            ok: parsed
                .get("ok")
                .and_then(JsonValue::as_bool)
                .unwrap_or(false),
            error: parsed
                .get("error")
                .and_then(JsonValue::as_str)
                .map(String::from),
        }
    }

    /// Snapshot of registered commands after the last load.
    pub fn command_defs(&self) -> Vec<CommandDef> {
        lock_state(&self.state).command_defs.clone()
    }

    /// Publish just the per-event handler counts of the JS registry
    /// (`__pir_handlerCountsJson`) into the fast-path snapshot
    /// (`RuntimeState::handler_counts`).
    ///
    /// [`Self::refresh_registry`] (load / `emit` / command / shortcut) also
    /// refreshes tools, commands, shortcuts and renderers. The JS-execution
    /// entry points below — a JS tool `execute` body, renderer and
    /// autocomplete callbacks — can register handlers through `pi.on(...)`
    /// exactly like a dispatch context can, but none of them needs the other
    /// registries, and some (a markdown transformer invoked per streaming
    /// update) are too hot to pay the full serialization. Serializing only the
    /// counts keeps a registration made inside those contexts visible to the
    /// next `has_handlers` gate instead of leaving it invisible until the next
    /// full refresh point.
    ///
    /// Best-effort by design: the JS call already produced its own result, and
    /// a publication failure must not turn a successful tool/renderer call
    /// into an error. The snapshot then stays as stale as it was before (the
    /// pre-existing behavior) until the next refresh point. Like `emit`, the
    /// call sites publish after a COMPLETED JS execution (not after a timeout
    /// interruption, where the recovery path returns first).
    ///
    /// The one JS context no refresh point can reach — JS resumed by the
    /// actor's drive loop with no command in flight (a `setTimeout`/promise
    /// continuation whose `pi.on(...)` runs while the actor is idle) — is
    /// covered by the JS dirty signal instead: such a registration bumps
    /// `RuntimeState::handlers_gen` through the `handlersDirty` host fn, so
    /// [`crate::extensions::ExtensionHandle::has_handlers`] answers
    /// conservatively until the next refresh point (this one, or
    /// [`Self::refresh_registry`]) re-publishes the counts with a matching
    /// `handlers_snapshot_gen`.
    pub(super) async fn refresh_handler_counts(&self) -> anyhow::Result<()> {
        // Generation read BEFORE the export, then stamped next to the counts
        // it produced (same protocol as `refresh_registry` in `ops.rs`): a
        // mutation landing after this read leaves the two generations unequal
        // and the accessor conservative — never trusting a snapshot that may
        // be missing the new handler.
        let handlers_gen_before = lock_state(&self.state).handlers_gen;
        let json = self.invoke_0("__pir_handlerCountsJson").await?;
        let counts: HashMap<String, usize> = serde_json::from_str(&json)?;
        let mut state = lock_state(&self.state);
        state.handler_counts = counts;
        state.handlers_snapshot_gen = handlers_gen_before;
        Ok(())
    }

    /// Extensions that loaded successfully.
    pub fn loaded(&self) -> Vec<LoadedExtension> {
        self.loaded
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Execute a registered extension tool through its JS `execute`.
    pub async fn call_tool(
        &self,
        tool_name: &str,
        tool_call_id: &str,
        params: &JsonValue,
    ) -> anyhow::Result<ToolResult> {
        let params_json = serde_json::to_string(params)?;
        let token = self.arm_deadline();
        let result = tokio::time::timeout(
            self.call_timeout(),
            self.invoke_str3(
                "__pir_callTool",
                tool_name.to_string(),
                tool_call_id.to_string(),
                params_json,
            ),
        )
        .await;
        // Heap-cap diagnosis: sample the pending exception and the heap
        // BEFORE the recovery drain clears them, so a memory-limit hit turns
        // into the actionable limit error instead of a bare "Exception
        // generated by QuickJS".
        let memory_failure = if !matches!(result, Ok(Ok(_))) {
            let failure = self.memory_limit_failure().await;
            self.recover_after_interrupt().await;
            failure
        } else {
            // Completed call: refresh the (once-per-crossing) 80% warning.
            self.sample_memory_pressure().await;
            None
        };
        self.disarm_deadline(token);
        let result_json = match result {
            Ok(Ok(result_json)) => result_json,
            // A JS exception: the interrupt handler aborted the job, or the
            // JS side threw where the prelude's try/catch could not see it.
            Ok(Err(js_error)) => {
                self.recover_after_interrupt().await;
                return Err(anyhow::anyhow!(
                    "{}",
                    memory_failure.unwrap_or_else(|| js_error.to_string())
                ));
            }
            // Elapsed tokio deadline.
            Err(_elapsed) => {
                self.recover_after_interrupt().await;
                return Err(anyhow::anyhow!("tool `{tool_name}` execution timed out"));
            }
        };
        // A tool body is an arbitrary JS context: a `pi.on(...)` it (or a
        // continuation it drove) registers must be visible to the caller's
        // gates before this result reaches them.
        let _ = self.refresh_handler_counts().await;
        let parsed: JsonValue = serde_json::from_str(&result_json)?;
        if parsed
            .get("ok")
            .and_then(JsonValue::as_bool)
            .unwrap_or(false)
        {
            Ok(serde_json::from_value::<ToolResult>(parsed)?)
        } else {
            // The prelude catches a throwing tool body and reports
            // `{ok:false, error}` — a heap exhaustion reaches this arm as the
            // error text, so it gets the same actionable rewrite.
            let error = parsed
                .get("error")
                .and_then(JsonValue::as_str)
                .unwrap_or("unknown error")
                .to_string();
            let error = self.decorate_memory_error(error).await;
            Err(anyhow::anyhow!("tool `{tool_name}` failed: {error}"))
        }
    }

    /// Run one deferred `withSession` callback registered by an extension
    /// command's `ctx.newSession/fork/switchSession` options (upstream
    /// `ReplacedSessionContext`, extensions/types.ts:437-452).
    ///
    /// `token` names the callback in the prelude's bounded table; the caller
    /// (the session host) must have pushed the replacement session's snapshot
    /// first — the callback's ctx is built from it. One-shot: the prelude
    /// consumes the token before invoking. Bounded by the ordinary extension
    /// call timeout. A throwing callback and an unknown/expired token are both
    /// errors, never a silent success.
    pub async fn call_session_callback(&self, callback_token: u64) -> anyhow::Result<()> {
        let deadline = self.arm_deadline();
        let result = tokio::time::timeout(
            self.call_timeout(),
            self.invoke_str1("__pir_callSessionCallback", callback_token.to_string()),
        )
        .await;
        // Same heap-cap diagnosis/recovery as `call_tool`: an interrupt or
        // exception leaves the engine needing the recovery drain before the
        // next call.
        let memory_failure = if !matches!(result, Ok(Ok(_))) {
            let failure = self.memory_limit_failure().await;
            self.recover_after_interrupt().await;
            failure
        } else {
            self.sample_memory_pressure().await;
            None
        };
        self.disarm_deadline(deadline);
        let result_json = match result {
            Ok(Ok(result_json)) => result_json,
            Ok(Err(js_error)) => {
                return Err(anyhow::anyhow!(
                    "{}",
                    memory_failure.unwrap_or_else(|| js_error.to_string())
                ));
            }
            Err(_elapsed) => {
                return Err(anyhow::anyhow!(
                    "withSession callback timed out after {:?}",
                    self.call_timeout()
                ));
            }
        };
        // The callback is an arbitrary JS context: a `pi.on(...)` it registers
        // must be visible to the caller's gates before this returns.
        let _ = self.refresh_handler_counts().await;
        let parsed: JsonValue = serde_json::from_str(&result_json)?;
        if parsed
            .get("ok")
            .and_then(JsonValue::as_bool)
            .unwrap_or(false)
        {
            return Ok(());
        }
        Err(anyhow::anyhow!(
            "{}",
            parsed
                .get("error")
                .and_then(JsonValue::as_str)
                .unwrap_or("withSession callback failed")
        ))
    }

    /// Dispatch an event to all registered handlers.
    ///
    /// Handlers run in registration order (extensions in load order); handler
    /// errors are isolated into `EmitOutcome.errors`. For `tool_call`, the
    /// returned `payload` carries handler mutations of `input` and `result`
    /// carries the merged `{block, reason}` outcome.
    pub async fn emit(&self, event: &str, payload: &mut JsonValue) -> anyhow::Result<EmitOutcome> {
        self.emit_with_budget(event, payload, self.call_timeout(), 0)
            .await
    }

    /// [`Self::emit`] with an explicit actor-side budget and dispatch epoch.
    ///
    /// `budget` bounds BOTH the tokio wait and the QuickJS interrupt
    /// deadline, so a handler awaiting a slow host future is actually aborted
    /// when the caller stops waiting; `epoch` is forwarded to the JS dispatch
    /// entry, which tags side effects so a late completion can be discarded.
    /// Epoch `0` disables the JS-side tagging (internal dispatches).
    pub async fn emit_with_budget(
        &self,
        event: &str,
        payload: &mut JsonValue,
        budget: Duration,
        epoch: u64,
    ) -> anyhow::Result<EmitOutcome> {
        // Iterative text, not `serde_json::to_string`: a payload can carry a
        // session tree nested one level per entry (`session_tree`), and the
        // derived serializer recurses one frame per level (a ~1400-entry
        // session overran a 2 MiB thread). The encoding is byte-identical.
        let payload_json = crate::session_manager::json_depth::write_value_iteratively(payload);
        let token = self.arm_deadline_for(budget);
        let result = tokio::time::timeout(
            budget,
            self.invoke_str3(
                "__pir_dispatchEvent",
                event.to_string(),
                payload_json,
                epoch.to_string(),
            ),
        )
        .await;
        // Same heap-cap diagnosis as `call_tool`: sample before recovery
        // clears the pending exception / the peak usage. When no heap cap is
        // configured that sampler returns without reading the exception, so
        // the readable cause (rquickjs's `Error::Exception` Display is only
        // "Exception generated by QuickJS") is taken here, before recovery
        // clears it.
        let (memory_failure, engine_text) = if !matches!(result, Ok(Ok(_))) {
            let failure = self.memory_limit_failure().await;
            let text = if failure.is_some() {
                None
            } else {
                self.take_pending_exception_text().await
            };
            self.recover_after_interrupt().await;
            (failure, text)
        } else {
            (None, None)
        };
        self.disarm_deadline(token);
        let result_json = match result {
            Ok(Ok(result_json)) => result_json,
            Ok(Err(js_error)) => {
                self.recover_after_interrupt().await;
                return Err(anyhow::anyhow!(
                    "{}",
                    memory_failure.unwrap_or_else(|| match engine_text {
                        Some(text) => format!("{js_error}: {text}"),
                        None => js_error.to_string(),
                    })
                ));
            }
            Err(_elapsed) => {
                // The actor-side deadline elapsed: this dispatch is abandoned
                // even if the caller's reply channel already timed out. Mark
                // the epoch so a continuation that resumes later drops its
                // side effects (`exitDispatch` clears the marker when the
                // handler's JS `finally` finally runs).
                self.mark_abandoned_dispatch(epoch);
                self.recover_after_interrupt().await;
                return Err(anyhow::anyhow!(
                    "event `{event}` dispatch timed out after {budget:?}{}",
                    engine_text
                        .map(|text| format!(" (pending engine exception: {text})"))
                        .unwrap_or_default()
                ));
            }
        };
        // Parse the JS dispatch reply and MOVE its parts out: the returned
        // `payload`/`outcome`/`errors` trees are deep clones otherwise, and the
        // provider-stream hook emits once per parsed provider event, so the
        // redundant copy of the (possibly large) event payload is avoided
        // here. The caller-side in-place write-back still costs the one clone
        // it must (the value both lands in `payload` and rides
        // `EmitOutcome::payload`).
        let mut parsed: JsonValue = serde_json::from_str(&result_json)?;
        let outcome = parsed
            .get_mut("outcome")
            .map(JsonValue::take)
            .unwrap_or(JsonValue::Null);
        let mut payload_out = parsed
            .get_mut("payload")
            .map(JsonValue::take)
            .unwrap_or(JsonValue::Null);
        if !payload_out.is_object() {
            payload_out = JsonValue::Null;
        }
        if payload_out.is_object() {
            *payload = payload_out.clone();
        }
        let mut errors = parsed
            .get_mut("errors")
            .and_then(|errors| serde_json::from_value::<Vec<EventError>>(errors.take()).ok())
            .unwrap_or_default();
        // A handler that hit the configured heap cap reports QuickJS's raw
        // out-of-memory text through the per-handler error isolation; rewrite
        // it to the actionable limit text so the failure names how to raise
        // the cap. Also refresh the (once-per-crossing) pressure diagnostic.
        for error in errors.iter_mut() {
            if ExtensionLimits::looks_like_memory_exhaustion(&error.error) {
                let text = std::mem::take(&mut error.error);
                error.error = self.decorate_memory_error(text).await;
            }
        }
        self.sample_memory_pressure().await;
        // Hook registrations during dispatch (e.g. tools registered inside
        // session_start) must become visible to the host registry.
        self.refresh_registry().await?;
        Ok(EmitOutcome {
            result: outcome,
            payload: payload_out,
            errors,
        })
    }

    /// Dispatch a slash command by name (first registration wins).
    pub async fn dispatch_command(&self, name: &str, args: &str) -> anyhow::Result<CommandOutcome> {
        let token = self.arm_deadline();
        let result = tokio::time::timeout(
            self.call_timeout(),
            self.invoke_str2("__pir_dispatchCommand", name.to_string(), args.to_string()),
        )
        .await;
        if !matches!(result, Ok(Ok(_))) {
            self.recover_after_interrupt().await;
        }
        self.disarm_deadline(token);
        let result_json = match result {
            Ok(Ok(result_json)) => result_json,
            Ok(Err(js_error)) => {
                self.recover_after_interrupt().await;
                return Err(js_error.into());
            }
            Err(_elapsed) => {
                self.recover_after_interrupt().await;
                return Err(anyhow::anyhow!("command `{name}` dispatch timed out"));
            }
        };
        let parsed: JsonValue = serde_json::from_str(&result_json)?;
        // Commands may register tools at runtime; refresh the snapshot.
        self.refresh_registry().await?;
        Ok(Self::parse_command_outcome(&parsed))
    }

    /// Dispatch a registered keyboard shortcut by its key combo (first
    /// registration wins). The TUI host calls this when the user presses a
    /// key bound by an extension; the JS handler runs to completion (or
    /// throws, surfaced in `CommandOutcome.error`) and any `ctx.ui.*` side
    /// effects are captured as notifications.
    pub async fn invoke_shortcut(&self, key: &str) -> anyhow::Result<CommandOutcome> {
        let token = self.arm_deadline();
        let result = tokio::time::timeout(
            self.call_timeout(),
            self.invoke_str1("__pir_dispatchShortcut", key.to_string()),
        )
        .await;
        if !matches!(result, Ok(Ok(_))) {
            self.recover_after_interrupt().await;
        }
        self.disarm_deadline(token);
        let result_json = match result {
            Ok(Ok(result_json)) => result_json,
            Ok(Err(js_error)) => {
                self.recover_after_interrupt().await;
                return Err(js_error.into());
            }
            Err(_elapsed) => {
                self.recover_after_interrupt().await;
                return Err(anyhow::anyhow!("shortcut `{key}` dispatch timed out"));
            }
        };
        let parsed: JsonValue = serde_json::from_str(&result_json)?;
        self.refresh_registry().await?;
        Ok(Self::parse_command_outcome(&parsed))
    }

    /// Stale-instance protocol: all captured `pi`/`ctx` calls now throw.
    pub fn invalidate(&self) {
        lock_state(&self.state).invalidated = true;
    }

    /// Invoke one extension renderer callback (`__pir_callRenderer`) with a
    /// bounded timeout — the TUI-side consumption of
    /// `registerMessageRenderer` / `registerEntryRenderer` /
    /// `registerMarkdownTransformer` / `registerToolRenderer`. The QuickJS
    /// interrupt deadline actually aborts a runaway JS job at `timeout`, so
    /// the wait is real. `Ok(outcome)` with
    /// [`RendererCallOutcome::is_unhandled`] means "not handled — keep the
    /// default rendering"; the envelope also carries the tool `renderShell`
    /// choice when the callback is a tool renderer.
    pub async fn invoke_renderer(
        &self,
        request: &JsonValue,
        timeout: Duration,
    ) -> anyhow::Result<RendererCallOutcome> {
        let request_json = serde_json::to_string(request)?;
        let token = self.arm_deadline_for(timeout);
        let result = tokio::time::timeout(
            timeout,
            self.invoke_str1("__pir_callRenderer", request_json),
        )
        .await;
        if !matches!(result, Ok(Ok(_))) {
            self.recover_after_interrupt().await;
        }
        self.disarm_deadline(token);
        let result_json = match result {
            Ok(Ok(result_json)) => result_json,
            Ok(Err(js_error)) => {
                self.recover_after_interrupt().await;
                return Err(js_error.into());
            }
            Err(_elapsed) => {
                self.recover_after_interrupt().await;
                return Err(anyhow::anyhow!("renderer call timed out"));
            }
        };
        // A renderer callback can register handlers too; publish them before
        // the reply is handed back (counts-only: this path runs per rendered
        // message/entry and per streaming markdown update).
        let _ = self.refresh_handler_counts().await;
        let parsed: JsonValue = serde_json::from_str(&result_json)?;
        if !parsed
            .get("ok")
            .and_then(JsonValue::as_bool)
            .unwrap_or(false)
        {
            let error = parsed
                .get("error")
                .and_then(JsonValue::as_str)
                .unwrap_or("renderer failed")
                .to_string();
            return Err(anyhow::anyhow!("{error}"));
        }
        Ok(RendererCallOutcome {
            outcome: RendererOutcome {
                lines: parsed
                    .get("lines")
                    .and_then(JsonValue::as_array)
                    .map(|list| {
                        list.iter()
                            .filter_map(|line| line.as_str().map(String::from))
                            .collect()
                    }),
                text: parsed
                    .get("text")
                    .and_then(JsonValue::as_str)
                    .map(String::from),
            },
            // Only tool renderer replies carry the field; every other
            // renderer kind leaves it absent (the wire omits undefined).
            shell: parsed
                .get("shell")
                .and_then(JsonValue::as_str)
                .and_then(ToolRenderShell::parse),
        })
    }

    /// Fetch argument completions for a slash command through the JS
    /// `getArgumentCompletions` hook (`__pir_callArgCompletions`); `prefix`
    /// is the argument prefix string being completed (JSON-encoded on the
    /// wire so the JS side can `JSON.parse` it back).
    pub async fn argument_completions(
        &self,
        command: &str,
        prefix: &str,
    ) -> anyhow::Result<Vec<JsonValue>> {
        let prefix_json = serde_json::to_string(prefix)?;
        let token = self.arm_deadline();
        let result = tokio::time::timeout(
            self.call_timeout(),
            self.invoke_str2("__pir_callArgCompletions", command.to_string(), prefix_json),
        )
        .await;
        if !matches!(result, Ok(Ok(_))) {
            self.recover_after_interrupt().await;
        }
        self.disarm_deadline(token);
        let result_json = match result {
            Ok(Ok(result_json)) => result_json,
            Ok(Err(js_error)) => {
                self.recover_after_interrupt().await;
                return Err(js_error.into());
            }
            Err(_elapsed) => {
                self.recover_after_interrupt().await;
                return Err(anyhow::anyhow!(
                    "argument completions for `{command}` timed out"
                ));
            }
        };
        // The completer hook is an arbitrary JS context: publish handler
        // registrations before the caller's gates consult the snapshot.
        let _ = self.refresh_handler_counts().await;
        let parsed: JsonValue = serde_json::from_str(&result_json)?;
        if !parsed
            .get("ok")
            .and_then(JsonValue::as_bool)
            .unwrap_or(false)
        {
            let error = parsed
                .get("error")
                .and_then(JsonValue::as_str)
                .unwrap_or("argument completion failed")
                .to_string();
            return Err(anyhow::anyhow!("{error}"));
        }
        Ok(parsed
            .get("items")
            .and_then(JsonValue::as_array)
            .cloned()
            .unwrap_or_default())
    }

    /// Consult the `ui.addAutocompleteProvider` suggesters
    /// (`__pir_callAutocompleteProviders`) with the serialized completion
    /// context `{lines, cursorLine, cursorCol, prefix}`; the first suggester
    /// returning a non-null item list wins.
    pub async fn extension_suggestions(
        &self,
        context_json: &str,
    ) -> anyhow::Result<Vec<JsonValue>> {
        let token = self.arm_deadline();
        let result = tokio::time::timeout(
            self.call_timeout(),
            self.invoke_str1("__pir_callAutocompleteProviders", context_json.to_string()),
        )
        .await;
        if !matches!(result, Ok(Ok(_))) {
            self.recover_after_interrupt().await;
        }
        self.disarm_deadline(token);
        let result_json = match result {
            Ok(Ok(result_json)) => result_json,
            Ok(Err(js_error)) => {
                self.recover_after_interrupt().await;
                return Err(js_error.into());
            }
            Err(_elapsed) => {
                self.recover_after_interrupt().await;
                return Err(anyhow::anyhow!("extension suggestions timed out"));
            }
        };
        // The suggester hook is an arbitrary JS context: publish handler
        // registrations before the caller's gates consult the snapshot.
        let _ = self.refresh_handler_counts().await;
        let parsed: JsonValue = serde_json::from_str(&result_json)?;
        if !parsed
            .get("ok")
            .and_then(JsonValue::as_bool)
            .unwrap_or(false)
        {
            let error = parsed
                .get("error")
                .and_then(JsonValue::as_str)
                .unwrap_or("extension suggestions failed")
                .to_string();
            return Err(anyhow::anyhow!("{error}"));
        }
        Ok(parsed
            .get("items")
            .and_then(JsonValue::as_array)
            .cloned()
            .unwrap_or_default())
    }
}
