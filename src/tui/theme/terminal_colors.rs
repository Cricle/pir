//! Terminal color reporting — the Rust port of upstream
//! `packages/tui/src/terminal-colors.ts` (OSC 10 foreground / OSC 11
//! background / OSC 4 palette replies and the mode-2031 light/dark report)
//! plus the reply bookkeeping upstream keeps in `tui.ts`'s
//! `PendingTerminalColorQuery` (`tui.ts:145-175`, `:1133-1189`, `:1494-1515`).
//!
//! Upstream writes one query string — OSC 10, OSC 11, OSC 4 for palette
//! indices 0-15, then a primary device attributes (DA1) request that every
//! terminal answers and that marks the end of the color replies — and
//! consumes the answers from its stdin buffer before they reach the input
//! handlers (`tui.ts:1055-1061`). The palette is only reported when all 16
//! indices arrived; colors the terminal did not report stay `None`.
//!
//! Parsing and the reply bookkeeping are pure so tests can drive them
//! without a TTY ([`TerminalColorQueries`]); [`super::osc11`] owns the
//! impure query at startup.

use std::collections::BTreeSet;

use crate::tui::terminal_reply::reply_token_len;

use super::color::relative_luminance;
use super::palette::Appearance;

/// An sRGB color as reported by the terminal (upstream `RgbColor`).
pub type Rgb = (u8, u8, u8);

/// ANSI palette entries the query asks for (upstream `TERMINAL_PALETTE_SIZE`,
/// tui.ts:160).
pub const TERMINAL_PALETTE_SIZE: usize = 16;

/// OSC 10 and 11 plus OSC 4 for every palette color (upstream
/// `TERMINAL_COLOR_REPLY_COUNT`, tui.ts:162).
pub const TERMINAL_COLOR_REPLY_COUNT: usize = 2 + TERMINAL_PALETTE_SIZE;

/// Default colors, palette colors 0-15, and a trailing DA1 request (upstream
/// `TERMINAL_COLOR_QUERY`, tui.ts:168-172).
pub const TERMINAL_COLOR_QUERY: &str = concat!(
    "\x1b]10;?\x07\x1b]11;?\x07",
    "\x1b]4;0;?\x07\x1b]4;1;?\x07\x1b]4;2;?\x07\x1b]4;3;?\x07",
    "\x1b]4;4;?\x07\x1b]4;5;?\x07\x1b]4;6;?\x07\x1b]4;7;?\x07",
    "\x1b]4;8;?\x07\x1b]4;9;?\x07\x1b]4;10;?\x07\x1b]4;11;?\x07",
    "\x1b]4;12;?\x07\x1b]4;13;?\x07\x1b]4;14;?\x07\x1b]4;15;?\x07",
    "\x1b[c",
);

/// Turn on mode-2031 light/dark notifications (upstream TUI `start`,
/// tui.ts:937-939).
pub const TERMINAL_COLOR_SCHEME_NOTIFY_ON: &str = "\x1b[?2031h";

/// Turn mode-2031 notifications off again (upstream TUI `stop`,
/// tui.ts:984-986).
pub const TERMINAL_COLOR_SCHEME_NOTIFY_OFF: &str = "\x1b[?2031l";

/// The terminal's reported colors (upstream `TerminalColors`,
/// terminal-colors.ts:10-18). Replaced, never mutated, on update.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TerminalColors {
    /// Default foreground (OSC 10).
    pub foreground: Option<Rgb>,
    /// Default background (OSC 11).
    pub background: Option<Rgb>,
    /// ANSI colors 0-15 (OSC 4); only set when the terminal reported all 16.
    pub palette: Option<Vec<Rgb>>,
}

impl TerminalColors {
    /// Whether the terminal reported nothing usable.
    pub fn is_empty(&self) -> bool {
        self.foreground.is_none() && self.background.is_none() && self.palette.is_none()
    }

    /// Fold a new report into the colors already known (upstream
    /// `applyTerminalColors`, theme-controller.ts:197-207): a field the
    /// terminal did not report keeps its previous value, so a query that
    /// times out does not erase the colors an earlier one delivered.
    pub fn merge_reported(&self, reported: &TerminalColors) -> TerminalColors {
        TerminalColors {
            foreground: reported.foreground.or(self.foreground),
            background: reported.background.or(self.background),
            palette: reported.palette.clone().or_else(|| self.palette.clone()),
        }
    }
}

/// What an OSC color reply reports: the default foreground (OSC 10),
/// background (OSC 11), or a palette index (OSC 4) (upstream
/// `OscColorTarget`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum OscColorTarget {
    Foreground,
    Background,
    Palette(usize),
}

impl OscColorTarget {
    /// The dedup key upstream builds with `String(target)`
    /// (tui.ts:1150).
    fn key(self) -> String {
        match self {
            OscColorTarget::Foreground => "foreground".to_string(),
            OscColorTarget::Background => "background".to_string(),
            OscColorTarget::Palette(index) => index.to_string(),
        }
    }
}

/// One parsed OSC color reply (upstream `parseOscColorResponse`'s return):
/// `rgb` is `None` when the reply's color cannot be parsed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OscColorReply {
    pub target: OscColorTarget,
    pub rgb: Option<Rgb>,
}

/// Parse an OSC 10, 11 or 4 color reply (upstream
/// `OSC_COLOR_RESPONSE_PATTERN`, terminal-colors.ts:41 and
/// `parseOscColorResponse`, :48-56):
///
/// ```text
/// ^ESC ] (?:1[01]) | 4;\d{1,3} ; [^BEL ESC]* (?: BEL | ESC \ ) $
/// ```
///
/// The pattern is anchored at both ends: trailing bytes make it not a reply.
/// A palette index of 1-3 digits parses; the color payload may be empty or
/// unparseable, in which case `rgb` is `None`.
pub fn parse_osc_color_response(data: &str) -> Option<OscColorReply> {
    let mut rest = data.strip_prefix('\x1b')?.strip_prefix(']')?;
    let target = if let Some(after) = rest.strip_prefix("10;") {
        rest = after;
        OscColorTarget::Foreground
    } else if let Some(after) = rest.strip_prefix("11;") {
        rest = after;
        OscColorTarget::Background
    } else {
        let after = rest.strip_prefix("4;")?;
        let digits = after
            .chars()
            .take_while(char::is_ascii_digit)
            .take(4)
            .count();
        if !(1..=3).contains(&digits) {
            return None;
        }
        rest = &after[digits..];
        rest = rest.strip_prefix(';')?;
        OscColorTarget::Palette(after[..digits].parse().ok()?)
    };
    // `[^\x07\x1b]*` then the terminator and end of input.
    let payload_end = rest
        .char_indices()
        .find(|(_, c)| *c == '\x07' || *c == '\x1b')
        .map(|(index, _)| index)
        .unwrap_or(rest.len());
    let payload = &rest[..payload_end];
    let terminator = &rest[payload_end..];
    if terminator != "\x07" && terminator != "\x1b\\" {
        return None;
    }
    Some(OscColorReply {
        target,
        rgb: parse_osc_color_value(payload),
    })
}

/// One hex channel of an OSC color reply: any-length hex scaled to 0-255
/// (upstream `parseOscHexChannel`, terminal-colors.ts:22-31).
fn osc_hex_channel(channel: &str) -> Option<u8> {
    if channel.is_empty() || !channel.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let max = 16f64.powi(channel.len() as i32) - 1.0;
    let value = u32::from_str_radix(channel, 16).ok()? as f64;
    Some(((value / max) * 255.0).round() as u8)
}

/// Parse an OSC color payload (upstream `parseOscColorValue`,
/// terminal-colors.ts:58-83): `#RRGGBB`, `#RRRRGGGGBBBB`, or `rgb:R/G/B` /
/// `rgba:R/G/B` with any consistent hex channel length. The value is
/// trimmed; `rgb:` is matched case-insensitively; extra `/`-separated
/// channels are ignored (upstream destructures the first three).
pub fn parse_osc_color_value(raw_value: &str) -> Option<Rgb> {
    let value = raw_value.trim();
    if let Some(hex) = value.strip_prefix('#') {
        // The payload between the reply markers is arbitrary terminal bytes;
        // hex digits are ASCII, so a non-ASCII payload is simply not a color.
        if !hex.is_ascii() {
            return None;
        }
        return match hex.len() {
            6 => Some((
                u8::from_str_radix(&hex[0..2], 16).ok()?,
                u8::from_str_radix(&hex[2..4], 16).ok()?,
                u8::from_str_radix(&hex[4..6], 16).ok()?,
            )),
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
    Some((r, g, b))
}

/// Parse a mode-2031 light/dark report (upstream
/// `COLOR_SCHEME_REPORT_PATTERN`, terminal-colors.ts:42 and
/// `parseTerminalColorSchemeReport`, :85-90): one or more
/// `ESC [ ? 997 ; (1|2) n` reports, nothing else; `1` is dark, `2` is light,
/// and the last report in a repetition wins.
pub fn parse_terminal_color_scheme_report(data: &str) -> Option<Appearance> {
    let mut rest = data;
    let mut scheme = None;
    while !rest.is_empty() {
        let after = rest.strip_prefix("\x1b[?997;")?;
        let mut chars = after.chars();
        let digit = chars.next()?;
        if digit != '1' && digit != '2' {
            return None;
        }
        rest = chars.as_str().strip_prefix('n')?;
        scheme = Some(if digit == '2' {
            Appearance::Light
        } else {
            Appearance::Dark
        });
    }
    scheme
}

/// Whether `data` is a primary device attributes reply (`ESC [ ? [\d;]* c`,
/// upstream `DEVICE_ATTRIBUTES_RESPONSE_PATTERN`, tui.ts:173), the marker
/// that ends the color replies.
pub fn is_device_attributes_response(data: &str) -> bool {
    let Some(rest) = data.strip_prefix("\x1b[?") else {
        return false;
    };
    let Some(body) = rest.strip_suffix('c') else {
        return false;
    };
    body.chars().all(|c| c.is_ascii_digit() || c == ';')
}

/// Dark or light from everything the terminal reported (upstream
/// `detectTerminalTheme`, theme.ts:628-636): the background it renders
/// decides, else its mode-2031 report, else `COLORFGBG`, else dark.
pub fn detect_terminal_theme(
    colors: &TerminalColors,
    reported_scheme: Option<Appearance>,
    colorfgbg: Option<Appearance>,
) -> Appearance {
    if let Some(background) = colors.background {
        return terminal_appearance(background, colors.foreground);
    }
    reported_scheme.or(colorfgbg).unwrap_or(Appearance::Dark)
}

/// Whether a terminal is dark or light, from its reported colors (upstream
/// `terminalAppearance`, system-theme.ts:416-435): the direction of its own
/// foreground when text can be readable that way, otherwise dark when white
/// text has more contrast on the background than black text.
pub fn terminal_appearance(background: Rgb, foreground: Option<Rgb>) -> Appearance {
    let white = (255u8, 255, 255);
    let black = (0u8, 0, 0);
    let contrast = |a: Rgb, b: Rgb| -> f64 {
        let a = relative_luminance(a.0, a.1, a.2);
        let b = relative_luminance(b.0, b.1, b.2);
        (a.max(b) + 0.05) / (a.min(b) + 0.05)
    };
    let white_contrast = contrast(white, background);
    let black_contrast = contrast(black, background);
    if let Some(foreground) = foreground {
        let lightness = |rgb: Rgb| super::color::rgb_to_oklch_channels(rgb).0;
        let foreground_l = lightness(foreground);
        let background_l = lightness(background);
        if (foreground_l - background_l).abs() > 0.05 {
            let appearance = if foreground_l > background_l {
                Appearance::Dark
            } else {
                Appearance::Light
            };
            let best = if appearance == Appearance::Dark {
                white_contrast
            } else {
                black_contrast
            };
            // Body text must reach the WCAG minimum (4.5:1) to be trusted.
            if best >= 4.5 {
                return appearance;
            }
        }
    }
    if white_contrast >= black_contrast {
        Appearance::Dark
    } else {
        Appearance::Light
    }
}

/// One in-flight color query's reply bookkeeping (upstream
/// `PendingTerminalColorQuery`, tui.ts:145-175).
#[derive(Debug, Default)]
struct QueryReplies {
    foreground: Option<Rgb>,
    background: Option<Rgb>,
    palette: [Option<Rgb>; TERMINAL_PALETTE_SIZE],
    /// Targets that already replied, so duplicates do not count twice.
    replied: BTreeSet<String>,
}

/// One color query (upstream `PendingTerminalColorQuery`): collects the
/// replies until every color or the DA1 marker arrived, then reports the
/// colors. After the query window closed ([`Self::timeout`]) it keeps
/// collecting and delivers once more when the replies complete, which is
/// upstream's `onLateReply`.
#[derive(Debug, Default)]
pub struct TerminalColorQuery {
    replies: QueryReplies,
    /// The query completed (DA1 or all replies); later replies are ignored.
    complete: bool,
    /// The query window closed. The next completion is a late reply and
    /// still delivered (upstream `query.deliver = onLateReply`).
    timed_out: bool,
}

/// What feeding one input string to a [`TerminalColorQuery`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TerminalColorFeed {
    /// Not a color reply: the caller keeps handling the input normally.
    NotAReply,
    /// A color reply (or a duplicate) that was consumed; nothing to deliver.
    Consumed,
    /// The replies are complete: these are the colors to deliver.
    Delivered(TerminalColors),
}

impl TerminalColorQuery {
    /// Start a query (upstream pushes a `PendingTerminalColorQuery` before
    /// writing [`TERMINAL_COLOR_QUERY`], tui.ts:1502-1514).
    pub fn new() -> Self {
        Self::default()
    }

    /// The colors reported so far (upstream `terminalColorQueryResult`,
    /// tui.ts:1167-1170): the palette is only reported when all 16 arrived.
    pub fn colors(&self) -> TerminalColors {
        let palette = if self.replies.palette.iter().all(Option::is_some) {
            Some(self.replies.palette.iter().filter_map(|c| *c).collect())
        } else {
            None
        };
        TerminalColors {
            foreground: self.replies.foreground,
            background: self.replies.background,
            palette,
        }
    }

    /// Whether the query completed and its colors were delivered (upstream
    /// clears `deliver`).
    pub fn is_complete(&self) -> bool {
        self.complete
    }

    /// Feed one input string, mirroring upstream
    /// `consumeTerminalColorResponse` (tui.ts:1133-1170): a DA1 reply
    /// completes the query, an OSC color reply records its target (targets
    /// 16 and above count as replied but are not stored), all 18 targets
    /// complete it, and replies after completion are swallowed.
    pub fn feed(&mut self, data: &str) -> TerminalColorFeed {
        if is_device_attributes_response(data) {
            if self.complete {
                return TerminalColorFeed::Consumed;
            }
            self.complete = true;
            return TerminalColorFeed::Delivered(self.colors());
        }
        let Some(reply) = parse_osc_color_response(data) else {
            return TerminalColorFeed::NotAReply;
        };
        let key = reply.target.key();
        if self.complete || self.replies.replied.contains(&key) {
            return TerminalColorFeed::Consumed;
        }
        self.replies.replied.insert(key);
        match reply.target {
            OscColorTarget::Foreground => self.replies.foreground = reply.rgb,
            OscColorTarget::Background => self.replies.background = reply.rgb,
            OscColorTarget::Palette(index) if index < TERMINAL_PALETTE_SIZE => {
                self.replies.palette[index] = reply.rgb;
            }
            OscColorTarget::Palette(_) => {}
        }
        if self.replies.replied.len() == TERMINAL_COLOR_REPLY_COUNT {
            self.complete = true;
            // Upstream delivers here through the promise's `resolve` when
            // the replies beat the timeout, and through `onLateReply` when
            // they arrive afterwards (tui.ts:1173-1178).
            return TerminalColorFeed::Delivered(self.colors());
        }
        TerminalColorFeed::Consumed
    }

    /// Feed every complete reply at the front of `data`, one reply per
    /// [`Self::feed`] call, and report how many bytes those complete replies
    /// covered. The caller drops exactly that prefix; an incomplete trailing
    /// reply is not covered, so it stays pending for the next batch.
    ///
    /// The Windows reader takes a whole peek batch out of the console queue,
    /// and the answers to [`TERMINAL_COLOR_QUERY`] arrive coalesced in it (a
    /// little over 200 one-character records for up to 19 replies).
    /// [`Self::feed`] parses *exactly one* reply — both of its parsers are
    /// anchored at the ends of the string, so a concatenation of two replies
    /// is neither — which is why a batch fed as one string would be consumed
    /// while recording nothing. The token walk ([`reply_token_len`], the
    /// single step [`crate::tui::terminal_reply::reply_tokens_len`] sums)
    /// knows the boundaries; splitting there feeds each reply on its own and
    /// keeps the last-reply-wins bookkeeping upstream has.
    ///
    /// The combined verdict is the one the delivering reply reached
    /// ([`TerminalColorFeed::Delivered`]; at most one reply in a batch can
    /// deliver, the query is complete afterwards), else
    /// [`TerminalColorFeed::Consumed`] when at least one complete reply was
    /// consumed, else [`TerminalColorFeed::NotAReply`]. A complete reply
    /// string that is not a color reply (an XTVersion answer, an OSC 7501
    /// notice, a `CAN`-cancelled string) is consumed too: upstream takes
    /// *every* reply string out of its stdin buffer, whether or not it is a
    /// color reply, so the reader drops it and keeps looking.
    ///
    /// The Unix reader keeps its own discipline of re-feeding the whole read
    /// accumulation (with its partial tail) to [`Self::feed`]; this method
    /// leaves that path untouched.
    pub fn feed_replies(&mut self, data: &[u8]) -> (TerminalColorFeed, usize) {
        let mut delivered = None;
        let mut consumed = false;
        let mut length = 0;
        let mut rest = data;
        loop {
            let token = reply_token_len(rest);
            if token == 0 {
                break;
            }
            match self.feed(&String::from_utf8_lossy(&rest[..token])) {
                TerminalColorFeed::Delivered(colors) => delivered = Some(colors),
                TerminalColorFeed::Consumed => consumed = true,
                TerminalColorFeed::NotAReply => {}
            }
            length += token;
            rest = &rest[token..];
        }
        let verdict = match delivered {
            Some(colors) => TerminalColorFeed::Delivered(colors),
            None if consumed => TerminalColorFeed::Consumed,
            None => TerminalColorFeed::NotAReply,
        };
        (verdict, length)
    }

    /// The query window closed (upstream's `setTimeout`): the colors so far
    /// are delivered and a later completion is delivered again through
    /// `onLateReply`.
    pub fn timeout(&mut self) -> TerminalColors {
        self.timed_out = true;
        self.colors()
    }

    /// Whether the query window closed without the terminal finishing.
    pub fn timed_out(&self) -> bool {
        self.timed_out
    }
}

/// The queue of in-flight color queries (upstream `pendingTerminalColorQueries`,
/// tui.ts:527): only the oldest query consumes replies, so a stale query's
/// replies can never feed a newer one.
#[derive(Debug, Default)]
pub struct TerminalColorQueries {
    pending: std::collections::VecDeque<TerminalColorQuery>,
}

/// What feeding one input string to [`TerminalColorQueries`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TerminalColorDelivery {
    /// No query is pending, or the input is not a color reply.
    NotAReply,
    /// A color reply consumed by the oldest query; nothing to deliver.
    Consumed,
    /// The replies completed: deliver these colors.
    Delivered(TerminalColors),
}

impl TerminalColorQueries {
    /// Start a new query (upstream pushes before writing the query string).
    pub fn begin_query(&mut self) {
        self.pending.push_back(TerminalColorQuery::new());
    }

    /// How many queries are in flight.
    pub fn pending_count(&self) -> usize {
        self.pending.len()
    }

    /// See [`TerminalColorQuery::feed`]; a completed DA1 query is dropped, a
    /// query completed by its own replies stays in front and swallows later
    /// color replies (upstream only shifts on DA1).
    pub fn feed(&mut self, data: &str) -> TerminalColorDelivery {
        let Some(query) = self.pending.front_mut() else {
            return TerminalColorDelivery::NotAReply;
        };
        if is_device_attributes_response(data) {
            let completed = query.is_complete();
            let colors = query.colors();
            self.pending.pop_front();
            return if completed {
                TerminalColorDelivery::Consumed
            } else {
                TerminalColorDelivery::Delivered(colors)
            };
        }
        match query.feed(data) {
            TerminalColorFeed::NotAReply => TerminalColorDelivery::NotAReply,
            TerminalColorFeed::Consumed => TerminalColorDelivery::Consumed,
            TerminalColorFeed::Delivered(colors) => TerminalColorDelivery::Delivered(colors),
        }
    }

    /// The oldest query's window closed (its timeout fired): returns the
    /// colors so far, delivered a second time if the replies complete later.
    pub fn timeout_oldest(&mut self) -> Option<TerminalColors> {
        self.pending.front_mut().map(TerminalColorQuery::timeout)
    }
}
