//! Startup-input regressions (S2 first-keystroke loss).
//!
//! The composer is live from the first frame, but the worker builds the agent
//! session off-thread and answers with `SessionAttached` later. Keys typed in
//! that window must survive the attach; the trust prompt must stop exactly at
//! Enter and keep the rest of the typed sequence for the TUI.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use pir::tui::app::{settle_attached_editor, SessionTab};
use pir::tui::editor::Editor;
use pir::tui::startup_input::{TrustChoiceInput, TrustChoiceKey, TrustChoiceStep};
use pir::tui::stdin_buffer::{decode_startup_input_bytes, StdinGate};

fn tab(path: &str, title: &str) -> SessionTab {
    SessionTab {
        path: path.to_string(),
        title: title.to_string(),
        draft: String::new(),
    }
}

fn press(character: char) -> Event {
    Event::Key(KeyEvent::new(KeyCode::Char(character), KeyModifiers::empty()))
}

/// Feed a key sequence through the live gate and return the presses in the
/// order the event loop would hand them to `handle_key`.
fn gate_presses(sequence: &str) -> Vec<KeyEvent> {
    let base = Instant::now();
    let mut gate = StdinGate::new(
        Duration::from_millis(50),
        Duration::from_millis(10),
    )
    .with_console_mouse_claim(false);
    let mut queue = VecDeque::new();
    for (offset, character) in sequence.chars().enumerate() {
        queue.extend(gate.push(
            press(character),
            base + Duration::from_millis(offset as u64),
        ));
    }
    queue
        .into_iter()
        .filter_map(|event| match event {
            Event::Key(key) if key.kind == KeyEventKind::Press => Some(key),
            _ => None,
        })
        .collect()
}

#[test]
fn startup_keystrokes_reach_the_composer_before_the_attach_and_survive_it() {
    let editor = Editor::new();
    for key in gate_presses("XYZ") {
        if let KeyCode::Char(character) = key.code {
            editor.insert_char(character);
        }
    }
    assert_eq!(editor.get_text(), "XYZ", "gate order is preserved");

    // The worker's attach reply arrives after those keys were typed. Before
    // the fix this restored the new tab's empty draft and erased them.
    let mut tabs = Vec::new();
    let mut active = None;
    let restored = settle_attached_editor(
        &mut tabs,
        &mut active,
        "/sessions/startup.jsonl",
        "startup",
        &editor.get_text(),
    );
    editor.set_text(&restored);
    assert_eq!(
        editor.get_text(),
        "XYZ",
        "the startup attach must keep keys typed before it"
    );
    assert_eq!(active.as_deref(), Some("/sessions/startup.jsonl"));
}

#[test]
fn attach_during_a_switch_saves_live_text_and_restores_the_target_draft() {
    let mut tabs = vec![
        tab("/sessions/a.jsonl", "A"),
        tab("/sessions/b.jsonl", "B"),
    ];
    let mut active = Some("/sessions/a.jsonl".to_string());
    // The switch request already saved A's draft; more text arrives while the
    // worker opens B.
    tabs[0].draft = "draft for A".to_string();
    tabs[1].draft = "draft for B".to_string();

    let restored = settle_attached_editor(
        &mut tabs,
        &mut active,
        "/sessions/b.jsonl",
        "B",
        "draft for A + typed while attaching",
    );
    assert_eq!(restored, "draft for B", "target draft is restored");
    assert_eq!(
        tabs[0].draft, "draft for A + typed while attaching",
        "text typed during the switch belongs to the outgoing tab"
    );
    assert_eq!(tabs[1].draft, "", "the taken target draft is not duplicated");
}

#[test]
fn trust_choice_accepts_digits_enter_backspace_and_cancel() {
    let mut input = TrustChoiceInput::new(5);
    assert_eq!(
        input.push(TrustChoiceKey::Digit('3')),
        TrustChoiceStep::Pending
    );
    assert_eq!(input.digits(), "3");
    assert_eq!(input.push(TrustChoiceKey::Submit), TrustChoiceStep::Submit(2));

    let mut input = TrustChoiceInput::new(5);
    input.push(TrustChoiceKey::Digit('1'));
    input.push(TrustChoiceKey::Digit('2'));
    input.push(TrustChoiceKey::Backspace);
    assert_eq!(input.digits(), "1");
    assert_eq!(input.push(TrustChoiceKey::Submit), TrustChoiceStep::Submit(0));

    // Empty and out-of-range answers cancel, like the old parse::<usize>()
    // path did.
    assert_eq!(
        TrustChoiceInput::new(5).push(TrustChoiceKey::Submit),
        TrustChoiceStep::Cancel
    );
    let mut input = TrustChoiceInput::new(5);
    input.push(TrustChoiceKey::Digit('9'));
    assert_eq!(input.push(TrustChoiceKey::Submit), TrustChoiceStep::Cancel);
    let mut input = TrustChoiceInput::new(5);
    input.push(TrustChoiceKey::Digit('3'));
    assert_eq!(input.push(TrustChoiceKey::Cancel), TrustChoiceStep::Cancel);

    // Non-digit keys neither corrupt nor submit the buffer.
    let mut input = TrustChoiceInput::new(5);
    input.push(TrustChoiceKey::Digit('3'));
    assert_eq!(
        input.push(TrustChoiceKey::Digit('x')),
        TrustChoiceStep::Pending
    );
    assert_eq!(input.push(TrustChoiceKey::Submit), TrustChoiceStep::Submit(2));
}

#[test]
fn startup_byte_handback_decodes_text_and_sequences_in_order() {
    let events = decode_startup_input_bytes(b"XYZ");
    let chars: Vec<char> = events
        .iter()
        .filter_map(|event| match event {
            Event::Key(key) if key.kind == KeyEventKind::Press => match key.code {
                KeyCode::Char(character) => Some(character),
                _ => None,
            },
            _ => None,
        })
        .collect();
    assert_eq!(chars, vec!['X', 'Y', 'Z']);

    let events = decode_startup_input_bytes(b"\x1b[A");
    assert_eq!(events.len(), 1);
    assert!(matches!(
        &events[0],
        Event::Key(key) if key.code == KeyCode::Up
    ));
}
