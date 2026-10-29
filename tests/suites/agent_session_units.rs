//! Unit tests for `pir::agent_session` helpers that the public prompt path
//! cannot reach directly (ephemeral history trimming).

use pir::agent_session::trim_ephemeral_history;
use pir::ai::types::{AssistantMessage, ContentBlock, Message, TextContent, UserMessage, UserRole};

/// Local copy of the crate-private `user_message` fixture in
/// `src/agent_session.rs`; integration tests cannot see it.
fn user_message(text: &str, images: Vec<ContentBlock>) -> Message {
    let mut content = vec![ContentBlock::Text(TextContent {
        text: text.to_owned(),
        text_signature: None,
    })];
    content.extend(images);
    Message::User(UserMessage {
        role: UserRole::User,
        content,
        tools_added: None,
        tools_removed: None,
        sections: None,
        timestamp: pir::ai::util::now_ms(),
    })
}

fn assistant_message(text: &str) -> Message {
    Message::Assistant(AssistantMessage {
        content: vec![ContentBlock::Text(TextContent {
            text: text.to_owned(),
            text_signature: None,
        })],
        ..AssistantMessage::default()
    })
}

fn message_text(message: &Message) -> &str {
    match message {
        Message::User(user) | Message::System(user) | Message::Developer(user) => {
            match user.content.first() {
                Some(ContentBlock::Text(text)) => &text.text,
                _ => "",
            }
        }
        Message::Assistant(assistant) => match assistant.content.first() {
            Some(ContentBlock::Text(text)) => &text.text,
            _ => "",
        },
        Message::ToolResult(_) => "",
    }
}

#[test]
fn keeps_newest_messages_within_count_cap() {
    let mut history: Vec<Message> = (0..8)
        .map(|index| user_message(&format!("m{index}"), Vec::new()))
        .collect();

    trim_ephemeral_history(&mut history, 3, u64::MAX);

    let texts: Vec<&str> = history.iter().map(message_text).collect();
    assert_eq!(texts, ["m5", "m6", "m7"]);
}

#[test]
fn keeps_newest_messages_within_token_cap() {
    // 1,000 chars ≈ 250 estimated tokens each (chars/4).
    let mut history: Vec<Message> = (0..4)
        .map(|_| user_message(&"x".repeat(1_000), Vec::new()))
        .collect();

    trim_ephemeral_history(&mut history, usize::MAX, 500);

    assert_eq!(history.len(), 2);
    assert!(history
        .iter()
        .all(|message| matches!(message, Message::User(_))));
}

#[test]
fn retained_window_starts_at_a_user_turn_boundary() {
    let mut history = vec![
        user_message("u1", Vec::new()),
        assistant_message("a1"),
        user_message("u2", Vec::new()),
        assistant_message("a2"),
    ];

    // The count cap would retain `a1, u2, a2`; the boundary rule drops the
    // partial turn so the window starts at `u2`.
    trim_ephemeral_history(&mut history, 3, u64::MAX);

    let texts: Vec<&str> = history.iter().map(message_text).collect();
    assert_eq!(texts, ["u2", "a2"]);
}

#[test]
fn empty_history_is_unchanged() {
    let mut history = Vec::new();
    trim_ephemeral_history(&mut history, 1, 1);
    assert!(history.is_empty());
}
