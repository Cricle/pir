//! GitHub Copilot dynamic request headers.
//!
//! Rust equivalent of upstream `packages/ai/src/api/github-copilot-headers.ts`:
//! `X-Initiator` (user vs agent), `Openai-Intent`, and
//! `Copilot-Vision-Request` when the transcript carries images.
//!
//! Shared by the OpenAI Responses, OpenAI Completions and Anthropic adapters
//! (upstream passes the same helper into every Copilot-capable client), so it
//! lives outside the `provider-openai` feature gate.

use std::collections::BTreeMap;

use crate::ai::types::{ContentBlock, Message, Model, ProviderHeaders};

/// Whether a model routes through GitHub Copilot (upstream checks
/// `model.provider === "github-copilot"`; pir also matches the Copilot hosts
/// so gateway-style base URLs qualify).
pub fn is_github_copilot_model(model: &Model) -> bool {
    model.provider == "github-copilot" || model.base_url.contains("githubcopilot.com")
}

/// Copilot expects `X-Initiator` to indicate whether the request is
/// user-initiated or agent-initiated (e.g. follow-up after assistant/tool
/// messages) (upstream `inferCopilotInitiator`).
pub fn infer_copilot_initiator(messages: &[Message]) -> &'static str {
    match messages.last() {
        Some(last) if !matches!(last, Message::User(_)) => "agent",
        _ => "user",
    }
}

/// Copilot requires the `Copilot-Vision-Request` header when sending images
/// (upstream `hasCopilotVisionInput`).
pub fn has_copilot_vision_input(messages: &[Message]) -> bool {
    messages.iter().any(|message| match message {
        Message::User(user) => has_image_block(&user.content),
        Message::ToolResult(result) => has_image_block(&result.content),
        _ => false,
    })
}

fn has_image_block(content: &[ContentBlock]) -> bool {
    content
        .iter()
        .any(|block| matches!(block, ContentBlock::Image(_)))
}

/// Build the dynamic Copilot headers (upstream `buildCopilotDynamicHeaders`).
/// Public for tests/.
pub fn build_copilot_dynamic_headers(messages: &[Message], has_images: bool) -> ProviderHeaders {
    let mut headers = BTreeMap::new();
    headers.insert(
        "X-Initiator".to_string(),
        infer_copilot_initiator(messages).to_string(),
    );
    headers.insert(
        "Openai-Intent".to_string(),
        "conversation-edits".to_string(),
    );
    if has_images {
        headers.insert("Copilot-Vision-Request".to_string(), "true".to_string());
    }
    headers
}
