//! Shared transcript transformation for the remaining adapters.
//!
//! Rust equivalent of upstream `packages/ai/src/api/transform-messages.ts`
//! (message replay normalization) plus the transcript utilities the adapters
//! need (`normalizeIdPart`, `shortHash`). Mirrors:
//!
//! - unsupported-image downgrade with text placeholders,
//! - cross-model thinking/text/signature lowering,
//! - tool-call id normalization with a map applied to tool results,
//! - errored/aborted assistant skip,
//! - synthetic `No result provided` tool results for orphaned calls,
//! - system messages held back across pending tool calls.

use crate::ai::types::{
    AssistantMessage, ContentBlock, Context, ImageContent, Message, Model, ToolResultMessage,
};

/// The leading system prompt of a context, when non-empty (upstream
/// `getInitialSystemMessage` + `getSystemMessageText`).
pub fn get_initial_system_prompt(context: &Context) -> Option<String> {
    context
        .system_prompt
        .as_deref()
        .filter(|system| !system.is_empty())
        .map(str::to_string)
}

/// Truncate marker length constants shared with the adapters.
const NON_VISION_USER_IMAGE_PLACEHOLDER: &str = "(image omitted: model does not support images)";
const NON_VISION_TOOL_IMAGE_PLACEHOLDER: &str =
    "(tool image omitted: model does not support images)";
/// Text that replaces each run of images in an error tool result
/// (upstream `ERROR_TOOL_IMAGE_PLACEHOLDER`).
const ERROR_TOOL_IMAGE_PLACEHOLDER: &str = "(image attached after the tool results)";

/// An error tool result split into a text-only result and the images to send
/// after the tool results (upstream `SplitErrorToolResult`).
pub struct SplitErrorToolResult {
    /// The result content with each run of images replaced by a note.
    pub content: Vec<ContentBlock>,
    /// Text naming the tool call, to send before `images`.
    pub caption: String,
    pub images: Vec<ImageContent>,
}

/// Split the images out of an error tool result, for APIs that reject images in
/// error results (Anthropic, and Claude on Bedrock). Send the returned content
/// as the tool result and the caption and images after all tool results of the
/// turn, in the same user message. Returns `None` for results that need no
/// split (#10794).
#[cfg_attr(
    not(any(feature = "provider-anthropic", feature = "provider-bedrock")),
    allow(dead_code)
)]
pub fn split_error_tool_result_images(message: &ToolResultMessage) -> Option<SplitErrorToolResult> {
    if !message.is_error {
        return None;
    }
    let images: Vec<ImageContent> = message
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Image(image) => Some(image.clone()),
            _ => None,
        })
        .collect();
    if images.is_empty() {
        return None;
    }
    Some(SplitErrorToolResult {
        content: replace_images_with_placeholder(&message.content, ERROR_TOOL_IMAGE_PLACEHOLDER),
        caption: format!(
            "Images from the error result of tool call {} ({}):",
            message.tool_call_id, message.tool_name
        ),
        images,
    })
}

/// Normalize an arbitrary id into the `[a-zA-Z0-9_-]` set capped at 64 chars
/// with trailing separators stripped (upstream `normalizeIdPart`).
pub fn normalize_id_part(part: &str) -> String {
    let mut normalized: String = part
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if normalized.chars().count() > 64 {
        normalized = normalized.chars().take(64).collect();
    }
    let trimmed = normalized.trim_end_matches('_');
    if trimmed.is_empty() {
        // All separators: keep the (possibly shortened) underscored form rather
        // than an empty id; upstream would return the same trimmed-empty string.
        String::new()
    } else {
        trimmed.to_string()
    }
}

/// Map an id into the `[a-zA-Z0-9_-]` set (the tool-call id sanitizer shared
/// by the Anthropic, Google, and OpenAI Completions adapters). Unlike
/// [`normalize_id_part`] it keeps trailing separators and does not cap the
/// length; callers truncate as their provider requires.
#[cfg_attr(
    not(any(
        feature = "provider-anthropic",
        feature = "provider-openai",
        feature = "provider-google"
    )),
    allow(dead_code)
)]
pub(crate) fn sanitize_id_part(part: &str) -> String {
    part.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// Fast deterministic hash to shorten long strings (upstream `shortHash` in
/// `utils/hash.ts`): two 32-bit lanes over the string's UTF-16 code units with
/// `Math.imul` multiply/xor mixing, rendered as base-36 `(h2, h1)`.
pub fn short_hash(input: &str) -> String {
    let mut h1: u32 = 0xdeadbeef;
    let mut h2: u32 = 0x41c6ce57;
    for unit in input.encode_utf16() {
        h1 = (h1 ^ u32::from(unit)).wrapping_mul(2654435761);
        h2 = (h2 ^ u32::from(unit)).wrapping_mul(1597334677);
    }
    h1 = (h1 ^ (h1 >> 16)).wrapping_mul(2246822507) ^ (h2 ^ (h2 >> 13)).wrapping_mul(3266489909);
    h2 = (h2 ^ (h2 >> 16)).wrapping_mul(2246822507) ^ (h1 ^ (h1 >> 13)).wrapping_mul(3266489909);
    format!("{}{}", base36(h2), base36(h1))
}

/// JS-style unsigned base-36 rendering (`(n >>> 0).toString(36)`).
fn base36(value: u32) -> String {
    const DIGITS: &[u8] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    if value == 0 {
        return "0".to_string();
    }
    let mut out = Vec::new();
    let mut rest = value;
    while rest > 0 {
        out.push(DIGITS[(rest % 36) as usize]);
        rest /= 36;
    }
    out.reverse();
    String::from_utf8(out).unwrap_or_default()
}

/// Normalize a tool call id for cross-provider replay when a callback is
/// given, mirroring upstream `transformMessages`.
type NormalizeToolCallId<'a> = &'a dyn Fn(&str, &AssistantMessage) -> String;

fn is_same_model(model: &Model, assistant: &AssistantMessage) -> bool {
    assistant.provider == model.provider
        && assistant.api == model.api
        && assistant.model == model.id
}

/// Whether a content list carries an image block (the only block kind the
/// image downgrade rewrites).
fn has_image_block(blocks: &[ContentBlock]) -> bool {
    blocks
        .iter()
        .any(|block| matches!(block, ContentBlock::Image(_)))
}

fn replace_images_with_placeholder(
    blocks: &[ContentBlock],
    placeholder: &str,
) -> Vec<ContentBlock> {
    let mut result: Vec<ContentBlock> = Vec::new();
    let mut previous_was_placeholder = false;
    for block in blocks {
        match block {
            ContentBlock::Image(_) => {
                if !previous_was_placeholder {
                    result.push(ContentBlock::Text(crate::ai::types::TextContent {
                        text: placeholder.to_string(),
                        text_signature: None,
                    }));
                }
                previous_was_placeholder = true;
            }
            ContentBlock::Text(text) => {
                previous_was_placeholder = text.text == placeholder;
                result.push(block.clone());
            }
            other => result.push(other.clone()),
        }
    }
    result
}

/// Transform one transcript for replay against `model` (upstream
/// `transformMessages`). `normalize_tool_call_id` is invoked for tool calls on
/// messages from a different provider/api/model; the mapping is applied to the
/// matching tool results.
pub fn transform_messages(
    messages: &[Message],
    model: &Model,
    normalize_tool_call_id: Option<NormalizeToolCallId<'_>>,
) -> Vec<Message> {
    transform_messages_lazy(messages, model, normalize_tool_call_id)
        .into_iter()
        .map(std::borrow::Cow::into_owned)
        .collect()
}

/// [`transform_messages`] without materializing the transcript: a message the
/// transformation leaves untouched (the common case — same model, no images to
/// downgrade, no id to normalize) comes back **borrowed**, and only a message
/// that genuinely changes is rebuilt. The returned list still carries the
/// reordering the second pass performs (orphaned-call synthesis, errored/
/// aborted assistant skip, system messages held across pending tool calls),
/// so the sequence is identical to [`transform_messages`]'s output.
///
/// This is the memory-lane shape: the former `transform_messages` deep-cloned
/// every message of the transcript on every provider request (a 100 MB session
/// paid a second ~100 MB conversation per request just to hand the adapter its
/// input).
pub fn transform_messages_lazy<'a>(
    messages: &'a [Message],
    model: &Model,
    normalize_tool_call_id: Option<NormalizeToolCallId<'_>>,
) -> Vec<std::borrow::Cow<'a, Message>> {
    transform_messages_lazy_iter(messages.iter(), model, normalize_tool_call_id)
}

/// [`transform_messages_lazy`] over messages selected by a filter (for
/// example the non-system half of a `collapseSystemMessages` transcript).
///
/// WHY (no transcript copy): the adapters that fold mid-conversation system
/// messages used to clone every surviving message into a temporary
/// `Vec<Message>` just to obtain a contiguous slice for
/// [`transform_messages_lazy`] — a second full copy of a large transcript on
/// every request. A `Vec<&Message>` (one pointer per message) has the same
/// shape for the transform, and every untouched message still comes back
/// borrowed from the caller's transcript.
pub fn transform_messages_lazy_refs<'a>(
    messages: &[&'a Message],
    model: &Model,
    normalize_tool_call_id: Option<NormalizeToolCallId<'_>>,
) -> Vec<std::borrow::Cow<'a, Message>> {
    transform_messages_lazy_iter(messages.iter().copied(), model, normalize_tool_call_id)
}

/// The shared body of [`transform_messages_lazy`] and
/// [`transform_messages_lazy_refs`]: an iterator of borrowed messages keeps
/// both entry points allocation-free at the call site (no intermediate
/// `Vec<&Message>` for the plain slice form).
fn transform_messages_lazy_iter<'a, I>(
    messages: I,
    model: &Model,
    normalize_tool_call_id: Option<NormalizeToolCallId<'_>>,
) -> Vec<std::borrow::Cow<'a, Message>>
where
    I: IntoIterator<Item = &'a Message>,
{
    use std::borrow::Cow;

    let supports_images = model.input.iter().any(|input| input == "image");

    // First pass: image downgrade, thinking/text lowering, tool-call id
    // normalization (recorded into a map for the tool results).
    let mut tool_call_id_map: Vec<(String, String)> = Vec::new();
    let messages = messages.into_iter();
    let mut transformed: Vec<Cow<'a, Message>> = Vec::with_capacity(messages.size_hint().0);
    for message in messages {
        transformed.push(transform_message_lazy(
            message,
            model,
            normalize_tool_call_id,
            &mut tool_call_id_map,
            supports_images,
        ));
    }

    // Second pass: synthetic tool results for orphaned calls, errored/aborted
    // assistant skip, system messages held across pending tool calls.
    let mut pending = PendingToolCalls::default();
    let mut result: Vec<Cow<'a, Message>> = Vec::with_capacity(transformed.len());
    for message in transformed {
        match &*message {
            Message::Assistant(assistant) => {
                pending.close(&mut result);
                if matches!(
                    assistant.stop_reason,
                    crate::ai::types::StopReason::Error | crate::ai::types::StopReason::Aborted
                ) {
                    // Skip errored/aborted assistant messages entirely.
                    continue;
                }
                // Only the (id, name) pair is needed to synthesize a missing
                // result later — cloning the whole ToolCall (arguments
                // included, potentially megabytes) was pure overhead.
                pending.calls = assistant
                    .content
                    .iter()
                    .filter_map(|block| match block {
                        ContentBlock::ToolCall(call) => Some((call.id.clone(), call.name.clone())),
                        _ => None,
                    })
                    .collect();
                pending.answered.clear();
                result.push(message);
            }
            Message::ToolResult(tool_result) => {
                pending.answered.push(tool_result.tool_call_id.clone());
                result.push(message);
            }
            Message::System(_) => {
                if !pending.calls.is_empty() {
                    pending.held.push(message);
                } else {
                    result.push(message);
                }
            }
            _ => {
                pending.close(&mut result);
                result.push(message);
            }
        }
    }
    pending.close(&mut result);
    result
}

/// Transform ONE message (first pass of [`transform_messages_lazy`]).
fn transform_message_lazy<'a>(
    message: &'a Message,
    model: &Model,
    normalize_tool_call_id: Option<NormalizeToolCallId<'_>>,
    tool_call_id_map: &mut Vec<(String, String)>,
    supports_images: bool,
) -> std::borrow::Cow<'a, Message> {
    use std::borrow::Cow;

    match message {
        Message::User(user) => {
            // The image downgrade is the identity when a non-vision model
            // receives a text-only message: borrow it instead of rebuilding
            // the whole content list (every request replays the transcript).
            if supports_images || !has_image_block(&user.content) {
                return Cow::Borrowed(message);
            }
            Cow::Owned(Message::User(crate::ai::types::UserMessage {
                content: replace_images_with_placeholder(
                    &user.content,
                    NON_VISION_USER_IMAGE_PLACEHOLDER,
                ),
                ..user.clone()
            }))
        }
        // System/developer messages are replayed unchanged (they only carry
        // text; the image downgrade does not apply to them).
        Message::System(_) | Message::Developer(_) => Cow::Borrowed(message),
        Message::Assistant(assistant) => match transform_assistant_lazy(
            assistant,
            model,
            normalize_tool_call_id,
            tool_call_id_map,
        ) {
            Cow::Borrowed(_) => Cow::Borrowed(message),
            Cow::Owned(assistant) => Cow::Owned(Message::Assistant(assistant)),
        },
        Message::ToolResult(tool_result) => {
            // ToolResult: remap the tool call id when the matching call was
            // normalized upstream in the transcript.
            let normalized = tool_call_id_map
                .iter()
                .find(|(original, _)| *original == tool_result.tool_call_id)
                .map(|(_, normalized)| normalized.clone());
            // A non-vision model needs the image downgrade only when the
            // result actually carries images; a text-only result is replayed
            // borrowed, not rebuilt (tool output can be megabytes).
            let needs_image_downgrade = !supports_images && has_image_block(&tool_result.content);
            if !needs_image_downgrade && normalized.is_none() {
                return Cow::Borrowed(message);
            }
            let mut result = tool_result.clone();
            if needs_image_downgrade {
                result.content = replace_images_with_placeholder(
                    &result.content,
                    NON_VISION_TOOL_IMAGE_PLACEHOLDER,
                );
            }
            if let Some(normalized) = normalized {
                result.tool_call_id = normalized;
            }
            Cow::Owned(Message::ToolResult(result))
        }
    }
}

/// Tool-call accounting state for the orphaned-call synthesis pass. A pending
/// call only needs its `(id, name)` to synthesize a `No result provided`
/// placeholder; a system message held across the pending batch keeps its
/// original form.
#[derive(Default)]
struct PendingToolCalls<'a> {
    calls: Vec<(String, String)>,
    answered: Vec<String>,
    held: Vec<std::borrow::Cow<'a, Message>>,
}

impl<'a> PendingToolCalls<'a> {
    /// Synthesize `No result provided` tool results for every pending call
    /// without one, then release the held system messages.
    fn close(&mut self, out: &mut Vec<std::borrow::Cow<'a, Message>>) {
        for (id, name) in self.calls.drain(..) {
            if !self.answered.contains(&id) {
                out.push(std::borrow::Cow::Owned(Message::ToolResult(
                    ToolResultMessage {
                        role: crate::ai::types::ToolResultRole::ToolResult,
                        tool_call_id: id,
                        tool_name: name,
                        content: vec![ContentBlock::Text(crate::ai::types::TextContent {
                            text: "No result provided".to_string(),
                            text_signature: None,
                        })],
                        details: None,
                        usage: None,
                        added_tool_names: Vec::new(),
                        structured_content: None,
                        nested_calls: None,
                        is_error: true,
                        duration_ms: None,
                        timestamp: crate::ai::util::now_ms(),
                    },
                )));
            }
        }
        self.answered.clear();
        out.append(&mut self.held);
    }
}

/// Transform one assistant message's content blocks for replay, borrowing the
/// message when the transformation is the identity (same model, no id
/// normalization, no empty thinking block to drop).
fn transform_assistant_lazy<'a>(
    assistant: &'a AssistantMessage,
    model: &Model,
    normalize_tool_call_id: Option<NormalizeToolCallId<'_>>,
    tool_call_id_map: &mut Vec<(String, String)>,
) -> std::borrow::Cow<'a, AssistantMessage> {
    use std::borrow::Cow;

    let same_model = is_same_model(model, assistant);
    // Same-model replay keeps signed thinking, text and tool calls as they
    // are; the ONLY block it drops is an unsigned thinking block whose text is
    // whitespace-only (the signed/redacted checks precede the emptiness check
    // in the loop below).
    let drops_empty_thinking = |block: &ContentBlock| {
        matches!(block, ContentBlock::Thinking(thinking)
            if thinking.thinking_signature.is_none() && thinking.thinking.trim().is_empty())
    };
    if same_model
        && normalize_tool_call_id.is_none()
        && !assistant.content.iter().any(drops_empty_thinking)
    {
        // Same model: text, signed thinking and tool calls replay unchanged;
        // only a whitespace-only thinking block is dropped.
        return Cow::Borrowed(assistant);
    }

    let mut content: Vec<ContentBlock> = Vec::with_capacity(assistant.content.len());
    for block in &assistant.content {
        match block {
            ContentBlock::Thinking(thinking) => {
                // Thinking from another model is dropped entirely (#10995,
                // drift D36). Signatures and encrypted reasoning are only
                // valid for the model that produced them, and forwarding the
                // readable text makes the new model treat the old model's
                // reasoning as part of the visible reply.
                if !same_model {
                    continue;
                }
                // Keep redacted and signed blocks even with empty thinking
                // text (OpenAI encrypted reasoning): replay needs them.
                if thinking.redacted || thinking.thinking_signature.is_some() {
                    content.push(block.clone());
                    continue;
                }
                if thinking.thinking.trim().is_empty() {
                    continue;
                }
                content.push(block.clone());
            }
            ContentBlock::Text(text) => {
                if same_model {
                    content.push(block.clone());
                } else {
                    content.push(ContentBlock::Text(crate::ai::types::TextContent {
                        text: text.text.clone(),
                        text_signature: None,
                    }));
                }
            }
            ContentBlock::ToolCall(call) => {
                let mut call = call.clone();
                if !same_model {
                    call.thought_signature = None;
                    if let Some(normalize) = normalize_tool_call_id {
                        let normalized = normalize(&call.id, assistant);
                        if normalized != call.id {
                            tool_call_id_map.push((call.id.clone(), normalized.clone()));
                            call.id = normalized;
                        }
                    }
                }
                content.push(ContentBlock::ToolCall(call));
            }
            ContentBlock::Image(_) => content.push(block.clone()),
        }
    }
    Cow::Owned(assistant_with_content(assistant, content))
}

/// Rebuild an assistant message around a new content list WITHOUT cloning the
/// original content (the `..message.clone()` struct-update form pays for a
/// full content copy that is immediately dropped).
fn assistant_with_content(
    assistant: &AssistantMessage,
    content: Vec<ContentBlock>,
) -> AssistantMessage {
    AssistantMessage {
        role: assistant.role,
        content,
        api: assistant.api.clone(),
        provider: assistant.provider.clone(),
        model: assistant.model.clone(),
        response_model: assistant.response_model.clone(),
        response_id: assistant.response_id.clone(),
        diagnostics: assistant.diagnostics.clone(),
        usage: assistant.usage,
        stop_reason: assistant.stop_reason,
        deferred: assistant.deferred.clone(),
        error_message: assistant.error_message.clone(),
        raw_stop_reason: assistant.raw_stop_reason.clone(),
        end_turn: assistant.end_turn,
        provider_thinking_level: assistant.provider_thinking_level.clone(),
        thinking_level: assistant.thinking_level,
        duration_ms: assistant.duration_ms,
        timestamp: assistant.timestamp,
    }
}
