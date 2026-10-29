//! Terminal reply strings on the input path — the Rust port of upstream
//! `pi-tui/src/stdin-buffer.ts`'s terminal-reply handling (commit
//! `edb2eb966`, fixes #10256/#10657).
//!
//! Upstream's `StdinBuffer` learned to keep a *claimed* terminal reply
//! together until its terminator, so a color/palette/XTVersion/Kitty reply
//! that arrives fragmented cannot leak into the keyboard input: the hex tail
//! of `\x1b]4;1;rgb:eaea/6c6c/6d6d\x07` used to be typed into the prompt and
//! its BEL terminator opened the external editor (Ctrl+G).
//!
//! pir has no `StdinBuffer`: crossterm owns the stdin parser, so the only
//! input pir ever reads raw is the OSC 11 background query's reply
//! ([`super::theme::osc11`]). Besides [`scan_reply`], this module exports the
//! rule that reader judges *pending* input with: [`reply_input_state`] says
//! whether a byte run is a claimed reply or could still become one,
//! [`reply_take`] says how much of a non-consuming look at the pending input
//! may be taken, [`reply_tokens_len`] finds the reply strings that are
//! already complete and [`reply_token_len`] the first of them — the boundary
//! the color query's batch feeding splits a coalesced peek batch at. A claimed reply string is consumed through its
//! terminator, so a reply that arrives fragmented cannot leak its hex tail
//! into the prompt, nor its BEL terminator open the external editor as Ctrl+G
//! (D19 = upstream `edb2eb966`); bytes that cannot be part of a reply are
//! never claimed, so they stay available to the normal input path.
//!
//! The same grammar guards the *live* input path: `StdinGate`
//! (`super::stdin_buffer`) applies [`scan_reply`] to the run of parsed key
//! events it has staged (`StdinGate::settle_reply_run`) and consumes a
//! recognized reply whenever it arrives — the boot reader's window no longer
//! decides who sees an answer, so a reply that lands after it (a slow
//! terminal, a second DA1) is claimed by the grammar rather than typed. Only a
//! run that still carries its own `ESC` can be claimed: the Windows console
//! queues a reply as one record per byte and crossterm drops the `VK 0` record
//! that carries the ESC, leaving a tail (`[?61;…c`) that is byte-for-byte
//! ordinary input — claiming that needs the console owned at the *record*
//! level, not a rule here (see the boundary note in `super::stdin_buffer`).
//!
//! One reply is not a control string: the device-attributes answer
//! (`ESC [ ? … c`, [`device_attributes_reply_len`]) that terminates the
//! startup color query. It is a complete CSI token at its final `c` — the
//! reader must consume it like the OSC replies, because on Windows the
//! console queues its bytes as records and crossterm would drop only the
//! leading ESC record, typing `[?61;6;…c` into the prompt.
//!
//! Deviation (documented in `theme/osc11.rs`): pir cannot hand bytes that
//! were already read back to crossterm's event stream, so upstream's
//! flush-time split — emit the two-byte `ESC ]`/`ESC P`/`ESC _` Alt-key
//! prefix, then reparse the tail as keyboard input — has no equivalent here;
//! whatever a reader claimed it discards instead.

/// `ESC`.
const ESC: u8 = 0x1b;
/// `CAN` — cancels a control string; the bytes after it are not part of the
/// string.
const CAN: u8 = 0x18;
/// `SUB` — the second control string cancel byte.
const SUB: u8 = 0x1a;
/// `BEL` — the OSC terminator.
const BEL: u8 = 0x07;

/// Whether `data` opens a control string: OSC (`ESC ]`), DCS (`ESC P`) or
/// APC (`ESC _`) (upstream `isControlString`).
pub fn is_control_string(data: &[u8]) -> bool {
    data.len() >= 2 && data[0] == ESC && matches!(data[1], b']' | b'P' | b'_')
}

/// Length of the complete device-attributes reply at the front of `data`:
/// `ESC [ ( ? | > ) [0-9;]* c` — the primary (DA1, `?`) and secondary (DA2,
/// `>`) forms (upstream `DEVICE_ATTRIBUTES_RESPONSE_PATTERN`, tui.ts:173,
/// matches the primary one; the secondary marker is included because the
/// reader consumes *every* reply string it takes, and a DA2 answer can only
/// arrive if something asked for one).
///
/// Unlike the OSC/DCS/APC replies this is one complete CSI token: the final
/// `c` is its own terminator, so there is no payload to scan for BEL/ST. The
/// length (terminator included) is returned, `None` when `data` does not
/// start with such an answer.
///
/// The device-attributes reply is the one reply pir *always* elicits:
/// `TERMINAL_COLOR_QUERY` (`theme/terminal_colors.rs`) ends with a DA1
/// request whose answer marks the end of the color replies. On Windows the
/// console queues that answer as input records; if the peek reader does not
/// claim them, crossterm drops only the leading ESC record (VK 0) and the
/// tail (`[?61;6;7;21;22;23;24;28;32;42c`) is typed into the prompt.
pub fn device_attributes_reply_len(data: &[u8]) -> Option<usize> {
    let rest = data.strip_prefix(b"\x1b[".as_slice())?;
    let marker = *rest.first()?;
    if marker != b'?' && marker != b'>' {
        return None;
    }
    let body = &rest[1..];
    let params = body
        .iter()
        .take_while(|byte| byte.is_ascii_digit() || **byte == b';')
        .count();
    // The first byte that is neither a digit nor `;` must be the `c`
    // terminator; anything else (a different CSI final, a payload) is not an
    // answer.
    (body.get(params) == Some(&b'c')).then_some(2 + 1 + params + 1)
}

/// Whether the buffer starts with a *complete* recognized terminal reply
/// header (upstream `TERMINAL_REPLY_HEADER_PATTERN`):
///
/// ```text
/// ^ESC (?: ] (?:1[01]; | 4;\d{1,3}; | 7501;?) | P>| | _G (?:[a-z]=\d+,)* [a-z]=\d+; )
/// ```
///
/// The headers are foreground/background color replies (`OSC 10`/`OSC 11`),
/// palette color replies (`OSC 4`, one-based index of 1..3 digits), program
/// status (`OSC 7501;?`), XTVersion (`DCS >|`) and Kitty graphics replies
/// (`APC _G` with `key=number` pairs).
///
/// A *partial* header does not match (upstream's regex is anchored and needs
/// the whole header); such input keeps the bounded recovery.
///
/// This predicate covers the delimited control-string replies. The
/// device-attributes answer is a complete CSI token instead and is handled by
/// [`device_attributes_reply_len`] inside [`scan_reply`].
pub fn has_terminal_reply_header(data: &[u8]) -> bool {
    if !is_control_string(data) {
        return false;
    }
    match (data.get(1), data.get(2)) {
        // `ESC ] 10;` / `ESC ] 11;`
        (Some(b']'), Some(b'1')) => data.starts_with(b"\x1b]10;") || data.starts_with(b"\x1b]11;"),
        // `ESC ] 4;<1-3 digits>;`
        (Some(b']'), Some(b'4')) => match data.strip_prefix(b"\x1b]4;".as_slice()) {
            Some(rest) => {
                let digits = rest.iter().take_while(|byte| byte.is_ascii_digit()).count();
                (1..=3).contains(&digits) && rest.get(digits) == Some(&b';')
            }
            None => false,
        },
        // `ESC ] 7501;?`
        (Some(b']'), Some(b'7')) => data.starts_with(b"\x1b]7501;?"),
        // XTVersion: `ESC P >|`
        (Some(b'P'), Some(b'>')) => data.starts_with(b"\x1bP>|"),
        // Kitty graphics: `ESC _G` then `key=number` pairs
        (Some(b'_'), Some(b'G')) => is_kitty_reply_header(data),
        _ => false,
    }
}

/// `ESC _G` followed by `key=number` pairs separated by `,` and closed by
/// `;` (upstream `_G(?:[a-z]=\d+,)*[a-z]=\d+;`). The final `;` must be
/// present for the header to count as complete.
fn is_kitty_reply_header(data: &[u8]) -> bool {
    let Some(rest) = data.strip_prefix(b"\x1b_G".as_slice()) else {
        return false;
    };
    let mut rest = rest;
    loop {
        let key_len = rest
            .iter()
            .take_while(|byte| byte.is_ascii_lowercase())
            .count();
        let after_key = &rest[key_len.min(rest.len())..];
        if after_key.first() != Some(&b'=') {
            return false;
        }
        let digits = after_key[1..]
            .iter()
            .take_while(|byte| byte.is_ascii_digit())
            .count();
        if digits == 0 {
            return false;
        }
        match after_key.get(1 + digits) {
            // The pair closed the header: `key=number;`.
            Some(b';') => return true,
            // Another pair follows: `key=number,`.
            Some(b',') => rest = &after_key[1 + digits + 1..],
            _ => return false,
        }
    }
}

/// How far a claimed terminal reply string has progressed
/// (upstream `extractCompleteSequences`' `terminalReply` branch: a sequence
/// is complete at its terminator, `CAN`/`SUB` cancel it, and a new escape
/// sequence aborts it).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplyScan {
    /// The bytes do not carry a recognized reply header: not a delimited
    /// reply string (upstream falls back to the timeout/bounded-recovery
    /// path).
    NotAReply,
    /// A recognized reply header whose string has not terminated yet: keep
    /// the bytes together and wait for the terminator.
    Open,
    /// The string ended after `len` bytes; the terminator is included in
    /// `len`.
    Complete { len: usize },
    /// `CAN` (0x18) or `SUB` (0x1a) canceled the string: `len` bytes are
    /// consumed up to and including the cancel byte (upstream `pos +=
    /// seqEnd`).
    Cancelled { len: usize },
    /// A new escape sequence aborted the unterminated string: `len` bytes
    /// are consumed *before* the aborting `ESC`, which upstream keeps
    /// buffered for the next parse.
    Aborted { len: usize },
}

/// Scan a claimed terminal reply string starting at `data[0]`.
///
/// The scan starts at the third byte (upstream checks `seqEnd > 2`), so the
/// two-byte `ESC ]`/`ESC P`/`ESC _` introducer itself can never be mistaken
/// for a terminator or a cancel.
///
/// A device-attributes answer is complete at its final `c` and is reported as
/// [`ReplyScan::Complete`] before the control-string scan runs
/// ([`device_attributes_reply_len`]).
pub fn scan_reply(data: &[u8]) -> ReplyScan {
    if let Some(len) = device_attributes_reply_len(data) {
        // The answer terminates itself; there is no payload to wait for.
        return ReplyScan::Complete { len };
    }
    if !has_terminal_reply_header(data) {
        return ReplyScan::NotAReply;
    }
    // OSC strings answer to BEL or ST; DCS/APC strings only to ST (upstream
    // `isCompleteOscSequence` / `isCompleteDcsSequence` /
    // `isCompleteApcSequence`).
    let bel_terminates = data.get(1) == Some(&b']');
    let mut index = 2;
    while index < data.len() {
        let byte = data[index];
        if byte == CAN || byte == SUB {
            return ReplyScan::Cancelled { len: index + 1 };
        }
        if byte == ESC && data.get(index + 1).is_some_and(|next| *next != b'\\') {
            // A new escape sequence aborts an unterminated reply. A trailing
            // `ESC` stays buffered until we can distinguish it from ST.
            return ReplyScan::Aborted { len: index };
        }
        if bel_terminates && byte == BEL {
            return ReplyScan::Complete { len: index + 1 };
        }
        if byte == b'\\' && data[index - 1] == ESC {
            return ReplyScan::Complete { len: index + 1 };
        }
        index += 1;
    }
    ReplyScan::Open
}

// ---------------------------------------------------------------------------
// Reader verdicts (the OSC query's raw reader)
// ---------------------------------------------------------------------------

/// Whether `data` is a prefix of a complete recognized reply header (see
/// [`has_terminal_reply_header`]): `ESC`, `ESC ]`, `ESC ] 1`, `ESC ] 4;1`,
/// `ESC P >`, `ESC _ G a=`, `ESC [`, `ESC [ ?`, `ESC [ > 6`. The empty buffer
/// is a prefix too (a reply may start at any time).
///
/// A header fragment cannot be told apart from typed input (`ESC` is also the
/// Escape key, `ESC ]` also `alt+]`), so it only keeps the reader *waiting*:
/// once it completes, the string is a claimed reply and is consumed through
/// its terminator ([`reply_input_state`]). For the device-attributes answer
/// the header *is* the complete token, so a fragment keeps the reader
/// waiting until its `c` arrives ([`device_attributes_reply_len`]).
pub fn is_reply_header_prefix(data: &[u8]) -> bool {
    // Device-attributes answer `ESC [ ? …` / `ESC [ > …`: every fragment
    // before the closing `c` can still grow into the answer. The bare CSI
    // introducer keeps the reader waiting (the marker may follow), but a byte
    // in the marker's position that is neither `?` nor `>` — and any byte
    // after the marker that is not a digit or `;` — is foreign, so ordinary
    // CSI input (`ESC [ A`, `ESC [ 2`, `ESC [ 2 0 0 ~`) is never claimed;
    // the complete answer is a token, not a prefix, and [`reply_tokens_len`]
    // skips it before this check runs.
    if let Some(rest) = data.strip_prefix(b"\x1b[".as_slice()) {
        if rest.is_empty() {
            return true;
        }
        if rest[0] != b'?' && rest[0] != b'>' {
            return false;
        }
        return rest[1..]
            .iter()
            .all(|byte| byte.is_ascii_digit() || *byte == b';');
    }
    // Prefixes of the fixed headers: `ESC ] 7501;?`, `ESC ] 10;`,
    // `ESC ] 11;`, the palette introducer `ESC ] 4;`, XTVersion `ESC P >|`
    // and the kitty APC `ESC _ G`.
    if b"\x1b]7501;?".starts_with(data)
        || b"\x1b]10;".starts_with(data)
        || b"\x1b]11;".starts_with(data)
        || b"\x1b]4;".starts_with(data)
        || b"\x1bP>|".starts_with(data)
        || b"\x1b_G".starts_with(data)
    {
        return true;
    }
    // Palette reply `ESC ] 4;<1-3 digits>;`: after the introducer only up to
    // three digits may follow (the `;` closes a complete header).
    if let Some(rest) = data.strip_prefix(b"\x1b]4;".as_slice()) {
        let digits = rest.iter().take_while(|byte| byte.is_ascii_digit()).count();
        return rest.len() == digits && digits <= 3;
    }
    // Kitty graphics reply `ESC _ G (?:[a-z]=\d+,)*[a-z]=\d+;`.
    if let Some(rest) = data.strip_prefix(b"\x1b_G".as_slice()) {
        return is_kitty_reply_prefix(rest);
    }
    false
}

/// Whether `rest` (the bytes after `ESC _ G`) is a prefix of the Kitty
/// graphics reply header `(?:[a-z]=\d+,)*[a-z]=\d+;`.
fn is_kitty_reply_prefix(rest: &[u8]) -> bool {
    let mut rest = rest;
    loop {
        if rest.is_empty() {
            return true;
        }
        let key = rest
            .iter()
            .take_while(|byte| byte.is_ascii_lowercase())
            .count();
        rest = &rest[key..];
        if rest.is_empty() {
            return true;
        }
        if rest[0] != b'=' {
            return false;
        }
        rest = &rest[1..];
        let digits = rest.iter().take_while(|byte| byte.is_ascii_digit()).count();
        rest = &rest[digits..];
        match rest.first() {
            // Truncated inside the pair: `a=`, `a=1`.
            None => return true,
            // Another pair follows.
            Some(b',') => rest = &rest[1..],
            // The header closed; anything after it is payload, not header.
            Some(b';') => return rest.len() == 1,
            Some(_) => return false,
        }
    }
}

/// Length of the first complete reply token at the front of `data`: the bytes
/// of one terminated reply string (its terminator included), of a
/// `CAN`/`SUB`-cancelled string (the cancel byte included), or of the part of
/// an aborted string that precedes the aborting escape. Zero when `data` does
/// not start with one — an incomplete string ([`ReplyScan::Open`]) or input
/// that cannot be a reply at all ([`ReplyScan::NotAReply`]).
///
/// One reply per token is what the color query's parsers expect: both are
/// anchored at the ends of the string, so a concatenation of two replies is
/// neither. A peeked batch can hold several coalesced replies (the terminal
/// answers the startup query with up to 19 of them) and the query's batch
/// feeding splits at this boundary
/// ([`crate::tui::theme::TerminalColorQuery::feed_replies`]).
pub fn reply_token_len(data: &[u8]) -> usize {
    match scan_reply(data) {
        ReplyScan::Complete { len } | ReplyScan::Cancelled { len } => len,
        ReplyScan::Aborted { len } => len,
        ReplyScan::Open | ReplyScan::NotAReply => 0,
    }
}

/// Length of the complete reply tokens at the front of `data`: every byte that
/// belongs to terminated reply strings (upstream's `Complete`), to
/// `CAN`/`SUB`-cancelled ones, or to the part of an aborted string that
/// precedes the aborting escape. Zero when `data` does not start with a
/// claimed reply string.
///
/// The color query's reader uses this to hand the complete replies to the
/// query (upstream consumes *every* reply string from its stdin buffer,
/// whether or not it is a color reply) and to know where a consumed reply ends
/// and the next input begins.
///
/// This is the walk that sums [`reply_token_len`]; the query's batch feeding
/// consumes those tokens one by one instead of as one run.
pub fn reply_tokens_len(data: &[u8]) -> usize {
    let mut length = 0;
    let mut rest = data;
    loop {
        let len = reply_token_len(rest);
        if len == 0 {
            return length;
        }
        length += len;
        rest = &rest[len..];
    }
}

/// What the bytes a raw reader consumed so far are, from the reply grammar's
/// point of view. Determined after skipping the complete reply tokens
/// ([`reply_tokens_len`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplyInputState {
    /// The bytes end inside a claimed reply string (its header is complete,
    /// its terminator is not): the reader keeps consuming — the tail belongs
    /// to the reply and would otherwise be typed into the prompt / open the
    /// editor through its BEL (D19 = upstream `edb2eb966`).
    Open,
    /// Nothing is claimed: no bytes, only complete reply tokens, or a header
    /// fragment ([`is_reply_header_prefix`]) that may still grow into a reply.
    /// The reader keeps looking but takes nothing.
    Waiting,
    /// The bytes cannot become a reply. The reader stops; anything still
    /// queued stays available to the normal input path.
    Foreign,
}

/// Classify the bytes a raw reader consumed (see [`ReplyInputState`]).
pub fn reply_input_state(data: &[u8]) -> ReplyInputState {
    let rest = &data[reply_tokens_len(data)..];
    if rest.is_empty() {
        return ReplyInputState::Waiting;
    }
    if scan_reply(rest) == ReplyScan::Open {
        return ReplyInputState::Open;
    }
    if is_reply_header_prefix(rest) {
        ReplyInputState::Waiting
    } else {
        ReplyInputState::Foreign
    }
}

/// How much of the *pending* input a non-consuming reader may take: it looks
/// at the records the terminal/console already queued and takes only what is
/// already a reply, leaving everything else for the normal input path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplyTake {
    /// Take this many leading input units: they are a claimed reply (its tail
    /// included). Units are whole input units (console records / bytes), so
    /// the caller never splits one.
    Units(usize),
    /// Take nothing and keep waiting: the front may still become a reply (only
    /// the header of one has arrived so far).
    Wait,
    /// Take nothing: the front cannot be part of a reply, so it stays where it
    /// is.
    Stop,
}

/// Decide how much of the pending input units at the front of `units` a
/// peeking reader may take, continuing the bytes it already consumed
/// (`consumed`).
///
/// A unit is taken only once the bytes it completes are *claimed* — a complete
/// reply header was seen ([`ReplyInputState::Open`]) or a reply string ended
/// ([`reply_tokens_len`] grew) — so typed input is never taken, not even the
/// leading `ESC` of a possible reply: while only a fragment has arrived the
/// units stay queued and are looked at again as more arrive. A unit that makes
/// the run [`ReplyInputState::Foreign`] ends the walk *and* is not taken.
///
/// The claim never reaches past the byte the reply grammar claims, and only
/// units lying *wholly* inside it are counted:
///
/// - While the run is [`ReplyInputState::Open`], a final `ESC` is ambiguous —
///   it may be the first half of the reply's `ST` terminator or the user's own
///   escape key — so the unit it ends is deferred: only the units strictly
///   before it are taken (a call with no such unit takes nothing and stays
///   [`ReplyTake::Wait`]). The next unit settles it: `\` completes the
///   string and both bytes are taken (D19), anything else aborts it and the
///   `ESC` stays queued.
/// - When a reply string ends, the claim reaches exactly to the end of the
///   complete token. A `CAN`/`SUB`-cancelled string includes its cancel byte,
///   but an [`ReplyScan::Aborted`] token ends *before* its aborting `ESC`
///   (upstream keeps that `ESC` buffered for the next parse), so a unit
///   reaching past that byte is not taken either.
///
/// `Wait` and `Stop` describe the front when nothing was taken: `Stop` means
/// the bytes already visible cannot become a reply, `Wait` means they can
/// (the caller keeps looking until its window closes).
pub fn reply_take<'a>(consumed: &[u8], units: impl IntoIterator<Item = &'a [u8]>) -> ReplyTake {
    let mut run: Vec<u8> = consumed.to_vec();
    // Where every unit looked at so far ends in `run`, so a partial claim can
    // be counted in whole units: units are never split.
    let mut ends: Vec<usize> = Vec::new();
    let mut taken = 0;
    let mut tokens = reply_tokens_len(&run);
    let mut foreign = false;
    for unit in units {
        run.extend_from_slice(unit);
        ends.push(run.len());
        match reply_input_state(&run) {
            ReplyInputState::Foreign => {
                foreign = true;
                break;
            }
            ReplyInputState::Open => {
                // The trailing `ESC` of a claimed-but-unterminated string may
                // be the start of its `ST` terminator or a typed escape key;
                // it cannot be claimed until the next byte decides, so the
                // unit it ends is deferred.
                let boundary = if run.last() == Some(&ESC) {
                    run.len() - 1
                } else {
                    run.len()
                };
                taken = units_wholly_inside(&ends, boundary);
            }
            ReplyInputState::Waiting => {
                let complete = reply_tokens_len(&run);
                if complete > tokens {
                    tokens = complete;
                    // An aborted token ends before its aborting `ESC`, which
                    // is not claimed: only units wholly inside the token walk
                    // may be taken.
                    taken = units_wholly_inside(&ends, complete);
                }
            }
        }
    }
    if taken > 0 {
        ReplyTake::Units(taken)
    } else if foreign {
        ReplyTake::Stop
    } else {
        ReplyTake::Wait
    }
}

/// How many leading units end at or before `boundary`, a byte offset into the
/// run they were appended to: the units a claim covering `boundary` bytes
/// holds whole.
fn units_wholly_inside(ends: &[usize], boundary: usize) -> usize {
    ends.iter().take_while(|end| **end <= boundary).count()
}
