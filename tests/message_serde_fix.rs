//! Regression: `Message` (and `Credential`) JSON round trips.
//!
//! `Message` used to be `#[serde(tag = "role")]` with newtype payloads
//! (user/system/developer/assistant/toolResult) that each carry a `role` field
//! of their own. Serde wrote the key twice on the way out (serde#2949), and on
//! the way back in its `TaggedContentVisitor` took the tag out before the
//! payload was deserialized, so the payload's `role` field was always missing.
//! Every round trip failed — in particular the extension bridge's
//! `JSON.parse`/`JSON.stringify` hop, which made the agent loop's
//! `context`/`context_with_system` message replacement
//! (`src/agent/agent_loop.rs:81` and `:128`) silently ineffective.
//!
//! `Credential` had the same defect: `#[serde(tag = "type")]` over
//! `ApiKeyCredential`, which renames its own `credential_type` field to `type`.

use std::collections::BTreeMap;

use pir::ai::types::{
    ApiKeyCredential, AssistantMessage, AssistantRole, ContentBlock, Context, Cost, Credential,
    CredentialType, DeferredHandle, DiagnosticInfo, ImageContent, Message, NestedToolCallRecord,
    NestedToolCallStatus, NestedToolCalls, ProviderEnv, StopReason, TextContent, ThinkingContent,
    ThinkingLevel, Tool, ToolCall, ToolReference, ToolResultMessage, ToolResultRole, Usage,
    UserMessage, UserRole,
};
use serde_json::{json, Value};

const TIMESTAMP: u64 = 1_700_000_000_000;

/// The duplicate-`role` shape older builds wrote: serde's variant tag first,
/// then the payload's own copy (JSON parsers keep the last one).
const LEGACY_DUPLICATE_ROLE_PAYLOAD: &str = concat!(
    r#"[{"role":"system","role":"system","content":"#,
    r#"[{"type":"text","text":"sys"}],"timestamp":1}]"#
);

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

fn text_block(value: &str) -> ContentBlock {
    ContentBlock::Text(TextContent {
        text: value.to_owned(),
        text_signature: None,
    })
}

fn tool_definition() -> Tool {
    Tool {
        name: "search".to_owned(),
        description: "search things".to_owned(),
        parameters: json!({ "type": "object" }),
        constrained_sampling: None,
    }
}

fn assistant_message() -> AssistantMessage {
    AssistantMessage {
        role: AssistantRole::Assistant,
        content: vec![
            text_block("answer"),
            ContentBlock::Thinking(ThinkingContent {
                thinking: "why".to_owned(),
                thinking_signature: Some("sig".to_owned()),
                redacted: false,
            }),
            ContentBlock::Image(ImageContent {
                data: "aGVsbG8=".to_owned(),
                mime_type: "image/png".to_owned(),
            }),
            ContentBlock::ToolCall(ToolCall {
                id: "call-1".to_owned(),
                name: "search".to_owned(),
                arguments: BTreeMap::from([("city".to_owned(), json!("gz"))]),
                thought_signature: Some("ts".to_owned()),
                namespace: Some("ns".to_owned()),
                arguments_error: None,
            }),
        ],
        api: "openai-responses".to_owned(),
        provider: "openai".to_owned(),
        model: "gpt-x".to_owned(),
        response_model: Some("gpt-x-2026".to_owned()),
        response_id: Some("resp-1".to_owned()),
        diagnostics: vec![DiagnosticInfo {
            diag_type: "retry".to_owned(),
            timestamp: TIMESTAMP,
            error_message: Some("timeout".to_owned()),
            error_name: Some("Timeout".to_owned()),
            error_stack: Some("at retry".to_owned()),
            error_code: Some(json!("ETIMEDOUT")),
            details: Some(BTreeMap::from([("attempt".to_owned(), json!(2))])),
        }],
        usage: Usage {
            input: 5,
            output: 6,
            cache_read: 0,
            cache_write: 0,
            cache_write_1h: Some(1),
            reasoning: Some(2),
            total_tokens: 11,
            cost: Cost::default(),
        },
        stop_reason: StopReason::ToolUse,
        deferred: Some(DeferredHandle {
            provider: "openai".to_owned(),
            model_id: "gpt-x".to_owned(),
            api: "openai-responses".to_owned(),
            id: "batch-1".to_owned(),
            expires_at: Some(TIMESTAMP + 60_000),
            poll_after_ms: Some(500),
            data: Some(json!({ "poll": true })),
        }),
        error_message: Some("partial".to_owned()),
        raw_stop_reason: Some("tool_use".to_owned()),
        end_turn: Some(true),
        provider_thinking_level: Some("high".to_owned()),
        thinking_level: Some(ThinkingLevel::High),
        duration_ms: Some(1234),
        timestamp: TIMESTAMP + 3,
    }
}

fn tool_result_message() -> ToolResultMessage {
    ToolResultMessage {
        role: ToolResultRole::ToolResult,
        tool_call_id: "call-1".to_owned(),
        tool_name: "search".to_owned(),
        content: vec![text_block("result")],
        details: Some(json!({ "cached": false })),
        usage: Some(Usage::default()),
        added_tool_names: vec!["search".to_owned()],
        structured_content: Some(json!({ "ok": true })),
        nested_calls: Some(NestedToolCalls {
            calls: vec![NestedToolCallRecord {
                id: "nested-1".to_owned(),
                name: "inner".to_owned(),
                arguments: Some(BTreeMap::from([("q".to_owned(), json!("x"))])),
                arguments_bytes: None,
                status: NestedToolCallStatus::Ok,
                duration_ms: Some(3),
                error: None,
            }],
            complete: true,
        }),
        is_error: true,
        duration_ms: Some(12),
        timestamp: TIMESTAMP + 4,
    }
}

/// All five variants in order: user, system, developer, assistant, toolResult.
fn all_five() -> Vec<Message> {
    vec![
        Message::User(UserMessage {
            role: UserRole::User,
            content: vec![text_block("hello")],
            tools_added: None,
            tools_removed: Some(vec![ToolReference {
                name: "old_tool".to_owned(),
            }]),
            timestamp: TIMESTAMP,
            sections: None,
        }),
        Message::System(UserMessage {
            role: UserRole::System,
            content: vec![text_block("system text")],
            tools_added: Some(vec![tool_definition()]),
            tools_removed: None,
            timestamp: TIMESTAMP + 1,
            sections: None,
        }),
        Message::Developer(UserMessage {
            role: UserRole::Developer,
            content: vec![text_block("developer text")],
            tools_added: None,
            tools_removed: None,
            timestamp: TIMESTAMP + 2,
            sections: None,
        }),
        Message::Assistant(assistant_message()),
        Message::ToolResult(tool_result_message()),
    ]
}

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

/// The extension bridge hop: `JSON.parse` then `JSON.stringify` keep a single
/// `role` key per message (the last of serde's duplicates).
fn through_the_bridge(text: &str) -> String {
    let parsed: Value = serde_json::from_str(text).expect("JSON.parse");
    serde_json::to_string(&parsed).expect("JSON.stringify")
}

fn parse_messages(text: &str) -> Vec<Message> {
    serde_json::from_str(text).expect("messages parse")
}

fn parse_message(text: &str) -> Message {
    serde_json::from_str(text).expect("message parses")
}

fn from_value_messages(value: Value) -> Vec<Message> {
    serde_json::from_value(value).expect("messages parse from value")
}

fn from_value_message(value: Value) -> Message {
    serde_json::from_value(value).expect("message parses from value")
}

fn message_error(text: &str) -> String {
    let parsed: Result<Message, _> = serde_json::from_str(text);
    parsed.expect_err("message must be rejected").to_string()
}

/// The payload shared by the user, system and developer variants.
fn user_payload(message: &Message) -> &UserMessage {
    match message {
        Message::User(user) | Message::System(user) | Message::Developer(user) => user,
        other => panic!("not a user payload: {other:?}"),
    }
}

fn assistant_payload(message: &Message) -> &AssistantMessage {
    match message {
        Message::Assistant(assistant) => assistant,
        other => panic!("not an assistant message: {other:?}"),
    }
}

fn result_payload(message: &Message) -> &ToolResultMessage {
    match message {
        Message::ToolResult(result) => result,
        other => panic!("not a tool result: {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Serialization shape
// ---------------------------------------------------------------------------

#[test]
fn serialized_messages_carry_exactly_one_role_key() {
    let roles = ["user", "system", "developer", "assistant", "toolResult"];
    for (message, expected_role) in all_five().iter().zip(roles) {
        let text = serde_json::to_string(message).expect("message serializes");
        let role_keys = text.matches("\"role\"").count();
        assert_eq!(role_keys, 1, "duplicate role key in {text}");
        let value: Value = serde_json::from_str(&text).expect("message is JSON");
        assert_eq!(value["role"], json!(expected_role));
        assert_eq!(message.role(), expected_role);
    }
}

#[test]
fn minimal_user_message_keeps_its_original_key_set() {
    let message = Message::User(UserMessage {
        role: UserRole::User,
        content: vec![text_block("hello")],
        tools_added: None,
        tools_removed: None,
        timestamp: TIMESTAMP,
        sections: None,
    });
    let value: Value = serde_json::to_value(&message).expect("serialize");
    let object = value.as_object().expect("object");
    let mut keys: Vec<&str> = object.keys().map(String::as_str).collect();
    keys.sort_unstable();
    assert_eq!(keys, ["content", "role", "timestamp"]);
}

// ---------------------------------------------------------------------------
// Round trips
// ---------------------------------------------------------------------------

#[test]
fn all_variants_round_trip_through_their_own_json() {
    let messages = all_five();
    let text = serde_json::to_string(&messages).expect("serialize");
    let parsed = parse_messages(&text);
    assert_eq!(parsed, messages, "every field must survive");
}

#[test]
fn all_variants_round_trip_through_the_bridge() {
    let messages = all_five();
    let text = serde_json::to_string(&messages).expect("serialize");
    let collapsed = through_the_bridge(&text);
    let role_keys = collapsed.matches("\"role\"").count();
    assert_eq!(role_keys, 5, "one role key per message");
    // The shape `apply_extension_context` parses back (agent_loop.rs:81).
    let parsed = parse_messages(&collapsed);
    assert_eq!(parsed, messages);
}

#[test]
fn rich_messages_keep_every_field() {
    let messages = all_five();
    let text = serde_json::to_string(&messages).expect("serialize");
    let parsed = parse_messages(&text);
    assert_eq!(parsed, messages);

    let user = user_payload(&parsed[0]);
    let removed = user.tools_removed.as_ref().expect("tools removed");
    assert_eq!(removed[0].name, "old_tool");

    let system = user_payload(&parsed[1]);
    assert_eq!(system.role, UserRole::System);
    let added = system.tools_added.as_ref().expect("tools added");
    assert_eq!(added[0].name, "search");

    let assistant = assistant_payload(&parsed[3]);
    assert_eq!(assistant.role, AssistantRole::Assistant);
    assert_eq!(assistant.api, "openai-responses");
    assert_eq!(assistant.provider, "openai");
    assert_eq!(assistant.model, "gpt-x");
    assert_eq!(assistant.response_model.as_deref(), Some("gpt-x-2026"));
    assert_eq!(assistant.response_id.as_deref(), Some("resp-1"));
    assert_eq!(assistant.stop_reason, StopReason::ToolUse);
    assert_eq!(assistant.thinking_level, Some(ThinkingLevel::High));
    assert_eq!(assistant.duration_ms, Some(1234));
    assert_eq!(assistant.end_turn, Some(true));
    assert_eq!(assistant.raw_stop_reason.as_deref(), Some("tool_use"));
    assert_eq!(assistant.error_message.as_deref(), Some("partial"));
    assert_eq!(assistant.usage.reasoning, Some(2));
    assert_eq!(assistant.usage.cache_write_1h, Some(1));
    assert_eq!(assistant.usage.total_tokens, 11);
    assert_eq!(assistant.diagnostics.len(), 1);
    assert_eq!(assistant.diagnostics[0].diag_type, "retry");
    assert_eq!(
        assistant.diagnostics[0].error_stack.as_deref(),
        Some("at retry")
    );
    assert_eq!(
        assistant.diagnostics[0].error_code,
        Some(json!("ETIMEDOUT"))
    );
    let details = assistant.diagnostics[0].details.as_ref().expect("details");
    assert_eq!(details.get("attempt"), Some(&json!(2)));
    let deferred = assistant.deferred.as_ref().expect("deferred handle");
    assert_eq!(deferred.id, "batch-1");
    assert_eq!(deferred.poll_after_ms, Some(500));
    assert_eq!(deferred.data, Some(json!({ "poll": true })));

    let result = result_payload(&parsed[4]);
    assert_eq!(result.role, ToolResultRole::ToolResult);
    assert_eq!(result.tool_call_id, "call-1");
    assert_eq!(result.tool_name, "search");
    assert!(result.is_error);
    assert_eq!(result.added_tool_names, vec!["search".to_owned()]);
    assert_eq!(result.duration_ms, Some(12));
    let structured = result.structured_content.as_ref().expect("structured");
    assert_eq!(structured["ok"], json!(true));
    let nested = result.nested_calls.as_ref().expect("nested calls");
    assert!(nested.complete);
    assert_eq!(nested.calls[0].id, "nested-1");
    assert_eq!(nested.calls[0].status, NestedToolCallStatus::Ok);
}

#[test]
fn context_round_trips_with_its_messages() {
    let context = Context {
        system_prompt: Some("be terse".to_owned()),
        messages: all_five(),
        tools: Some(vec![tool_definition()]),
    };
    let text = serde_json::to_string(&context).expect("serialize");
    let role_keys = text.matches("\"role\"").count();
    assert_eq!(role_keys, 5, "one role key per message");
    let parsed: Context = serde_json::from_str(&text).expect("parse");
    assert_eq!(parsed, context);
}

// ---------------------------------------------------------------------------
// Tag dispatch and role backfill
// ---------------------------------------------------------------------------

#[test]
fn user_system_and_developer_backfill_their_exact_role() {
    // The three variants share one payload struct, so the tag alone decides
    // both the variant and the payload's own `role` field.
    let cases = [
        ("user", UserRole::User, "user"),
        ("system", UserRole::System, "system"),
        ("developer", UserRole::Developer, "developer"),
    ];
    for (tag, role, variant) in cases {
        let value = json!({
            "role": tag,
            "content": [{ "type": "text", "text": tag }],
            "timestamp": 42,
        });
        let message = from_value_message(value);
        let selected = match &message {
            Message::User(_) => "user",
            Message::System(_) => "system",
            Message::Developer(_) => "developer",
            other => panic!("role `{tag}` chose the wrong variant: {other:?}"),
        };
        assert_eq!(selected, variant, "the tag must select the variant");
        let payload = user_payload(&message);
        assert_eq!(payload.role, role);
        assert_eq!(payload.timestamp, 42);
        assert_eq!(payload.content, vec![text_block(tag)]);
        // The backfilled role survives the trip back out with one tag.
        let text = serde_json::to_string(&message).expect("serialize");
        let role_keys = text.matches("\"role\"").count();
        assert_eq!(role_keys, 1);
        let round = parse_message(&text);
        assert_eq!(round, message);
    }
}

#[test]
fn legacy_duplicate_role_payloads_still_parse() {
    let parsed = parse_messages(LEGACY_DUPLICATE_ROLE_PAYLOAD);
    assert_eq!(parsed.len(), 1);
    let user = user_payload(&parsed[0]);
    assert_eq!(user.role, UserRole::System);
    assert_eq!(user.timestamp, 1);
    // ... and the same payload after the bridge collapsed the duplicate key.
    let collapsed = through_the_bridge(LEGACY_DUPLICATE_ROLE_PAYLOAD);
    assert_eq!(parse_messages(&collapsed), parsed);
}

#[test]
fn a_message_without_a_usable_role_is_rejected() {
    let error = message_error(r#"{"role":"custom","content":[],"timestamp":1}"#);
    assert!(error.contains("unknown variant"), "{error}");

    let error = message_error(r#"{"content":[],"timestamp":1}"#);
    assert!(error.contains("`role`"), "{error}");

    let error = message_error("[]");
    assert!(error.contains("JSON object"), "{error}");

    let error = message_error(r#"{"role":7}"#);
    assert!(error.contains("`role`"), "{error}");
}

// ---------------------------------------------------------------------------
// The extension `context` / `context_with_system` replacement path
// ---------------------------------------------------------------------------

#[test]
fn extension_context_replacement_parses_like_the_agent_loop_call() {
    // `apply_extension_context` serializes the transcript into the event
    // payload (agent_loop.rs:74), the bridge hands the handler's array back
    // through JSON, and the loop parses it with `serde_json::from_value`.
    let event = json!({ "type": "context", "messages": all_five() });
    let returned: Value = serde_json::from_str(&event.to_string()).expect("bridge");
    let messages = returned.get("messages").expect("messages").clone();
    let replaced = from_value_messages(messages);
    assert_eq!(replaced, all_five());

    // A handler that edits the array in place (or returns a rebuilt one)
    // replaces the transcript too, including messages it builds by hand.
    let mut handled: Value = serde_json::from_str(&event.to_string()).expect("bridge");
    handled["messages"]
        .as_array_mut()
        .expect("messages")
        .push(json!({
            "role": "developer",
            "content": [{ "type": "text", "text": "runtime note" }],
            "timestamp": 9,
        }));
    let messages = handled.get("messages").expect("messages").clone();
    let replaced = from_value_messages(messages);
    assert_eq!(replaced.len(), 6);
    assert_eq!(replaced[5].role(), "developer");
    let user = user_payload(&replaced[5]);
    assert_eq!(user.role, UserRole::Developer);
    assert_eq!(user.timestamp, 9);
    assert_eq!(user.content, vec![text_block("runtime note")]);
}

#[test]
fn context_with_system_leading_message_survives_the_bridge() {
    // `apply_extension_context_with_system` synthesizes the leading system
    // message (agent_loop.rs:104-116) and splits it back out of the handler's
    // array by variant (agent_loop.rs:132-158), so the variant — not just the
    // `role` string — must survive.
    let leading = Message::System(UserMessage {
        role: UserRole::System,
        content: vec![text_block("system prompt")],
        tools_added: None,
        tools_removed: None,
        timestamp: TIMESTAMP,
        sections: None,
    });
    let mut messages = vec![leading];
    messages.extend(all_five());
    let event = json!({ "type": "context_with_system", "messages": messages });
    let returned: Value = serde_json::from_str(&event.to_string()).expect("bridge");
    let messages = returned.get("messages").expect("messages").clone();
    let replaced = from_value_messages(messages);

    let is_system = matches!(replaced.first(), Some(Message::System(_)));
    assert!(is_system, "leading message: {:?}", replaced.first());
    let user = user_payload(&replaced[0]);
    let prompt = user
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text(text) => Some(text.text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert_eq!(prompt, "system prompt");
    assert_eq!(replaced.len(), 6);
}

// ---------------------------------------------------------------------------
// Credential
// ---------------------------------------------------------------------------

fn api_key_credential(key: Option<&str>) -> Credential {
    Credential::ApiKey(ApiKeyCredential {
        credential_type: CredentialType::ApiKey,
        key: key.map(str::to_owned),
        env: Some(ProviderEnv::from([("REGION".to_owned(), "eu".to_owned())])),
    })
}

fn parse_credential(text: &str) -> Credential {
    serde_json::from_str(text).expect("credential parses")
}

fn credential_error(text: &str) -> String {
    let parsed: Result<Credential, _> = serde_json::from_str(text);
    parsed.expect_err("credential must be rejected").to_string()
}

#[test]
fn credentials_serialize_with_one_type_key_and_round_trip() {
    let credential = api_key_credential(Some("sk-test"));
    let text = serde_json::to_string(&credential).expect("serialize");
    let type_keys = text.matches("\"type\"").count();
    assert_eq!(type_keys, 1, "duplicate type key in {text}");
    let value: Value = serde_json::from_str(&text).expect("json");
    assert_eq!(value["type"], json!("api_key"));

    let parsed = parse_credential(&text);
    let Credential::ApiKey(payload) = &parsed;
    assert_eq!(payload.credential_type, CredentialType::ApiKey);
    assert_eq!(payload.key.as_deref(), Some("sk-test"));
    let env = payload.env.as_ref().expect("credential env");
    assert_eq!(env.get("REGION").map(String::as_str), Some("eu"));

    // A key-less credential still carries its tag (and round-trips).
    let credential = api_key_credential(None);
    let text = serde_json::to_string(&credential).expect("serialize");
    let parsed = parse_credential(&text);
    let Credential::ApiKey(payload) = &parsed;
    assert!(payload.key.is_none());
}

#[test]
fn bridge_shaped_credentials_parse() {
    // The single-key form the bridge hands back (what the fix produces).
    let parsed = parse_credential(r#"{"type":"api_key","key":"sk-bridge"}"#);
    let Credential::ApiKey(payload) = &parsed;
    assert_eq!(payload.credential_type, CredentialType::ApiKey);
    assert_eq!(payload.key.as_deref(), Some("sk-bridge"));

    // The duplicate-key shape older builds wrote.
    let legacy = r#"{"type":"api_key","key":"sk-legacy","type":"api_key"}"#;
    let parsed = parse_credential(legacy);
    let Credential::ApiKey(payload) = &parsed;
    assert_eq!(payload.key.as_deref(), Some("sk-legacy"));

    // `key: null` (upstream's absent key) stays representable.
    let parsed = parse_credential(r#"{"type":"api_key","key":null}"#);
    let Credential::ApiKey(payload) = &parsed;
    assert!(payload.key.is_none());

    // Unknown credential types are still rejected with the tag list.
    let error = credential_error(r#"{"type":"oauth","accessToken":"x"}"#);
    assert!(error.contains("unknown variant"), "{error}");

    let error = credential_error(r#"{"key":"sk"}"#);
    assert!(error.contains("`type`"), "{error}");
}
