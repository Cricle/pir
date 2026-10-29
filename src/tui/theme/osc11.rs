//! Terminal color detection — upstream `system-theme.ts`
//! `terminalAppearance` + `theme.ts` `detectTerminalTheme` (background →
//! mode-2031 report → COLORFGBG → dark fallback chain) and upstream
//! `tui.ts`'s `queryTerminalColors` (OSC 10 foreground, OSC 11 background,
//! OSC 4 palette 0-15, DA1 terminator).

use std::sync::{Mutex, OnceLock};

use crate::tui::terminal_reply::{reply_input_state, reply_tokens_len, ReplyInputState};

use super::color::relative_luminance;
use super::palette::Appearance;
use super::terminal_colors::{
    TerminalColorFeed, TerminalColorQuery, TerminalColors, TERMINAL_COLOR_QUERY,
    TERMINAL_COLOR_SCHEME_NOTIFY_OFF, TERMINAL_COLOR_SCHEME_NOTIFY_ON,
};

/// The OSC 11 background-color query sent to the terminal.
pub const OSC11_QUERY: &str = "\x1b]11;?\x1b\\";

/// How long to wait for the OSC 11 reply before falling back (upstream
/// `TERMINAL_QUERY_TIMEOUT_MS`; narrowed from upstream's 100 ms because the
/// read is non-consuming on both platforms — nothing is taken while the
/// terminal is silent — so the window only bounds how long startup waits for
/// the answer and how long a *recognized* reply string may still be read
/// through its terminator ([`LATE_REPLY_HOLD`])).
pub const TERMINAL_QUERY_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(50);

/// How long the reader keeps consuming a *claimed* terminal reply string
/// (D19 = upstream `edb2eb966`) while it waits for the string's terminator:
/// `OSC 11`/`OSC 4`/`OSC 7501`/XTVersion/Kitty replies all terminate within
/// milliseconds, and the bound keeps a terminal that never terminates from
/// holding the reader — and, because both readers run on the calling thread,
/// the startup — past the query window.
pub const LATE_REPLY_HOLD: std::time::Duration = std::time::Duration::from_millis(1000);

/// Byte cap for the same hold: a reply longer than this is released rather
/// than held (real replies are tens of bytes). It bounds the bytes one reader
/// keeps as a possible reply on both platforms.
pub const MAX_LATE_REPLY_BYTES: usize = 4096;

/// Byte cap for the bytes one reply-read session gives back to the input path
/// (the reader's hand-back buffer, see [`ReplyReadOutcome::handed_back`]).
///
/// A tty has no peek: when `poll(2)` reports input the Unix reader must
/// *read*, so a keystroke that raced the query window cannot be put back into
/// the tty. Those bytes are preserved in arrival order instead (see
/// [`PENDING_STDIN_BYTES`]), and once this many bytes have been taken the
/// reader stops taking bytes altogether — so the cap bounds this process'
/// memory without ever dropping a byte: what the reader does not take stays in
/// the terminal's own input queue (which flow control keeps bounded) for the
/// input path to read once the event loop starts. 64 KiB is far above what a
/// human types into a 50 ms startup window and far below a memory concern
/// (compare `crate::tui::stdin_buffer`'s `MAX_PASTE_BYTES`, 8 MiB).
pub const MAX_HANDED_BACK_BYTES: usize = 64 * 1024;

/// Bytes one look at the terminal's input may take at a time. It is the size
/// the reader's hand-back budget reserves for the next look
/// ([`MAX_HANDED_BACK_BYTES`]), so one session hands back the cap plus at most
/// this much.
const READ_CHUNK_BYTES: usize = 256;

/// How long one look at the terminal's input may block while the reader waits
/// for the answer: the Windows reader sleeps this long between two peeks and
/// the Unix reader passes it to `poll(2)` as the timeout. The whole wait is the
/// query window ([`TERMINAL_QUERY_TIMEOUT`]), so this is a few dozen wake-ups,
/// and an answer that lands is seen within a millisecond of its arrival.
const REPLY_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(1);

/// How many console records one peek covers on Windows (a full answer to
/// [`TERMINAL_COLOR_QUERY`] is a little over 200 one-character records; the
/// reader peeks again while a batch is exhausted).
#[cfg(windows)]
const MAX_PEEKED_RECORDS: usize = 512;

/// Bytes a reply reader had to take out of the terminal input but that are not
/// part of a recognized reply, in arrival order — the hand-back queue.
///
/// The Unix reader ([`unix_reply_reader`]) can only look at fd 0 by *reading*
/// it (`poll(2)` first, but a tty has no peek), so a byte it takes that turns
/// out to be ordinary input — a keystroke that raced the query window, an
/// unterminated reply string released at its bound (D19) — cannot be put back
/// into the tty, and crossterm has no API for injecting raw bytes into its
/// event stream. Those bytes are preserved here, in order, for the input path
/// to drain ([`take_pending_stdin_bytes`]); the reader stops taking bytes once
/// the queue is full ([`MAX_HANDED_BACK_BYTES`]), so nothing it took is ever
/// dropped — what it cannot keep stays in the terminal's own input queue.
///
/// The queue is filled by at most one reader session: the process-wide query
/// gate ([`OSC11_QUERY_GATE`]) lets exactly one query initiate and a reader
/// that never starts hands back nothing, so it holds [`MAX_HANDED_BACK_BYTES`]
/// bytes at most.
///
/// The Windows reader never takes a byte that is not a reply, so it never hands
/// anything back: on Windows the cell stays empty.
static PENDING_STDIN_BYTES: OnceLock<Mutex<Vec<u8>>> = OnceLock::new();

/// Take (and clear) the bytes a reply reader handed back because they are not
/// part of a recognized reply. Empty unless a byte the reader had to take was
/// ordinary input — the typed line that raced the query window, or a reply
/// string that ran out its hold unterminated; always empty on Windows.
#[allow(dead_code)] // process-wide hand-back channel (re-exported by `theme`), no in-crate consumer yet
pub(crate) fn take_pending_stdin_bytes() -> Vec<u8> {
    match PENDING_STDIN_BYTES
        .get_or_init(|| Mutex::new(Vec::new()))
        .lock()
    {
        Ok(mut pending) => std::mem::take(&mut *pending),
        Err(_) => Vec::new(),
    }
}

/// Hand bytes the Unix reader had to take but that are not part of a recognized
/// reply back to the process' input path, in order (see
/// [`PENDING_STDIN_BYTES`]).
#[cfg(not(windows))]
fn hand_back_stdin_bytes(bytes: &[u8]) {
    if bytes.is_empty() {
        return;
    }
    if let Ok(mut pending) = PENDING_STDIN_BYTES
        .get_or_init(|| Mutex::new(Vec::new()))
        .lock()
    {
        pending.extend_from_slice(bytes);
    }
    tracing::debug!(
        count = bytes.len(),
        "OSC 11 reader handed bytes that are not a terminal color reply back to the input path"
    );
}

/// The light/dark theme names in effect for the auto settings, recorded by
/// [`super::set_theme_from_settings`] so a later appearance change (the
/// terminal's mode-2031 report) can re-resolve the pair to a concrete name
/// (upstream `reapplyForTerminal`). Neither platform's reply reader applies
/// colors behind the caller's back any more, so the record is what keeps a
/// `light/dark` pair resolvable after the startup query.
static AUTO_THEME_NAMES: OnceLock<Mutex<Option<(String, String)>>> = OnceLock::new();

/// Record the light/dark names the auto theme setting resolved to.
pub(super) fn record_auto_theme_names(light: &str, dark: &str) {
    if let Ok(mut slot) = AUTO_THEME_NAMES.get_or_init(|| Mutex::new(None)).lock() {
        *slot = Some((light.to_owned(), dark.to_owned()));
    }
}

/// The recorded light/dark names of an auto (`light/dark`) theme setting,
/// `None` when the setting is not a pair.
pub(super) fn recorded_auto_theme_names() -> Option<(String, String)> {
    AUTO_THEME_NAMES
        .get()
        .and_then(|cell| cell.lock().ok())
        .and_then(|slot| slot.clone())
}

/// Parse one hex channel of an OSC color reply: any-length hex scaled to
/// 0-255 (upstream `parseOscHexChannel`).
fn osc_hex_channel(channel: &str) -> Option<u8> {
    if channel.is_empty() || !channel.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let max = 16f64.powi(channel.len() as i32) - 1.0;
    let value = u32::from_str_radix(channel, 16).ok()? as f64;
    Some(((value / max) * 255.0).round() as u8)
}

/// Parse an OSC color reply payload: `#RRGGBB`, `#RRRRGGGGBBBB`, or
/// `rgb:R/G/B` with any consistent hex channel length (upstream
/// `parseOscColorValue`).
fn parse_osc_color_payload(value: &str) -> Option<(u8, u8, u8)> {
    let value = value.trim();
    if let Some(hex) = value.strip_prefix('#') {
        // The payload between the reply markers is arbitrary terminal bytes;
        // a non-ASCII payload would make the byte-index slicing below panic
        // on a char boundary, and hex digits are ASCII anyway — so a
        // non-ASCII payload is simply not a color.
        if !hex.is_ascii() {
            return None;
        }
        return match hex.len() {
            6 => {
                let r = u8::from_str_radix(&hex[0..2], 16).ok()?;
                let g = u8::from_str_radix(&hex[2..4], 16).ok()?;
                let b = u8::from_str_radix(&hex[4..6], 16).ok()?;
                Some((r, g, b))
            }
            12 => Some((
                osc_hex_channel(&hex[0..4])?,
                osc_hex_channel(&hex[4..8])?,
                osc_hex_channel(&hex[8..12])?,
            )),
            _ => None,
        };
    }
    let lowered = value.to_ascii_lowercase();
    let rgb = lowered
        .strip_prefix("rgba:")
        .or_else(|| lowered.strip_prefix("rgb:"))?;
    let mut channels = rgb.split('/');
    let r = osc_hex_channel(channels.next()?)?;
    let g = osc_hex_channel(channels.next()?)?;
    let b = osc_hex_channel(channels.next()?)?;
    if channels.next().is_some() {
        return None;
    }
    Some((r, g, b))
}

/// Parse an OSC 11 background-color reply
/// (`ESC ] 11 ; <payload> BEL | ESC \`), upstream `parseOscColorResponse`
/// restricted to the background target.
pub fn parse_osc11_background(data: &str) -> Option<(u8, u8, u8)> {
    let rest = data.strip_prefix('\x1b')?.strip_prefix("]11;")?;
    let cut = rest.find('\x07').or_else(|| rest.find("\x1b\\"))?;
    parse_osc_color_payload(&rest[..cut])
}

/// Whether a terminal is dark or light from its reported background: dark
/// when white text has at least as much WCAG contrast on it as black text
/// (upstream `terminalAppearance` without a reported foreground).
pub fn appearance_from_background(background: (u8, u8, u8)) -> Appearance {
    let luminance = relative_luminance(background.0, background.1, background.2);
    let white_contrast = 1.05 / (luminance + 0.05);
    let black_contrast = (luminance + 0.05) / 0.05;
    if white_contrast >= black_contrast {
        Appearance::Dark
    } else {
        Appearance::Light
    }
}

/// Dark or light from the `COLORFGBG` environment variable some terminals
/// set (upstream `detectColorFgBgTheme`, classified by index like Vim:
/// 0-6 and 8 are dark, 7 and 9-15 are light).
pub fn detect_colorfgbg_theme() -> Option<Appearance> {
    let value = std::env::var("COLORFGBG").ok()?;
    let bg = value.rsplit(';').next()?.trim();
    if bg.is_empty() || !bg.chars().all(|c| c.is_ascii_digit()) || bg.len() > 2 {
        return None;
    }
    let index: usize = bg.parse().ok()?;
    if index > 15 {
        return None;
    }
    Some(if index <= 6 || index == 8 {
        Appearance::Dark
    } else {
        Appearance::Light
    })
}

/// The terminal's appearance from everything reported so far (upstream
/// `getTerminalTheme`): the reported background (OSC 11), then its mode-2031
/// light/dark report, then `COLORFGBG`, then dark. Queries the terminal
/// (once) when nothing has been reported yet.
pub fn detect_terminal_appearance() -> Appearance {
    let recorded = super::state::terminal_colors();
    let scheme = super::state::terminal_color_scheme();
    if recorded.background.is_some() || scheme.is_some() {
        return super::terminal_colors::detect_terminal_theme(
            &recorded,
            scheme,
            detect_colorfgbg_theme(),
        );
    }
    if let Some(colors) = query_terminal_colors(TERMINAL_QUERY_TIMEOUT) {
        return super::terminal_colors::detect_terminal_theme(
            &colors,
            scheme,
            detect_colorfgbg_theme(),
        );
    }
    detect_colorfgbg_theme().unwrap_or(Appearance::Dark)
}

/// Query the terminal's default background color via OSC 11 and parse the
/// reply (upstream `queryTerminalColors`' background half).
pub fn query_terminal_background(timeout: std::time::Duration) -> Option<(u8, u8, u8)> {
    query_terminal_colors(timeout).and_then(|colors| colors.background)
}

/// Query the terminal's colors: the default foreground (OSC 10), the default
/// background (OSC 11), ANSI colors 0-15 (OSC 4) and a DA1 request that
/// marks the end of the replies (upstream `queryTerminalColors`, tui.ts:1494-1515
/// with `TERMINAL_COLOR_QUERY`, tui.ts:168-172). Writes the query, waits up
/// to `timeout` for the replies, and returns what arrived.
/// `colors.palette` is only set when all 16 palette entries arrived.
///
/// The reader must never take keyboard input away from the application, and
/// the two platforms reach that differently:
///
/// - **Windows** ([`console_reply_reader`]): the reader *peeks* at the
///   console's pending records (`PeekConsoleInputW`, reached through
///   [`crate::tui::stdin_buffer::peek_pending_console_input`]) and takes them
///   only once they are a recognized terminal reply
///   ([`crate::tui::terminal_reply::reply_take`]). A console that does not
///   answer the query has nothing pending for the whole window, so **not one
///   record is consumed** — no matter when the user types — and the reader
///   never blocks on stdin, so it cannot outlive the query either. A claimed
///   reply is consumed through its terminator, so a fragmented reply cannot
///   leak its hex tail into the prompt nor open the external editor through
///   its BEL (D19 = upstream `edb2eb966`); anything that cannot be part of a
///   reply stays in the console queue for crossterm. The wait is bounded by
///   `timeout`, plus [`LATE_REPLY_HOLD`] while an unterminated *claimed*
///   string waits for its tail. The DA1 answer (`ESC [ ? … c`) is a
///   recognized reply token like the OSC ones and is consumed too — leaving
///   it queued would hand crossterm records whose leading ESC it drops (VK 0),
///   typing `[?61;6;…c` into the prompt.
/// - **Unix** ([`unix_reply_reader`], `#[cfg(not(windows))]`): a tty has no
///   peek, so the reader *polls* fd 0 (`poll(2)`, with `O_NONBLOCK` for the
///   read itself) and reads **only when the terminal has something to say**.
///   A terminal that does not answer the query therefore has nothing readable
///   for the whole window: not one byte is taken — no matter when the user
///   types — and no read can block past the window. Because a byte that turned
///   out not to be a reply cannot be put back into the tty (and crossterm has
///   no API for injecting raw bytes), it is never swallowed either: the reader
///   hands every such byte back, in arrival order, through the process-wide
///   pushback queue ([`take_pending_stdin_bytes`]) — a keystroke that raced the
///   window, an Alt chord, an unterminated reply string released at its bound —
///   and stops taking bytes once that queue is full, so the bytes it cannot
///   keep stay in the terminal's own input queue for crossterm. The hold policy
///   is the Windows one: a recognized reply string is consumed through its
///   terminator (D19), the wait is bounded by `timeout` plus
///   [`LATE_REPLY_HOLD`] while a claimed string is open, the DA1 answer is
///   consumed, and an unterminated string is released at the bound. The reader
///   runs on the calling thread and returns when the window is over, so it
///   cannot outlive the query: a reply that lands afterwards is not read here at
///   all (upstream's `onLateReply: apply` has no counterpart on either
///   platform) — it is left to the live input gate, which applies the same
///   reply grammar to whatever arrives once the event loop runs
///   ([`crate::tui::stdin_buffer`]'s `StdinGate`). `poll(2)`/`read(2)` are the
///   documented `unsafe_code` exception of this module ([`stdin_poll`]).
///
/// Queries run before the interactive event loop: the only caller,
/// [`super::set_theme_from_settings`], runs during startup, before
/// `enable_raw_mode()`, because crossterm owns stdin parsing once the loop
/// starts. Raw mode is enabled here for the duration of the query: in cooked
/// mode the line discipline would hold the (newline-less) reply until Enter.
/// Skipped when stdin/stdout are not TTYs (tests, pipes) or when
/// `PIR_DISABLE_TERMINAL_QUERY` is set; at most one query per process — the
/// gate ([`OSC11_QUERY_GATE`]) is claimed only when the query actually starts,
/// so failed checks never consume that budget.
///
/// On any failure the caller falls back to [`detect_colorfgbg_theme`] /
/// dark — the query result is never required.
pub fn query_terminal_colors(timeout: std::time::Duration) -> Option<TerminalColors> {
    use crossterm::tty::IsTty;

    if std::env::var_os("PIR_DISABLE_TERMINAL_QUERY").is_some() {
        return None;
    }
    let stdin_is_tty = std::io::stdin().is_tty();
    let stdout_is_tty = std::io::stdout().is_tty();
    if !stdin_is_tty || !stdout_is_tty {
        return None;
    }
    // Every check passed: this is the moment the query really starts, so
    // claim the process-wide gate here. A failed check above returns the
    // fallback path without consuming it, and concurrent callers past this
    // point get `None` from the losing claim.
    if !try_claim_query_gate(&OSC11_QUERY_GATE) {
        return None;
    }
    // Own raw mode for the duration of the query: in cooked mode the line
    // discipline would hold the (newline-less) reply until Enter.
    if crossterm::terminal::enable_raw_mode().is_err() {
        return None;
    }
    let reply = collect_terminal_color_replies(timeout);
    let _ = crossterm::terminal::disable_raw_mode();
    reply
}

/// Write [`TERMINAL_COLOR_QUERY`] and collect the replies with this platform's
/// reader; a write failure is the fallback path, exactly as before the query.
fn collect_terminal_color_replies(timeout: std::time::Duration) -> Option<TerminalColors> {
    use std::io::Write as _;

    let mut out = std::io::stdout();
    out.write_all(TERMINAL_COLOR_QUERY.as_bytes()).ok()?;
    out.flush().ok()?;
    #[cfg(windows)]
    {
        console_reply_reader(timeout)
    }
    #[cfg(not(windows))]
    {
        unix_reply_reader(timeout)
    }
}

/// The Windows reply reader: peek the console's pending records
/// (`PeekConsoleInputW`), take only what is already a reply, and feed the
/// complete replies to the query. See [`query_terminal_colors`] for the full
/// discipline; the essential property is that a console with nothing pending —
/// the console that never answers the query — never has a single record taken
/// out of it.
#[cfg(windows)]
fn console_reply_reader(timeout: std::time::Duration) -> Option<TerminalColors> {
    use std::time::Instant;

    use crate::tui::stdin_buffer::{consume_console_records, peek_pending_console_input};
    use crate::tui::terminal_reply::{reply_input_state, reply_take, ReplyInputState, ReplyTake};

    let mut query = TerminalColorQuery::new();
    let mut collected: Vec<u8> = Vec::new();
    let answer_deadline = Instant::now() + timeout;
    // A *claimed* reply string that has not terminated yet keeps the reader
    // consuming its tail (D19); [`LATE_REPLY_HOLD`] bounds that hold, and a
    // fragment that never completes is never taken at all.
    let hold_deadline = answer_deadline + LATE_REPLY_HOLD;
    loop {
        let pending = peek_pending_console_input(MAX_PEEKED_RECORDS);
        // A *claimed* reply string that has not terminated yet keeps the
        // reader consuming its tail (D19): the hold window applies while that
        // tail is awaited — also when the only record visible is the lone
        // `ESC` that may open the string's `ST` terminator, which
        // [`reply_take`] defers rather than consume until the next record
        // settles what it is. Before the deferral existed an open run always
        // took such a record, so both wait arms now share this deadline.
        let deadline = if reply_input_state(&collected) == ReplyInputState::Open {
            hold_deadline
        } else {
            answer_deadline
        };
        if pending.is_empty() {
            if Instant::now() >= deadline {
                break;
            }
            std::thread::sleep(REPLY_POLL_INTERVAL);
            continue;
        }
        let units: Vec<Vec<u8>> = pending
            .iter()
            .map(|record| record.key_bytes().unwrap_or_default())
            .collect();
        match reply_take(&collected, units.iter().map(Vec::as_slice)) {
            ReplyTake::Units(count) => {
                // Take exactly the records that form the reply; the rest of
                // the peeked batch stays queued for crossterm.
                let taken = consume_console_records(count);
                if taken == 0 {
                    break;
                }
                for unit in units.iter().take(taken) {
                    collected.extend_from_slice(unit);
                }
                if let Some(colors) = feed_reply_tokens(&mut query, &mut collected) {
                    return Some(colors);
                }
                if collected.len() >= MAX_LATE_REPLY_BYTES {
                    break;
                }
            }
            // The front of the console queue cannot be part of a reply: leave
            // it — and everything behind it — where it is.
            ReplyTake::Stop => break,
            // Only a fragment of a possible reply has arrived; nothing is
            // taken yet, so the wait is bounded by the deadline above — the
            // hold window while a claimed string is still open, the query
            // window otherwise.
            ReplyTake::Wait => {
                if Instant::now() >= deadline {
                    break;
                }
                std::thread::sleep(REPLY_POLL_INTERVAL);
            }
        }
    }
    let colors = query.timeout();
    (!colors.is_empty()).then_some(colors)
}

/// Feed the complete reply strings the reader took out of the console queue to
/// the query and forget them — upstream consumes every reply string from its
/// stdin buffer, whether or not it is a color reply, so an OSC 7501 notice or
/// an XTVersion answer is dropped here.
///
/// The queue can hand over several replies at once: the terminal answers
/// [`TERMINAL_COLOR_QUERY`] with up to 19 of them (OSC 10, OSC 11, 16 × OSC 4,
/// DA1) and a peek batch can hold them coalesced. The run is therefore split at
/// the token boundaries and each reply is fed on its own
/// ([`TerminalColorQuery::feed_replies`]): `feed` parses exactly one reply, and
/// a concatenation of two is neither, so feeding the batch as one string would
/// consume it while recording no color at all. Only complete reply strings are
/// consumed; an incomplete trailing one stays in `collected` for the next batch
/// (the reader is already taking its tail through its terminator). The DA1
/// answer is one of these tokens and completes the query (delivering the colors
/// reported so far), exactly like the all-replies-complete path. Returns the
/// colors once the query completed.
#[cfg(windows)]
fn feed_reply_tokens(
    query: &mut TerminalColorQuery,
    collected: &mut Vec<u8>,
) -> Option<TerminalColors> {
    let (verdict, consumed) = query.feed_replies(collected);
    collected.drain(..consumed);
    match verdict {
        TerminalColorFeed::Delivered(colors) => Some(colors),
        TerminalColorFeed::Consumed | TerminalColorFeed::NotAReply => None,
    }
}

/// What one reply-read session produced (the return of [`read_reply_bytes`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplyReadOutcome {
    /// The colors the query collected: the delivered set (the DA1 answer or
    /// every color reported), else whatever the terminal reported before the
    /// window closed; `None` when it reported nothing.
    pub colors: Option<TerminalColors>,
    /// Every byte the session took out of the terminal that is not part of a
    /// recognized reply, in arrival order: input that raced the query window
    /// (a typed line, an Alt chord), or an unterminated reply string released
    /// at its bound (D19). The caller hands them back to the input path
    /// ([`PENDING_STDIN_BYTES`]); nothing the session took is dropped — it
    /// stops taking bytes before this buffer would exceed the session's cap.
    pub handed_back: Vec<u8>,
}

/// What a raw reader does with the bytes it has already taken out of the
/// terminal (the pure classification behind [`read_reply_bytes`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReplyReadStep {
    /// The front `count` bytes are complete recognized reply tokens
    /// (terminators included): feed them to the query and forget them.
    Consume(usize),
    /// The front `count` bytes cannot be part of a recognized reply: hand them
    /// back to the input path, in order, and look at the rest again.
    Release(usize),
    /// The bytes are (part of) a possible reply: keep them and wait for the
    /// string's tail — the reader's deadline bounds that wait.
    Hold,
}

/// `ESC` — every recognized reply starts with it (the headers of
/// [`crate::tui::terminal_reply`] and the complete device-attributes answer).
const ESC: u8 = 0x1b;

/// Classify the bytes a raw reader has already taken (see [`ReplyReadStep`]).
///
/// This is the walk the Windows reader applies to *pending* console records
/// ([`crate::tui::terminal_reply::reply_take`]), applied to bytes that are in
/// hand because a tty has no peek to look first: complete reply tokens at the
/// front are consumed, the input in front of the first byte that can still
/// start a reply is released, and the rest is held. Bytes that cannot be part
/// of a reply are therefore never swallowed — they are handed back — while a
/// reply that follows input inside the same read is still recognized.
fn reply_read_step(run: &[u8]) -> ReplyReadStep {
    if run.is_empty() {
        return ReplyReadStep::Hold;
    }
    let tokens = reply_tokens_len(run);
    if tokens > 0 {
        return ReplyReadStep::Consume(tokens);
    }
    if reply_input_state(run) != ReplyInputState::Foreign {
        return ReplyReadStep::Hold;
    }
    // The front cannot be a reply. Release it up to the first offset that can
    // still begin one; only `ESC` can, so the scan skips everything else.
    let mut index = 1;
    while index < run.len() {
        if run[index] == ESC && reply_could_start(&run[index..]) {
            return ReplyReadStep::Release(index);
        }
        index += 1;
    }
    ReplyReadStep::Release(run.len())
}

/// Whether a recognized reply could still start at `run[0]`: the bytes are a
/// complete reply token, an open claimed string, or a prefix that may still
/// grow into one.
fn reply_could_start(run: &[u8]) -> bool {
    reply_tokens_len(run) > 0 || reply_input_state(run) != ReplyInputState::Foreign
}

/// The reply reader's policy over an injected "bytes readable now" source: the
/// production caller passes [`stdin_poll::read_readable`] (`poll(2)` +
/// non-blocking `read(2)` on fd 0) and the tests pass a scripted source, so
/// every byte-level case is exercised without a tty. The session owns the query
/// it feeds and touches nothing else.
///
/// `read_now` is asked for the bytes the input has readable within the wait it
/// is given: `None` means the input ended, an empty vector that nothing was
/// readable within the wait. The session never blocks past `timeout` — plus
/// `hold` while a *recognized* reply string is still open (D19: its tail is
/// read through its terminator so it can neither be typed into the prompt nor
/// open the external editor through its BEL; [`LATE_REPLY_HOLD`] in
/// production). It stops before it would have to drop a byte: at
/// [`MAX_LATE_REPLY_BYTES`] held, and when the bytes taken would exceed `cap`
/// ([`MAX_HANDED_BACK_BYTES`] in production), which is also the size its
/// [`ReplyReadOutcome::handed_back`] stays within (plus one look's chunk).
#[doc(hidden)] // policy seam driven by tests, like [`try_claim_query_gate`]
pub fn read_reply_bytes(
    timeout: std::time::Duration,
    hold: std::time::Duration,
    cap: usize,
    read_now: &mut dyn FnMut(std::time::Duration) -> Option<Vec<u8>>,
) -> ReplyReadOutcome {
    use std::time::Instant;

    let mut query = TerminalColorQuery::new();
    // The possible reply read so far: a complete token is consumed at once, an
    // open string is held for its tail, a header prefix waits for the rest.
    let mut held: Vec<u8> = Vec::new();
    let mut handed_back: Vec<u8> = Vec::new();
    let answer_deadline = Instant::now() + timeout;
    // A *claimed* reply string that has not terminated yet keeps the session
    // reading (D19); this bounds that hold.
    let hold_deadline = answer_deadline + hold;
    loop {
        // Classify everything taken so far: consume complete reply tokens, hand
        // back the input in front of the next possible reply, keep the rest.
        loop {
            match reply_read_step(&held) {
                ReplyReadStep::Consume(count) => {
                    let (verdict, consumed) = query.feed_replies(&held[..count]);
                    held.drain(..consumed);
                    if let TerminalColorFeed::Delivered(colors) = verdict {
                        // The query is complete. Whatever else was taken is a
                        // reply the query no longer needs or plain input;
                        // neither may be dropped, so both are settled first.
                        handed_back.extend(settle_after_delivery(&mut query, &mut held));
                        return ReplyReadOutcome {
                            colors: Some(colors),
                            handed_back,
                        };
                    }
                    if consumed == 0 {
                        break;
                    }
                }
                ReplyReadStep::Release(count) => handed_back.extend(held.drain(..count)),
                ReplyReadStep::Hold => break,
            }
        }
        let open = matches!(reply_input_state(&held), ReplyInputState::Open);
        let deadline = if open { hold_deadline } else { answer_deadline };
        let now = Instant::now();
        // Every bound of the session, checked *before* taking more bytes: the
        // window (plus the hold while a claimed string is open), the reply cap
        // and the hand-back cap. Hitting one of the latter two stops the reader
        // instead of dropping bytes — what it does not take stays in the
        // terminal's input queue.
        if now >= deadline
            || held.len() >= MAX_LATE_REPLY_BYTES
            || handed_back.len() + held.len() + READ_CHUNK_BYTES > cap
        {
            break;
        }
        let wait = (deadline - now).min(REPLY_POLL_INTERVAL);
        match read_now(wait) {
            None => break,
            Some(bytes) => held.extend_from_slice(&bytes),
        }
    }
    // Nothing still held is a reply: an unterminated *claimed* string has run
    // out its hold (D19's bound) and everything else is input. Hand it back in
    // arrival order and report what the terminal did say.
    handed_back.extend_from_slice(&held);
    let colors = query.timeout();
    ReplyReadOutcome {
        colors: (!colors.is_empty()).then_some(colors),
        handed_back,
    }
}

/// The query delivered: everything still in `held` is either a reply the query
/// no longer accepts (its parsers ignore post-completion replies) or plain
/// input — neither may be dropped, so the input is returned and the buffer
/// emptied.
fn settle_after_delivery(query: &mut TerminalColorQuery, held: &mut Vec<u8>) -> Vec<u8> {
    let mut input = Vec::new();
    loop {
        match reply_read_step(held) {
            ReplyReadStep::Consume(count) => {
                let (_, consumed) = query.feed_replies(&held[..count]);
                held.drain(..consumed);
                if consumed == 0 {
                    break;
                }
            }
            ReplyReadStep::Release(count) => input.extend(held.drain(..count)),
            ReplyReadStep::Hold => break,
        }
    }
    input.extend_from_slice(held);
    held.clear();
    input
}

/// The Unix reader (see [`query_terminal_colors`]): [`read_reply_bytes`] wired
/// to `poll(2)`/`read(2)` on fd 0. It runs on the calling thread and returns at
/// the query window (plus the hold while a claimed reply string is open), so it
/// can never outlive the query; bytes it had to take that are not a reply are
/// handed back through [`PENDING_STDIN_BYTES`].
#[cfg(not(windows))]
fn unix_reply_reader(timeout: std::time::Duration) -> Option<TerminalColors> {
    // A byte that arrives between the poll and the read must not park this
    // thread, so fd 0 is non-blocking for the session; the guard restores the
    // previous flags before crossterm owns stdin again. A descriptor whose
    // flags cannot be read or set gets no query.
    let nonblocking = stdin_poll::NonBlockingStdin::acquire()?;
    let mut read_now = stdin_poll::read_readable;
    let outcome = read_reply_bytes(
        timeout,
        LATE_REPLY_HOLD,
        MAX_HANDED_BACK_BYTES,
        &mut read_now,
    );
    drop(nonblocking);
    hand_back_stdin_bytes(&outcome.handed_back);
    outcome.colors
}

/// The one raw Unix FFI this module makes: `poll(2)` on fd 0 decides whether
/// there is anything to read, and only a readable fd is read (`read(2)`,
/// non-blocking), so the reader can never park past its deadline. The crate
/// denies `unsafe_code`; this module is osc11's documented exception,
/// mirroring `crate::tui::stdin_buffer`'s `console_peek` module, with a SAFETY
/// note on every block.
///
/// The libc (0.2) items relied on, as declared in the crate and already used by
/// `crate::env_daemon::sys::unix`'s pipe reader:
/// - `poll(fds: *mut pollfd, nfds: nfds_t, timeout: c_int) -> c_int` and
///   `pollfd { fd: c_int, events: c_short, revents: c_short }`, with
///   `POLLIN: c_short` and `POLLHUP`/`POLLERR: c_short`;
/// - `fcntl(fd: c_int, cmd: c_int, ...) -> c_int` (variadic, so `F_GETFL`
///   without and `F_SETFL` with the third `c_int` argument), and the
///   `F_GETFL`/`F_SETFL`/`O_NONBLOCK` constants;
/// - `read(fd: c_int, buf: *mut c_void, count: size_t) -> ssize_t`;
/// - `STDIN_FILENO: c_int = 0`.
#[cfg(not(windows))]
mod stdin_poll {
    #![allow(unsafe_code)]

    use std::time::Duration;

    /// fd 0 with `O_NONBLOCK` set, restoring the previous file-status flags on
    /// drop.
    pub(super) struct NonBlockingStdin {
        previous: libc::c_int,
    }

    impl NonBlockingStdin {
        /// Put fd 0 into non-blocking mode and remember the flags to restore.
        /// `None` when they cannot be read or set: the query is then skipped
        /// rather than run on a descriptor that could park the reader.
        pub(super) fn acquire() -> Option<Self> {
            // SAFETY: `fcntl` on the process' own stdin descriptor; `F_GETFL`
            // takes no argument and returns the current file-status flags (or
            // -1).
            let previous = unsafe { libc::fcntl(libc::STDIN_FILENO, libc::F_GETFL) };
            if previous < 0 {
                return None;
            }
            if previous & libc::O_NONBLOCK == 0 {
                // SAFETY: as above; `F_SETFL` takes the new flags as its third,
                // `c_int` argument (`libc::fcntl` is the variadic C function).
                let set = unsafe {
                    libc::fcntl(
                        libc::STDIN_FILENO,
                        libc::F_SETFL,
                        previous | libc::O_NONBLOCK,
                    )
                };
                if set < 0 {
                    return None;
                }
            }
            Some(Self { previous })
        }
    }

    impl Drop for NonBlockingStdin {
        fn drop(&mut self) {
            // SAFETY: `fcntl` on the process' own stdin descriptor with the
            // flags `acquire` read from it; fd 0 stays open for as long as this
            // guard lives (it is created and dropped inside one query, before
            // any other reader of stdin exists).
            unsafe {
                libc::fcntl(libc::STDIN_FILENO, libc::F_SETFL, self.previous);
            }
        }
    }

    /// The bytes fd 0 has readable within `wait`: `None` when the input ended
    /// (EOF or a hangup), an empty vector when nothing became readable.
    ///
    /// `poll(2)` first and `read(2)` only once it reports `POLLIN`, so a silent
    /// terminal is never read and the bytes a user types into a terminal that
    /// does not answer the query stay in its input queue. The read is
    /// non-blocking (the caller holds [`NonBlockingStdin`]), so even a byte
    /// taken between the poll and the read cannot park the caller.
    pub(super) fn read_readable(wait: Duration) -> Option<Vec<u8>> {
        let mut descriptor = libc::pollfd {
            fd: libc::STDIN_FILENO,
            events: libc::POLLIN,
            revents: 0,
        };
        let millis = wait.as_millis().min(libc::c_int::MAX as u128) as libc::c_int;
        // SAFETY: `descriptor` is one valid, initialized `pollfd` for the
        // descriptor the caller owns; `poll` only reads `fd`/`events` and
        // writes `revents`.
        let ready = unsafe { libc::poll(&mut descriptor, 1, millis) };
        if ready < 0 {
            // A signal interrupted the wait (EINTR): nothing was taken, the
            // caller re-polls with the time left. Any other error ends the
            // session.
            let error = std::io::Error::last_os_error();
            return if error.kind() == std::io::ErrorKind::Interrupted {
                Some(Vec::new())
            } else {
                None
            };
        }
        if ready == 0 {
            // The wait elapsed without input.
            return Some(Vec::new());
        }
        if descriptor.revents & libc::POLLIN == 0 {
            // An error/hangup event without input: the input is gone.
            return None;
        }
        let mut buffer = [0u8; super::READ_CHUNK_BYTES];
        // SAFETY: `buffer` is a writable array of `READ_CHUNK_BYTES` bytes and
        // its length is passed as the count; `read` writes at most that many
        // bytes into it and returns how many (0 at EOF, -1 on error).
        let read =
            unsafe { libc::read(libc::STDIN_FILENO, buffer.as_mut_ptr().cast(), buffer.len()) };
        if read > 0 {
            return Some(buffer[..read as usize].to_vec());
        }
        if read == 0 {
            return None; // EOF
        }
        // -1: `EAGAIN` means the byte was taken between the poll and the read
        // (nothing was taken here); anything else ends the session.
        let error = std::io::Error::last_os_error();
        if matches!(
            error.kind(),
            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
        ) {
            Some(Vec::new())
        } else {
            None
        }
    }
}

/// Process-wide "a terminal color query has actually been initiated" gate.
/// Claimed only after the TTY and `PIR_DISABLE_TERMINAL_QUERY` checks pass,
/// at the moment the query is about to start — so a first call in a non-TTY
/// environment (tests, pipes) does not burn the one-query budget for the
/// whole process.
static OSC11_QUERY_GATE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Claim a query gate: `true` exactly once, first claimant wins. A
/// `compare_exchange` (not a load-then-store) so two threads racing through
/// the checks cannot both pass — exactly one initiates the query, matching
/// the previous `OnceLock::set` guarantee.
#[doc(hidden)]
pub fn try_claim_query_gate(gate: &std::sync::atomic::AtomicBool) -> bool {
    use std::sync::atomic::Ordering;
    gate.compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_ok()
}

/// Whether the process-wide OSC 11 query gate has been claimed, i.e. a real
/// query has been (or is being) initiated. Observability/test hook.
#[doc(hidden)]
pub fn osc11_query_gate_claimed() -> bool {
    OSC11_QUERY_GATE.load(std::sync::atomic::Ordering::SeqCst)
}

/// Whether the terminal's mode-2031 light/dark notifications were enabled in
/// this process (upstream `terminalColorSchemeNotificationsEnabled`,
/// tui.ts:1391-1409).
static COLOR_SCHEME_NOTIFY: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// Whether the theme follows the terminal's appearance (a `light/dark` pair
/// or the system theme) and the mode-2031 notifications are on.
pub fn terminal_color_scheme_notifications_enabled() -> bool {
    COLOR_SCHEME_NOTIFY.load(std::sync::atomic::Ordering::SeqCst)
}

/// Turn the terminal's mode-2031 light/dark notifications on or off
/// (upstream `setTerminalColorSchemeNotifications`, tui.ts:961-970: writes
/// `\x1b[?2031h` / `\x1b[?2031l`). Records the state in every case; writes
/// only when `PIR_TERMINAL_COLOR_SCHEME_NOTIFY=1` asks for it.
///
/// Deviation (same class as the OSC 7501 one in
/// [`crate::tui::core::Terminal::handle_program_status_reply`]): upstream
/// consumes the `\x1b[?997;1n` / `\x1b[?997;2n` report in its own stdin
/// buffer (`consumeTerminalColorSchemeReport`, tui.ts:1179-1189) and strips
/// it before input handling. crossterm owns stdin in pir and its parser does
/// not know `CSI ? 997 ; n`, so a report would stay buffered (swallowing the
/// keys typed after it) with nothing able to consume it. Enabling the
/// notifications is therefore opt-in; without it the appearance still
/// resolves from the reported background, `COLORFGBG` and a fresh query on
/// the next launch.
pub fn set_terminal_color_scheme_notifications(enabled: bool) {
    use std::io::Write as _;
    use std::sync::atomic::Ordering;

    let previous = COLOR_SCHEME_NOTIFY.swap(enabled, Ordering::SeqCst);
    if previous == enabled {
        return;
    }
    if std::env::var_os("PIR_TERMINAL_COLOR_SCHEME_NOTIFY").is_none() {
        tracing::debug!(
            enabled,
            "mode-2031 notifications recorded but not written (PIR_TERMINAL_COLOR_SCHEME_NOTIFY not set)"
        );
        return;
    }
    let sequence = if enabled {
        TERMINAL_COLOR_SCHEME_NOTIFY_ON
    } else {
        TERMINAL_COLOR_SCHEME_NOTIFY_OFF
    };
    let mut out = std::io::stdout();
    let _ = out.write_all(sequence.as_bytes());
    let _ = out.flush();
}
