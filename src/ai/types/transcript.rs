//! Transcript replay helpers.
//!
//! Port of upstream `packages/ai/src/utils/transcript.ts` (plus the two text
//! helpers it borrows from `utils/text.ts`). A transcript is a flat message
//! list whose system messages are *deltas*: the leading one declares the
//! prompt and tools, and later ones replace prompt sections by name, append
//! content, and add or remove tools. These helpers replay every delta into one
//! leading system message, which is what APIs that carry the system prompt out
//! of band need.
//!
//! The helpers are additive: adapters keep their own replay (Anthropic's
//! `convert_messages`, for example, folds later system text into the top-level
//! prompt and replays `toolsAdded`/`toolsRemoved` in place), so nothing here
//! is wired into them.

use std::collections::BTreeMap;

use super::messages::{ContentBlock, Context, Message, Tool, UserMessage, UserRole};

/// A branded transcript: messages whose system entries are deltas to replay.
///
/// Upstream brands the plain array to force every provider-facing function
/// through [`normalize_context`]; the newtype gives Rust the same guarantee.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TranscriptContext(Vec<Message>);

impl TranscriptContext {
    pub fn new(messages: Vec<Message>) -> Self {
        Self(messages)
    }

    /// The transcript's messages.
    pub fn messages(&self) -> &[Message] {
        &self.0
    }

    /// The transcript's messages (borrowing alias of [`Self::messages`]).
    pub fn as_slice(&self) -> &[Message] {
        &self.0
    }

    /// Take the transcript's messages.
    pub fn into_messages(self) -> Vec<Message> {
        self.0
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl From<Vec<Message>> for TranscriptContext {
    fn from(messages: Vec<Message>) -> Self {
        Self(messages)
    }
}

/// Build the leading system message for a prompt and tool set. Returns `None`
/// when both are empty, so an empty transcript stays empty.
///
/// `content` is the prompt (or `""` when only tools are declared, as upstream
/// `content: systemPrompt ?? ""`); `toolsAdded` is present only when there are
/// tools. The timestamp is `0` — this message is synthesized, not observed.
pub fn create_initial_system_message(
    system_prompt: Option<&str>,
    tools: Option<&[Tool]>,
) -> Option<Message> {
    let has_system_prompt = system_prompt.is_some_and(|prompt| !prompt.is_empty());
    let has_tools = tools.is_some_and(|tools| !tools.is_empty());
    if !has_system_prompt && !has_tools {
        return None;
    }
    Some(Message::System(UserMessage {
        role: UserRole::System,
        content: vec![ContentBlock::Text(super::messages::TextContent {
            text: system_prompt.unwrap_or("").to_owned(),
            text_signature: None,
        })],
        tools_added: if has_tools {
            tools.map(<[Tool]>::to_vec)
        } else {
            None
        },
        tools_removed: None,
        sections: None,
        timestamp: 0,
    }))
}

/// Fold [`Context::system_prompt`] and [`Context::tools`] into a leading system
/// message. This is the only entry point that produces a
/// [`TranscriptContext`]; every provider-facing function expects the result.
pub fn normalize_context(context: &Context) -> TranscriptContext {
    let mut messages = Vec::with_capacity(context.messages.len() + 1);
    if let Some(initial) =
        create_initial_system_message(context.system_prompt.as_deref(), context.tools.as_deref())
    {
        messages.push(initial);
    }
    messages.extend(context.messages.iter().cloned());
    TranscriptContext(messages)
}

/// Whether a message is a system delta (upstream `role === "system"`).
/// `developer` messages are not system messages upstream and are not here.
pub fn is_system_message(message: &Message) -> bool {
    matches!(message, Message::System(_))
}

/// The leading system message, if the transcript starts with one.
pub fn get_initial_system_message(messages: &[Message]) -> Option<&UserMessage> {
    match messages.first() {
        Some(Message::System(system)) => Some(system),
        _ => None,
    }
}

/// Drop the leading system message for APIs that carry the prompt outside the
/// message list.
pub fn without_initial_system_message(messages: &[Message]) -> &[Message] {
    if get_initial_system_message(messages).is_some() {
        &messages[1..]
    } else {
        messages
    }
}

/// Resolve the tools available after applying every transcript delta in order.
///
/// Within one system message removals apply before additions; later
/// definitions replace earlier ones under the same name and keep the position
/// of the first declaration (upstream `Map` semantics).
pub fn get_current_tools(messages: &[Message]) -> Vec<Tool> {
    let mut tools: Vec<Tool> = Vec::new();
    for message in messages {
        let Message::System(system) = message else {
            continue;
        };
        for reference in system.tools_removed.iter().flatten() {
            tools.retain(|tool| tool.name != reference.name);
        }
        for tool in system.tools_added.iter().flatten() {
            match tools.iter_mut().find(|existing| existing.name == tool.name) {
                Some(existing) => *existing = tool.clone(),
                None => tools.push(tool.clone()),
            }
        }
    }
    tools
}

/// Join a message's text blocks with `"\n"` (upstream `contentText`'s default
/// separator). Non-text blocks are skipped; a message with no text blocks
/// yields `""`.
pub fn content_text(message: &UserMessage) -> String {
    message
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text(text) => Some(text.text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Render a system message as a complete prompt: its content followed by its
/// section texts, non-empty parts joined with a blank line (upstream
/// `getSystemMessageText`).
pub fn system_message_text(message: &UserMessage) -> String {
    let mut parts = vec![content_text(message)];
    for text in message
        .sections
        .iter()
        .flat_map(|sections| sections.values())
        .flatten()
    {
        parts.push(text.clone());
    }
    parts.retain(|part| !part.is_empty());
    parts.join("\n\n")
}

/// Replay every system message into one leading system message holding the
/// current prompt, sections and tools.
///
/// Later `content` is appended to the base prompt as a new paragraph,
/// `sections` are patched by name (`None` deletes), and tools are resolved
/// with [`get_current_tools`]. The timestamp is the first system message's
/// (upstream `timestamp ??=`), or `0` when only a synthesized head results.
/// Returns `None` when no system message exists.
pub fn get_current_system_message(messages: &[Message]) -> Option<Message> {
    let mut content: Vec<String> = Vec::new();
    let mut sections: BTreeMap<String, String> = BTreeMap::new();
    let mut timestamp: Option<u64> = None;
    for message in messages {
        let Message::System(system) = message else {
            continue;
        };
        if timestamp.is_none() {
            timestamp = Some(system.timestamp);
        }
        let text = content_text(system);
        if !text.is_empty() {
            content.push(text);
        }
        for (name, value) in system.sections.iter().flatten() {
            match value {
                None => {
                    sections.remove(name);
                }
                Some(value) => {
                    sections.insert(name.clone(), value.clone());
                }
            }
        }
    }
    let tools = get_current_tools(messages);
    if timestamp.is_none() && tools.is_empty() {
        return None;
    }
    Some(Message::System(UserMessage {
        role: UserRole::System,
        content: vec![ContentBlock::Text(super::messages::TextContent {
            text: content.join("\n\n"),
            text_signature: None,
        })],
        tools_added: if tools.is_empty() { None } else { Some(tools) },
        tools_removed: None,
        sections: if sections.is_empty() {
            None
        } else {
            Some(
                sections
                    .into_iter()
                    .map(|(name, text)| (name, Some(text)))
                    .collect(),
            )
        },
        timestamp: timestamp.unwrap_or(0),
    }))
}

/// Render the current system prompt text after replaying every system message.
pub fn get_current_system_prompt(messages: &[Message]) -> String {
    match get_current_system_message(messages) {
        Some(Message::System(system)) => system_message_text(&system),
        _ => String::new(),
    }
}

/// Rebuild the transcript for APIs without mid-conversation system messages:
/// the replayed system message leads and every later system message is
/// dropped. Non-system messages keep their order.
pub fn collapse_system_messages(context: TranscriptContext) -> TranscriptContext {
    let head = get_current_system_message(context.as_slice());
    let mut messages: Vec<Message> = Vec::with_capacity(context.as_slice().len() + 1);
    if let Some(head) = head {
        messages.push(head);
    }
    messages.extend(
        context
            .into_messages()
            .into_iter()
            .filter(|message| !is_system_message(message)),
    );
    TranscriptContext(messages)
}

/// Keep later system messages in place when the model accepts them; otherwise
/// collapse them. `None` means the caller did not say — upstream treats that
/// as unsupported and collapses.
pub fn resolve_transcript(
    context: TranscriptContext,
    supports_mid_convo_system_messages: Option<bool>,
) -> TranscriptContext {
    if supports_mid_convo_system_messages == Some(true) {
        context
    } else {
        collapse_system_messages(context)
    }
}
