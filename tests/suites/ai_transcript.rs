//! Transcript replay helpers (upstream `packages/ai/src/utils/transcript.ts`).

use std::collections::BTreeMap;

use pir::ai::types::*;
use serde_json::json;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn text(t: &str) -> ContentBlock {
    ContentBlock::Text(TextContent {
        text: t.to_owned(),
        text_signature: None,
    })
}

fn tool(name: &str) -> Tool {
    Tool {
        name: name.to_owned(),
        description: format!("{name} tool"),
        parameters: json!({"type": "object", "properties": {}}),
        constrained_sampling: None,
    }
}

fn tool_ref(name: &str) -> ToolReference {
    ToolReference {
        name: name.to_owned(),
    }
}

fn system(content: &str, timestamp: u64) -> Message {
    Message::System(UserMessage {
        role: UserRole::System,
        content: if content.is_empty() {
            Vec::new()
        } else {
            vec![text(content)]
        },
        tools_added: None,
        tools_removed: None,
        sections: None,
        timestamp,
    })
}

fn system_with(
    content: &str,
    sections: Option<&[(&str, Option<&str>)]>,
    added: Option<Vec<Tool>>,
    removed: Option<Vec<ToolReference>>,
    timestamp: u64,
) -> Message {
    Message::System(UserMessage {
        role: UserRole::System,
        content: if content.is_empty() {
            Vec::new()
        } else {
            vec![text(content)]
        },
        tools_added: added,
        tools_removed: removed,
        sections: sections.map(|entries| {
            entries
                .iter()
                .map(|(name, value)| ((*name).to_owned(), value.map(|text| text.to_owned())))
                .collect::<BTreeMap<_, _>>()
        }),
        timestamp,
    })
}

#[test]
fn system_message_with_content_has_one_text_block() {
    // Sanity check for the fixtures above: an empty string means no block, so
    // `get_current_system_message` skips it exactly like upstream's
    // `text.length === 0` check.
    let message = system_payload(Some(system_with("", None, None, None, 0)));
    assert!(message.content.is_empty());
    let message = system_payload(Some(system_with("text", None, None, None, 0)));
    assert_eq!(message.content, vec![text("text")]);
}

fn user(content: &str) -> Message {
    Message::User(UserMessage {
        role: UserRole::User,
        content: vec![text(content)],
        tools_added: None,
        tools_removed: None,
        sections: None,
        timestamp: 5,
    })
}

fn system_payload(message: Option<Message>) -> UserMessage {
    match message {
        Some(Message::System(payload)) => payload,
        other => panic!("not a system message: {other:?}"),
    }
}

fn payload_text(message: &UserMessage) -> String {
    content_text(message)
}

fn tool_names(tools: &[Tool]) -> Vec<&str> {
    tools.iter().map(|tool| tool.name.as_str()).collect()
}

// ---------------------------------------------------------------------------
// create_initial_system_message
// ---------------------------------------------------------------------------

#[test]
fn initial_system_message_absent_when_prompt_and_tools_are_empty() {
    assert!(create_initial_system_message(None, None).is_none());
    assert!(create_initial_system_message(Some(""), None).is_none());
    assert!(create_initial_system_message(None, Some(&[])).is_none());
    assert!(create_initial_system_message(Some(""), Some(&[])).is_none());
}

#[test]
fn initial_system_message_prompt_only() {
    let message = system_payload(create_initial_system_message(Some("be helpful"), None));
    assert_eq!(payload_text(&message), "be helpful");
    assert_eq!(message.tools_added, None);
    assert_eq!(message.tools_removed, None);
    assert_eq!(message.sections, None);
    assert_eq!(message.timestamp, 0);
    assert_eq!(message.role, UserRole::System);
}

#[test]
fn initial_system_message_tools_only_has_empty_content() {
    let tools = vec![tool("read"), tool("write")];
    let message = system_payload(create_initial_system_message(None, Some(&tools)));
    assert_eq!(payload_text(&message), "");
    assert_eq!(message.tools_added.as_deref(), Some(tools.as_slice()));
    assert_eq!(message.timestamp, 0);
}

#[test]
fn initial_system_message_prompt_and_tools() {
    let tools = vec![tool("read")];
    let message = system_payload(create_initial_system_message(Some("prompt"), Some(&tools)));
    assert_eq!(payload_text(&message), "prompt");
    assert_eq!(message.tools_added.as_deref(), Some(tools.as_slice()));
    // The initial message never declares removals or sections (upstream shape).
    assert_eq!(message.tools_removed, None);
    assert_eq!(message.sections, None);
}

// ---------------------------------------------------------------------------
// normalize_context
// ---------------------------------------------------------------------------

#[test]
fn normalize_context_leads_with_the_initial_message() {
    let context = Context {
        system_prompt: Some("prompt".to_owned()),
        messages: vec![user("hi")],
        tools: Some(vec![tool("read")]),
    };
    let transcript = normalize_context(&context);
    assert_eq!(transcript.messages().len(), 2);
    assert!(is_system_message(&transcript.messages()[0]));
    assert_eq!(get_current_system_prompt(transcript.messages()), "prompt");
    assert_eq!(
        tool_names(&get_current_tools(transcript.messages())),
        ["read"]
    );
    assert_eq!(transcript.into_messages().len(), 2);
}

#[test]
fn normalize_context_without_prompt_or_tools_is_the_messages() {
    let context = Context {
        system_prompt: None,
        messages: vec![user("hi")],
        tools: None,
    };
    let transcript = normalize_context(&context);
    assert_eq!(transcript.as_slice(), context.messages.as_slice());
    assert!(get_initial_system_message(transcript.as_slice()).is_none());
}

// ---------------------------------------------------------------------------
// get_current_tools
// ---------------------------------------------------------------------------

#[test]
fn current_tools_apply_additions_in_order() {
    let messages = vec![
        system_with("", None, Some(vec![tool("a")]), None, 0),
        system_with("", None, Some(vec![tool("b")]), None, 1),
    ];
    assert_eq!(tool_names(&get_current_tools(&messages)), ["a", "b"]);
}

#[test]
fn current_tools_apply_removals() {
    let messages = vec![
        system_with("", None, Some(vec![tool("a"), tool("b")]), None, 0),
        system_with("", None, None, Some(vec![tool_ref("a")]), 1),
    ];
    assert_eq!(tool_names(&get_current_tools(&messages)), ["b"]);
}

#[test]
fn current_tools_readdition_replaces_the_definition_in_place() {
    let original = tool("a");
    let mut replacement = tool("a");
    replacement.description = "replacement".to_owned();
    let messages = vec![
        system_with("", None, Some(vec![original, tool("b")]), None, 0),
        system_with("", None, Some(vec![replacement.clone()]), None, 1),
    ];
    let current = get_current_tools(&messages);
    // Position is the first declaration's; the definition is the later one.
    assert_eq!(tool_names(&current), ["a", "b"]);
    assert_eq!(current[0].description, "replacement");
}

#[test]
fn current_tools_apply_removals_before_additions_within_one_message() {
    // Upstream iterates `toolsRemoved` then `toolsAdded` in the same message,
    // so a remove-then-re-add under one name keeps the new definition.
    let mut replacement = tool("a");
    replacement.description = "second".to_owned();
    let messages = vec![
        system_with("", None, Some(vec![tool("a")]), None, 0),
        system_with(
            "",
            None,
            Some(vec![replacement]),
            Some(vec![tool_ref("a")]),
            1,
        ),
    ];
    let current = get_current_tools(&messages);
    assert_eq!(tool_names(&current), ["a"]);
    assert_eq!(current[0].description, "second");
}

#[test]
fn current_tools_ignore_non_system_messages() {
    let messages = vec![
        system_with("", None, Some(vec![tool("a")]), None, 0),
        user("hello"),
        system_with("", None, Some(vec![tool("b")]), None, 1),
    ];
    assert_eq!(tool_names(&get_current_tools(&messages)), ["a", "b"]);
}

// ---------------------------------------------------------------------------
// content_text / system_message_text
// ---------------------------------------------------------------------------

#[test]
fn content_text_joins_text_blocks_with_a_newline() {
    let mut message = system_payload(Some(system("", 0)));
    message.content = vec![text("one"), text("two")];
    assert_eq!(content_text(&message), "one\ntwo");
}

#[test]
fn system_message_text_appends_sections_after_content() {
    let message = system_payload(Some(system_with(
        "base",
        Some(&[("a", Some("A")), ("b", None), ("c", Some("C"))]),
        None,
        None,
        0,
    )));
    // Upstream renders content then every non-null section value.
    assert_eq!(system_message_text(&message), "base\n\nA\n\nC");
    assert_eq!(
        get_current_system_prompt(&[Message::System(message)]),
        "base\n\nA\n\nC"
    );
}

// ---------------------------------------------------------------------------
// get_current_system_message
// ---------------------------------------------------------------------------

#[test]
fn current_system_message_is_none_without_system_messages() {
    assert!(get_current_system_message(&[user("hi")]).is_none());
    assert!(get_current_system_message(&[]).is_none());
}

#[test]
fn current_system_message_concatenates_content_with_a_blank_line() {
    let messages = vec![system("first", 1), user("hi"), system("second", 2)];
    let message = system_payload(get_current_system_message(&messages));
    assert_eq!(payload_text(&message), "first\n\nsecond");
    // Empty content contributes nothing (upstream skips `text.length === 0`).
    let messages = vec![system("first", 1), system("", 2)];
    let message = system_payload(get_current_system_message(&messages));
    assert_eq!(payload_text(&message), "first");
}

#[test]
fn current_system_message_patches_sections_by_name() {
    let messages = vec![
        system_with(
            "base",
            Some(&[("a", Some("A1")), ("b", Some("B1"))]),
            None,
            None,
            1,
        ),
        system_with(
            "",
            Some(&[("a", Some("A2")), ("c", Some("C"))]),
            None,
            None,
            2,
        ),
    ];
    let message = system_payload(get_current_system_message(&messages));
    let sections = message.sections.expect("sections present");
    assert_eq!(sections.get("a"), Some(&Some("A2".to_owned())));
    assert_eq!(sections.get("b"), Some(&Some("B1".to_owned())));
    assert_eq!(sections.get("c"), Some(&Some("C".to_owned())));
    assert_eq!(sections.len(), 3);
}

#[test]
fn current_system_message_null_section_deletes_it() {
    let messages = vec![
        system_with(
            "",
            Some(&[("a", Some("A")), ("b", Some("B"))]),
            None,
            None,
            1,
        ),
        system_with("", Some(&[("a", None)]), None, None, 2),
    ];
    let message = system_payload(get_current_system_message(&messages));
    let sections = message.sections.expect("sections present");
    assert_eq!(sections.get("a"), None);
    assert_eq!(sections.get("b"), Some(&Some("B".to_owned())));
    assert_eq!(sections.len(), 1);
    // Removing the only section leaves no map at all (upstream omits the key).
    let messages = vec![
        system_with("", Some(&[("a", Some("A"))]), None, None, 1),
        system_with("", Some(&[("a", None)]), None, None, 2),
    ];
    let message = system_payload(get_current_system_message(&messages));
    assert_eq!(message.sections, None);
}

#[test]
fn current_system_message_timestamp_is_the_first_system_message() {
    let messages = vec![user("hi"), system("prompt", 7), system("later", 99)];
    let message = system_payload(get_current_system_message(&messages));
    assert_eq!(message.timestamp, 7);
}

#[test]
fn current_system_message_carries_replayed_tools_without_removals() {
    let messages = vec![
        system_with("prompt", None, Some(vec![tool("a"), tool("b")]), None, 1),
        system_with("", None, None, Some(vec![tool_ref("a")]), 2),
        system_with("", None, Some(vec![tool("c")]), None, 3),
    ];
    let message = system_payload(get_current_system_message(&messages));
    let tools = message.tools_added.expect("tools present");
    assert_eq!(tool_names(&tools), ["b", "c"]);
    // A collapsed replay is a declaration, not a delta.
    assert_eq!(message.tools_removed, None);
}

#[test]
fn current_system_message_ignores_tool_state_on_non_system_messages() {
    // Only system messages are replayed (upstream `isSystemMessage`); a user
    // payload carrying `toolsAdded` is not a declaration.
    let messages = vec![Message::User(UserMessage {
        role: UserRole::User,
        content: vec![text("hi")],
        tools_added: Some(vec![tool("a")]),
        tools_removed: None,
        sections: None,
        timestamp: 4,
    })];
    assert!(get_current_system_message(&messages).is_none());
}

// ---------------------------------------------------------------------------
// collapse / resolve
// ---------------------------------------------------------------------------

#[test]
fn collapse_keeps_the_replayed_head_and_drops_later_system_messages() {
    let messages = vec![
        system_with("base", None, Some(vec![tool("a")]), None, 1),
        user("hello"),
        system_with("more", None, None, Some(vec![tool_ref("a")]), 2),
        user("again"),
    ];
    let collapsed = collapse_system_messages(TranscriptContext::new(messages));
    assert_eq!(collapsed.as_slice().len(), 3);
    assert!(is_system_message(&collapsed.as_slice()[0]));
    let head = system_payload(Some(collapsed.as_slice()[0].clone()));
    assert_eq!(payload_text(&head), "base\n\nmore");
    assert_eq!(head.timestamp, 1);
    assert_eq!(
        head.tools_added, None,
        "the removed tool must not be declared"
    );
    assert_eq!(collapsed.as_slice()[1], user("hello"));
    assert_eq!(collapsed.as_slice()[2], user("again"));
}

#[test]
fn collapse_without_system_messages_is_a_filter() {
    let messages = vec![user("hello"), user("again")];
    let collapsed = collapse_system_messages(TranscriptContext::new(messages.clone()));
    assert_eq!(collapsed.into_messages(), messages);
}

#[test]
fn collapse_on_an_empty_transcript_is_empty() {
    assert!(collapse_system_messages(TranscriptContext::default())
        .into_messages()
        .is_empty());
}

#[test]
fn resolve_transcript_collapses_unless_mid_convo_system_messages_are_supported() {
    let messages = vec![system("base", 1), system("more", 2), user("hi")];
    let kept = resolve_transcript(TranscriptContext::new(messages.clone()), Some(true));
    assert_eq!(kept.as_slice(), messages.as_slice());
    let collapsed = resolve_transcript(TranscriptContext::new(messages.clone()), Some(false));
    assert_eq!(collapsed.as_slice().len(), 2);
    let defaulted = resolve_transcript(TranscriptContext::new(messages), None);
    assert_eq!(defaulted.as_slice().len(), 2);
}

// ---------------------------------------------------------------------------
// without_initial_system_message / get_initial_system_message
// ---------------------------------------------------------------------------

#[test]
fn initial_system_message_helpers() {
    let messages = vec![system("base", 1), user("hi")];
    assert!(get_initial_system_message(&messages).is_some());
    assert_eq!(without_initial_system_message(&messages), &messages[1..]);
    let no_system = vec![user("hi")];
    assert!(get_initial_system_message(&no_system).is_none());
    assert_eq!(
        without_initial_system_message(&no_system),
        no_system.as_slice()
    );
}

// ---------------------------------------------------------------------------
// Serde
// ---------------------------------------------------------------------------

#[test]
fn system_message_with_sections_round_trips() {
    // pir's in-memory `UserMessage.content` is a block list (upstream's
    // string content is decoded by `StoredSystemMessage` when a session is
    // loaded), so the wire shape here is the block form with `sections`.
    let raw = json!({
        "role": "system",
        "content": [{"type": "text", "text": "prompt"}],
        "sections": {"alpha": "A", "beta": null},
        "timestamp": 12,
    });
    let message: Message = serde_json::from_value(raw.clone()).expect("parses");
    let payload = match &message {
        Message::System(payload) => payload,
        other => panic!("not a system message: {other:?}"),
    };
    let sections = payload.sections.as_ref().expect("sections present");
    assert_eq!(sections.get("alpha"), Some(&Some("A".to_owned())));
    assert_eq!(sections.get("beta"), Some(&None));

    let reserialized = serde_json::to_value(&message).expect("serializes");
    assert_eq!(reserialized, raw);
}

#[test]
fn user_message_without_sections_omits_the_key() {
    let message = Message::User(UserMessage {
        role: UserRole::User,
        content: vec![text("hello")],
        tools_added: None,
        tools_removed: None,
        sections: None,
        timestamp: 1,
    });
    let value = serde_json::to_value(&message).expect("serializes");
    assert!(value.get("sections").is_none(), "unexpected key in {value}");
    let object = value.as_object().expect("object");
    let mut keys: Vec<&str> = object.keys().map(String::as_str).collect();
    keys.sort_unstable();
    assert_eq!(keys, ["content", "role", "timestamp"]);
}

#[test]
fn user_message_with_sections_serializes_text_and_null_values() {
    let mut sections = BTreeMap::new();
    sections.insert("alpha".to_owned(), Some("A".to_owned()));
    sections.insert("beta".to_owned(), None);
    let message = Message::System(UserMessage {
        role: UserRole::System,
        content: vec![text("prompt")],
        tools_added: None,
        tools_removed: None,
        sections: Some(sections),
        timestamp: 2,
    });
    let value = serde_json::to_value(&message).expect("serializes");
    assert_eq!(value["sections"], json!({"alpha": "A", "beta": null}));
    let decoded: Message = serde_json::from_value(value).expect("parses");
    assert_eq!(decoded, message);
}
