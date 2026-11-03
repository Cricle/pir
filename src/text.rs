//! Terminal-text helpers shared by the core and the TUI.
//!
//! These are pure string transforms over terminal output. They live here
//! rather than under `crate::tui` because core surfaces that must work without
//! the `tui` feature consume them too — the HTML session export strips ANSI
//! codes from tool output, and headless SDK hosts render the same text. The
//! TUI re-exports [`strip_ansi`], so there is exactly one implementation.

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
