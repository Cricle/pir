//! The OSC 11 background query's reply-reader policy on platforms without a
//! console peek, driven with injected byte sequences.
//!
//! A tty has no peek, so the Unix reader cannot look at fd 0 without reading
//! it: `src/tui/theme/osc11.rs` therefore makes its policy generic over the
//! "bytes readable now" source ([`read_reply_bytes`]) and wires the production
//! path to `poll(2)` + a non-blocking `read(2)` on fd 0. These tests pass
//! scripted sources instead, so the cases that used to need a terminal — a
//! fragmented reply, a typed line before/after the reply, the DA1 answer that
//! ends the query, a reply string that never terminates, a reply that lands
//! after the window — are exercised here.
//!
//! What is *not* covered here is the `poll(2)`/`read(2)`/`fcntl(2)` plumbing
//! itself (`stdin_poll` in `src/tui/theme/osc11.rs`, compiled only for
//! `not(windows)`): it needs a real or pseudoterminal, so it is CI-only (the
//! Linux/macOS legs), and its shape follows `crate::env_daemon::sys::unix`'s
//! existing `poll` on a pipe.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use pir::tui::theme::{read_reply_bytes, ReplyReadOutcome, MAX_HANDED_BACK_BYTES};

/// One scripted look at the terminal's input.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Step<'a> {
    /// The input had these bytes readable.
    Bytes(&'a [u8]),
    /// Nothing was readable within the wait; the source waits it out, like a
    /// silent terminal.
    Idle,
    /// The input ended (EOF or a hangup).
    Eof,
}

/// A scripted source for [`read_reply_bytes`]; the steps it never handed out
/// stay queued, which is how a test shows the session stopped reading.
struct Script<'a> {
    steps: VecDeque<Step<'a>>,
}

impl<'a> Script<'a> {
    fn new(steps: &[Step<'a>]) -> Self {
        Self {
            steps: steps.iter().copied().collect(),
        }
    }

    fn look(&mut self, wait: Duration) -> Option<Vec<u8>> {
        match self.steps.pop_front() {
            Some(Step::Bytes(bytes)) => Some(bytes.to_vec()),
            Some(Step::Eof) => None,
            // The script ran out: stay silent like a terminal that does not
            // answer, so the session ends at its own deadline.
            Some(Step::Idle) | None => {
                std::thread::sleep(wait);
                Some(Vec::new())
            }
        }
    }

    fn unread(&self) -> Vec<Step<'a>> {
        self.steps.iter().copied().collect()
    }
}

/// Run the reader against a script; returns the outcome, the steps never asked
/// for, and how long the session took.
fn drive<'a>(
    steps: &[Step<'a>],
    timeout: Duration,
    hold: Duration,
) -> (ReplyReadOutcome, Vec<Step<'a>>, Duration) {
    drive_with_cap(steps, timeout, hold, MAX_HANDED_BACK_BYTES)
}

fn drive_with_cap<'a>(
    steps: &[Step<'a>],
    timeout: Duration,
    hold: Duration,
    cap: usize,
) -> (ReplyReadOutcome, Vec<Step<'a>>, Duration) {
    let mut script = Script::new(steps);
    let started = Instant::now();
    let outcome = read_reply_bytes(timeout, hold, cap, &mut |wait| script.look(wait));
    let elapsed = started.elapsed();
    let unread = script.unread();
    (outcome, unread, elapsed)
}

/// `ESC ] 11 ; rgb:1a2b/3c4d/5e6f BEL` — an OSC 11 background reply, the one
/// target the theme is decided from.
const REPLY: &[u8] = b"\x1b]11;rgb:1a2b/3c4d/5e6f\x07";
/// The primary device-attributes answer `TERMINAL_COLOR_QUERY` ends with; it
/// marks the end of the color replies.
const DA1: &[u8] = b"\x1b[?61;6;7;21;22;23;24;28;32;42c";
/// What [`REPLY`]'s channels decode to: each hex channel is scaled from its own
/// width to 8 bits (`0x1a2b` -> 26, `0x3c4d` -> 60, `0x5e6f` -> 94).
const BACKGROUND: (u8, u8, u8) = (26, 60, 94);

fn background(outcome: &ReplyReadOutcome) -> Option<(u8, u8, u8)> {
    outcome.colors.as_ref().and_then(|colors| colors.background)
}

#[test]
fn a_fragmented_reply_is_read_through_its_terminator() {
    // The terminal answers in three pieces (and only the DA1 ends the query).
    let (outcome, unread, _) = drive(
        &[
            Step::Bytes(&REPLY[..4]),
            Step::Bytes(&REPLY[4..11]),
            Step::Bytes(&REPLY[11..]),
            Step::Bytes(DA1),
        ],
        Duration::from_millis(50),
        Duration::from_millis(50),
    );
    assert_eq!(background(&outcome), Some(BACKGROUND));
    assert_eq!(outcome.handed_back, Vec::<u8>::new());
    assert!(unread.is_empty(), "every step was read: {unread:?}");
}

#[test]
fn a_typed_line_before_the_reply_is_handed_back_in_order() {
    let (outcome, _, _) = drive(
        &[Step::Bytes(b"hello"), Step::Bytes(REPLY), Step::Bytes(DA1)],
        Duration::from_millis(50),
        Duration::from_millis(50),
    );
    // The reply still lands, and the typed line is not swallowed: it comes
    // back byte for byte, before anything read after it.
    assert_eq!(background(&outcome), Some(BACKGROUND));
    assert_eq!(outcome.handed_back, b"hello");
}

#[test]
fn input_around_the_reply_in_one_read_is_handed_back_in_arrival_order() {
    // A peek-free reader takes a whole chunk at once: typed input, the reply,
    // the DA1 answer and more typed input can share one read.
    let mut chunk = Vec::new();
    chunk.extend_from_slice(b"hi");
    chunk.extend_from_slice(REPLY);
    chunk.extend_from_slice(DA1);
    chunk.extend_from_slice(b"bye");
    let (outcome, _, _) = drive(
        &[Step::Bytes(&chunk)],
        Duration::from_millis(50),
        Duration::from_millis(50),
    );
    assert_eq!(background(&outcome), Some(BACKGROUND));
    assert_eq!(outcome.handed_back, b"hibye");
}

#[test]
fn the_da1_answer_is_consumed_and_ends_the_query() {
    // The DA1 answer alone completes the query (with no color reported), and
    // the session returns at it: the typed bytes behind it were never read.
    let (outcome, unread, _) = drive(
        &[Step::Bytes(DA1), Step::Bytes(b"typed")],
        Duration::from_millis(50),
        Duration::from_millis(50),
    );
    let colors = outcome.colors.expect("the DA1 answer completes the query");
    assert!(colors.is_empty(), "the DA1 answer reports no color");
    assert_eq!(outcome.handed_back, Vec::<u8>::new());
    assert_eq!(unread, vec![Step::Bytes(b"typed")]);
}

#[test]
fn a_typed_escape_sequence_is_handed_back_and_not_claimed() {
    // Shift+Tab (`ESC [ Z`) during the window: an escape that cannot begin a
    // reply. It is handed back, not swallowed as a reply fragment.
    let (outcome, _, _) = drive(
        &[Step::Bytes(b"\x1b[Z"), Step::Bytes(REPLY), Step::Bytes(DA1)],
        Duration::from_millis(50),
        Duration::from_millis(50),
    );
    assert_eq!(background(&outcome), Some(BACKGROUND));
    assert_eq!(outcome.handed_back, b"\x1b[Z");
}

#[test]
fn an_unterminated_reply_string_is_released_when_its_hold_expires() {
    // D19: a *claimed* reply string is held for its terminator, not typed —
    // but only for the hold window (the query window plus `LATE_REPLY_HOLD`
    // here). When the terminator never comes, the bytes are released back to
    // the input path rather than dropped.
    let partial = b"\x1b]11;rgb:1a2b/3c4d";
    let timeout = Duration::from_millis(5);
    let hold = Duration::from_millis(20);
    let (outcome, unread, elapsed) = drive(&[Step::Bytes(partial)], timeout, hold);
    assert!(outcome.colors.is_none(), "nothing was fed as a reply");
    assert_eq!(outcome.handed_back, partial.to_vec());
    assert!(
        elapsed >= hold,
        "the held string waited out its hold ({elapsed:?}), not the shorter query window"
    );
    assert!(unread.is_empty());
}

#[test]
fn a_reply_that_lands_after_the_window_is_never_read() {
    // A silent terminal: the session ends at its window without taking a byte,
    // so the reply that arrives afterwards stays in the terminal's input queue
    // (the input gate consumes it there; this query no longer applies it).
    let timeout = Duration::from_millis(5);
    let hold = Duration::from_millis(20);
    let mut steps = vec![Step::Idle; 100];
    steps.push(Step::Bytes(REPLY));
    let (outcome, unread, elapsed) = drive(&steps, timeout, hold);
    assert!(outcome.colors.is_none());
    assert_eq!(outcome.handed_back, Vec::<u8>::new());
    assert!(
        elapsed >= timeout,
        "the session waits out the query window ({elapsed:?})"
    );
    assert!(
        unread.contains(&Step::Bytes(REPLY)),
        "the late reply was never read: {unread:?}"
    );
}

#[test]
fn the_reader_stops_taking_bytes_once_the_hand_back_budget_is_spent() {
    // The cap reserves one read chunk (256 bytes) for the next look, so with a
    // 264-byte budget the session takes one chunk more and then stops: what it
    // did not take is left in the terminal's input queue, and what it took is
    // handed back whole.
    let (outcome, unread, _) = drive_with_cap(
        &[
            Step::Bytes(b"typing"),
            Step::Bytes(b"more typing"),
            Step::Bytes(b"never read"),
        ],
        Duration::from_millis(20),
        Duration::from_millis(20),
        256 + 8,
    );
    assert_eq!(outcome.handed_back, b"typingmore typing");
    assert_eq!(unread, vec![Step::Bytes(b"never read")]);
}

#[test]
fn an_eof_releases_the_held_bytes_and_ends_the_session() {
    let partial = b"\x1b]11;rgb:1a2b";
    let (outcome, unread, _) = drive(
        &[Step::Bytes(partial), Step::Eof],
        Duration::from_millis(50),
        Duration::from_millis(50),
    );
    assert!(outcome.colors.is_none());
    assert_eq!(outcome.handed_back, partial.to_vec());
    assert!(unread.is_empty());
}
