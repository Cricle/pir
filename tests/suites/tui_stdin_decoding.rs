//! Stdin-decoding parity tests: the Rust port of upstream
//! `packages/tui/src/stdin-buffer.ts` (`StdinBuffer`) plus the input half of
//! `packages/tui/src/terminal.ts`.
//!
//! Everything here drives the decoder through its pure seams — bytes (or
//! parsed crossterm events) in, sequences/events out, with explicit `Instant`
//! deadlines instead of real timers — so no terminal is involved. The
//! expected values were probed from the upstream TypeScript decoder with
//! synthetic byte streams (`node --experimental-strip-types`), hence the
//! exact sequence/order assertions below.
//!
//! Upstream anchors:
//! - `stdin-buffer.ts:23-29` timeouts + reply header
//! - `stdin-buffer.ts:38-88` `isCompleteSequence`
//! - `stdin-buffer.ts:201-279` `extractCompleteSequences`
//! - `stdin-buffer.ts:320-433` `process` (paste, timeout arming, flush split)
//! - `terminal.ts:128-145` `resolveEscapeTimeoutMs`
//! - `terminal.ts:62-73` Shift+Enter synthesis, `terminal.ts:44-46` Apple
//!   Terminal gate
//! - `terminal.ts:170-183`/`:521-530` `PI_TUI_WRITE_LOG`
//! - `terminal.ts:414-427` Windows VT input

use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crossterm::event::{
    Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use pir::tui::stdin_buffer::{
    classify_sgr_mouse_tail, coalesce_paste_burst, decode_input_sequence, decode_sgr_mouse_tail,
    decode_x10_mouse, enable_windows_vt_input, is_apple_terminal_session, is_complete_sequence,
    key_state_is_down, native_shift_detection_enabled, normalize_apple_terminal_input,
    normalize_native_shift_enter_input, resolve_escape_timeout_ms, shift_flag_set,
    should_enable_windows_vt_input, vt_input_mode_change, windows_vt_input_status, write_log_path,
    SequenceStatus, SgrMouseTail, StdinBuffer, StdinEvent, StdinGate, VtInputOutcome,
    VtInputStatus, WriteLog, BEL, DEFAULT_ESCAPE_TIMEOUT_MS, DEFAULT_SEQUENCE_TIMEOUT_MS,
    DEFAULT_SSH_ESCAPE_TIMEOUT_MS, ENABLE_VIRTUAL_TERMINAL_INPUT, MAX_PASTE_BYTES,
    NATIVE_SHIFT_ENTER_SEQUENCE, SHIFT_TAB_VT_SEQUENCE,
};

use pir::tui::terminal::vt_input_opt_in;

use pir::tui::terminal_reply::{
    device_attributes_reply_len, is_reply_header_prefix, reply_input_state, reply_take,
    reply_token_len, reply_tokens_len, ReplyInputState, ReplyTake,
};

use pir::tui::theme::{TerminalColorFeed, TerminalColorQuery, TERMINAL_PALETTE_SIZE};

const ESC: u8 = 0x1b;

fn at(base: Instant, ms: u64) -> Instant {
    base + Duration::from_millis(ms)
}

fn data(bytes: &[u8]) -> StdinEvent {
    StdinEvent::Data(bytes.to_vec())
}

fn paste(bytes: &[u8]) -> StdinEvent {
    StdinEvent::Paste(bytes.to_vec())
}

fn buffer() -> StdinBuffer {
    StdinBuffer::default()
}

fn key(code: KeyCode, modifiers: KeyModifiers) -> Event {
    Event::Key(KeyEvent::new(code, modifiers))
}

fn shift_pressed() -> bool {
    true
}

fn shift_released() -> bool {
    false
}

/// Counts how often the gate asks for the native Shift state (the provider is
/// a plain `fn()`, so the counter lives at module scope).
static SHIFT_PROVIDER_CALLS: AtomicUsize = AtomicUsize::new(0);

fn counting_shift_pressed() -> bool {
    SHIFT_PROVIDER_CALLS.fetch_add(1, Ordering::SeqCst);
    true
}

// ---------------------------------------------------------------------------
// Sequence reassembly across read boundaries
// ---------------------------------------------------------------------------

#[test]
fn stdin_buffer_reassembles_an_sgr_mouse_report_split_at_every_boundary() {
    // Upstream `should buffer incomplete mouse SGR sequence`: ESC, `[<35`,
    // `;20;5m` arrive as three chunks and only the last one completes the
    // sequence.
    let report = b"\x1b[<35;20;5m";
    for split in 1..report.len() {
        let base = Instant::now();
        let mut buf = buffer();
        let head = buf.process(&report[..split], base);
        assert!(
            head.is_empty(),
            "split {split} emitted {:?} before the tail arrived",
            head
        );
        assert_eq!(buf.buffer(), &report[..split], "split {split}");
        let tail = buf.process(&report[split..], at(base, 1));
        assert_eq!(tail, vec![data(report)], "split {split}");
        assert!(buf.buffer().is_empty(), "split {split} left bytes held");
    }
}

#[test]
fn stdin_buffer_reassembles_an_x10_mouse_report_split_at_every_boundary() {
    // The legacy X10 report is `ESC [ M` + three payload bytes; upstream's
    // old-style-mouse completeness rule (`stdin-buffer.ts:51-55`) holds it as
    // one 6-byte sequence regardless of where the reads split.
    let report = b"\x1b[M\x23\x21\x22";
    for split in 1..report.len() {
        let base = Instant::now();
        let mut buf = buffer();
        let head = buf.process(&report[..split], base);
        assert!(
            head.is_empty(),
            "split {split} emitted {:?} before the tail arrived",
            head
        );
        assert_eq!(buf.buffer(), &report[..split], "split {split}");
        let tail = buf.process(&report[split..], at(base, 1));
        assert_eq!(tail, vec![data(report)], "split {split}");
        assert!(buf.buffer().is_empty(), "split {split} left bytes held");
    }
}

#[test]
fn stdin_buffer_holds_a_sequence_split_mid_escape() {
    // The first chunk is a bare ESC (the mid-escape split) and the rest
    // follows inside the escape window.
    let base = Instant::now();
    let mut buf = buffer();
    assert!(buf.process(b"\x1b", base).is_empty());
    assert!(buf.process(b"[", at(base, 2)).is_empty());
    assert!(!buf.process(b"<65;48;39M", at(base, 4)).is_empty());
    assert_eq!(buf.buffer(), b"");
}

#[test]
fn stdin_buffer_reassembles_a_split_csi_u_sequence() {
    let base = Instant::now();
    let mut buf = buffer();
    assert!(buf.process(b"\x1b[13", base).is_empty());
    assert!(buf.process(b";2", at(base, 1)).is_empty());
    let out = buf.process(b"u", at(base, 2));
    assert_eq!(out, vec![data(b"\x1b[13;2u")]);
    // The reassembled sequence is the Shift+Enter synthesis target.
    assert_eq!(
        decode_input_sequence(b"\x1b[13;2u"),
        Some(vec![key(KeyCode::Enter, KeyModifiers::SHIFT)])
    );
}

#[test]
fn stdin_buffer_passes_regular_characters_immediately() {
    let base = Instant::now();
    let mut buf = buffer();
    // Upstream emits one sequence per character, including multi-byte ones.
    let out = buf.process("hello 世界".as_bytes(), base);
    assert_eq!(
        out,
        vec![
            data(b"h"),
            data(b"e"),
            data(b"l"),
            data(b"l"),
            data(b"o"),
            data(b" "),
            data("世".as_bytes()),
            data("界".as_bytes()),
        ]
    );
}

#[test]
fn stdin_buffer_handles_characters_around_a_partial_sequence() {
    let base = Instant::now();
    let mut buf = buffer();
    assert_eq!(
        buf.process(b"abc\x1b[<35", base),
        vec![data(b"a"), data(b"b"), data(b"c")]
    );
    assert_eq!(buf.buffer(), b"\x1b[<35");
    assert_eq!(
        buf.process(b";20;5m", at(base, 1)),
        vec![data(b"\x1b[<35;20;5m")]
    );
}

#[test]
fn stdin_buffer_splits_double_escape_before_a_new_sequence() {
    // WezTerm sends the Escape key press as a raw ESC and its release as a
    // CSI-u sequence, concatenated (upstream `stdin-buffer.ts:232-254`).
    let base = Instant::now();
    let mut buf = buffer();
    assert_eq!(
        buf.process(b"\x1b\x1b[27;129:3u", base),
        vec![data(b"\x1b"), data(b"\x1b[27;129:3u")]
    );
    // ESC+ESC with nothing following stays a single (meta) sequence.
    let mut buf = buffer();
    assert_eq!(
        buf.process(b"\x1b\x1b", Instant::now()),
        vec![data(b"\x1b\x1b")]
    );
}

#[test]
fn stdin_buffer_drops_the_raw_duplicate_after_a_kitty_printable_sequence() {
    let base = Instant::now();
    let mut buf = buffer();
    assert_eq!(
        buf.process("\x1b[224uà".as_bytes(), base),
        vec![data(b"\x1b[224u")]
    );
}

#[test]
fn stdin_buffer_emits_a_complete_bracketed_paste() {
    let base = Instant::now();
    let mut buf = buffer();
    assert_eq!(
        buf.process(b"\x1b[200~hello world\x1b[201~", base),
        vec![paste(b"hello world")]
    );
    // Chunked paste, with input before and after.
    let mut buf = buffer();
    assert_eq!(buf.process(b"a\x1b[200~pa", base), vec![data(b"a")]);
    assert!(!buf.process(b"sted\x1b[201~", at(base, 1)).is_empty());
    assert_eq!(buf.process(b"b", at(base, 2)), vec![data(b"b")]);
}

// ---------------------------------------------------------------------------
// Timeout rules (10 / 50 / 100 ms) and the flush split
// ---------------------------------------------------------------------------

#[test]
fn stdin_buffer_flushes_a_lone_escape_after_the_escape_timeout() {
    let base = Instant::now();
    let mut buf = buffer();
    assert!(buf.process(b"\x1b", base).is_empty());
    assert!(buf
        .flush_due(at(base, DEFAULT_ESCAPE_TIMEOUT_MS - 1))
        .is_empty());
    assert_eq!(
        buf.flush_due(at(base, DEFAULT_ESCAPE_TIMEOUT_MS)),
        vec![data(b"\x1b")]
    );
    assert!(buf.buffer().is_empty());
}

#[test]
fn stdin_buffer_uses_the_sequence_timeout_for_everything_but_a_lone_escape() {
    // Upstream: only the bare ESC uses `escapeTimeout`; a two-byte-plus
    // incomplete sequence arms the sequence timeout instead.
    let base = Instant::now();
    let mut buf = StdinBuffer::with_timeouts(Duration::from_millis(100), Duration::from_millis(10));
    assert!(buf.process(b"\x1b", base).is_empty());
    assert_eq!(buf.flush_due(at(base, 10)), vec![data(b"\x1b")]);

    let base = Instant::now();
    let mut buf = buffer();
    assert!(buf.process(b"\x1b[<35", base).is_empty());
    assert!(buf
        .flush_due(at(base, DEFAULT_SEQUENCE_TIMEOUT_MS - 1))
        .is_empty());
    // The expired flush emits the whole held buffer as one sequence; the key
    // layer then resolves `\x1b[<35` to nothing (upstream `parseKey` undefined)
    // and `\x1b]` to alt+] (upstream `parseKey` = "alt+]").
    assert_eq!(
        buf.flush_due(at(base, DEFAULT_SEQUENCE_TIMEOUT_MS)),
        vec![data(b"\x1b[<35")]
    );
    assert_eq!(decode_input_sequence(b"\x1b[<35"), None);
}

#[test]
fn stdin_buffer_flushes_control_string_introducers_as_two_byte_sequences() {
    let base = Instant::now();
    for prefix in [
        b"\x1b]".as_slice(),
        b"\x1bP".as_slice(),
        b"\x1b_".as_slice(),
    ] {
        let mut buf = buffer();
        assert!(buf.process(prefix, base).is_empty());
        assert_eq!(
            buf.flush_due(at(base, DEFAULT_SEQUENCE_TIMEOUT_MS)),
            vec![data(prefix)],
            "{:?}",
            String::from_utf8_lossy(prefix)
        );
    }
    // `\x1b]` and `\x1b_` are legacy Alt chords upstream, `\x1bP` is not.
    assert_eq!(
        decode_input_sequence(b"\x1b]"),
        Some(vec![key(KeyCode::Char(']'), KeyModifiers::ALT)])
    );
}

#[test]
fn stdin_buffer_merges_escape_and_cr_inside_the_escape_window() {
    // Upstream `should merge ESC + CR split across chunks within a larger
    // timeout`: with the SSH/override window the ESC is still held when CR
    // arrives, so the two bytes are one Alt+Enter sequence.
    let base = Instant::now();
    let mut buf = StdinBuffer::with_timeouts(
        Duration::from_millis(DEFAULT_SEQUENCE_TIMEOUT_MS),
        Duration::from_millis(DEFAULT_SSH_ESCAPE_TIMEOUT_MS),
    );
    assert!(buf.process(b"\x1b", base).is_empty());
    assert!(buf.flush_due(at(base, 20)).is_empty());
    assert_eq!(buf.process(b"\r", at(base, 20)), vec![data(b"\x1b\r")]);
    assert_eq!(
        decode_input_sequence(b"\x1b\r"),
        Some(vec![key(KeyCode::Enter, KeyModifiers::ALT)])
    );
}

#[test]
fn stdin_buffer_flushes_escape_when_cr_arrives_after_the_timeout() {
    // The same bytes past the window are two independent keys: Escape, then
    // Enter (upstream `should flush a lone ESC as Escape when CR arrives after
    // the timeout`).
    let base = Instant::now();
    let mut buf = buffer();
    assert!(buf.process(b"\x1b", base).is_empty());
    assert!(!buf.flush_due(at(base, 20)).is_empty());
    assert_eq!(buf.process(b"\r", at(base, 20)), vec![data(b"\r")]);
}

#[test]
fn stdin_buffer_holds_a_claimed_terminal_reply_across_expired_timeouts() {
    // A recognized reply header waits for its terminator, however late
    // (upstream `stdin-buffer.ts:411-416`); the BEL must never become Ctrl+G.
    let reply = b"\x1b]4;1;rgb:eaea/6c6c/6d6d\x07";
    let base = Instant::now();
    let mut buf = buffer();
    assert!(buf.process(&reply[..14], base).is_empty());
    assert!(buf.flush_due(at(base, 5000)).is_empty());
    assert_eq!(buf.process(&reply[14..], at(base, 5000)), vec![data(reply)]);
}

#[test]
fn stdin_buffer_recovers_batched_legacy_alt_input_on_expiry() {
    // Upstream `recovers batched legacy Alt input`: the flushed control-string
    // prefix is re-split into the Alt chord plus the tail keys.
    let base = Instant::now();
    let mut buf = buffer();
    assert!(buf.process(b"\x1b]", base).is_empty());
    assert!(buf.process(b"x\r\x03\x04", at(base, 20)).is_empty());
    assert!(buf.flush_due(at(base, 69)).is_empty());
    assert_eq!(
        buf.flush_due(at(base, 70)),
        vec![
            data(b"\x1b]"),
            data(b"x"),
            data(b"\r"),
            data(&[0x03]),
            data(&[0x04]),
        ]
    );
}

#[test]
fn stdin_buffer_high_byte_conversion_matches_upstream() {
    // A lone high byte becomes ESC + (byte - 128) (upstream `process`).
    let base = Instant::now();
    let mut buf = buffer();
    assert_eq!(buf.process(&[0xa1], base), vec![data(&[0x1b, 0x21])]);
}

#[test]
fn stdin_buffer_reports_sequence_completeness() {
    assert_eq!(is_complete_sequence(b"a"), SequenceStatus::NotEscape);
    assert_eq!(is_complete_sequence(b"\x1b"), SequenceStatus::Incomplete);
    assert_eq!(is_complete_sequence(b"\x1b["), SequenceStatus::Incomplete);
    assert_eq!(is_complete_sequence(b"\x1b[A"), SequenceStatus::Complete);
    assert_eq!(is_complete_sequence(b"\x1bO"), SequenceStatus::Incomplete);
    assert_eq!(is_complete_sequence(b"\x1bOA"), SequenceStatus::Complete);
    assert_eq!(
        is_complete_sequence(b"\x1b[<35;20;5"),
        SequenceStatus::Incomplete
    );
    assert_eq!(
        is_complete_sequence(b"\x1b[<35;20;5m"),
        SequenceStatus::Complete
    );
    assert_eq!(is_complete_sequence(b"\x1b[M"), SequenceStatus::Incomplete);
    assert_eq!(is_complete_sequence(b"\x1b[M "), SequenceStatus::Incomplete);
    assert_eq!(is_complete_sequence(b"\x1b[M ab"), SequenceStatus::Complete);
    assert_eq!(
        is_complete_sequence(b"\x1b]0;t"),
        SequenceStatus::Incomplete
    );
    assert_eq!(
        is_complete_sequence(b"\x1b]0;t\x07"),
        SequenceStatus::Complete
    );
    assert_eq!(
        is_complete_sequence(b"\x1b]0;t\x1b\\"),
        SequenceStatus::Complete
    );
    assert_eq!(
        is_complete_sequence(b"\x1bP>|x"),
        SequenceStatus::Incomplete
    );
    assert_eq!(
        is_complete_sequence(b"\x1bP>|x\x1b\\"),
        SequenceStatus::Complete
    );
    assert_eq!(
        is_complete_sequence(b"\x1b_Gi=1;"),
        SequenceStatus::Incomplete
    );
}

// ---------------------------------------------------------------------------
// PI_TUI_ESC_TIMEOUT override (upstream terminal.ts:136-145)
// ---------------------------------------------------------------------------

#[test]
fn escape_timeout_prefers_pi_tui_esc_timeout() {
    let env = |key: &str| match key {
        "PI_TUI_ESC_TIMEOUT" => Some("80".to_string()),
        "SSH_TTY" => Some("/dev/pts/1".to_string()),
        _ => None,
    };
    assert_eq!(resolve_escape_timeout_ms(env), 80);

    let env = |key: &str| (key == "PI_TUI_ESC_TIMEOUT").then(|| "80".to_string());
    assert_eq!(resolve_escape_timeout_ms(env), 80);
}

#[test]
fn escape_timeout_ignores_invalid_overrides() {
    for value in ["abc", "0", "-5", ""] {
        let env = move |key: &str| (key == "PI_TUI_ESC_TIMEOUT").then(|| value.to_string());
        assert_eq!(
            resolve_escape_timeout_ms(env),
            DEFAULT_ESCAPE_TIMEOUT_MS,
            "override {value:?}"
        );
    }
}

#[test]
fn escape_timeout_defaults_to_100ms_over_ssh() {
    let env = |key: &str| (key == "SSH_CONNECTION").then(|| "10.0.0.1 22".to_string());
    assert_eq!(
        resolve_escape_timeout_ms(env),
        DEFAULT_SSH_ESCAPE_TIMEOUT_MS
    );
    let env = |key: &str| (key == "SSH_TTY").then(|| "/dev/pts/1".to_string());
    assert_eq!(
        resolve_escape_timeout_ms(env),
        DEFAULT_SSH_ESCAPE_TIMEOUT_MS
    );
    assert_eq!(DEFAULT_SSH_ESCAPE_TIMEOUT_MS, 100);
}

#[test]
fn escape_timeout_defaults_to_10ms() {
    assert_eq!(
        resolve_escape_timeout_ms(|_| None),
        DEFAULT_ESCAPE_TIMEOUT_MS
    );
    assert_eq!(DEFAULT_ESCAPE_TIMEOUT_MS, 10);
    assert_eq!(DEFAULT_SEQUENCE_TIMEOUT_MS, 50);
}

#[test]
fn stdin_gate_honours_the_escape_timeout_for_a_bare_escape() {
    let base = Instant::now();
    let mut gate = StdinGate::new(
        Duration::from_millis(DEFAULT_SEQUENCE_TIMEOUT_MS),
        Duration::from_millis(DEFAULT_SSH_ESCAPE_TIMEOUT_MS),
    );
    assert!(gate
        .push(key(KeyCode::Esc, KeyModifiers::empty()), base)
        .is_empty());
    assert!(gate.flush_due(at(base, 99)).is_empty());
    assert_eq!(
        gate.flush_due(at(base, 100)),
        vec![key(KeyCode::Esc, KeyModifiers::empty())]
    );
}

#[test]
fn stdin_gate_merges_escape_and_enter_inside_the_window() {
    // Upstream meta sequence: ESC + one byte is complete at once, so Alt+Enter
    // is produced as soon as CR arrives inside the escape window.
    let base = Instant::now();
    let mut gate = StdinGate::default();
    assert!(gate
        .push(key(KeyCode::Esc, KeyModifiers::empty()), base)
        .is_empty());
    assert_eq!(
        gate.push(key(KeyCode::Enter, KeyModifiers::empty()), at(base, 5)),
        vec![key(KeyCode::Enter, KeyModifiers::ALT)]
    );
}

#[test]
fn stdin_gate_flushes_escape_before_an_already_parsed_sequence() {
    // An arrow key arrives already parsed; upstream splits `ESC ESC [A` into
    // ESC then the sequence, which is the same observable.
    let base = Instant::now();
    let mut gate = StdinGate::default();
    assert!(gate
        .push(key(KeyCode::Esc, KeyModifiers::empty()), base)
        .is_empty());
    assert_eq!(
        gate.push(key(KeyCode::Up, KeyModifiers::empty()), at(base, 5)),
        vec![
            key(KeyCode::Esc, KeyModifiers::empty()),
            key(KeyCode::Up, KeyModifiers::empty())
        ]
    );
}

#[test]
fn stdin_gate_reassembles_a_split_mouse_report() {
    let base = Instant::now();
    let mut gate = StdinGate::default();
    // Wheel-up (`64`) and a no-button move (`35`) split across reads.
    let chunks = ["\x1b", "[<64", ";20", ";5", "M"];
    let mut out = Vec::new();
    for (index, chunk) in chunks.iter().enumerate() {
        let events: Vec<Event> = chunk
            .chars()
            .map(|c| match c {
                '\x1b' => key(KeyCode::Esc, KeyModifiers::empty()),
                other => key(KeyCode::Char(other), KeyModifiers::empty()),
            })
            .collect();
        for (offset, event) in events.into_iter().enumerate() {
            out.extend(gate.push(event, at(base, (index * 10 + offset) as u64)));
        }
    }
    assert_eq!(
        out,
        vec![Event::Mouse(MouseEvent {
            kind: MouseEventKind::ScrollUp,
            column: 19,
            row: 4,
            modifiers: KeyModifiers::empty(),
        })]
    );
    // `\x1b[<35;20;5m` is a no-button motion report (upstream's own example).
    assert_eq!(
        decode_input_sequence(b"\x1b[<35;20;5m"),
        Some(vec![Event::Mouse(MouseEvent {
            kind: MouseEventKind::Moved,
            column: 19,
            row: 4,
            modifiers: KeyModifiers::empty(),
        })])
    );
    // `\x1b[<0;20;5M` is a left-button press, `...m` its release.
    assert_eq!(
        decode_input_sequence(b"\x1b[<0;20;5M"),
        Some(vec![Event::Mouse(MouseEvent {
            kind: MouseEventKind::Down(crossterm::event::MouseButton::Left),
            column: 19,
            row: 4,
            modifiers: KeyModifiers::empty(),
        })])
    );
}

#[test]
fn stdin_gate_reassembles_a_split_csi_u_shift_enter() {
    let base = Instant::now();
    let mut gate = StdinGate::default();
    let mut out = Vec::new();
    for (index, c) in "\x1b[13;2u".chars().enumerate() {
        let event = if c == '\x1b' {
            key(KeyCode::Esc, KeyModifiers::empty())
        } else {
            key(KeyCode::Char(c), KeyModifiers::empty())
        };
        out.extend(gate.push(event, at(base, index as u64)));
    }
    assert_eq!(
        out,
        vec![key(KeyCode::Enter, KeyModifiers::SHIFT)],
        "split CSI-u Shift+Enter must reassemble"
    );
}

#[test]
fn stdin_gate_reassembles_the_legacy_shift_enter_sequence() {
    // Upstream's editor accepts `ESC [ 13 ; 2 ~` as a newline
    // (`components/editor.ts:895`) — the legacy Shift+F3 spelling. The gate
    // must shadow Shift+F3 and emit Shift+Enter, split at any boundary.
    let sequence = "\x1b[13;2~";
    let key_for = |c: char| {
        if c == '\x1b' {
            key(KeyCode::Esc, KeyModifiers::empty())
        } else {
            key(KeyCode::Char(c), KeyModifiers::empty())
        }
    };
    for split in 1..sequence.len() {
        let base = Instant::now();
        let mut gate = StdinGate::default();
        let mut out = Vec::new();
        for (index, c) in sequence[..split].chars().enumerate() {
            out.extend(gate.push(key_for(c), at(base, index as u64)));
        }
        assert!(out.is_empty(), "split {split} leaked {out:?}");
        for (index, c) in sequence[split..].chars().enumerate() {
            out.extend(gate.push(key_for(c), at(base, 10 + index as u64)));
        }
        assert_eq!(
            out,
            vec![key(KeyCode::Enter, KeyModifiers::SHIFT)],
            "split {split} must reassemble Shift+Enter, not Shift+F3"
        );
    }
    // The generic decoder still reads the bytes as Shift+F3; only the gate's
    // upstream-mirroring special case shadows it.
    assert_eq!(
        decode_input_sequence(b"\x1b[13;2~"),
        Some(vec![key(KeyCode::F(3), KeyModifiers::SHIFT)])
    );
}

#[test]
fn stdin_gate_decodes_the_csi_u_shift_enter_spellings() {
    // CSI-u Enter, CSI-u Enter with an explicit `:1` press kind, and Kitty's
    // keypad Enter codepoint (upstream `keys.ts:311` `kpEnter: 57414`).
    for sequence in ["\x1b[13;2u", "\x1b[13;2:1u", "\x1b[57414;2u"] {
        let base = Instant::now();
        let mut gate = StdinGate::default();
        let mut out = Vec::new();
        for (index, c) in sequence.chars().enumerate() {
            let event = if c == '\x1b' {
                key(KeyCode::Esc, KeyModifiers::empty())
            } else {
                key(KeyCode::Char(c), KeyModifiers::empty())
            };
            out.extend(gate.push(event, at(base, index as u64)));
        }
        assert_eq!(
            out,
            vec![key(KeyCode::Enter, KeyModifiers::SHIFT)],
            "{sequence:?} must decode to Shift+Enter"
        );
    }
    assert_eq!(
        decode_input_sequence(b"\x1b[57414;2u"),
        Some(vec![key(KeyCode::Enter, KeyModifiers::SHIFT)])
    );
}

#[test]
fn stdin_gate_resolves_an_incomplete_sequence_on_the_sequence_timeout() {
    let base = Instant::now();
    let mut gate = StdinGate::default();
    assert!(gate
        .push(key(KeyCode::Esc, KeyModifiers::empty()), base)
        .is_empty());
    assert!(gate
        .push(key(KeyCode::Char('['), KeyModifiers::empty()), at(base, 2))
        .is_empty());
    assert!(gate.flush_due(at(base, 51)).is_empty());
    // `\x1b[` is a legacy Alt chord upstream (`parseKey` → "alt+[").
    assert_eq!(
        gate.flush_due(at(base, 52)),
        vec![key(KeyCode::Char('['), KeyModifiers::ALT)]
    );
}

#[test]
fn stdin_gate_drops_an_unparseable_partial_sequence() {
    // Upstream flushes `\x1b[<35` as one sequence, which its key layer
    // resolves to nothing — the tail must not be typed into the prompt.
    let base = Instant::now();
    let mut gate = StdinGate::default();
    let mut out = Vec::new();
    for (index, c) in "\x1b[<35".chars().enumerate() {
        let event = if c == '\x1b' {
            key(KeyCode::Esc, KeyModifiers::empty())
        } else {
            key(KeyCode::Char(c), KeyModifiers::empty())
        };
        out.extend(gate.push(event, at(base, index as u64)));
    }
    assert!(out.is_empty());
    out.extend(gate.flush_due(at(base, 100)));
    assert!(
        out.is_empty(),
        "unparseable partial sequence leaked {out:?}"
    );
}

#[test]
fn stdin_gate_passes_unrelated_events_through() {
    let base = Instant::now();
    let mut gate = StdinGate::default();
    assert_eq!(
        gate.push(key(KeyCode::Char('c'), KeyModifiers::CONTROL), base),
        vec![key(KeyCode::Char('c'), KeyModifiers::CONTROL)]
    );
    assert_eq!(
        gate.push(Event::Resize(80, 24), base),
        vec![Event::Resize(80, 24)]
    );
}

// ---------------------------------------------------------------------------
// Late terminal replies at the input gate
//
// A reply that lands *outside* the boot color query's window (a slow terminal
// answering late, a second DA1) is no longer claimed by the query's reader, so
// it reaches the input path as key records. The gate applies the reply grammar
// (`stdin_buffer::StdinGate::settle_reply_run`) to the run of parsed key events
// it has staged: a recognized reply is consumed whenever it arrives —
// fragmented included — instead of being typed into the editor (WAVE-REPORT §3
// red point 12 / battery s16). Anything the grammar does not recognize (arrows,
// Alt chords, paste markers) passes through the sequence machinery unchanged.
//
// Scope boundary (coordinator measurement, 2026-10-16): only a run that still
// carries its own `ESC` can be claimed. The Windows console's own DA1 answer is
// queued as one record per byte with `wVirtualKeyCode 0`, and crossterm drops
// the record that carries the ESC, so the tail (`[?61;…c`) arrives as plain
// `Char` keys — bytes indistinguishable from a user typing them, which the
// gate therefore does not eat. Closing that shape needs the console input owned
// at the *record* level (the `PeekConsoleInputW` machinery the boot query uses,
// driven from `tui::terminal::run`'s loop); the boundary is pinned below.
// ---------------------------------------------------------------------------

/// The DA1 answer to the `ESC [ c` request that terminates
/// `TERMINAL_COLOR_QUERY` (Windows Terminal / conhost shape, `[?61;…c`).
const DA1_REPLY_SPLIT: &[u8] = b"[?61;6;7;21;22;23;24;28;32;42c";

fn typed(text: &str) -> Vec<Event> {
    text.chars()
        .map(|c| key(KeyCode::Char(c), KeyModifiers::empty()))
        .collect()
}

#[test]
fn stdin_gate_swallows_a_late_device_attributes_answer() {
    let base = Instant::now();
    let mut answer = vec![ESC];
    answer.extend_from_slice(DA1_REPLY_SPLIT);
    // One burst.
    let mut gate = StdinGate::default();
    let out = push_console_bytes(&mut gate, base, 0, 1, &answer);
    assert!(
        out.is_empty(),
        "the late DA1 answer leaked into the input: {out:?}"
    );
    assert!(gate.held().is_empty(), "nothing may stay held: {gate:?}");
    assert!(!gate.paste_open());
    // Fragmented across pushes: the incomplete run is held (the sequence
    // window) and consumed the moment its `c` completes it.
    let mut gate = StdinGate::default();
    let mut out = push_console_bytes(&mut gate, base, 0, 1, &answer[..6]);
    assert!(out.is_empty(), "a DA1 fragment leaked: {out:?}");
    out.extend(push_console_bytes(&mut gate, base, 6, 1, &answer[6..]));
    assert!(out.is_empty(), "the fragmented DA1 leaked: {out:?}");
    assert!(gate.held().is_empty());
    // The key behind it is untouched.
    assert_eq!(
        push_console_bytes(&mut gate, base, 40, 1, b"q"),
        vec![key(KeyCode::Char('q'), KeyModifiers::empty())]
    );
}

#[test]
fn stdin_gate_swallows_a_late_osc11_reply() {
    // The color replies answer the same startup query: a complete OSC 11
    // background reply, BEL-terminated, arriving outside the window.
    let base = Instant::now();
    let mut reply = vec![ESC];
    reply.extend_from_slice(b"]11;#1a1a2b");
    reply.push(BEL);
    let mut gate = StdinGate::default();
    let out = push_console_bytes(&mut gate, base, 0, 1, &reply);
    assert!(out.is_empty(), "the late OSC 11 reply leaked: {out:?}");
    assert!(gate.held().is_empty());
    assert!(!gate.paste_open());
}

#[test]
fn stdin_gate_holds_a_fragmented_reply_until_its_terminator() {
    // The reply arrives over several pushes: the claim holds its open string —
    // no byte of it is typed while it is open — and its terminator closes it
    // without anything being dispatched.
    let base = Instant::now();
    let mut gate = StdinGate::default();
    let mut out = push_console_bytes(&mut gate, base, 0, 1, b"\x1b]11;");
    assert!(out.is_empty(), "a reply fragment leaked: {out:?}");
    // Inside the hold window the claim still holds.
    assert!(gate.flush_due(at(base, 8)).is_empty());
    out.extend(push_console_bytes(&mut gate, base, 10, 1, b"#1a"));
    assert!(out.is_empty(), "a reply fragment leaked: {out:?}");
    assert!(gate.flush_due(at(base, 15)).is_empty());
    out.extend(push_console_bytes(&mut gate, base, 20, 1, b"1a2b"));
    assert!(out.is_empty(), "a reply fragment leaked: {out:?}");
    out.extend(push_console_bytes(&mut gate, base, 40, 1, b"\x07"));
    assert!(out.is_empty(), "the completed reply leaked: {out:?}");
    assert!(gate.held().is_empty());
    // The keys behind it are untouched, in order.
    assert_eq!(
        push_console_bytes(&mut gate, base, 50, 1, b"ok"),
        typed("ok")
    );
}

#[test]
fn stdin_gate_consumes_every_reply_of_a_coalesced_batch() {
    // The terminal answers the startup query with several replies; when they
    // arrive late as one burst, *every* reply string is consumed — upstream
    // consumes every reply string out of its stdin buffer, whether or not it is
    // a color reply.
    let base = Instant::now();
    let mut burst = vec![ESC];
    burst.extend_from_slice(b"]10;#e0e0e0");
    burst.push(BEL);
    burst.push(ESC);
    burst.extend_from_slice(b"]11;#1a1a2b");
    burst.push(BEL);
    burst.push(ESC);
    burst.extend_from_slice(DA1_REPLY_SPLIT);
    let mut gate = StdinGate::default();
    let out = push_console_bytes(&mut gate, base, 0, 1, &burst);
    assert!(out.is_empty(), "a coalesced reply batch leaked: {out:?}");
    assert!(gate.held().is_empty());
}

#[test]
fn stdin_gate_keeps_input_behind_a_reply_in_order() {
    // The answer and the user's typing share one burst: only the reply is
    // swallowed, everything behind it is dispatched in order.
    let base = Instant::now();
    let mut gate = StdinGate::default();
    let mut burst = vec![ESC];
    burst.extend_from_slice(DA1_REPLY_SPLIT);
    burst.extend_from_slice(b"/quit");
    let out = push_console_bytes(&mut gate, base, 0, 1, &burst);
    assert_eq!(out, typed("/quit"));
    assert!(gate.held().is_empty());
}

#[test]
fn stdin_gate_releases_an_unterminated_control_string_reply() {
    // A claimed reply whose terminator never arrives is held for the sequence
    // window and then released by upstream's rules: the two-byte control-string
    // prefix becomes the Alt chord and the tail is replayed as its own keys, so
    // an `Alt+]` typed by hand is not eaten. (Upstream's flush split.)
    let base = Instant::now();
    let mut gate = StdinGate::default();
    let out = push_console_bytes(&mut gate, base, 0, 1, b"\x1b]11;#1a1a");
    assert!(
        out.is_empty(),
        "a claimed string is held for its window: {out:?}"
    );
    assert!(gate.flush_due(at(base, 30)).is_empty());
    let mut expected = vec![key(KeyCode::Char(']'), KeyModifiers::ALT)];
    expected.extend(typed("11;#1a1a"));
    assert_eq!(gate.flush_due(at(base, 100)), expected);
    assert!(gate.held().is_empty());
}

#[test]
fn stdin_gate_passes_non_reply_escape_runs_through() {
    let base = Instant::now();
    // An arrow key: `ESC [ A` reassembles to the Up event (not a reply header).
    let mut gate = StdinGate::default();
    assert_eq!(
        push_console_bytes(&mut gate, base, 0, 1, b"\x1b[A"),
        vec![key(KeyCode::Up, KeyModifiers::empty())]
    );
    // An Alt chord: `ESC x`.
    let mut gate = StdinGate::default();
    assert_eq!(
        push_console_bytes(&mut gate, base, 0, 1, b"\x1bx"),
        vec![key(KeyCode::Char('x'), KeyModifiers::ALT)]
    );
    // A bracketed-paste start marker still opens a paste (it is not a reply
    // header) and its payload still leaves as one paste event.
    let mut gate = StdinGate::default();
    let mut out = push_console_bytes(&mut gate, base, 0, 1, b"\x1b[200~ok");
    assert!(out.is_empty());
    assert!(gate.paste_open());
    out.extend(push_console_bytes(&mut gate, base, 20, 1, b"\x1b[201~"));
    assert_eq!(out, vec![Event::Paste("ok".to_string())]);
    // `ESC ] 12` is not an OSC reply target: nothing is claimed, and when the
    // sequence window closes the sequence machinery resolves it to the Alt
    // chord plus its tail.
    let mut gate = StdinGate::default();
    let mut out = push_console_bytes(&mut gate, base, 0, 1, b"\x1b]12");
    assert!(out.is_empty(), "not a reply, not dispatched yet: {out:?}");
    out.extend(gate.flush_due(at(base, 200)));
    assert_eq!(
        out,
        vec![key(KeyCode::Char(']'), KeyModifiers::ALT)]
            .into_iter()
            .chain(typed("12"))
            .collect::<Vec<Event>>()
    );
}

#[test]
fn stdin_gate_leaves_an_esc_stripped_reply_tail_alone() {
    // Documented boundary (see the section comment): the console's own answer
    // arrives with the ESC record already dropped by crossterm, so the tail is
    // plain `Char` events — indistinguishable from a user typing those
    // characters, and the gate must not eat them (`[` is common in text, and
    // upstream has no such rule either: it owns the byte stream and never sees
    // this shape). Closing it needs the record-level stdin reader; when that
    // lands, this test must flip to expecting the tail consumed.
    let base = Instant::now();
    let mut gate = StdinGate::default();
    let out = push_console_bytes(&mut gate, base, 0, 1, b"[?61;6;7c");
    assert_eq!(
        out,
        typed("[?61;6;7c"),
        "an ESC-stripped tail is ordinary input at the gate"
    );
    assert!(gate.held().is_empty());
}

// ---------------------------------------------------------------------------
// Bracketed paste on the event gate (the Windows record parser has no paste
// handling; upstream `stdin-buffer.ts` `pasteMode`)
// ---------------------------------------------------------------------------

/// Feed `bytes` the way crossterm's Windows record parser hands them over:
/// the ESC byte is `KeyCode::Esc`, every other byte a `Char` key press, one
/// event per byte (`event/sys/windows/parse.rs` parses each console record on
/// its own).
fn push_console_bytes(
    gate: &mut StdinGate,
    base: Instant,
    start_ms: u64,
    step_ms: u64,
    bytes: &[u8],
) -> Vec<Event> {
    let mut out = Vec::new();
    for (index, byte) in bytes.iter().enumerate() {
        let event = if *byte == ESC {
            key(KeyCode::Esc, KeyModifiers::empty())
        } else {
            key(KeyCode::Char(*byte as char), KeyModifiers::empty())
        };
        out.extend(gate.push(event, at(base, start_ms + index as u64 * step_ms)));
    }
    out
}

#[test]
fn stdin_gate_turns_a_bracketed_paste_into_one_paste_event() {
    let base = Instant::now();
    let mut gate = StdinGate::default();
    let out = push_console_bytes(&mut gate, base, 0, 1, b"\x1b[200~PASTE4242\x1b[201~");
    assert_eq!(
        out,
        vec![Event::Paste("PASTE4242".to_string())],
        "one paste event with exactly the payload: no marker bytes, no keystrokes"
    );
    assert!(!gate.paste_open());
}

#[test]
fn stdin_gate_collects_a_paste_across_pushes_and_gaps() {
    let base = Instant::now();
    let mut gate = StdinGate::default();
    let mut out = Vec::new();
    // The start marker arrives fragmented across pushes.
    out.extend(push_console_bytes(&mut gate, base, 0, 1, b"\x1b[20"));
    assert!(out.is_empty());
    out.extend(push_console_bytes(&mut gate, base, 4, 1, b"0~frag"));
    assert!(out.is_empty());
    assert!(
        gate.paste_open(),
        "the assembled start marker opens the paste"
    );
    // An open paste has no timer (upstream `pasteMode` sets no deadline), so
    // a long gap in the payload must not flush anything.
    assert!(gate.flush_due(at(base, 5_000)).is_empty());
    assert!(gate.paste_open());
    // The end marker arrives fragmented too, and the payload leaves as one
    // event with the fragments joined in order.
    out.extend(push_console_bytes(
        &mut gate,
        base,
        5_001,
        1,
        b"mented\x1b[201",
    ));
    assert!(out.is_empty(), "a fragmented end marker leaked: {out:?}");
    out.extend(push_console_bytes(&mut gate, base, 5_020, 1, b"~"));
    assert_eq!(out, vec![Event::Paste("fragmented".to_string())]);
    assert!(!gate.paste_open());
}

#[test]
fn stdin_gate_does_not_swallow_the_key_after_a_paste() {
    let base = Instant::now();
    let mut gate = StdinGate::default();
    let mut out = push_console_bytes(&mut gate, base, 0, 1, b"\x1b[200~ab\x1b[201~");
    // The key follows the end marker immediately: it must not be swallowed.
    out.extend(gate.push(key(KeyCode::Char('x'), KeyModifiers::empty()), at(base, 15)));
    out.extend(gate.push(key(KeyCode::Enter, KeyModifiers::empty()), at(base, 16)));
    assert_eq!(
        out,
        vec![
            Event::Paste("ab".to_string()),
            key(KeyCode::Char('x'), KeyModifiers::empty()),
            key(KeyCode::Enter, KeyModifiers::empty()),
        ]
    );
}

#[test]
fn stdin_gate_keeps_an_unterminated_paste_open_until_the_end_marker() {
    // Definition: a lone start marker collects payload with no timer
    // (upstream `pasteMode` has none), so neither the escape nor the sequence
    // timeout ends it; teardown flushes held sequences but never an open
    // paste's payload (upstream `flush()` leaves `pasteBuffer` alone), so an
    // unterminated paste is dropped rather than typed.
    let base = Instant::now();
    let mut gate = StdinGate::default();
    let mut out = push_console_bytes(&mut gate, base, 0, 1, b"\x1b[200~abc");
    assert!(
        out.is_empty(),
        "markers and payload must never be typed: {out:?}"
    );
    assert!(gate.flush_due(at(base, 10_000)).is_empty());
    assert!(gate.paste_open(), "only the end marker closes a paste");
    assert!(gate.flush_all().is_empty());
    assert!(gate.paste_open(), "flush_all leaves the paste state alone");
    // The still-open paste completes with its payload intact.
    out.extend(push_console_bytes(
        &mut gate,
        base,
        20_000,
        1,
        b"def\x1b[201~",
    ));
    assert_eq!(out, vec![Event::Paste("abcdef".to_string())]);
    assert!(!gate.paste_open());
}

#[test]
fn stdin_gate_caps_an_unterminated_paste() {
    // A paste whose end marker never arrives is bounded (pir's deviation from
    // upstream's unbounded `pasteBuffer`): once MAX_PASTE_BYTES were collected
    // the payload is delivered as one paste and the gate returns to normal
    // mode, so the memory is bounded and the data still reaches the editor.
    let base = Instant::now();
    let mut gate = StdinGate::default();
    let mut out = push_console_bytes(&mut gate, base, 0, 1, b"\x1b[200~");
    assert!(out.is_empty() && gate.paste_open());
    // A Unix `Event::Paste` nested in a gate-collected paste is payload too
    // (`collect_paste`), which feeds a payload this size without a million
    // pushes.
    let half = "a".repeat(MAX_PASTE_BYTES / 2);
    out.extend(gate.push(Event::Paste(half.clone()), at(base, 10)));
    assert!(out.is_empty(), "below the cap nothing is delivered yet");
    assert!(gate.paste_open());
    out.extend(gate.push(Event::Paste(half), at(base, 11)));
    assert_eq!(
        out,
        vec![Event::Paste("a".repeat(MAX_PASTE_BYTES))],
        "the cap delivers what was collected, as one paste"
    );
    assert!(!gate.paste_open());
    // The gate is back in normal mode: later input is typed, not collected.
    assert_eq!(
        push_console_bytes(&mut gate, base, 20, 1, b"x"),
        vec![key(KeyCode::Char('x'), KeyModifiers::empty())]
    );
    // ... and a fresh start marker opens a fresh (empty) buffer.
    let mut out = push_console_bytes(&mut gate, base, 30, 1, b"\x1b[200~tail");
    assert!(out.is_empty());
    out.extend(push_console_bytes(&mut gate, base, 50, 1, b"\x1b[201~"));
    assert_eq!(out, vec![Event::Paste("tail".to_string())]);
}

#[test]
fn stdin_gate_does_not_treat_a_bare_marker_run_as_a_paste() {
    // Plain typing of `[200~` carries no ESC, so it is text — not a paste.
    let base = Instant::now();
    let mut gate = StdinGate::default();
    let out = push_console_bytes(&mut gate, base, 0, 1, b"[200~x[201~");
    let expected: Vec<Event> = "[200~x[201~"
        .chars()
        .map(|c| key(KeyCode::Char(c), KeyModifiers::empty()))
        .collect();
    assert_eq!(out, expected, "typed text without ESC is never a paste");
    assert!(!gate.paste_open());
}

#[test]
fn stdin_gate_keeps_a_shift_reported_payload_character() {
    // The console reports a pasted uppercase character together with
    // SHIFT_PRESSED; it is text, not a chord.
    let base = Instant::now();
    let mut gate = StdinGate::default();
    let mut out = push_console_bytes(&mut gate, base, 0, 1, b"\x1b[200~");
    out.extend(gate.push(key(KeyCode::Char('A'), KeyModifiers::SHIFT), at(base, 10)));
    out.extend(push_console_bytes(&mut gate, base, 11, 1, b"\x1b[201~"));
    assert_eq!(out, vec![Event::Paste("A".to_string())]);
}

#[test]
fn stdin_gate_drops_keys_it_cannot_represent_inside_a_paste() {
    let base = Instant::now();
    let mut gate = StdinGate::default();
    let mut out = push_console_bytes(&mut gate, base, 0, 1, b"\x1b[200~ab");
    // Arrows are not plain text and their bytes were consumed by crossterm:
    // dropped, never dispatched as a key.
    assert!(gate
        .push(key(KeyCode::Up, KeyModifiers::empty()), at(base, 10))
        .is_empty());
    // Console-level events are not paste content: they pass through at once.
    assert_eq!(
        gate.push(Event::Resize(80, 24), at(base, 11)),
        vec![Event::Resize(80, 24)]
    );
    out.extend(push_console_bytes(&mut gate, base, 12, 1, b"cd\x1b[201~"));
    assert_eq!(out, vec![Event::Paste("abcd".to_string())]);
}

#[test]
fn stdin_gate_forwards_a_unix_paste_event_unchanged() {
    // Off Windows crossterm parses the markers itself and the gate only
    // forwards the resulting event; that path stays untouched.
    let base = Instant::now();
    let mut gate = StdinGate::default();
    assert_eq!(
        gate.push(Event::Paste("hello".to_string()), base),
        vec![Event::Paste("hello".to_string())]
    );
    // A held ESC precedes a paste as its own key (the pre-existing
    // already-parsed-sequence rule) and the paste itself is untouched.
    assert!(gate
        .push(key(KeyCode::Esc, KeyModifiers::empty()), base)
        .is_empty());
    assert_eq!(
        gate.push(Event::Paste("world".to_string()), at(base, 5)),
        vec![
            key(KeyCode::Esc, KeyModifiers::empty()),
            Event::Paste("world".to_string()),
        ]
    );
}

#[test]
fn stdin_gate_synthesizes_shift_enter_when_native_shift_is_held() {
    // Upstream `forwardInputSequence`: a bare CR on Apple Terminal (or win32)
    // becomes CSI-u Shift+Enter when the native helper reports Shift.
    let base = Instant::now();
    let mut gate = StdinGate::with_native_shift_enter(
        Duration::from_millis(DEFAULT_SEQUENCE_TIMEOUT_MS),
        Duration::from_millis(DEFAULT_ESCAPE_TIMEOUT_MS),
        true,
        shift_pressed,
    );
    assert_eq!(
        gate.push(key(KeyCode::Enter, KeyModifiers::empty()), base),
        vec![key(KeyCode::Enter, KeyModifiers::SHIFT)]
    );

    let mut gate = StdinGate::with_native_shift_enter(
        Duration::from_millis(DEFAULT_SEQUENCE_TIMEOUT_MS),
        Duration::from_millis(DEFAULT_ESCAPE_TIMEOUT_MS),
        true,
        shift_released,
    );
    assert_eq!(
        gate.push(key(KeyCode::Enter, KeyModifiers::empty()), base),
        vec![key(KeyCode::Enter, KeyModifiers::empty())]
    );

    let mut gate = StdinGate::with_native_shift_enter(
        Duration::from_millis(DEFAULT_SEQUENCE_TIMEOUT_MS),
        Duration::from_millis(DEFAULT_ESCAPE_TIMEOUT_MS),
        false,
        shift_pressed,
    );
    assert_eq!(
        gate.push(key(KeyCode::Enter, KeyModifiers::empty()), base),
        vec![key(KeyCode::Enter, KeyModifiers::empty())]
    );
}

// ---------------------------------------------------------------------------
// Apple Terminal Shift+Enter synthesis (upstream terminal.ts:62-73)
// ---------------------------------------------------------------------------

#[test]
fn native_shift_enter_normalization_matches_upstream() {
    assert_eq!(
        normalize_native_shift_enter_input("\r", true, true),
        NATIVE_SHIFT_ENTER_SEQUENCE
    );
    assert_eq!(NATIVE_SHIFT_ENTER_SEQUENCE, "\x1b[13;2u");
    assert_eq!(normalize_native_shift_enter_input("\r", false, true), "\r");
    assert_eq!(normalize_native_shift_enter_input("\r", true, false), "\r");
    assert_eq!(
        normalize_native_shift_enter_input("\x1b[13;2u", true, true),
        "\x1b[13;2u"
    );
    assert_eq!(normalize_native_shift_enter_input("a", true, true), "a");
}

#[test]
fn apple_terminal_normalization_matches_upstream() {
    assert_eq!(
        normalize_apple_terminal_input("\r", true, true),
        NATIVE_SHIFT_ENTER_SEQUENCE
    );
    assert_eq!(normalize_apple_terminal_input("\r", true, false), "\r");
    assert_eq!(normalize_apple_terminal_input("\r", false, true), "\r");
    assert_eq!(normalize_apple_terminal_input("a", true, true), "a");
}

#[test]
fn apple_terminal_session_needs_darwin_and_the_term_program() {
    assert!(is_apple_terminal_session("darwin", Some("Apple_Terminal")));
    // Rust's `std::env::consts::OS` spells macOS "macos"; the environment
    // caller passes that, so the gate must accept it (the old "darwin"-only
    // check was a dead gate).
    assert!(is_apple_terminal_session("macos", Some("Apple_Terminal")));
    assert!(!is_apple_terminal_session("darwin", Some("iTerm.app")));
    assert!(!is_apple_terminal_session("macos", Some("iTerm.app")));
    assert!(!is_apple_terminal_session("linux", Some("Apple_Terminal")));
    assert!(!is_apple_terminal_session(
        "windows",
        Some("Apple_Terminal")
    ));
    assert!(!is_apple_terminal_session("darwin", None));
    assert!(!is_apple_terminal_session("macos", None));
}

#[test]
fn native_shift_detection_gate_matches_upstream_platforms() {
    // Upstream: `isAppleTerminalSession() || process.platform === "win32"`.
    // Both the Node spelling and Rust's `OS` spelling are accepted.
    assert!(native_shift_detection_enabled("win32", None));
    assert!(native_shift_detection_enabled("windows", None));
    assert!(native_shift_detection_enabled(
        "darwin",
        Some("Apple_Terminal")
    ));
    assert!(native_shift_detection_enabled(
        "macos",
        Some("Apple_Terminal")
    ));
    // macOS outside Apple Terminal, and every other platform, stay off.
    assert!(!native_shift_detection_enabled("darwin", Some("iTerm.app")));
    assert!(!native_shift_detection_enabled("macos", Some("iTerm.app")));
    assert!(!native_shift_detection_enabled("linux", None));
    assert!(!native_shift_detection_enabled(
        "linux",
        Some("Apple_Terminal")
    ));
}

#[test]
fn native_shift_state_helpers_read_the_upstream_bits() {
    // Windows: `GetAsyncKeyState`'s `KEY_PRESSED_MASK 0x8000` on a `SHORT`.
    assert!(key_state_is_down(-32768)); // 0x8000
    assert!(key_state_is_down(-1)); // 0xffff: down + pressed since last query
    assert!(!key_state_is_down(0));
    assert!(!key_state_is_down(0x7fff));
    // macOS: `kCGEventFlagMaskShift` in a `CGEventFlags` value.
    assert!(shift_flag_set(0x0002_0000));
    assert!(shift_flag_set(0x0002_0000 | 0x0010_0000));
    assert!(!shift_flag_set(0));
    assert!(!shift_flag_set(0x0001_0000));
    // The platform entry point never panics; off Windows/macOS it is false.
    let pressed = pir::tui::stdin_buffer::native_shift_pressed();
    if !cfg!(any(windows, target_os = "macos")) {
        assert!(!pressed);
    }
}

#[test]
fn stdin_gate_consults_the_shift_provider_only_when_detection_is_enabled() {
    let base = Instant::now();
    let sequence_timeout = Duration::from_millis(DEFAULT_SEQUENCE_TIMEOUT_MS);
    let escape_timeout = Duration::from_millis(DEFAULT_ESCAPE_TIMEOUT_MS);
    SHIFT_PROVIDER_CALLS.store(0, Ordering::SeqCst);

    // Detection disabled: no event may consult the native provider, not even
    // a bare CR (upstream's `shouldDetectNativeShiftEnter` short-circuits the
    // `isNativeModifierPressed` call).
    let mut gate = StdinGate::with_native_shift_enter(
        sequence_timeout,
        escape_timeout,
        false,
        counting_shift_pressed,
    );
    assert_eq!(
        gate.push(key(KeyCode::Enter, KeyModifiers::empty()), base),
        vec![key(KeyCode::Enter, KeyModifiers::empty())]
    );
    assert_eq!(
        gate.push(key(KeyCode::Char('a'), KeyModifiers::empty()), at(base, 1)),
        vec![key(KeyCode::Char('a'), KeyModifiers::empty())]
    );
    assert_eq!(
        gate.push(key(KeyCode::Up, KeyModifiers::empty()), at(base, 2)),
        vec![key(KeyCode::Up, KeyModifiers::empty())]
    );
    assert_eq!(
        SHIFT_PROVIDER_CALLS.load(Ordering::SeqCst),
        0,
        "detection disabled must not touch the platform API"
    );

    // Detection enabled: only a bare CR consults it (the byte rule runs
    // before the provider, exactly like `sequence === "\r" && ...`).
    let mut gate = StdinGate::with_native_shift_enter(
        sequence_timeout,
        escape_timeout,
        true,
        counting_shift_pressed,
    );
    assert_eq!(
        gate.push(key(KeyCode::Enter, KeyModifiers::empty()), base),
        vec![key(KeyCode::Enter, KeyModifiers::SHIFT)]
    );
    assert_eq!(SHIFT_PROVIDER_CALLS.load(Ordering::SeqCst), 1);
    for (index, event) in [
        key(KeyCode::Char('a'), KeyModifiers::empty()),
        key(KeyCode::Char('\n'), KeyModifiers::empty()),
        key(KeyCode::Enter, KeyModifiers::SHIFT),
        key(KeyCode::Enter, KeyModifiers::CONTROL),
        Event::Resize(80, 24),
    ]
    .into_iter()
    .enumerate()
    {
        let _ = gate.push(event, at(base, 10 + index as u64));
    }
    assert_eq!(
        SHIFT_PROVIDER_CALLS.load(Ordering::SeqCst),
        1,
        "only the bare CR is a candidate (an LF shape is not)"
    );
    // `Char('\r')` encodes to the same bare CR byte, so it is a candidate too.
    assert_eq!(
        gate.push(
            key(KeyCode::Char('\r'), KeyModifiers::empty()),
            at(base, 20)
        ),
        vec![key(KeyCode::Enter, KeyModifiers::SHIFT)]
    );
    assert_eq!(SHIFT_PROVIDER_CALLS.load(Ordering::SeqCst), 2);
}

// ---------------------------------------------------------------------------
// PI_TUI_WRITE_LOG (upstream terminal.ts:170-183, appended by write())
// ---------------------------------------------------------------------------

#[test]
fn write_log_path_is_disabled_when_unset_or_empty() {
    let now = UNIX_EPOCH + Duration::from_secs(1_700_000_000);
    assert_eq!(write_log_path(|_| None, now, 4242), None);
    assert_eq!(write_log_path(|_| Some(String::new()), now, 4242), None);
}

#[test]
fn write_log_path_generates_a_file_inside_an_existing_directory() {
    let dir = std::env::temp_dir().join(format!("pir-write-log-dir-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");
    let now = UNIX_EPOCH + Duration::from_secs(1_700_000_000);
    let path = write_log_path(|_| Some(dir.to_string_lossy().into_owned()), now, 4242)
        .expect("directory value resolves to a generated file");
    assert_eq!(path.parent(), Some(dir.as_path()));
    let name = path.file_name().unwrap().to_string_lossy().into_owned();
    assert!(name.starts_with("tui-"), "{name}");
    assert!(name.ends_with(".log"), "{name}");
    assert!(name.contains("4242"), "{name} carries the pid: {name}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn write_log_path_uses_a_non_directory_value_as_the_file_path() {
    let now = UNIX_EPOCH + Duration::from_secs(1_700_000_000);
    let file = std::env::temp_dir().join("pir-write-log-file.log");
    let path = write_log_path(|_| Some(file.to_string_lossy().into_owned()), now, 1);
    assert_eq!(path, Some(file));
}

#[test]
fn write_log_appends_every_output_chunk() {
    let file = std::env::temp_dir().join(format!("pir-write-log-{}.log", std::process::id()));
    let _ = std::fs::remove_file(&file);
    let log = WriteLog::at(file.clone());
    assert!(log.enabled());
    assert_eq!(log.path(), Some(file.as_path()));
    log.append("\x1b[2J");
    log.append("hello");
    let body = std::fs::read_to_string(&file).expect("log file");
    assert_eq!(body, "\x1b[2Jhello");
    // A second sink appends rather than truncating (upstream appendFileSync).
    let log = WriteLog::at(file.clone());
    log.append("!");
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "\x1b[2Jhello!");
    let _ = std::fs::remove_file(&file);
}

#[test]
fn write_log_disabled_writes_nothing() {
    let log = WriteLog::disabled();
    assert!(!log.enabled());
    assert_eq!(log.path(), None);
    log.append("ignored");
    if std::env::var_os("PI_TUI_WRITE_LOG").is_none() {
        assert!(!WriteLog::from_env().enabled());
    }
}

// ---------------------------------------------------------------------------
// Windows VT-input helper (upstream terminal.ts:414-427)
// ---------------------------------------------------------------------------

#[test]
fn windows_vt_input_is_gated_on_win32() {
    assert!(should_enable_windows_vt_input("win32"));
    assert!(!should_enable_windows_vt_input("darwin"));
    assert!(!should_enable_windows_vt_input("linux"));
    if !cfg!(windows) {
        assert_eq!(
            enable_windows_vt_input(),
            VtInputOutcome::UnsupportedPlatform
        );
        assert_eq!(
            windows_vt_input_status(),
            VtInputStatus::UnsupportedPlatform
        );
    }
}

#[test]
fn windows_vt_input_mode_change_only_adds_the_flag_once() {
    // Upstream `mode | ENABLE_VIRTUAL_TERMINAL_INPUT` with the flag already
    // present stays as it is (no console write at all).
    assert_eq!(vt_input_mode_change(0x0001), Some(0x0201));
    assert_eq!(vt_input_mode_change(0x0200), None);
    assert_eq!(vt_input_mode_change(0x0000), Some(0x0200));
    assert_eq!(ENABLE_VIRTUAL_TERMINAL_INPUT, 0x0200);
}

#[test]
fn windows_vt_input_degrades_without_console_mode_access() {
    // Upstream swallows a missing native helper (`catch`): Shift+Tab then
    // arrives as plain Tab. The probe must never panic and must report the
    // degradation when stdin is not a console.
    let status = windows_vt_input_status();
    if cfg!(windows) {
        assert_ne!(
            status,
            VtInputStatus::UnsupportedPlatform,
            "win32 always probes the console handle"
        );
    } else {
        assert_eq!(status, VtInputStatus::UnsupportedPlatform);
    }
    // The byte a VT-input console emits for Shift+Tab decodes to BackTab.
    assert_eq!(
        decode_input_sequence(b"\x1b[Z"),
        Some(vec![key(KeyCode::BackTab, KeyModifiers::SHIFT)])
    );
    assert_eq!(SHIFT_TAB_VT_SEQUENCE, b"\x1b[Z");
    assert!(should_enable_windows_vt_input("win32"));
    // Node spells the platform "win32"; Rust's own const is "windows" and
    // must not accidentally enable the path.
    assert!(!should_enable_windows_vt_input("windows"));
}

#[test]
fn windows_vt_input_is_off_unless_the_env_hatch_says_one() {
    // The normal startup path must NOT install ENABLE_VIRTUAL_TERMINAL_INPUT:
    // crossterm's Windows INPUT_RECORD parser resolves records by virtual key
    // code and cannot round-trip the console's VT translation, so on a real
    // terminal Backspace/Enter go dead and sequence bytes (`[13;2u`) leak into
    // the editor as literal text. Only the documented `1` opts back in,
    // matching the `PIR_VERBOSE` / `PI_EXPERIMENTAL` house style; the value is
    // matched exactly (no trimming, no truthy aliases).
    assert!(!vt_input_opt_in(None));
    assert!(vt_input_opt_in(Some("1")));
    assert!(!vt_input_opt_in(Some("")));
    assert!(!vt_input_opt_in(Some("0")));
    assert!(!vt_input_opt_in(Some("true")));
    assert!(!vt_input_opt_in(Some(" 1")));
}

#[test]
fn decoded_sequences_cover_the_kitty_and_legacy_shapes() {
    assert_eq!(
        decode_input_sequence(b"\x1b[A"),
        Some(vec![key(KeyCode::Up, KeyModifiers::empty())])
    );
    assert_eq!(
        decode_input_sequence(b"\x1b[1;5A"),
        Some(vec![key(KeyCode::Up, KeyModifiers::CONTROL)])
    );
    assert_eq!(
        decode_input_sequence(b"\x1b[A"),
        Some(vec![key(KeyCode::Up, KeyModifiers::empty())])
    );
}

#[test]
fn debug_helpers_do_not_leak_held_state() {
    // `held` exists for diagnostics; a fresh gate holds nothing.
    let gate = StdinGate::default();
    assert!(gate.held().is_empty());
    let _ = ESC;
    let _ = SystemTime::now();
}

// ---------------------------------------------------------------------------
// The raw reply reader's verdicts (the startup color query, D19)
// ---------------------------------------------------------------------------
//
// `theme/osc11.rs` reads the terminal's answer out of the console queue (or
// tty) before crossterm ever runs, so its judgement of what may be taken must
// be stricter than the input path's: input that is not a *claimed* terminal
// reply must stay where it is, or the query would swallow the user's first
// keystrokes on a terminal that never answers.

/// A background reply as a terminal sends it (OSC 11, BEL-terminated).
const OSC11_BG_REPLY: &[u8] = b"\x1b]11;#1a1a2b\x07";
/// A palette reply with an ST terminator (OSC 4).
const OSC4_ST_REPLY: &[u8] = b"\x1b]4;1;rgb:ff/00/00\x1b\\";

/// One input unit per byte — the console's record granularity (a VT reply is
/// queued as one one-character record per byte).
fn byte_units(bytes: &[u8]) -> Vec<Vec<u8>> {
    bytes.iter().map(|byte| vec![*byte]).collect()
}

fn take_from(consumed: &[u8], pending: &[u8]) -> ReplyTake {
    let units = byte_units(pending);
    reply_take(consumed, units.iter().map(Vec::as_slice))
}

#[test]
fn reply_header_prefix_matches_only_the_reply_grammar() {
    for prefix in [
        b"".as_slice(),
        b"\x1b",
        b"\x1b]",
        b"\x1b]1",
        b"\x1b]10",
        b"\x1b]10;",
        b"\x1b]11;",
        b"\x1b]4",
        b"\x1b]4;",
        b"\x1b]4;1",
        b"\x1b]4;123",
        b"\x1b]7501;",
        b"\x1b]7501;?",
        b"\x1bP",
        b"\x1bP>",
        b"\x1bP>|",
        b"\x1b_",
        b"\x1b_G",
        b"\x1b_Ga",
        b"\x1b_Ga=",
        b"\x1b_Ga=1",
        b"\x1b_Ga=1,",
        b"\x1b_Ga=1,b=2;",
        // Device-attributes answer fragments: the CSI introducer, then the
        // `?`/`>` marker, then digits/`;` up to the closing `c`.
        b"\x1b[",
        b"\x1b[?",
        b"\x1b[?6",
        b"\x1b[?1;2",
        b"\x1b[>0;2",
    ] {
        assert!(is_reply_header_prefix(prefix), "{prefix:?}");
    }
    for other in [
        b"q".as_slice(),
        b"/quit\r",
        b"\x1bq",   // alt+q
        b"\x1b]12", // only 10/11 exist
        b"\x1b]4;1234",
        b"\x1b]4;1x",
        b"\x1bX",
        b"\x1bP>|x",
        b"\x1b_Ga=x",
        b"\x1b[A", // an arrow key
        // A byte where the answer's `?`/`>` marker would be means the run can
        // never become the answer: ordinary input and key sequences stay
        // foreign and are never claimed.
        b"\x1b[2",
        b"\x1b[200~",
        b"\x1b[?1;2c", // the complete DA1 answer is a token, not a fragment
        OSC11_BG_REPLY,
    ] {
        assert!(!is_reply_header_prefix(other), "{other:?}");
    }
}

#[test]
fn reply_tokens_len_finds_only_terminated_reply_strings() {
    assert_eq!(reply_tokens_len(b""), 0);
    assert_eq!(reply_tokens_len(b"q"), 0);
    assert_eq!(reply_tokens_len(b"\x1b]11;#1a1a2b"), 0, "no terminator yet");
    assert_eq!(reply_tokens_len(b"\x1b"), 0);
    assert_eq!(reply_tokens_len(OSC11_BG_REPLY), OSC11_BG_REPLY.len());
    assert_eq!(reply_tokens_len(OSC4_ST_REPLY), OSC4_ST_REPLY.len());
    let both = [OSC11_BG_REPLY, OSC4_ST_REPLY].concat();
    assert_eq!(reply_tokens_len(&both), both.len(), "every complete token");
    let with_key = [OSC11_BG_REPLY, b"q".as_slice()].concat();
    assert_eq!(reply_tokens_len(&with_key), OSC11_BG_REPLY.len());
    let with_fragment = [OSC11_BG_REPLY, b"\x1b]1".as_slice()].concat();
    assert_eq!(reply_tokens_len(&with_fragment), OSC11_BG_REPLY.len());
    // `CAN` cancels a string; the bytes up to it are consumed reply bytes.
    assert_eq!(reply_tokens_len(b"\x1b]11;#1a\x18"), 9);
}

#[test]
fn reply_input_state_separates_claimed_replies_from_typed_input() {
    // Nothing, and a fragment that may still grow, mean "keep looking".
    assert_eq!(reply_input_state(b""), ReplyInputState::Waiting);
    assert_eq!(reply_input_state(b"\x1b"), ReplyInputState::Waiting);
    assert_eq!(reply_input_state(b"\x1b]"), ReplyInputState::Waiting);
    assert_eq!(reply_input_state(b"\x1b]11"), ReplyInputState::Waiting);
    // Typed input never becomes a reply.
    assert_eq!(reply_input_state(b"q"), ReplyInputState::Foreign);
    assert_eq!(reply_input_state(b"/quit\r"), ReplyInputState::Foreign);
    assert_eq!(reply_input_state(b"\x1bq"), ReplyInputState::Foreign);
    assert_eq!(reply_input_state(b"\x1b]12"), ReplyInputState::Foreign);
    assert_eq!(reply_input_state(b"\x1b[A"), ReplyInputState::Foreign);
    assert_eq!(reply_input_state(b"\x1b[2"), ReplyInputState::Foreign);
    // The bare CSI introducer may still become the device-attributes answer,
    // so it keeps the reader waiting without taking anything.
    assert_eq!(reply_input_state(b"\x1b["), ReplyInputState::Waiting);
    // The device-attributes answer is a recognized complete token, not
    // "typed input": it is skipped like the OSC replies.
    assert_eq!(reply_input_state(b"\x1b[?1;2c"), ReplyInputState::Waiting);
    // A claimed reply whose string is still open must be consumed through its
    // terminator.
    assert_eq!(reply_input_state(b"\x1b]11;"), ReplyInputState::Open);
    assert_eq!(reply_input_state(b"\x1b]11;#1a1a"), ReplyInputState::Open);
    assert_eq!(reply_input_state(b"\x1b]4;1;rgb:ff"), ReplyInputState::Open);
    // Complete replies are skipped: the verdict is about what follows them.
    assert_eq!(reply_input_state(OSC11_BG_REPLY), ReplyInputState::Waiting);
    assert_eq!(reply_input_state(OSC4_ST_REPLY), ReplyInputState::Waiting);
    let with_key = [b"q".as_slice(), OSC11_BG_REPLY].concat();
    assert_eq!(reply_input_state(&with_key), ReplyInputState::Foreign);
    let with_open_next = [OSC11_BG_REPLY, b"\x1b]10;".as_slice()].concat();
    assert_eq!(reply_input_state(&with_open_next), ReplyInputState::Open);
    // ... and a fragment of one keeps the reader waiting, while a completed
    // answer followed by a keystroke makes the keystroke the verdict.
    assert_eq!(reply_input_state(b"\x1b[?61;"), ReplyInputState::Waiting);
    let answer_then_key = [b"\x1b[?1;2c".as_slice(), b"q".as_slice()].concat();
    assert_eq!(
        reply_input_state(&answer_then_key),
        ReplyInputState::Foreign
    );
}

#[test]
fn reply_take_never_takes_typed_input() {
    // A console that never answers: whatever the user typed is left alone,
    // whether or not the reader has already taken anything.
    assert_eq!(take_from(b"", b""), ReplyTake::Wait);
    assert_eq!(take_from(b"", b"q"), ReplyTake::Stop);
    assert_eq!(take_from(b"", b"/quit\r"), ReplyTake::Stop);
    assert_eq!(take_from(b"", b"\x1b[A"), ReplyTake::Stop);
    assert_eq!(take_from(b"", b"\x1bq"), ReplyTake::Stop);
    // A bare `ESC` stays queued too: it may still turn out to be a reply, and
    // the query window — not a guess — decides when to give up on it.
    assert_eq!(take_from(b"", b"\x1b"), ReplyTake::Wait);
    assert_eq!(take_from(b"", b"\x1b]"), ReplyTake::Wait);
    assert_eq!(take_from(b"", &OSC11_BG_REPLY[..4]), ReplyTake::Wait);
    // A complete reply header claims the run: the reply (header and later its
    // tail) is taken.
    assert_eq!(take_from(b"", &OSC11_BG_REPLY[..5]), ReplyTake::Units(5));
    assert_eq!(
        take_from(b"", OSC11_BG_REPLY),
        ReplyTake::Units(OSC11_BG_REPLY.len())
    );
    assert_eq!(
        take_from(&OSC11_BG_REPLY[..5], &OSC11_BG_REPLY[5..]),
        ReplyTake::Units(OSC11_BG_REPLY.len() - 5),
        "the tail of a claimed reply is taken as it arrives"
    );
    // A key typed right behind a complete reply (or before it) is not taken
    // with the reply.
    let trailed = [OSC11_BG_REPLY, b"q".as_slice()].concat();
    assert_eq!(
        take_from(b"", &trailed),
        ReplyTake::Units(OSC11_BG_REPLY.len())
    );
    let led = [b"q".as_slice(), OSC11_BG_REPLY].concat();
    assert_eq!(take_from(b"", &led), ReplyTake::Stop);
    // While a reply is claimed, a non-key record is taken with it: the tail
    // behind it must stay reachable (a 0-byte unit stands for such a record).
    let non_key: Vec<Vec<u8>> = vec![Vec::new()];
    assert_eq!(
        reply_take(b"\x1b]11;#1a", non_key.iter().map(Vec::as_slice)),
        ReplyTake::Units(1)
    );
    // ... but a non-key record in front of an unclaimed run stops the reader
    // (it cannot be taken, and the reply behind it is not the reader's).
    assert_eq!(
        reply_take(b"", non_key.iter().map(Vec::as_slice)),
        ReplyTake::Wait
    );
}

#[test]
fn reply_take_defers_the_ambiguous_trailing_esc_of_an_open_run() {
    // The last byte of a claimed-but-unterminated reply string may be the
    // first half of its `ST` terminator or the user's own escape key: it
    // cannot be claimed until the next byte decides, so the unit it ends
    // stays queued.
    let body: &[u8] = b"\x1b]11;#1a1a";
    let with_esc = [body, b"\x1b".as_slice()].concat();
    assert_eq!(
        take_from(b"", &with_esc),
        ReplyTake::Units(body.len()),
        "the reply bytes, not the ambiguous trailing ESC"
    );
    // A lone trailing `ESC` in an open run takes nothing at all, and the
    // verdict stays `Wait`: the window, not a guess, ends the wait.
    assert_eq!(take_from(body, b"\x1b"), ReplyTake::Wait);
    // The next byte settles it: `\` makes the ESC the reply's own `ST`, and
    // the whole tail — both records — is consumed (D19) ...
    assert_eq!(take_from(body, b"\x1b\\"), ReplyTake::Units(2));
    assert_eq!(
        take_from(b"", b"\x1b]11;#1a1a\x1b\\"),
        ReplyTake::Units(body.len() + 2),
        "an ST-terminated reply is still consumed whole in one batch"
    );
    // ... while the user's own bytes abort the string: the ESC is not taken
    // and the walk stops there, so the whole key stays queued.
    assert_eq!(take_from(body, b"\x1bq"), ReplyTake::Stop);
    assert_eq!(take_from(body, b"\x1b[200~"), ReplyTake::Stop);
}

#[test]
fn reply_take_never_takes_an_esc_initial_key_behind_a_held_reply() {
    // The interleaving the peek reader must survive: it holds `ESC]11;` (a
    // fragmented OSC 11 answer) and the user presses Up (`ESC [ A`) or
    // pastes (`ESC [ 2 0 0 ~`) before the answer's BEL/ST arrives. The key's
    // leading `ESC` is not reply material — taking it would type `A` or
    // `[200~…` into the prompt.
    let held: &[u8] = b"\x1b]11;";
    assert_eq!(take_from(held, b"\x1b[A"), ReplyTake::Stop);
    assert_eq!(take_from(held, b"\x1b[200~hello"), ReplyTake::Stop);
    // A partial key decides nothing yet: the reader keeps waiting without
    // taking a record.
    assert_eq!(take_from(held, b"\x1b"), ReplyTake::Wait);
    assert_eq!(take_from(held, b"\x1b["), ReplyTake::Wait);
    // The reply bytes before the user's ESC are still taken — with the header
    // as a record of its own, exactly one record is consumed ...
    assert_eq!(
        reply_take(
            b"",
            [held, b"\x1b".as_slice(), b"[".as_slice(), b"A".as_slice(),]
        ),
        ReplyTake::Units(1)
    );
    // ... and with the held header already consumed, nothing is taken at all.
    assert_eq!(
        reply_take(held, [b"\x1b".as_slice(), b"[".as_slice(), b"A".as_slice()]),
        ReplyTake::Stop
    );
    // A paste front behind the held header: same guarantee.
    assert_eq!(
        reply_take(b"", [held, b"\x1b[200~".as_slice(), b"hello".as_slice()]),
        ReplyTake::Units(1)
    );
}

#[test]
fn reply_take_stops_at_the_boundary_a_user_esc_aborts() {
    // `Aborted` claims the bytes *before* the aborting ESC only: when the
    // reply bytes and the user's ESC share a peek batch, exactly the
    // reply-byte records are taken — never the ESC, never what follows it.
    let body: &[u8] = b"\x1b]11;#1a1a";
    assert_eq!(
        take_from(b"", b"\x1b]11;#1a1a\x1b[A"),
        ReplyTake::Units(body.len()),
        "Up behind the reply bytes"
    );
    assert_eq!(
        take_from(b"", b"\x1b]11;#1a1a\x1b[200~"),
        ReplyTake::Units(body.len()),
        "a paste front behind the reply bytes"
    );
    // The aborting ESC inside the record that carries the reply bytes makes
    // the whole record stay: units are never split.
    assert_eq!(
        reply_take(b"", [body, b"\x1b[".as_slice()]),
        ReplyTake::Units(1)
    );
    // With a held header it is the payload record that is inside the claim,
    // the key records that are not.
    assert_eq!(
        reply_take(
            b"\x1b]11;",
            [b"#1a1a".as_slice(), b"\x1b".as_slice(), b"[".as_slice()]
        ),
        ReplyTake::Units(1)
    );
    // A `CAN`-cancelled string still includes its cancel byte in the claim.
    assert_eq!(take_from(b"", b"\x1b]11;#1a\x18"), ReplyTake::Units(9));
}

/// The terminal's answer to the `ESC [ c` request that terminates
/// `TERMINAL_COLOR_QUERY`: the Windows Terminal / conhost shape (seen in the
/// ConPTY battery as `[?61;6;…c` typed into the editor when left queued).
const DA1_REPLY: &[u8] = b"\x1b[?61;6;7;21;22;23;24;28;32;42c";

#[test]
fn reply_take_claims_the_device_attributes_answer() {
    // The whole answer is claimed: the peek reader consumes exactly its
    // records, so none of the `[?61;…c` tail can reach crossterm (which drops
    // only the leading ESC record and types the rest into the prompt).
    assert_eq!(take_from(b"", DA1_REPLY), ReplyTake::Units(DA1_REPLY.len()));
    assert_eq!(
        take_from(b"", b"\x1b[?1;2c"),
        ReplyTake::Units(7),
        "the VT100-with-AVO answer"
    );
    assert_eq!(
        take_from(b"", b"\x1b[?c"),
        ReplyTake::Units(4),
        "an answer with an empty parameter list"
    );
    // A secondary device-attributes answer (DA2) has the same shape.
    assert_eq!(take_from(b"", b"\x1b[>0;276;0c"), ReplyTake::Units(11));
    // A fragment keeps the reader waiting — nothing is taken until the
    // reply's own `c` terminator arrives ...
    assert_eq!(take_from(b"", &DA1_REPLY[..5]), ReplyTake::Wait);
    assert_eq!(reply_tokens_len(&DA1_REPLY[..5]), 0);
    // ... and the tail is taken as it arrives, like a claimed OSC reply
    // (D19: the reply cannot leak into the prompt, not even fragmented).
    assert_eq!(
        take_from(&DA1_REPLY[..5], &DA1_REPLY[5..]),
        ReplyTake::Units(DA1_REPLY.len() - 5)
    );
    assert_eq!(
        take_from(b"\x1b[?61;6", b"c"),
        ReplyTake::Units(1),
        "a fragmented answer: the tail record completes it"
    );
    // The answer behind the color replies in one peek batch: everything the
    // terminal sent is consumed, a keystroke behind it is not.
    let batch = [OSC11_BG_REPLY, DA1_REPLY, b"q".as_slice()].concat();
    assert_eq!(
        take_from(b"", &batch),
        ReplyTake::Units(OSC11_BG_REPLY.len() + DA1_REPLY.len())
    );
    // A keystroke in front stops the reader: the answer behind it is not the
    // reader's to take (it stays queued for the normal input path).
    let led = [b"q".as_slice(), DA1_REPLY].concat();
    assert_eq!(take_from(b"", &led), ReplyTake::Stop);
}

#[test]
fn device_attributes_shape_never_claims_plain_input() {
    // Plain typing never carries the `?`/`>` marker, so it is not claimed —
    // not even when it looks like `[<digits>…` ...
    assert_eq!(take_from(b"", b"["), ReplyTake::Stop);
    assert_eq!(take_from(b"", b"[1;2c"), ReplyTake::Stop);
    assert_eq!(take_from(b"", b"\x1b[2"), ReplyTake::Stop);
    assert_eq!(take_from(b"", b"\x1b[200~"), ReplyTake::Stop);
    // ... nor when a marker-valid fragment ends in a byte that cannot be the
    // answer's `c`.
    assert_eq!(take_from(b"\x1b[?61;6", b"x"), ReplyTake::Stop);
    // The bare introducer takes nothing either; it only keeps the reader
    // looking (the answer may yet start).
    assert_eq!(take_from(b"", b"\x1b["), ReplyTake::Wait);
    // The shape itself: complete only at the `c`, primary/secondary markers
    // only, digits/`;` only.
    assert_eq!(device_attributes_reply_len(b"\x1b[?1;2c"), Some(7));
    assert_eq!(device_attributes_reply_len(b"\x1b[?c"), Some(4));
    assert_eq!(device_attributes_reply_len(b"\x1b[>0;276;0c"), Some(11));
    assert_eq!(device_attributes_reply_len(b"\x1b[?1;2"), None);
    assert_eq!(device_attributes_reply_len(b"\x1b[?1;2x"), None);
    assert_eq!(device_attributes_reply_len(b"\x1b[25h"), None);
    assert_eq!(device_attributes_reply_len(b"\x1b[?25h"), None);
    assert_eq!(device_attributes_reply_len(b"q"), None);
    // The length covers only the reply itself; a trailing byte is not part of
    // it (the reader hands the token slice to the query, never the whole run).
    assert_eq!(device_attributes_reply_len(b"\x1b[?1;2c\x07"), Some(7));
}

#[cfg(windows)]
#[test]
fn console_input_record_mirrors_the_win32_layout() {
    use std::mem::{offset_of, size_of};

    use pir::tui::stdin_buffer::{peek_pending_console_input, ConsoleInputRecord, KEY_EVENT_TYPE};

    // `INPUT_RECORD` (`wincon.h`): WORD EventType, two bytes of padding to the
    // union's four-byte alignment, then the 16-byte union whose leading member
    // is `KEY_EVENT_RECORD` (`BOOL`, WORD, WORD, WORD, WCHAR, DWORD).
    assert_eq!(size_of::<ConsoleInputRecord>(), 20);
    assert_eq!(offset_of!(ConsoleInputRecord, event_type), 0);
    assert_eq!(offset_of!(ConsoleInputRecord, key_down), 4);
    assert_eq!(offset_of!(ConsoleInputRecord, repeat_count), 8);
    assert_eq!(offset_of!(ConsoleInputRecord, u_char), 14);
    assert_eq!(offset_of!(ConsoleInputRecord, control_key_state), 16);
    assert_eq!(KEY_EVENT_TYPE, 1);

    let key = |key_down: i32, repeat_count: u16, u_char: u16| ConsoleInputRecord {
        event_type: KEY_EVENT_TYPE,
        key_down,
        repeat_count,
        u_char,
        ..ConsoleInputRecord::default()
    };
    // A key press stands for its character, repeated `wRepeatCount` times.
    assert_eq!(key(1, 1, u16::from(b'q')).key_bytes(), Some(b"q".to_vec()));
    assert_eq!(
        key(1, 3, u16::from(b'a')).key_bytes(),
        Some(b"aaa".to_vec())
    );
    assert_eq!(key(1, 1, 0x1b).key_bytes(), Some(vec![0x1b]));
    // A key release, a key without a character, a UTF-16 surrogate half and a
    // non-key record are not reply material.
    assert_eq!(key(0, 1, u16::from(b'q')).key_bytes(), None);
    assert_eq!(key(1, 0, u16::from(b'q')).key_bytes(), None);
    assert_eq!(key(1, 1, 0).key_bytes(), None);
    assert_eq!(key(1, 1, 0xd800).key_bytes(), None);
    let resize = ConsoleInputRecord {
        event_type: 0x0004,
        key_down: 1,
        ..ConsoleInputRecord::default()
    };
    assert_eq!(resize.key_bytes(), None);

    // The peek is capped by its argument and asks for nothing when the cap is
    // zero (it may return real records otherwise, which is the reader's job).
    assert!(peek_pending_console_input(0).is_empty());
    assert!(peek_pending_console_input(16).len() <= 16);
}

// ---------------------------------------------------------------------------
// The color query's batch feeding (coalesced replies in one peek batch)
// ---------------------------------------------------------------------------
//
// A terminal answers `TERMINAL_COLOR_QUERY` with up to 19 replies (OSC 10,
// OSC 11, 16 × OSC 4, DA1) and the console can queue them coalesced in one peek
// batch. `TerminalColorQuery::feed` parses exactly one reply — both of its
// parsers are anchored at the ends of the string — so the reader splits the
// batch at the token boundaries (`reply_token_len`) and feeds every complete
// reply on its own (`TerminalColorQuery::feed_replies`); an incomplete trailing
// reply stays in the reader's buffer (`collected`) for the next batch.

/// A foreground reply (OSC 10, BEL-terminated).
const OSC10_FG_REPLY: &[u8] = b"\x1b]10;#e0e0e0\x07";

#[test]
fn reply_token_len_finds_the_first_complete_reply() {
    // Nothing complete at the front: no bytes, typed input, an open string.
    assert_eq!(reply_token_len(b""), 0);
    assert_eq!(reply_token_len(b"q"), 0);
    assert_eq!(reply_token_len(b"\x1b]11;#1a1a2b"), 0, "no terminator yet");
    assert_eq!(reply_token_len(&OSC11_BG_REPLY[..4]), 0);
    // One token: its own length, terminator included.
    assert_eq!(reply_token_len(OSC11_BG_REPLY), OSC11_BG_REPLY.len());
    assert_eq!(reply_token_len(OSC4_ST_REPLY), OSC4_ST_REPLY.len());
    assert_eq!(reply_token_len(DA1_REPLY), DA1_REPLY.len());
    // A coalesced run: only the first token, where `reply_tokens_len` sums
    // every complete token of the run.
    let both = [OSC11_BG_REPLY, OSC4_ST_REPLY].concat();
    assert_eq!(reply_token_len(&both), OSC11_BG_REPLY.len());
    assert_eq!(reply_tokens_len(&both), both.len());
    // A `CAN`-cancelled string is one token (the cancel byte included) ...
    assert_eq!(reply_token_len(b"\x1b]11;#1a\x18"), 9);
    // ... and a partial reply behind a complete one is not part of it.
    let with_partial = [OSC11_BG_REPLY, b"\x1b]4;1;#ff".as_slice()].concat();
    assert_eq!(reply_token_len(&with_partial), OSC11_BG_REPLY.len());
}

#[test]
fn feed_parses_exactly_one_reply_so_a_batch_must_be_split() {
    // The contract the batch feeding is built on: `feed`'s parsers are
    // anchored at both ends, so a concatenation of complete replies is not a
    // reply at all — feeding a whole batch as one string would consume it
    // while recording nothing (the defect the per-token split removes).
    let both = [OSC11_BG_REPLY, OSC4_ST_REPLY].concat();
    let mut query = TerminalColorQuery::new();
    let text = String::from_utf8_lossy(&both);
    assert_eq!(query.feed(&text), TerminalColorFeed::NotAReply);
    assert!(
        query.colors().is_empty(),
        "the concatenation recorded nothing"
    );
    // The same bytes fed through the token walk record every reply.
    let (verdict, consumed) = query.feed_replies(&both);
    assert_eq!(verdict, TerminalColorFeed::Consumed);
    assert_eq!(consumed, both.len());
    assert_eq!(query.colors().background, Some((26, 26, 43)));
}

#[test]
fn color_query_records_every_reply_of_a_coalesced_batch() {
    // Three replies in one peek batch: OSC 10 foreground, OSC 11 background,
    // OSC 4 palette 1 (ST-terminated, as xterm answers).
    let batch = [OSC10_FG_REPLY, OSC11_BG_REPLY, OSC4_ST_REPLY].concat();
    let mut query = TerminalColorQuery::new();
    let (verdict, consumed) = query.feed_replies(&batch);
    assert_eq!(consumed, batch.len(), "every complete reply is consumed");
    assert_eq!(
        verdict,
        TerminalColorFeed::Consumed,
        "3 of the 18 targets: nothing to deliver yet"
    );
    assert_eq!(query.colors().foreground, Some((0xe0, 0xe0, 0xe0)));
    assert_eq!(query.colors().background, Some((26, 26, 43)));
    assert_eq!(
        query.colors().palette,
        None,
        "one palette reply is not all 16"
    );
    // The batch's OSC 4;1 was recorded all the same: the delivery that
    // completes the palette carries it.
    let mut rest = Vec::new();
    for index in (0..TERMINAL_PALETTE_SIZE).filter(|index| *index != 1) {
        rest.extend_from_slice(format!("\x1b]4;{index};#0000ff\x07").as_bytes());
    }
    let colors = match query.feed_replies(&rest) {
        (TerminalColorFeed::Delivered(colors), consumed) => {
            assert_eq!(consumed, rest.len());
            colors
        }
        (other, _) => panic!("the 18th target must deliver: {other:?}"),
    };
    assert!(query.is_complete());
    let palette = colors.palette.expect("all 16 palette entries");
    assert_eq!(palette.len(), TERMINAL_PALETTE_SIZE);
    assert_eq!(palette[0], (0, 0, 255));
    assert_eq!(palette[1], (255, 0, 0), "the batch's OSC 4;1 reply");
}

#[test]
fn color_query_keeps_the_incomplete_tail_of_a_batch_pending() {
    // Two complete replies, then a third whose terminator has not arrived:
    // only the terminated replies are consumed, so the reader keeps the tail
    // in its `collected` buffer and the next batch can complete it.
    let partial: &[u8] = b"\x1b]4;1;rgb:ff/00/00";
    let complete = [OSC10_FG_REPLY, OSC11_BG_REPLY].concat();
    let mut batch = complete.clone();
    batch.extend_from_slice(partial);

    let mut query = TerminalColorQuery::new();
    let (verdict, consumed) = query.feed_replies(&batch);
    assert_eq!(
        consumed,
        complete.len(),
        "only the terminated replies are consumed"
    );
    assert_eq!(verdict, TerminalColorFeed::Consumed);
    assert_eq!(query.colors().foreground, Some((0xe0, 0xe0, 0xe0)));
    assert_eq!(query.colors().background, Some((26, 26, 43)));

    // The tail the reader keeps, completed by the next batch: the reply lands.
    let mut pending = batch[consumed..].to_vec();
    assert_eq!(pending.as_slice(), partial);
    pending.extend_from_slice(b"\x1b\\");
    let (verdict, consumed) = query.feed_replies(&pending);
    assert_eq!(consumed, pending.len(), "the completed reply is consumed");
    assert_eq!(verdict, TerminalColorFeed::Consumed);
    // ... and it was recorded: completing the palette delivers it.
    let mut rest = Vec::new();
    for index in (0..TERMINAL_PALETTE_SIZE).filter(|index| *index != 1) {
        rest.extend_from_slice(format!("\x1b]4;{index};#0000ff\x07").as_bytes());
    }
    let colors = match query.feed_replies(&rest) {
        (TerminalColorFeed::Delivered(colors), _) => colors,
        (other, _) => panic!("the 18th target must deliver: {other:?}"),
    };
    assert_eq!(
        colors.palette.expect("all 16 palette entries")[1],
        (255, 0, 0),
        "the tail reply completed by the second batch was recorded"
    );
}

#[test]
fn color_query_delivers_when_the_batch_carries_da1_behind_the_colors() {
    // The DA1 answer behind the color replies of the same batch: the colors
    // stay and the answer completes the query (upstream resolves on DA1).
    let batch = [OSC11_BG_REPLY, OSC4_ST_REPLY, DA1_REPLY].concat();
    let mut query = TerminalColorQuery::new();
    let (verdict, consumed) = query.feed_replies(&batch);
    assert_eq!(consumed, batch.len(), "the DA1 token is consumed too");
    let colors = match verdict {
        TerminalColorFeed::Delivered(colors) => colors,
        other => panic!("the DA1 answer must complete the query: {other:?}"),
    };
    assert_eq!(colors.foreground, None);
    assert_eq!(colors.background, Some((26, 26, 43)));
    assert_eq!(colors.palette, None, "15 palette entries are still missing");
    assert!(query.is_complete());

    // A reply after the terminator is swallowed — and still consumed, so the
    // reader drops it instead of leaving it for the normal input path.
    let (verdict, consumed) = query.feed_replies(OSC10_FG_REPLY);
    assert_eq!(consumed, OSC10_FG_REPLY.len());
    assert_eq!(verdict, TerminalColorFeed::Consumed);
    assert_eq!(
        query.colors().foreground,
        None,
        "a complete query records nothing new"
    );
}

#[test]
fn color_query_feeds_a_single_reply_batch_exactly_as_before() {
    let mut query = TerminalColorQuery::new();
    // One reply in the batch: the verdict and consumption `feed` had.
    assert_eq!(
        query.feed_replies(OSC11_BG_REPLY),
        (TerminalColorFeed::Consumed, OSC11_BG_REPLY.len())
    );
    assert_eq!(query.colors().background, Some((26, 26, 43)));
    // A re-fed duplicate is ignored, as on the Unix reader's re-feeding path.
    assert_eq!(
        query.feed_replies(OSC11_BG_REPLY),
        (TerminalColorFeed::Consumed, OSC11_BG_REPLY.len())
    );
    // Nothing complete at the front: nothing is consumed, so the reader keeps
    // the bytes (its window, not a guess, decides when to give up).
    assert_eq!(
        query.feed_replies(b"\x1b]11;#1a"),
        (TerminalColorFeed::NotAReply, 0)
    );
    assert_eq!(query.feed_replies(b""), (TerminalColorFeed::NotAReply, 0));
    // A complete reply string that is not a color reply is consumed and
    // dropped (upstream takes every reply string out of its buffer).
    let xtversion: &[u8] = b"\x1bP>|XTVERSION\x1b\\";
    assert_eq!(
        query.feed_replies(xtversion),
        (TerminalColorFeed::NotAReply, xtversion.len())
    );
    assert_eq!(query.colors().background, Some((26, 26, 43)));
    // The DA1 alone delivers the colors reported so far.
    match query.feed_replies(DA1_REPLY) {
        (TerminalColorFeed::Delivered(colors), consumed) => {
            assert_eq!(consumed, DA1_REPLY.len());
            assert_eq!(colors.background, Some((26, 26, 43)));
        }
        (other, _) => panic!("DA1 must deliver: {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// The console's Ctrl+Enter line feed (Windows ConPTY encodes a raw `\n` as a
// Ctrl+Enter chord, `VK_CONTROL` + `VK_RETURN` with `uChar == '\n'`, which
// crossterm maps to `Enter` + CONTROL — a shape no binding matched before
// `resolve_console_line_feed` rewrote it to the codebase's LF shape, `ctrl+j`)
// ---------------------------------------------------------------------------

#[test]
fn stdin_gate_rewrites_the_console_ctrl_enter_line_feed() {
    let base = Instant::now();
    let mut gate = StdinGate::default();
    assert_eq!(
        gate.push(key(KeyCode::Enter, KeyModifiers::CONTROL), at(base, 0)),
        vec![key(KeyCode::Char('j'), KeyModifiers::CONTROL)],
        "the Ctrl+Enter chord becomes the LF shape (ctrl+j)"
    );
    // A release record is not a key press: it passes through as-is and is
    // never rewritten into the LF shape, so it cannot insert text.
    let release = Event::Key(KeyEvent::new_with_kind(
        KeyCode::Enter,
        KeyModifiers::CONTROL,
        KeyEventKind::Release,
    ));
    assert_eq!(gate.push(release.clone(), at(base, 1)), vec![release]);
    // A bare Enter is CR: its own shape, untouched.
    assert_eq!(
        gate.push(key(KeyCode::Enter, KeyModifiers::empty()), at(base, 2)),
        vec![key(KeyCode::Enter, KeyModifiers::empty())],
        "CR keeps its own record shape"
    );
}

#[test]
fn coalesce_paste_burst_turns_a_rapid_run_into_one_paste() {
    let mut queue: std::collections::VecDeque<Event> = ['a', 'b', 'c', 'd', 'e', 'f', 'g', 'h']
        .iter()
        .map(|character| key(KeyCode::Char(*character), KeyModifiers::empty()))
        .collect();
    queue.push_back(key(KeyCode::Enter, KeyModifiers::empty()));
    assert!(coalesce_paste_burst(&mut queue));
    assert_eq!(
        queue.into_iter().collect::<Vec<_>>(),
        vec![
            Event::Paste("abcdefgh".to_string()),
            key(KeyCode::Enter, KeyModifiers::empty()),
        ],
        "a one-line run ending in Enter keeps the submit key outside the paste"
    );

    // A multi-line rapid run is a real paste: every line break, including the
    // trailing one, stays literal so the block does not auto-submit.
    let mut multiline: std::collections::VecDeque<Event> = vec![
        key(KeyCode::Char('a'), KeyModifiers::empty()),
        key(KeyCode::Char('b'), KeyModifiers::empty()),
        key(KeyCode::Enter, KeyModifiers::empty()),
        key(KeyCode::Char('c'), KeyModifiers::empty()),
        key(KeyCode::Char('d'), KeyModifiers::empty()),
        key(KeyCode::Enter, KeyModifiers::empty()),
        key(KeyCode::Char('e'), KeyModifiers::empty()),
        key(KeyCode::Char('f'), KeyModifiers::empty()),
        key(KeyCode::Enter, KeyModifiers::empty()),
    ]
    .into();
    assert!(coalesce_paste_burst(&mut multiline));
    assert_eq!(
        multiline.into_iter().collect::<Vec<_>>(),
        vec![Event::Paste("ab\ncd\nef\n".to_string())],
        "the whole multi-line run becomes one paste"
    );

    // A run with no Enter is text insertion either way and coalesces.
    let mut plain: std::collections::VecDeque<Event> = ['a'; 8]
        .iter()
        .map(|character| key(KeyCode::Char(*character), KeyModifiers::empty()))
        .collect();
    assert!(coalesce_paste_burst(&mut plain));
    assert_eq!(
        plain.into_iter().collect::<Vec<_>>(),
        vec![Event::Paste("aaaaaaaa".to_string())]
    );

    // Held Enter is key repeat, never a paste.
    let mut enters: std::collections::VecDeque<Event> = (0..8)
        .map(|_| key(KeyCode::Enter, KeyModifiers::empty()))
        .collect();
    assert!(!coalesce_paste_burst(&mut enters));
    assert_eq!(enters.len(), 8);

    // A short run is ordinary typing; the coalescer leaves it untouched.
    let mut short: std::collections::VecDeque<Event> = vec![
        key(KeyCode::Char('a'), KeyModifiers::empty()),
        key(KeyCode::Char('b'), KeyModifiers::empty()),
    ]
    .into();
    assert!(!coalesce_paste_burst(&mut short));
    assert_eq!(short.len(), 2);
}

/// P1-D: the Windows line-feed chord (`Char('j')` + CONTROL, what
/// `resolve_console_line_feed` produces for a raw LF) is a line break inside
/// the run, not a run terminator. A short multi-line paste therefore
/// coalesces (regardless of the 8-key threshold) instead of leaving its
/// trailing Enter as a submit key.
#[test]
fn coalesce_paste_burst_treats_the_lf_chord_as_a_line_break() {
    let mut queue: std::collections::VecDeque<Event> = vec![
        key(KeyCode::Char('a'), KeyModifiers::empty()),
        key(KeyCode::Char('j'), KeyModifiers::CONTROL),
        key(KeyCode::Char('b'), KeyModifiers::empty()),
    ]
    .into();
    assert!(coalesce_paste_burst(&mut queue));
    assert_eq!(
        queue.into_iter().collect::<Vec<_>>(),
        vec![Event::Paste("a\nb".to_string())],
        "a short multi-line paste must not auto-submit"
    );

    // An LF-only run is a newline key, never a paste.
    let mut lone_lf: std::collections::VecDeque<Event> =
        vec![key(KeyCode::Char('j'), KeyModifiers::CONTROL)].into();
    assert!(!coalesce_paste_burst(&mut lone_lf));
    assert_eq!(lone_lf.len(), 1);
}

/// P1-D: a pasted CRLF delivers a plain Enter (CR) followed by the LF chord.
/// The pair is ONE line break, and the run is a paste even when it is short
/// or ends on the line break — the CR half must never survive as a submit.
#[test]
fn coalesce_paste_burst_keeps_crlf_pastes_off_the_submit_path() {
    let mut inline: std::collections::VecDeque<Event> = vec![
        key(KeyCode::Char('a'), KeyModifiers::empty()),
        key(KeyCode::Char('b'), KeyModifiers::empty()),
        key(KeyCode::Enter, KeyModifiers::empty()),
        key(KeyCode::Char('j'), KeyModifiers::CONTROL),
        key(KeyCode::Char('c'), KeyModifiers::empty()),
        key(KeyCode::Char('d'), KeyModifiers::empty()),
    ]
    .into();
    assert!(coalesce_paste_burst(&mut inline));
    assert_eq!(
        inline.into_iter().collect::<Vec<_>>(),
        vec![Event::Paste("ab\ncd".to_string())],
        "CRLF collapses to one newline and the block is a paste"
    );

    // A pasted block ending in CRLF: the whole run (trailing break included)
    // is the paste; no Enter key remains to submit it.
    let mut trailing: std::collections::VecDeque<Event> = vec![
        key(KeyCode::Char('a'), KeyModifiers::empty()),
        key(KeyCode::Char('b'), KeyModifiers::empty()),
        key(KeyCode::Enter, KeyModifiers::empty()),
        key(KeyCode::Char('j'), KeyModifiers::CONTROL),
    ]
    .into();
    assert!(coalesce_paste_burst(&mut trailing));
    assert_eq!(
        trailing.into_iter().collect::<Vec<_>>(),
        vec![Event::Paste("ab\n".to_string())]
    );
}

/// P1-D: a genuinely short multi-line paste (plain Enter separator) coalesces
/// without the 8-key burst threshold, while a one-line trailing Enter still
/// submits and a short single-line run stays ordinary typing.
#[test]
fn coalesce_paste_burst_handles_short_multiline_and_keeps_plain_submits() {
    let mut short_multiline: std::collections::VecDeque<Event> = vec![
        key(KeyCode::Char('a'), KeyModifiers::empty()),
        key(KeyCode::Enter, KeyModifiers::empty()),
        key(KeyCode::Char('b'), KeyModifiers::empty()),
    ]
    .into();
    assert!(coalesce_paste_burst(&mut short_multiline));
    assert_eq!(
        short_multiline.into_iter().collect::<Vec<_>>(),
        vec![Event::Paste("a\nb".to_string())]
    );

    // One line + Enter stays keys: the Enter is the submit.
    let mut submit: std::collections::VecDeque<Event> = vec![
        key(KeyCode::Char('a'), KeyModifiers::empty()),
        key(KeyCode::Enter, KeyModifiers::empty()),
    ]
    .into();
    assert!(!coalesce_paste_burst(&mut submit));
    assert_eq!(submit.len(), 2);

    // Tab is paste payload/separator too: a trailing Enter after it belongs
    // to the block instead of submitting it.
    let mut tabbed: std::collections::VecDeque<Event> = vec![
        key(KeyCode::Char('a'), KeyModifiers::empty()),
        key(KeyCode::Tab, KeyModifiers::empty()),
        key(KeyCode::Enter, KeyModifiers::empty()),
    ]
    .into();
    assert!(coalesce_paste_burst(&mut tabbed));
    assert_eq!(
        tabbed.into_iter().collect::<Vec<_>>(),
        vec![Event::Paste("a\t\n".to_string())],
        "the tab marks the run as paste content"
    );
}

#[test]
fn stdin_gate_maps_the_ctrl_enter_chord_into_an_open_paste() {
    let base = Instant::now();
    let mut gate = StdinGate::default();
    let mut out = push_console_bytes(&mut gate, base, 0, 1, b"\x1b[200~a");
    assert!(out.is_empty());
    out.extend(gate.push(key(KeyCode::Enter, KeyModifiers::CONTROL), at(base, 100)));
    assert!(
        out.is_empty(),
        "the chord must be collected into the payload, not emitted: {out:?}"
    );
    out.extend(push_console_bytes(&mut gate, base, 101, 1, b"b\x1b[201~"));
    assert_eq!(
        out,
        vec![Event::Paste("a\nb".to_string())],
        "the chord contributes the byte `0x0a`, the byte a Unix parser carries"
    );
}

// ---------------------------------------------------------------------------
// Legacy X10 mouse reports (ESC-prefixed)
//
// `ESC [ M` + three payload bytes is the pre-SGR encoding upstream's crossterm
// parses on Unix (`event/sys/unix/parse.rs:690-716`). The completeness rule
// already held the 6-byte shape; the decode (`decode_x10_mouse`) turns it into
// the same `Event::Mouse` crossterm produces, so a fragmented report assembled
// at the gate is no longer completed and then dropped. Only this ESC-prefixed
// spelling is decoded: the ESC-less X10 tail is indistinguishable from typing
// and stays unclaimed (the SGR claim exists only because the Windows console
// drops the report's ESC record, and that path runs in the SGR encoding).
// ---------------------------------------------------------------------------

#[test]
fn decode_x10_mouse_matches_crossterms_grammar() {
    // crossterm's own X10 vector (`test_parse_csi_normal_mouse`):
    // `ESC [ M 0 \x60 \x70` — button byte '0' (48) - 32 = 16 (the control bit),
    // column 0x60 - 33 = 63, row 0x70 - 33 = 79.
    let control_left_press = MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: 63,
        row: 79,
        modifiers: KeyModifiers::CONTROL,
    };
    assert_eq!(
        decode_input_sequence(b"\x1b[M0\x60\x70"),
        Some(vec![Event::Mouse(control_left_press)])
    );
    assert_eq!(
        decode_x10_mouse(b"\x1b[M0\x60\x70"),
        Some(control_left_press)
    );

    // The release marker: no `m` suffix exists here, so a release is the third
    // button code (`\x23` - 32 = 3) and decodes to `Up(Left)` with the
    // coordinates still carried (column 0, row 1).
    assert_eq!(
        decode_x10_mouse(b"\x1b[M\x23\x21\x22"),
        Some(MouseEvent {
            kind: MouseEventKind::Up(MouseButton::Left),
            column: 0,
            row: 1,
            modifiers: KeyModifiers::empty(),
        })
    );
    // Motion sets bit 32: `@` (64) - 32 = 32 drags the left button, `C` (67) -
    // 32 = 35 is the button-less move crossterm maps to `Moved`.
    assert_eq!(
        decode_x10_mouse(b"\x1b[M@\x21\x22").map(|mouse| mouse.kind),
        Some(MouseEventKind::Drag(MouseButton::Left))
    );
    assert_eq!(
        decode_x10_mouse(b"\x1b[M\x43\x21\x22").map(|mouse| mouse.kind),
        Some(MouseEventKind::Moved)
    );
    // The wheel: `\x60` - 32 = 64 → ScrollUp, `\x61` → ScrollDown — the same
    // offsets upstream's `parseWheelEvent` X10 branch applies.
    assert_eq!(
        decode_x10_mouse(b"\x1b[M\x60\x21\x22").map(|mouse| mouse.kind),
        Some(MouseEventKind::ScrollUp)
    );
    assert_eq!(
        decode_x10_mouse(b"\x1b[M\x61\x21\x22").map(|mouse| mouse.kind),
        Some(MouseEventKind::ScrollDown)
    );

    // Truncated: five of the six bytes are incomplete, and no event is
    // invented from them.
    assert_eq!(
        is_complete_sequence(b"\x1b[M\x23\x21"),
        SequenceStatus::Incomplete
    );
    assert_eq!(
        is_complete_sequence(b"\x1b[M\x23\x21\x22"),
        SequenceStatus::Complete
    );
    assert_eq!(decode_x10_mouse(b"\x1b[M\x23\x21"), None);
    assert_eq!(decode_input_sequence(b"\x1b[M\x23\x21"), None);

    // Out-of-range coordinates: the encoded position is one-based, so a byte
    // below 33 cannot be a cell (`byte - 33` would be negative) — the report
    // is consumed but no coordinate is invented.
    assert_eq!(decode_x10_mouse(b"\x1b[M0\x20\x22"), None);
    assert_eq!(decode_x10_mouse(b"\x1b[M0\x21\x20"), None);
    // The highest byte still maps plainly (255 - 33 = 222); the decoder has no
    // terminal size to clamp against.
    assert_eq!(
        decode_x10_mouse(b"\x1b[M\x20\xff\xff"),
        Some(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 222,
            row: 222,
            modifiers: KeyModifiers::empty(),
        })
    );

    // Grammar rejections: a button byte below 32, an unknown button number
    // (the high bits select a button outside 0..=7) and payloads that are not
    // exactly three bytes are not reports.
    assert_eq!(decode_x10_mouse(b"\x1b[M\x1f\x21\x22"), None);
    assert_eq!(decode_x10_mouse(b"\x1b[M\xa0\x21\x22"), None);
    assert_eq!(decode_x10_mouse(b"\x1b[M\x23\x21\x22\x23"), None);
    assert_eq!(decode_x10_mouse(b"\x1b[<0;1;1M"), None);
}

#[test]
fn stdin_gate_decodes_a_fragmented_x10_mouse_report() {
    // The report reaches the gate one event at a time (`ESC`, `[`, `M`, the
    // three payload bytes) and reassembles into the mouse event crossterm's
    // byte parser produces — the same event an indivisible read gets.
    let base = Instant::now();
    let mut gate = StdinGate::default();
    let mut out = Vec::new();
    for (index, byte) in b"\x1b[M\x23\x21\x22".iter().enumerate() {
        let event = if *byte == ESC {
            key(KeyCode::Esc, KeyModifiers::empty())
        } else {
            key(KeyCode::Char(*byte as char), KeyModifiers::empty())
        };
        out.extend(gate.push(event, at(base, index as u64)));
    }
    assert_eq!(
        out,
        vec![Event::Mouse(MouseEvent {
            kind: MouseEventKind::Up(MouseButton::Left),
            column: 0,
            row: 1,
            modifiers: KeyModifiers::empty(),
        })]
    );
    assert!(gate.held().is_empty());

    // The ESC-less X10 tail is NOT claimed: the gate has no self-identifying
    // shape for it (a user typing `[M` plus three characters is the same
    // input), so the bytes stay typed — the `[` is released the moment the
    // `M` proves it cannot start an SGR report, and the rest follows.
    let mut gate = StdinGate::default().with_console_mouse_claim(true);
    let typed_out = push_console_report(&mut gate, base, 0, 1, "[M#!\"", false);
    assert_eq!(typed_out, typed_console("[M#!\""));
    assert!(gate.held().is_empty());
}

// ---------------------------------------------------------------------------
// ESC-less console mouse reports at the input gate
//
// The Windows console translates the terminal's SGR mouse report into one key
// record per byte and crossterm drops the record that carries the ESC (the
// same shape as the DA1 tail), so the report reaches the gate as a run of
// plain `Char` events spelling `[<Cb;Cx;CyM|m`. The gate's
// `claim_console_mouse` (default on Windows; forced on here so the tests are
// host-independent) holds the run while `classify_sgr_mouse_tail` reports a
// prefix, ignores the key-up record each press is paired with, and turns a
// complete report into exactly the `Event::Mouse` crossterm's byte parser
// produces. Every run the grammar does not claim is released as its staged
// key events — the claim never swallows a byte.
// ---------------------------------------------------------------------------

/// The console synthesizes a character with `VkKeyScan`'s shift state
/// (`<` and `M` carry SHIFT_PRESSED on a US layout); crossterm mirrors it into
/// the modifiers.
fn console_shift(c: char) -> bool {
    matches!(c, '<' | 'M')
}

/// The same text the way the console delivers it as plain typing: one press
/// record per byte, `VkKeyScan`'s shift state on `<`/`M` ([`console_shift`]).
fn typed_console(text: &str) -> Vec<Event> {
    text.chars()
        .map(|c| {
            key(
                KeyCode::Char(c),
                if console_shift(c) {
                    KeyModifiers::SHIFT
                } else {
                    KeyModifiers::empty()
                },
            )
        })
        .collect()
}

/// Feed one report text the way the console delivers it when the ESC record is
/// dropped: one key-press record per byte and (when `releases`) the key-up
/// record that follows it, with the console's shift state on `<`/`M`.
fn push_console_report(
    gate: &mut StdinGate,
    base: Instant,
    start_ms: u64,
    step_ms: u64,
    text: &str,
    releases: bool,
) -> Vec<Event> {
    let mut out = Vec::new();
    let mut step = 0u64;
    for c in text.chars() {
        let modifiers = if console_shift(c) {
            KeyModifiers::SHIFT
        } else {
            KeyModifiers::empty()
        };
        let kinds: &[KeyEventKind] = if releases {
            &[KeyEventKind::Press, KeyEventKind::Release]
        } else {
            &[KeyEventKind::Press]
        };
        for kind in kinds {
            out.extend(gate.push(
                Event::Key(KeyEvent::new_with_kind(KeyCode::Char(c), modifiers, *kind)),
                at(base, start_ms + step * step_ms),
            ));
            step += 1;
        }
    }
    out
}

fn mouse(kind: MouseEventKind, column: u16, row: u16, modifiers: KeyModifiers) -> Event {
    Event::Mouse(MouseEvent {
        kind,
        column,
        row,
        modifiers,
    })
}

#[test]
fn console_mouse_tail_classifier_pins_the_claimed_shapes() {
    assert_eq!(classify_sgr_mouse_tail(b""), SgrMouseTail::NotTail);
    assert_eq!(classify_sgr_mouse_tail(b"a"), SgrMouseTail::NotTail);
    assert_eq!(
        classify_sgr_mouse_tail(b"\x1b[<64;40;20M"),
        SgrMouseTail::NotTail
    );
    // Prefixes: held until the report completes (or the window expires).
    for prefix in [
        "[",
        "[<",
        "[<6",
        "[<64",
        "[<64;",
        "[<64;4",
        "[<64;4;",
        "[<64;40",
        "[<64;40;",
        "[<64;40;2",
        "[<64;40;20",
    ] {
        assert_eq!(
            classify_sgr_mouse_tail(prefix.as_bytes()),
            SgrMouseTail::Open,
            "{prefix:?}"
        );
    }
    // Complete reports (`M` press, `m` release).
    for report in ["[<64;40;20M", "[<64;40;20m", "[<0;1;1M", "[<35;20;5m"] {
        assert_eq!(
            classify_sgr_mouse_tail(report.as_bytes()),
            SgrMouseTail::Complete,
            "{report:?}"
        );
    }
    // Runs that started like a tail but can never become one.
    for invalid in [
        "[a",
        "[x",
        "[<;",
        "[<a",
        "[<64;;40",
        "[<1;2;3;4M",
        "[<1;2M",
        "[<1M",
        "[<64;40;20MM",
        "[<64;40;20M ",
    ] {
        assert_eq!(
            classify_sgr_mouse_tail(invalid.as_bytes()),
            SgrMouseTail::Invalid,
            "{invalid:?}"
        );
    }
    // The decoder is crossterm's grammar: a complete shape with an unknown
    // button code decodes to `None` (and is released by the gate).
    assert_eq!(
        classify_sgr_mouse_tail(b"[<98;1;1M"),
        SgrMouseTail::Complete
    );
    assert!(decode_sgr_mouse_tail(b"[<64;40;20M").is_some());
    assert!(decode_sgr_mouse_tail(b"[<98;1;1M").is_none());
    assert!(decode_sgr_mouse_tail(b"[<64;40;20").is_none());
}

#[test]
fn stdin_gate_claims_an_esc_less_console_mouse_report() {
    let base = Instant::now();
    // Wheel-up: `ESC [ < 64 ; 40 ; 20 M` with the ESC record dropped, press
    // and release records interleaved (the console's per-byte synthesis).
    let mut gate = StdinGate::default().with_console_mouse_claim(true);
    let out = push_console_report(&mut gate, base, 0, 1, "[<64;40;20M", true);
    assert_eq!(
        out,
        vec![mouse(
            MouseEventKind::ScrollUp,
            39,
            19,
            KeyModifiers::empty()
        )],
        "wheel-up at 1-based (40,20)"
    );
    assert!(gate.held().is_empty(), "nothing may stay held");
    // The same report without the release records: identical event.
    let mut gate = StdinGate::default().with_console_mouse_claim(true);
    let out = push_console_report(&mut gate, base, 0, 1, "[<64;40;20M", false);
    assert_eq!(
        out,
        vec![mouse(
            MouseEventKind::ScrollUp,
            39,
            19,
            KeyModifiers::empty()
        )]
    );
}

#[test]
fn stdin_gate_claims_every_sgr_mouse_shape_crossterm_models() {
    let base = Instant::now();
    let cases: &[(&str, MouseEventKind, KeyModifiers)] = &[
        (
            "[<0;10;5M",
            MouseEventKind::Down(MouseButton::Left),
            KeyModifiers::empty(),
        ),
        (
            "[<1;10;5M",
            MouseEventKind::Down(MouseButton::Middle),
            KeyModifiers::empty(),
        ),
        (
            "[<2;10;5M",
            MouseEventKind::Down(MouseButton::Right),
            KeyModifiers::empty(),
        ),
        (
            "[<0;10;5m",
            MouseEventKind::Up(MouseButton::Left),
            KeyModifiers::empty(),
        ),
        (
            "[<1;10;5m",
            MouseEventKind::Up(MouseButton::Middle),
            KeyModifiers::empty(),
        ),
        (
            "[<2;10;5m",
            MouseEventKind::Up(MouseButton::Right),
            KeyModifiers::empty(),
        ),
        (
            "[<32;10;5M",
            MouseEventKind::Drag(MouseButton::Left),
            KeyModifiers::empty(),
        ),
        (
            "[<33;10;5M",
            MouseEventKind::Drag(MouseButton::Middle),
            KeyModifiers::empty(),
        ),
        (
            "[<34;10;5M",
            MouseEventKind::Drag(MouseButton::Right),
            KeyModifiers::empty(),
        ),
        ("[<35;10;5M", MouseEventKind::Moved, KeyModifiers::empty()),
        (
            "[<64;10;5M",
            MouseEventKind::ScrollUp,
            KeyModifiers::empty(),
        ),
        (
            "[<65;10;5M",
            MouseEventKind::ScrollDown,
            KeyModifiers::empty(),
        ),
        (
            "[<66;10;5M",
            MouseEventKind::ScrollLeft,
            KeyModifiers::empty(),
        ),
        (
            "[<67;10;5M",
            MouseEventKind::ScrollRight,
            KeyModifiers::empty(),
        ),
        ("[<68;10;5M", MouseEventKind::ScrollUp, KeyModifiers::SHIFT),
        ("[<72;10;5M", MouseEventKind::ScrollUp, KeyModifiers::ALT),
        (
            "[<80;10;5M",
            MouseEventKind::ScrollUp,
            KeyModifiers::CONTROL,
        ),
        (
            "[<92;10;5M",
            MouseEventKind::ScrollUp,
            KeyModifiers::SHIFT | KeyModifiers::ALT | KeyModifiers::CONTROL,
        ),
    ];
    for (text, kind, modifiers) in cases {
        let mut gate = StdinGate::default().with_console_mouse_claim(true);
        let out = push_console_report(&mut gate, base, 0, 1, text, true);
        assert_eq!(
            out,
            vec![mouse(*kind, 9, 4, *modifiers)],
            "{text} must become exactly one mouse event"
        );
        assert!(gate.held().is_empty(), "{text} left bytes held");
    }
}

#[test]
fn stdin_gate_claims_a_console_mouse_report_split_across_reads() {
    let base = Instant::now();
    let mut gate = StdinGate::default().with_console_mouse_claim(true);
    // The first fragment ends mid-report; its window (sequence timeout once
    // past the single `[`) must cover a slow continuation.
    let mut out = push_console_report(&mut gate, base, 0, 1, "[<64", true);
    assert!(out.is_empty(), "a fragment leaked: {out:?}");
    assert!(
        gate.flush_due(at(base, 11)).is_empty(),
        "the escape window alone must not release a `[<` run"
    );
    let deadline = gate.pending_deadline().expect("the run is held");
    assert!(
        deadline >= at(base, 50),
        "a `[<` run waits out the sequence window, got {deadline:?}"
    );
    out.extend(push_console_report(&mut gate, base, 20, 1, ";40;20M", true));
    assert_eq!(
        out,
        vec![mouse(
            MouseEventKind::ScrollUp,
            39,
            19,
            KeyModifiers::empty()
        )]
    );
    assert!(gate.held().is_empty());
}

#[test]
fn stdin_gate_releases_broken_console_mouse_tails_as_input() {
    let base = Instant::now();
    // A run that stops being a report is dispatched immediately, in order.
    let mut gate = StdinGate::default().with_console_mouse_claim(true);
    assert_eq!(
        push_console_report(&mut gate, base, 0, 1, "[a", true),
        typed("[a")
    );
    assert!(gate.held().is_empty());
    // A complete shape outside crossterm's button grammar (cb 98) is released,
    // not swallowed — and the key-up of the byte that ended the run goes with
    // the run, not into the editor.
    let mut gate = StdinGate::default().with_console_mouse_claim(true);
    assert_eq!(
        push_console_report(&mut gate, base, 0, 1, "[<98;1;1M", true),
        typed_console("[<98;1;1M")
    );
    assert!(gate.held().is_empty());
    // Four parameters never complete: released the moment the `;` arrives.
    let mut gate = StdinGate::default().with_console_mouse_claim(true);
    assert_eq!(
        push_console_report(&mut gate, base, 0, 1, "[<1;2;3;4M", false),
        typed_console("[<1;2;3;4M")
    );
    assert!(gate.held().is_empty());
    // A report whose terminator never arrives expires as typed input.
    let mut gate = StdinGate::default().with_console_mouse_claim(true);
    assert!(push_console_report(&mut gate, base, 0, 1, "[<64;40", false).is_empty());
    assert_eq!(
        gate.flush_due(at(base, 100)),
        typed_console("[<64;40"),
        "an unterminated report must never be swallowed"
    );
    assert!(gate.held().is_empty());
}

#[test]
fn stdin_gate_holds_a_typed_bracket_only_for_the_sequence_window() {
    let base = Instant::now();
    let mut gate = StdinGate::default().with_console_mouse_claim(true);
    // A lone `[` may start a report: it is held for the SEQUENCE window (the
    // console delivers a report's records one read at a time, and the gaps
    // between them exceed the 10 ms escape window — see
    // `scripts/tui_conpty/scenarios/s34_console_mouse_no_typing.py`), and a
    // `[` that goes nowhere is dispatched unchanged. Any byte that cannot
    // continue a report releases it at once (the Invalid arm).
    assert!(gate
        .push(key(KeyCode::Char('['), KeyModifiers::empty()), base)
        .is_empty());
    assert_eq!(
        gate.pending_deadline(),
        Some(at(base, DEFAULT_SEQUENCE_TIMEOUT_MS))
    );
    assert!(gate
        .flush_due(at(base, DEFAULT_SEQUENCE_TIMEOUT_MS - 1))
        .is_empty());
    assert_eq!(
        gate.flush_due(at(base, DEFAULT_SEQUENCE_TIMEOUT_MS)),
        typed("[")
    );
    assert!(gate.held().is_empty());
}

#[test]
fn stdin_gate_never_swallows_a_second_press_of_the_claimed_key() {
    // The console reports each byte of an ESC-less mouse report as a
    // press/release pair. The gate claims the `[` press and remembers its
    // key-up; that release is swallowed. A genuine second `[` press — the
    // user typing a bracket while the claim is pending — must never be
    // mistaken for the partner: BOTH presses reach the editor, in order.
    // (This is the invariant that makes the code-only release pairing sound:
    // presses are never swallowed, and the TUI input loop only acts on
    // presses.)
    let base = Instant::now();
    let mut gate = StdinGate::default().with_console_mouse_claim(true);
    let press = || key(KeyCode::Char('['), KeyModifiers::empty());
    let release = || {
        Event::Key(KeyEvent::new_with_kind(
            KeyCode::Char('['),
            KeyModifiers::empty(),
            KeyEventKind::Release,
        ))
    };

    // The claimed byte's press is held; its key-up is swallowed with it.
    assert!(gate.push(press(), base).is_empty());
    assert!(
        gate.push(release(), at(base, 1)).is_empty(),
        "the claimed byte's release must not be dispatched"
    );

    // A second real press is delivered: the run stops being a report prefix,
    // so the held `[` flushes together with it.
    let mut out = gate.push(press(), at(base, 2));
    out.extend(gate.flush_due(at(base, 100)));
    assert_eq!(
        out,
        vec![press(), press()],
        "both presses must reach the editor — a press is never swallowed"
    );
    assert!(gate.held().is_empty());
}

#[test]
fn stdin_gate_keeps_the_console_mouse_claim_off_by_request() {
    let base = Instant::now();
    // Off (the non-Windows default): the tail is ordinary typing, as before
    // the claim — the byte parser path already synthesizes mouse events.
    let mut gate = StdinGate::default().with_console_mouse_claim(false);
    assert_eq!(
        push_console_report(&mut gate, base, 0, 1, "[<64;40;20M", false),
        typed_console("[<64;40;20M")
    );
    assert!(gate.held().is_empty());
}

#[test]
fn stdin_gate_keeps_keys_and_reply_tails_around_a_console_mouse_report() {
    let base = Instant::now();
    let mut gate = StdinGate::default().with_console_mouse_claim(true);
    let mut out = typed("a");
    out.extend(push_console_report(
        &mut gate,
        base,
        0,
        1,
        "[<65;3;4M",
        false,
    ));
    out.extend(typed("b"));
    assert_eq!(
        out,
        vec![
            key(KeyCode::Char('a'), KeyModifiers::empty()),
            mouse(MouseEventKind::ScrollDown, 2, 3, KeyModifiers::empty()),
            key(KeyCode::Char('b'), KeyModifiers::empty()),
        ],
        "the report is claimed in place, the keys around it are untouched"
    );
    // The ESC-stripped reply tail is not a mouse report: it stays ordinary
    // input (the documented boundary the DA1 test above pins).
    let out = push_console_report(&mut gate, base, 40, 1, "[?61;6;7c", false);
    assert_eq!(out, typed("[?61;6;7c"));
    // A typed ESC behind a held `[` still reaches the editor: the `[` is
    // released first, then the escape is held for its own window.
    let mut gate = StdinGate::default().with_console_mouse_claim(true);
    assert!(gate
        .push(key(KeyCode::Char('['), KeyModifiers::empty()), base)
        .is_empty());
    assert_eq!(
        gate.push(key(KeyCode::Esc, KeyModifiers::empty()), at(base, 2)),
        typed("[")
    );
    assert_eq!(
        gate.flush_due(at(base, 2 + DEFAULT_ESCAPE_TIMEOUT_MS)),
        vec![key(KeyCode::Esc, KeyModifiers::empty())]
    );
}
