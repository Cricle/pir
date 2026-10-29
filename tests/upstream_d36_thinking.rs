//! D36 — thinking from a different model is dropped, never sent as text
//! (upstream #10995).
//!
//! When the session switches models, the earlier assistant messages' thinking
//! blocks were forwarded to the new model as plain text. The new model then
//! treated the old model's reasoning as part of its visible reply and could
//! start writing its own reasoning into the answer. Text and tool calls are
//! kept; signed/redacted blocks survive only for the model that produced them.

use std::collections::BTreeMap;

use pir::ai::api::transform::transform_messages;
use pir::ai::types::{
    AssistantMessage, ContentBlock, Message, Model, ModelCost, ModelCostRates, StopReason,
    TextContent, ThinkingContent, ToolCall,
};

fn model(id: &str, api: &str) -> Model {
    Model {
        id: id.into(),
        name: id.into(),
        api: api.into(),
        provider: "test".into(),
        base_url: "https://example.invalid".into(),
        reasoning: true,
        thinking_level_map: None,
        input: vec!["text".into()],
        cost: ModelCost {
            rates: ModelCostRates {
                input: 0.0,
                output: 0.0,
                cache_read: 0.0,
                cache_write: 0.0,
            },
            tiers: vec![],
        },
        context_window: 16_000,
        max_tokens: 2_000,
        sampling_params: None,
        headers: None,
        thinking_token_budget_field: None,
        input_limits: None,
        prompt_cache: None,
        sampling_params_by_thinking_level: None,
        model_type: None,
        compat: None,
    }
}

fn assistant(api: &str, id: &str, thinking: ThinkingContent) -> Message {
    Message::Assistant(AssistantMessage {
        api: api.into(),
        provider: "test".into(),
        model: id.into(),
        content: vec![
            ContentBlock::Thinking(thinking),
            ContentBlock::Text(TextContent {
                text: "visible answer".into(),
                text_signature: None,
            }),
            ContentBlock::ToolCall(ToolCall {
                id: "call-1".into(),
                name: "bash".into(),
                arguments: BTreeMap::new(),
                thought_signature: None,
                namespace: None,
                arguments_error: None,
            }),
        ],
        stop_reason: StopReason::ToolUse,
        ..AssistantMessage::default()
    })
}

fn plain_thinking() -> ThinkingContent {
    ThinkingContent {
        thinking: "old model reasoning".into(),
        thinking_signature: None,
        redacted: false,
    }
}

fn content_of(message: &Message) -> &[ContentBlock] {
    match message {
        Message::Assistant(assistant) => &assistant.content,
        _ => panic!("expected an assistant message"),
    }
}

#[test]
fn d36_cross_model_thinking_is_dropped() {
    let source = assistant("openai-chat", "old-model", plain_thinking());
    let transformed = transform_messages(&[source], &model("new-model", "openai-chat"), None);
    let blocks = content_of(&transformed[0]);

    assert!(
        !blocks
            .iter()
            .any(|block| matches!(block, ContentBlock::Thinking(_))),
        "cross-model thinking must be dropped, got {blocks:?}"
    );
    assert!(
        !blocks.iter().any(|block| matches!(
            block,
            ContentBlock::Text(text) if text.text == "old model reasoning"
        )),
        "cross-model thinking must not be replayed as text, got {blocks:?}"
    );
    assert!(
        blocks.iter().any(
            |block| matches!(block, ContentBlock::Text(text) if text.text == "visible answer")
        ),
        "the visible text must be kept"
    );
    assert!(
        blocks
            .iter()
            .any(|block| matches!(block, ContentBlock::ToolCall(_))),
        "tool calls must be kept"
    );
}

#[test]
fn d36_same_model_thinking_is_kept() {
    let source = assistant("openai-chat", "same-model", plain_thinking());
    let transformed = transform_messages(&[source], &model("same-model", "openai-chat"), None);
    let blocks = content_of(&transformed[0]);

    assert!(
        blocks.iter().any(|block| matches!(
            block,
            ContentBlock::Thinking(thinking) if thinking.thinking == "old model reasoning"
        )),
        "same-model thinking must replay unchanged, got {blocks:?}"
    );
}

#[test]
fn d36_cross_model_signed_thinking_is_also_dropped() {
    // A signature is only valid for the model that produced it, so the block
    // goes with it rather than being downgraded to text.
    let mut thinking = plain_thinking();
    thinking.thinking_signature = Some("sig-123".into());
    let source = assistant("anthropic-messages", "old-model", thinking);
    let transformed =
        transform_messages(&[source], &model("new-model", "anthropic-messages"), None);
    let blocks = content_of(&transformed[0]);
    assert!(
        !blocks
            .iter()
            .any(|block| matches!(block, ContentBlock::Thinking(_))),
        "cross-model signed thinking must be dropped, got {blocks:?}"
    );
}
