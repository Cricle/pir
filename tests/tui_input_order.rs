//! TUI input-order regressions: the printable burst collector must preserve
//! the physical order of keys and pastes.
//!
//! The ConPTY defect these tests pin down: `event::poll` pre-reads the first
//! console record into crossterm's internal queue, while the batch reader
//! drains the records behind it directly. If that first record is delivered
//! after the batch, the first character of a run lands at the end
//! (`hello` -> `elloh`). `PrintableBurst` must never reorder the events it is
//! fed; these tests exercise the collector (and the batch seam in front of it)
//! with that exact shape.

use std::collections::VecDeque;
use std::sync::Mutex;
use std::time::Instant;

use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use pir::tui::autocomplete::{AutocompletePopup, CombinedAutocompleteProvider};
use pir::tui::editor::Editor;
use pir::tui::terminal::{accept_autocomplete_selection, ConsoleBatchCoalescer, PrintableBurst};

fn press(c: char) -> Event {
    Event::Key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::empty()))
}

fn enter() -> Event {
    Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::empty()))
}

fn release(c: char) -> Event {
    let mut key = KeyEvent::new(KeyCode::Char(c), KeyModifiers::empty());
    key.kind = KeyEventKind::Release;
    Event::Key(key)
}

fn paste(text: &str) -> Event {
    Event::Paste(text.to_string())
}

fn paste_texts(events: &VecDeque<Event>) -> Vec<String> {
    events
        .iter()
        .filter_map(|event| match event {
            Event::Paste(text) => Some(text.clone()),
            _ => None,
        })
        .collect()
}

/// One Key followed by a Paste must stay `key + paste`, not `paste + key`.
#[test]
fn single_key_then_paste_keeps_key_before_paste() {
    let mut queue: VecDeque<Event> = VecDeque::new();
    queue.push_back(press('h'));
    queue.push_back(paste("ello"));

    let mut burst = PrintableBurst::new();
    let blocked = burst.absorb(&mut queue, Instant::now(), true);
    assert!(!blocked, "a printable key + paste must be absorbable");
    assert!(queue.is_empty(), "the whole run is consumed: {queue:?}");

    let mut out = VecDeque::new();
    burst.flush_now(&mut out);
    assert_eq!(paste_texts(&out), vec!["hello".to_string()], "{out:?}");
}

/// A pure Paste is delivered as one Paste with its bytes untouched.
#[test]
fn pure_paste_is_one_unchanged_paste() {
    let mut queue: VecDeque<Event> = VecDeque::from([paste("paste-small-123")]);
    let mut burst = PrintableBurst::new();
    assert!(!burst.absorb(&mut queue, Instant::now(), true));

    let mut out = VecDeque::new();
    burst.flush_now(&mut out);
    assert_eq!(
        paste_texts(&out),
        vec!["paste-small-123".to_string()],
        "{out:?}"
    );
}

/// A multi-key run flushes as one Paste in the same character order.
#[test]
fn multi_key_run_keeps_character_order() {
    let mut queue: VecDeque<Event> = "hello".chars().map(press).collect();
    let mut burst = PrintableBurst::new();
    assert!(!burst.absorb(&mut queue, Instant::now(), true));

    let mut out = VecDeque::new();
    burst.flush_now(&mut out);
    assert_eq!(paste_texts(&out), vec!["hello".to_string()], "{out:?}");
}

/// The batch seam: the first console record is consumed by `event::poll`
/// before the rest is drained directly. Feeding it first keeps `hello`
/// intact; feeding it last is the pre-fix `elloh` rotation.
#[test]
fn split_batch_first_record_stays_first_through_burst() {
    let mut coalescer = ConsoleBatchCoalescer::new();

    // `event::poll` already returned the 'h' press.
    let first = coalescer.coalesce(vec![press('h')]);
    // The direct batch drain sees the remaining records: 'h' release, then
    // 'e'/'l'/'l'/'o' press-release pairs (all below the batch paste
    // threshold, so they stay keys).
    let mut rest = vec![release('h')];
    for c in "ello".chars() {
        rest.push(press(c));
        rest.push(release(c));
    }
    let batch = coalescer.coalesce(rest);

    let mut queue: VecDeque<Event> = first.into_iter().collect();
    queue.extend(batch);
    let mut burst = PrintableBurst::new();
    assert!(!burst.absorb(&mut queue, Instant::now(), true));
    let mut out = VecDeque::new();
    burst.flush_now(&mut out);
    assert_eq!(paste_texts(&out), vec!["hello".to_string()], "{out:?}");

    // The broken order (first record after the batch) really does rotate the
    // text, so this test fails if the seam regresses to that shape.
    let mut rotated: VecDeque<Event> = VecDeque::new();
    for c in "ello".chars() {
        rotated.push_back(press(c));
    }
    rotated.push_back(press('h'));
    let mut burst = PrintableBurst::new();
    assert!(!burst.absorb(&mut rotated, Instant::now(), true));
    let mut out = VecDeque::new();
    burst.flush_now(&mut out);
    assert_eq!(paste_texts(&out), vec!["elloh".to_string()], "{out:?}");
}

/// A lone Enter batch (the paste has already been processed, the user presses
/// Enter) must stay a Key event; `Paste("\n")` only inserts a line break.
#[test]
fn lone_enter_stays_a_key() {
    let mut coalescer = ConsoleBatchCoalescer::new();
    let mut release = KeyEvent::new(KeyCode::Enter, KeyModifiers::empty());
    release.kind = KeyEventKind::Release;

    let out = coalescer.coalesce(vec![enter(), Event::Key(release)]);

    assert_eq!(out.len(), 1, "release must be dropped: {out:?}");
    match &out[0] {
        Event::Key(key) => {
            assert_eq!(key.code, KeyCode::Enter, "{out:?}");
            assert_eq!(key.kind, KeyEventKind::Press, "{out:?}");
        }
        other => panic!("lone Enter must stay a Key event, got {other:?}"),
    }
}

/// BUILD5 regression: a typed run whose batch ends with Enter must leave the
/// Enter as its own key event. Folding it into the run yields
/// `Paste("abcdef\n")`, which the editor inserts as a newline instead of
/// submitting the prompt.
#[test]
fn batch_key_run_then_final_enter_submits() {
    let mut coalescer = ConsoleBatchCoalescer::new();
    let mut batch: Vec<Event> = "abcdef".chars().map(press).collect();
    batch.push(enter());

    let out = coalescer.coalesce(batch);

    // The final CR is held at the batch seam (it may be the CR half of a
    // CRLF split across batches), but its deadline releases it as the submit
    // key.
    assert!(
        !out.iter().any(|event| matches!(
            event,
            Event::Key(key) if key.code == KeyCode::Enter
        )),
        "the final CR must be held, not folded into the paste: {out:?}"
    );
    let deadline = coalescer
        .pending_enter_deadline()
        .expect("held CR must publish a wake-up deadline");
    let released = coalescer.poll_pending_enter(deadline);
    match released.as_slice() {
        [Event::Key(key)] => {
            assert_eq!(key.code, KeyCode::Enter, "released: {released:?}");
            assert_eq!(key.kind, KeyEventKind::Press, "released: {released:?}");
        }
        other => panic!("final Enter must be released as a Key event: {other:?}"),
    }
    assert!(
        !out.iter()
            .any(|event| matches!(event, Event::Paste(text) if text.contains('\n'))),
        "the final Enter must not be folded into paste payload: {out:?}"
    );

    // Nothing before the Enter is lost or reordered. The Enter itself is the
    // separately released submit key above, so it contributes no text here.
    let mut typed = String::new();
    for event in &out {
        match event {
            Event::Paste(text) => typed.push_str(text),
            Event::Key(key) if key.kind == KeyEventKind::Press => match key.code {
                KeyCode::Char(c) => typed.push(c),
                KeyCode::Enter => typed.push('\n'),
                _ => {}
            },
            _ => {}
        }
    }
    assert_eq!(typed, "abcdef", "{out:?}");
}

/// An Enter with more payload after it is a newline inside a multiline paste:
/// the existing coalescing must still turn the whole run into one Paste.
#[test]
fn multiline_paste_keeps_internal_newlines() {
    let mut coalescer = ConsoleBatchCoalescer::new();
    let mut batch: Vec<Event> = vec![press('a')];
    batch.push(enter());
    batch.extend("bc".chars().map(press));

    let out = coalescer.coalesce(batch);

    assert_eq!(out.len(), 1, "one multiline paste expected: {out:?}");
    match &out[0] {
        Event::Paste(text) => assert_eq!(text, "a\nbc", "{out:?}"),
        other => panic!("expected Paste with internal newline, got {other:?}"),
    }
}

/// Inside a bracketed paste the Enter is payload: `paste_open` bypasses the
/// final-Enter split and the newline stays in the paste text.
#[test]
fn bracketed_paste_enter_stays_payload() {
    let mut coalescer = ConsoleBatchCoalescer::new();
    let batch = vec![
        Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::empty())),
        press('['),
        press('2'),
        press('0'),
        press('0'),
        press('~'),
        press('a'),
        enter(),
        press('b'),
        Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::empty())),
        press('['),
        press('2'),
        press('0'),
        press('1'),
        press('~'),
    ];

    let out = coalescer.coalesce(batch);

    assert!(
        !coalescer.is_paste_open(),
        "end marker must close the paste"
    );
    assert!(
        out.iter()
            .any(|event| matches!(event, Event::Paste(text) if text == "a\nb")),
        "Enter inside a bracketed paste must remain paste payload: {out:?}"
    );
    assert!(
        !out.iter()
            .any(|event| matches!(event, Event::Key(key) if key.code == KeyCode::Enter)),
        "no standalone Enter may escape a bracketed paste: {out:?}"
    );
}

fn esc() -> Event {
    Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::empty()))
}

/// BUILD8 SLASH-ESC repro A: typing `/`, pausing (the lone key is replayed as
/// a Key and the slash menu opens), then typing `quit` as one fast run must
/// leave the editor at `/quit` — not `//quit`. Enter then accepts the `quit`
/// row and falls through to submit `/quit`.
#[test]
fn slash_then_quit_burst_keeps_one_slash_and_enter_submits_it() {
    // First run: the lone `/` press is flushed after the quiet window.
    let mut burst = PrintableBurst::new();
    let mut queue: VecDeque<Event> = VecDeque::from([press('/')]);
    assert!(!burst.absorb(&mut queue, Instant::now(), true));
    let mut first = VecDeque::new();
    burst.flush_now(&mut first);
    assert!(
        matches!(first.front(), Some(Event::Key(key)) if key.code == KeyCode::Char('/')),
        "a lone press must replay as its key: {first:?}"
    );
    let mut events: VecDeque<Event> = first;

    // Second run: `quit` arrives as a multi-press burst. The replayed `/` must
    // NOT still be in the collector text (the pre-fix `//quit` bug).
    let mut queue: VecDeque<Event> = "quit".chars().map(press).collect();
    assert!(!burst.absorb(&mut queue, Instant::now(), true));
    let mut second = VecDeque::new();
    burst.flush_now(&mut second);
    events.extend(second);
    assert_eq!(
        paste_texts(&events),
        vec!["quit".to_string()],
        "the replayed single key must not leak into the next burst: {events:?}"
    );

    // Replay the exact event stream into an editor, like the input loop.
    let editor = Editor::new();
    for event in events {
        match event {
            Event::Key(key) if key.kind == KeyEventKind::Press => {
                if let KeyCode::Char(c) = key.code {
                    editor.insert_char(c);
                }
            }
            Event::Paste(text) => {
                editor.paste_text(&text);
            }
            _ => {}
        }
    }
    assert_eq!(editor.get_text(), "/quit", "composer buffer");

    // Menu open on `/quit`: Enter accepts the highlighted `quit` row; the
    // accepted prefix is a slash command, so the submit path runs `/quit`.
    let provider =
        CombinedAutocompleteProvider::with_builtin_commands(std::env::current_dir().expect("cwd"));
    let state = editor.state_snapshot();
    let suggestions = provider
        .suggestions(&state.lines, state.cursor_line, state.cursor_col, false)
        .expect("the slash menu is open");
    let mut popup = AutocompletePopup::new(suggestions);
    popup.selected = popup
        .suggestions
        .items
        .iter()
        .position(|item| item.value == "quit")
        .expect("the `quit` row is offered");
    let slot = Mutex::new(Some(popup));
    let accepted = accept_autocomplete_selection(&slot, &provider, &editor)
        .expect("Enter accepts the selected row");
    assert_eq!(accepted, "/quit");
    assert_eq!(editor.get_text().trim_end(), "/quit");
    assert!(slot.lock().unwrap().is_none(), "accept closes the popup");
}

/// BUILD8 SLASH-ESC repro B: a lone bare Esc must be dispatched immediately
/// (after the input gate's escape window), not held as a possible split
/// bracketed-paste start until the NEXT console batch.
#[test]
fn lone_bare_escape_is_dispatched_as_a_key() {
    let mut coalescer = ConsoleBatchCoalescer::new();
    let mut release = KeyEvent::new(KeyCode::Esc, KeyModifiers::empty());
    release.kind = KeyEventKind::Release;

    let out = coalescer.coalesce(vec![esc(), Event::Key(release)]);
    assert_eq!(
        out.len(),
        1,
        "a lone Esc batch must emit one key, not be withheld: {out:?}"
    );
    match &out[0] {
        Event::Key(key) => {
            assert_eq!(key.code, KeyCode::Esc);
            assert_eq!(key.kind, KeyEventKind::Press);
        }
        other => panic!("lone Esc must stay a Key event, got {other:?}"),
    }
    assert!(!coalescer.is_paste_open());
}

/// While a bracketed paste is open, a lone Esc may be the first half of the
/// `ESC[201~` end marker: it must still be held for the next batch.
#[test]
fn split_bracketed_paste_end_marker_is_still_held() {
    let mut coalescer = ConsoleBatchCoalescer::new();
    let start = vec![
        esc(),
        press('['),
        press('2'),
        press('0'),
        press('0'),
        press('~'),
        press('a'),
    ];
    let out = coalescer.coalesce(start);
    assert!(coalescer.is_paste_open(), "{out:?}");

    // The console splits the end marker after its ESC.
    let held = coalescer.coalesce(vec![esc()]);
    assert!(held.is_empty(), "the Esc half must be withheld: {held:?}");
    let tail = coalescer.coalesce(vec![
        press('['),
        press('2'),
        press('0'),
        press('1'),
        press('~'),
    ]);
    assert!(
        !coalescer.is_paste_open(),
        "end marker must close the paste"
    );
    assert!(
        tail.iter()
            .any(|event| matches!(event, Event::Paste(text) if text == "a")),
        "payload must survive the split end marker: {tail:?}"
    );
}

/// D-1: while a bracketed paste is open, a held end-marker prefix must NOT be
/// released as ordinary keys when the 40 ms marker window expires. A released
/// Esc dispatches `Action::Interrupt` and aborts the running agent.
#[test]
fn paste_end_marker_survives_the_marker_timeout() {
    use pir::tui::terminal::CONSOLE_MARKER_TAIL_TIMEOUT;

    let mut coalescer = ConsoleBatchCoalescer::new();
    let start = vec![
        esc(),
        press('['),
        press('2'),
        press('0'),
        press('0'),
        press('~'),
        press('a'),
    ];
    let out = coalescer.coalesce(start);
    assert!(coalescer.is_paste_open(), "{out:?}");

    let held = coalescer.coalesce(vec![esc()]);
    assert!(
        held.is_empty(),
        "the end-marker Esc must be withheld: {held:?}"
    );
    assert!(
        coalescer.marker_tail_deadline().is_none(),
        "an open paste must not publish a key-release deadline for its end marker"
    );
    let after_window =
        Instant::now() + CONSOLE_MARKER_TAIL_TIMEOUT + std::time::Duration::from_millis(10);
    assert!(
        coalescer.poll_marker_tail(after_window).is_empty(),
        "expiry must not release the end-marker Esc while the paste is open"
    );

    let tail = coalescer.coalesce(vec![
        press('['),
        press('2'),
        press('0'),
        press('1'),
        press('~'),
    ]);
    assert!(
        !coalescer.is_paste_open(),
        "the end marker closes the paste"
    );
    assert!(
        tail.iter()
            .any(|event| matches!(event, Event::Paste(text) if text == "a")),
        "the paste payload must survive: {tail:?}"
    );
}

/// A pasted document containing the literal `[200~` in the middle must stay
/// payload; only a marker at the start of a batch opens a paste.
#[test]
fn literal_paste_marker_text_stays_payload() {
    let mut coalescer = ConsoleBatchCoalescer::new();
    let mut events = Vec::new();
    for character in "abc[200~def".chars() {
        events.push(press(character));
        events.push(release(character));
    }
    let out = coalescer.coalesce(events);
    assert!(
        !coalescer.is_paste_open(),
        "literal text opened a paste: {out:?}"
    );
    let out: VecDeque<Event> = out.into();
    assert_eq!(
        paste_texts(&out),
        vec!["abc[200~def".to_string()],
        "{out:?}"
    );
}

/// P1-1 (TUI-PERF2): a printable run whose batch ends with Enter must flush
/// the text as a Paste and leave the Enter in the queue as its own Key. The
/// pre-fix collector absorbed it as `Paste("abcdefgh\n")`, which the editor
/// inserts as a line break instead of submitting the prompt.
#[test]
fn printable_burst_keeps_final_enter_as_a_key() {
    let mut queue: VecDeque<Event> = "abcdefgh".chars().map(press).collect();
    queue.push_back(enter());
    let mut release_enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::empty());
    release_enter.kind = KeyEventKind::Release;
    queue.push_back(Event::Key(release_enter));

    let mut burst = PrintableBurst::new();
    let blocked = burst.absorb(&mut queue, Instant::now(), true);
    assert!(blocked, "the final Enter must terminate the run");

    let mut out = VecDeque::new();
    burst.flush_now(&mut out);
    assert_eq!(
        paste_texts(&out),
        vec!["abcdefgh".to_string()],
        "the final Enter must not become paste payload: {out:?}"
    );
    assert!(
        matches!(
            queue.front(),
            Some(Event::Key(key))
                if key.code == KeyCode::Enter && key.kind == KeyEventKind::Press
        ),
        "the Enter must stay queued for the submit path: {queue:?}"
    );
}

/// The same split is required when the coalescer already emitted the text as
/// an explicit Paste and the user then presses Enter: the burst collector
/// runs after the batch coalescer and must not merge them back into
/// `Paste("hello\n")`.
#[test]
fn printable_burst_keeps_final_enter_after_explicit_paste() {
    let mut queue: VecDeque<Event> = VecDeque::from([paste("hello"), enter()]);
    let mut burst = PrintableBurst::new();
    assert!(burst.absorb(&mut queue, Instant::now(), true));

    let mut out = VecDeque::new();
    burst.flush_now(&mut out);
    assert_eq!(paste_texts(&out), vec!["hello".to_string()], "{out:?}");
    assert!(
        matches!(queue.front(), Some(Event::Key(key)) if key.code == KeyCode::Enter),
        "{queue:?}"
    );
}

/// P1-2 (TUI-PERF2): a lone `[` is a key press, not a bracketed-paste start.
/// Holding it in `marker_tail` until the next console batch made the key
/// disappear on Windows whenever no further record arrived.
#[test]
fn lone_bracket_is_dispatched_not_withheld() {
    let mut coalescer = ConsoleBatchCoalescer::new();
    let out = coalescer.coalesce(vec![press('['), release('[')]);
    assert_eq!(out.len(), 1, "a lone `[` must be emitted: {out:?}");
    assert!(
        matches!(&out[0], Event::Key(key) if key.code == KeyCode::Char('[')),
        "{out:?}"
    );
    assert!(coalescer.marker_tail_deadline().is_none());
}

/// A multi-character marker prefix (`[2`) may still straddle batches, but
/// only for `CONSOLE_MARKER_TAIL_TIMEOUT`: the event loop wakes at that
/// deadline and releases the keys instead of withholding them forever.
#[test]
fn split_marker_prefix_is_released_when_its_window_expires() {
    use pir::tui::terminal::CONSOLE_MARKER_TAIL_TIMEOUT;

    let mut coalescer = ConsoleBatchCoalescer::new();
    let out = coalescer.coalesce(vec![press('['), press('2')]);
    assert!(out.is_empty(), "a split prefix is held for the next batch");
    let deadline = coalescer
        .marker_tail_deadline()
        .expect("held prefix must publish a wake-up deadline");

    let released = coalescer.poll_marker_tail(deadline);
    let keys: Vec<KeyCode> = released
        .iter()
        .filter_map(|event| match event {
            Event::Key(key) => Some(key.code),
            _ => None,
        })
        .collect();
    assert_eq!(
        keys,
        vec![KeyCode::Char('['), KeyCode::Char('2')],
        "{released:?}"
    );
    assert!(coalescer.marker_tail_deadline().is_none());
    assert!(coalescer.poll_marker_tail(deadline).is_empty());
    assert!(
        deadline.saturating_duration_since(Instant::now()) <= CONSOLE_MARKER_TAIL_TIMEOUT,
        "the deadline must stay inside the sequence window"
    );
}
