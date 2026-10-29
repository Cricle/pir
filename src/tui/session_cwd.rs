//! Interactive missing-session-cwd handling for the TUI (upstream
//! `core/session-cwd.ts` + `interactive-mode.ts:promptForMissingSessionCwd` /
//! `main.ts` startup selector).
//!
//! Headless hosts fail hard when a resumed session's stored `cwd` no longer
//! exists. The interactive host instead offers the upstream two-choice prompt:
//! continue in the current working directory, or cancel the resume. Choosing
//! *continue* reopens the session through
//! [`SessionManager::open_with`] with the current cwd as the override — the
//! same `cwdOverride` upstream passes to `SessionManager.open` — so nothing is
//! rewritten and no entry is appended before the user confirms.
//!
//! The key resolution and prompt text are pure and unit-tested from
//! `tests/suites/tui_session_cwd.rs`; only [`prompt_missing_session_cwd`]
//! touches the terminal.

use std::io::Write;
use std::path::Path;

use crossterm::cursor::MoveTo;
use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::terminal::{self, Clear, ClearType, EnterAlternateScreen, LeaveAlternateScreen};

use crate::session_manager::{
    MissingSessionCwd, SessionManager, SessionTailPage, DEFAULT_TAIL_PAGE_BYTES,
    DEFAULT_TAIL_PAGE_ENTRIES,
};

/// Prompt title (upstream `promptForMissingSessionCwd` →
/// `showExtensionConfirm("Session cwd not found", …)`).
pub const MISSING_CWD_TITLE: &str = "Session cwd not found";

/// The two decisions offered by the prompt, in upstream order
/// (`main.ts:promptForMissingSessionCwd` options).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MissingCwdChoice {
    /// Continue in the current working directory (`cwdOverride`).
    Continue,
    /// Abort the resume without touching the session file.
    Cancel,
}

/// One key's effect in the prompt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MissingCwdPromptStep {
    /// Move the highlight to this option index.
    Move(usize),
    /// Commit the highlighted/asked decision.
    Choose(MissingCwdChoice),
    /// Any other key: ignored.
    Ignore,
}

/// The prompt content (upstream `formatMissingSessionCwdPrompt` +
/// `showExtensionConfirm` labels).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MissingCwdPrompt {
    pub title: String,
    pub message: String,
    pub options: Vec<&'static str>,
}

impl MissingCwdPrompt {
    /// The default highlighted option (upstream `showExtensionConfirm`
    /// preselects "Yes"; the startup selector preselects the first row).
    pub fn default_selection(&self) -> usize {
        0
    }
}

/// Upstream `formatMissingSessionCwdPrompt` (session-cwd.ts:40-42): the stored
/// cwd, then the current cwd offered as the continuation target.
pub fn format_missing_session_cwd_prompt(issue: &MissingSessionCwd, fallback_cwd: &str) -> String {
    format!(
        "cwd from session file does not exist\n{}\n\ncontinue in current cwd\n{}",
        issue.session_cwd, fallback_cwd
    )
}

/// The prompt shown for `issue`: the upstream title and continue/cancel rows.
pub fn missing_cwd_prompt(issue: &MissingSessionCwd, fallback_cwd: &str) -> MissingCwdPrompt {
    MissingCwdPrompt {
        title: MISSING_CWD_TITLE.to_string(),
        message: format_missing_session_cwd_prompt(issue, fallback_cwd),
        options: vec!["Continue", "Cancel"],
    }
}

/// Resolve one key press against the prompt: arrows move the highlight, Enter
/// commits it, Esc / Ctrl+C cancel (upstream `tui.select.confirm` /
/// `tui.select.cancel` semantics).
pub fn missing_cwd_prompt_step(
    code: KeyCode,
    modifiers: KeyModifiers,
    selected: usize,
    option_count: usize,
) -> MissingCwdPromptStep {
    let last = option_count.saturating_sub(1);
    match code {
        KeyCode::Up => MissingCwdPromptStep::Move(selected.saturating_sub(1)),
        KeyCode::Down => MissingCwdPromptStep::Move((selected + 1).min(last)),
        KeyCode::Enter => MissingCwdPromptStep::Choose(if selected == 0 {
            MissingCwdChoice::Continue
        } else {
            MissingCwdChoice::Cancel
        }),
        KeyCode::Esc => MissingCwdPromptStep::Choose(MissingCwdChoice::Cancel),
        KeyCode::Char('c') if modifiers.contains(KeyModifiers::CONTROL) => {
            MissingCwdPromptStep::Choose(MissingCwdChoice::Cancel)
        }
        _ => MissingCwdPromptStep::Ignore,
    }
}

/// The cwd override for a decision (`None` = abort the resume).
pub fn missing_cwd_override(choice: MissingCwdChoice, fallback_cwd: &str) -> Option<String> {
    match choice {
        MissingCwdChoice::Continue => Some(fallback_cwd.to_string()),
        MissingCwdChoice::Cancel => None,
    }
}

/// Detect a stored session cwd that no longer exists without parsing the whole
/// file: the bounded header probe of [`SessionTailPage`] carries the cwd even
/// for a session whose body is only ever read tail-first.
///
/// Returns `None` for a readable existing cwd, a missing header, or a file the
/// caller cannot probe (the normal open path then reports the read error).
pub fn detect_missing_session_cwd(path: &Path) -> Option<MissingSessionCwd> {
    let page = SessionTailPage::open(path, DEFAULT_TAIL_PAGE_BYTES, DEFAULT_TAIL_PAGE_ENTRIES)
        .ok()
        .flatten()?;
    let header = page.file_header()?;
    if header.cwd.is_empty() || Path::new(&header.cwd).exists() {
        return None;
    }
    Some(MissingSessionCwd {
        session_file: path.to_path_buf(),
        session_cwd: header.cwd,
    })
}

/// Reopen `path` with `fallback_cwd` as the manager cwd (upstream
/// `SessionManager.open(path, sessionDir, cwdOverride)`). The file is opened
/// read/append only; the stored header is not rewritten.
pub fn open_session_in_current_cwd(
    path: &Path,
    fallback_cwd: &str,
) -> crate::Result<SessionManager> {
    let mut manager = SessionManager::open_with(path, None, Some(fallback_cwd))?;
    // The manager re-derives its cwd from the stored header while loading the
    // entries, so the constructor's `cwdOverride` does not survive that step.
    // Apply the override on the IN-MEMORY entry list instead: rehydrate the
    // same manager (same file, same id) with a header whose cwd is the current
    // directory. `set_session_file` with preloaded entries performs no I/O, so
    // the stored file keeps its original header exactly as upstream's
    // `cwdOverride` does.
    let overridden_cwd = crate::session_manager::resolve_path_str(fallback_cwd);
    let Some(mut header) = manager.header().cloned() else {
        return Ok(manager);
    };
    header.cwd = overridden_cwd;
    // `entries()` excludes the header by contract; re-add the overridden one
    // in front of the untouched transcript entries.
    let mut entries = Vec::new();
    entries.push(crate::session_manager::FileEntry::Session(header));
    entries.extend(manager.entries());
    manager.set_session_file(path, Some(entries))?;
    Ok(manager)
}

/// Run the interactive prompt. Enter / Esc / Ctrl+C end it; a read failure is
/// treated as cancel so raw mode and the alternate screen never stay behind.
pub fn prompt_missing_session_cwd(
    issue: &MissingSessionCwd,
    fallback_cwd: &str,
) -> MissingCwdChoice {
    let prompt = missing_cwd_prompt(issue, fallback_cwd);
    let mut selected = prompt.default_selection();
    let raw_was_enabled = terminal::is_raw_mode_enabled().unwrap_or(false);
    if !raw_was_enabled && terminal::enable_raw_mode().is_err() {
        return MissingCwdChoice::Cancel;
    }
    let mut stdout = std::io::stdout();
    let alt_entered = crossterm::execute!(stdout, EnterAlternateScreen).is_ok();
    let choice = loop {
        render_prompt(&prompt, selected);
        match crossterm::event::read() {
            Ok(Event::Key(key)) if key.kind == KeyEventKind::Press => {
                match missing_cwd_prompt_step(
                    key.code,
                    key.modifiers,
                    selected,
                    prompt.options.len(),
                ) {
                    MissingCwdPromptStep::Move(next) => selected = next,
                    MissingCwdPromptStep::Choose(choice) => break choice,
                    MissingCwdPromptStep::Ignore => {}
                }
            }
            Ok(_) => {}
            Err(_) => break MissingCwdChoice::Cancel,
        }
    };
    if alt_entered {
        let _ = crossterm::execute!(stdout, LeaveAlternateScreen);
    }
    if !raw_was_enabled {
        let _ = terminal::disable_raw_mode();
    }
    let _ = stdout.flush();
    choice
}

/// Width used when the terminal size cannot be queried.
const FALLBACK_WIDTH: usize = 100;

fn render_prompt(prompt: &MissingCwdPrompt, selected: usize) {
    let width = crossterm::terminal::size()
        .map(|(columns, _)| columns as usize)
        .unwrap_or(FALLBACK_WIDTH);
    let mut lines: Vec<String> = Vec::new();
    lines.push(super::with_theme(|theme| {
        theme.fg(super::ThemeColor::Accent, &theme.bold(&prompt.title))
    }));
    lines.push(String::new());
    for line in prompt.message.lines() {
        lines.push(super::truncate_to_width(line, width, "…"));
    }
    lines.push(String::new());
    for (index, option) in prompt.options.iter().enumerate() {
        let marker = if index == selected { "› " } else { "  " };
        let text = format!("{marker}{option}");
        lines.push(if index == selected {
            super::with_theme(|theme| theme.fg(super::ThemeColor::Accent, &text))
        } else {
            text
        });
    }
    lines.push(String::new());
    lines.push(super::with_theme(|theme| {
        theme.fg(super::ThemeColor::Muted, "Enter to select · Esc to cancel")
    }));
    let mut stdout = std::io::stdout();
    let _ = crossterm::execute!(stdout, MoveTo(0, 0), Clear(ClearType::All));
    for line in lines {
        let _ = writeln!(stdout, "{}", super::truncate_to_width(&line, width, "…"));
    }
    let _ = stdout.flush();
}
