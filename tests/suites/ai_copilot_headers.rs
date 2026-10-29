//! GitHub Copilot dynamic request headers (upstream
//! `packages/ai/src/api/github-copilot-headers.ts`). Moved out of the inline
//! `#[cfg(test)]` module in `src/ai/api/openai_responses/copilot.rs` by the
//! source-hygiene gate (all tests live under `tests/`).

use pir::ai::api::openai_responses::copilot::{
    build_copilot_dynamic_headers, has_copilot_vision_input, infer_copilot_initiator,
};
use pir::ai::types::{
    ContentBlock, ImageContent, Message, ToolResultMessage, UserMessage, UserRole,
};

fn user_message(content: Vec<ContentBlock>) -> Message {
    Message::User(UserMessage {
        role: UserRole::User,
        content,
        tools_added: None,
        tools_removed: None,
        timestamp: 0,
        sections: None,
    })
}

fn tool_result_message(content: Vec<ContentBlock>) -> Message {
    Message::ToolResult(ToolResultMessage {
        role: pir::ai::types::ToolResultRole::ToolResult,
        tool_call_id: "t1".into(),
        tool_name: "read".into(),
        content,
        details: None,
        usage: None,
        added_tool_names: Vec::new(),
        structured_content: None,
        nested_calls: None,
        is_error: false,
        duration_ms: None,
        timestamp: 0,
    })
}

#[test]
fn initiator_follows_last_role() {
    assert_eq!(infer_copilot_initiator(&[user_message(vec![])]), "user");
    assert_eq!(infer_copilot_initiator(&[]), "user");
    // A trailing tool result means the request is agent-initiated.
    assert_eq!(
        infer_copilot_initiator(&[user_message(vec![]), tool_result_message(vec![])]),
        "agent"
    );
}

#[test]
fn vision_input_detects_images_in_user_and_tool_results() {
    let image = ContentBlock::Image(ImageContent {
        data: "aGk=".to_string(),
        mime_type: "image/png".to_string(),
    });
    assert!(has_copilot_vision_input(&[user_message(vec![
        image.clone()
    ])]));
    assert!(has_copilot_vision_input(&[tool_result_message(vec![
        image
    ])]));
    assert!(!has_copilot_vision_input(&[user_message(vec![])]));
    assert!(!has_copilot_vision_input(&[]));
}

#[test]
fn dynamic_headers_shape() {
    let headers = build_copilot_dynamic_headers(&[], false);
    assert_eq!(headers.get("X-Initiator").map(String::as_str), Some("user"));
    assert_eq!(
        headers.get("Openai-Intent").map(String::as_str),
        Some("conversation-edits")
    );
    assert!(!headers.contains_key("Copilot-Vision-Request"));

    let with_images = build_copilot_dynamic_headers(&[], true);
    assert_eq!(
        with_images
            .get("Copilot-Vision-Request")
            .map(String::as_str),
        Some("true")
    );
}
