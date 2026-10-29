//! Extension dialog overlays — the visual half of the
//! `Notification::DialogRequest` channel (`ctx.ui.select/confirm/input/editor`).
//!
//! Port of upstream `components/extension-selector.ts`,
//! `extension-input.ts` and `extension-editor.ts`: a DynamicBorder-framed
//! modal that seizes keyboard input while open (mirroring how the
//! model/thinking selectors do), renders the chat transcript behind it, and
//! resolves through [`DialogOverlay::handle_key`] into a JSON reply routed
//! back over `ExtensionHandle::respond_dialog`.
//!
//! Key contracts (upstream file:behavior, with the pir key-map noted where
//! it diverges):
//! - select: `↑/↓` and `j`/`k` navigate with a clamped cursor, Enter
//!   selects, Esc cancels (reply `null` → `undefined` in JS). Upstream's
//!   selector has no type-to-filter, so typed characters are inert
//!   (`extension-selector.ts:96-111`);
//! - confirm: rendered as the upstream Yes/No selector, but `y` → `true`,
//!   `n` → `false`, `←/→` toggle the highlight, Enter submits the
//!   configured default, Esc cancels (upstream's selector has only
//!   `↑/↓`/`k`/`j` + Enter);
//! - input: single-line editing, Enter submits, Esc cancels;
//! - editor: multi-line editing with a prefill, Enter submits,
//!   Shift+Enter / Ctrl+J newlines, Esc cancels (pir has no external
//!   editor route, unlike upstream `ctrl+g`).

use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde_json::Value as JsonValue;
use unicode_width::UnicodeWidthChar;

use super::ansi::{pad_to_width, truncate_to_width, visible_width};
use super::core::{Component, Container, DynamicBorder, Spacer, Text};
use super::editor::Editor;
use super::theme::{with_theme, Theme, ThemeColor};

/// Modal input keys (decoupled from crossterm so the state machine is a
/// pure, directly testable function; the TUI maps real key events in).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DialogKey {
    Up,
    Down,
    Enter,
    /// Insert a newline (Ctrl+J / Shift+Enter in the editor dialog; plain
    /// Enter submits).
    Newline,
    Esc,
    Backspace,
    Left,
    Right,
    Char(char),
    // -- Full editor surface (upstream `extension-input.ts` /
    // `extension-editor.ts` route the whole pi-tui Editor keymap; pir's
    // dialogs previously exposed only Left/Right/Backspace). --
    Home,
    End,
    Delete,
    /// Word-left motion (ctrl+left / alt+left / alt+b).
    WordLeft,
    /// Word-right motion (ctrl+right / alt+right / alt+f).
    WordRight,
    /// Delete word backward (ctrl+w / alt+backspace).
    DeleteWordBackward,
    /// Delete word forward (alt+d / alt+delete).
    DeleteWordForward,
    /// Delete to line start (ctrl+u).
    DeleteToLineStart,
    /// Delete to line end (ctrl+k).
    DeleteToLineEnd,
    /// Undo (ctrl+-).
    Undo,
    /// Yank the newest kill (ctrl+y).
    Yank,
    /// Rotate the kill ring and yank again (alt+y).
    YankPop,
}

/// What a dialog key press resolves to.
#[derive(Debug, Clone, PartialEq)]
pub enum DialogOutcome {
    /// Key not consumed by the dialog (ignored while open).
    None,
    /// The user answered; the value is the JSON reply for
    /// `ExtensionHandle::respond_dialog`.
    Submit(JsonValue),
    /// The user cancelled; the reply is `null` (JS `undefined`).
    Cancel,
}

/// One `select` choice (upstream string options are normalized to
/// `{value, label}` by the JS prelude).
#[derive(Debug, Clone, PartialEq)]
pub struct DialogOption {
    /// JSON value returned to the extension when chosen.
    pub value: JsonValue,
    /// Display label.
    pub label: String,
    /// Optional dimmed description.
    pub description: Option<String>,
}

impl DialogOption {
    fn from_json(value: &JsonValue) -> DialogOption {
        match value {
            JsonValue::String(text) => DialogOption {
                value: value.clone(),
                label: text.clone(),
                description: None,
            },
            other => DialogOption {
                value: other.get("value").cloned().unwrap_or_else(|| other.clone()),
                label: other
                    .get("label")
                    .and_then(JsonValue::as_str)
                    .or_else(|| other.get("value").and_then(JsonValue::as_str))
                    .map(str::to_string)
                    .unwrap_or_else(|| other.to_string()),
                description: other
                    .get("description")
                    .and_then(JsonValue::as_str)
                    .map(str::to_string),
            },
        }
    }
}

/// The dialog body: which `ctx.ui` prompt is being answered.
#[derive(Debug, Clone, PartialEq)]
pub enum DialogBody {
    Select {
        title: String,
        options: Vec<DialogOption>,
        /// Index of the option pre-selected as current (`opts.default`).
        default: Option<usize>,
    },
    Confirm {
        title: String,
        message: String,
        /// Value submitted on bare Enter (`opts.defaultYes`, upstream
        /// confirm dialogs default to `false`).
        default_yes: bool,
    },
    Input {
        title: String,
        placeholder: String,
        default_value: String,
    },
    Editor {
        title: String,
        prefill: String,
    },
}

/// Whole seconds a countdown shows for `remaining` (upstream
/// `CountdownTimer`: `Math.ceil(timeoutMs / 1000)`, so a fresh 5000 ms wait
/// reads `(5s)`). Public for the display-layer tests.
pub fn countdown_seconds(remaining: Duration) -> u64 {
    (remaining.as_millis() as u64).div_ceil(1000)
}

/// A pending dialog: correlation id + parsed body + interaction state.
pub struct DialogOverlay {
    /// `Notification::DialogRequest.id` (reply correlation).
    pub id: u64,
    body: DialogBody,
    /// Select cursor, an index into `options` (upstream
    /// `ExtensionSelectorComponent.selectedIndex`: clamped moves, no wrap).
    select_index: usize,
    /// Input/editor buffer.
    buffer: Option<Editor>,
    /// Confirm answer toggled with ←/→ (y/n keys remain).
    confirm_yes: bool,
    /// Per-dialog timeout deadline (upstream `ExtensionUIDialogOptions`
    /// `timeout` via `CountdownTimer`, countdown-timer.ts): the title counts
    /// down `(Ns)` and the TUI closes the dialog (null reply) on expiry.
    /// `None` = no timeout.
    deadline: Option<Instant>,
    /// Queue-only expiry for a request that carried no `timeout`: the
    /// runtime's global dialog TTL bounds the extension-side wait, so an
    /// overlay still QUEUED when that TTL elapses can never be seen (its JS
    /// waiter already resolved `null`). Not rendered as a countdown and not
    /// applied to an already-visible dialog — upstream dialogs have no
    /// global timeout; only the runtime wait is bounded.
    queued_deadline: Option<Instant>,
    /// Internal (built-in) dialog: NOT an extension `ctx.ui.*` request — its
    /// reply routes to an in-process waiter (the `/login` API-key dialog, the
    /// interactive code-paste prompt, the `/tree` summarize prompt) instead of
    /// `ExtensionHandle::respond_dialog`.
    pub internal: bool,
    /// Monotonic revision of every render-visible interaction-state change
    /// (select cursor, confirm toggle, editor text/cursor). The body itself is
    /// immutable after construction and each overlay owns its own memo, but
    /// the revision is still the content key: every mutator bumps it, so a
    /// memo hit can never serve bytes from before the change.
    content_revision: u64,
    /// One-entry memo of the framed modal rows ([`DialogRenderMemo`]). The
    /// dock copies rows every frame, but a steady repaint now re-serves the
    /// built `Vec` instead of rebuilding a `Container` plus one `Text` per
    /// option (500+ allocations for a 500-option select).
    render_memo: Mutex<DialogRenderMemo>,
}

impl DialogOverlay {
    /// Parse a `Notification::DialogRequest` payload into an overlay.
    /// Malformed payloads still produce a usable dialog: a select with no
    /// parseable options renders an empty list that only cancels. A positive
    /// `options.timeout` (ms, same key the runtime's per-dialog TTL reads)
    /// arms the countdown + auto-close deadline.
    pub fn from_request(id: u64, kind: &str, options: &JsonValue) -> DialogOverlay {
        let body = Self::parse_body(kind, options);
        let mut overlay = Self::new(id, body);
        let now = Instant::now();
        overlay.deadline = options
            .get("timeout")
            .and_then(JsonValue::as_f64)
            .filter(|ms| ms.is_finite() && *ms > 0.0)
            // `Instant + Duration` panics on overflow; the clamp keeps absurd
            // JS timeouts from aborting the host.
            .map(|ms| now + Duration::from_millis(ms.min(u32::MAX as f64) as u64));
        // Row 6 (W5-AB): without a request-level `timeout` the JS runtime still
        // resolves its waiter with `null` after the global dialog TTL. Stamp
        // that TTL on the queue entry so a request stranded behind a modal is
        // dropped instead of pinning an editor buffer forever. An explicit
        // `timeout` already bounds the queue through `deadline`.
        //
        // NATIVE requests (`id >= NATIVE_DIALOG_ID_BASE`) are NOT JS waiters:
        // their waiter is `NativeDialogWait`, bounded by the same request-level
        // `DialogOptions::timeout` (or not at all), never by the JS runtime's
        // global TTL. Inheriting it would silently drop a queued native dialog
        // whose waiter is still parked, leaving that future waiting forever for
        // a prompt that never surfaces.
        if overlay.deadline.is_none()
            && !crate::plugins::host::NativeDialogCell::is_native_dialog_id(id)
        {
            overlay.queued_deadline = Some(now + crate::extensions::runtime::DIALOG_TIMEOUT);
        }
        overlay
    }

    /// Dialog title for the OSC 7501 blocked status (upstream reports the
    /// open dialog's title as the `blocked` message).
    pub fn blocked_title(&self) -> String {
        match &self.body {
            DialogBody::Select { title, .. }
            | DialogBody::Confirm { title, .. }
            | DialogBody::Input { title, .. }
            | DialogBody::Editor { title, .. } => title.clone(),
        }
    }

    /// Pure payload parsing (select options / input defaults / prefill).
    pub fn parse_body(kind: &str, options: &JsonValue) -> DialogBody {
        match kind {
            "select" => {
                let options_list: Vec<DialogOption> = options
                    .get("options")
                    .and_then(JsonValue::as_array)
                    .map(|list| list.iter().map(DialogOption::from_json).collect())
                    .unwrap_or_default();
                let default = options
                    .get("default")
                    .and_then(JsonValue::as_u64)
                    .map(|index| index as usize)
                    .filter(|index| *index < options_list.len());
                DialogBody::Select {
                    title: title_of(options),
                    options: options_list,
                    default,
                }
            }
            "confirm" => DialogBody::Confirm {
                title: title_of(options),
                message: options
                    .get("message")
                    .and_then(JsonValue::as_str)
                    .unwrap_or_default()
                    .to_string(),
                default_yes: options
                    .get("defaultYes")
                    .and_then(JsonValue::as_bool)
                    .unwrap_or(false),
            },
            "input" => DialogBody::Input {
                title: title_of(options),
                placeholder: options
                    .get("placeholder")
                    .and_then(JsonValue::as_str)
                    .unwrap_or_default()
                    .to_string(),
                default_value: options
                    .get("defaultValue")
                    .or_else(|| options.get("default"))
                    .and_then(JsonValue::as_str)
                    .unwrap_or_default()
                    .to_string(),
            },
            "editor" => DialogBody::Editor {
                title: title_of(options),
                prefill: options
                    .get("prefill")
                    .and_then(JsonValue::as_str)
                    .unwrap_or_default()
                    .to_string(),
            },
            // Unknown kinds cancel immediately upstream too (the reply is
            // `null`); keep a confirm-shaped body so the overlay still
            // renders something honest instead of vanishing.
            _ => DialogBody::Confirm {
                title: title_of(options),
                message: format!("unsupported dialog kind: {kind}"),
                default_yes: false,
            },
        }
    }

    pub fn new(id: u64, body: DialogBody) -> DialogOverlay {
        // Upstream seeds the selector at 0 (`selectedIndex = 0`,
        // `extension-selector.ts:21`); a `default` index in the pir payload
        // pre-moves the clamped cursor.
        let select_index = match &body {
            DialogBody::Select {
                options, default, ..
            } => default.unwrap_or(0).min(options.len().saturating_sub(1)),
            _ => 0,
        };
        let buffer = match &body {
            DialogBody::Input { default_value, .. } => {
                let editor = Editor::new();
                editor.set_text(default_value);
                Some(editor)
            }
            DialogBody::Editor { prefill, .. } => {
                let editor = Editor::new();
                editor.set_text(prefill);
                Some(editor)
            }
            _ => None,
        };
        let confirm_yes = matches!(
            body,
            DialogBody::Confirm {
                default_yes: true,
                ..
            }
        );
        DialogOverlay {
            id,
            body,
            select_index,
            buffer,
            confirm_yes,
            deadline: None,
            queued_deadline: None,
            internal: false,
            content_revision: 0,
            render_memo: Mutex::new(DialogRenderMemo::default()),
        }
    }

    /// Build an INTERNAL dialog (built-in flows, not `ctx.ui.*`): the reply
    /// routes through the TUI's internal-waiter map instead of the extension
    /// reply channel.
    pub fn internal(id: u64, body: DialogBody) -> DialogOverlay {
        let mut overlay = Self::new(id, body);
        overlay.internal = true;
        overlay
    }

    pub fn body(&self) -> &DialogBody {
        &self.body
    }

    /// Whether the dialog's timeout deadline has passed (auto-close sweep).
    pub fn expired(&self, now: Instant) -> bool {
        self.deadline.is_some_and(|deadline| now >= deadline)
    }

    /// Whether a still-QUEUED overlay can never become visible: its
    /// request-level `timeout` or the runtime's global dialog TTL elapsed,
    /// so the extension-side waiter already resolved `null`. Deliberately
    /// separate from [`Self::expired`]: a dialog that is already visible
    /// keeps upstream's no-global-timeout behaviour.
    pub fn queue_expired(&self, now: Instant) -> bool {
        self.expired(now) || self.queued_deadline.is_some_and(|deadline| now >= deadline)
    }

    /// Remaining whole seconds for the title's `(Ns)` suffix; `None` when
    /// the dialog carries no timeout.
    pub fn countdown_seconds_at(&self, now: Instant) -> Option<u64> {
        self.deadline
            .map(|deadline| countdown_seconds(deadline.saturating_duration_since(now)))
    }

    /// The title as rendered at a given countdown second: the base text plus
    /// the live `(Ns)` suffix (upstream `CountdownTimer` `onTick`,
    /// countdown-timer.ts:16-25). `seconds` is the value folded into the render
    /// memo key, so the stored rows and their key can never disagree.
    fn countdown_title(title: &str, seconds: Option<u64>) -> String {
        match seconds {
            Some(seconds) => format!("{title} ({seconds}s)"),
            None => title.to_string(),
        }
    }

    /// Handle one key press (pure state machine). Overlay-capturing: every
    /// key is consumed by the dialog while it is open.
    pub fn handle_key(&mut self, key: DialogKey) -> DialogOutcome {
        // Bump the render-memo revision for exactly the keys that can move a
        // rendered byte. Inert keys (select type-to-filter characters, input
        // `Up`/`Down`/`Newline`) leave the memo intact: they cannot change the
        // output, and a 500-option list must not rebuild on a stray keystroke.
        let changed = match &self.body {
            DialogBody::Select { .. } => matches!(
                key,
                DialogKey::Up | DialogKey::Down | DialogKey::Char('j') | DialogKey::Char('k')
            ),
            DialogBody::Confirm { .. } => matches!(key, DialogKey::Left | DialogKey::Right),
            DialogBody::Input { .. } => !matches!(
                key,
                DialogKey::Enter
                    | DialogKey::Esc
                    | DialogKey::Up
                    | DialogKey::Down
                    | DialogKey::Newline
            ),
            DialogBody::Editor { .. } => !matches!(key, DialogKey::Enter | DialogKey::Esc),
        };
        if changed {
            self.content_revision = self.content_revision.wrapping_add(1);
        }
        match &mut self.body {
            DialogBody::Select { options, .. } => {
                let count = options.len();
                // Clamped cursor (upstream `Math.max(0, i - 1)` /
                // `Math.min(len - 1, i + 1)`, `extension-selector.ts:101-105`).
                let step_up = |index: usize| index.saturating_sub(1);
                let step_down = |index: usize| {
                    if count == 0 {
                        index
                    } else {
                        (index + 1).min(count - 1)
                    }
                };
                match key {
                    DialogKey::Up => {
                        self.select_index = step_up(self.select_index);
                        DialogOutcome::None
                    }
                    DialogKey::Down => {
                        self.select_index = step_down(self.select_index);
                        DialogOutcome::None
                    }
                    // `j`/`k` are navigation aliases upstream
                    // (`extension-selector.ts:100-105`); every other
                    // character is inert (upstream has no type-to-filter).
                    DialogKey::Char('k') => {
                        self.select_index = step_up(self.select_index);
                        DialogOutcome::None
                    }
                    DialogKey::Char('j') => {
                        self.select_index = step_down(self.select_index);
                        DialogOutcome::None
                    }
                    DialogKey::Enter => match options.get(self.select_index) {
                        Some(option) => DialogOutcome::Submit(option.value.clone()),
                        // Empty list: cancel rather than wedge the JS await
                        // (upstream does nothing; a select with no parseable
                        // options only cancels here).
                        None => DialogOutcome::Cancel,
                    },
                    DialogKey::Esc => DialogOutcome::Cancel,
                    DialogKey::Backspace
                    | DialogKey::Newline
                    | DialogKey::Char(_)
                    | DialogKey::Left
                    | DialogKey::Right
                    | DialogKey::Home
                    | DialogKey::End
                    | DialogKey::Delete
                    | DialogKey::WordLeft
                    | DialogKey::WordRight
                    | DialogKey::DeleteWordBackward
                    | DialogKey::DeleteWordForward
                    | DialogKey::DeleteToLineStart
                    | DialogKey::DeleteToLineEnd
                    | DialogKey::Undo
                    | DialogKey::Yank
                    | DialogKey::YankPop => DialogOutcome::None,
                }
            }
            DialogBody::Confirm { .. } => match key {
                DialogKey::Char('y') | DialogKey::Char('Y') => DialogOutcome::Submit(true.into()),
                DialogKey::Char('n') | DialogKey::Char('N') => DialogOutcome::Submit(false.into()),
                DialogKey::Left | DialogKey::Right => {
                    self.confirm_yes = !self.confirm_yes;
                    DialogOutcome::None
                }
                DialogKey::Enter => DialogOutcome::Submit(self.confirm_yes.into()),
                DialogKey::Esc => DialogOutcome::Cancel,
                DialogKey::Up | DialogKey::Down | DialogKey::Backspace => DialogOutcome::None,
                DialogKey::Newline
                | DialogKey::Char(_)
                | DialogKey::Home
                | DialogKey::End
                | DialogKey::Delete
                | DialogKey::WordLeft
                | DialogKey::WordRight
                | DialogKey::DeleteWordBackward
                | DialogKey::DeleteWordForward
                | DialogKey::DeleteToLineStart
                | DialogKey::DeleteToLineEnd
                | DialogKey::Undo
                | DialogKey::Yank
                | DialogKey::YankPop => DialogOutcome::None,
            },
            DialogBody::Input { .. } => {
                let Some(editor) = self.buffer.as_ref() else {
                    return DialogOutcome::None;
                };
                match key {
                    DialogKey::Enter => DialogOutcome::Submit(JsonValue::String(editor.get_text())),
                    DialogKey::Esc => DialogOutcome::Cancel,
                    DialogKey::Left => {
                        editor.move_cursor_left();
                        DialogOutcome::None
                    }
                    DialogKey::Right => {
                        editor.move_cursor_right();
                        DialogOutcome::None
                    }
                    DialogKey::Backspace => {
                        editor.backspace();
                        DialogOutcome::None
                    }
                    DialogKey::Char(c) => {
                        editor.insert_text(&c.to_string());
                        DialogOutcome::None
                    }
                    // Full single-line editing surface (upstream pi-tui
                    // Input's keymap): word motion, kill ring, undo.
                    DialogKey::Home => {
                        editor.move_cursor_home();
                        DialogOutcome::None
                    }
                    DialogKey::End => {
                        editor.move_cursor_end();
                        DialogOutcome::None
                    }
                    DialogKey::Delete => {
                        editor.delete_char_forward();
                        DialogOutcome::None
                    }
                    DialogKey::WordLeft => {
                        editor.move_word_left();
                        DialogOutcome::None
                    }
                    DialogKey::WordRight => {
                        editor.move_word_right();
                        DialogOutcome::None
                    }
                    DialogKey::DeleteWordBackward => {
                        editor.delete_word_backward();
                        DialogOutcome::None
                    }
                    DialogKey::DeleteWordForward => {
                        editor.delete_word_forward();
                        DialogOutcome::None
                    }
                    DialogKey::DeleteToLineStart => {
                        editor.delete_to_line_start();
                        DialogOutcome::None
                    }
                    DialogKey::DeleteToLineEnd => {
                        editor.delete_to_line_end();
                        DialogOutcome::None
                    }
                    DialogKey::Undo => {
                        editor.undo();
                        DialogOutcome::None
                    }
                    DialogKey::Yank => {
                        editor.yank();
                        DialogOutcome::None
                    }
                    DialogKey::YankPop => {
                        editor.yank_pop();
                        DialogOutcome::None
                    }
                    // Single-line input: no vertical movement, no newline.
                    DialogKey::Up | DialogKey::Down | DialogKey::Newline => DialogOutcome::None,
                }
            }
            DialogBody::Editor { .. } => {
                let Some(editor) = self.buffer.as_ref() else {
                    return DialogOutcome::None;
                };
                match key {
                    DialogKey::Enter => {
                        // Enter submits (upstream extension-editor wires the
                        // editor's Enter-submit with Shift+Enter newlines;
                        // the pir dialog editor treats Enter as submit and
                        // Ctrl+J as newline).
                        DialogOutcome::Submit(JsonValue::String(editor.get_text()))
                    }
                    DialogKey::Newline => {
                        editor.newline();
                        DialogOutcome::None
                    }
                    DialogKey::Esc => DialogOutcome::Cancel,
                    DialogKey::Left => {
                        editor.move_cursor_left();
                        DialogOutcome::None
                    }
                    DialogKey::Right => {
                        editor.move_cursor_right();
                        DialogOutcome::None
                    }
                    DialogKey::Up => {
                        editor.move_cursor_up();
                        DialogOutcome::None
                    }
                    DialogKey::Down => {
                        editor.move_cursor_down();
                        DialogOutcome::None
                    }
                    DialogKey::Backspace => {
                        editor.backspace();
                        DialogOutcome::None
                    }
                    DialogKey::Char(c) => {
                        editor.insert_text(&c.to_string());
                        DialogOutcome::None
                    }
                    // Full editor surface (upstream extension-editor embeds
                    // the real Editor component): line/word motion, kill
                    // ring, undo.
                    DialogKey::Home => {
                        editor.move_cursor_home();
                        DialogOutcome::None
                    }
                    DialogKey::End => {
                        editor.move_cursor_end();
                        DialogOutcome::None
                    }
                    DialogKey::Delete => {
                        editor.delete_char_forward();
                        DialogOutcome::None
                    }
                    DialogKey::WordLeft => {
                        editor.move_word_left();
                        DialogOutcome::None
                    }
                    DialogKey::WordRight => {
                        editor.move_word_right();
                        DialogOutcome::None
                    }
                    DialogKey::DeleteWordBackward => {
                        editor.delete_word_backward();
                        DialogOutcome::None
                    }
                    DialogKey::DeleteWordForward => {
                        editor.delete_word_forward();
                        DialogOutcome::None
                    }
                    DialogKey::DeleteToLineStart => {
                        editor.delete_to_line_start();
                        DialogOutcome::None
                    }
                    DialogKey::DeleteToLineEnd => {
                        editor.delete_to_line_end();
                        DialogOutcome::None
                    }
                    DialogKey::Undo => {
                        editor.undo();
                        DialogOutcome::None
                    }
                    DialogKey::Yank => {
                        editor.yank();
                        DialogOutcome::None
                    }
                    DialogKey::YankPop => {
                        editor.yank_pop();
                        DialogOutcome::None
                    }
                }
            }
        }
    }

    /// Multi-line newline for the editor dialog (Ctrl+J in the TUI layer;
    /// exposed here so the modal does not reimplement editor plumbing).
    pub fn editor_newline(&mut self) {
        if matches!(self.body, DialogBody::Editor { .. }) {
            if let Some(editor) = &self.buffer {
                editor.newline();
            }
            self.content_revision = self.content_revision.wrapping_add(1);
        }
    }

    /// Whether this dialog carries a text buffer (input/editor bodies).
    pub fn accepts_text(&self) -> bool {
        matches!(
            self.body,
            DialogBody::Input { .. } | DialogBody::Editor { .. }
        )
    }

    /// Whether this is the multi-line editor dialog (Shift+Enter newline
    /// handling in the TUI layer).
    pub fn is_editor(&self) -> bool {
        matches!(self.body, DialogBody::Editor { .. })
    }

    /// Paste into the dialog's text buffer (newline-normalized, cursor
    /// placed after the insert like the main editor).
    pub fn paste_text(&mut self, text: &str) {
        if let Some(editor) = &self.buffer {
            editor.paste_text(text);
        }
        if self.buffer.is_some() {
            self.content_revision = self.content_revision.wrapping_add(1);
        }
    }

    /// The dialog's buffer text (the external-editor route reads it).
    pub fn text(&self) -> Option<String> {
        self.buffer.as_ref().map(Editor::get_text)
    }

    /// Replace the dialog's buffer text (the external-editor route writes
    /// the round-tripped content back).
    pub fn set_text(&mut self, text: &str) {
        if let Some(editor) = &self.buffer {
            editor.set_text(text);
        }
        if self.buffer.is_some() {
            self.content_revision = self.content_revision.wrapping_add(1);
        }
    }
}

fn title_of(options: &JsonValue) -> String {
    options
        .get("title")
        .and_then(JsonValue::as_str)
        .unwrap_or_default()
        .to_string()
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

/// One hint token, upstream `keyHint` (`keybinding-hints.ts:42-48`): dim key
/// text + muted description.
fn key_hint(key: &str, description: &str) -> String {
    with_theme(|theme| {
        format!(
            "{}{}",
            theme.fg(ThemeColor::Dim, key),
            theme.fg(ThemeColor::Muted, &format!(" {description}"))
        )
    })
}

/// Select footer (upstream `extension-selector.ts:68-78`).
fn select_hint() -> String {
    format!(
        "{}  {}  {}",
        key_hint("↑↓", "navigate"),
        key_hint("enter", "select"),
        key_hint("esc", "cancel"),
    )
}

/// Confirm footer. Upstream reuses the selector hint
/// (`↑↓ navigate  enter select  esc cancel`), but the pir confirm key map
/// submits `y`/`n` directly and toggles with `←/→` — there is no `↑↓`/`j`/`k`
/// navigation in it, so the footer names the real keys instead of claiming
/// dead ones.
fn confirm_hint() -> String {
    format!(
        "{}  {}  {}  (y/n)",
        key_hint("←→", "select"),
        key_hint("enter", "confirm"),
        key_hint("esc", "cancel"),
    )
}

/// Input footer (upstream `extension-input.ts:73-75`; upstream's default
/// cancel binding renders as `escape/ctrl+c`, pir only routes `esc`).
fn input_hint() -> String {
    format!(
        "{}  {}  {}  {}  {}",
        key_hint("enter", "submit"),
        key_hint("ctrl+u/ctrl+k", "delete line"),
        key_hint("ctrl+w", "delete word"),
        key_hint("ctrl+y", "yank"),
        key_hint("esc", "cancel"),
    )
}

/// Editor footer (upstream `extension-editor.ts:92-99`): `ctrl+g` routes the
/// dialog buffer through the external editor (the terminal layer intercepts
/// the `app.editor.external` chord before the dialog key map).
/// `shift+enter/ctrl+j` is upstream's `tui.input.newLine` key text; both
/// bindings reach the pir editor dialog (`shift+enter` at the terminal
/// layer, `ctrl+j` via [`DialogKey::Newline`]).
fn editor_hint() -> String {
    format!(
        "{}  {}  {}  {}",
        key_hint("enter", "submit"),
        key_hint("shift+enter/ctrl+j", "newline"),
        key_hint("ctrl+g", "external editor"),
        key_hint("esc", "cancel"),
    )
}

/// One selector row (upstream `extension-selector.ts:87-92`): the selected
/// row is accent `→ ` + accent label, every other row two plain spaces +
/// text-colored label (the `Text` row applies the 1-cell pad).
fn selector_option(label: &str, selected: bool) -> String {
    if selected {
        with_theme(|theme| {
            format!(
                "{}{}",
                theme.fg(ThemeColor::Accent, "→ "),
                theme.fg(ThemeColor::Accent, label)
            )
        })
    } else {
        with_theme(|theme| format!("  {}", theme.fg(ThemeColor::Text, label)))
    }
}

/// Upstream `sliceByColumn` (pi-tui `utils.ts`): take up to `max_width`
/// cells starting at column `start_col`, splitting whole chars (a wide char
/// straddling the start column is dropped).
fn slice_by_column(text: &str, start_col: usize, max_width: usize) -> String {
    if max_width == 0 {
        return String::new();
    }
    let mut out = String::new();
    let mut column = 0usize;
    let mut used = 0usize;
    for c in text.chars() {
        let char_width = UnicodeWidthChar::width(c).unwrap_or(0);
        if column < start_col {
            column += char_width;
            continue;
        }
        if used + char_width > max_width {
            break;
        }
        used += char_width;
        out.push(c);
        column += char_width;
    }
    out
}

/// Upstream `Input.render` (pi-tui `input.ts:419-500`): a `> ` prompt, the
/// value with a reverse-video cursor cell, right-padded to `width`. The pir
/// dialog keeps its `Editor` as the text store; positions map char-wise
/// (pir has no grapheme-segmented `sliceByColumn` port), and embedded
/// newlines (the pir paste path can insert them; upstream's input strips
/// them at paste time, `input.ts:408`) are omitted from the single display
/// line.
fn render_input_line(editor: &Editor, width: usize) -> String {
    const PROMPT: &str = "> ";
    let state = editor.state_snapshot();
    let mut text = String::new();
    let mut cursor = 0usize;
    for (index, line) in state.lines.iter().enumerate() {
        if index < state.cursor_line {
            cursor += line.chars().count();
        } else if index == state.cursor_line {
            cursor += state.cursor_col.min(line.chars().count());
        }
        text.push_str(line);
    }
    let prompt_width = visible_width(PROMPT);
    let available = width.saturating_sub(prompt_width);
    if available == 0 {
        return truncate_to_width(PROMPT, width, "");
    }
    let chars: Vec<char> = text.chars().collect();
    let cursor = cursor.min(chars.len());
    let total = visible_width(&text);
    let (visible, cursor_display) = if total < available {
        (text.clone(), cursor)
    } else {
        // Scroll so the cursor stays visible (upstream start-column rules,
        // `input.ts:450-475`).
        let scroll = if cursor == chars.len() {
            available.saturating_sub(1)
        } else {
            available
        };
        if scroll == 0 {
            (String::new(), 0)
        } else {
            let before: String = chars[..cursor].iter().collect();
            let cursor_col = visible_width(&before);
            let half = scroll / 2;
            let start = if cursor_col < half {
                0
            } else if cursor_col > total.saturating_sub(half) {
                total.saturating_sub(scroll)
            } else {
                cursor_col.saturating_sub(half)
            };
            let visible = slice_by_column(&text, start, scroll);
            let visible_before = slice_by_column(&text, start, cursor_col.saturating_sub(start));
            let cursor_display = visible_before.chars().count().min(visible.chars().count());
            (visible, cursor_display)
        }
    };
    let visible_chars: Vec<char> = visible.chars().collect();
    let before: String = visible_chars[..cursor_display].iter().collect();
    let after: String = visible_chars[cursor_display..].iter().collect();
    let mut line = format!("{PROMPT}{before}");
    match after.chars().next() {
        Some(first) => {
            let rest: String = after.chars().skip(1).collect();
            line.push_str(&format!("\x1b[7m{first}\x1b[0m{rest}"));
        }
        None => line.push_str("\x1b[7m \x1b[0m"),
    }
    pad_to_width(&line, width)
}

/// DynamicBorder-framed modal body shared by all four dialog kinds:
/// border / blank / title / blank / body / blank / hint / blank / border
/// (upstream `extension-selector.ts:45-80`, `extension-input.ts:49-77`,
/// `extension-editor.ts:66-104`). Title and hint render with a 1-cell pad
/// (`new Text(..., 1, 0)` upstream); `line_pad` is the body rows' own pad
/// (selector options 1, the input `> ` line 0, the nested editor 0).
fn framed_lines(
    title: &str,
    inner: Vec<String>,
    hint: &str,
    line_pad: usize,
    width: usize,
) -> Vec<String> {
    let mut container = Container::new();
    container.add_child(DynamicBorder::new());
    container.add_child(Spacer::new(1));
    container.add_child(Text::with_padding(title.to_string(), 1, 0));
    container.add_child(Spacer::new(1));
    for line in inner {
        container.add_child(Text::with_padding(line, line_pad, 0));
    }
    container.add_child(Spacer::new(1));
    container.add_child(Text::with_padding(hint.to_string(), 1, 0));
    container.add_child(Spacer::new(1));
    container.add_child(DynamicBorder::new());
    container.render(width)
}

/// The render-visible interaction state folded into [`DialogRenderKey`].
/// `selected` / `yes` are the audit key's first-class inputs; `revision`
/// covers everything else that can move (editor text + cursor, and any future
/// body mutation) because every mutator bumps [`DialogOverlay::content_revision`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DialogRenderState {
    Select { selected: usize, revision: u64 },
    Confirm { yes: bool, revision: u64 },
    Input { revision: u64 },
    Editor { revision: u64 },
}

/// The complete render input set of one dialog frame: everything the framed
/// rows' bytes depend on —
/// - `width` and the process-wide theme generation (`with_theme` colors the
///   title, every option row and the hint footer);
/// - the displayed countdown second (`None` without a deadline);
/// - the body kind plus its interaction state (`selected`, `yes`) and the
///   content revision (editor text/cursor).
///
/// The select body renders the full list (no window), so its scroll offset is
/// structurally zero; the nested editor's scroll is derived from the
/// revision-tracked cursor state. The body itself is immutable after
/// construction and every overlay owns its own memo, so no cross-dialog key
/// component is needed for the options.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct DialogRenderKey {
    width: usize,
    theme_generation: u64,
    countdown: Option<u64>,
    state: DialogRenderState,
}

/// One-entry memo of the framed modal rows plus the key they were built
/// under. `render` takes `&self`, so the slot sits behind a `Mutex`; the dock
/// already serializes dialog access through the open-dialog lock, so the
/// extra lock is uncontended. `key = None` forces a rebuild.
#[derive(Default)]
struct DialogRenderMemo {
    key: Option<DialogRenderKey>,
    rows: Vec<String>,
}

impl DialogOverlay {
    /// The memo key for a render at `width` / `countdown` under the current
    /// theme and interaction state.
    fn render_key(&self, width: usize, countdown: Option<u64>) -> DialogRenderKey {
        let state = match &self.body {
            DialogBody::Select { .. } => DialogRenderState::Select {
                selected: self.select_index,
                revision: self.content_revision,
            },
            DialogBody::Confirm { .. } => DialogRenderState::Confirm {
                yes: self.confirm_yes,
                revision: self.content_revision,
            },
            DialogBody::Input { .. } => DialogRenderState::Input {
                revision: self.content_revision,
            },
            DialogBody::Editor { .. } => DialogRenderState::Editor {
                revision: self.content_revision,
            },
        };
        DialogRenderKey {
            width,
            theme_generation: Theme::generation(),
            countdown,
            state,
        }
    }

    /// Build the framed rows — the pre-memo `render` body, unchanged. Only a
    /// memo miss runs this. `countdown` is the second already folded into the
    /// key, so the title bytes and the key can never disagree.
    fn build_rows(&self, width: usize, countdown: Option<u64>) -> Vec<String> {
        match &self.body {
            DialogBody::Select { title, options, .. } => {
                // Full list, no window: upstream renders every option in
                // order (`extension-selector.ts:85-94`).
                let inner: Vec<String> = options
                    .iter()
                    .enumerate()
                    .map(|(index, option)| {
                        selector_option(&option.label, index == self.select_index)
                    })
                    .collect();
                // Bold accent title with the live countdown (upstream
                // `theme.fg("accent", theme.bold(`${baseTitle} (${s}s)`))`,
                // extension-selector.ts:48/58).
                let title = with_theme(|theme| {
                    theme.fg(
                        ThemeColor::Accent,
                        &theme.bold(&Self::countdown_title(title, countdown)),
                    )
                });
                framed_lines(&title, inner, &select_hint(), 1, width)
            }
            DialogBody::Confirm { title, message, .. } => {
                // Upstream `showExtensionConfirm` is a Yes/No selector with a
                // bold-accent `title\nmessage` header
                // (`interactive-mode.ts:2750-2759` →
                // `ExtensionSelectorComponent`); the highlighted row follows
                // the pir Enter default.
                let header = Self::countdown_title(&format!("{title}\n{message}"), countdown);
                let title = with_theme(|theme| theme.fg(ThemeColor::Accent, &theme.bold(&header)));
                let inner = vec![
                    selector_option("Yes", self.confirm_yes),
                    selector_option("No", !self.confirm_yes),
                ];
                framed_lines(&title, inner, &confirm_hint(), 1, width)
            }
            DialogBody::Input { title, .. } => {
                let Some(editor) = self.buffer.as_ref() else {
                    return Vec::new();
                };
                // Upstream `ExtensionInputComponent` ignores the placeholder
                // (`_placeholder`, `extension-input.ts:37,69`) and renders a
                // plain pi-tui `Input` row instead of a bordered editor. The
                // title carries the live countdown (`extension-input.ts:60`).
                let title = with_theme(|theme| {
                    theme.fg(ThemeColor::Accent, &Self::countdown_title(title, countdown))
                });
                framed_lines(
                    &title,
                    vec![render_input_line(editor, width)],
                    &input_hint(),
                    0,
                    width,
                )
            }
            DialogBody::Editor { title, .. } => {
                let Some(editor) = self.buffer.as_ref() else {
                    return Vec::new();
                };
                // The nested editor is upstream-faithful here
                // (`extension-editor.ts:79-87` adds the real `Editor`
                // component); only the title pad and hint change. A payload
                // `timeout` counts down here too so the TUI never outlives
                // its waiter.
                let inner = editor.render(width);
                let title = with_theme(|theme| {
                    theme.fg(ThemeColor::Accent, &Self::countdown_title(title, countdown))
                });
                framed_lines(&title, inner, &editor_hint(), 0, width)
            }
        }
    }
}

impl Component for DialogOverlay {
    fn render(&self, width: usize) -> Vec<String> {
        // One clock read for both the key and the title: the displayed second
        // is exactly the one the rows are built with.
        let countdown = self.countdown_seconds_at(Instant::now());
        let key = self.render_key(width, countdown);
        {
            let memo = self
                .render_memo
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if memo.key.as_ref() == Some(&key) {
                return memo.rows.clone();
            }
        }
        // Build outside the memo lock: `build_rows` takes the theme lock (and
        // the editor's), so the memo lock never nests inside those.
        let rows = self.build_rows(width, countdown);
        let mut memo = self
            .render_memo
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        memo.key = Some(key);
        memo.rows = rows.clone();
        rows
    }

    /// Drop the memo: the next render rebuilds from the current state (the
    /// trait's invalidation contract; state mutators also bump
    /// `content_revision`, which misses on its own).
    fn invalidate(&self) {
        let mut memo = self
            .render_memo
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        memo.key = None;
        memo.rows.clear();
    }
}

/// Map a crossterm key event onto [`DialogKey`] (the TUI layer's only
/// coupling point; `ctrl+j` maps to the editor dialog's newline action via
/// [`DialogKey::Newline`], while the terminal layer routes `shift+enter` to
/// [`DialogOverlay::editor_newline`] for editor dialogs before consulting
/// this mapping — most terminals report shift+enter as plain enter, so
/// `ctrl+j` stays the reliable multi-line key). `ctrl+c` cancels like Esc:
/// the upstream `tui.select.cancel` default is `escape` + `ctrl+c` for every
/// dialog component (keybindings registry), so the mapping translates it to
/// the shared Esc path instead of dropping it.
pub fn dialog_key_from_crossterm(
    code: crossterm::event::KeyCode,
    modifiers: crossterm::event::KeyModifiers,
) -> Option<DialogKey> {
    use crossterm::event::{KeyCode, KeyModifiers};
    let plain = modifiers == KeyModifiers::NONE || modifiers == KeyModifiers::SHIFT;
    let ctrl = modifiers.contains(KeyModifiers::CONTROL);
    let alt = modifiers.contains(KeyModifiers::ALT);
    Some(match code {
        KeyCode::Up => DialogKey::Up,
        KeyCode::Down => DialogKey::Down,
        KeyCode::Left if ctrl || alt => DialogKey::WordLeft,
        KeyCode::Left => DialogKey::Left,
        KeyCode::Right if ctrl || alt => DialogKey::WordRight,
        KeyCode::Right => DialogKey::Right,
        KeyCode::Home => DialogKey::Home,
        KeyCode::End => DialogKey::End,
        KeyCode::Delete if alt => DialogKey::DeleteWordForward,
        KeyCode::Delete => DialogKey::Delete,
        KeyCode::Enter if plain => DialogKey::Enter,
        // Ctrl+C is `tui.select.cancel` (default keys escape + ctrl+c): the
        // modal cancels exactly like Esc.
        KeyCode::Char('c') if ctrl => DialogKey::Esc,
        // Ctrl+J is the newline binding: most terminals report Shift+Enter as
        // plain Enter (submit), so Ctrl+J is the reliable multi-line input.
        KeyCode::Char('j') if ctrl => DialogKey::Newline,
        KeyCode::Esc => DialogKey::Esc,
        KeyCode::Backspace if alt => DialogKey::DeleteWordBackward,
        KeyCode::Backspace => DialogKey::Backspace,
        KeyCode::Char('w') if ctrl => DialogKey::DeleteWordBackward,
        KeyCode::Char('d') if alt => DialogKey::DeleteWordForward,
        KeyCode::Char('u') if ctrl => DialogKey::DeleteToLineStart,
        KeyCode::Char('k') if ctrl => DialogKey::DeleteToLineEnd,
        KeyCode::Char('b') if alt => DialogKey::WordLeft,
        KeyCode::Char('f') if alt => DialogKey::WordRight,
        KeyCode::Char('y') if ctrl => DialogKey::Yank,
        KeyCode::Char('y') if alt => DialogKey::YankPop,
        KeyCode::Char('-') if ctrl => DialogKey::Undo,
        KeyCode::Char(c) if plain && !ctrl => DialogKey::Char(c),
        _ => return None,
    })
}
