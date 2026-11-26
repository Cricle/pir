//! The result of a completed prompt run.

use crate::ai::types::{
    AssistantMessage, ContentBlock, Message, StopReason, ToolResultMessage, Usage,
};

/// The terminal state of one prompt run.
///
/// `message` is the final assistant message (including error stop reasons —
/// inspect `message.stop_reason` / `message.error_message` when
/// `aborted`/errors are possible). `tool_results` lists the tool-result
/// messages appended during the run in source order, and `messages` is the
/// run's own transcript slice in append order (upstream `agent_end.messages`:
/// every assistant and tool-result message THIS run appended, including the
/// intermediate tool-calling assistant messages that `message` alone omits).
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct AgentOutcome {
    /// The terminal assistant message of the run.
    pub message: AssistantMessage,
    /// Tool-result messages appended during the run, in source order.
    pub tool_results: Vec<ToolResultMessage>,
    /// Every message the run appended (assistant + tool results), in append
    /// order (upstream `AgentEvent["agent_end"].messages`). The terminal
    /// message is its last element for a run that produced one.
    pub messages: Vec<Message>,
    /// Usage reported by the TERMINAL assistant message only. The usage of
    /// earlier turns in the same run is NOT included here; session-level
    /// totals across every turn of every run are surfaced by
    /// [`crate::sdk::Agent::usage_totals`].
    pub usage: Usage,
    /// Number of loop turns the run executed.
    pub turns: usize,
    /// Whether the run stopped because it was aborted.
    pub aborted: bool,
}

impl AgentOutcome {
    pub(crate) fn from_parts(outcome: super::event::LoopOutcome) -> Self {
        Self {
            usage: outcome.terminal.usage,
            message: outcome.terminal,
            tool_results: outcome.tool_results,
            messages: outcome.messages,
            turns: outcome.turns,
            aborted: outcome.aborted,
        }
    }

    /// Fallback synthesis when the run returned a terminal message without
    /// an `AgentEnd` capture (defensive; not expected).
    pub(crate) fn from_terminal(terminal: AssistantMessage) -> Self {
        Self {
            usage: terminal.usage,
            aborted: terminal.stop_reason == StopReason::Aborted,
            messages: vec![Message::Assistant(terminal.clone())],
            message: terminal,
            tool_results: Vec::new(),
            turns: 0,
        }
    }

    /// The concatenated text content of the terminal assistant message
    /// (empty for tool-use-only or error messages).
    pub fn message_text(&self) -> Option<String> {
        text_of_blocks(&self.message.content)
    }

    /// The text content of each tool result, in source order.
    pub fn tool_result_texts(&self) -> Vec<String> {
        self.tool_results
            .iter()
            .map(|result| text_of_blocks(&result.content).unwrap_or_default())
            .collect()
    }
}

/// Concatenate the text blocks of an assistant content list.
fn text_of_blocks(content: &[ContentBlock]) -> Option<String> {
    let mut text = String::new();
    for block in content {
        if let ContentBlock::Text(text_content) = block {
            if !text.is_empty() {
                text.push('\n');
            }
            text.push_str(&text_content.text);
        }
    }
    if text.is_empty() {
        None
    } else {
        Some(text)
    }
}
