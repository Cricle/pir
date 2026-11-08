//! TUI-P5 console batch coalescing regression tests.
//!
//! The Windows console reports a pasted paragraph as one key press + release
//! per character. These tests pin the batch seam that turns that record
//! stream into a small number of `Event::Paste` chunks before the gate and
//! the editor see it.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use pir::tui::editor::Editor;
use pir::tui::stdin_buffer::{decode_vt_input_bytes, stdin_events_to_events, StdinBuffer};
use pir::tui::terminal::{
    decode_console_vt_batch, ConsoleBatchCoalescer, ConsoleBatchRecord, PrintableBurst,
};

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

/// A23-F01 (record path): an end marker inside the payload is followed by
/// more payload in the same batch, so it is literal text, not a terminator.
#[test]
fn inner_end_marker_stays_literal_payload() {
    let mut coalescer = ConsoleBatchCoalescer::new();
    let mut events = bracket_marker_byte("200~");
    events.extend(text_records("AAA"));
    events.extend(bracket_marker_byte("201~"));
    events.extend(text_records("BBB"));
    events.extend(bracket_marker_byte("201~"));
    let out = coalescer.coalesce(events);
    assert_eq!(
        paste_texts(&out),
        vec!["AAA[201~BBB".to_string()],
        "{out:?}"
    );
    assert!(!coalescer.is_paste_open());
}

#[test]
fn two_inner_end_markers_stay_literal_payload() {
    let mut coalescer = ConsoleBatchCoalescer::new();
    let mut events = bracket_marker_byte("200~");
    events.extend(text_records("A"));
    events.extend(bracket_marker_byte("201~"));
    events.extend(text_records("B"));
    events.extend(bracket_marker_byte("201~"));
    events.extend(text_records("C"));
    events.extend(bracket_marker_byte("201~"));
    let out = coalescer.coalesce(events);
    assert_eq!(
        paste_texts(&out),
        vec!["A[201~B[201~C".to_string()],
        "{out:?}"
    );
    assert!(!coalescer.is_paste_open());
}

/// An inner marker in one batch is followed by more payload, so it stays
/// literal across the batch seam; only the next batch's final marker closes.
#[test]
fn inner_end_marker_across_batches_stays_literal() {
    let mut coalescer = ConsoleBatchCoalescer::new();
    let mut first = bracket_marker_byte("200~");
    first.extend(text_records("AA"));
    assert!(paste_texts(&coalescer.coalesce(first)).is_empty());

    let mut second = bracket_marker_byte("201~");
    second.extend(text_records("BB"));
    let out = coalescer.coalesce(second);
    assert!(paste_texts(&out).is_empty(), "{out:?}");
    assert!(coalescer.is_paste_open());

    let out = coalescer.coalesce(bracket_marker_byte("201~"));
    assert_eq!(paste_texts(&out), vec!["AA[201~BB".to_string()], "{out:?}");
    assert!(!coalescer.is_paste_open());
}

/// A paste whose end marker is the last event still terminates, and an Enter
/// that follows it in the same batch is dispatched as a submit key rather
/// than swallowed by the paste.
#[test]
fn paste_end_followed_by_enter_dispatches_enter() {
    let mut coalescer = ConsoleBatchCoalescer::new();
    let mut events = bracket_marker_byte("200~");
    events.extend(text_records("PASTE"));
    events.extend(bracket_marker_byte("201~"));
    events.push(press(KeyCode::Enter));
    let out = coalescer.coalesce(events);
    assert_eq!(paste_texts(&out), vec!["PASTE".to_string()], "{out:?}");
    assert!(matches!(
        out.last(),
        Some(Event::Key(key)) if key.code == KeyCode::Enter
    ));
    assert!(!coalescer.is_paste_open());
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

// ---------------------------------------------------------------------------
// Windows VT input mode (`ENABLE_VIRTUAL_TERMINAL_INPUT`): the console queues
// the terminal's raw byte stream, one UTF-16 unit per KEY_EVENT record.
// ---------------------------------------------------------------------------

/// Console records for one raw VT byte run, as the VT-mode console reports
/// them: each byte is a key-down record with `u_char = byte` and `VK 0`.
fn vt_records(bytes: &[u8]) -> Vec<ConsoleBatchRecord> {
    bytes
        .iter()
        .map(|byte| ConsoleBatchRecord::Key {
            key_down: true,
            virtual_key_code: 0,
            u_char: u16::from(*byte),
            modifiers: KeyModifiers::empty(),
        })
        .collect()
}

fn vt_text(events: &[Event]) -> Vec<String> {
    events
        .iter()
        .filter_map(|event| match event {
            Event::Paste(text) => Some(text.clone()),
            _ => None,
        })
        .collect()
}

/// A lone ESC byte must decode to the Escape key (dialogs close), not the
/// `ctrl+{` chord the generic C0 mapping would produce.
#[test]
fn vt_lone_escape_is_the_escape_key() {
    let events = decode_console_vt_batch(&vt_records(b"\x1b"));
    assert!(
        matches!(events.as_slice(), [Event::Key(key)] if key.code == KeyCode::Esc),
        "{events:?}"
    );
}

/// QA repro: `ESC[200~AAA\x03BBB ESC[201~`. In VT mode the markers and the
/// literal `\x03` reach the byte buffer; the paste stays ONE event, so the
/// control byte can never dispatch Clear and delete the text before it.
#[test]
fn vt_paste_ctrl_c_is_one_paste_and_cleans_to_plain_text() {
    let events = decode_console_vt_batch(&vt_records(b"\x1b[200~AAA\x03BBB\x1b[201~"));
    assert_eq!(
        vt_text(&events),
        vec!["AAA\u{3}BBB".to_string()],
        "{events:?}"
    );
    assert!(
        !events.iter().any(|event| matches!(event, Event::Key(_))),
        "no key record may escape a bracketed paste: {events:?}"
    );
    let editor = Editor::new();
    assert_eq!(editor.paste_text("AAA\u{3}BBB"), "AAABBB");
}

/// The minimal exit repro `ESC[200~\x03\x04 ESC[201~`: Ctrl+C/Ctrl+D inside
/// the paste are payload (and are stripped by the editor), never
/// Clear/Exit actions.
#[test]
fn vt_paste_ctrl_c_ctrl_d_does_not_dispatch_clear_or_exit() {
    let events = decode_console_vt_batch(&vt_records(b"\x1b[200~\x03\x04\x1b[201~"));
    assert_eq!(
        vt_text(&events),
        vec!["\u{3}\u{4}".to_string()],
        "{events:?}"
    );
    assert!(
        !events.iter().any(|event| matches!(event, Event::Key(_))),
        "Ctrl+C/Ctrl+D inside a paste must not become keys: {events:?}"
    );
    let editor = Editor::new();
    assert_eq!(editor.paste_text("\u{3}\u{4}"), "");
}

/// Multiline paste: line breaks survive, the interior Ctrl+C is stripped,
/// and none of LINE1/AAA is lost.
#[test]
fn vt_multiline_paste_keeps_every_line() {
    let events = decode_console_vt_batch(&vt_records(
        b"\x1b[200~LINE1\r\nAAA\x03BBB\r\nLINE2\x1b[201~",
    ));
    assert_eq!(
        vt_text(&events),
        vec!["LINE1\r\nAAA\u{3}BBB\r\nLINE2".to_string()],
        "{events:?}"
    );
    let editor = Editor::new();
    assert_eq!(
        editor.paste_text("LINE1\r\nAAA\u{3}BBB\r\nLINE2"),
        "LINE1\nAAABBB\nLINE2"
    );
}

/// Text typed after the closing marker in a LATER console read is normal
/// input, not paste payload. (Bytes after a marker in the same read are
/// ambiguous with an embedded marker and stay payload — the A23-F01 rule.)
#[test]
fn vt_text_after_paste_marker_dispatches_as_keys() {
    let now = Instant::now();
    let mut buffer = StdinBuffer::default();
    let mut sequences = buffer.process(b"\x1b[200~PASTE\x1b[201~", now);
    sequences.extend(buffer.process(b"TAIL", now));
    let events = stdin_events_to_events(sequences);
    assert_eq!(vt_text(&events), vec!["PASTE".to_string()], "{events:?}");
    let tail: String = events
        .iter()
        .filter_map(|event| match event {
            Event::Key(key) if key.kind == KeyEventKind::Press => match key.code {
                KeyCode::Char(c) => Some(c),
                _ => None,
            },
            _ => None,
        })
        .collect();
    assert_eq!(tail, "TAIL");
}

/// A genuine Ctrl+C with no paste marker (typed after idle) stays a real
/// Ctrl+C key: Clear/abort semantics are preserved.
#[test]
fn vt_lone_ctrl_c_is_a_real_control_key() {
    let events = decode_console_vt_batch(&vt_records(b"\x03"));
    assert_eq!(events.len(), 1, "{events:?}");
    assert!(
        matches!(
            &events[0],
            Event::Key(key)
                if key.code == KeyCode::Char('c')
                    && key.modifiers == KeyModifiers::CONTROL
                    && key.kind == KeyEventKind::Press
        ),
        "a typed Ctrl+C must stay a control key: {events:?}"
    );
    assert!(vt_text(&events).is_empty());

    // Typing "AAA" then Ctrl+C is not a paste either: the control key still
    // arrives as its own event after the three characters.
    let events = decode_console_vt_batch(&vt_records(b"AAA\x03"));
    assert_eq!(events.len(), 4, "{events:?}");
    assert!(events.last().is_some_and(|event| matches!(
        event,
        Event::Key(key)
            if key.code == KeyCode::Char('c') && key.modifiers == KeyModifiers::CONTROL
    )));
}

/// A paste split across console reads keeps its payload: the byte buffer is
/// stateful across chunks, exactly like the live reader's. The end marker is
/// split too; only the chunk-final completion closes the paste, and text in
/// the next read is ordinary input.
#[test]
fn vt_paste_markers_may_straddle_console_reads() {
    let now = Instant::now();
    let mut buffer = StdinBuffer::default();
    let mut sequences = buffer.process(b"\x1b[200~AAA", now);
    sequences.extend(buffer.process(b"\x03BBB\x1b[20", now));
    sequences.extend(buffer.process(b"1~", now));
    sequences.extend(buffer.process(b"TAILMARK", now));
    let events = stdin_events_to_events(sequences);
    assert_eq!(
        vt_text(&events),
        vec!["AAA\u{3}BBB".to_string()],
        "{events:?}"
    );
    let tail: String = events
        .iter()
        .filter_map(|event| match event {
            Event::Key(key) if key.kind == KeyEventKind::Press => match key.code {
                KeyCode::Char(c) => Some(c),
                _ => None,
            },
            _ => None,
        })
        .collect();
    assert_eq!(tail, "TAILMARK");
}

/// The one-shot seam used by the live reader produces the same shapes.
#[test]
fn vt_input_bytes_one_shot_matches_the_live_decoder() {
    assert_eq!(
        decode_vt_input_bytes(b"\x1b[200~AAA\x03BBB\x1b[201~"),
        decode_console_vt_batch(&vt_records(b"\x1b[200~AAA\x03BBB\x1b[201~"))
    );
}

/// Non-BMP characters arrive as a surrogate pair in VT mode; the pair must
/// decode to the emoji and be part of the paste payload.
#[test]
fn vt_paste_pairs_surrogate_characters() {
    let mut records = vt_records(b"\x1b[200~A");
    let emoji = "😀";
    let mut units = emoji.encode_utf16();
    let high = units.next().expect("high surrogate");
    let low = units.next().expect("low surrogate");
    // ConPTY reports both halves as key-up records (probed 2026-11-07).
    records.push(ConsoleBatchRecord::Key {
        key_down: false,
        virtual_key_code: 0,
        u_char: high,
        modifiers: KeyModifiers::empty(),
    });
    records.push(ConsoleBatchRecord::Key {
        key_down: false,
        virtual_key_code: 0,
        u_char: low,
        modifiers: KeyModifiers::empty(),
    });
    records.extend(vt_records(b"B\x1b[201~"));
    let events = decode_console_vt_batch(&records);
    assert_eq!(vt_text(&events), vec!["A😀B".to_string()], "{events:?}");
}
