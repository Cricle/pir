//! TUI-P5 console batch coalescing regression tests.
//!
//! The Windows console reports a pasted paragraph as one key press + release
//! per character. These tests pin the batch seam that turns that record
//! stream into a small number of `Event::Paste` chunks before the gate and
//! the editor see it.

use std::collections::VecDeque;
use std::time::Duration;

use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use pir::tui::editor::Editor;
use pir::tui::terminal::{ConsoleBatchCoalescer, PrintableBurst};

fn press(code: KeyCode) -> Event {
    Event::Key(KeyEvent::new(code, KeyModifiers::empty()))
}

fn press_mod(code: KeyCode, modifiers: KeyModifiers) -> Event {
    Event::Key(KeyEvent::new(code, modifiers))
}

fn release(code: KeyCode) -> Event {
    let mut key = KeyEvent::new(code, KeyModifiers::empty());
    key.kind = KeyEventKind::Release;
    Event::Key(key)
}

/// The console's per-character record pair.
fn text_records(text: &str) -> Vec<Event> {
    let mut events = Vec::new();
    for c in text.chars() {
        events.push(press(KeyCode::Char(c)));
        events.push(release(KeyCode::Char(c)));
    }
    events
}

/// The winpty/ConPTY spelling of `ESC [ <digits> ~`: crossterm folds the
/// dropped ESC record into an ALT-modified `[`.
fn bracket_marker_alt(digits: &str) -> Vec<Event> {
    let mut events = vec![press_mod(KeyCode::Char('['), KeyModifiers::ALT)];
    for c in digits.chars() {
        events.push(press(KeyCode::Char(c)));
        events.push(release(KeyCode::Char(c)));
    }
    events
}

/// The byte-parser spelling: `Esc`, `[`, then the digits.
fn bracket_marker_byte(digits: &str) -> Vec<Event> {
    let mut events = vec![press(KeyCode::Esc), press(KeyCode::Char('['))];
    for c in digits.chars() {
        events.push(press(KeyCode::Char(c)));
        events.push(release(KeyCode::Char(c)));
    }
    events
}

fn paste_texts(events: &[Event]) -> Vec<String> {
    events
        .iter()
        .filter_map(|event| match event {
            Event::Paste(text) => Some(text.clone()),
            _ => None,
        })
        .collect()
}

#[test]
fn printable_burst_becomes_one_paste_event() {
    let mut coalescer = ConsoleBatchCoalescer::new();
    let events = text_records("K00001 中文 big paste line 1\nK00002 next line");
    let out = coalescer.coalesce(events);
    assert_eq!(out.len(), 1, "one insertion for a printable burst");
    assert_eq!(
        paste_texts(&out),
        vec!["K00001 中文 big paste line 1\nK00002 next line".to_string()]
    );
}

#[test]
fn single_character_stays_a_key() {
    let mut coalescer = ConsoleBatchCoalescer::new();
    let out = coalescer.coalesce(vec![press(KeyCode::Char('a')), release(KeyCode::Char('a'))]);
    assert_eq!(out.len(), 1);
    assert!(matches!(out[0], Event::Key(_)));
}

#[test]
fn control_keys_break_the_run_and_survive() {
    let mut coalescer = ConsoleBatchCoalescer::new();
    let mut events = text_records("abcdefgh");
    events.push(press_mod(KeyCode::Char('c'), KeyModifiers::CONTROL));
    events.push(release(KeyCode::Char('c')));
    events.push(press(KeyCode::Up));
    let out = coalescer.coalesce(events);
    assert_eq!(out.len(), 3, "{out:?}");
    assert!(matches!(&out[0], Event::Paste(text) if text == "abcdefgh"));
    assert!(
        matches!(&out[1], Event::Key(key) if key.code == KeyCode::Char('c') && key.modifiers == KeyModifiers::CONTROL)
    );
    assert!(matches!(&out[2], Event::Key(key) if key.code == KeyCode::Up));
}

#[test]
fn alt_bracket_paste_markers_are_consumed_not_typed() {
    let mut coalescer = ConsoleBatchCoalescer::new();
    let payload = "K00001 中文 line\nK00002 tail";
    let mut events = bracket_marker_alt("200~");
    events.extend(text_records(payload));
    events.extend(bracket_marker_alt("201~"));
    let out = coalescer.coalesce(events);
    assert_eq!(paste_texts(&out), vec![payload.to_string()], "{out:?}");
    assert!(!coalescer.is_paste_open());
}

#[test]
fn byte_parser_paste_markers_are_consumed_not_typed() {
    let mut coalescer = ConsoleBatchCoalescer::new();
    let payload = "first line\nsecond line\n";
    let mut events = bracket_marker_byte("200~");
    events.extend(text_records(payload));
    events.extend(bracket_marker_byte("201~"));
    let out = coalescer.coalesce(events);
    assert_eq!(paste_texts(&out), vec![payload.to_string()], "{out:?}");
}

#[test]
fn a_paste_spanning_batches_keeps_state_and_emits_bounded_chunks() {
    let mut coalescer = ConsoleBatchCoalescer::new();
    let mut first = bracket_marker_alt("200~");
    let chunk = "x".repeat(40 * 1024);
    first.extend(text_records(&chunk));
    let out1 = coalescer.coalesce(first);
    // Below the chunk cap: not visible yet, but the marker was recognized.
    assert!(paste_texts(&out1).is_empty(), "{:?}", paste_texts(&out1));
    assert!(coalescer.is_paste_open());

    let mut second = text_records(&"y".repeat(40 * 1024));
    second.extend(bracket_marker_alt("201~"));
    let out2 = coalescer.coalesce(second);
    let joined = paste_texts(&out2).concat();
    assert_eq!(joined, format!("{}{}", chunk, "y".repeat(40 * 1024)));
    assert!(!coalescer.is_paste_open());
}

#[test]
fn ten_thousand_lines_coalesce_in_bounded_chunks() {
    let payload: String = (1..=10_000)
        .map(|i| format!("K{i:05} 中文 big paste line {i}\n"))
        .collect();
    let mut events = bracket_marker_alt("200~");
    events.extend(text_records(&payload));
    events.extend(bracket_marker_alt("201~"));
    let mut coalescer = ConsoleBatchCoalescer::new();
    let out = coalescer.coalesce(events);
    let pastes = paste_texts(&out);
    assert!(!pastes.is_empty());
    assert!(
        pastes.iter().all(|chunk| chunk.len() <= 64 * 1024),
        "chunks stay bounded: {:?}",
        pastes.iter().map(String::len).collect::<Vec<_>>()
    );
    assert_eq!(pastes.concat(), payload);
}

#[test]
fn printable_burst_flushes_on_the_interval_even_while_input_stays_hot() {
    let mut burst = PrintableBurst::new();
    let mut queue: VecDeque<Event> = "abcdefghi"
        .chars()
        .map(|c| Event::Key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::empty())))
        .collect();
    let start = std::time::Instant::now();
    let blocked = burst.absorb(&mut queue, start, true);
    assert!(!blocked);
    assert!(!burst.should_flush(start, false, true));
    assert!(
        burst.should_flush(start + PrintableBurst::FLUSH_INTERVAL, false, true),
        "a hot trickle must not hold the paste invisible"
    );
    assert!(
        !burst.should_flush(start + Duration::from_millis(1), false, true),
        "the quiet window still coalesces a short run"
    );
}

#[test]
fn paste_text_uses_the_editor_batch_api_for_a_short_multiline_paste() {
    let editor = Editor::new();
    let inserted = editor.paste_text("alpha\nbeta");
    assert_eq!(inserted, "alpha\nbeta");
    assert_eq!(editor.get_text(), "alpha\nbeta");
    assert_eq!(editor.undo_depth(), 1, "one undo snapshot for the paste");
    let stats = editor.io_stats();
    assert_eq!(stats.insert_bursts, 1, "{stats:?}");
    assert_eq!(
        stats.insert_burst_chars,
        "alpha\nbeta".chars().count() as u64
    );
}

/// The console's CRLF spelling: a plain Enter record (CR) followed by the
/// synthesized Ctrl+Enter record (LF). Both halves are ONE newline.
fn ctrl_enter() -> Event {
    press_mod(KeyCode::Enter, KeyModifiers::CONTROL)
}

/// P1-4 (TUI-PERF2): a pasted `a\r\nb` must become `a\nb`, not `a\n\nb`.
#[test]
fn crlf_paste_contributes_one_newline() {
    let mut coalescer = ConsoleBatchCoalescer::new();
    let mut events = text_records("a");
    events.push(press(KeyCode::Enter));
    events.push(ctrl_enter());
    events.extend(text_records("b"));

    let out = coalescer.coalesce(events);
    assert_eq!(
        paste_texts(&out),
        vec!["a\nb".to_string()],
        "CRLF must collapse to one newline: {out:?}"
    );
}

/// A CRLF at the end of a paste: the CR terminates the printable run as a
/// newline (the LF half follows), and the LF chord must be swallowed rather
/// than emitted as a standalone `ctrl+j` key that inserts an extra line.
#[test]
fn trailing_crlf_lf_half_is_swallowed() {
    let mut coalescer = ConsoleBatchCoalescer::new();
    let mut events = text_records("abcdefgh");
    events.push(press(KeyCode::Enter));
    events.push(ctrl_enter());

    let out = coalescer.coalesce(events);
    assert_eq!(paste_texts(&out), vec!["abcdefgh\n".to_string()], "{out:?}");
    assert!(
        !out.iter().any(|event| matches!(
            event,
            Event::Key(key) if key.code == KeyCode::Enter
        )),
        "the LF half must not survive as a key: {out:?}"
    );
}

/// The CRLF pair can split across console batches. The CR must be held at
/// the seam: emitting it immediately submitted the half-paste as a prompt
/// before the LF half (and the rest of the paste) arrived.
#[test]
fn cross_batch_crlf_holds_cr_until_the_lf_half() {
    let mut coalescer = ConsoleBatchCoalescer::new();
    let mut first = text_records("abcdefgh");
    first.push(press(KeyCode::Enter));
    let out = coalescer.coalesce(first);
    assert!(
        !out.iter().any(|event| matches!(
            event,
            Event::Key(key) if key.code == KeyCode::Enter
        )),
        "the final CR must be held, not dispatched as a submit: {out:?}"
    );
    assert!(matches!(out.last(), Some(Event::Paste(text)) if text == "abcdefgh"));

    let second = coalescer.coalesce(vec![ctrl_enter()]);
    assert_eq!(
        paste_texts(&second),
        vec!["\n".to_string()],
        "the CRLF pair must contribute exactly one newline: {second:?}"
    );
    assert!(
        !second.iter().any(|event| matches!(event, Event::Key(_))),
        "neither the held CR nor the LF half may become keys: {second:?}"
    );
}

/// A held batch-final CR followed by ordinary text is a real submit: the CR
/// is dispatched before the next batch's content (not turned into a pasted
/// newline).
#[test]
fn cross_batch_final_cr_then_text_is_a_submit() {
    let mut coalescer = ConsoleBatchCoalescer::new();
    let mut first = text_records("hello world");
    first.push(press(KeyCode::Enter));
    let out = coalescer.coalesce(first);
    assert!(
        matches!(out.last(), Some(Event::Paste(text)) if text == "hello world"),
        "first batch must flush the typed run: {out:?}"
    );

    let second = coalescer.coalesce(text_records("x"));
    assert!(
        matches!(second.first(), Some(Event::Key(key)) if key.code == KeyCode::Enter),
        "the held CR must be dispatched as the submit key first: {second:?}"
    );
    assert!(second
        .iter()
        .any(|event| matches!(event, Event::Key(key) if key.code == KeyCode::Char('x'))));
}
