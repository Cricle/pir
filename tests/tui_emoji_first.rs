//! Windows console batch path: the FIRST record after idle must not be
//! pre-read by crossterm (QA INC-003).
//!
//! `event::poll(timeout)` on Windows reads one `INPUT_RECORD` into crossterm's
//! internal queue before returning. That first record bypassed `BatchReader`,
//! and crossterm's Windows surrogate pairing ignores `key_down`: a high
//! surrogate press followed by its release consumed the pending half, so an
//! emoji typed after idle produced no key at all. The batch path now takes
//! every console record directly; these tests pin its record → event seam.
//!
//! No inline tests: the seam is `decode_console_batch`, the same decoder the
//! live `BatchReader` uses.

use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use pir::tui::terminal::{decode_console_batch, ConsoleBatchCoalescer, ConsoleBatchRecord};

const HIGH: u16 = 0xD83D;
const LOW: u16 = 0xDE00;

fn key(key_down: bool, unit: u16) -> ConsoleBatchRecord {
    ConsoleBatchRecord::Key {
        key_down,
        virtual_key_code: 0,
        u_char: unit,
        modifiers: KeyModifiers::empty(),
    }
}

fn press(unit: u16) -> ConsoleBatchRecord {
    key(true, unit)
}

fn release(unit: u16) -> ConsoleBatchRecord {
    key(false, unit)
}

/// The idle-first-record shape: the console delivers a high surrogate and
/// then the low surrogate before any other record. The batch path must
/// produce exactly one `Char('😀')` press, not zero.
#[test]
fn first_emoji_high_then_low_surrogate_press_decodes() {
    let events = decode_console_batch(&[press(HIGH), press(LOW)]);

    assert_eq!(
        events,
        vec![Event::Key(KeyEvent::new(
            KeyCode::Char('😀'),
            KeyModifiers::empty()
        ))],
        "high+low surrogate presses must decode to one emoji: {events:?}"
    );
}

/// The full console shape repeats each UTF-16 unit on its key-up record. The
/// first emoji of an idle session arrives exactly like this.
#[test]
fn first_emoji_with_surrogate_releases_decodes_once() {
    let events = decode_console_batch(&[press(HIGH), release(HIGH), press(LOW), release(LOW)]);

    assert_eq!(events.len(), 1, "one emoji key expected: {events:?}");
    match &events[0] {
        Event::Key(key) => {
            assert_eq!(key.code, KeyCode::Char('😀'), "{events:?}");
            assert_eq!(key.kind, KeyEventKind::Press, "{events:?}");
        }
        other => panic!("expected a Key event, got {other:?}"),
    }
}

/// The ordinary first record after idle stays a normal ASCII key (no
/// off-by-one, no swallowed press).
#[test]
fn first_ascii_record_survives() {
    let events = decode_console_batch(&[
        press(b'h' as u16),
        release(b'h' as u16),
        press(b'i' as u16),
        release(b'i' as u16),
    ]);

    let typed: String = events
        .iter()
        .filter_map(|event| match event {
            Event::Key(key) if key.kind == KeyEventKind::Press => match key.code {
                KeyCode::Char(c) => Some(c),
                _ => None,
            },
            _ => None,
        })
        .collect();
    assert_eq!(typed, "hi", "{events:?}");
    match &events[0] {
        Event::Key(key) => assert_eq!(key.code, KeyCode::Char('h'), "{events:?}"),
        other => panic!("first event must be the 'h' press, got {other:?}"),
    }
}

/// A resize record interleaved with the first emoji is not swallowed by the
/// batch path or by the existing console coalescer (the seam in front of the
/// editor).
#[test]
fn resize_record_survives_batch_and_coalescer() {
    let records = [
        ConsoleBatchRecord::Resize { x: 119, y: 39 },
        press(HIGH),
        press(LOW),
    ];
    let events = decode_console_batch(&records);

    assert!(
        events.contains(&Event::Resize(120, 40)),
        "resize must be emitted by the batch decoder: {events:?}"
    );
    assert!(
        events
            .iter()
            .any(|event| matches!(event, Event::Key(key) if key.code == KeyCode::Char('😀'))),
        "the emoji must survive next to the resize: {events:?}"
    );

    let coalesced = ConsoleBatchCoalescer::new().coalesce(events);
    assert!(
        coalesced.contains(&Event::Resize(120, 40)),
        "resize must pass the coalescer untouched: {coalesced:?}"
    );
    assert!(
        coalesced
            .iter()
            .any(|event| matches!(event, Event::Key(key) if key.code == KeyCode::Char('😀'))),
        "the emoji must pass the coalescer untouched: {coalesced:?}"
    );
}
