//! Stdin decoding — the port of upstream `packages/tui/src/stdin-buffer.ts`
//! (`StdinBuffer`) together with the input half of
//! `packages/tui/src/terminal.ts`.
//!
//! # Upstream anchors
//!
//! * `stdin-buffer.ts:22-29` — `ESC`, the 50 ms sequence / 10 ms escape
//!   timeouts, bracketed-paste markers and the terminal-reply header pattern.
//! * `stdin-buffer.ts:38-188` — `isCompleteSequence` (CSI / OSC / DCS / APC /
//!   SS3 / meta-key completeness, including the SGR-mouse shape check).
//! * `stdin-buffer.ts:201-279` — `extractCompleteSequences` (reply-string
//!   scan, ESC+ESC+CSI split, per-character fallback).
//! * `stdin-buffer.ts:320-471` — `process` / `flush` / paste handling, the
//!   "lone ESC uses `escapeTimeout`, everything else the sequence timeout"
//!   rule, and the expiry flush split for control-string prefixes.
//! * `terminal.ts:128-145` — `resolveEscapeTimeoutMs` (`PI_TUI_ESC_TIMEOUT`,
//!   100 ms over SSH, 10 ms otherwise).
//! * `terminal.ts:62-73`, `:390-400` — `normalizeNativeShiftEnterInput` /
//!   `normalizeAppleTerminalInput` / `isAppleTerminalSession` and the
//!   `shouldDetectNativeShiftEnter` gate.
//! * `terminal.ts:170-183`, `:521-530` — `PI_TUI_WRITE_LOG` resolution and
//!   the `appendFileSync` on every `terminal.write()`.
//! * `terminal.ts:414-427` — `enableWindowsVTInput`
//!   (`native/win32/src/win32-platform.c:33-48`).
//! * `crossterm-0.29.0/src/event/sys/unix/parse.rs:690-716` —
//!   `parse_csi_normal_mouse`, the legacy X10 report (`ESC [ M` + 3 bytes)
//!   the Unix byte parser decodes; [`decode_x10_mouse`] is its port, sharing
//!   that file's `parse_cb` port ([`decode_mouse_cb`]) with the SGR decoder
//!   (`:718-761` `parse_csi_sgr_mouse`, `:776-810` `parse_cb`). pir keeps
//!   crossterm as the byte parser, so the X10 shape is decoded in the same
//!   reassembly seam that reproduces crossterm's other parses.
//!
//! # pir wiring and deliberate deviations
//!
//! pir keeps crossterm as the byte parser (replacing it with a hand-written
//! escape parser would put every existing terminal behavior at risk), so the
//! live input loop cannot feed raw bytes into [`StdinBuffer`]. Instead:
//!
//! * [`StdinGate`] applies the same decision table to crossterm's *parsed*
//!   events: a bare Escape is held for the escape window, a following
//!   introducer byte keeps the sequence open for the sequence window, and the
//!   held bytes are reassembled with [`decode_input_sequence`] — the subset of
//!   sequences crossterm would have produced had the bytes arrived in one
//!   read. This is wired into `tui::terminal::run`'s input loop.
//! * A recognized *terminal reply* is consumed at the same gate, whenever it
//!   arrives: the reply grammar of [`super::terminal_reply`] is applied to the
//!   held run ([`StdinGate::settle_reply_run`]), so a color/DA1/XTVersion/Kitty
//!   answer that arrives outside the startup query's window is consumed through
//!   its terminator — the late DA1 the boot query no longer waits for included
//!   — instead of reaching the editor, fragmented replies included. Only a run
//!   that still carries its own `ESC` can be claimed, and only when the walker
//!   recognizes its header (a complete device-attributes answer, or a control
//!   string whose header is one of the reply headers); everything the grammar
//!   does not claim — arrows, Alt chords, paste markers, fragments that are not
//!   yet a full header — passes through the sequence machinery unchanged. The
//!   boundary is the *ESC-stripped* tail: the Windows console queues a reply as
//!   one record per byte and crossterm drops the record that carries the ESC
//!   (`VK 0`, no character), so the tail (`[?61;…c`) arrives as plain `Char`
//!   keys — bytes indistinguishable from a user typing them, which this gate
//!   therefore does **not** eat (upstream has no such rule either: it owns the
//!   byte stream, so it never sees the shape). Claiming that tail needs the
//!   console input to be owned at the *record* level (the `PeekConsoleInputW`
//!   machinery [`peek_pending_console_input`] already provides for the boot
//!   query, driven from `tui::terminal::run`'s loop before crossterm's reader),
//!   where the dropped ESC record is still visible; until then the tail reaches
//!   the editor, as pinned by
//!   `tests/suites/tui_stdin_decoding.rs::stdin_gate_leaves_an_esc_stripped_reply_tail_alone`.
//!   A claimed reply whose terminator never arrives is held for the sequence
//!   window and then released by the sequence machinery's own rule (for a
//!   control string that is upstream's Alt-prefix split, so `Alt+]` typing is
//!   not eaten). A run that outgrows [`MAX_HELD_BYTES`]/[`MAX_HELD_EVENTS`]
//!   is released as ordinary input instead of waiting for a terminator that
//!   never comes.
//! * *Console mouse reports* are claimed at the same gate ([`StdinGate`]'s
//!   `claim_console_mouse`, on by default on Windows). The same dropped-ESC
//!   record that leaves the DA1 tail as `Char` keys leaves an SGR mouse report
//!   (`ESC [ < Cb ; Cx ; Cy M|m`, the shape `EnableMouseCapture` asks the
//!   terminal for) as the ESC-less tail `[<Cb;Cx;CyM|m` — one key press record
//!   per byte, each followed by its key-up record. The gate holds a run that
//!   starts with `[` (the sequence window — see [`StdinGate::settle_mouse_run`])
//!   while
//!   [`classify_sgr_mouse_tail`] reports [`SgrMouseTail::Open`], ignores the
//!   key-up records that belong to the same burst (they carry no bytes), and
//!   on [`SgrMouseTail::Complete`] emits the `Event::Mouse` the ESC-prefixed
//!   decoder would have produced ([`decode_sgr_mouse_tail`], sharing
//!   `decode_sgr_mouse`/`decode_mouse_cb` with the byte path). The claimed
//!   shapes are exactly crossterm's SGR grammar: wheel up/down/left/right,
//!   left/middle/right press and release, drags and motion, with the
//!   shift/alt/control bits. A run that turns out not to be a report
//!   ([`SgrMouseTail::Invalid`], a complete shape whose button code the grammar
//!   does not model, a run that times out) is released as its staged key
//!   events unchanged — the claim never swallows, and never types, a byte of
//!   it. Only the *ESC-less* tail is claimed here; a run that still carries its
//!   own `ESC` stays with the sequence machinery (`ESC [ < … M|m` is held by
//!   `is_complete_sgr_mouse_payload` and decoded by `decode_input_sequence`,
//!   upstream's rule for that spelling). The cost of claiming without the ESC
//!   is a typed `[` waiting out the sequence window before it is dispatched
//!   (it is released immediately when the next event cannot continue a
//!   report); the gain is that the whole interaction family (wheel scroll,
//!   drag selection, scrollbar) works on the default Windows input path.
//! * *Legacy X10 mouse reports* (`ESC [ M` + 3 bytes) are decoded on the
//!   ESC-prefixed byte path only ([`decode_x10_mouse`], the port of
//!   crossterm's `parse_csi_normal_mouse`): upstream's parser decodes them on
//!   Unix, and the completeness rule already holds the 6-byte shape
//!   ([`is_complete_sequence`], upstream `stdin-buffer.ts:51-55`), so a
//!   fragmented report that reaches the gate as `ESC` + characters used to be
//!   *dropped* by `complete_held` (no decoder modeled the shape) and now
//!   becomes the same [`Event::Mouse`] crossterm would have produced. The
//!   ESC-less X10 tail (`[M` + 3 bytes) is deliberately **not** claimed by
//!   [`SgrMouseTail`]: unlike `[<Cb;Cx;CyM|m` it is not self-identifying —
//!   `[M` followed by three typed characters is byte-for-byte the same input
//!   — and the SGR claim exists only because the Windows console drops the
//!   record that carries the report's `ESC` while the terminal is in the SGR
//!   mouse mode `EnableMouseCapture` asks for, so no X10 report arrives that
//!   way. An out-of-range coordinate byte (below 33) is rejected — `None`
//!   rather than crossterm's debug panic / release wrap to 65535 — a
//!   deliberate deviation recorded in [`decode_x10_mouse`]'s docs.
//! * Bracketed paste behind the same seam. On Unix crossterm parses
//!   `ESC [ 2 0 0 ~ … ESC [ 2 0 1 ~` into one `Event::Paste`, which the gate
//!   forwards untouched. Its Windows `INPUT_RECORD` parser has no paste
//!   handling at all (`event/sys/windows/parse.rs` translates one console
//!   record per char), so whenever the console forwards the markers they
//!   arrive as separate key events; the gate reassembles the start marker
//!   through the same byte walk as every escape sequence, collects the payload
//!   as raw bytes until the end marker, and emits the Unix path's
//!   `Event::Paste(String)` — the markers are never decoded (they are not key
//!   sequences) and the payload is never typed as keystrokes. Upstream's
//!   byte-level `pasteMode` (`stdin-buffer.ts:346-410`) is the model: a paste
//!   has no timer while it is open, so its payload may take arbitrarily long;
//!   an unterminated paste is never flushed (upstream `flush()` leaves
//!   `pasteBuffer` alone), so its content is dropped at teardown rather than
//!   typed — and its payload is capped at [`MAX_PASTE_BYTES`] (pir's
//!   deviation, upstream grows it unbounded), at which point it is delivered
//!   as one paste and the gate returns to normal mode.
//!   ConPTY itself, though, does not forward them: in the console mode pir
//!   runs in (`PIR_TUI_VT_INPUT` off) a bracketed paste written into the
//!   pseudoconsole arrives as synthesized key records with the markers
//!   swallowed — `scripts/tui_conpty/probe_paste_records.py` records nothing
//!   at all for `ESC [ 2 0 0 ~` alone, and an `ESC [ A` between the markers
//!   comes back as a real Up arrow. Pasted text is therefore no different from
//!   typed text on this path and the collector stays dormant there; what such
//!   a payload leaves behind is the line feed shape below.
//! * Line feed from the console: ConPTY synthesizes a Ctrl+Enter chord for a
//!   raw `\n`, which [`StdinGate::push`] resolves to the LF key shape
//!   (`ctrl+j`) so a pasted or typed line feed inserts a line break instead of
//!   vanishing — see [`resolve_console_line_feed`].
//! * [`StdinBuffer`] itself stays the byte-level model of the same rules (the
//!   `PI_TUI_WRITE_LOG`-era upstream code path) and is the seam the tests
//!   drive; the reply-string rules are shared with
//!   [`super::terminal_reply`], the port of the same upstream functions.
//! * Expiry flush: upstream emits the whole held buffer as one sequence, which
//!   the key layer then resolves (`\x1b]` → `alt+]`, `\x1b[<35` → nothing).
//!   pir reproduces those resolutions instead of re-entering crossterm's
//!   parser: a two-byte control-string prefix becomes an Alt chord, a
//!   control-string prefix plus tail replays the tail as its own keys, and an
//!   unparseable multi-byte partial is dropped (never typed as text). Anything
//!   pir cannot model is flushed *unsplit* rather than swallowed, so no key is
//!   lost relative to the pre-port eager parse.
//! * The Shift+Enter synthesis reads the physical Shift state like upstream's
//!   native helper (`isNativeModifierPressed`): `GetAsyncKeyState` on Windows
//!   (any of `VK_SHIFT`/`VK_LSHIFT`/`VK_RSHIFT`) and `CGEventSourceFlagsState`
//!   with the combined session state on macOS (`kCGEventFlagMaskShift`), both
//!   behind the documented `native_shift` `unsafe_code` exception; every other
//!   platform reports `false`. The gate takes the provider as a parameter too,
//!   so the port stays testable. On Windows crossterm additionally reports
//!   `VK_RETURN` + `SHIFT_PRESSED` records as Shift+Enter already
//!   (`event/sys/windows/parse.rs:258`).
//! * Upstream's `enableModifyOtherKeys` (`ESC [ > 4 ; 2 m`) is a deliberate
//!   deviation: crossterm's parser has no modifyOtherKeys support, so enabling
//!   it would turn Shift+Enter into a `CSI 27 ; 2 ; 13 ~` sequence the parser
//!   drops. The gate accepts the spellings crossterm can parse instead — CSI-u
//!   `ESC [ 13 ; 2 u` (also keypad `57414;2u`) and `ESC [ 13 ; 2 ~`.
//! * `PI_TUI_WRITE_LOG` is resolved like upstream and appended on every
//!   [`super::core::Terminal::write`] chunk (the single output choke point);
//!   upstream's direct `process.stdout.write` calls (progress, cleanup dump)
//!   are not logged there either. The generated file name uses UTC instead of
//!   JS local time.
//! * [`enable_windows_vt_input`] performs upstream's console-mode change
//!   (`GetStdHandle` / `GetConsoleMode` / `SetConsoleMode |
//!   ENABLE_VIRTUAL_TERMINAL_INPUT`) and the TUI restores the previous mode on
//!   cleanup, which upstream does not do; a console that refuses the call
//!   reports [`VtInputOutcome::Unavailable`] — the degradation upstream
//!   swallows (Shift+Tab would then arrive as plain Tab). The TUI installs the
//!   flag only behind the `PIR_TUI_VT_INPUT=1` opt-in: upstream parses the
//!   console's own VT translation, while crossterm's Windows `INPUT_RECORD`
//!   parser cannot round-trip it, so the default startup path leaves the
//!   console mode untouched (see the call site in [`super::terminal`]).
//! * [`peek_pending_console_input`] adds the one raw Win32 call the TUI makes
//!   (`PeekConsoleInputW`, via the `console_peek` module): the startup color
//!   query must judge the console's pending input *before* taking anything out
//!   of the queue, and `crossterm_winapi` wraps no peek. It is the documented
//!   `unsafe_code` exception of this module.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crossterm::event::{
    Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};

use super::terminal_reply::{has_terminal_reply_header, is_control_string, scan_reply, ReplyScan};

/// `ESC`.
pub const ESC: u8 = 0x1b;
/// `BEL`, the OSC terminator.
pub const BEL: u8 = 0x07;
/// Upstream `stdin-buffer.ts:23` `DEFAULT_SEQUENCE_TIMEOUT_MS`.
pub const DEFAULT_SEQUENCE_TIMEOUT_MS: u64 = 50;
/// Upstream `stdin-buffer.ts:24` `DEFAULT_ESCAPE_TIMEOUT_MS`.
pub const DEFAULT_ESCAPE_TIMEOUT_MS: u64 = 10;
/// Upstream `terminal.ts:129` `DEFAULT_SSH_ESCAPE_TIMEOUT_MS`.
pub const DEFAULT_SSH_ESCAPE_TIMEOUT_MS: u64 = 100;
/// `ESC [ 2 0 0 ~` — bracketed-paste start (`stdin-buffer.ts:25`).
pub const BRACKETED_PASTE_START: &[u8] = b"\x1b[200~";
/// `ESC [ 2 0 1 ~` — bracketed-paste end (`stdin-buffer.ts:26`).
pub const BRACKETED_PASTE_END: &[u8] = b"\x1b[201~";
/// Byte cap for an open bracketed paste on the event gate (pir's deviation;
/// upstream `stdin-buffer.ts:346-410` has no bound). A payload is collected
/// until this many bytes have arrived; the paste is then treated as ended —
/// what was collected is delivered as one [`Event::Paste`] and the gate
/// returns to normal mode, so the bytes still reach the editor while the
/// buffer stays bounded. 8 MiB is far above any real paste (a full-screen
/// terminal's paste is a few KiB) and far below a size that would threaten the
/// process; an unterminated paste therefore costs at most this much.
pub const MAX_PASTE_BYTES: usize = 8 * 1024 * 1024;

/// The event-loop coalescer's threshold: a run of at least this many rapid
/// text key events in one drained input batch becomes one `Event::Paste`.
/// Kept in the event loop (not the per-key gate) so key-sequence parsing and
/// its tests keep their one-event-per-key contract.
pub const PASTE_COALESCE_RUN: usize = 8;
/// Byte cap for the gate's held escape/reply run (pir's deviation: upstream
/// reassembles its sequence buffer without a bound). A legitimate reply, mouse
/// report or escape sequence is at most a few dozen bytes; a continuous
/// unterminated run (an OSC/CSI that never terminates, a console mouse tail
/// that never completes) would otherwise grow `held`/`held_events` for as long
/// as the input keeps arriving, because every byte refreshes its deadline. On
/// reaching the cap the run is released as ordinary input — the same path a
/// run that cannot begin a recognized reply takes — so nothing is dropped and
/// the buffer stays bounded. 64 KiB is orders of magnitude above any real held
/// run and far below a size that would threaten the process.
pub const MAX_HELD_BYTES: usize = 64 * 1024;
/// Event-count cap for the gate's held run: the byte cap alone still allows
/// 65K one-byte key events, and every staged event also costs a `held_ends`
/// entry. 4096 events is far above any real held run and bounds that
/// per-event bookkeeping.
pub const MAX_HELD_EVENTS: usize = 4 * 1024;
/// Upstream `terminal.ts:17` `NATIVE_SHIFT_ENTER_SEQUENCE`.
pub const NATIVE_SHIFT_ENTER_SEQUENCE: &str = "\x1b[13;2u";
/// The legacy `Shift`+`F3`-shaped spelling upstream's editor also accepts as
/// Shift+Enter (`components/editor.ts:895`); the gate shadows Shift+F3 with it.
const NATIVE_SHIFT_ENTER_LEGACY: &[u8] = b"\x1b[13;2~";
/// The Shift+Tab byte a console in VT-input mode delivers
/// (`terminal.ts:414-419`; plain Tab arrives as `\t` when the mode cannot be
/// enabled).
pub const SHIFT_TAB_VT_SEQUENCE: &[u8] = b"\x1b[Z";
/// `ENABLE_VIRTUAL_TERMINAL_INPUT` (`native/win32/src/win32-platform.c:40`).
pub const ENABLE_VIRTUAL_TERMINAL_INPUT: u32 = 0x0200;

// ---------------------------------------------------------------------------
// Timeouts (upstream terminal.ts:128-145)
// ---------------------------------------------------------------------------

/// Resolve the escape-reassembly window (upstream `resolveEscapeTimeoutMs`):
/// `PI_TUI_ESC_TIMEOUT` when it parses to a positive number, else 100 ms when
/// the session is remote (`SSH_CONNECTION` / `SSH_TTY`), else 10 ms.
/// Fractional values truncate to whole milliseconds.
pub fn resolve_escape_timeout_ms(get: impl FnMut(&str) -> Option<String>) -> u64 {
    let mut get = get;
    if let Some(raw) = get("PI_TUI_ESC_TIMEOUT") {
        if let Ok(configured) = raw.trim().parse::<f64>() {
            if configured.is_finite() && configured > 0.0 {
                return configured as u64;
            }
        }
    }
    if get("SSH_CONNECTION").is_some() || get("SSH_TTY").is_some() {
        return DEFAULT_SSH_ESCAPE_TIMEOUT_MS;
    }
    DEFAULT_ESCAPE_TIMEOUT_MS
}

/// [`resolve_escape_timeout_ms`] over the process environment.
pub fn escape_timeout_ms_from_env() -> u64 {
    resolve_escape_timeout_ms(|key| std::env::var(key).ok())
}

// ---------------------------------------------------------------------------
// Sequence completeness (upstream stdin-buffer.ts:38-188)
// ---------------------------------------------------------------------------

/// How complete an escape sequence is (upstream `isCompleteSequence`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SequenceStatus {
    /// The bytes do not start an escape sequence.
    NotEscape,
    /// More bytes are needed before the sequence can be dispatched.
    Incomplete,
    /// The sequence is complete.
    Complete,
}

/// Classify an accumulated buffer (upstream `isCompleteSequence`).
pub fn is_complete_sequence(data: &[u8]) -> SequenceStatus {
    if data.first() != Some(&ESC) {
        return SequenceStatus::NotEscape;
    }
    if data.len() == 1 {
        return SequenceStatus::Incomplete;
    }
    let after_esc = &data[1..];
    match after_esc[0] {
        b'[' => {
            // Old-style mouse: `ESC [ M` plus three payload bytes.
            if after_esc.starts_with(b"[M") {
                return if data.len() >= 6 {
                    SequenceStatus::Complete
                } else {
                    SequenceStatus::Incomplete
                };
            }
            is_complete_csi_sequence(data)
        }
        b']' => is_complete_osc_sequence(data),
        b'P' => is_complete_dcs_sequence(data),
        b'_' => is_complete_apc_sequence(data),
        b'O' => {
            if after_esc.len() >= 2 {
                SequenceStatus::Complete
            } else {
                SequenceStatus::Incomplete
            }
        }
        // Meta key (`ESC` + one character) or an unknown escape sequence:
        // upstream treats both as complete.
        _ => SequenceStatus::Complete,
    }
}

/// CSI sequences end with a byte in `0x40..=0x7e`; SGR mouse reports are held
/// until their `[<b;x;yM|m]` shape is complete (upstream
/// `isCompleteCsiSequence`).
fn is_complete_csi_sequence(data: &[u8]) -> SequenceStatus {
    let Some(payload) = data.strip_prefix(b"\x1b[".as_slice()) else {
        return SequenceStatus::Complete;
    };
    if data.len() < 3 {
        return SequenceStatus::Incomplete;
    }
    let Some(&last) = payload.last() else {
        return SequenceStatus::Incomplete;
    };
    if !(0x40..=0x7e).contains(&last) {
        return SequenceStatus::Incomplete;
    }
    if payload.starts_with(b"<") && !is_complete_sgr_mouse_payload(payload) {
        return SequenceStatus::Incomplete;
    }
    SequenceStatus::Complete
}

/// `<\d+;\d+;\d+[Mm]` (upstream's SGR-mouse shape check).
fn is_complete_sgr_mouse_payload(payload: &[u8]) -> bool {
    if !payload.starts_with(b"<") || payload.len() < 2 {
        return false;
    }
    let last = *payload.last().unwrap_or(&0);
    if last != b'M' && last != b'm' {
        return false;
    }
    let body = &payload[1..payload.len() - 1];
    let parts: Vec<&[u8]> = body.split(|byte| *byte == b';').collect();
    parts.len() == 3
        && parts
            .iter()
            .all(|part| !part.is_empty() && part.iter().all(u8::is_ascii_digit))
}

/// OSC strings end with `BEL` or ST (`ESC \`).
fn is_complete_osc_sequence(data: &[u8]) -> SequenceStatus {
    if data.ends_with(&[ESC, b'\\']) || data.ends_with(&[BEL]) {
        SequenceStatus::Complete
    } else {
        SequenceStatus::Incomplete
    }
}

/// DCS strings end with ST (`ESC \`).
fn is_complete_dcs_sequence(data: &[u8]) -> SequenceStatus {
    if data.ends_with(&[ESC, b'\\']) {
        SequenceStatus::Complete
    } else {
        SequenceStatus::Incomplete
    }
}

/// APC strings end with ST (`ESC \`).
fn is_complete_apc_sequence(data: &[u8]) -> SequenceStatus {
    if data.ends_with(&[ESC, b'\\']) {
        SequenceStatus::Complete
    } else {
        SequenceStatus::Incomplete
    }
}

// ---------------------------------------------------------------------------
// Byte-level reassembly (upstream StdinBuffer)
// ---------------------------------------------------------------------------

/// One sequence (or paste payload) the buffer emitted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StdinEvent {
    /// A complete input sequence (upstream `data` event).
    Data(Vec<u8>),
    /// Bracketed-paste content (upstream `paste` event).
    Paste(Vec<u8>),
}

/// Buffers stdin bytes and emits complete sequences (upstream `StdinBuffer`).
///
/// The upstream constructor's `setTimeout` callbacks are modelled as explicit
/// deadlines: [`Self::process`] flushes anything that expired before the new
/// chunk (what the event loop would already have done) and [`Self::flush_due`]
/// plays the timer callback.
#[derive(Debug)]
pub struct StdinBuffer {
    buffer: Vec<u8>,
    deadline: Option<Instant>,
    sequence_timeout: Duration,
    escape_timeout: Duration,
    paste_mode: bool,
    paste_buffer: Vec<u8>,
    pending_kitty_printable: Option<u32>,
}

impl Default for StdinBuffer {
    fn default() -> Self {
        Self::with_timeouts(
            Duration::from_millis(DEFAULT_SEQUENCE_TIMEOUT_MS),
            Duration::from_millis(DEFAULT_ESCAPE_TIMEOUT_MS),
        )
    }
}

impl StdinBuffer {
    /// Upstream `StdinBufferOptions` (`timeout` / `escapeTimeout`).
    pub fn with_timeouts(sequence_timeout: Duration, escape_timeout: Duration) -> Self {
        Self {
            buffer: Vec::new(),
            deadline: None,
            sequence_timeout,
            escape_timeout,
            paste_mode: false,
            paste_buffer: Vec::new(),
            pending_kitty_printable: None,
        }
    }

    /// A buffer whose windows come from the environment (upstream
    /// `new StdinBuffer({ escapeTimeout: resolveEscapeTimeoutMs() })`).
    pub fn from_env() -> Self {
        Self::with_timeouts(
            Duration::from_millis(DEFAULT_SEQUENCE_TIMEOUT_MS),
            Duration::from_millis(escape_timeout_ms_from_env()),
        )
    }

    /// Feed a raw stdin chunk; returns the sequences ready to dispatch.
    pub fn process(&mut self, data: &[u8], now: Instant) -> Vec<StdinEvent> {
        let mut out = Vec::new();
        self.process_into(data, now, &mut out);
        out
    }

    fn process_into(&mut self, data: &[u8], now: Instant, out: &mut Vec<StdinEvent>) {
        // Upstream's entry `clearTimeout`: a timer that already came due has
        // fired by now, so its flush precedes this chunk.
        self.expire_into(now, out);

        // Upstream's high-byte conversion: a lone byte above 127 is the meta
        // (ESC-prefixed) form of the byte minus 128.
        //
        // pir reads raw bytes, so the rule must not fire for a byte that is
        // the START of a UTF-8 character (or a continuation byte that belongs
        // to one already buffered): doing so turns a Windows IME / UTF-8
        // character split across reads into `ESC` + a garbage metakey. Lead
        // bytes and their continuations stay in `buffer` until the character
        // is complete (see `extract_complete_sequences`); a truly lone
        // high byte (0x80..=0xC1, 0xF5..=0xFF) keeps upstream's conversion.
        let chunk: Vec<u8> = if data.len() == 1
            && data[0] > 127
            && !is_utf8_lead_byte(data[0])
            && !buffer_holds_partial_utf8(&self.buffer)
        {
            vec![ESC, data[0] - 128]
        } else {
            data.to_vec()
        };

        if chunk.is_empty() && self.buffer.is_empty() {
            out.push(StdinEvent::Data(Vec::new()));
            return;
        }

        self.buffer.extend_from_slice(&chunk);

        if self.paste_mode {
            let buffer = std::mem::take(&mut self.buffer);
            self.paste_buffer.extend_from_slice(&buffer);
            self.finish_paste_if_closed(now, out);
            return;
        }

        if let Some(start) = find_subslice(&self.buffer, BRACKETED_PASTE_START) {
            if start > 0 {
                let before = self.buffer[..start].to_vec();
                let (sequences, _) = extract_complete_sequences(&before);
                for sequence in sequences {
                    self.emit_data_sequence(sequence, out);
                }
            }
            self.pending_kitty_printable = None;
            let rest = self.buffer[start + BRACKETED_PASTE_START.len()..].to_vec();
            self.buffer = rest;
            self.paste_mode = true;
            self.paste_buffer = std::mem::take(&mut self.buffer);
            self.finish_paste_if_closed(now, out);
            return;
        }

        let (sequences, remainder) = extract_complete_sequences(&self.buffer);
        self.buffer = remainder;
        for sequence in sequences {
            self.emit_data_sequence(sequence, out);
        }

        if self.buffer.is_empty() {
            self.deadline = None;
            return;
        }
        // A recognized terminal-reply header waits for its terminator with no
        // timer at all: flushing it would type the tail as text and turn an
        // OSC's BEL into Ctrl+G.
        if has_terminal_reply_header(&self.buffer) {
            self.deadline = None;
            return;
        }
        let window = if self.buffer.len() == 1 {
            self.escape_timeout
        } else {
            self.sequence_timeout
        };
        self.deadline = Some(now + window);
    }

    /// Close an in-flight paste once its `ESC [ 2 0 1 ~` arrives.
    fn finish_paste_if_closed(&mut self, now: Instant, out: &mut Vec<StdinEvent>) {
        let Some(end) = find_subslice(&self.paste_buffer, BRACKETED_PASTE_END) else {
            self.deadline = None;
            return;
        };
        let content = self.paste_buffer[..end].to_vec();
        let remaining = self.paste_buffer[end + BRACKETED_PASTE_END.len()..].to_vec();
        self.paste_mode = false;
        self.paste_buffer.clear();
        self.pending_kitty_printable = None;
        out.push(StdinEvent::Paste(content));
        if !remaining.is_empty() {
            self.process_into(&remaining, now, out);
        }
    }

    /// Emit one sequence through the Kitty duplicate filter (upstream
    /// `emitDataSequence`).
    fn emit_data_sequence(&mut self, sequence: Vec<u8>, out: &mut Vec<StdinEvent>) {
        let raw_codepoint = single_codepoint(&sequence);
        if raw_codepoint.is_some() && raw_codepoint == self.pending_kitty_printable {
            self.pending_kitty_printable = None;
            return;
        }
        self.pending_kitty_printable = kitty_printable_codepoint(&sequence);
        out.push(StdinEvent::Data(sequence));
    }

    /// Emit whatever the pending timeout would have emitted by `now`.
    pub fn flush_due(&mut self, now: Instant) -> Vec<StdinEvent> {
        let mut out = Vec::new();
        self.expire_into(now, &mut out);
        out
    }

    /// The pending flush deadline, when bytes are held.
    pub fn pending_deadline(&self) -> Option<Instant> {
        self.deadline
    }

    fn expire_into(&mut self, now: Instant, out: &mut Vec<StdinEvent>) {
        match self.deadline {
            Some(deadline) if now >= deadline => {}
            _ => return,
        }
        self.deadline = None;
        let sequences = self.flush();
        for sequence in sequences {
            if sequence.len() > 2 && is_control_string(&sequence) {
                // Upstream's recovery: emit the two-byte Alt prefix and reparse
                // the tail so batched text, Enter and control keys are not
                // swallowed as one event.
                self.emit_data_sequence(sequence[..2].to_vec(), out);
                let tail = sequence[2..].to_vec();
                if !tail.is_empty() {
                    let mut tail_out = Vec::new();
                    self.process_into(&tail, now, &mut tail_out);
                    out.append(&mut tail_out);
                }
            } else {
                self.emit_data_sequence(sequence, out);
            }
        }
    }

    /// Emit the held bytes as they are (upstream `flush`).
    pub fn flush(&mut self) -> Vec<Vec<u8>> {
        self.deadline = None;
        if self.buffer.is_empty() {
            return Vec::new();
        }
        self.pending_kitty_printable = None;
        vec![std::mem::take(&mut self.buffer)]
    }

    /// Currently held bytes.
    pub fn buffer(&self) -> &[u8] {
        &self.buffer
    }

    /// Drop buffered bytes without emitting (upstream `clear`).
    pub fn clear(&mut self) {
        self.deadline = None;
        self.buffer.clear();
        self.paste_mode = false;
        self.paste_buffer.clear();
        self.pending_kitty_printable = None;
    }

    /// Sequence / escape windows this buffer was built with.
    pub fn timeouts(&self) -> (Duration, Duration) {
        (self.sequence_timeout, self.escape_timeout)
    }
}

/// Split an accumulated buffer into complete sequences (upstream
/// `extractCompleteSequences`). The remainder is what must stay buffered.
fn extract_complete_sequences(buffer: &[u8]) -> (Vec<Vec<u8>>, Vec<u8>) {
    let mut sequences = Vec::new();
    let mut pos = 0;
    while pos < buffer.len() {
        let remaining = &buffer[pos..];
        if remaining[0] != ESC {
            // Not an escape sequence: emit one whole character. A UTF-8
            // character split across reads is held until its continuation
            // bytes arrive (never emitted as a partial byte run).
            let len = utf8_char_len(remaining[0]);
            if len > 1 && remaining.len() < len {
                return (sequences, remaining.to_vec());
            }
            let len = len.min(remaining.len());
            sequences.push(remaining[..len].to_vec());
            pos += len;
            continue;
        }

        if has_terminal_reply_header(remaining) {
            // A claimed reply string: terminator, cancel (CAN/SUB) or an
            // aborting new escape sequence decides where it ends.
            match scan_reply(remaining) {
                ReplyScan::Complete { len } => {
                    sequences.push(remaining[..len].to_vec());
                    pos += len;
                }
                ReplyScan::Cancelled { len } | ReplyScan::Aborted { len } => pos += len,
                ReplyScan::Open => return (sequences, remaining.to_vec()),
                ReplyScan::NotAReply => {}
            }
            if pos >= buffer.len() {
                break;
            }
            if &buffer[pos..] == remaining {
                // The reply is open (handled above); keep the rest buffered.
                return (sequences, remaining.to_vec());
            }
            continue;
        }

        let mut seq_end = 1usize;
        let mut emitted = false;
        while seq_end <= remaining.len() {
            let candidate = &remaining[..seq_end];
            match is_complete_sequence(candidate) {
                SequenceStatus::Complete => {
                    // WezTerm sends the Escape key press as a raw ESC and its
                    // release as a CSI-u sequence, concatenated: `ESC ESC [`.
                    // Only the first ESC is a key when a new escape sequence
                    // follows it.
                    if candidate == b"\x1b\x1b" {
                        if let Some(next) = remaining.get(seq_end) {
                            if matches!(next, b'[' | b']' | b'O' | b'P' | b'_') {
                                sequences.push(vec![ESC]);
                                pos += 1;
                                emitted = true;
                                break;
                            }
                        }
                    }
                    sequences.push(candidate.to_vec());
                    pos += seq_end;
                    emitted = true;
                    break;
                }
                SequenceStatus::Incomplete => seq_end += 1,
                SequenceStatus::NotEscape => {
                    sequences.push(candidate.to_vec());
                    pos += seq_end;
                    emitted = true;
                    break;
                }
            }
        }
        if !emitted {
            return (sequences, remaining.to_vec());
        }
    }
    (sequences, Vec::new())
}

/// Length of the UTF-8 character starting with `first` (1 for stray
/// continuation bytes).
fn utf8_char_len(first: u8) -> usize {
    match first {
        0x00..=0x7f => 1,
        0xc0..=0xdf => 2,
        0xe0..=0xef => 3,
        0xf0..=0xf7 => 4,
        _ => 1,
    }
}

/// Whether `byte` can start a multi-byte UTF-8 sequence (`C2..=F4`; C0/C1
/// are overlong leads and F5.. are invalid, so they are not treated as
/// partial UTF-8).
fn is_utf8_lead_byte(byte: u8) -> bool {
    (0xc2..=0xf4).contains(&byte)
}

/// Whether the held buffer starts with the lead byte of a UTF-8 character
/// whose continuation bytes have not all arrived. `extract_complete_sequences`
/// only leaves such a partial character behind (everything before it is
/// complete), so a continuation byte arriving next belongs to it rather than
/// being an 8-bit meta byte.
fn buffer_holds_partial_utf8(buffer: &[u8]) -> bool {
    match buffer.first() {
        Some(&first) if first != ESC => is_utf8_lead_byte(first),
        _ => false,
    }
}

/// The codepoint of a one-character sequence, when there is exactly one.
fn single_codepoint(sequence: &[u8]) -> Option<u32> {
    let text = std::str::from_utf8(sequence).ok()?;
    let mut chars = text.chars();
    let first = chars.next()?;
    if chars.next().is_some() {
        return None;
    }
    Some(first as u32)
}

/// `ESC [ <digits>[:<digits>][:<digits>] u` with a printable codepoint
/// (upstream `parseUnmodifiedKittyPrintableCodepoint`).
fn kitty_printable_codepoint(sequence: &[u8]) -> Option<u32> {
    let body = sequence.strip_prefix(b"\x1b[".as_slice())?;
    let body = body.strip_suffix(b"u")?;
    let first_end = body
        .iter()
        .position(|byte| !byte.is_ascii_digit())
        .unwrap_or(body.len());
    if first_end == 0 {
        return None;
    }
    // Whatever follows the leading digits may only be `:`-separated digits.
    let rest = &body[first_end..];
    let mut parts = rest.split(|byte| *byte == b':');
    if parts.next() != Some(&b""[..]) {
        return None;
    }
    let mut sub_groups = 0;
    for part in parts {
        sub_groups += 1;
        if sub_groups > 2 || !part.iter().all(u8::is_ascii_digit) {
            return None;
        }
    }
    let codepoint: u32 = std::str::from_utf8(&body[..first_end]).ok()?.parse().ok()?;
    (codepoint >= 32).then_some(codepoint)
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

// ---------------------------------------------------------------------------
// Shift+Enter synthesis (upstream terminal.ts:44-73, :390-400)
// ---------------------------------------------------------------------------

/// Upstream `isAppleTerminalSession`: macOS running Apple's Terminal.app.
///
/// Node spells the platform `darwin`, Rust's [`std::env::consts::OS`] spells
/// it `macos`; both are accepted so the environment caller is not a dead gate.
pub fn is_apple_terminal_session(platform: &str, term_program: Option<&str>) -> bool {
    matches!(platform, "darwin" | "macos") && term_program == Some("Apple_Terminal")
}

/// [`is_apple_terminal_session`] over the process environment.
pub fn apple_terminal_session_from_env() -> bool {
    is_apple_terminal_session(
        std::env::consts::OS,
        std::env::var("TERM_PROGRAM").ok().as_deref(),
    )
}

/// Whether native Shift+Enter detection runs on this platform (upstream
/// `forwardInputSequence`: `isAppleTerminalSession() || process.platform ===
/// "win32"`).
///
/// Both platform spellings are accepted: Node's (`win32`, `darwin`) and the
/// one [`std::env::consts::OS`] produces (`windows`, `macos`), which is what
/// [`StdinGate::from_env`] passes.
pub fn native_shift_detection_enabled(platform: &str, term_program: Option<&str>) -> bool {
    platform == "win32"
        || platform == "windows"
        || is_apple_terminal_session(platform, term_program)
}

/// Upstream `normalizeNativeShiftEnterInput`: a bare Return becomes the
/// CSI-u Shift+Enter sequence when native Shift detection is enabled and the
/// physical Shift key is down.
pub fn normalize_native_shift_enter_input(
    data: &str,
    should_detect_native_shift_enter: bool,
    is_shift_pressed: bool,
) -> String {
    if should_detect_native_shift_enter && data == "\r" && is_shift_pressed {
        return NATIVE_SHIFT_ENTER_SEQUENCE.to_string();
    }
    data.to_string()
}

/// Upstream `normalizeAppleTerminalInput`.
pub fn normalize_apple_terminal_input(
    data: &str,
    is_apple_terminal: bool,
    is_shift_pressed: bool,
) -> String {
    normalize_native_shift_enter_input(data, is_apple_terminal, is_shift_pressed)
}

/// Whether a `GetAsyncKeyState` result reports the key as down: the high bit
/// of the returned `SHORT` (`KEY_PRESSED_MASK 0x8000`, upstream
/// `native/win32/src/win32-platform.c`).
pub fn key_state_is_down(state: i16) -> bool {
    (state as u16 & 0x8000) != 0
}

/// Whether `kCGEventFlagMaskShift` is set in a `CGEventSourceFlagsState`
/// result (upstream `native/darwin/src/darwin-platform.m`).
pub fn shift_flag_set(flags: u64) -> bool {
    const KCG_EVENT_FLAG_MASK_SHIFT: u64 = 0x0002_0000;
    flags & KCG_EVENT_FLAG_MASK_SHIFT != 0
}

/// Upstream `isNativeModifierPressed("shift")`: the physical Shift key state
/// from the platform's native API.
///
/// Windows reads `GetAsyncKeyState` for the generic and both side-specific
/// Shift keys; macOS reads `CGEventSourceFlagsState` with the combined session
/// state. Every other platform — and any call that cannot observe the state —
/// reports `false`, upstream's fallback when its native helper is unavailable.
pub fn native_shift_pressed() -> bool {
    native_shift::shift_pressed()
}

/// The raw platform call behind [`native_shift_pressed`].
///
/// The workspace denies `unsafe_code`; this module is the documented
/// exception for this feature (the same pattern as the `console_peek` module
/// below).
#[cfg(windows)]
mod native_shift {
    #![allow(unsafe_code)]

    use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
        GetAsyncKeyState, VK_LSHIFT, VK_RSHIFT, VK_SHIFT,
    };

    pub(super) fn shift_pressed() -> bool {
        // SAFETY: `GetAsyncKeyState` reads the process-global asynchronous key
        // state for one virtual-key code; it dereferences no pointer and has
        // no failure mode this feature must observe.
        unsafe {
            [VK_SHIFT, VK_LSHIFT, VK_RSHIFT]
                .into_iter()
                .any(|key| super::key_state_is_down(GetAsyncKeyState(i32::from(key))))
        }
    }
}

/// The macOS half of the `native_shift` exception: `CGEventSourceFlagsState`
/// is the modifier source upstream's native helper reads.
#[cfg(target_os = "macos")]
mod native_shift {
    #![allow(unsafe_code)]

    /// `kCGEventSourceStateCombinedSessionState` (`CGEventSource.h`).
    const COMBINED_SESSION_STATE: i32 = 0;

    #[link(name = "CoreGraphics", kind = "framework")]
    extern "C" {
        /// `CGEventFlags CGEventSourceFlagsState(CGEventSourceStateID stateID)`.
        fn CGEventSourceFlagsState(state_id: i32) -> u64;
    }

    pub(super) fn shift_pressed() -> bool {
        // SAFETY: the call takes an enum value and returns the current
        // modifier flags; it dereferences no pointer.
        super::shift_flag_set(unsafe { CGEventSourceFlagsState(COMBINED_SESSION_STATE) })
    }
}

/// No native modifier query outside Windows/macOS (upstream's helper is
/// `undefined` there, and `isNativeModifierPressed` then reports `false`).
#[cfg(not(any(windows, target_os = "macos")))]
mod native_shift {
    pub(super) fn shift_pressed() -> bool {
        false
    }
}

/// Apply the synthesis to a parsed event (the pir shape of upstream's
/// byte-level rewrite: `\x1b[13;2u` is what its key layer reads as
/// Shift+Enter, which pir represents as Return + SHIFT).
///
/// Upstream's rule is byte-exact: only a bare `"\r"` is rewritten. The event
/// must therefore encode to exactly that byte — an LF (`KeyCode::Char('\n')`)
/// or any modified Enter is not a candidate.
pub fn synthesize_shift_enter(
    event: Event,
    should_detect_native_shift_enter: bool,
    is_shift_pressed: bool,
) -> Event {
    if should_detect_native_shift_enter
        && is_shift_pressed
        && event_input_bytes(&event).as_deref() == Some(b"\r".as_slice())
    {
        if let Event::Key(key) = &event {
            return Event::Key(KeyEvent::new_with_kind(
                KeyCode::Enter,
                KeyModifiers::SHIFT,
                key.kind,
            ));
        }
    }
    event
}

// ---------------------------------------------------------------------------
// PI_TUI_WRITE_LOG (upstream terminal.ts:170-183, :521-530)
// ---------------------------------------------------------------------------

/// Resolve the `PI_TUI_WRITE_LOG` path: an existing directory gets a
/// generated `tui-<timestamp>-<pid>.log`, any other value is the file path,
/// and an unset or empty value disables the log.
pub fn write_log_path(
    get: impl FnMut(&str) -> Option<String>,
    now: SystemTime,
    pid: u32,
) -> Option<PathBuf> {
    let mut get = get;
    let raw = get("PI_TUI_WRITE_LOG")?;
    if raw.is_empty() {
        return None;
    }
    let candidate = PathBuf::from(raw);
    if candidate.is_dir() {
        let stamp = log_timestamp(now);
        return Some(candidate.join(format!("tui-{stamp}-{pid}.log")));
    }
    Some(candidate)
}

/// [`write_log_path`] over the process environment.
pub fn write_log_path_from_env() -> Option<PathBuf> {
    write_log_path(
        |key| std::env::var(key).ok(),
        SystemTime::now(),
        std::process::id(),
    )
}

/// `YYYY-MM-DD_HH-MM-SS` (UTC) for the generated log file name.
fn log_timestamp(now: SystemTime) -> String {
    let seconds = now
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0);
    let days = seconds / 86_400;
    let seconds_of_day = seconds % 86_400;
    let (year, month, day) = civil_from_days(days as i64);
    format!(
        "{year:04}-{month:02}-{day:02}_{:02}-{:02}-{:02}",
        seconds_of_day / 3600,
        (seconds_of_day % 3600) / 60,
        seconds_of_day % 60
    )
}

/// Days-since-epoch to `(year, month, day)`, Howard Hinnant's algorithm.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let day_of_era = z.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = month_prime + if month_prime < 10 { 3 } else { -9 };
    let year = year + i64::from(month <= 2);
    (year, month as u32, day as u32)
}

/// Cap for the `PI_TUI_WRITE_LOG` file. The log is a frame-by-frame trace;
/// 64 MiB is hours of interaction, and past it the sink writes one truncation
/// marker and stops appending instead of filling the disk.
const WRITE_LOG_MAX_BYTES: u64 = 64 * 1024 * 1024;

/// Marker written once when the log reaches its cap.
const WRITE_LOG_TRUNCATED_MARKER: &str =
    "\n[pir] PI_TUI_WRITE_LOG truncated: the log reached its size cap\n";

#[derive(Default)]
struct WriteLogState {
    /// The file stays OPEN across appends: the previous shape opened and
    /// closed the path on every frame write.
    file: Option<std::fs::File>,
    written: u64,
    truncated_marker_written: bool,
}

/// `PI_TUI_WRITE_LOG` sink (upstream `ProcessTerminal.writeLogPath`, appended
/// by every `write()`).
pub struct WriteLog {
    path: Option<PathBuf>,
    cap: u64,
    state: std::sync::Mutex<WriteLogState>,
}

impl std::fmt::Debug for WriteLog {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("WriteLog")
            .field("path", &self.path)
            .field("cap", &self.cap)
            .finish_non_exhaustive()
    }
}

impl Default for WriteLog {
    fn default() -> Self {
        Self::disabled()
    }
}

impl WriteLog {
    /// Resolve the path from the environment; no path means logging is off.
    pub fn from_env() -> Self {
        Self {
            path: write_log_path_from_env(),
            cap: WRITE_LOG_MAX_BYTES,
            state: std::sync::Mutex::new(WriteLogState::default()),
        }
    }

    /// A permanently disabled sink.
    pub fn disabled() -> Self {
        Self {
            path: None,
            cap: WRITE_LOG_MAX_BYTES,
            state: std::sync::Mutex::new(WriteLogState::default()),
        }
    }

    /// A sink that appends to an explicit path (upstream `PI_TUI_WRITE_LOG`
    /// already pointing at a file).
    pub fn at(path: impl Into<PathBuf>) -> Self {
        Self::at_with_cap(path, WRITE_LOG_MAX_BYTES)
    }

    /// [`WriteLog::at`] with an explicit byte cap (tests drive a tiny cap
    /// instead of writing 64 MiB).
    #[doc(hidden)]
    pub fn at_with_cap(path: impl Into<PathBuf>, cap: u64) -> Self {
        Self {
            path: Some(path.into()),
            cap,
            state: std::sync::Mutex::new(WriteLogState::default()),
        }
    }

    /// The log file this sink appends to, when enabled.
    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    /// Whether writes are logged.
    pub fn enabled(&self) -> bool {
        self.path.is_some()
    }

    /// Append one output chunk. The file is opened once and kept open; once
    /// the cap is reached a single truncation marker is appended and later
    /// chunks are dropped. Errors are ignored (upstream
    /// `fs.appendFileSync`; the log must never break the UI).
    pub fn append(&self, data: &str) {
        let Some(path) = self.path.as_deref() else {
            return;
        };
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // Take the file out while writing so the state fields stay
        // independently borrowable; an early open failure simply leaves it
        // absent and retries on the next append.
        let mut file = match state.file.take() {
            Some(file) => file,
            None => match std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
            {
                Ok(file) => file,
                Err(_) => return,
            },
        };
        append_capped(&mut file, &mut state, self.cap, data);
        state.file = Some(file);
    }
}

/// Append up to `cap` bytes overall, then one truncation marker, then nothing.
fn append_capped(file: &mut std::fs::File, state: &mut WriteLogState, cap: u64, data: &str) {
    use std::io::Write as _;
    if state.written >= cap {
        if !state.truncated_marker_written {
            let _ = file.write_all(WRITE_LOG_TRUNCATED_MARKER.as_bytes());
            state.truncated_marker_written = true;
        }
        return;
    }
    let bytes = data.as_bytes();
    let remaining = (cap - state.written) as usize;
    let (chunk, hit_cap) = if bytes.len() > remaining {
        (&bytes[..remaining], true)
    } else {
        (bytes, false)
    };
    let _ = file.write_all(chunk);
    state.written += chunk.len() as u64;
    if hit_cap && !state.truncated_marker_written {
        let _ = file.write_all(WRITE_LOG_TRUNCATED_MARKER.as_bytes());
        state.truncated_marker_written = true;
    }
}

// ---------------------------------------------------------------------------
// Windows VT input (upstream terminal.ts:414-427)
// ---------------------------------------------------------------------------

/// Upstream `enableWindowsVTInput`'s platform gate: `win32` only.
pub fn should_enable_windows_vt_input(platform: &str) -> bool {
    platform == "win32"
}

/// Upstream native helper `enableVirtualTerminalInput`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VtInputOutcome {
    /// Not a Windows console: upstream returns before touching anything.
    UnsupportedPlatform,
    /// The console mode could not be read or set (upstream's `catch`: Shift+Tab
    /// then arrives as plain Tab).
    Unavailable,
    /// The flag was already part of the console mode.
    AlreadySet,
    /// `ENABLE_VIRTUAL_TERMINAL_INPUT` was added.
    Enabled,
}

/// Read-only view of the console input mode: safe to call without changing
/// anything (the TUI's own enable/restore pair is exercised at startup).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VtInputStatus {
    /// Not a Windows console.
    UnsupportedPlatform,
    /// No console input mode could be read (stdin is a pipe, or the console
    /// refuses access — upstream's `catch`).
    Unavailable,
    /// The console is available and VT input is off.
    FlagAbsent,
    /// The console is available and VT input is on.
    FlagSet,
}

/// Pure decision table behind [`enable_windows_vt_input`]: the mode to install
/// (current mode plus `ENABLE_VIRTUAL_TERMINAL_INPUT`), or `None` when the
/// flag is already set (upstream `mode | ENABLE_VIRTUAL_TERMINAL_INPUT`).
pub fn vt_input_mode_change(current: u32) -> Option<u32> {
    (current & ENABLE_VIRTUAL_TERMINAL_INPUT == 0)
        .then_some(current | ENABLE_VIRTUAL_TERMINAL_INPUT)
}

/// The console input mode before [`enable_windows_vt_input`] changed it, for
/// the cleanup restore (upstream leaves the flag set). The state lives inside
/// this Windows-only accessor: console modes exist only on Windows, so no
/// unread static is carried by the other builds.
#[cfg(windows)]
fn previous_console_mode() -> &'static std::sync::atomic::AtomicI64 {
    static PREVIOUS_CONSOLE_MODE: std::sync::atomic::AtomicI64 =
        std::sync::atomic::AtomicI64::new(-1);
    &PREVIOUS_CONSOLE_MODE
}

/// The console input mode before [`disable_windows_quick_edit`] changed it, or
/// `-1` when QuickEdit was already off. Like [`previous_console_mode`], the
/// state lives inside this Windows-only accessor.
#[cfg(windows)]
fn previous_quick_edit_mode() -> &'static std::sync::atomic::AtomicI64 {
    static PREVIOUS_QUICK_EDIT_MODE: std::sync::atomic::AtomicI64 =
        std::sync::atomic::AtomicI64::new(-1);
    &PREVIOUS_QUICK_EDIT_MODE
}

/// Clear `ENABLE_QUICK_EDIT_MODE` on the console input handle.
///
/// With QuickEdit enabled (classic conhost default) a mouse selection suspends
/// all console output until the user presses Esc, which blocks
/// `Terminal::write` forever and looks like a dead TUI. `enable_raw_mode` only
/// clears LINE/ECHO/PROCESSED, so the TUI clears this flag itself and restores
/// it on cleanup. Best-effort: a non-console stdin is left untouched.
#[cfg(windows)]
pub fn disable_windows_quick_edit() -> bool {
    use std::sync::atomic::Ordering;

    use crossterm_winapi::{ConsoleMode, Handle, HandleType};

    const ENABLE_QUICK_EDIT_MODE: u32 = 0x0040;
    const ENABLE_EXTENDED_FLAGS: u32 = 0x0080;

    let Ok(handle) = Handle::new(HandleType::InputHandle) else {
        return false;
    };
    let console = ConsoleMode::from(handle);
    let Ok(mode) = console.mode() else {
        return false;
    };
    if mode & ENABLE_QUICK_EDIT_MODE == 0 {
        return false;
    }
    let next = (mode | ENABLE_EXTENDED_FLAGS) & !ENABLE_QUICK_EDIT_MODE;
    if console.set_mode(next).is_err() {
        return false;
    }
    previous_quick_edit_mode().store(mode as i64, Ordering::Relaxed);
    true
}

/// Non-Windows: QuickEdit is a Windows console concept.
#[cfg(not(windows))]
pub fn disable_windows_quick_edit() -> bool {
    false
}

/// Restore the console mode captured by [`disable_windows_quick_edit`].
#[cfg(windows)]
pub fn restore_windows_quick_edit() {
    use std::sync::atomic::Ordering;

    use crossterm_winapi::{ConsoleMode, Handle, HandleType};

    let previous = previous_quick_edit_mode().swap(-1, Ordering::Relaxed);
    if previous < 0 {
        return;
    }
    if let Ok(handle) = Handle::new(HandleType::InputHandle) {
        let _ = ConsoleMode::from(handle).set_mode(previous as u32);
    }
}

/// Restore the console mode captured by [`disable_windows_quick_edit`].
#[cfg(not(windows))]
pub fn restore_windows_quick_edit() {}

/// Probe the console input mode without changing it.
#[cfg(windows)]
pub fn windows_vt_input_status() -> VtInputStatus {
    use crossterm_winapi::{ConsoleMode, Handle, HandleType};

    let Ok(handle) = Handle::new(HandleType::InputHandle) else {
        return VtInputStatus::Unavailable;
    };
    let Ok(mode) = ConsoleMode::from(handle).mode() else {
        return VtInputStatus::Unavailable;
    };
    if mode & ENABLE_VIRTUAL_TERMINAL_INPUT != 0 {
        VtInputStatus::FlagSet
    } else {
        VtInputStatus::FlagAbsent
    }
}

/// Probe the console input mode without changing it.
#[cfg(not(windows))]
pub fn windows_vt_input_status() -> VtInputStatus {
    VtInputStatus::UnsupportedPlatform
}

/// Enable `ENABLE_VIRTUAL_TERMINAL_INPUT` on stdin, best-effort (upstream
/// `native/win32/src/win32-platform.c:33-48`). Must run after raw mode, which
/// resets the console flags.
///
/// The TUI reaches this only through the `PIR_TUI_VT_INPUT=1` opt-in:
/// crossterm's Windows `INPUT_RECORD` parser cannot round-trip the console's
/// VT translation, so the default startup path leaves the flag off (see the
/// call site in `super::terminal`).
#[cfg(windows)]
pub fn enable_windows_vt_input() -> VtInputOutcome {
    use crossterm_winapi::{ConsoleMode, Handle, HandleType};
    use std::sync::atomic::Ordering;

    let Ok(handle) = Handle::new(HandleType::InputHandle) else {
        return VtInputOutcome::Unavailable;
    };
    let console = ConsoleMode::from(handle);
    let Ok(mode) = console.mode() else {
        return VtInputOutcome::Unavailable;
    };
    let Some(next) = vt_input_mode_change(mode) else {
        return VtInputOutcome::AlreadySet;
    };
    if console.set_mode(next).is_err() {
        return VtInputOutcome::Unavailable;
    }
    previous_console_mode().store(mode as i64, Ordering::Relaxed);
    VtInputOutcome::Enabled
}

/// Enable `ENABLE_VIRTUAL_TERMINAL_INPUT` on stdin, best-effort.
#[cfg(not(windows))]
pub fn enable_windows_vt_input() -> VtInputOutcome {
    VtInputOutcome::UnsupportedPlatform
}

/// Restore the console mode captured by [`enable_windows_vt_input`] (upstream
/// never does this; pir restores it so the flag cannot leak to the parent
/// console).
#[cfg(windows)]
pub fn restore_windows_vt_input() {
    use crossterm_winapi::{ConsoleMode, Handle, HandleType};
    use std::sync::atomic::Ordering;

    let previous = previous_console_mode().swap(-1, Ordering::Relaxed);
    if previous < 0 {
        return;
    }
    if let Ok(handle) = Handle::new(HandleType::InputHandle) {
        let _ = ConsoleMode::from(handle).set_mode(previous as u32);
    }
}

/// Restore the console mode captured by [`enable_windows_vt_input`].
#[cfg(not(windows))]
pub fn restore_windows_vt_input() {}

// ---------------------------------------------------------------------------
// Windows console input peek (the startup color query's non-consuming reader)
// ---------------------------------------------------------------------------

/// One console input record (`INPUT_RECORD`, `wincon.h`) as far as the color
/// query's reader needs it. The reader only ever looks at `KEY_EVENT` records;
/// the other variants (mouse, resize, focus, menu) are 4- to 16-byte payloads
/// in the same place, so their bytes are read as the key fields and reported as
/// "not a key" by [`Self::key_bytes`].
///
/// The layout is the documented one — a `WORD` `EventType`, two bytes of
/// padding to the event union's alignment, then the 16-byte union whose first
/// member is `KEY_EVENT_RECORD` — and is pinned by the compile-time assertion
/// in `console_peek` plus the offset tests in
/// `tests/suites/tui_stdin_decoding.rs`.
#[cfg(windows)]
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ConsoleInputRecord {
    /// `INPUT_RECORD.EventType` ([`KEY_EVENT_TYPE`] for a key press).
    pub event_type: u16,
    /// Padding to the event union's four-byte alignment.
    pub padding: u16,
    /// `KEY_EVENT_RECORD.bKeyDown` (a `BOOL`: nonzero while the key is held).
    pub key_down: i32,
    /// `KEY_EVENT_RECORD.wRepeatCount`.
    pub repeat_count: u16,
    /// `KEY_EVENT_RECORD.wVirtualKeyCode` (unused by the reader).
    pub virtual_key_code: u16,
    /// `KEY_EVENT_RECORD.wVirtualScanCode` (unused by the reader).
    pub virtual_scan_code: u16,
    /// `KEY_EVENT_RECORD.uChar.UnicodeChar` (one UTF-16 unit).
    pub u_char: u16,
    /// `KEY_EVENT_RECORD.dwControlKeyState` (unused by the reader).
    pub control_key_state: u32,
}

/// `KEY_EVENT` (`INPUT_RECORD.EventType`).
#[cfg(windows)]
pub const KEY_EVENT_TYPE: u16 = 0x0001;

#[cfg(windows)]
impl ConsoleInputRecord {
    /// The input bytes this record stands for: the pressed character, repeated
    /// `repeat_count` times. `None` for every other record — a key release, a
    /// key without a character, a UTF-16 surrogate half (a non-BMP character
    /// arrives as two records and is not reply material on its own), or a
    /// mouse/resize/focus/menu record. The reader treats those as "not reply
    /// material" and leaves them in the console queue.
    pub fn key_bytes(&self) -> Option<Vec<u8>> {
        if self.event_type != KEY_EVENT_TYPE || self.key_down == 0 || self.repeat_count == 0 {
            return None;
        }
        // `u_char == 0` is the "no character" key (arrows/function keys carry a
        // VK and no `UnicodeChar`) — not reply material, and a NUL byte must
        // never enter the reply walk.
        if self.u_char == 0 {
            return None;
        }
        let character = char::from_u32(u32::from(self.u_char))?;
        let mut bytes = Vec::with_capacity(usize::from(self.repeat_count) * character.len_utf8());
        let mut buffer = [0u8; 4];
        for _ in 0..self.repeat_count {
            bytes.extend_from_slice(character.encode_utf8(&mut buffer).as_bytes());
        }
        Some(bytes)
    }
}

/// Peek at the console's pending input records **without consuming them**
/// (`PeekConsoleInputW`; see [`ConsoleInputRecord`]).
///
/// Returns at most `max_records` records, and an empty vector when stdin is not
/// a console, when nothing is pending, or when a call fails. A caller that
/// looks at pending input this way can decide what is a terminal reply before
/// taking anything out of the console queue — which is what keeps the startup
/// color query from swallowing keyboard input on a console that never answers
/// it (nothing is pending there, so nothing is ever taken).
#[cfg(windows)]
pub fn peek_pending_console_input(max_records: usize) -> Vec<ConsoleInputRecord> {
    use crossterm_winapi::{Console, Handle, HandleType};

    if max_records == 0 {
        return Vec::new();
    }
    let Ok(handle) = Handle::new(HandleType::InputHandle) else {
        return Vec::new();
    };
    let console = Console::from(handle.clone());
    let Ok(pending) = console.number_of_console_input_events() else {
        return Vec::new();
    };
    let capacity = (pending as usize).min(max_records);
    if capacity == 0 {
        return Vec::new();
    }
    let mut records = vec![ConsoleInputRecord::default(); capacity];
    let read = console_peek::peek(*handle as usize, &mut records);
    records.truncate(read as usize);
    records
}

/// Consume `count` input records from the console queue (`ReadConsoleInputW`,
/// reached through `crossterm_winapi`'s safe wrapper).
///
/// Each record is checked to be pending immediately before it is read, so this
/// never blocks — not even when the console's input changed since the caller
/// peeked. Reading stops at the first record that is not pending, and the
/// number of records actually taken is returned, so a caller that mirrored its
/// buffer from a peek never records a byte that is still queued.
#[cfg(windows)]
pub fn consume_console_records(count: usize) -> usize {
    use crossterm_winapi::{Console, Handle, HandleType};

    let Ok(handle) = Handle::new(HandleType::InputHandle) else {
        return 0;
    };
    let console = Console::from(handle);
    let mut taken = 0;
    for _ in 0..count {
        if console.number_of_console_input_events().unwrap_or(0) == 0 {
            break;
        }
        if console.read_single_input_event().is_err() {
            break;
        }
        taken += 1;
    }
    taken
}

/// The one raw Win32 call the TUI makes: `PeekConsoleInputW` (`wincon.h`).
///
/// `crossterm_winapi` wraps `GetNumberOfConsoleInputEvents` and
/// `ReadConsoleInputW`, but not the peek — and the peek is the whole point of
/// [`peek_pending_console_input`]: it is the only way to inspect the records a
/// console has queued without taking them. The crate denies `unsafe_code`; this
/// module is its documented TUI exception, mirroring `src/env_daemon/pty.rs`.
#[cfg(windows)]
mod console_peek {
    #![allow(unsafe_code)]

    use std::ffi::c_void;

    use super::ConsoleInputRecord;

    /// The `INPUT_RECORD` layout this module mirrors: `WORD` `EventType`, two
    /// bytes of padding to the union's four-byte alignment, 16-byte union.
    const _: () = assert!(std::mem::size_of::<ConsoleInputRecord>() == 20);

    #[link(name = "kernel32")]
    extern "system" {
        /// `PeekConsoleInputW(console_input, buffer, length, events_read)`
        /// (`wincon.h`): copies up to `length` pending records into `buffer`
        /// **without** removing them from the console's input queue.
        fn PeekConsoleInputW(
            console_input: *mut c_void,
            buffer: *mut ConsoleInputRecord,
            length: u32,
            events_read: *mut u32,
        ) -> i32;
    }

    /// Peek up to `records.len()` records through the console input handle,
    /// returning how many were read (0 when the call fails).
    pub(super) fn peek(handle: usize, records: &mut [ConsoleInputRecord]) -> u32 {
        let mut read = 0u32;
        // SAFETY: `handle` is the process' console input handle, obtained from
        // `crossterm_winapi::Handle`; `records` is a live slice whose element
        // type mirrors `INPUT_RECORD` (asserted above); the call only copies
        // pending records into that buffer and reports how many it wrote.
        let ok = unsafe {
            PeekConsoleInputW(
                handle as *mut c_void,
                records.as_mut_ptr(),
                records.len() as u32,
                &mut read,
            )
        };
        if ok == 0 {
            0
        } else {
            read.min(records.len() as u32)
        }
    }
}

// ---------------------------------------------------------------------------
// Sequence decoding (the subset crossterm parses out of one read)
// ---------------------------------------------------------------------------

/// Decode one complete input sequence into the events crossterm would have
/// produced, had the bytes arrived in a single read. Sequences outside the
/// ported subset (Kitty specials, unmodeled CSI finals, reply strings) return
/// `None`: upstream's key layer resolves those to no key either.
pub fn decode_input_sequence(data: &[u8]) -> Option<Vec<Event>> {
    if data.len() < 2 || data[0] != ESC {
        return None;
    }
    // Two bytes are the legacy meta rule for *every* second byte: `ESC [` is
    // `alt+[`, `ESC ]` is `alt+]`, `ESC 1` is `alt+1` (upstream keys.ts:1346).
    if data.len() == 2 {
        return decode_legacy_meta(data[1], &[]);
    }
    // The legacy X10 mouse report (`ESC [ M` + three payload bytes) is handled
    // before the CSI branch: its payload bytes are not CSI parameters, and the
    // generic path must not read a key sequence out of them (a payload byte
    // can spell a CSI final byte). See [`decode_x10_mouse`].
    if let Some(mouse) = decode_x10_mouse(data) {
        return Some(vec![Event::Mouse(mouse)]);
    }
    match data[1] {
        b'[' => decode_csi(&data[2..]),
        b'O' => decode_ss3(&data[2..]),
        // Control strings (OSC/DCS/APC) are reply strings or paste markers.
        b']' | b'P' | b'_' | b'\\' => None,
        byte => decode_legacy_meta(byte, &data[2..]),
    }
}

/// Upstream's legacy meta rule (`keys.ts:1346-1356`): `ESC` + one byte is an
/// Alt chord for letters, digits and symbol keys, `alt+space` /
/// `alt+backspace` for space and DEL, and `ctrl+alt+letter` for control bytes.
fn decode_legacy_meta(byte: u8, tail: &[u8]) -> Option<Vec<Event>> {
    if !tail.is_empty() {
        // `ESC` + a multi-byte character: an Alt chord for that character.
        let text = std::str::from_utf8(tail).ok()?;
        let mut chars = text.chars();
        let ch = chars.next()?;
        if chars.next().is_some() {
            return None;
        }
        return Some(vec![key_event(KeyCode::Char(ch), KeyModifiers::ALT)]);
    }
    match byte {
        b'a'..=b'z' | b'0'..=b'9' => Some(vec![key_event(
            KeyCode::Char(byte as char),
            KeyModifiers::ALT,
        )]),
        b' ' => Some(vec![key_event(KeyCode::Char(' '), KeyModifiers::ALT)]),
        b'\r' => Some(vec![key_event(KeyCode::Enter, KeyModifiers::ALT)]),
        0x7f | 0x08 => Some(vec![key_event(KeyCode::Backspace, KeyModifiers::ALT)]),
        1..=26 => Some(vec![key_event(
            KeyCode::Char((byte - 1 + b'a') as char),
            KeyModifiers::CONTROL | KeyModifiers::ALT,
        )]),
        byte if is_symbol_key(byte) => Some(vec![key_event(
            KeyCode::Char(byte as char),
            KeyModifiers::ALT,
        )]),
        _ => None,
    }
}

/// Upstream `SYMBOL_KEYS` (`keys.ts:258-290`).
fn is_symbol_key(byte: u8) -> bool {
    matches!(
        byte,
        b'`' | b'-'
            | b'='
            | b'['
            | b']'
            | b'\\'
            | b';'
            | b'\''
            | b','
            | b'.'
            | b'/'
            | b'!'
            | b'@'
            | b'#'
            | b'$'
            | b'%'
            | b'^'
            | b'&'
            | b'*'
            | b'('
            | b')'
            | b'_'
            | b'+'
            | b'|'
            | b'~'
            | b'{'
            | b'}'
            | b':'
            | b'<'
            | b'>'
            | b'?'
    )
}

fn decode_csi(body: &[u8]) -> Option<Vec<Event>> {
    let first = *body.first()?;
    match first {
        b'I' if body.len() == 1 => return Some(vec![Event::FocusGained]),
        b'O' if body.len() == 1 => return Some(vec![Event::FocusLost]),
        b'<' => return decode_sgr_mouse(body),
        _ => {}
    }
    let final_byte = *body.last()?;
    let params = &body[..body.len() - 1];
    let (primary, modifiers, kind) = parse_csi_params(params)?;
    let modifiers = modifiers
        .map(decode_modifier_mask)
        .unwrap_or_else(KeyModifiers::empty);
    let kind = kind
        .map(decode_key_event_kind)
        .unwrap_or(KeyEventKind::Press);
    let code = match final_byte {
        b'A' => KeyCode::Up,
        b'B' => KeyCode::Down,
        b'C' => KeyCode::Right,
        b'D' => KeyCode::Left,
        b'H' => KeyCode::Home,
        b'F' => KeyCode::End,
        b'Z' => return Some(vec![key_event(KeyCode::BackTab, KeyModifiers::SHIFT)]),
        b'~' => decode_tilde_key(primary?)?,
        b'u' => {
            let codepoint = primary?;
            return Some(vec![Event::Key(KeyEvent::new_with_kind(
                decode_codepoint(codepoint)?,
                modifiers,
                kind,
            ))]);
        }
        b'P' => KeyCode::F(1),
        b'Q' => KeyCode::F(2),
        b'S' => KeyCode::F(4),
        _ => return None,
    };
    Some(vec![Event::Key(KeyEvent::new_with_kind(
        code, modifiers, kind,
    ))])
}

/// `ESC [ < Cb ; Cx ; Cy (M|m)` (crossterm `parse_csi_sgr_mouse`).
fn decode_sgr_mouse(body: &[u8]) -> Option<Vec<Event>> {
    let last = *body.last()?;
    if last != b'M' && last != b'm' {
        return None;
    }
    let text = std::str::from_utf8(&body[1..body.len() - 1]).ok()?;
    let mut parts = text.split(';');
    let cb: u8 = parts.next()?.parse().ok()?;
    let column: u16 = parts.next()?.parse::<u16>().ok()?.checked_sub(1)?;
    let row: u16 = parts.next()?.parse::<u16>().ok()?.checked_sub(1)?;
    let (kind, modifiers) = decode_mouse_cb(cb)?;
    let kind = if last == b'm' {
        match kind {
            MouseEventKind::Down(button) => MouseEventKind::Up(button),
            other => other,
        }
    } else {
        kind
    };
    Some(vec![Event::Mouse(MouseEvent {
        kind,
        column,
        row,
        modifiers,
    })])
}

/// `ESC [ M Cb Cx Cy` — the legacy ("X10") mouse report, decoded exactly as
/// crossterm's Unix byte parser reads it (`event/sys/unix/parse.rs:690-716`
/// `parse_csi_normal_mouse`). pir keeps crossterm as its byte parser, so this
/// is the shape a fragmented report decodes to at the gate; the complete
/// 6-byte rule itself is upstream's ([`is_complete_sequence`]'s old-style
/// mouse branch, `stdin-buffer.ts:51-55`).
///
/// The grammar: the button byte carries `Cb = byte - 32` (crossterm's
/// `checked_sub(32)`, so anything below 32 is not a report) and the two
/// coordinates are `byte - 33` — the encoded position is one-based and the
/// wire byte is `value + 32`, so the 0-based cell is exactly `byte - 33`.
/// The button/modifier/drag bits are the ones the SGR grammar shares, so the
/// decode runs through [`decode_mouse_cb`] (crossterm's `parse_cb` port) just
/// like [`decode_sgr_mouse`] does.
///
/// Release-marker semantics: this encoding has no per-button release and no
/// `M`/`m` suffix — a release is the *third* button code, decoded as
/// `Event::Mouse` with kind `Up(MouseButton::Left)` (crossterm's
/// `(3, false)` arm; the specific button is unrecoverable by design). Motion
/// sets bit 32 (`Cb & 32` → drags and the button-less move), and `Cb` 4..=7
/// (bit 32 clear) are the wheel directions and the no-button move.
///
/// `None` for every shape the grammar does not model, and for an out-of-range
/// coordinate byte (below 33). That rejection is a **deliberate deviation from
/// crossterm's observable behavior**, called out here so the parity note stays
/// honest: `byte - 33` cannot be a cell, and the vendored parser's own
/// arithmetic for such a byte is `u16::from(byte.saturating_sub(32)) - 1`
/// (`event/sys/unix/parse.rs:707-708`), which panics with an arithmetic
/// overflow in debug builds and wraps to coordinate 65535 in release builds
/// (its tests never exercise a byte below 33). pir reproduces neither — no
/// coordinate is invented and no overflow is reproduced — and the sequence is
/// still consumed: the 6-byte shape is never typed as text.
///
/// Only this ESC-prefixed spelling is decoded. The ESC-less X10 tail
/// (`[M` + 3 bytes) is deliberately **not** claimed: unlike `[<Cb;Cx;CyM|m`
/// it is not self-identifying — a user typing `[M` plus three characters
/// produces the same bytes — and the SGR claim exists only because the
/// Windows console drops the record that carries the report's `ESC` while the
/// terminal is in the SGR encoding `EnableMouseCapture` asks for, so no X10
/// report can arrive that way (see the module docs, and
/// [`classify_sgr_mouse_tail`]).
pub fn decode_x10_mouse(report: &[u8]) -> Option<MouseEvent> {
    let payload = report.strip_prefix(b"\x1b[M".as_slice())?;
    if payload.len() != 3 {
        return None;
    }
    let cb = payload[0].checked_sub(32)?;
    let (kind, modifiers) = decode_mouse_cb(cb)?;
    let column = u16::from(payload[1]).checked_sub(33)?;
    let row = u16::from(payload[2]).checked_sub(33)?;
    Some(MouseEvent {
        kind,
        column,
        row,
        modifiers,
    })
}

/// crossterm's `parse_cb`: button number, dragging bit, wheel codes.
fn decode_mouse_cb(cb: u8) -> Option<(MouseEventKind, KeyModifiers)> {
    let button_number = (cb & 0b0000_0011) | ((cb & 0b1100_0000) >> 4);
    let dragging = cb & 0b0010_0000 == 0b0010_0000;
    let kind = match (button_number, dragging) {
        (0, false) => MouseEventKind::Down(MouseButton::Left),
        (1, false) => MouseEventKind::Down(MouseButton::Middle),
        (2, false) => MouseEventKind::Down(MouseButton::Right),
        (0, true) => MouseEventKind::Drag(MouseButton::Left),
        (1, true) => MouseEventKind::Drag(MouseButton::Middle),
        (2, true) => MouseEventKind::Drag(MouseButton::Right),
        (3, false) => MouseEventKind::Up(MouseButton::Left),
        (3, true) | (4, true) | (5, true) => MouseEventKind::Moved,
        (4, false) => MouseEventKind::ScrollUp,
        (5, false) => MouseEventKind::ScrollDown,
        (6, false) => MouseEventKind::ScrollLeft,
        (7, false) => MouseEventKind::ScrollRight,
        _ => return None,
    };
    let mut modifiers = KeyModifiers::empty();
    if cb & 0b0000_0100 != 0 {
        modifiers |= KeyModifiers::SHIFT;
    }
    if cb & 0b0000_1000 != 0 {
        modifiers |= KeyModifiers::ALT;
    }
    if cb & 0b0001_0000 != 0 {
        modifiers |= KeyModifiers::CONTROL;
    }
    Some((kind, modifiers))
}

/// State of an *ESC-less* SGR mouse tail — the bytes the Windows console
/// leaves when crossterm drops the record that carries the report's `ESC`
/// (see the module docs and [`StdinGate`]'s `claim_console_mouse`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SgrMouseTail {
    /// Not (and never) an SGR-mouse tail: the run does not start `[`.
    NotTail,
    /// A strict prefix of `[<Cb;Cx;CyM|m` (`[`, `[<`, `[<64`, `[<64;40`, …):
    /// more bytes may complete it, so the run is held.
    Open,
    /// A complete `[<Cb;Cx;CyM|m` report.
    Complete,
    /// Started like a tail but can no longer become one (`[a`, `[<1;2;3;4M`,
    /// an unterminated `[<`): ordinary input that must be released unchanged,
    /// never swallowed.
    Invalid,
}

/// Classify an accumulated run against the ESC-less SGR mouse grammar
/// `[<Cb;Cx;CyM|m` (the report `EnableMouseCapture` enables, with the ESC
/// record already dropped). The shape rules are upstream's
/// ([`is_complete_sgr_mouse_payload`] decides the complete form); this
/// function adds the prefix/invalid split the claim needs: only a run that
/// could still become a report is held, and everything else is released.
pub fn classify_sgr_mouse_tail(bytes: &[u8]) -> SgrMouseTail {
    if bytes.first() != Some(&b'[') {
        return SgrMouseTail::NotTail;
    }
    let mut introducer = true;
    let mut parameters = 0usize;
    let mut digits = 0usize;
    for (index, byte) in bytes[1..].iter().enumerate() {
        if introducer {
            if *byte != b'<' {
                return SgrMouseTail::Invalid;
            }
            introducer = false;
            continue;
        }
        match byte {
            b'0'..=b'9' => digits += 1,
            b';' => {
                // Three parameters exactly: a fourth `;` can never complete.
                if digits == 0 || parameters >= 2 {
                    return SgrMouseTail::Invalid;
                }
                parameters += 1;
                digits = 0;
            }
            b'M' | b'm' => {
                let last = index + 2 == bytes.len();
                return if last && is_complete_sgr_mouse_payload(&bytes[1..]) {
                    SgrMouseTail::Complete
                } else {
                    // A terminator before the third parameter, or bytes after
                    // it: a run like that never becomes a report.
                    SgrMouseTail::Invalid
                };
            }
            _ => return SgrMouseTail::Invalid,
        }
    }
    // Ran out of bytes without a terminator: still a valid prefix (`[` alone
    // included — the introducer simply has not arrived yet).
    SgrMouseTail::Open
}

/// Decode a complete ESC-less SGR mouse tail (`[<Cb;Cx;CyM|m`) into the mouse
/// event the ESC-prefixed [`decode_sgr_mouse`] would produce. `None` for a
/// shape outside crossterm's button grammar (an unknown button code), which
/// the gate releases as typed input instead of swallowing.
pub fn decode_sgr_mouse_tail(tail: &[u8]) -> Option<MouseEvent> {
    let body = tail.strip_prefix(b"[".as_slice())?;
    let events = decode_sgr_mouse(body)?;
    match events.into_iter().next()? {
        Event::Mouse(mouse) => Some(mouse),
        _ => None,
    }
}

/// `ESC O <final>` (crossterm's `parse_ss3`).
fn decode_ss3(body: &[u8]) -> Option<Vec<Event>> {
    if body.len() != 1 {
        return None;
    }
    let code = match body[0] {
        b'A' => KeyCode::Up,
        b'B' => KeyCode::Down,
        b'C' => KeyCode::Right,
        b'D' => KeyCode::Left,
        b'H' => KeyCode::Home,
        b'F' => KeyCode::End,
        b'P' => KeyCode::F(1),
        b'Q' => KeyCode::F(2),
        b'R' => KeyCode::F(3),
        b'S' => KeyCode::F(4),
        b'M' => KeyCode::Enter,
        _ => return None,
    };
    Some(vec![key_event(code, KeyModifiers::empty())])
}

/// `ESC [ <n> ~` / `ESC [ <n> ; <mods> ~`.
fn decode_tilde_key(primary: u32) -> Option<KeyCode> {
    Some(match primary {
        1 => KeyCode::Home,
        2 => KeyCode::Insert,
        3 => KeyCode::Delete,
        4 => KeyCode::End,
        5 => KeyCode::PageUp,
        6 => KeyCode::PageDown,
        7 => KeyCode::Home,
        8 => KeyCode::End,
        11 => KeyCode::F(1),
        12 => KeyCode::F(2),
        13 => KeyCode::F(3),
        14 => KeyCode::F(4),
        15 => KeyCode::F(5),
        17 => KeyCode::F(6),
        18 => KeyCode::F(7),
        19 => KeyCode::F(8),
        20 => KeyCode::F(9),
        21 => KeyCode::F(10),
        23 => KeyCode::F(11),
        24 => KeyCode::F(12),
        _ => return None,
    })
}

/// CSI-u (`ESC [ <codepoint> ; <mods> [ : <kind> ] u`) codepoints.
fn decode_codepoint(codepoint: u32) -> Option<KeyCode> {
    Some(match codepoint {
        9 => KeyCode::Tab,
        13 => KeyCode::Enter,
        27 => KeyCode::Esc,
        32 => KeyCode::Char(' '),
        127 => KeyCode::Backspace,
        // Kitty's keypad Enter (upstream `keys.ts:311` `kpEnter: 57414`);
        // decoded as Enter like every other Kitty functional codepoint it has
        // an equivalent for.
        57414 => KeyCode::Enter,
        33..=126 => KeyCode::Char(char::from_u32(codepoint)?),
        128.. => KeyCode::Char(char::from_u32(codepoint)?),
        _ => return None,
    })
}

/// Split CSI parameters into `(primary, modifiers, event kind)`, accepting the
/// `a;b:c` CSI-u shape.
fn parse_csi_params(params: &[u8]) -> Option<(Option<u32>, Option<u32>, Option<u32>)> {
    if params.is_empty() {
        return Some((None, None, None));
    }
    let groups: Vec<&[u8]> = params.split(|byte| *byte == b';').collect();
    if groups.len() > 3 {
        return None;
    }
    let mut values = (None, None, None);
    for (index, group) in groups.iter().enumerate() {
        let (number, sub) = match group.iter().position(|byte| *byte == b':') {
            Some(colon) => (&group[..colon], Some(&group[colon + 1..])),
            None => (*group, None),
        };
        let value = if number.is_empty() {
            None
        } else {
            Some(std::str::from_utf8(number).ok()?.parse::<u32>().ok()?)
        };
        match index {
            0 => values.0 = value,
            1 => {
                values.1 = value;
                if let Some(sub) = sub {
                    if !sub.is_empty() {
                        values.2 = Some(std::str::from_utf8(sub).ok()?.parse::<u32>().ok()?);
                    }
                }
            }
            2 => {
                if !group.is_empty() {
                    values.2 = Some(std::str::from_utf8(group).ok()?.parse::<u32>().ok()?);
                }
            }
            _ => return None,
        }
    }
    Some(values)
}

/// crossterm's `parse_modifiers`: the mask minus one, bit 0 shift, 1 alt,
/// 2 ctrl, 3 super, 4 hyper, 5 meta.
fn decode_modifier_mask(mask: u32) -> KeyModifiers {
    let mask = mask.saturating_sub(1);
    let mut modifiers = KeyModifiers::empty();
    if mask & 1 != 0 {
        modifiers |= KeyModifiers::SHIFT;
    }
    if mask & 2 != 0 {
        modifiers |= KeyModifiers::ALT;
    }
    if mask & 4 != 0 {
        modifiers |= KeyModifiers::CONTROL;
    }
    if mask & 8 != 0 {
        modifiers |= KeyModifiers::SUPER;
    }
    if mask & 16 != 0 {
        modifiers |= KeyModifiers::HYPER;
    }
    if mask & 32 != 0 {
        modifiers |= KeyModifiers::META;
    }
    modifiers
}

/// crossterm's `parse_key_event_kind`.
fn decode_key_event_kind(kind: u32) -> KeyEventKind {
    match kind {
        2 => KeyEventKind::Repeat,
        3 => KeyEventKind::Release,
        _ => KeyEventKind::Press,
    }
}

fn key_event(code: KeyCode, modifiers: KeyModifiers) -> Event {
    Event::Key(KeyEvent::new(code, modifiers))
}

// ---------------------------------------------------------------------------
// Event-level gate (pir's stand-in for the byte-level buffer)
// ---------------------------------------------------------------------------

/// The reply grammar's verdict on the gate's held run (see
/// [`StdinGate::settle_reply_run`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReplyRun {
    /// The run is not (and cannot become) a claimed reply: the sequence
    /// machinery owns it.
    Foreign,
    /// A recognized reply header whose string has not terminated: hold it
    /// (bounded by the sequence window and [`MAX_HELD_BYTES`]) and consume it
    /// whole when it does.
    Open,
    /// `len` bytes of the held run are a complete reply token: consume them
    /// (they are never dispatched) and keep looking at what trails.
    Token { len: usize },
}

/// Event-level escape-timeout gate: crossterm hands over already-parsed
/// events, so the gate reassembles the *bytes* those events stand for and
/// applies upstream's rules to them. The same reassembly recognizes the
/// bracketed-paste markers on Windows, where crossterm's record parser has no
/// paste handling (see [`Self::collect_paste`]), and the terminal replies the
/// console hands over as key records (see [`Self::settle_reply_run`]).
#[derive(Debug)]
pub struct StdinGate {
    held: Vec<u8>,
    held_events: VecDeque<Event>,
    /// Byte offset in `held` where each held event ends (parallel to
    /// `held_events`, the last entry is `held.len()`), so a claimed reply
    /// token can be dropped event-whole: units are never split.
    held_ends: Vec<usize>,
    deadline: Option<Instant>,
    sequence_timeout: Duration,
    escape_timeout: Duration,
    detect_native_shift_enter: bool,
    shift_pressed: fn() -> bool,
    /// `Some(payload)` while a bracketed paste is open: the start marker
    /// arrived and the payload's bytes are collected here until the end
    /// marker (upstream `pasteMode` + `pasteBuffer`). An open paste has no
    /// deadline — only `ESC [ 2 0 1 ~` closes it — but its payload is capped
    /// at [`MAX_PASTE_BYTES`].
    paste: Option<Vec<u8>>,
    /// Claim ESC-less SGR mouse tails (`[<Cb;Cx;CyM|m`) as [`Event::Mouse`]
    /// (see the module docs). On by default on Windows, where crossterm drops
    /// the console record that carries the report's `ESC`; off elsewhere,
    /// where crossterm's byte parser synthesizes the mouse event itself and a
    /// held `[` would only delay typed text.
    claim_console_mouse: bool,
    /// The key-up partner of the last byte the console claim consumed. The
    /// console synthesizes one press and one release record per byte of a
    /// report; once the press that ended a run is claimed, its release no
    /// longer belongs to any run and must be swallowed with it (nothing else
    /// recognizes the record — it is a bare `M` key-up).
    ///
    /// SOUNDNESS: the match below is by key code only, so *a* same-code
    /// release is swallowed regardless of which press it belongs to. That is
    /// safe only while no consumer acts on [`KeyEventKind::Release`] events:
    /// the TUI input loop matches `Press` exclusively (`terminal.rs`), and
    /// everywhere else in the gate a release is ignored. If a Release
    /// consumer is ever added, this pairing must become exact (match by
    /// timestamp/code instead of "the next same-code release"), or that
    /// consumer would lose a key-up.
    pending_byte_release: Option<KeyCode>,
}

impl Default for StdinGate {
    fn default() -> Self {
        Self::new(
            Duration::from_millis(DEFAULT_SEQUENCE_TIMEOUT_MS),
            Duration::from_millis(DEFAULT_ESCAPE_TIMEOUT_MS),
        )
    }
}

impl StdinGate {
    /// A gate with explicit windows and no native Shift+Enter detection.
    pub fn new(sequence_timeout: Duration, escape_timeout: Duration) -> Self {
        Self {
            held: Vec::new(),
            held_events: VecDeque::new(),
            held_ends: Vec::new(),
            deadline: None,
            sequence_timeout,
            escape_timeout,
            detect_native_shift_enter: false,
            shift_pressed: native_shift_pressed,
            paste: None,
            claim_console_mouse: cfg!(windows),
            pending_byte_release: None,
        }
    }

    /// A gate with the Shift+Enter synthesis seam configured (upstream
    /// `shouldDetectNativeShiftEnter` + `isNativeModifierPressed("shift")`).
    pub fn with_native_shift_enter(
        sequence_timeout: Duration,
        escape_timeout: Duration,
        detect_native_shift_enter: bool,
        shift_pressed: fn() -> bool,
    ) -> Self {
        let mut gate = Self::new(sequence_timeout, escape_timeout);
        gate.detect_native_shift_enter = detect_native_shift_enter;
        gate.shift_pressed = shift_pressed;
        gate
    }

    /// A gate configured from the environment: `PI_TUI_ESC_TIMEOUT` (or the
    /// SSH default) plus the Apple Terminal / Windows Shift+Enter gate
    /// (upstream `setupStdinBuffer` + `forwardInputSequence`).
    pub fn from_env() -> Self {
        Self::with_native_shift_enter(
            Duration::from_millis(DEFAULT_SEQUENCE_TIMEOUT_MS),
            Duration::from_millis(escape_timeout_ms_from_env()),
            native_shift_detection_enabled(
                std::env::consts::OS,
                std::env::var("TERM_PROGRAM").ok().as_deref(),
            ),
            native_shift_pressed,
        )
    }

    /// Force the ESC-less console-mouse claim on or off (see
    /// [`Self::claim_console_mouse`]; the default is per platform). Tests use
    /// it to exercise the Windows shape on any host.
    pub fn with_console_mouse_claim(mut self, claim: bool) -> Self {
        self.claim_console_mouse = claim;
        self
    }

    /// Apply the configured native Shift+Enter synthesis to one event.
    ///
    /// Mirrors upstream `forwardInputSequence`'s short-circuit order: the
    /// platform gate is checked first, then the byte rule
    /// (`sequence === "\r"`), and only when both hold is the native provider
    /// consulted. A gate with detection disabled therefore never touches the
    /// platform API, for any event.
    fn synthesize_shift_enter_if_enabled(&self, event: Event) -> Event {
        if !self.detect_native_shift_enter
            || event_input_bytes(&event).as_deref() != Some(b"\r".as_slice())
        {
            return event;
        }
        synthesize_shift_enter(event, true, (self.shift_pressed)())
    }

    /// Feed one parsed event; returns the events to dispatch in order.
    pub fn push(&mut self, event: Event, now: Instant) -> Vec<Event> {
        // The key-up that pairs with the last byte the console claim consumed
        // goes with it: the console reports a report's bytes as press/release
        // pairs, so a release arriving with nothing held is that partner (the
        // claim already dispatched the byte it belonged to). Anything else
        // clears the expectation and takes the ordinary path.
        if let Some(code) = self.pending_byte_release.take() {
            if matches!(
                &event,
                Event::Key(key)
                    if key.kind == KeyEventKind::Release && key.code == code
            ) {
                return Vec::new();
            }
        }
        let event = resolve_console_line_feed(event);
        if self.paste.is_some() {
            // An open bracketed paste consumes raw text only: no Shift+Enter
            // synthesis and no sequence decode — the end marker closes it.
            return self.collect_paste(event, now);
        }
        let mut out = Vec::new();
        let event = self.synthesize_shift_enter_if_enabled(event);
        // An in-flight console mouse tail (Windows: the report's ESC record
        // was dropped by crossterm, so the run is plain `Char` events). The
        // console synthesizes one key *press* record per byte, each followed
        // by its key-up record; the release carries no byte and must not end
        // the run.
        if self.claim_console_mouse && classify_sgr_mouse_tail(&self.held) == SgrMouseTail::Open {
            if let Some(bytes) = mouse_tail_bytes(&event) {
                self.pending_byte_release = key_code_of(&event);
                if self.hold_bytes(bytes, event) {
                    // The claimed run outgrew any report shape: release the
                    // staged events as ordinary input and forget the pending
                    // key-up (the press is no longer part of a claimed report).
                    self.pending_byte_release = None;
                    return self.flush_held();
                }
                out.extend(self.settle_mouse_run(now));
                return out;
            }
            if is_key_release(&event) {
                return out;
            }
            // Not mouse material (a key chord, an already-parsed sequence, a
            // non-character event): release the run as its typed events and
            // dispatch this event through the ordinary path below.
            out.extend(self.flush_held());
        }
        if self.held.is_empty() {
            if is_bare_escape(&event) {
                self.hold_bytes(vec![ESC], event);
                self.deadline = Some(now + self.escape_timeout);
                return out;
            }
            // A single typed `[` may be the start of an ESC-less mouse
            // report: hold it for the sequence window (a report's records
            // reach the gate one console read at a time, inside that
            // window; a `[` that goes nowhere is released by it, and any
            // byte that cannot continue a report releases it at once).
            if self.claim_console_mouse
                && mouse_tail_bytes(&event).as_deref() == Some(b"[".as_slice())
            {
                self.pending_byte_release = key_code_of(&event);
                self.hold_bytes(vec![b'['], event);
                self.deadline = Some(now + self.sequence_timeout);
                return out;
            }
            out.push(event);
            return out;
        }
        // A multi-byte character is ONE key event, not a run of single-byte
        // keys: never feed its UTF-8 bytes into the escape-sequence walk.
        // A held ESC would turn `ESC` + the first UTF-8 byte into a legacy
        // meta chord (`decode_legacy_meta`) and the continuation bytes into
        // further garbage, losing/mangling IME-committed text. Release the
        // held prefix as its own key(s) and dispatch the character whole.
        if !self.held.is_empty() && is_multibyte_char_event(&event) {
            out.extend(self.flush_held());
            out.push(event);
            return out;
        }
        match event_input_bytes(&event) {
            // An already-parsed sequence (arrow, BackTab, mouse, paste, focus)
            // ends the held prefix: upstream splits `ESC ESC [A` into ESC and
            // the sequence.
            None => {
                // A key release carries no byte: the console's per-byte record
                // synthesis pairs every press with its release, so ignoring it
                // keeps the held run (ESC window, reply claim, mouse tail)
                // assembling instead of splitting on every key-up.
                if is_key_release(&event) {
                    return out;
                }
                out.extend(self.flush_held());
                out.push(self.synthesize_shift_enter_if_enabled(event));
            }
            Some(bytes) => {
                if self.hold_bytes(bytes, event) {
                    // Over the cap the run can no longer become any recognized
                    // sequence or reply: release it as ordinary input instead
                    // of waiting for a terminator that will never come.
                    out.extend(self.flush_held());
                } else {
                    out.extend(self.settle_reply_run(now));
                }
            }
        }
        out
    }

    /// Stage one event's bytes as the held run's next unit. Returns `true`
    /// when the run has outgrown [`MAX_HELD_BYTES`] or [`MAX_HELD_EVENTS`]:
    /// the caller must then release it as ordinary input ([`Self::flush_held`])
    /// instead of letting it wait for a terminator — every byte of an open run
    /// refreshes its deadline, so without this cap it would grow for as long
    /// as the stream continues.
    fn hold_bytes(&mut self, bytes: Vec<u8>, event: Event) -> bool {
        self.held.extend_from_slice(&bytes);
        self.held_events.push_back(event);
        self.held_ends.push(self.held.len());
        self.held_over_cap()
    }

    /// Whether the held run exceeds its byte or event cap.
    fn held_over_cap(&self) -> bool {
        self.held.len() > MAX_HELD_BYTES || self.held_events.len() > MAX_HELD_EVENTS
    }

    /// Apply the terminal-reply grammar to the held run
    /// ([`Self::classify_reply_run`]) and act on its verdict. Returns the
    /// events to dispatch.
    ///
    /// The rule: a run is a *claimed reply* exactly when the walker in
    /// [`super::terminal_reply`] recognizes it — a device-attributes answer
    /// (`ESC [ ? `…`c` / `ESC [ > `…`c`), or a control string whose header is
    /// one of the reply headers (OSC 10/11/4/7501, XTVersion `ESC P >|`, Kitty
    /// `ESC _G k=n;`) that has terminated, was cancelled, or is still waiting
    /// for its terminator (`ReplyScan::Open`). The header needs the run's own
    /// `ESC`; a run without one is never claimed (see the module docs: the
    /// console's ESC-stripped tail is indistinguishable from typing).
    ///
    /// Everything else is handed to the sequence machinery unchanged. That is
    /// what keeps the gate's pre-existing behavior: a bare `ESC`, `ESC [`, an
    /// arrow `ESC [ A`, a paste marker `ESC [ 2 0 0 ~`, an Alt chord `ESC x`,
    /// `ESC ] 12` — none of those is a recognized reply header, and the two
    /// fragments that are prefixes of one (`ESC [ ? 6 1 ; 6`, `ESC ] 1`) stay
    /// with the sequence machinery, which holds them for its own window and
    /// resolves them by upstream's rules (an unparseable partial is dropped, a
    /// control-string prefix becomes the Alt chord plus its tail).
    ///
    /// Claimed reply bytes are never dispatched: a terminal answers the
    /// startup color query with up to 19 replies, and upstream consumes *every*
    /// reply string out of its stdin buffer, whether or not it is a color reply.
    fn settle_reply_run(&mut self, now: Instant) -> Vec<Event> {
        loop {
            match self.classify_reply_run() {
                ReplyRun::Token { len } => {
                    let dropped = self.drop_held_bytes(len);
                    if self.held.is_empty() {
                        self.deadline = None;
                        return Vec::new();
                    }
                    if dropped == 0 {
                        // Defensive: the claimed token does not end on an event
                        // boundary (a multi-byte key event straddles it), so it
                        // cannot be dropped unit-whole — hand the rest to the
                        // sequence machinery rather than spin here.
                        return self.settle_sequence_run(now);
                    }
                    // The bytes behind the token may hold another reply (a
                    // coalesced batch), or ordinary input.
                    continue;
                }
                ReplyRun::Open => {
                    // A run past the cap cannot be a terminal reply any more
                    // (nothing real is this large): release it as ordinary
                    // input — the path a run that cannot begin a recognized
                    // reply takes — rather than refresh a deadline that its
                    // own growth may keep pushing away forever.
                    if self.held_over_cap() {
                        return self.flush_held();
                    }
                    // The same window the sequence machinery would have given
                    // this run (its own would expire the string of a reply that
                    // is still arriving), refreshed by every byte that lands:
                    // a claimed string is therefore never released earlier than
                    // it would have been before the claim existed.
                    self.deadline = Some(now + self.sequence_timeout);
                    return Vec::new();
                }
                ReplyRun::Foreign => return self.settle_sequence_run(now),
            }
        }
    }

    /// Classify the held run against the reply grammar. A run that is not
    /// (and cannot become) reply material is [`ReplyRun::Foreign`].
    fn classify_reply_run(&self) -> ReplyRun {
        // Every recognized reply header starts at the run's own `ESC`
        // (`is_control_string` in `has_terminal_reply_header`, and the
        // `ESC [ ?`/`ESC [ >` prefix in `device_attributes_reply_len`), so a
        // run that does not start with one can never be a claim. No
        // synthesized `ESC`: a run without it is a console tail whose ESC
        // crossterm already dropped, and the remaining bytes are
        // indistinguishable from typed text (see the module docs).
        if self.held.first() != Some(&ESC) {
            return ReplyRun::Foreign;
        }
        match scan_reply(&self.held) {
            ReplyScan::Complete { len }
            | ReplyScan::Cancelled { len }
            | ReplyScan::Aborted { len } => ReplyRun::Token { len },
            ReplyScan::Open => ReplyRun::Open,
            ReplyScan::NotAReply => ReplyRun::Foreign,
        }
    }

    /// Upstream's sequence rules, applied to a run the reply grammar did not
    /// claim.
    fn settle_sequence_run(&mut self, now: Instant) -> Vec<Event> {
        match is_complete_sequence(&self.held) {
            SequenceStatus::Complete => self.complete_held(now),
            SequenceStatus::Incomplete => {
                self.deadline = Some(now + self.sequence_timeout);
                Vec::new()
            }
            SequenceStatus::NotEscape => self.flush_held(),
        }
    }

    /// The console-mouse claim's state machine, applied after a byte landed in
    /// an ESC-less `[<…` run (see the module docs). Returns the events to
    /// dispatch.
    ///
    /// * [`SgrMouseTail::Open`] — hold: every plausible prefix (a lone `[`
    ///   included) waits out the sequence window, and each arriving byte
    ///   re-arms it. The console hands the report's records to crossterm one
    ///   read at a time and the gaps between them exceed the 10 ms escape
    ///   window — a `[` held for only that window is flushed as typing and the
    ///   rest of the report lands in the editor as literal text (measured on
    ///   the ConPTY by `scripts/tui_conpty/scenarios/s34_console_mouse_no_typing.py`,
    ///   where the 10 ms default failed and a longer window passed).
    /// * [`SgrMouseTail::Complete`] — decode with the shared SGR decoder and
    ///   emit the `Event::Mouse`. A complete shape outside crossterm's button
    ///   grammar (an unknown button code) is not dropped: the staged events
    ///   are released unchanged.
    /// * [`SgrMouseTail::Invalid`] — the run is ordinary input: released
    ///   unchanged, immediately (never held to the window, never swallowed).
    fn settle_mouse_run(&mut self, now: Instant) -> Vec<Event> {
        match classify_sgr_mouse_tail(&self.held) {
            SgrMouseTail::Open => {
                if self.held_over_cap() {
                    // No mouse report is this large: release the run as the
                    // typed input it must be (including the pending key-up,
                    // whose press is dispatched with the rest).
                    self.pending_byte_release = None;
                    return self.flush_held();
                }
                self.deadline = Some(now + self.sequence_timeout);
                Vec::new()
            }
            SgrMouseTail::Complete => match decode_sgr_mouse_tail(&self.held) {
                Some(mouse) => {
                    self.take_held();
                    vec![Event::Mouse(mouse)]
                }
                None => self.flush_held(),
            },
            SgrMouseTail::Invalid => self.flush_held(),
            // Defensive: only `Open` runs are ever held under the claim, so a
            // NotTail run reaches the sequence machinery instead.
            SgrMouseTail::NotTail => self.settle_sequence_run(now),
        }
    }

    /// Drop the first `len` bytes of the held run, together with the events
    /// that spell them (those bytes are claimed reply bytes and must never be
    /// dispatched). Returns how many bytes were dropped; an event is never
    /// split, so a token end inside one leaves that event for the caller.
    fn drop_held_bytes(&mut self, len: usize) -> usize {
        let mut dropped = 0;
        while dropped < len {
            let Some(end) = self.held_ends.first().copied() else {
                break;
            };
            if end > len {
                break;
            }
            dropped = end;
            self.held_ends.remove(0);
            self.held_events.pop_front();
        }
        self.held.drain(..dropped);
        for end in &mut self.held_ends {
            *end -= dropped;
        }
        dropped
    }

    /// Events whose window expired by `now` (upstream's timer callback).
    pub fn flush_due(&mut self, now: Instant) -> Vec<Event> {
        if self.deadline.is_some_and(|deadline| now >= deadline) {
            self.deadline = None;
            return self.flush_expired();
        }
        Vec::new()
    }

    /// Flush everything still held (loop teardown). An open bracketed paste is
    /// not affected (upstream `flush()` leaves `pasteBuffer` alone): an
    /// unterminated paste is never typed and its payload goes away with the
    /// loop.
    pub fn flush_all(&mut self) -> Vec<Event> {
        self.deadline = None;
        self.pending_byte_release = None;
        self.flush_expired()
    }

    /// The held prefix (diagnostics/tests).
    pub fn held(&self) -> &[u8] {
        &self.held
    }

    /// The pending flush deadline, when bytes are held (the sequence window a
    /// partial escape sequence is waiting out). The event loop clamps its poll
    /// timeout to this so a held ESC/sequence tail still flushes on its own
    /// schedule — a longer idle wait must never delay it.
    pub fn pending_deadline(&self) -> Option<Instant> {
        self.deadline
    }

    /// Whether a bracketed paste is open — its start marker arrived and the
    /// end marker is still outstanding (diagnostics/tests).
    pub fn paste_open(&self) -> bool {
        self.paste.is_some()
    }

    fn take_held(&mut self) -> (Vec<u8>, VecDeque<Event>) {
        self.deadline = None;
        self.held_ends.clear();
        (
            std::mem::take(&mut self.held),
            std::mem::take(&mut self.held_events),
        )
    }

    /// Upstream's expiry flush, resolved to the events its key layer would
    /// produce: a bare ESC is Escape, a two-byte control-string prefix is an
    /// Alt chord, a longer control-string prefix replays its tail as keys, and
    /// an unparseable multi-byte partial is dropped.
    ///
    /// A claimed reply string that never terminated expires here too: for a
    /// control string that is upstream's Alt-prefix split (so `Alt+]` typing is
    /// not eaten), for anything else the staged events are released.
    ///
    /// A console mouse tail that never completed is released as its staged
    /// events as well: the claim exists to turn a report into a mouse event,
    /// and a run that stopped mid-report is the user's `[<…` typing — it must
    /// never be swallowed.
    fn flush_expired(&mut self) -> Vec<Event> {
        let (held, events) = self.take_held();
        if held.is_empty() {
            return Vec::new();
        }
        if held == [ESC] {
            return events.into_iter().collect();
        }
        if held.len() == 2 {
            return decode_input_sequence(&held).unwrap_or_else(|| events.into_iter().collect());
        }
        if is_control_string(&held) {
            let mut out = decode_input_sequence(&held[..2])
                .unwrap_or_else(|| events.front().cloned().into_iter().collect::<Vec<_>>());
            out.extend(events.into_iter().skip(2));
            return out;
        }
        if self.claim_console_mouse && classify_sgr_mouse_tail(&held) != SgrMouseTail::NotTail {
            return events.into_iter().collect();
        }
        Vec::new()
    }

    fn flush_held(&mut self) -> Vec<Event> {
        self.deadline = None;
        let (held, events) = self.take_held();
        if held.is_empty() {
            return Vec::new();
        }
        if held == [ESC] {
            return events.into_iter().collect();
        }
        decode_input_sequence(&held).unwrap_or_else(|| events.into_iter().collect())
    }

    /// A complete sequence was assembled: decode it, or apply upstream's
    /// ESC+ESC restart when a new escape sequence follows.
    fn complete_held(&mut self, now: Instant) -> Vec<Event> {
        if self.held == BRACKETED_PASTE_START {
            // Bracketed-paste start (`ESC [ 2 0 0 ~`). Unix crossterm parses
            // this into `Event::Paste` before the gate ever sees it; its
            // Windows record parser has no paste handling, so the marker
            // arrives as separate key events and is recognized here. The
            // marker is not a key: nothing is dispatched, and the payload is
            // collected until the end marker.
            self.take_held();
            self.paste = Some(Vec::new());
            return Vec::new();
        }
        if self.held == NATIVE_SHIFT_ENTER_LEGACY {
            // Upstream's editor also accepts this Shift+F3-shaped sequence as
            // a newline (`components/editor.ts:895`). The mapping is
            // intentionally shadowing: the gate treats Shift+F3 as Shift+Enter
            // exactly like upstream, which never sees the raw F3 key.
            self.take_held();
            return vec![Event::Key(KeyEvent::new(
                KeyCode::Enter,
                KeyModifiers::SHIFT,
            ))];
        }
        if self.held == [ESC, ESC] {
            let mut events: VecDeque<Event> = std::mem::take(&mut self.held_events);
            self.held_ends.clear();
            self.held.clear();
            let first = events.pop_front();
            if let Some(second) = events.pop_front() {
                self.held.push(ESC);
                self.held_events.push_back(second);
                self.held_ends.push(1);
                self.deadline = Some(now + self.escape_timeout);
            }
            return first.into_iter().collect();
        }
        // Complete but outside the modeled subset: upstream's key layer
        // resolves those to no key at all, so the staged events are dropped
        // with the sequence (they were never dispatched).
        let (held, _events) = self.take_held();
        decode_input_sequence(&held).unwrap_or_default()
    }

    /// Collect one event of an open bracketed paste (upstream `pasteMode`):
    /// the payload is raw bytes and `ESC [ 2 0 1 ~` ends it, or
    /// [`MAX_PASTE_BYTES`] caps it.
    ///
    /// No deadline runs while the paste is open (upstream sets no timer for
    /// `pasteMode`), so the payload may arrive over any span of time — but it
    /// is bounded: an unterminated paste must not grow without limit. Nothing
    /// is dispatched as a keystroke: the payload leaves as one
    /// [`Event::Paste`] once the end marker arrives (or once the cap is
    /// reached). Console-level events (resize, mouse, focus) are not paste
    /// content and pass through immediately; a key whose bytes cannot be
    /// reconstructed (arrows, function keys, Alt/Ctrl chords, key releases) is
    /// dropped — it was never plain text and must not be typed.
    fn collect_paste(&mut self, event: Event, now: Instant) -> Vec<Event> {
        let mut out = Vec::new();
        if let Event::Key(key) = &event {
            let Some(bytes) = paste_payload_bytes(key) else {
                return out;
            };
            self.append_paste_bytes(bytes, now, &mut out);
            return out;
        }
        if let Event::Paste(text) = &event {
            // A Unix `Event::Paste` cannot nest inside a gate-collected paste
            // (crossterm consumes both markers itself); its text is payload
            // either way, faithful to the byte model.
            self.append_paste_bytes(text.as_bytes().to_vec(), now, &mut out);
            return out;
        }
        // Resize / mouse / focus: console-level, not paste content.
        out.push(event);
        out
    }

    /// Append payload bytes and close the paste once the end marker is in
    /// range (upstream `finishPaste`), or once the payload reached
    /// [`MAX_PASTE_BYTES`].
    ///
    /// The cap is a deliberate deviation from upstream (`stdin-buffer.ts`
    /// grows `pasteBuffer` until the terminator): a paste whose `ESC [ 2 0 1 ~`
    /// never arrives — a terminal that drops it, a truncated payload — would
    /// otherwise grow the buffer without bound for as long as the app runs.
    /// Reaching the cap ends the paste exactly like the end marker does: what
    /// was collected is delivered as one [`Event::Paste`] (the user's data is
    /// not lost), the gate returns to normal mode, and everything after it
    /// goes through the normal input path. The check runs once per append, so
    /// the payload can exceed the cap only by the bytes of the append that
    /// crossed it, and the cap is per paste: the next start marker opens a
    /// fresh buffer.
    fn append_paste_bytes(&mut self, bytes: Vec<u8>, now: Instant, out: &mut Vec<Event>) {
        let Some(payload) = self.paste.as_mut() else {
            return;
        };
        payload.extend_from_slice(&bytes);
        // The end marker closes the paste wherever it landed in the payload —
        // including on the very append that also crossed the cap.
        if let Some(end) = find_subslice(payload, BRACKETED_PASTE_END) {
            let content = payload[..end].to_vec();
            let remaining = payload[end + BRACKETED_PASTE_END.len()..].to_vec();
            self.paste = None;
            out.push(Event::Paste(String::from_utf8_lossy(&content).into_owned()));
            if !remaining.is_empty() {
                out.extend(self.reabsorb_bytes(remaining, now));
            }
            return;
        }
        if payload.len() >= MAX_PASTE_BYTES {
            // An unterminated paste is capped (see the doc above): flush what
            // was collected — this append included — as the paste payload and
            // return the gate to normal mode. Nothing collected is dropped.
            let content = std::mem::take(payload);
            self.paste = None;
            out.push(Event::Paste(String::from_utf8_lossy(&content).into_owned()));
        }
    }

    /// Bytes that trailed the end marker in one append (upstream
    /// `finishPaste` re-processes the remainder): hand them back to the normal
    /// reassembly. With crossterm's event granularity a key event contributes
    /// at most one character, so this is a defensive path.
    fn reabsorb_bytes(&mut self, bytes: Vec<u8>, now: Instant) -> Vec<Event> {
        if bytes.first() != Some(&ESC) {
            // Plain text: dispatch it as its own character events (the paste
            // that carried it is already closed).
            return String::from_utf8_lossy(&bytes)
                .chars()
                .map(|c| key_event(KeyCode::Char(c), KeyModifiers::empty()))
                .collect();
        }
        // A run with no event behind it: nothing can be replayed from it, so
        // the parallel event state is empty too.
        self.held_ends.clear();
        self.held = bytes;
        match is_complete_sequence(&self.held) {
            SequenceStatus::Complete => self.complete_held(now),
            SequenceStatus::Incomplete => {
                self.deadline = Some(now + self.sequence_timeout);
                Vec::new()
            }
            SequenceStatus::NotEscape => self.flush_held(),
        }
    }
}

fn is_bare_escape(event: &Event) -> bool {
    matches!(
        event,
        Event::Key(key)
            if key.kind != KeyEventKind::Release
                && key.code == KeyCode::Esc
                && key.modifiers == KeyModifiers::empty()
    )
}

/// Whether the event is a key *release* record. The Windows console synthesizes
/// one press and one release record per byte of an incoming VT sequence (the
/// probe in `scripts/tui_conpty/probe_paste_records.py`), so a release carries
/// no byte and must not end a held run.
fn is_key_release(event: &Event) -> bool {
    matches!(event, Event::Key(key) if key.kind == KeyEventKind::Release)
}

/// Whether the event is one multi-byte character key press (a CJK character,
/// an emoji, any non-ASCII text). Such an event is a single logical key: its
/// UTF-8 bytes must never be treated as a run of one-byte keys by the
/// escape-sequence machinery (see `StdinGate::push`).
fn is_multibyte_char_event(event: &Event) -> bool {
    matches!(
        event,
        Event::Key(key)
            if key.kind != KeyEventKind::Release
                && matches!(key.code, KeyCode::Char(c) if !c.is_ascii())
    )
}

/// The key code of a key event — the pairing key of the console's per-byte
/// press/release record synthesis (see `StdinGate::pending_byte_release`).
fn key_code_of(event: &Event) -> Option<KeyCode> {
    match event {
        Event::Key(key) => Some(key.code),
        _ => None,
    }
}

/// The bytes a key event contributes to an ESC-less console mouse tail: plain
/// characters whose modifiers are at most SHIFT. The console reports the
/// characters of a synthesized report the way a keyboard would — `<` and `M`
/// carry `SHIFT_PRESSED` (`to_string`'s UTF-8 bytes are still just those
/// characters) — while `[`, the digits and `;` are unshifted. Control/Alt
/// chords are never mouse material (and must not be eaten as if they were).
fn mouse_tail_bytes(event: &Event) -> Option<Vec<u8>> {
    let Event::Key(key) = event else {
        return None;
    };
    if key.kind == KeyEventKind::Release {
        return None;
    }
    if key
        .modifiers
        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
    {
        return None;
    }
    match key.code {
        KeyCode::Char(c) if c.is_ascii() => Some(c.to_string().into_bytes()),
        _ => None,
    }
}

/// The bytes a parsed event stands for, when the parse was 1:1 with single
/// bytes. Multi-byte sequences (arrows, function keys, mouse reports, paste)
/// return `None`: crossterm already consumed their bytes.
fn event_input_bytes(event: &Event) -> Option<Vec<u8>> {
    let Event::Key(key) = event else {
        return None;
    };
    key_input_bytes(key)
}

/// [`event_input_bytes`] for a key event.
fn key_input_bytes(key: &KeyEvent) -> Option<Vec<u8>> {
    if key.kind == KeyEventKind::Release {
        return None;
    }
    let modifiers = key.modifiers;
    match key.code {
        KeyCode::Esc if modifiers.is_empty() => Some(vec![ESC]),
        KeyCode::Enter if modifiers.is_empty() => Some(vec![b'\r']),
        KeyCode::Tab if modifiers.is_empty() => Some(vec![b'\t']),
        KeyCode::Backspace if modifiers.is_empty() => Some(vec![0x7f]),
        KeyCode::Char('\r') if modifiers.is_empty() => Some(vec![b'\r']),
        KeyCode::Char('\n') if modifiers.is_empty() => Some(vec![b'\n']),
        KeyCode::Char('\t') if modifiers.is_empty() => Some(vec![b'\t']),
        KeyCode::Char(c) if modifiers == KeyModifiers::CONTROL => {
            control_byte(c).map(|byte| vec![byte])
        }
        KeyCode::Char(c) if modifiers.is_empty() => Some(c.to_string().into_bytes()),
        _ => None,
    }
}

/// Resolve the console's spelling of a line feed to the LF key shape.
///
/// A raw `\n` has no console key of its own, so ConPTY's legacy (non-VT-input)
/// translation synthesizes a Ctrl+Enter chord for it: a `VK_CONTROL` down/up
/// pair bracketing a `VK_RETURN` record whose `uChar` is `\n` and whose
/// `dwControlKeyState` has `LEFT_CTRL_PRESSED` (observed with
/// `scripts/tui_conpty/probe_paste_records.py`). crossterm's record parser
/// yields no event for the `VK_CONTROL` records (`VK_SHIFT | VK_CONTROL |
/// VK_MENU => None`, `event/sys/windows/parse.rs:243`) and maps `VK_RETURN` to
/// [`KeyCode::Enter`] with the control state mirrored into the modifiers
/// (`:246`), so the chord arrives as `Enter` + [`KeyModifiers::CONTROL`] — a
/// shape no editor or dialog binding matches. Left alone it inserts nothing,
/// which is how a multi-line paste loses every line break on Windows.
///
/// A line feed *is* `ctrl+j` (`0x0a`), which this codebase already spells as
/// `Char('j')` + CONTROL: [`super::terminal::plain_enter_key`] and
/// `should_submit_on_backslash_enter` call that shape "the LF shape", the
/// input editor binds its `newline` action to `ctrl+j`
/// (`keybindings.rs:19`, `:124`), and a dialog's text input reads it as
/// `DialogKey::Newline` (`dialogs.rs:991`). It is also exactly what
/// crossterm's Unix byte parser produces for the byte `0x0a`, so rewriting the
/// console record to this shape makes the Windows path behave like a
/// byte-oriented terminal: a pasted or typed line feed inserts a line break,
/// and inside an open bracketed paste the same event contributes the byte
/// `0x0a` ([`key_input_bytes`] maps it through [`control_byte`]) — the byte a
/// Unix parser carries between the paste markers.
///
/// CR needs no rewrite: the console sends it as a bare `Return` (`uChar` `\r`,
/// no control state), [`key_input_bytes`] still maps it to `b"\r"`, and a CRLF
/// ending therefore keeps both bytes for the editor's newline normalization to
/// collapse into the single line break Unix produces.
/// Coalesce a drained input batch's leading rapid-text run into one
/// `Event::Paste`. A terminal without bracketed-paste markers delivers a paste
/// as one Key event per character; inserting each character separately is what
/// makes a pasted paragraph appear one glyph at a time (and re-run the file
/// scan per character). Returns `true` when the queue was rewritten.
///
/// The run's final plain Enter stays a key, not paste payload, when the run has
/// no line break before it: `"/compact\r"` and a submitted prompt are one line
/// of text plus a submit, and swallowing that Enter into the paste would leave
/// the command sitting in the editor.
///
/// A run containing a line break is a real multi-line paste and coalesces
/// regardless of its length: the whole run (trailing line break included)
/// becomes one `Event::Paste`, so a pasted block keeps its newlines instead of
/// auto-submitting. Every spelling of a line break counts — plain Enter,
/// `Char('\r')`/`Char('\n')`, and the console's `ctrl+j` LF chord (a Windows
/// CRLF paste delivers a plain Enter followed by this chord). A run that is
/// only line breaks is key repeat, never a paste. ctrl+Enter always
/// contributes a newline and never acts as a submit.
///
/// Tab is paste payload inside such a run but never turns a single-line run
/// into a paste: `tui.input.tab` is an editor action (accept the autocomplete
/// item / Tab completion), so a lone Tab or a typed prefix followed by Tab
/// must stay key events. Coalescing it into `Event::Paste("\t")` bypasses
/// `handle_key` and types a literal tab instead of accepting the completion.
pub fn coalesce_paste_burst(queue: &mut VecDeque<Event>) -> bool {
    let mut presses = 0usize;
    let mut printable = 0usize;
    let mut end = 0usize;
    let mut text = String::new();
    let mut last_plain_enter: Option<usize> = None;
    let mut pending_line_break = false;
    let mut internal_line_break = false;
    let mut saw_line_break = false;
    // A Tab was part of the run. Inside a real paste it is payload; without a
    // real line break the run stays ordinary typing (see the final gate).
    let mut has_tab = false;
    // The previous contributing key was a plain Enter. A following LF chord is
    // the second half of a CRLF pair and must not add a second newline.
    let mut previous_plain_enter = false;
    for (index, event) in queue.iter().enumerate() {
        match event {
            Event::Key(key) if key.kind == KeyEventKind::Release => {
                // Windows consoles report a release after every press.
                end = index + 1;
            }
            Event::Key(key) if key.kind == KeyEventKind::Press => {
                let control = key.modifiers == KeyModifiers::CONTROL;
                let text_char = matches!(key.code, KeyCode::Char(_))
                    && !key
                        .modifiers
                        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT);
                // The Windows LF chord (`Ctrl+Enter` resolved by the gate, or
                // a typed Ctrl+J): bytes 0x0a. Never a submit.
                let line_feed = key.code == KeyCode::Char('j') && control;
                // The console CR spelling (a plain Enter normally arrives as
                // KeyCode::Enter, but a bare `\r` can be a Char).
                let carriage_return = (key.code == KeyCode::Char('\r')
                    || key.code == KeyCode::Char('m') && control)
                    && (key.modifiers.is_empty() || control);
                let tab = key.code == KeyCode::Tab && key.modifiers.is_empty();
                let enter = key.code == KeyCode::Enter && (key.modifiers.is_empty() || control);
                if !text_char && !line_feed && !tab && !enter && !carriage_return {
                    break;
                }
                presses += 1;
                match key.code {
                    KeyCode::Enter => {
                        text.push('\n');
                        pending_line_break = true;
                        saw_line_break = true;
                        last_plain_enter = key.modifiers.is_empty().then_some(index);
                        previous_plain_enter = key.modifiers.is_empty();
                    }
                    KeyCode::Char('j') if line_feed => {
                        // A CRLF pair contributes ONE line break: the LF half
                        // immediately after a plain Enter is the same newline.
                        if !previous_plain_enter {
                            text.push('\n');
                        }
                        pending_line_break = true;
                        saw_line_break = true;
                        last_plain_enter = None;
                        previous_plain_enter = false;
                    }
                    KeyCode::Char('\r') | KeyCode::Char('m') if carriage_return => {
                        text.push('\n');
                        pending_line_break = true;
                        saw_line_break = true;
                        last_plain_enter = Some(index);
                        previous_plain_enter = true;
                    }
                    KeyCode::Char(character) => {
                        if pending_line_break {
                            internal_line_break = true;
                            pending_line_break = false;
                        }
                        printable += 1;
                        text.push(character);
                        last_plain_enter = None;
                        previous_plain_enter = false;
                    }
                    KeyCode::Tab => {
                        // Inside a real pasted block (the run already has a
                        // line break) the tab is payload and a separator: a
                        // trailing Enter after it belongs to the block.
                        // `has_tab` keeps a single-line run (no real line
                        // break) as key events so Tab reaches `handle_key`.
                        has_tab = true;
                        internal_line_break = true;
                        printable += 1;
                        text.push('\t');
                        last_plain_enter = None;
                        previous_plain_enter = false;
                    }
                    _ => break,
                }
                end = index + 1;
            }
            _ => break,
        }
    }
    if presses == 0 || end == 0 || printable == 0 {
        return false;
    }
    // A one-line run ending in a plain Enter: coalesce only the text before
    // that Enter and leave the submit key in the queue.
    if !internal_line_break {
        if let Some(enter_index) = last_plain_enter {
            if enter_index == 0 || presses - 1 < PASTE_COALESCE_RUN {
                return false;
            }
            text.pop();
            for _ in 0..enter_index {
                queue.pop_front();
            }
            queue.push_front(Event::Paste(text));
            return true;
        }
    }
    // A Tab without a real line break is an editor action key, not paste
    // payload: a lone Tab or a typed prefix followed by Tab must reach
    // `handle_key` (`tui.input.tab` → accept the highlighted autocomplete
    // item / Tab completion), not become `Event::Paste("\t")`.
    if has_tab && !saw_line_break {
        return false;
    }
    // A run with a line break is a paste at any length; a single-line run is
    // ordinary typing unless it is a rapid burst.
    if !saw_line_break && !internal_line_break && presses < PASTE_COALESCE_RUN {
        return false;
    }
    for _ in 0..end {
        queue.pop_front();
    }
    queue.push_front(Event::Paste(text));
    true
}

fn resolve_console_line_feed(event: Event) -> Event {
    let Event::Key(key) = &event else {
        return event;
    };
    if key.kind == KeyEventKind::Release
        || key.code != KeyCode::Enter
        || key.modifiers != KeyModifiers::CONTROL
    {
        return event;
    }
    Event::Key(KeyEvent::new_with_kind(
        KeyCode::Char('j'),
        KeyModifiers::CONTROL,
        key.kind,
    ))
}

/// The payload bytes a key event contributes to an open bracketed paste: plain
/// text only, never a key. [`key_input_bytes`] covers the unmodified shapes;
/// a character the console reported together with its case modifier
/// (`SHIFT_PRESSED` accompanies pasted uppercase text, crossterm's Windows
/// parser mirrors `dwControlKeyState` into [`KeyModifiers`]) is still that
/// character.
fn paste_payload_bytes(key: &KeyEvent) -> Option<Vec<u8>> {
    if let Some(bytes) = key_input_bytes(key) {
        return Some(bytes);
    }
    match key.code {
        KeyCode::Char(c)
            if key.kind != KeyEventKind::Release && key.modifiers == KeyModifiers::SHIFT =>
        {
            Some(c.to_string().into_bytes())
        }
        _ => None,
    }
}

/// `ctrl+<letter>` as the control byte crossterm's parser would have read.
fn control_byte(c: char) -> Option<u8> {
    if c.is_ascii_alphabetic() {
        Some(c.to_ascii_uppercase() as u8 & 0x1f)
    } else {
        match c {
            ' ' => Some(0x00),
            '[' => Some(0x1b),
            '\\' => Some(0x1c),
            ']' => Some(0x1d),
            '^' => Some(0x1e),
            '_' => Some(0x1f),
            _ => None,
        }
    }
}
