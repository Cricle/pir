//! Agent event to transcript progress mapping.

use crate::protocol::{ModelRef, TranscriptItem};

pub(super) fn agent_event_to_progress(
    event: &crate::agent::agent_loop::AgentEvent,
    message_id: &str,
) -> Option<crate::protocol::schemas::TranscriptProgress> {
    use crate::agent::agent_loop::AgentEvent;
    use crate::ai::types::AssistantMessageEvent;
    match event {
        AgentEvent::StreamEvent { event, .. } => match event {
            AssistantMessageEvent::Start { partial } => {
                Some(crate::protocol::schemas::TranscriptProgress::ItemStarted {
                    item: assistant_progress_item(partial, "streaming", message_id, None, None),
                })
            }
            AssistantMessageEvent::TextStart { partial, .. }
            | AssistantMessageEvent::ThinkingStart { partial, .. }
            | AssistantMessageEvent::ToolCallStart { partial, .. } => {
                Some(crate::protocol::schemas::TranscriptProgress::ItemUpdated {
                    item: assistant_progress_item(partial, "streaming", message_id, None, None),
                })
            }
            AssistantMessageEvent::TextDelta {
                content_index,
                delta,
                ..
            } => Some(delta_progress(message_id, *content_index, "text", delta)),
            AssistantMessageEvent::ThinkingDelta {
                content_index,
                delta,
                ..
            } => Some(delta_progress(
                message_id,
                *content_index,
                "thinking",
                delta,
            )),
            AssistantMessageEvent::ToolCallDelta {
                content_index,
                delta,
                ..
            } => Some(delta_progress(
                message_id,
                *content_index,
                "toolCall",
                delta,
            )),
            AssistantMessageEvent::TextEnd { partial, .. }
            | AssistantMessageEvent::ThinkingEnd { partial, .. }
            | AssistantMessageEvent::ToolCallEnd { partial, .. } => {
                Some(crate::protocol::schemas::TranscriptProgress::ItemUpdated {
                    item: assistant_progress_item(partial, "streaming", message_id, None, None),
                })
            }
            AssistantMessageEvent::Done { message, .. } => {
                let (status, stop) = finished_assistant_status(message.stop_reason);
                Some(crate::protocol::schemas::TranscriptProgress::ItemFinished {
                    item: assistant_progress_item(
                        message,
                        status,
                        message_id,
                        Some(stop),
                        message.error_message.clone(),
                    ),
                })
            }
            AssistantMessageEvent::Error { error, .. } => {
                Some(crate::protocol::schemas::TranscriptProgress::ItemFinished {
                    item: assistant_progress_item(
                        error,
                        "error",
                        message_id,
                        Some("error".to_owned()),
                        error.error_message.clone(),
                    ),
                })
            }
        },
        AgentEvent::ToolExecutionStart { event, .. } => {
            let crate::agent::tool_batch::ToolExecutionEvent::Start {
                call_id, tool_name, ..
            } = event
            else {
                return None;
            };
            Some(crate::protocol::schemas::TranscriptProgress::ItemStarted {
                item: tool_progress_item(call_id, tool_name, "running", false),
            })
        }
        AgentEvent::ToolExecutionEnd { event, .. } => {
            let crate::agent::tool_batch::ToolExecutionEvent::End {
                call_id,
                tool_name,
                is_error,
                ..
            } = event
            else {
                return None;
            };
            let status = if *is_error { "error" } else { "complete" };
            Some(crate::protocol::schemas::TranscriptProgress::ItemFinished {
                item: tool_progress_item(call_id, tool_name, status, *is_error),
            })
        }
        _ => None,
    }
}

fn delta_progress(
    message_id: &str,
    content_index: usize,
    kind: &str,
    delta: &str,
) -> crate::protocol::schemas::TranscriptProgress {
    crate::protocol::schemas::TranscriptProgress::AssistantDelta {
        message_id: message_id.to_owned(),
        content_index: content_index as i64,
        kind: kind.to_owned(),
        delta: delta.to_owned(),
    }
}

fn finished_assistant_status(stop_reason: crate::ai::types::StopReason) -> (&'static str, String) {
    use crate::ai::types::StopReason;
    match stop_reason {
        StopReason::Length => ("complete", "length".to_owned()),
        StopReason::ToolUse => ("complete", "toolUse".to_owned()),
        StopReason::Error => ("error", "error".to_owned()),
        StopReason::Aborted => ("aborted", "aborted".to_owned()),
        // Stop, Deferred, and Pending all end the visible assistant item.
        _ => ("complete", "stop".to_owned()),
    }
}

fn assistant_progress_item(
    message: &crate::ai::types::AssistantMessage,
    status: &str,
    message_id: &str,
    stop_reason: Option<String>,
    error_message: Option<String>,
) -> TranscriptItem {
    TranscriptItem::Assistant(crate::protocol::schemas::AssistantTranscriptItem {
        id: message_id.to_owned(),
        role: "assistant".to_owned(),
        content: assistant_content_blocks(&message.content),
        model: ModelRef {
            provider: nonempty_or_default(&message.provider),
            id: nonempty_or_default(&message.model),
        },
        response_model: message
            .response_model
            .clone()
            .filter(|value| !value.is_empty()),
        usage: None,
        status: status.to_owned(),
        stop_reason,
        error_message,
        timestamp: message.timestamp as i64,
    })
}

fn tool_progress_item(
    call_id: &str,
    tool_name: &str,
    status: &str,
    is_error: bool,
) -> TranscriptItem {
    TranscriptItem::Tool(crate::protocol::schemas::ToolTranscriptItem {
        id: call_id.to_owned(),
        role: "tool".to_owned(),
        tool_call_id: call_id.to_owned(),
        tool_name: tool_name.to_owned(),
        input: serde_json::json!({}),
        content: Vec::new(),
        details: None,
        usage: None,
        status: status.to_owned(),
        is_error,
        timestamp: crate::ai::util::now_ms() as i64,
    })
}

fn assistant_content_blocks(
    blocks: &[crate::ai::types::ContentBlock],
) -> Vec<crate::protocol::schemas::AssistantContent> {
    blocks
        .iter()
        .filter_map(|block| match block {
            crate::ai::types::ContentBlock::Text(text) => {
                Some(crate::protocol::schemas::AssistantContent::Text(
                    crate::protocol::schemas::TextContent {
                        type_: "text".to_owned(),
                        text: text.text.clone(),
                    },
                ))
            }
            crate::ai::types::ContentBlock::Thinking(thinking) => {
                Some(crate::protocol::schemas::AssistantContent::Thinking(
                    crate::protocol::schemas::ThinkingContent {
                        type_: "thinking".to_owned(),
                        thinking: thinking.thinking.clone(),
                        redacted: Some(thinking.redacted),
                    },
                ))
            }
            crate::ai::types::ContentBlock::ToolCall(call) => {
                Some(crate::protocol::schemas::AssistantContent::ToolCall(
                    crate::protocol::schemas::ToolCallContent {
                        type_: "toolCall".to_owned(),
                        tool_call_id: call.id.clone(),
                        tool_name: call.name.clone(),
                        input: serde_json::to_value(&call.arguments)
                            .unwrap_or_else(|_| serde_json::json!({})),
                    },
                ))
            }
            crate::ai::types::ContentBlock::Image(_) => None,
        })
        .collect()
}

fn nonempty_or_default(value: &str) -> String {
    if value.is_empty() {
        "default".to_owned()
    } else {
        value.to_owned()
    }
}
