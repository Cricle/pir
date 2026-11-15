//! PIR-TUI-026: a complete printable batch must be inserted and painted at
//! the next frame, not after the slow-paste quiet window.
//!
//! Repro (`.play/issues/PIR-TUI-026.md`): one `write()` delivering six
//! printable characters stayed invisible for ~140 ms because
//! `PrintableBurst::should_flush` required `RUN_QUIET` (120 ms) of silence
//! after the run, and `time_until_flush` made the event loop sleep that long.
//! Upstream pi inserts and paints the same complete batch in 17-30 ms: its
//! `StdinBuffer` dispatches every complete sequence immediately
//! (`packages/tui/src/stdin-buffer.ts:200-279`) and `ProcessTerminal`
//! forwards them one by one (`packages/tui/src/terminal.ts:240-266`); the
//! only wait is the 16 ms `requestRender` frame clamp.
//!
//! These pins are pure decisions over explicit `Instant` values (no sleeps),
//! so they cannot flake.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use pir::tui::terminal::PrintableBurst;

fn press(c: char) -> Event {
    Event::Key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::empty()))
}

fn release(c: char) -> Event {
    let mut key = KeyEvent::new(KeyCode::Char(c), KeyModifiers::empty());
    key.kind = KeyEventKind::Release;
    Event::Key(key)
}

fn keys(text: &str) -> VecDeque<Event> {
    text.chars().map(press).collect()
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

/// The issue's exact shape: `#qwerty` (6 printable characters, below
/// `CONSOLE_BURST_MIN_CHARS`, so the console seam leaves them as keys) arrives
/// in ONE drain. The collector must ask the event loop to wake at the
/// frame-scale batch window, and the same run must be released as one paste
/// at that deadline.
#[test]
fn one_six_character_batch_flushes_at_the_frame_window() {
    let window = PrintableBurst::BATCH_QUIET;
    assert!(
        window <= Duration::from_millis(30),
        "the complete-batch window must stay inside upstream's measured \
         17-30 ms insert-and-paint latency (got {window:?})"
    );
    assert!(
        window < PrintableBurst::RUN_QUIET,
        "a complete batch must not wait the slow-paste quiet window"
    );

    let mut burst = PrintableBurst::new();
    let start = Instant::now();
    let mut queue = keys("#qwerty");
    assert!(
        !burst.absorb(&mut queue, start, true),
        "a plain printable run has no blocker"
    );
    assert!(queue.is_empty(), "the whole batch was absorbed: {queue:?}");

    // The loop's poll sleeps only to the batch window, not to RUN_QUIET.
    assert_eq!(burst.time_until_flush(start), Some(window));

    // Not before the window (a follow-up read may still join), at it exactly
    // the run is released.
    assert!(!burst.should_flush(start, false, true));
    assert!(!burst.should_flush(start + Duration::from_millis(1), false, true));
    assert!(burst.should_flush(start + window, false, true));

    let mut out = VecDeque::new();
    let outcome = burst.flush(&mut out, true);
    assert!(outcome.pasted, "{outcome:?}");
    assert!(outcome.finished, "{outcome:?}");
    assert_eq!(paste_texts(&out), vec!["#qwerty".to_string()], "{out:?}");
}

/// Coalescing stays intact: a second read that lands inside the batch window
/// joins the same run (one insertion, no reordering), and only silence after
/// the LAST character releases it.
#[test]
fn a_follow_up_read_inside_the_window_joins_the_same_run() {
    let mut burst = PrintableBurst::new();
    let start = Instant::now();
    let mut first = keys("abc");
    assert!(!burst.absorb(&mut first, start, true));

    // 10 ms later (inside BATCH_QUIET) the rest of the paste arrives.
    let second_at = start + Duration::from_millis(10);
    let mut second = keys("def");
    assert!(!burst.absorb(&mut second, second_at, true));
    assert!(second.is_empty());
    assert!(
        !burst.should_flush(start + PrintableBurst::BATCH_QUIET, false, true),
        "the window restarts at the last absorbed character"
    );
    assert!(burst.should_flush(second_at + PrintableBurst::BATCH_QUIET, false, true));

    let mut out = VecDeque::new();
    burst.flush(&mut out, true);
    assert_eq!(paste_texts(&out), vec!["abcdef".to_string()], "{out:?}");
}

/// A complete bracketed paste is one `Event::Paste` from the console seam;
/// it reaches the frame deadline the same way (PIR-TUI-026 measured 139.8 /
/// 144.6 ms for 7/35-character bracketed pastes, upstream 16.6-25.3 ms).
#[test]
fn a_complete_bracketed_paste_chunk_flushes_at_the_frame_window() {
    let mut burst = PrintableBurst::new();
    let start = Instant::now();
    let mut queue: VecDeque<Event> = VecDeque::from([Event::Paste("pasted".to_string())]);
    assert!(!burst.absorb(&mut queue, start, true));
    assert_eq!(
        burst.time_until_flush(start),
        Some(PrintableBurst::BATCH_QUIET)
    );

    let mut out = VecDeque::new();
    burst.flush(&mut out, true);
    assert_eq!(paste_texts(&out), vec!["pasted".to_string()], "{out:?}");
}

/// Ordinary typing keeps its exact key semantics: a lone press is still
/// replayed as its original key after `SINGLE_QUIET` (6 ms) and is unaffected
/// by the multi-key batch window.
#[test]
fn a_lone_press_keeps_the_single_quiet_window() {
    let mut burst = PrintableBurst::new();
    let start = Instant::now();
    let mut queue: VecDeque<Event> = VecDeque::from([press('/')]);
    assert!(!burst.absorb(&mut queue, start, true));
    assert_eq!(
        burst.time_until_flush(start),
        Some(PrintableBurst::SINGLE_QUIET)
    );
    assert!(!burst.should_flush(start, false, true));
    assert!(burst.should_flush(start + PrintableBurst::SINGLE_QUIET, false, true));

    let mut out = VecDeque::new();
    let outcome = burst.flush(&mut out, true);
    assert!(outcome.replayed_key, "{outcome:?}");
    assert!(matches!(
        out.front(),
        Some(Event::Key(key)) if key.code == KeyCode::Char('/')
    ));
}

/// A multi-byte run is measured in decoded characters, not bytes: one drain
/// carrying a CJK word is one paste at the frame window, complete and in
/// order (no code-unit splitting at the new deadline).
#[test]
fn a_multibyte_batch_flushes_complete_at_the_frame_window() {
    let mut burst = PrintableBurst::new();
    let start = Instant::now();
    let mut queue = keys("中文输入");
    assert!(!burst.absorb(&mut queue, start, true));
    assert!(burst.should_flush(start + PrintableBurst::BATCH_QUIET, false, true));

    let mut out = VecDeque::new();
    burst.flush(&mut out, true);
    assert_eq!(paste_texts(&out), vec!["中文输入".to_string()], "{out:?}");
}

/// Releases are not payload: the console's press+release pair per character
/// must still produce one paste of the presses only at the frame window.
#[test]
fn release_pairs_do_not_split_the_batch() {
    let mut burst = PrintableBurst::new();
    let start = Instant::now();
    let mut queue: VecDeque<Event> = VecDeque::new();
    for c in "abc".chars() {
        queue.push_back(press(c));
        queue.push_back(release(c));
    }
    assert!(!burst.absorb(&mut queue, start, true));
    assert!(queue.is_empty(), "{queue:?}");
    assert!(burst.should_flush(start + PrintableBurst::BATCH_QUIET, false, true));

    let mut out = VecDeque::new();
    burst.flush(&mut out, true);
    assert_eq!(paste_texts(&out), vec!["abc".to_string()], "{out:?}");
    assert!(
        !out.iter().any(|event| matches!(
            event,
            Event::Key(key) if key.kind == KeyEventKind::Release
        )),
        "no release may survive the burst: {out:?}"
    );
}
