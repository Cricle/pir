//! Terminal-text helpers shared by the core and the TUI.
//!
//! These are pure string transforms over terminal output. They live here
//! rather than under `crate::tui` because core surfaces that must work without
//! the `tui` feature consume them too — the HTML session export strips ANSI
//! codes from tool output, and headless SDK hosts render the same text. The
//! TUI re-exports [`strip_ansi`], so there is exactly one implementation.

use std::borrow::Cow;
use std::iter::Peekable;
use std::str::CharIndices;

/// Strip ANSI escape sequences (CSI, OSC, DCS and the common two-byte /
/// charset-designator ESC sequences) from `text`, keeping the printable
/// payload.
pub fn strip_ansi(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            if chars.peek() == Some(&'[') {
                // CSI: `ESC [ parameters final`.
                chars.next();
                for c in chars.by_ref() {
                    if ('@'..='~').contains(&c) {
                        break;
                    }
                }
            } else if chars.peek() == Some(&']') || chars.peek() == Some(&'_') {
                // OSC / PM / APC: `... BEL` or `... ST` (`ESC \`).
                chars.next();
                let mut prev = '\0';
                for c in chars.by_ref() {
                    if c == '\x07' || (prev == '\x1b' && c == '\\') {
                        break;
                    }
                    prev = c;
                }
            } else if chars.peek() == Some(&'P') {
                // DCS (sixel images and other device strings): the payload
                // runs to ST (`ESC \`) and is never printable text.
                chars.next();
                let mut prev = '\0';
                for c in chars.by_ref() {
                    if prev == '\x1b' && c == '\\' {
                        break;
                    }
                    prev = c;
                }
            } else if let Some(&next) = chars.peek() {
                // Two-byte and charset-designator sequences: `ESC 7`/`ESC 8`
                // (save/restore cursor), `ESC =`/`ESC >` (keypad mode),
                // `ESC M` (reverse index), `ESC # ...` (line attributes) and
                // `ESC ( X` / `ESC ) X` (character-set designators), which
                // carry one extra byte naming the set.
                if matches!(next, '(' | ')' | '#' | '7' | '8' | '=' | '>' | 'M') {
                    chars.next();
                    if matches!(next, '(' | ')') {
                        chars.next();
                    }
                }
            }
            continue;
        }
        out.push(c);
    }
    out
}

/// Escape text for HTML body and attribute contexts (upstream `escapeHtml`):
/// `& < > " '` become their named/numeric entities.
pub(crate) fn escape_html(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            other => out.push(other),
        }
    }
    out
}

/// Compact token counts (upstream `formatTokens`): `999`, `1.2k`, `12k`,
/// `1.2M`, `12M`. Counts below 10k use one decimal; from 10k the k/M value is
/// rounded to the nearest integer (`12_500` → `13k`, `12_345_678` → `12M`).
pub fn format_tokens(count: u64) -> String {
    if count < 1000 {
        return count.to_string();
    }
    if count < 10_000 {
        return format!("{:.1}k", count as f64 / 1000.0);
    }
    if count < 1_000_000 {
        return format!("{}k", (count as f64 / 1000.0).round() as u64);
    }
    if count < 10_000_000 {
        return format!("{:.1}M", count as f64 / 1_000_000.0);
    }
    format!("{}M", (count as f64 / 1_000_000.0).round() as u64)
}

/// Truncate summary text to `max_chars` Unicode scalar values, appending the
/// upstream `[... N more characters truncated]` marker when content is
/// dropped. Character (not byte) based, so multi-byte text never splits.
pub(crate) fn truncate_chars_with_marker(text: &str, max_chars: usize) -> String {
    let length = text.chars().count();
    if length <= max_chars {
        return text.to_owned();
    }
    let head: String = text.chars().take(max_chars).collect();
    format!("{head}{}", truncated_chars_marker(length - max_chars))
}

/// The marker appended by [`truncate_chars_with_marker`].
pub(crate) fn truncated_chars_marker(dropped: usize) -> String {
    format!("\n\n[... {dropped} more characters truncated]")
}

// ---------------------------------------------------------------------------
// Tool-output truncation core (shared by `coding_agent::truncate` and
// `durable::truncate`).
// ---------------------------------------------------------------------------

/// Default maximum number of lines for tool-output truncation.
pub const DEFAULT_MAX_LINES: usize = 2000;
/// Default maximum number of bytes (50 KiB) for tool-output truncation.
pub const DEFAULT_MAX_BYTES: usize = 50 * 1024;

/// Which limit stopped a truncation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TruncatedBy {
    /// The line limit.
    Lines,
    /// The byte limit.
    Bytes,
}

/// Format bytes as a human-readable size (upstream `formatSize`).
pub fn format_size(bytes: usize) -> String {
    if bytes < 1024 {
        format!("{bytes}B")
    } else if bytes < 1024 * 1024 {
        format!("{:.1}KB", bytes as f64 / 1024.0)
    } else {
        format!("{:.1}MB", bytes as f64 / (1024.0 * 1024.0))
    }
}

/// Split content into lines the way the upstream counter does: a trailing
/// newline does not create a final empty line.
pub(crate) fn split_truncate_lines(content: &str) -> Vec<&str> {
    if content.is_empty() {
        return Vec::new();
    }
    let mut lines: Vec<&str> = content.split('\n').collect();
    if content.ends_with('\n') {
        lines.pop();
    }
    lines
}

/// Shared outcome of head truncation; each public `TruncationResult` adapts it.
pub(crate) struct TruncatedHead {
    pub content: String,
    pub truncated: bool,
    pub truncated_by: Option<TruncatedBy>,
    pub total_lines: usize,
    pub total_bytes: usize,
    pub output_lines: usize,
    pub output_bytes: usize,
    pub first_line_exceeds_limit: bool,
    pub max_lines: usize,
    pub max_bytes: usize,
}

/// Truncate `prefix` from the head, keeping whole lines. `total_lines` and
/// `total_bytes` describe the whole text (they may cover more than `prefix`).
/// A first line over the byte limit yields empty content and
/// [`TruncatedHead::first_line_exceeds_limit`].
pub(crate) fn truncate_head_core(
    prefix: &str,
    total_lines: usize,
    total_bytes: usize,
    max_lines: usize,
    max_bytes: usize,
) -> TruncatedHead {
    let lines = split_truncate_lines(prefix);

    if total_lines <= max_lines && total_bytes <= max_bytes {
        return TruncatedHead {
            content: prefix.to_owned(),
            truncated: false,
            truncated_by: None,
            total_lines,
            total_bytes,
            output_lines: total_lines,
            output_bytes: total_bytes,
            first_line_exceeds_limit: false,
            max_lines,
            max_bytes,
        };
    }

    let first_line_bytes = lines.first().map_or(0, |line| line.len());
    if first_line_bytes > max_bytes {
        return TruncatedHead {
            content: String::new(),
            truncated: true,
            truncated_by: Some(TruncatedBy::Bytes),
            total_lines,
            total_bytes,
            output_lines: 0,
            output_bytes: 0,
            first_line_exceeds_limit: true,
            max_lines,
            max_bytes,
        };
    }

    let mut kept: Vec<&str> = Vec::new();
    let mut used = 0usize;
    let mut truncated_by = TruncatedBy::Lines;
    for (index, line) in lines.iter().enumerate().take(max_lines) {
        let line_bytes = line.len() + usize::from(index > 0);
        if used + line_bytes > max_bytes {
            truncated_by = TruncatedBy::Bytes;
            break;
        }
        used += line_bytes;
        kept.push(line);
    }
    // Without a byte break, only omitted lines prove the line limit was
    // reached; otherwise a trailing newline alone exceeded the byte limit.
    if truncated_by != TruncatedBy::Bytes {
        truncated_by = if kept.len() < total_lines {
            TruncatedBy::Lines
        } else {
            TruncatedBy::Bytes
        };
    }

    let output = kept.join("\n");
    TruncatedHead {
        output_bytes: output.len(),
        content: output,
        truncated: true,
        truncated_by: Some(truncated_by),
        total_lines,
        total_bytes,
        output_lines: kept.len(),
        first_line_exceeds_limit: false,
        max_lines,
        max_bytes,
    }
}

/// Sanitize untrusted text before it is rendered into the terminal.
///
/// Model responses, tool stdout/stderr, file contents and replayed session
/// records are attacker-controlled: a raw `ESC ] 0 ; title BEL` changes the
/// window title, `ESC ] 52 ; c ; … BEL` writes the clipboard, and `ESC [ 2 J`
/// erases the screen. The TUI's own styling is generated *after* this pass, so
/// stripping untrusted control sequences cannot break its palette.
///
/// Policy:
/// - keep printable text, newline, carriage return and tab;
/// - keep SGR only (`ESC [ <digits/;/:> m`), so normal color/style runs from
///   trusted renderers survive; every other CSI (cursor movement, erase,
///   private modes) is dropped;
/// - drop every OSC string (OSC 0/2 title, OSC 52 clipboard, OSC 8 hyperlinks,
///   …). This is deliberate: the TUI never needs an untrusted OSC, and
///   terminals differ in which OSC strings they honor, so the safe default is
///   to strip all of them;
/// - drop DCS / SOS / PM / APC control strings and their payload;
/// - drop all other C0 controls (BEL, BS, VT, FF, …), DEL, and every C1
///   control (including the 8-bit CSI/OSC/DCS/SOS/PM/APC introducers, which
///   are consumed together with their payload).
///
/// An unterminated escape sequence at the end of the input is dropped whole:
/// when a stream is cut mid-sequence the terminal must never receive the
/// partial prefix.
pub fn sanitize_terminal_text(text: &str) -> Cow<'_, str> {
    sanitize_impl(text, true)
}

/// [`sanitize_terminal_text`] with SGR stripped as well: the form used by
/// non-terminal sinks such as the HTML session export, where every escape and
/// control byte must be gone while `\n`/`\t` stay meaningful.
pub fn sanitize_plain_text(text: &str) -> String {
    match sanitize_impl(text, false) {
        Cow::Borrowed(borrowed) => borrowed.to_owned(),
        Cow::Owned(owned) => owned,
    }
}

fn sanitize_impl(text: &str, keep_sgr: bool) -> Cow<'_, str> {
    if !contains_control(text) {
        return Cow::Borrowed(text);
    }
    let mut out = String::with_capacity(text.len());
    let mut chars = text.char_indices().peekable();
    while let Some((start, ch)) = chars.next() {
        match ch {
            '\x1b' => {
                let Some(&(_, next)) = chars.peek() else {
                    // A trailing lone ESC: never forward a partial sequence.
                    break;
                };
                match next {
                    '[' => {
                        chars.next();
                        let mut sgr_ok = keep_sgr;
                        let mut final_char = None;
                        while let Some(&(pos, c)) = chars.peek() {
                            if ('\x30'..='\x3f').contains(&c) {
                                // Parameter bytes. SGR only uses digits and
                                // the `;` / `:` sub-parameter separators;
                                // anything else (`?`, `>`, `=`, …) makes this
                                // a private/other CSI, which we drop.
                                if !(c.is_ascii_digit() || c == ';' || c == ':') {
                                    sgr_ok = false;
                                }
                                chars.next();
                            } else if ('\x20'..='\x2f').contains(&c) {
                                // Intermediate bytes are never part of SGR.
                                sgr_ok = false;
                                chars.next();
                            } else if ('\x40'..='\x7e').contains(&c) {
                                final_char = Some((pos, c));
                                chars.next();
                                break;
                            } else {
                                // Malformed CSI: stop before this character.
                                break;
                            }
                        }
                        if let Some((pos, 'm')) = final_char {
                            if sgr_ok {
                                // `start` is the ESC; `pos` is the inclusive
                                // byte end of the final byte.
                                out.push_str(&text[start..=pos]);
                            }
                        }
                    }
                    // OSC, plus the DCS/SOS/PM/APC families: consume through
                    // BEL or ST (ESC \ / U+009C) and drop the whole string.
                    ']' | 'P' | 'X' | '^' | '_' => {
                        chars.next();
                        consume_control_string(&mut chars);
                    }
                    _ => {
                        // Two-byte ESC sequence or a charset designator with
                        // one extra byte.
                        let Some((_, second)) = chars.next() else {
                            break;
                        };
                        if matches!(second, '(' | ')' | '*' | '+' | '-' | '.' | '/' | '#' | '%') {
                            chars.next();
                        }
                    }
                }
            }
            // 8-bit C1 introducers. Consume their payload so the body does
            // not survive as literal text.
            '\u{009b}' => consume_csi(&mut chars),
            '\u{009d}' | '\u{0090}' | '\u{0098}' | '\u{009e}' | '\u{009f}' => {
                consume_control_string(&mut chars)
            }
            '\n' | '\r' | '\t' => out.push(ch),
            c if c.is_control() => {
                // C0 (minus newline/tab above), DEL and the remaining C1
                // controls (including 8-bit ST) are dropped.
            }
            _ => out.push(ch),
        }
    }
    Cow::Owned(out)
}

/// Whether `text` contains anything [`sanitize_terminal_text`] must touch.
/// The fast path keeps clean renderer output allocation- and copy-free.
fn contains_control(text: &str) -> bool {
    text.chars()
        .any(|c| c == '\x1b' || (c.is_control() && c != '\n' && c != '\r' && c != '\t'))
}

/// Consume one CSI sequence starting just after `ESC [` (or after the 8-bit
/// `U+009B` introducer). The sequence is dropped by the caller.
fn consume_csi(chars: &mut Peekable<CharIndices<'_>>) {
    while let Some(&(_, c)) = chars.peek() {
        chars.next();
        if ('\x40'..='\x7e').contains(&c) {
            break;
        }
        if !('\x20'..='\x3f').contains(&c) {
            break;
        }
    }
}

/// Consume an OSC/DCS/SOS/PM/APC payload through BEL, ST (`ESC \`) or the
/// 8-bit ST (`U+009C`). An unterminated string runs to the end of the input.
fn consume_control_string(chars: &mut Peekable<CharIndices<'_>>) {
    let mut prev_esc = false;
    while let Some((_, c)) = chars.next() {
        if c == '\x07' || c == '\u{009c}' {
            break;
        }
        if prev_esc && c == '\\' {
            break;
        }
        prev_esc = c == '\x1b';
    }
}
