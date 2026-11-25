//! A v3 session line that omits `timestamp` must still deserialize.
//!
//! These records are read back from JSONL that a user can hand-edit and that
//! older writers produced without the field. A required `timestamp` made the
//! whole entry fail to parse, so a single missing clock reading discarded the
//! transcript content it carried.

use pir_ai_types::{AssistantMessage, DiagnosticInfo, ToolResultMessage, UserMessage};

#[test]
fn user_message_without_timestamp_still_deserializes() {
    let message: UserMessage = serde_json::from_str(r#"{"role":"user","content":"hello"}"#)
        .expect("a user message without `timestamp` must parse");
    assert_eq!(message.timestamp, 0);
}

#[test]
fn assistant_message_without_timestamp_still_deserializes() {
    let message: AssistantMessage = serde_json::from_str(
        r#"{"role":"assistant","api":"openai-completions","provider":"p","model":"m","content":[],"stopReason":"stop","usage":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0,"totalTokens":0,"cost":{"input":0.0,"output":0.0,"cacheRead":0.0,"cacheWrite":0.0,"total":0.0}}}"#,
    )
    .expect("an assistant message without `timestamp` must parse");
    assert_eq!(message.timestamp, 0);
}

#[test]
fn tool_result_without_timestamp_still_deserializes() {
    let message: ToolResultMessage = serde_json::from_str(
        r#"{"role":"toolResult","toolCallId":"c-1","toolName":"read","content":[],"isError":false}"#,
    )
    .expect("a tool result without `timestamp` must parse");
    assert_eq!(message.timestamp, 0);
}

#[test]
fn diagnostic_without_timestamp_still_deserializes() {
    let diagnostic: DiagnosticInfo = serde_json::from_str(r#"{"type":"retry"}"#)
        .expect("a diagnostic without `timestamp` must parse");
    assert_eq!(diagnostic.timestamp, 0);
}

#[test]
fn timestamp_still_round_trips_when_present() {
    let message: UserMessage =
        serde_json::from_str(r#"{"role":"user","content":"hi","timestamp":1729000000000}"#)
            .expect("parse");
    assert_eq!(message.timestamp, 1729000000000);
    let json = serde_json::to_value(&message).expect("serialize");
    assert_eq!(json["timestamp"], 1729000000000u64);
}
