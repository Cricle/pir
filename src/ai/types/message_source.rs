//! Streaming, replayable model-message sources.
//!
//! [`crate::ai::types::Context`] carries its transcript as a `Vec<Message>`.
//! Every provider request that needs the conversation materializes that
//! vector, and a large session pays a transcript-sized allocation (a 100 MB
//! session is ~140 MB of message slots plus text). A [`MessageSource`] is the
//! streaming twin: it yields one message at a time — borrowed from a resident
//! slice, or owned by a file-backed reader — so an adapter can
//! transform/serialize and drop each message before reading the next.
//!
//! Sources are replayable: [`MessageSource::messages`] may be called more than
//! once. The OpenAI streamed body asks [`MessageSource::summary`] once for the
//! replay-plan facts (instruction messages, tool history) and then walks the
//! messages once for serialization. A file-backed source re-reads its file per
//! pass.

use super::messages::Message;

/// One item produced by a [`MessageSource`].
///
/// `Borrowed` is the zero-copy shape for resident transcripts (the former
/// `&[Message]` builder input); `Owned` is the shape a file-backed source
/// produces, one parsed message at a time.
// The `Owned` arm is one message by value on purpose: this enum exists so a
// resident transcript can be handed out as `Borrowed` (16 B per element) while
// a file-backed source yields one parsed message at a time. Boxing would add
// one allocation per message on the very path the type was introduced for.
#[allow(clippy::large_enum_variant)]
pub enum SourceMessage<'a> {
    Borrowed(&'a Message),
    Owned(Message),
}

impl SourceMessage<'_> {
    /// The message, whichever ownership shape carries it.
    pub fn message(&self) -> &Message {
        match self {
            Self::Borrowed(message) => message,
            Self::Owned(message) => message,
        }
    }
}

/// The transcript-level facts a replay plan needs before it can stream the
/// messages.
///
/// Kept deliberately small: only instruction-role messages (system/developer)
/// are retained, because the replay plan folds their text into the leading
/// system prompt when the API cannot take mid-conversation system messages.
/// Everything else is a count/size/boolean.
#[derive(Clone, Debug, Default)]
pub struct MessageSourceSummary {
    /// Number of messages in the source.
    pub message_count: usize,
    /// Conservative estimate of the serialized JSON size of all messages.
    pub approximate_json_bytes: usize,
    /// Every system/developer message, in transcript order.
    pub instruction_messages: Vec<Message>,
    /// Whether the transcript contains tool calls or tool results.
    pub has_tool_history: bool,
}

/// A replayable sequence of model messages that can be walked one message at
/// a time without retaining the whole transcript.
pub trait MessageSource: Send + Sync {
    /// A fresh iterator over the messages, in wire order. Each call restarts
    /// from the first message.
    fn messages(&self) -> Box<dyn Iterator<Item = crate::Result<SourceMessage<'_>>> + '_>;

    /// Conservative estimate of the serialized JSON size of all messages.
    fn approximate_json_bytes(&self) -> usize;

    /// The replay-plan facts for this source.
    ///
    /// The default walks the source once. Implementations that already hold
    /// the facts (a file-backed source caches them from its index pass) should
    /// override this to avoid a second parse.
    fn summary(&self) -> crate::Result<MessageSourceSummary> {
        summarize_messages(self.messages())
    }
}

/// [`MessageSource::summary`]'s shared default implementation.
pub fn summarize_messages(
    mut messages: Box<dyn Iterator<Item = crate::Result<SourceMessage<'_>>> + '_>,
) -> crate::Result<MessageSourceSummary> {
    let mut summary = MessageSourceSummary::default();
    for message in messages.by_ref() {
        let message = message?;
        let message = message.message();
        summary.message_count += 1;
        summary.approximate_json_bytes = summary
            .approximate_json_bytes
            .saturating_add(approximate_message_json_bytes(message));
        if matches!(message, Message::System(_) | Message::Developer(_)) {
            summary.instruction_messages.push(message.clone());
        }
        if !summary.has_tool_history {
            summary.has_tool_history = message_has_tool_history(message);
        }
    }
    Ok(summary)
}

/// Whether the message carries a tool call or is a tool result (upstream
/// `hasToolHistory`).
pub fn message_has_tool_history(message: &Message) -> bool {
    match message {
        Message::ToolResult(_) => true,
        Message::Assistant(assistant) => assistant
            .content
            .iter()
            .any(|block| matches!(block, super::messages::ContentBlock::ToolCall(_))),
        _ => false,
    }
}

/// A conservative per-message serialized-size estimate: text/image/tool-call
/// payload bytes plus fixed JSON overhead. Underestimating only costs one
/// output-buffer reallocation, never correctness.
pub fn approximate_message_json_bytes(message: &Message) -> usize {
    let content = match message {
        Message::User(payload) | Message::System(payload) | Message::Developer(payload) => {
            payload.content.iter()
        }
        Message::Assistant(payload) => payload.content.iter(),
        Message::ToolResult(payload) => payload.content.iter(),
    };
    content
        .map(approximate_content_json_bytes)
        .fold(0usize, usize::saturating_add)
}

fn approximate_content_json_bytes(block: &super::messages::ContentBlock) -> usize {
    use super::messages::ContentBlock;
    match block {
        ContentBlock::Text(content) => content
            .text
            .len()
            .saturating_add(content.text_signature.as_deref().map_or(0, str::len)),
        ContentBlock::Thinking(content) => content
            .thinking
            .len()
            .saturating_add(content.thinking_signature.as_deref().map_or(0, str::len)),
        ContentBlock::Image(content) => content.data.len().saturating_add(content.mime_type.len()),
        ContentBlock::ToolCall(call) => call
            .id
            .len()
            .saturating_add(call.name.len())
            .saturating_add(
                call.arguments
                    .iter()
                    .map(|(key, value)| {
                        key.len()
                            .saturating_add(value.as_str().map_or(64, str::len))
                    })
                    .sum::<usize>(),
            )
            .saturating_add(call.thought_signature.as_deref().map_or(0, str::len)),
    }
}

/// A [`MessageSource`] over a resident message slice — the adapter shape that
/// keeps every existing `&[Message]` caller on the same code path.
pub struct SliceMessageSource<'a> {
    messages: &'a [Message],
}

impl<'a> SliceMessageSource<'a> {
    pub fn new(messages: &'a [Message]) -> Self {
        Self { messages }
    }
}

impl MessageSource for SliceMessageSource<'_> {
    fn messages(&self) -> Box<dyn Iterator<Item = crate::Result<SourceMessage<'_>>> + '_> {
        Box::new(
            self.messages
                .iter()
                .map(|message| Ok(SourceMessage::Borrowed(message))),
        )
    }

    fn approximate_json_bytes(&self) -> usize {
        self.messages
            .iter()
            .map(approximate_message_json_bytes)
            .fold(0usize, usize::saturating_add)
    }
}

/// A source prefix followed by a resident suffix. The print host uses this
/// shape: the session file supplies the historical transcript and
/// `Context.messages` supplies the messages this run produced.
pub struct ChainedMessageSource<'a> {
    prefix: &'a dyn MessageSource,
    suffix: &'a [Message],
}

impl<'a> ChainedMessageSource<'a> {
    pub fn new(prefix: &'a dyn MessageSource, suffix: &'a [Message]) -> Self {
        Self { prefix, suffix }
    }
}

impl MessageSource for ChainedMessageSource<'_> {
    fn messages(&self) -> Box<dyn Iterator<Item = crate::Result<SourceMessage<'_>>> + '_> {
        let suffix = self
            .suffix
            .iter()
            .map(|message| Ok(SourceMessage::Borrowed(message)));
        Box::new(self.prefix.messages().chain(suffix))
    }

    fn approximate_json_bytes(&self) -> usize {
        let suffix: usize = self
            .suffix
            .iter()
            .map(approximate_message_json_bytes)
            .fold(0usize, usize::saturating_add);
        self.prefix.approximate_json_bytes().saturating_add(suffix)
    }

    fn summary(&self) -> crate::Result<MessageSourceSummary> {
        let mut summary = self.prefix.summary()?;
        let suffix = SliceMessageSource::new(self.suffix).summary()?;
        summary.message_count = summary.message_count.saturating_add(suffix.message_count);
        summary.approximate_json_bytes = summary
            .approximate_json_bytes
            .saturating_add(suffix.approximate_json_bytes);
        summary
            .instruction_messages
            .extend(suffix.instruction_messages);
        summary.has_tool_history |= suffix.has_tool_history;
        Ok(summary)
    }
}
