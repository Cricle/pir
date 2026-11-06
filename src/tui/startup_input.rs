//! Pre-TUI startup input: the project-trust prompt's choice reader.
//!
//! The trust prompt runs before [`super::terminal::run`] owns the terminal.
//! It used to call `std::io::stdin().read_line()`, whose private `BufReader`
//! can take bytes beyond the terminating newline (and whose cooked-mode line
//! editing makes an Enter delivered as a bare `\r` easy to strand). Bytes it
//! read ahead are invisible to crossterm, so they can never reach the TUI —
//! the trust-stage half of the S2 startup-input race (`3` + Enter sitting on
//! the prompt, or the first composer keys disappearing).
//!
//! This module reads the answer as **crossterm key events** instead: one
//! event at a time, stopping exactly at Enter/Esc. Everything after the
//! submit key stays in crossterm's process-wide event queue, which the TUI
//! event loop drains once it starts — the startup input is buffered, never
//! dropped. The choice state machine ([`TrustChoiceInput`]) is pure so the
//! digit/Enter semantics are unit-tested without a terminal.

use std::io::{IsTerminal, Write};

use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};

/// One key the trust prompt understands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrustChoiceKey {
    /// A decimal digit (`0`-`9`; only `1..=max` is accepted on submit).
    Digit(char),
    Backspace,
    Submit,
    Cancel,
    /// Any other key: ignored, like a non-choice key on the old prompt.
    Ignore,
}

/// What one key did to the choice buffer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrustChoiceStep {
    /// More input is needed.
    Pending,
    /// Enter with a valid 1-based choice; carries the zero-based index.
    Submit(usize),
    /// Esc, Ctrl+C, an empty line or an out-of-range number.
    Cancel,
}

/// The trust prompt's line buffer: digits, Backspace, Enter.
#[derive(Debug, Clone)]
pub struct TrustChoiceInput {
    digits: String,
    max: usize,
}

impl TrustChoiceInput {
    /// A choice buffer for `max` options (the prompt's 1-based numbering).
    pub fn new(max: usize) -> Self {
        Self {
            digits: String::new(),
            max,
        }
    }

    /// The digits typed so far (used by the echo path and tests).
    pub fn digits(&self) -> &str {
        &self.digits
    }

    /// Apply one key. Only a valid in-range Enter submits; everything else
    /// either edits the short digit buffer or cancels, exactly like the old
    /// `read_line` + `parse::<usize>()` path (which also treated an empty or
    /// malformed line as "no choice").
    pub fn push(&mut self, key: TrustChoiceKey) -> TrustChoiceStep {
        match key {
            TrustChoiceKey::Digit(character) if character.is_ascii_digit() => {
                // The prompt has at most a handful of options; a few digits
                // are enough and keep a held key from growing the buffer.
                if self.digits.len() < 3 {
                    self.digits.push(character);
                }
                TrustChoiceStep::Pending
            }
            TrustChoiceKey::Backspace => {
                self.digits.pop();
                TrustChoiceStep::Pending
            }
            TrustChoiceKey::Submit => match self.digits.parse::<usize>() {
                Ok(picked) if (1..=self.max).contains(&picked) => {
                    TrustChoiceStep::Submit(picked - 1)
                }
                _ => TrustChoiceStep::Cancel,
            },
            TrustChoiceKey::Cancel => TrustChoiceStep::Cancel,
            // A non-decimal character is not a choice digit.
            TrustChoiceKey::Digit(_) | TrustChoiceKey::Ignore => TrustChoiceStep::Pending,
        }
    }
}

/// Read one 1-based trust choice from the terminal.
///
/// Returns `None` when this reader is unavailable (stdin is not a terminal or
/// raw mode could not be enabled), so the caller can fall back to its
/// line-based prompt. Returns `Some(choice)` when the prompt ran: `Some(index)`
/// is the zero-based selection, `None` means cancelled.
///
/// The raw-mode guard restores the previous console mode before the caller
/// continues into the TUI startup. Events after the submit key are left in
/// crossterm's global queue for the TUI.
pub fn read_trust_choice(max: usize) -> Option<Option<usize>> {
    if !std::io::stdin().is_terminal() {
        return None;
    }
    let _raw = RawModeGuard::enable()?;
    let mut input = TrustChoiceInput::new(max);
    let mut stdout = std::io::stdout();
    loop {
        let event = match event::read() {
            Ok(event) => event,
            // A terminal that cannot deliver events behaves like the old
            // reader's EOF: cancel instead of falling back to a blocking
            // line read.
            Err(_) => return Some(None),
        };
        let Event::Key(key) = event else {
            continue;
        };
        if key.kind != KeyEventKind::Press {
            continue;
        }
        let action = match key.code {
            KeyCode::Char(character)
                if character.is_ascii_digit()
                    && !key
                        .modifiers
                        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                TrustChoiceKey::Digit(character)
            }
            KeyCode::Enter | KeyCode::Char('\r') | KeyCode::Char('\n') => TrustChoiceKey::Submit,
            KeyCode::Backspace => TrustChoiceKey::Backspace,
            KeyCode::Esc => TrustChoiceKey::Cancel,
            KeyCode::Char('c') | KeyCode::Char('C')
                if key.modifiers.contains(KeyModifiers::CONTROL) =>
            {
                TrustChoiceKey::Cancel
            }
            _ => TrustChoiceKey::Ignore,
        };
        match input.push(action) {
            TrustChoiceStep::Pending => match action {
                TrustChoiceKey::Digit(character) => {
                    let _ = write!(stdout, "{character}");
                    let _ = stdout.flush();
                }
                TrustChoiceKey::Backspace => {
                    let _ = write!(stdout, "\x08 \x08");
                    let _ = stdout.flush();
                }
                _ => {}
            },
            TrustChoiceStep::Submit(index) => {
                let _ = write!(stdout, "\r\n");
                let _ = stdout.flush();
                return Some(Some(index));
            }
            TrustChoiceStep::Cancel => {
                let _ = write!(stdout, "\r\n");
                let _ = stdout.flush();
                return Some(None);
            }
        }
    }
}

/// Restore the console mode on every return path (including a read error).
struct RawModeGuard;

impl RawModeGuard {
    fn enable() -> Option<Self> {
        crossterm::terminal::enable_raw_mode().ok().map(|_| Self)
    }
}

impl Drop for RawModeGuard {
    fn drop(&mut self) {
        let _ = crossterm::terminal::disable_raw_mode();
    }
}
