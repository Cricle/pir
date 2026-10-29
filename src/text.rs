//! Terminal-text helpers shared by the core and the TUI.
//!
//! These are pure string transforms over terminal output. They live here
//! rather than under `crate::tui` because core surfaces that must work without
//! the `tui` feature consume them too — the HTML session export strips ANSI
//! codes from tool output, and headless SDK hosts render the same text. The
//! TUI re-exports [`strip_ansi`], so there is exactly one implementation.

/// Strip ANSI escape sequences (CSI, OSC and DCS introducers) from `text`,
/// keeping the printable payload.
pub fn strip_ansi(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            if chars.peek() == Some(&'[') {
                chars.next();
                for c in chars.by_ref() {
                    if ('@'..='~').contains(&c) {
                        break;
                    }
                }
            } else if chars.peek() == Some(&']') || chars.peek() == Some(&'_') {
                chars.next();
                let mut prev = '\0';
                for c in chars.by_ref() {
                    if c == '\x07' || (prev == '\x1b' && c == '\\') {
                        break;
                    }
                    prev = c;
                }
            }
            continue;
        }
        out.push(c);
    }
    out
}
