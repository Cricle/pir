//! Reducer suites for the headless Native Shell bridge, moved out of
//! `src/gui_core.rs`.
//!
//! Source hygiene forbids inline `#[cfg(test)]` modules in `src/`. These drive
//! only the public bridge surface (`describe` → `reduce` → `GuiState`), which
//! is the contract a new front end consumes.

use pir_core::sdk::SdkEvent;
use pir_native_shell::gui_core::{GuiCodec, GuiCommand, GuiMessage, GuiModelRef, GuiState};
use pir_native_shell::NativeShellBridge;

#[test]
fn reducer_builds_streaming_assistant_message() {
    let mut state = GuiState::default();
    NativeShellBridge::reduce(&mut state, SdkEvent::MessageStart);
    NativeShellBridge::reduce(
        &mut state,
        SdkEvent::MessageUpdate {
            delta: "hello".to_owned(),
            thinking: false,
        },
    );
    NativeShellBridge::reduce(
        &mut state,
        SdkEvent::MessageEnd {
            message: pir_core::ai::types::AssistantMessage::default(),
        },
    );
    assert_eq!(state.messages[0].text, "hello");
    assert!(!state.messages[0].streaming);
}

#[test]
fn reducer_accumulates_multiple_deltas_and_keeps_prior_messages() {
    let mut state = GuiState {
        model: Some(GuiModelRef {
            provider: "test".to_owned(),
            id: "model".to_owned(),
        }),
        messages: vec![GuiMessage {
            role: "user".to_owned(),
            text: "question".to_owned(),
            streaming: false,
            thinking: false,
        }],
        ..GuiState::default()
    };
    NativeShellBridge::reduce(&mut state, SdkEvent::MessageStart);
    for delta in ["hel", "lo ", "world"] {
        NativeShellBridge::reduce(
            &mut state,
            SdkEvent::MessageUpdate {
                delta: delta.to_owned(),
                thinking: false,
            },
        );
    }
    assert_eq!(state.messages[0].text, "question");
    assert_eq!(state.messages[1].text, "hello world");
    assert_eq!(state.model.as_ref().unwrap().id, "model");
}

#[test]
fn settlement_clears_running_but_outcome_does_not() {
    let mut state = GuiState {
        running: true,
        ..GuiState::default()
    };
    assert!(state.running);
    NativeShellBridge::reduce(&mut state, SdkEvent::AgentSettled { aborted: true });
    assert!(!state.running);
}

#[test]
fn rejection_is_visible_and_stops_running() {
    let mut state = GuiState {
        running: true,
        ..GuiState::default()
    };
    NativeShellBridge::reduce(
        &mut state,
        SdkEvent::PromptRejected {
            message: "bad prompt".to_owned(),
        },
    );
    assert_eq!(state.error.as_deref(), Some("bad prompt"));
    assert!(!state.running);
}

#[test]
fn command_codec_is_stable() {
    let command = GuiCommand::Submit {
        text: "hello".to_owned(),
    };
    let value = serde_json::to_value(&command).unwrap();
    assert_eq!(value["type"], "submit");
    assert_eq!(GuiCodec::decode_command(&value).unwrap(), command);
}
