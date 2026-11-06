//! Windows console non-BMP emoji input regression (QA S2).
//!
//! ConPTY/winpty delivers a non-BMP emoji as two UTF-16 surrogate records,
//! each followed by its key-up record. `BatchReader` paired surrogate halves
//! without looking at `key_down`: the high-surrogate release consumed the
//! stored high half and `decode_utf16([high, high])` failed, so the low press
//! started over and the whole emoji produced no key event. The composer then
//! showed blanks and the mock request contained zero emoji bytes.
//!
//! These tests pin the pure decode seam: only a key-down record may complete
//! a pair, and surrogate releases must leave the pending half untouched.

use crossterm::event::KeyCode;
use pir::tui::terminal::decode_console_char;

const HIGH: u16 = 0xD83D;
const LOW: u16 = 0xDE00;

/// 😀 arrives as high press, high release, low press, low release.
#[test]
fn non_bmp_emoji_press_pair_ignores_release_records() {
    let mut pending = None;

    // High-surrogate press: no character yet; the half stays pending.
    assert_eq!(decode_console_char(HIGH, true, &mut pending), None);
    assert_eq!(pending, Some(HIGH));

    // High-surrogate release: the console repeats the unit; it must not
    // consume the pending high half (the pre-fix bug cleared it here).
    assert_eq!(decode_console_char(HIGH, false, &mut pending), None);
    assert_eq!(
        pending,
        Some(HIGH),
        "high-surrogate release must leave the pending half untouched"
    );

    // Low-surrogate press completes the pair exactly once.
    assert_eq!(
        decode_console_char(LOW, true, &mut pending),
        Some(KeyCode::Char('😀'))
    );
    assert_eq!(pending, None);

    // Low-surrogate release is a no-op.
    assert_eq!(decode_console_char(LOW, false, &mut pending), None);
    assert_eq!(pending, None);
}

/// BMP characters decode directly; plain releases keep their key code and
/// never disturb a pending surrogate half.
#[test]
fn bmp_chars_and_plain_releases_do_not_touch_surrogate_state() {
    let mut pending = Some(HIGH);

    assert_eq!(
        decode_console_char('中' as u16, true, &mut pending),
        Some(KeyCode::Char('中'))
    );
    assert_eq!(pending, Some(HIGH));

    assert_eq!(
        decode_console_char('é' as u16, true, &mut pending),
        Some(KeyCode::Char('é'))
    );
    assert_eq!(pending, Some(HIGH));

    assert_eq!(
        decode_console_char(b'a' as u16, false, &mut pending),
        Some(KeyCode::Char('a'))
    );
    assert_eq!(pending, Some(HIGH));
}

/// The exact QA composer line: feeding every UTF-16 unit as a press+release
/// record pair reconstructs all scalars, including every non-BMP emoji.
#[test]
fn qa_composer_line_survives_press_release_record_pairs() {
    const LINE: &str = "中文 日本語 한글 émoji 😀 🚀 👍🏽 🇨🇳 end";

    let mut pending = None;
    let mut typed = String::new();
    for unit in LINE.encode_utf16() {
        if let Some(KeyCode::Char(ch)) = decode_console_char(unit, true, &mut pending) {
            typed.push(ch);
        }
        let release = decode_console_char(unit, false, &mut pending);
        if (0xD800..=0xDFFF).contains(&unit) {
            assert_eq!(release, None, "surrogate release must not produce a key");
        } else {
            assert_eq!(
                release,
                char::from_u32(unit as u32).map(KeyCode::Char),
                "BMP release keeps its key code"
            );
        }
    }

    assert_eq!(typed, LINE);
    assert_eq!(pending, None);
}
