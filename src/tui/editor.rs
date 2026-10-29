//! Editor component — port of `pi-tui`'s `components/editor.ts` (the chat
//! input): bordered multiline input with word-wrapped layout, inverted
//! cursor, scroll borders (`↑ N more`), editor history, and the
//! thinking/bash border colors.
//!
//! Upstream geometry: max visible lines = max(5, terminal rows × 30%);
//! top/bottom borders are full-width `─` runs that become
//! `─── ↑ N more ───`-style scroll indicators; the cursor replaces the
//! grapheme under it with SGR 7 (inverse).

// Pre-existing baseline lints (tui is frozen for the parity batch).
#![allow(clippy::identity_op)]
#![allow(clippy::unnecessary_cast)]

use std::borrow::Cow;
use std::collections::VecDeque;
use std::sync::Mutex;

use unicode_segmentation::UnicodeSegmentation;

use super::ansi::{grapheme_spans, grapheme_width, truncate_to_width, visible_width};
use super::core::{Component, Rows};
use super::theme::{with_theme, ThemeColor};

/// One wrapped layout line: text + cursor position in visible columns.
/// The text borrows the editor state whenever the logical line occupies one
/// layout line; the layout always lives inside the render call that built it,
/// so the common case costs no per-frame `String` copy.
#[derive(Debug, Clone, PartialEq, Eq)]
struct LayoutLine<'a> {
    text: Cow<'a, str>,
    cursor_pos: Option<usize>,
}

/// Editor state (lines + cursor), matching the upstream `EditorState`.
#[derive(Debug, Clone)]
pub struct EditorState {
    pub lines: Vec<String>,
    pub cursor_line: usize,
    pub cursor_col: usize,
}

impl Default for EditorState {
    fn default() -> Self {
        Self {
            lines: vec![String::new()],
            cursor_line: 0,
            cursor_col: 0,
        }
    }
}

/// What the last editing action was — port of upstream `lastAction`
/// (`editor.ts:352`): consecutive kills accumulate into a single kill-ring
/// entry, yank-pop only fires immediately after a yank, and consecutive
/// word characters coalesce into one undo unit (fish-style).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LastAction {
    Kill,
    Yank,
    TypeWord,
}

/// Large-paste folding thresholds (upstream `handlePaste`,
/// `editor.ts:1298-1316`): more than 10 lines or more than 1000 characters
/// fold into a `[paste #N …]` marker.
const PASTE_FOLD_MAX_LINES: usize = 10;
const PASTE_FOLD_MAX_CHARS: usize = 1000;

/// Upper bound for the backward scan that locates a paste marker ending at
/// the cursor (`[paste #` + id digits + optional suffix + `]`).
const PASTE_MARKER_MAX_LEN: usize = 64;

/// A registered large paste (upstream `pastes: Map<number, string>` +
/// `pasteCounter`). Inserted text keeps a `[paste #N +L lines]` /
/// `[paste #N C chars]` marker; [`Editor::expanded_text`] and submit
/// expand markers back to the stored content.
#[derive(Debug, Clone, Default)]
struct PasteRegistry {
    /// Folded payloads are shared: an undo snapshot clones the MAP (one
    /// pointer per paste), never the up-to-`MAX_PASTE_BYTES` text, and a
    /// mutating edit pushes one snapshot per word. Cloning the payloads made
    /// a single large paste cost megabytes per keystroke.
    pastes: std::collections::BTreeMap<usize, std::sync::Arc<str>>,
    paste_counter: usize,
}

/// Undo snapshot — port of upstream `EditorSnapshot`
/// (`editor.ts:225-230`): editor text state plus the paste registry, so
/// undo restores folded markers and their ids atomically.
#[derive(Debug, Clone)]
struct EditorSnapshot {
    state: EditorState,
    pastes: std::collections::BTreeMap<usize, std::sync::Arc<str>>,
    paste_counter: usize,
}

/// One wrapped layout chunk with its char range in the logical line
/// (upstream `TextChunk`, `editor.ts:107-111`).
#[derive(Debug, Clone, PartialEq, Eq)]
struct TextChunk {
    start: usize,
    /// Char index one past the chunk end.
    end: usize,
}

/// One entry of the visual-line map (upstream
/// `buildVisualLineMap`, `editor.ts:1836-1861`): a wrapped chunk of one
/// logical line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct VisualLine {
    logical_line: usize,
    start_col: usize,
    length: usize,
}

/// A parsed `[paste #N ( +L lines | C chars)?]` marker occurrence.
#[derive(Debug, Clone, PartialEq, Eq)]
struct PasteMarker {
    id: usize,
    /// Total char length including the brackets.
    len: usize,
    /// The optional suffix between the id digits and `]` (e.g.
    /// ` +5 lines`), preserved verbatim through renumbering.
    suffix: String,
}

/// The editor component.
pub struct Editor {
    state: Mutex<EditorState>,
    /// Border SGR prefix (thinking/bash colored; set by the app).
    border_color: Mutex<Option<String>>,
    /// Submitted prompts, NEWEST-FIRST (upstream `history.unshift`,
    /// `editor.ts:433`).
    history: Mutex<Vec<String>>,
    /// Browse index into [`Self::history`]: `None` = not browsing
    /// (upstream `historyIndex === -1`), `Some(0)` = most recent.
    history_index: Mutex<Option<usize>>,
    /// Draft snapshot captured when browsing starts; restored when
    /// navigating past the newest entry (upstream `historyDraft`,
    /// `editor.ts:348 / 464-483`).
    history_draft: Mutex<Option<EditorState>>,
    padding_x: std::sync::atomic::AtomicUsize,
    /// Submit handler text snapshot (the app reads it on Enter).
    submitted: Mutex<Option<String>>,
    /// Emacs-style kill ring — port of upstream `KillRing`
    /// (`kill-ring.ts`): killed texts in insertion order, unbounded;
    /// consecutive kills accumulate into the newest entry.
    kill_ring: Mutex<VecDeque<String>>,
    /// What the previous editing action was (upstream `lastAction`,
    /// `editor.ts:352`).
    last_action: Mutex<Option<LastAction>>,
    /// Undo snapshots — port of upstream `UndoStack<EditorSnapshot>`
    /// (`undo-stack.ts`): a full state clone (including the paste registry)
    /// pushed before every mutating operation; `undo()` pops and restores.
    /// Upstream keeps no redo stack (`tui.editor.undo` is the only registered
    /// id) and no size cap, so pir does the same.
    undo_stack: Mutex<Vec<EditorSnapshot>>,
    /// Folded large pastes (upstream `pastes` + `pasteCounter`,
    /// `editor.ts:338-339`).
    paste_registry: Mutex<PasteRegistry>,
    /// Layout width recorded by the last render (upstream `lastWidth`,
    /// `editor.ts:312`); the wrap layout for vertical movement and the
    /// visual-line map. Defaults to upstream's initial 80.
    last_width: std::sync::atomic::AtomicUsize,
    /// Preferred visual column for vertical cursor movement (sticky column,
    /// upstream `preferredVisualCol`, `editor.ts:358`).
    preferred_visual_col: Mutex<Option<usize>>,
    /// Pre-snap cursor column when the cursor was snapped to the start of an
    /// atomic segment, e.g. a paste marker (upstream `snappedFromCursorCol`,
    /// `editor.ts:365`).
    snapped_from_cursor_col: Mutex<Option<usize>>,
    /// `showHardwareCursor`: with the terminal cursor visible at the editor
    /// cell, the reverse-video fake cursor is dropped (upstream
    /// `resolveFakeCursors` keeps the marked cell plain when the hardware
    /// cursor is shown, tui.ts:1478-1484).
    show_hardware_cursor: std::sync::atomic::AtomicBool,
    /// Awaiting-character mode of `tui.editor.jumpForward` /
    /// `tui.editor.jumpBackward` (upstream `jumpMode`, `editor.ts:699-718`):
    /// `Some(true)` = forward, `Some(false)` = backward, `None` = inactive.
    /// The next printable key jumps; any control key cancels and falls
    /// through.
    jump_mode: Mutex<Option<bool>>,
}

impl Editor {
    pub fn new() -> Self {
        Self {
            state: Mutex::new(EditorState::default()),
            border_color: Mutex::new(None),
            history: Mutex::new(Vec::new()),
            history_index: Mutex::new(None),
            history_draft: Mutex::new(None),
            padding_x: std::sync::atomic::AtomicUsize::new(1),
            submitted: Mutex::new(None),
            kill_ring: Mutex::new(VecDeque::new()),
            last_action: Mutex::new(None),
            undo_stack: Mutex::new(Vec::new()),
            paste_registry: Mutex::new(PasteRegistry::default()),
            last_width: std::sync::atomic::AtomicUsize::new(80),
            preferred_visual_col: Mutex::new(None),
            snapped_from_cursor_col: Mutex::new(None),
            show_hardware_cursor: std::sync::atomic::AtomicBool::new(false),
            jump_mode: Mutex::new(None),
        }
    }

    /// `showHardwareCursor` (settings / `PI_HARDWARE_CURSOR=1`): when true
    /// the terminal cursor is drawn at the editor cell, so the reverse-video
    /// fake cursor is suppressed (the cell keeps its layout).
    pub fn set_show_hardware_cursor(&self, show: bool) {
        self.show_hardware_cursor
            .store(show, std::sync::atomic::Ordering::Relaxed);
    }

    pub fn set_border_color(&self, color: String) {
        *self
            .border_color
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(color);
    }

    /// `editorPaddingX` (upstream `getEditorPaddingX`, clamped 0..=3 by the
    /// settings writer): the horizontal inset of the editor text. pir's
    /// baseline inset is 1, so the effective inset is `1 + setting` — the
    /// default `0` reproduces today's look.
    pub fn set_padding_x(&self, padding_x: usize) {
        self.padding_x
            .store(padding_x, std::sync::atomic::Ordering::Relaxed);
    }

    fn border(&self) -> String {
        let custom = self
            .border_color
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        match custom {
            Some(prefix) => prefix,
            None => with_theme(|theme| theme.fg(ThemeColor::BorderMuted, "")),
        }
    }

    pub fn get_text(&self) -> String {
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.lines.join("\n")
    }

    pub fn set_text(&self, text: &str) {
        // Programmatic changes are undoable when the content actually
        // differs (upstream `setText`, `editor.ts:1115-1127`).
        if self.get_text() != text {
            self.push_undo_snapshot();
        }
        self.set_last_action(None);
        // Upstream `setText` drops the paste registry (editor.ts:1124-1125).
        *self
            .paste_registry
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = PasteRegistry::default();
        self.clear_sticky_columns();
        self.set_text_placement(text, false);
    }

    /// Placement-aware text replacement — port of upstream `setTextInternal`
    /// (`editor.ts:493-508`): `at_start` puts the cursor at the start of the
    /// first line (history restore going Up), otherwise at the end of the
    /// last line (default / Down).
    fn set_text_placement(&self, text: &str, at_start: bool) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.lines = text.split('\n').map(str::to_string).collect();
        if at_start {
            state.cursor_line = 0;
            state.cursor_col = 0;
        } else {
            state.cursor_line = state.lines.len().saturating_sub(1);
            state.cursor_col = state
                .lines
                .last()
                .map(|line| line.chars().count())
                .unwrap_or(0);
        }
    }

    pub fn clear(&self) {
        self.set_text("");
    }

    /// Snapshot of the editor state (lines + cursor) for autocomplete
    /// providers.
    pub fn state_snapshot(&self) -> EditorState {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Replace the whole state with an externally computed result (used by
    /// autocomplete `applyCompletion`): lines, cursor line and cursor column.
    /// Upstream's `applyCompletion` callers push an undo snapshot first
    /// (`editor.ts:772-783, 2244-2255, 2395-2406`), so the replacement is
    /// undoable.
    pub fn apply_state(&self, lines: Vec<String>, cursor_line: usize, cursor_col: usize) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if lines.is_empty() {
            return;
        }
        self.push_undo(&state);
        state.lines = lines;
        state.cursor_line = cursor_line.min(state.lines.len() - 1);
        state.cursor_col = Self::normalize_col(&state, state.cursor_line, cursor_col);
        self.clear_sticky_columns();
    }

    /// Insert pasted text at the cursor after cleaning it — port of upstream
    /// `handlePaste` (`editor.ts:1260-1326`).
    ///
    /// Cleaning order matches upstream: decode tmux CSI-u control
    /// re-encodings (:1267-1277), normalize line endings + expand tabs to
    /// 4 spaces (`normalizeText`, :1148-1150), then keep only `\n` and
    /// characters `>= 0x20` (:1283-1286) so C0 controls (incl. ESC `\x1b`)
    /// can never reach the terminal.
    ///
    /// A paste starting with a path-like character (`/`, `~`, `.`) right
    /// after a word character gains a leading space (`editor.ts:1290-1296`).
    ///
    /// Pastes above the fold thresholds ([`PASTE_FOLD_MAX_LINES`] /
    /// [`PASTE_FOLD_MAX_CHARS`]) are stored in the paste registry and insert
    /// a `[paste #N +L lines]` / `[paste #N C chars]` marker
    /// (:1298-1316); submit and [`Editor::expanded_text`] expand the
    /// markers back to the full content (`expandPasteMarkers`,
    /// `editor.ts:1090-1097`), and backspace into a marker deletes it whole
    /// with registry cleanup and renumbering (:1395-1419).
    ///
    /// Returns the cleaned text actually inserted (for status reporting).
    pub fn paste_text(&self, text: &str) -> String {
        self.set_last_action(None);
        self.clear_sticky_columns();
        self.push_undo_snapshot();
        let mut filtered = Self::clean_paste_text(text);

        // Path-like paste after a word character gets a leading space for
        // readability (upstream `editor.ts:1290-1296`).
        if filtered.starts_with(['/', '~', '.']) {
            let word_before_cursor = {
                let state = self
                    .state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                let col = Self::normalize_col(&state, state.cursor_line, state.cursor_col);
                col > 0
                    && state.lines.get(state.cursor_line).is_some_and(|line| {
                        line.chars()
                            .nth(col - 1)
                            .is_some_and(|c| c.is_ascii_alphanumeric() || c == '_')
                    })
            };
            if word_before_cursor {
                filtered.insert(0, ' ');
            }
        }

        let pasted_lines = filtered.split('\n').count();
        let total_chars = filtered.chars().count();
        if pasted_lines > PASTE_FOLD_MAX_LINES || total_chars > PASTE_FOLD_MAX_CHARS {
            // Store the paste and insert a marker (upstream
            // `editor.ts:1304-1315`).
            let paste_id = {
                let mut registry = self
                    .paste_registry
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                registry.paste_counter += 1;
                registry.paste_counter
            };
            self.paste_registry
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .pastes
                .insert(paste_id, std::sync::Arc::from(filtered.as_str()));
            let marker = if pasted_lines > PASTE_FOLD_MAX_LINES {
                format!("[paste #{paste_id} +{pasted_lines} lines]")
            } else {
                format!("[paste #{paste_id} {total_chars} chars]")
            };
            self.insert_text_inner(&marker);
            return filtered;
        }

        self.insert_text_inner(&filtered);
        filtered
    }

    /// Paste cleaning pipeline — upstream `handlePaste` (:1267-1286).
    fn clean_paste_text(text: &str) -> String {
        let decoded = Self::decode_csi_u_controls(text);
        let normalized = super::clipboard::normalize_newlines(&decoded).replace('\t', "    ");
        normalized
            .chars()
            .filter(|character| *character == '\n' || (*character as u32) >= 0x20)
            .collect()
    }

    /// Expand `[paste #N …]` markers back to their stored content (port of
    /// upstream `expandPasteMarkers`, `editor.ts:1090-1097`); markers whose
    /// id is not registered are left untouched.
    pub fn expanded_text(&self) -> String {
        let text = self.get_text();
        let registry = self
            .paste_registry
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if registry.pastes.is_empty() || !text.contains("[paste #") {
            return text;
        }
        drop(registry);
        Self::expand_paste_markers(&text, &self.paste_registry_snapshot().pastes)
    }

    fn expand_paste_markers(
        text: &str,
        pastes: &std::collections::BTreeMap<usize, std::sync::Arc<str>>,
    ) -> String {
        let chars: Vec<char> = text.chars().collect();
        let mut out = String::with_capacity(text.len());
        let mut index = 0;
        while index < chars.len() {
            if chars[index] == '[' {
                if let Some(marker) = Self::parse_paste_marker(&chars, index) {
                    if let Some(content) = pastes.get(&marker.id) {
                        out.push_str(content);
                        index += marker.len;
                        continue;
                    }
                }
            }
            out.push(chars[index]);
            index += 1;
        }
        out
    }

    /// Decode CSI-u re-encoded control bytes (`ESC [ <codepoint> ; 5 u`)
    /// back to their literal control character — port of the decode step in
    /// upstream `handlePaste` (`editor.ts:1267-1277`). Some terminals (tmux
    /// with `extended-keys-format=csi-u`) re-encode control bytes inside
    /// bracketed paste; decoding first keeps newlines intact instead of
    /// letting the `>= 0x20` filter strip the ESC and leak the printable
    /// tail (e.g. "[106;5u") into the editor.
    fn decode_csi_u_controls(text: &str) -> String {
        let chars: Vec<char> = text.chars().collect();
        let mut decoded = String::with_capacity(text.len());
        let mut index = 0;
        while index < chars.len() {
            if chars[index] == '\x1b' && chars.get(index + 1) == Some(&'[') {
                let digits_start = index + 2;
                let mut end = digits_start;
                while end < chars.len() && chars[end].is_ascii_digit() {
                    end += 1;
                }
                let is_csi_u = end > digits_start
                    && end + 2 < chars.len()
                    && chars[end] == ';'
                    && chars[end + 1] == '5'
                    && chars[end + 2] == 'u';
                if is_csi_u {
                    let codepoint = chars[digits_start..end].iter().fold(0u32, |acc, digit| {
                        acc.saturating_mul(10)
                            .saturating_add(*digit as u32 - '0' as u32)
                    });
                    let control = match codepoint {
                        97..=122 => Some((codepoint - 96) as u8 as char),
                        65..=90 => Some((codepoint - 64) as u8 as char),
                        _ => None,
                    };
                    if let Some(control) = control {
                        decoded.push(control);
                        index = end + 3;
                        continue;
                    }
                }
            }
            decoded.push(chars[index]);
            index += 1;
        }
        decoded
    }

    fn normalize_col(state: &EditorState, line_index: usize, col: usize) -> usize {
        state
            .lines
            .get(line_index)
            .map(|line| col.min(line.chars().count()))
            .unwrap_or(0)
    }

    /// Parse a paste marker starting at `chars[index] == '['` — port of the
    /// upstream `PASTE_MARKER_REGEX` shape `\[paste #(\d+)( (\+\d+ lines|\d+
    /// chars))?\]` (`editor.ts:32`).
    fn parse_paste_marker(chars: &[char], index: usize) -> Option<PasteMarker> {
        const HEAD: &str = "[paste #";
        if chars.len() < index + HEAD.len() + 2 {
            return None;
        }
        if chars[index..index + HEAD.len()]
            .iter()
            .copied()
            .ne(HEAD.chars())
        {
            return None;
        }
        let mut cursor = index + HEAD.len();
        let digits_start = cursor;
        while cursor < chars.len() && chars[cursor].is_ascii_digit() {
            cursor += 1;
        }
        if cursor == digits_start {
            return None;
        }
        let id = chars[digits_start..cursor]
            .iter()
            .fold(0usize, |acc, digit| {
                acc.saturating_mul(10)
                    .saturating_add((*digit as usize) - ('0' as usize))
            });
        let mut suffix = String::new();
        if chars.get(cursor) == Some(&' ') {
            let mut probe = cursor + 1;
            if chars.get(probe) == Some(&'+') {
                probe += 1;
            }
            let count_start = probe;
            while probe < chars.len() && chars[probe].is_ascii_digit() {
                probe += 1;
            }
            if probe == count_start {
                return None;
            }
            let unit: String = chars[probe..(probe + 6).min(chars.len())].iter().collect();
            if unit.starts_with(" lines") {
                probe += " lines".len();
            } else if unit.starts_with(" chars") {
                probe += " chars".len();
            } else {
                return None;
            }
            suffix = chars[cursor..probe].iter().collect();
            cursor = probe;
        }
        if chars.get(cursor) != Some(&']') {
            return None;
        }
        Some(PasteMarker {
            id,
            len: cursor + 1 - index,
            suffix,
        })
    }

    /// The paste marker, if any, whose closing `]` sits exactly at `col` on
    /// this char slice AND whose id is registered (upstream detects the
    /// whole-marker case via marker-aware segmentation, `editor.ts:1392-1395`).
    fn paste_marker_ending_at(
        chars: &[char],
        col: usize,
        valid_ids: &[usize],
    ) -> Option<PasteMarker> {
        if col == 0 || chars.get(col - 1) != Some(&']') {
            return None;
        }
        let scan_start = col.saturating_sub(PASTE_MARKER_MAX_LEN);
        for start in (scan_start..col).rev() {
            if chars[start] != '[' {
                continue;
            }
            if let Some(marker) = Self::parse_paste_marker(chars, start) {
                if start + marker.len == col && valid_ids.contains(&marker.id) {
                    return Some(marker);
                }
            }
        }
        None
    }

    /// Snapshot of the paste registry (for undo snapshots and tests).
    fn paste_registry_snapshot(&self) -> PasteRegistry {
        self.paste_registry
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Ids of the currently registered pastes, ascending (upstream
    /// `validPasteIds`, `editor.ts:385-387`).
    fn valid_paste_ids(&self) -> Vec<usize> {
        self.paste_registry
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .pastes
            .keys()
            .copied()
            .collect()
    }

    /// Remove one registered paste and renumber the higher ids down, in both
    /// the registry and every `[paste #N …]` marker in the text — port of
    /// upstream `handleBackspace`'s marker branch (`editor.ts:1397-1419`).
    fn remove_paste_and_renumber(&self, target_id: usize) {
        {
            let mut registry = self
                .paste_registry
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            registry.pastes.remove(&target_id);
            // Upstream decrements the counter so the freed id is reused.
            registry.paste_counter = registry.paste_counter.saturating_sub(1);
            let higher_ids: Vec<usize> = registry
                .pastes
                .keys()
                .copied()
                .filter(|id| *id > target_id)
                .collect();
            for id in higher_ids {
                if let Some(content) = registry.pastes.remove(&id) {
                    registry.pastes.insert(id - 1, content);
                }
            }
        }
        // Renumber markers with ids greater than the removed one, in every
        // line; the optional suffix is preserved (upstream replaces via
        // `PASTE_MARKER_REGEX` regardless of registry membership).
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for line in &mut state.lines {
            let chars: Vec<char> = line.chars().collect();
            if !chars.contains(&'[') {
                continue;
            }
            let mut out = String::with_capacity(line.len());
            let mut index = 0;
            while index < chars.len() {
                if chars[index] == '[' {
                    if let Some(marker) = Self::parse_paste_marker(&chars, index) {
                        if marker.id > target_id {
                            out.push_str(&format!("[paste #{}{}]", marker.id - 1, marker.suffix));
                            index += marker.len;
                            continue;
                        }
                    }
                }
                out.push(chars[index]);
                index += 1;
            }
            *line = out;
        }
    }

    /// Insert `text` at the cursor (newlines split lines). One undo
    /// snapshot for the whole insertion (upstream `handlePaste` /
    /// `addNewLine` push a single snapshot before the splice).
    pub fn insert_text(&self, text: &str) {
        if text.is_empty() {
            return;
        }
        self.push_undo_snapshot();
        self.set_last_action(None);
        self.insert_text_inner(text);
    }

    /// Type a single character — port of upstream `insertCharacter`
    /// (`editor.ts:1201-1214`) with fish-style undo coalescing: consecutive
    /// word characters coalesce into one undo unit; a whitespace character
    /// captures the state before itself (so undo removes the space plus the
    /// word typed after it together).
    pub fn insert_char(&self, c: char) {
        if c.is_whitespace() || self.last_action() != Some(LastAction::TypeWord) {
            self.push_undo_snapshot();
        }
        self.set_last_action(Some(LastAction::TypeWord));
        self.insert_text_inner(&c.to_string());
    }

    /// Insertion without undo bookkeeping — callers push their own snapshot
    /// (upstream `insertTextAtCursorInternal`, `editor.ts:1157-1198`).
    fn insert_text_inner(&self, text: &str) {
        if text.is_empty() {
            return;
        }
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        self.clear_sticky_columns();
        let cursor_line = state.cursor_line;
        let col = Self::normalize_col(&state, cursor_line, state.cursor_col);

        // Splice into the line vector IN PLACE: the previous shape rebuilt
        // `state.lines` with `.to_vec()` plus a clone of every tail line, so
        // every typed character copied the whole draft (O(n²) bytes over a
        // long prompt). Only the edited line's bytes are copied now; the
        // untouched lines are moved within the same allocation.
        let parts: Vec<&str> = text.split('\n').collect();
        if parts.len() == 1 {
            // Single piece: splice into the current line, cursor after it.
            let only = parts[0];
            let new_line = {
                let line = &state.lines[cursor_line];
                let char_index = line
                    .char_indices()
                    .nth(col)
                    .map(|(i, _)| i)
                    .unwrap_or(line.len());
                let (before, after) = line.split_at(char_index);
                format!("{before}{only}{after}")
            };
            state.lines[cursor_line] = new_line;
            state.cursor_col = col + only.chars().count();
        } else {
            // Multi-piece: first joins the current line, the last piece
            // joins the remainder, middle pieces become their own lines
            // (upstream `insertText`).
            let last_index = parts.len() - 1;
            let (first, last) = {
                let line = &state.lines[cursor_line];
                let char_index = line
                    .char_indices()
                    .nth(col)
                    .map(|(i, _)| i)
                    .unwrap_or(line.len());
                let (before, after) = line.split_at(char_index);
                (
                    format!("{before}{}", parts[0]),
                    format!("{}{after}", parts[last_index]),
                )
            };
            let middle = parts[1..last_index].iter().map(|part| (*part).to_string());
            state.lines.splice(
                cursor_line..=cursor_line,
                std::iter::once(first)
                    .chain(middle)
                    .chain(std::iter::once(last)),
            );
            state.cursor_line = cursor_line + last_index;
            state.cursor_col = parts[last_index].chars().count();
        }
    }

    pub fn newline(&self) {
        self.insert_text("\n");
    }

    pub fn backspace(&self) {
        self.set_last_action(None);
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        self.clear_sticky_columns();
        let col = Self::normalize_col(&state, state.cursor_line, state.cursor_col);
        if col > 0 {
            self.push_undo(&state);
            let line = state.lines[state.cursor_line].clone();
            let chars: Vec<char> = line.chars().collect();
            // Marker-aware backspace (upstream `editor.ts:1392-1428`): a
            // registered paste marker ending exactly at the cursor is
            // deleted whole, removing its registry entry and renumbering the
            // markers above it.
            if let Some(marker) = Self::paste_marker_ending_at(&chars, col, &self.valid_paste_ids())
            {
                let keep: String = chars[..col - marker.len].iter().collect();
                let rest: String = chars[col..].iter().collect();
                let cursor_line = state.cursor_line;
                state.lines[cursor_line] = format!("{keep}{rest}");
                state.cursor_col = col - marker.len;
                drop(state);
                self.remove_paste_and_renumber(marker.id);
                return;
            }
            let cursor_line = state.cursor_line;
            // Delete the grapheme cluster before the cursor (upstream
            // `handleBackspace`: `segment(beforeCursor, "grapheme")` last
            // segment, `editor.ts:1392-1400`).
            let step = Self::backward_step_len(&chars, col, &self.valid_paste_ids()).min(col);
            let keep: String = chars[..col - step].iter().collect();
            let rest: String = chars[col..].iter().collect();
            state.lines[cursor_line] = format!("{keep}{rest}");
            state.cursor_col = col - step;
        } else if state.cursor_line > 0 {
            self.push_undo(&state);
            // Upstream merges into `lines[cursorLine - 1]` BEFORE splicing
            // the current line (editor.ts:1432-1441); removing first makes
            // `lines[cursor_line]` the line AFTER the join point (and out of
            // bounds when the removed line was the last one).
            let cursor_line = state.cursor_line;
            let current = state.lines[cursor_line].clone();
            let prev_len = state.lines[cursor_line - 1].chars().count();
            state.lines[cursor_line - 1].push_str(&current);
            state.lines.remove(cursor_line);
            state.cursor_line -= 1;
            state.cursor_col = prev_len;
        }
    }

    /// Move left one unit — port of upstream `moveCursor(0, -1)`
    /// (`editor.ts:1928-1940`): one grapheme step, with registered paste
    /// markers as atomic units; at column 0 the previous line is entered at
    /// its end. Any horizontal move clears the sticky column.
    pub fn move_cursor_left(&self) {
        // Upstream `moveCursor` clears the kill/yank accumulation context
        // (`editor.ts:1895`).
        self.set_last_action(None);
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let col = Self::normalize_col(&state, state.cursor_line, state.cursor_col);
        if col > 0 {
            let line = state.lines[state.cursor_line].clone();
            let chars: Vec<char> = line.chars().collect();
            let step = Self::backward_step_len(&chars, col, &self.valid_paste_ids());
            state.cursor_col = col - step;
            self.clear_sticky_columns();
        } else if state.cursor_line > 0 {
            state.cursor_line -= 1;
            state.cursor_col = state.lines[state.cursor_line].chars().count();
            self.clear_sticky_columns();
        }
    }

    /// Move right one unit — port of upstream `moveCursor(0, 1)`
    /// (`editor.ts:1907-1927`): one grapheme step with markers atomic; at
    /// the end of the line the next line is entered; at the very end of the
    /// editor the visual column is remembered as the sticky preferred
    /// column for later vertical moves (`editor.ts:1921-1927`).
    pub fn move_cursor_right(&self) {
        self.set_last_action(None);
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let line_len = state.lines[state.cursor_line].chars().count();
        let col = Self::normalize_col(&state, state.cursor_line, state.cursor_col);
        if col < line_len {
            let line = state.lines[state.cursor_line].clone();
            let chars: Vec<char> = line.chars().collect();
            let step = Self::forward_step_len(&chars, col, &self.valid_paste_ids());
            state.cursor_col = col + step;
            self.clear_sticky_columns();
        } else if state.cursor_line + 1 < state.lines.len() {
            state.cursor_line += 1;
            state.cursor_col = 0;
            self.clear_sticky_columns();
        } else {
            // At the end of the last line — remember the visual column
            // (upstream `editor.ts:1921-1927`).
            let visual_lines = self.build_visual_line_map(&state, self.last_width());
            let current = find_current_visual_line(&visual_lines, &state);
            if let Some(vl) = visual_lines.get(current) {
                *self
                    .preferred_visual_col
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) =
                    Some(state.cursor_col - vl.start_col);
            }
        }
    }

    /// Unit length when stepping backward from `col` (one grapheme cluster,
    /// or a whole registered paste marker ending at `col`) — upstream
    /// `handleBackspace` / `moveCursor(0, -1)` segment the text before the
    /// cursor and take the last grapheme (`editor.ts:1392-1400`,
    /// `1928-1940`).
    fn backward_step_len(chars: &[char], col: usize, valid_ids: &[usize]) -> usize {
        if let Some(marker) = Self::paste_marker_ending_at(chars, col, valid_ids) {
            return marker.len;
        }
        Self::grapheme_last_step_len(&chars[..col.min(chars.len())]).unwrap_or(1)
    }

    /// Unit length when stepping forward from `col` (one grapheme cluster,
    /// or a whole registered paste marker starting at `col`) — upstream
    /// `handleForwardDelete` / `moveCursor(0, 1)` (`editor.ts:1779-1802`,
    /// `1907-1927`).
    fn forward_step_len(chars: &[char], col: usize, valid_ids: &[usize]) -> usize {
        if chars.get(col) == Some(&'[') {
            if let Some(marker) = Self::parse_paste_marker(chars, col) {
                if valid_ids.contains(&marker.id) {
                    return marker.len;
                }
            }
        }
        Self::grapheme_step_len(chars.get(col..).unwrap_or(&[])).unwrap_or(1)
    }

    /// Length in chars of the first grapheme cluster of `chars` (`None` when
    /// empty) — the `Intl.Segmenter` unit upstream moves and deletes by.
    fn grapheme_step_len(chars: &[char]) -> Option<usize> {
        let text: String = chars.iter().collect();
        grapheme_spans(&text).first().map(|&(_, len)| len)
    }

    /// Length in chars of the LAST grapheme cluster of `chars` (`None` when
    /// empty) — the unit stepping backward segments the text before the
    /// cursor and takes its final grapheme (upstream `handleBackspace` /
    /// `moveCursor(0, -1)`: `segment(beforeCursor, "grapheme").at(-1)`).
    fn grapheme_last_step_len(chars: &[char]) -> Option<usize> {
        let text: String = chars.iter().collect();
        grapheme_spans(&text).last().map(|&(_, len)| len)
    }

    /// Move up one VISUAL line — port of upstream `moveCursor(-1, 0)`
    /// over the editor's own wrap layout (`editor.ts:1894-1905,
    /// 1479-1612`): the sticky preferred column is honored across wrapped
    /// lines and the cursor snaps to atomic segment boundaries.
    pub fn move_cursor_up(&self) {
        self.set_last_action(None);
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let visual_lines = self.build_visual_line_map(&state, self.last_width());
        let current = find_current_visual_line(&visual_lines, &state);
        if current > 0 {
            self.move_to_visual_line(&mut state, &visual_lines, current, current - 1);
        }
    }

    /// Move down one VISUAL line (upstream `moveCursor(1, 0)`).
    pub fn move_cursor_down(&self) {
        self.set_last_action(None);
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let visual_lines = self.build_visual_line_map(&state, self.last_width());
        let current = find_current_visual_line(&visual_lines, &state);
        if current + 1 < visual_lines.len() {
            self.move_to_visual_line(&mut state, &visual_lines, current, current + 1);
        }
    }

    /// The layout width recorded by the last render (upstream `lastWidth`).
    fn last_width(&self) -> usize {
        let width = self.last_width.load(std::sync::atomic::Ordering::Relaxed);
        if width == 0 {
            80
        } else {
            width
        }
    }

    /// Build the visual-line map (port of upstream `buildVisualLineMap`,
    /// `editor.ts:1836-1861`): for each logical line either a single entry
    /// (fits `width` cells) or one entry per wrapped chunk, carrying
    /// `(logical line, start column, char length)`.
    fn build_visual_line_map(&self, state: &EditorState, width: usize) -> Vec<VisualLine> {
        let valid_ids = self.valid_paste_ids();
        let mut visual_lines = Vec::new();
        for (line_index, line) in state.lines.iter().enumerate() {
            let chars: Vec<char> = line.chars().collect();
            if chars.is_empty() {
                // Empty lines still take one visual line.
                visual_lines.push(VisualLine {
                    logical_line: line_index,
                    start_col: 0,
                    length: 0,
                });
            } else if visible_width(line) <= width {
                visual_lines.push(VisualLine {
                    logical_line: line_index,
                    start_col: 0,
                    length: chars.len(),
                });
            } else {
                for chunk in word_wrap_line(line, width, &valid_ids) {
                    visual_lines.push(VisualLine {
                        logical_line: line_index,
                        start_col: chunk.start,
                        length: chunk.end - chunk.start,
                    });
                }
            }
        }
        visual_lines
    }

    /// The visual line containing `(line, col)` — port of upstream
    /// `findVisualLineAt` (`editor.ts:1866-1883`).
    fn find_visual_line_at(visual_lines: &[VisualLine], line: usize, col: usize) -> usize {
        for (index, vl) in visual_lines.iter().enumerate() {
            if vl.logical_line != line {
                continue;
            }
            let offset = col.saturating_sub(vl.start_col);
            let is_last_segment_of_line =
                index + 1 == visual_lines.len() || visual_lines[index + 1].logical_line != line;
            if offset < vl.length || (is_last_segment_of_line && offset == vl.length) {
                return index;
            }
        }
        visual_lines.len().saturating_sub(1)
    }

    /// Move the cursor to a target visual line with sticky-column and
    /// atomic-segment snapping — port of upstream `moveToVisualLine`
    /// (`editor.ts:1479-1559`).
    fn move_to_visual_line(
        &self,
        state: &mut EditorState,
        visual_lines: &[VisualLine],
        current: usize,
        target: usize,
    ) {
        let Some(current_vl) = visual_lines.get(current).copied() else {
            return;
        };
        let Some(target_vl) = visual_lines.get(target).copied() else {
            return;
        };

        // When the cursor was snapped to a segment start, resolve the
        // pre-snap position against the visual line it belongs to
        // (upstream `editor.ts:1491-1497`).
        let snapped = *self
            .snapped_from_cursor_col
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let current_visual_col = if let Some(snapped) = snapped {
            let vl_index =
                Self::find_visual_line_at(visual_lines, current_vl.logical_line, snapped);
            snapped - visual_lines[vl_index].start_col
        } else {
            state.cursor_col - current_vl.start_col
        };

        // For non-last segments, clamp to length-1 to stay within the
        // segment (upstream `editor.ts:1499-1508`).
        let is_last_source_segment = current + 1 == visual_lines.len()
            || visual_lines[current + 1].logical_line != current_vl.logical_line;
        let source_max_visual_col = if is_last_source_segment {
            current_vl.length
        } else {
            current_vl.length.saturating_sub(1)
        };
        let is_last_target_segment = target + 1 == visual_lines.len()
            || visual_lines[target + 1].logical_line != target_vl.logical_line;
        let target_max_visual_col = if is_last_target_segment {
            target_vl.length
        } else {
            target_vl.length.saturating_sub(1)
        };

        let move_to_visual_col = self.compute_vertical_move_column(
            current_visual_col,
            source_max_visual_col,
            target_max_visual_col,
        );

        state.cursor_line = target_vl.logical_line;
        let target_col = target_vl.start_col + move_to_visual_col;
        let logical_line = state
            .lines
            .get(target_vl.logical_line)
            .cloned()
            .unwrap_or_default();
        state.cursor_col = target_col.min(logical_line.chars().count());

        // Snap the cursor to an atomic segment boundary (e.g. a paste
        // marker) so it never lands mid-marker (upstream
        // `editor.ts:1518-1558`).
        let chars: Vec<char> = logical_line.chars().collect();
        let valid_ids = self.valid_paste_ids();
        for (seg_index, seg_len) in atomic_char_segments(&chars, &valid_ids) {
            if seg_index > state.cursor_col {
                break;
            }
            if seg_len <= 1 {
                continue;
            }
            if state.cursor_col < seg_index + seg_len {
                let is_continuation = seg_index < target_vl.start_col;
                let is_moving_down = target > current;
                if is_continuation && is_moving_down {
                    // The marker started on a previous visual line; skip the
                    // remaining continuation visual lines and land past it.
                    let seg_end = seg_index + seg_len;
                    let mut next = target + 1;
                    while next < visual_lines.len()
                        && visual_lines[next].logical_line == target_vl.logical_line
                        && visual_lines[next].start_col < seg_end
                    {
                        next += 1;
                    }
                    if next < visual_lines.len() {
                        self.move_to_visual_line(state, visual_lines, current, next);
                        return;
                    }
                }
                // Snap to the segment start, remembering the pre-snap
                // column for the next vertical move.
                *self
                    .snapped_from_cursor_col
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(state.cursor_col);
                state.cursor_col = seg_index;
                return;
            }
        }
        // No snap occurred — the sticky pre-snap position is resolved.
        *self
            .snapped_from_cursor_col
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
    }

    /// Sticky-column decision table for vertical moves — port of upstream
    /// `computeVerticalMoveColumn` (`editor.ts:1581-1612`).
    fn compute_vertical_move_column(
        &self,
        current_visual_col: usize,
        source_max_visual_col: usize,
        target_max_visual_col: usize,
    ) -> usize {
        let mut preferred = self
            .preferred_visual_col
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let has_preferred = preferred.is_some();
        let cursor_in_middle = current_visual_col < source_max_visual_col;
        let target_too_short = target_max_visual_col < current_visual_col;

        if !has_preferred || cursor_in_middle {
            if target_too_short {
                *preferred = Some(current_visual_col);
                return target_max_visual_col;
            }
            *preferred = None;
            return current_visual_col;
        }

        let preferred_col = preferred.unwrap_or(current_visual_col);
        let target_cant_fit_preferred = target_max_visual_col < preferred_col;
        if target_too_short || target_cant_fit_preferred {
            return target_max_visual_col;
        }
        *preferred = None;
        preferred_col
    }

    /// Move to the start of the current line (upstream `cursorLineStart`:
    /// Home / Ctrl+A). Clears the sticky column (upstream `setCursorCol`).
    pub fn move_cursor_home(&self) {
        self.set_last_action(None);
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.cursor_col = 0;
        self.clear_sticky_columns();
    }

    /// Move to the end of the current line (upstream `cursorLineEnd`:
    /// End / Ctrl+E). Clears the sticky column (upstream `setCursorCol`).
    pub fn move_cursor_end(&self) {
        self.set_last_action(None);
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.cursor_col = state.lines[state.cursor_line].chars().count();
        self.clear_sticky_columns();
    }

    /// Move left to the start of the previous word (upstream
    /// `cursorWordLeft` → `findWordBackward`, `word-navigation.ts`):
    /// whitespace is skipped first, then one word-like / punctuation /
    /// atomic-marker segment.
    pub fn move_word_left(&self) {
        self.set_last_action(None);
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let line = state.lines[state.cursor_line].clone();
        let chars: Vec<char> = line.chars().collect();
        let col = Self::normalize_col(&state, state.cursor_line, state.cursor_col).min(chars.len());
        state.cursor_col = find_word_backward(&chars, col, &self.valid_paste_ids());
    }

    /// Move right past the end of the next word (upstream `cursorWordRight`
    /// → `findWordForward`).
    pub fn move_word_right(&self) {
        self.set_last_action(None);
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let line = state.lines[state.cursor_line].clone();
        let chars: Vec<char> = line.chars().collect();
        let col = Self::normalize_col(&state, state.cursor_line, state.cursor_col).min(chars.len());
        state.cursor_col = find_word_forward(&chars, col, &self.valid_paste_ids());
    }

    /// Enter the awaiting-character mode of `tui.editor.jumpForward` /
    /// `tui.editor.jumpBackward` (upstream `jumpMode = "forward"/"backward"`,
    /// `editor.ts:971-978`).
    pub fn set_jump_mode(&self, forward: bool) {
        *self
            .jump_mode
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(forward);
    }

    /// The active jump direction (`Some(true)` = forward), or `None` when the
    /// editor is not awaiting a jump character.
    pub fn jump_mode(&self) -> Option<bool> {
        *self
            .jump_mode
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Leave the awaiting-character mode without moving (upstream
    /// `jumpMode = null`).
    pub fn cancel_jump_mode(&self) {
        *self
            .jump_mode
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
    }

    /// Move the cursor to the next/previous occurrence of `ch` — port of
    /// upstream `jumpToChar` (`editor.ts:2138-2166`). The current line is
    /// searched after/before the cursor, every following (forward) or
    /// preceding (backward) line from its start/end; with no match the cursor
    /// stays in place.
    pub fn jump_to_char(&self, ch: char, forward: bool) {
        self.set_last_action(None);
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let line_count = state.lines.len();
        let mut line_index = state.cursor_line as isize;
        loop {
            if line_index < 0 || line_index as usize >= line_count {
                return;
            }
            let line = state.lines[line_index as usize].clone();
            let chars: Vec<char> = line.chars().collect();
            let is_current_line = line_index as usize == state.cursor_line;
            // Current line: start after/before the cursor (upstream
            // `searchFrom`); other lines: the whole line (JS `indexOf` from 0
            // forward, `lastIndexOf` with an undefined position = the last
            // match backward).
            let found = if forward {
                let from = if is_current_line {
                    state.cursor_col.saturating_add(1)
                } else {
                    0
                };
                chars
                    .iter()
                    .enumerate()
                    .skip(from)
                    .find(|(_, candidate)| **candidate == ch)
                    .map(|(index, _)| index)
            } else {
                // JS `lastIndexOf(char, pos)` clamps `pos` to `[0, len]`, so a
                // cursor at column 0 still searches index 0 of the line.
                let end = match (is_current_line, state.cursor_col) {
                    (false, _) => chars.len(),
                    (true, 0) => 1.min(chars.len()),
                    (true, col) => col.min(chars.len()),
                };
                chars[..end].iter().rposition(|candidate| *candidate == ch)
            };
            if let Some(index) = found {
                state.cursor_line = line_index as usize;
                state.cursor_col = index;
                self.clear_sticky_columns();
                return;
            }
            line_index += if forward { 1 } else { -1 };
        }
    }

    /// Delete the character under the cursor (upstream `handleForwardDelete`:
    /// the Delete key). Deletes the whole grapheme cluster under the cursor.
    pub fn delete_char_forward(&self) {
        self.set_last_action(None);
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let col = Self::normalize_col(&state, state.cursor_line, state.cursor_col);
        let line = state.lines[state.cursor_line].clone();
        let len = line.chars().count();
        if col < len {
            self.push_undo(&state);
            // Upstream `handleForwardDelete`: `segment(afterCursor,
            // "grapheme")` first segment (`editor.ts:1779-1802`).
            let chars: Vec<char> = line.chars().collect();
            let step = Self::forward_step_len(&chars, col, &self.valid_paste_ids()).min(len - col);
            let keep: String = chars[..col].iter().collect();
            let rest: String = chars[col + step..].iter().collect();
            let index = state.cursor_line;
            state.lines[index] = format!("{keep}{rest}");
        } else if state.cursor_line + 1 < state.lines.len() {
            // At end of line: join the next line (upstream deleteCharForward).
            self.push_undo(&state);
            let next_index = state.cursor_line + 1;
            let current = state.cursor_line;
            let next = state.lines.remove(next_index);
            state.lines[current].push_str(&next);
        }
    }

    /// Delete back to the start of the previous word (upstream
    /// `deleteWordBackward`: Ctrl+W / Alt+Backspace). The deleted text
    /// enters the kill ring (backward deletion prepends, `editor.ts:1712`).
    /// At the start of a line it behaves like backspace at column 0: the
    /// previous line is merged in and the `\n` enters the kill ring
    /// (upstream `deleteWordBackwards`, `editor.ts:1698-1711`).
    pub fn delete_word_backward(&self) {
        let mut merged = false;
        let mut target = 0usize;
        {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let line = state.lines[state.cursor_line].clone();
            let chars: Vec<char> = line.chars().collect();
            let cursor =
                Self::normalize_col(&state, state.cursor_line, state.cursor_col).min(chars.len());
            if cursor == 0 {
                // Start of line: merge with the previous line, treating the
                // newline as the deleted text (backward = prepend).
                if state.cursor_line > 0 {
                    self.push_undo(&state);
                    self.kill_push("\n", true, self.last_action_is_kill());
                    self.set_last_action(Some(LastAction::Kill));
                    let cursor_line = state.cursor_line;
                    let current = state.lines[cursor_line].clone();
                    let previous_len = state.lines[cursor_line - 1].chars().count();
                    state.lines[cursor_line - 1].push_str(&current);
                    state.lines.remove(cursor_line);
                    state.cursor_line -= 1;
                    state.cursor_col = previous_len;
                    merged = true;
                }
            } else {
                target = find_word_backward(&chars, cursor, &self.valid_paste_ids());
                if target < cursor {
                    let deleted: String = chars[target..cursor].iter().collect();
                    self.push_undo(&state);
                    self.kill_push(&deleted, true, self.last_action_is_kill());
                    self.set_last_action(Some(LastAction::Kill));
                }
            }
        }
        if !merged {
            self.delete_range_on_current_line(target);
        }
    }

    /// Delete forward to the end of the next word (upstream
    /// `deleteWordForward`: Alt+D). The deleted text enters the kill ring
    /// (forward deletion appends, `editor.ts:1755`). At the end of a line
    /// the next line is merged in and the `\n` enters the kill ring
    /// (upstream `deleteWordForward`, `editor.ts:1743-1754`).
    pub fn delete_word_forward(&self) {
        let mut merged = false;
        let mut target = 0usize;
        {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let line = state.lines[state.cursor_line].clone();
            let chars: Vec<char> = line.chars().collect();
            let cursor =
                Self::normalize_col(&state, state.cursor_line, state.cursor_col).min(chars.len());
            if cursor >= chars.len() {
                // End of line: merge with the next line (delete the
                // newline), treating it as the deleted text (forward =
                // append); the cursor stays at the junction.
                if state.cursor_line + 1 < state.lines.len() {
                    self.push_undo(&state);
                    self.kill_push("\n", false, self.last_action_is_kill());
                    self.set_last_action(Some(LastAction::Kill));
                    let cursor_line = state.cursor_line;
                    let next = state.lines.remove(cursor_line + 1);
                    state.lines[cursor_line].push_str(&next);
                    merged = true;
                }
            } else {
                target = find_word_forward(&chars, cursor, &self.valid_paste_ids());
                if target > cursor {
                    let deleted: String = chars[cursor..target].iter().collect();
                    self.push_undo(&state);
                    self.kill_push(&deleted, false, self.last_action_is_kill());
                    self.set_last_action(Some(LastAction::Kill));
                }
            }
        }
        if !merged {
            self.delete_range_from_current_line(target);
        }
    }

    /// Delete from the line start to the cursor (upstream
    /// `deleteToLineStart`: Ctrl+U). The deleted text enters the kill ring
    /// (backward deletion prepends, `editor.ts:1625-1658`). At the start of
    /// a line the previous line is merged in and the `\n` enters the kill
    /// ring (upstream `editor.ts:1641-1653`).
    pub fn delete_to_line_start(&self) {
        let mut merged = false;
        {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let col = Self::normalize_col(&state, state.cursor_line, state.cursor_col);
            if col > 0 {
                let line = state.lines[state.cursor_line].clone();
                let deleted: String = line.chars().take(col).collect();
                self.push_undo(&state);
                self.kill_push(&deleted, true, self.last_action_is_kill());
                self.set_last_action(Some(LastAction::Kill));
            } else if state.cursor_line > 0 {
                // Start of line: merge with the previous line, treating the
                // newline as the deleted text (backward = prepend).
                self.push_undo(&state);
                self.kill_push("\n", true, self.last_action_is_kill());
                self.set_last_action(Some(LastAction::Kill));
                let cursor_line = state.cursor_line;
                let current = state.lines[cursor_line].clone();
                let previous_len = state.lines[cursor_line - 1].chars().count();
                state.lines[cursor_line - 1].push_str(&current);
                state.lines.remove(cursor_line);
                state.cursor_line -= 1;
                state.cursor_col = previous_len;
                merged = true;
            }
        }
        if !merged {
            self.delete_range_on_current_line(0);
        }
    }

    /// Delete from the cursor to the line end (upstream `deleteToLineEnd`:
    /// Ctrl+K). The deleted text enters the kill ring (forward deletion
    /// appends, `editor.ts:1660-1690`). At the end of a line the next line
    /// is merged in and the `\n` enters the kill ring (upstream
    /// `editor.ts:1675-1685`).
    pub fn delete_to_line_end(&self) {
        let mut merged = false;
        {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let col = Self::normalize_col(&state, state.cursor_line, state.cursor_col);
            let line = state.lines[state.cursor_line].clone();
            let len = line.chars().count();
            if col < len {
                let deleted: String = line.chars().skip(col).collect();
                self.push_undo(&state);
                self.kill_push(&deleted, false, self.last_action_is_kill());
                self.set_last_action(Some(LastAction::Kill));
            } else if state.cursor_line + 1 < state.lines.len() {
                // End of line: merge with the next line, treating the
                // newline as the deleted text (forward = append); the
                // cursor stays at the junction.
                self.push_undo(&state);
                self.kill_push("\n", false, self.last_action_is_kill());
                self.set_last_action(Some(LastAction::Kill));
                let cursor_line = state.cursor_line;
                let next = state.lines.remove(cursor_line + 1);
                state.lines[cursor_line].push_str(&next);
                merged = true;
            }
        }
        if !merged {
            let len = {
                let state = self
                    .state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                state.lines[state.cursor_line].chars().count()
            };
            self.delete_range_from_current_line(len);
        }
    }

    /// Delete `[target_col, cursor_col)` on the current line (target must be
    /// `<= cursor`).
    fn delete_range_on_current_line(&self, target_col: usize) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let cursor = Self::normalize_col(&state, state.cursor_line, state.cursor_col);
        let target = target_col.min(cursor);
        if target == cursor {
            return;
        }
        let line = state.lines[state.cursor_line].clone();
        let keep: String = line.chars().take(target).collect();
        let rest: String = line.chars().skip(cursor).collect();
        let index = state.cursor_line;
        state.lines[index] = format!("{keep}{rest}");
        state.cursor_col = target;
    }

    /// Delete `[cursor_col, target_col)` on the current line (target must be
    /// `>= cursor`); the cursor does not move.
    fn delete_range_from_current_line(&self, target_col: usize) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let cursor = Self::normalize_col(&state, state.cursor_line, state.cursor_col);
        let target = target_col.max(cursor);
        let line = state.lines[state.cursor_line].clone();
        let len = line.chars().count();
        if target >= len {
            let keep: String = line.chars().take(cursor).collect();
            let index = state.cursor_line;
            state.lines[index] = keep;
        } else {
            let keep: String = line.chars().take(cursor).collect();
            let rest: String = line.chars().skip(target).collect();
            let index = state.cursor_line;
            state.lines[index] = format!("{keep}{rest}");
        }
    }

    /// Submit: expand paste markers, snapshot the text, clear the editor,
    /// push history (newest-first, upstream `editor.ts:433`) and reset
    /// browsing (upstream `submitValue` → `exitHistoryBrowsing`,
    /// `editor.ts:1364-1386`). The expanded text is what is submitted and
    /// recorded in history (upstream `submitValue`,
    /// `editor.ts:1364-1378`).
    pub fn submit(&self) -> Option<String> {
        let text = self.get_text();
        let registry = self.paste_registry_snapshot();
        let text = Self::expand_paste_markers(&text, &registry.pastes);
        let text = text.trim().to_string();
        if text.is_empty() {
            return None;
        }
        let mut history = self
            .history
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if history.first().map(String::as_str) != Some(text.as_str()) {
            history.insert(0, text.clone());
            // Cap the history at 100 entries, dropping the oldest (upstream
            // `addToHistory`, `editor.ts:428-438`).
            if history.len() > 100 {
                history.pop();
            }
        }
        *self
            .history_index
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
        *self
            .history_draft
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
        drop(history);
        self.clear();
        // Upstream `submitValue` clears the paste registry, resets the undo
        // stack and last action (the kill ring survives,
        // `editor.ts:1364-1378`).
        *self
            .paste_registry
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = PasteRegistry::default();
        self.undo_stack
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clear();
        self.set_last_action(None);
        self.clear_sticky_columns();
        *self
            .submitted
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(text.clone());
        Some(text)
    }

    /// `true` while the editor is browsing history (upstream
    /// `historyIndex > -1`, the `cursorUp`/`cursorDown` gating at
    /// `editor.ts:926-949`).
    pub fn is_browsing_history(&self) -> bool {
        self.history_index
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_some()
    }

    /// Browse history — port of upstream `navigateHistory`
    /// (`editor.ts:456-491`). History is newest-first; `direction` matches
    /// upstream: `-1` = older (Up), `1` = newer (Down) and
    /// `new_index = current - direction`. Entering browsing captures the
    /// draft snapshot (`historyDraft`); moving past the newest entry
    /// restores it. Out-of-range moves are ignored. Restores put the cursor
    /// at the start of the first line going Up and at the end of the last
    /// line going Down (`editor.ts:487` + `setTextInternal` placement,
    /// `editor.ts:493-508`).
    pub fn history_navigate(&self, direction: isize) {
        let history = self
            .history
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if history.is_empty() {
            return;
        }
        self.set_last_action(None);
        let mut index = self
            .history_index
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let current: isize = match *index {
            Some(i) => i as isize,
            None => -1,
        };
        let new_index = current - direction; // Up(-1) increases → older
        if new_index < -1 || new_index >= history.len() as isize {
            return;
        }
        // First entry into browsing: capture the draft and an undo snapshot
        // (upstream `historyDraft = structuredClone(state)` +
        // `pushUndoSnapshot()`, `editor.ts:464-467`).
        if current == -1 && new_index >= 0 {
            let draft = {
                let state = self
                    .state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                self.push_undo(&state);
                state.clone()
            };
            *self
                .history_draft
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(draft);
        }
        *index = (new_index >= 0).then_some(new_index as usize);
        if new_index == -1 {
            // Back past the newest entry: restore the draft (:471-483).
            let draft = self
                .history_draft
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .take();
            drop(index);
            drop(history);
            match draft {
                Some(draft) => {
                    *self
                        .state
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner) = draft;
                }
                None => self.set_text_placement("", false),
            }
        } else {
            let entry = history[new_index as usize].clone();
            drop(index);
            drop(history);
            self.set_text_placement(&entry, direction == -1);
        }
    }

    // =========================================================================
    // Kill ring / undo (ports of upstream `kill-ring.ts`, `undo-stack.ts`
    // and the editor wiring at `components/editor.ts:1614-1777, 1996-2132`).
    // =========================================================================

    fn last_action(&self) -> Option<LastAction> {
        *self
            .last_action
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn set_last_action(&self, action: Option<LastAction>) {
        *self
            .last_action
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = action;
    }

    fn last_action_is_kill(&self) -> bool {
        self.last_action() == Some(LastAction::Kill)
    }

    /// Push a snapshot of `state` plus the paste registry onto the undo
    /// stack (upstream `pushUndoSnapshot`, `editor.ts:2116-2118`).
    fn push_undo(&self, state: &EditorState) {
        let registry = self.paste_registry_snapshot();
        self.undo_stack
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(EditorSnapshot {
                state: state.clone(),
                pastes: registry.pastes,
                paste_counter: registry.paste_counter,
            });
    }

    /// Snapshot the current state onto the undo stack.
    fn push_undo_snapshot(&self) {
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        self.push_undo(&state);
    }

    /// Push killed text onto the kill ring — port of upstream `KillRing.push`
    /// (`kill-ring.ts:19-28`): empty text is dropped; `accumulate` merges
    /// into the newest entry instead of creating a new one, prepending for
    /// backward deletions and appending for forward ones. The ring is
    /// unbounded, matching upstream.
    fn kill_push(&self, text: &str, prepend: bool, accumulate: bool) {
        if text.is_empty() {
            return;
        }
        let mut ring = self
            .kill_ring
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if accumulate {
            if let Some(last) = ring.pop_back() {
                ring.push_back(if prepend {
                    format!("{text}{last}")
                } else {
                    format!("{last}{text}")
                });
                return;
            }
        }
        ring.push_back(text.to_string());
    }

    /// Undo the most recent change — port of upstream `undo()`
    /// (`editor.ts:2120-2132`): pop the newest snapshot and restore it,
    /// including the paste registry. A no-op when the stack is empty.
    /// Upstream registers no redo keybinding (a snapshot stack, not an
    /// inverse-operation stack), so popped snapshots are discarded.
    pub fn undo(&self) {
        let snapshot = self
            .undo_stack
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .pop();
        let Some(snapshot) = snapshot else {
            return;
        };
        {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            *state = snapshot.state;
        }
        *self
            .paste_registry
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = PasteRegistry {
            pastes: snapshot.pastes,
            paste_counter: snapshot.paste_counter,
        };
        self.set_last_action(None);
        self.clear_sticky_columns();
    }

    /// Clear the sticky-column state (upstream `setCursorCol`,
    /// `editor.ts:1469-1473`): any non-vertical cursor move or edit resets
    /// both the preferred visual column and the pre-snap position. Callers
    /// hold no other editor lock.
    fn clear_sticky_columns(&self) {
        *self
            .preferred_visual_col
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
        *self
            .snapped_from_cursor_col
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
    }

    /// Yank (paste) the most recent kill-ring entry at the cursor — port of
    /// upstream `yank()` (`editor.ts:1996-2007`). A no-op on an empty ring.
    pub fn yank(&self) {
        let text = self
            .kill_ring
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .back()
            .cloned();
        let Some(text) = text else {
            return;
        };
        self.push_undo_snapshot();
        self.insert_text_inner(&text);
        self.set_last_action(Some(LastAction::Yank));
    }

    /// Cycle through the kill ring — port of upstream `yankPop()`
    /// (`editor.ts:2009-2030`): only fires immediately after a yank and
    /// only with more than one entry; deletes the just-yanked text, rotates
    /// the ring (the newest entry moves to the front, making the previous
    /// entry the yank target) and yanks again.
    pub fn yank_pop(&self) {
        if self.last_action() != Some(LastAction::Yank) {
            return;
        }
        let ring_len = self
            .kill_ring
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len();
        if ring_len <= 1 {
            return;
        }
        self.push_undo_snapshot();
        // Delete the previously yanked text — still the ring's back entry
        // before the rotation (upstream `deleteYankedText`).
        let yanked = self
            .kill_ring
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .back()
            .cloned();
        if let Some(yanked) = yanked {
            self.delete_yanked_text(&yanked);
        }
        // Rotate: move the back entry to the front (upstream
        // `KillRing.rotate`, `kill-ring.ts:35-41`).
        {
            let mut ring = self
                .kill_ring
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(last) = ring.pop_back() {
                ring.push_front(last);
            }
        }
        if let Some(text) = self
            .kill_ring
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .back()
            .cloned()
        {
            self.insert_text_inner(&text);
        }
        self.set_last_action(Some(LastAction::Yank));
    }

    /// Delete the previously yanked text sitting immediately before the
    /// cursor (port of upstream `deleteYankedText`,
    /// `editor.ts:2078-2114`, char-indexed like the rest of pir's editor).
    fn delete_yanked_text(&self, yanked: &str) {
        let yank_lines: Vec<&str> = yanked.split('\n').collect();
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.cursor_line >= state.lines.len() {
            return;
        }
        let cursor = Self::normalize_col(&state, state.cursor_line, state.cursor_col);
        if yank_lines.len() == 1 {
            // Single line: delete the yanked characters backward from the
            // cursor.
            let start = cursor.saturating_sub(yanked.chars().count());
            let line = state.lines[state.cursor_line].clone();
            let chars: Vec<char> = line.chars().collect();
            let before: String = chars[..start].iter().collect();
            let after: String = chars[cursor.min(chars.len())..].iter().collect();
            let index = state.cursor_line;
            state.lines[index] = format!("{before}{after}");
            state.cursor_col = start;
        } else {
            // Multi-line: the cursor sits at the end of the last yanked
            // line; the yank spans `yank_lines.len()` logical lines.
            let start_line = state.cursor_line.saturating_sub(yank_lines.len() - 1);
            let first_len = yank_lines[0].chars().count();
            let start_col = state.lines[start_line]
                .chars()
                .count()
                .saturating_sub(first_len);
            let line_at_cursor = state.lines[state.cursor_line].clone();
            let after_cursor: String = line_at_cursor.chars().skip(cursor).collect();
            let before_yank: String = state.lines[start_line].chars().take(start_col).collect();
            let end_line = state.cursor_line;
            state.lines.splice(
                start_line..=end_line,
                std::iter::once(format!("{before_yank}{after_cursor}")),
            );
            state.cursor_line = start_line;
            state.cursor_col = start_col;
        }
    }

    /// Page up — port of upstream `pageScroll(-1)` (`editor.ts:1957-1971`):
    /// move the cursor up by a page (`max(5, terminal rows × 30%)` VISUAL
    /// lines, the same budget as the render viewport) over the editor's own
    /// wrap layout, with sticky-column and atomic-segment handling.
    pub fn page_up(&self, terminal_rows: usize) {
        self.page_scroll(terminal_rows, false);
    }

    /// Upstream `page down`, port of `pageScroll(1)`; the target clamps to
    /// the last visual line.
    pub fn page_down(&self, terminal_rows: usize) {
        self.page_scroll(terminal_rows, true);
    }

    /// W11-C main-screen mouse hit-test: the logical cursor position
    /// `(line, col)` a click at `row` (0-based, relative to the editor's
    /// CONTENT rows — the top border is not a content row) and `column`
    /// (0-based screen cell) maps to. Mirrors upstream `Editor.handleMouse`'s
    /// click branch (`editor.ts:660-687`): the clicked visual line comes from
    /// the current wrap layout + scroll window, and the column walks the
    /// segment's grapheme clusters, snapping to the cluster start whose
    /// visible span contains the target column (end-of-segment for a click
    /// past the text).
    ///
    /// Returns `None` when the row is outside the rendered content (a border
    /// row or below the visible window) — the caller then leaves the cursor
    /// where it is.
    pub fn cursor_position_at(
        &self,
        width: usize,
        max_visible_rows: usize,
        row: usize,
        column: usize,
    ) -> Option<(usize, usize)> {
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let padding_x = self
            .padding_x
            .load(std::sync::atomic::Ordering::Relaxed)
            .min(width.saturating_sub(1) / 2);
        let content_width = width.saturating_sub(padding_x * 2).max(1);
        let layout_width = if padding_x > 0 {
            content_width
        } else {
            content_width.saturating_sub(1).max(1)
        };
        let visual_lines = self.build_visual_line_map(&state, layout_width);
        // The render window follows the cursor; recompute it exactly like
        // `render_with_cursor_into` so a click maps to the visible row.
        let cursor_line_index = find_current_visual_line(&visual_lines, &state);
        let max_visible = max_visible_rows.max(1);
        let scroll_offset = if cursor_line_index >= max_visible {
            (cursor_line_index + 1)
                .saturating_sub(max_visible)
                .min(visual_lines.len().saturating_sub(max_visible))
        } else {
            0
        };
        let visual = visual_lines.get(scroll_offset + row)?;
        let line = state.lines.get(visual.logical_line)?;
        let segment: String = line
            .chars()
            .skip(visual.start_col)
            .take(visual.length)
            .collect();
        let target = column.saturating_sub(padding_x);
        let mut visible_column = 0usize;
        let mut target_index = segment.len();
        let mut last_start = 0usize;
        for (byte_index, grapheme) in segment.grapheme_indices(true) {
            let next = visible_column + grapheme_width(grapheme);
            last_start = byte_index;
            if target < next {
                target_index = byte_index;
                break;
            }
            visible_column = next;
        }
        // A click past the last cluster on a non-final wrapped segment snaps
        // to that segment's last cluster (upstream `isLastSegment` rule), so
        // the cursor never lands at the start of the next visual row.
        let is_last_segment = visual_lines
            .get(scroll_offset + row + 1)
            .is_none_or(|next| next.logical_line != visual.logical_line);
        if !is_last_segment && target_index == segment.len() && !segment.is_empty() {
            target_index = last_start;
        }
        let column = visual.start_col + segment[..target_index].chars().count();
        Some((visual.logical_line, column))
    }

    /// Move the cursor to a logical `(line, col)` position (the click
    /// hit-test's action half, upstream `Editor.handleMouse`'s
    /// `state.cursorLine = …; setCursorCol(…)`): the column clamps into the
    /// line, history browsing ends, and the autocomplete popup refreshes on
    /// the next input pass. Out-of-range lines are ignored.
    pub fn set_cursor_position(&self, line: usize, col: usize) -> bool {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if line >= state.lines.len() {
            return false;
        }
        state.cursor_line = line;
        state.cursor_col = col.min(state.lines[line].chars().count());
        true
    }

    fn page_scroll(&self, terminal_rows: usize, down: bool) {
        self.set_last_action(None);
        let page = Self::max_visible_lines(terminal_rows);
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let visual_lines = self.build_visual_line_map(&state, self.last_width());
        let current = find_current_visual_line(&visual_lines, &state);
        let target = if down {
            (current + page).min(visual_lines.len().saturating_sub(1))
        } else {
            current.saturating_sub(page)
        };
        self.move_to_visual_line(&mut state, &visual_lines, current, target);
    }

    /// `true` when the cursor is on the first visual line of the wrap
    /// layout (upstream `isOnFirstVisualLine`, `editor.ts:444-448`) — the
    /// Up-arrow history gate.
    pub fn is_on_first_visual_line(&self) -> bool {
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let visual_lines = self.build_visual_line_map(&state, self.last_width());
        find_current_visual_line(&visual_lines, &state) == 0
    }

    /// `true` when the cursor is on the last visual line (upstream
    /// `isOnLastVisualLine`, `editor.ts:450-454`) — the Down-arrow history
    /// gate.
    pub fn is_on_last_visual_line(&self) -> bool {
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let visual_lines = self.build_visual_line_map(&state, self.last_width());
        find_current_visual_line(&visual_lines, &state) + 1 == visual_lines.len()
    }

    /// Wrap the logical lines at `wrap_width` (the caller already subtracted
    /// any cursor column / padding inset — upstream `layoutText(layoutWidth)`,
    /// editor.ts:523-528, 998-1084). Wrapping is marker-aware: registered
    /// paste markers never break across visual lines.
    fn layout_lines<'a>(
        state: &'a EditorState,
        wrap_width: usize,
        valid_ids: &[usize],
    ) -> Vec<LayoutLine<'a>> {
        let content_width = wrap_width.max(1);
        // Every logical line yields at least one layout line, so the logical
        // line count is a lower bound that saves the growth reallocations.
        let mut layout: Vec<LayoutLine<'a>> = Vec::with_capacity(state.lines.len());
        for (line_index, line) in state.lines.iter().enumerate() {
            let col = if line_index == state.cursor_line {
                Some(state.cursor_col)
            } else {
                None
            };
            if line.is_empty() {
                layout.push(LayoutLine {
                    text: Cow::Borrowed(""),
                    cursor_pos: col.map(|_| 0),
                });
                continue;
            }
            if visible_width(line) <= content_width {
                // Line fits in one layout line (upstream `editor.ts:1017-1030`).
                layout.push(LayoutLine {
                    text: Cow::Borrowed(line.as_str()),
                    cursor_pos: col,
                });
                continue;
            }
            // Word-aware wrapping with (start, end) char chunks
            // (upstream `wordWrapLine`).
            let chunks = word_wrap_line(line, content_width, valid_ids);
            let chars: Vec<char> = line.chars().collect();
            for (chunk_index, chunk) in chunks.iter().enumerate() {
                let is_last_chunk = chunk_index + 1 == chunks.len();
                let has_cursor_in_chunk = col.is_some_and(|c| {
                    if is_last_chunk {
                        c >= chunk.start
                    } else {
                        c >= chunk.start && c < chunk.end
                    }
                });
                let cursor_pos = has_cursor_in_chunk.then(|| {
                    let pos = col.unwrap_or_default().saturating_sub(chunk.start);
                    // Clamp into the chunk text (upstream clamps when the
                    // cursor sits in trimmed whitespace, editor.ts:1059-1062).
                    pos.min(chunk.end - chunk.start)
                });
                layout.push(LayoutLine {
                    text: Cow::Owned(chars[chunk.start..chunk.end].iter().collect()),
                    cursor_pos,
                });
            }
        }
        layout
    }

    fn max_visible_lines(terminal_rows: usize) -> usize {
        (terminal_rows * 30 / 100).max(5)
    }

    /// Port of upstream `createScrollBorder`
    /// (`pi/packages/tui/src/components/editor.ts:279-295`): a centered
    /// ` ↑ N more ` label padded with `─`; two narrow-width fallbacks — a
    /// left-aligned `─── `-prefixed indicator padded with `─`, then an
    /// ellipsis-clamped slice. Upstream returns this only when lines are
    /// actually hidden (the callers at :511-519 pick a plain `─` run
    /// otherwise).
    fn scroll_border(direction: &str, hidden: usize, width: usize) -> String {
        // Upstream `Math.max(0, width)`; `usize` is always >= 0.
        let available_width = width;
        let label = format!(" {direction} {hidden} more ");
        let label_width = visible_width(&label);
        if label_width + 2 <= available_width {
            let left = (available_width - label_width) / 2;
            return format!(
                "{}{}{}",
                "─".repeat(left),
                label,
                "─".repeat(available_width - left - label_width)
            );
        }

        let indicator = format!("─── {direction} {hidden} more ");
        let indicator_width = visible_width(&indicator);
        let remaining = available_width.saturating_sub(indicator_width);
        if indicator_width <= available_width {
            return format!("{indicator}{}", "─".repeat(remaining));
        }

        // `"...".slice(0, availableWidth)` + `sliceByColumn(indicator, 0,
        // availableWidth - ellipsisWidth, true)` (upstream `utils.ts:1263`).
        // The strict column slice equals a hard ASCII clip from column 0.
        let ellipsis = ".".repeat(available_width.min(3));
        let kept = available_width - visible_width(&ellipsis);
        format!("{}{}", truncate_to_width(&indicator, kept, ""), ellipsis)
    }

    /// Render the editor; returns (lines, cursor_screen_row, cursor_screen_col).
    /// `terminal_rows` derives the visible-row budget (upstream
    /// `max(5, rows × 30%)`).
    pub fn render_with_cursor(
        &self,
        width: usize,
        terminal_rows: usize,
    ) -> (Vec<String>, usize, usize) {
        self.render_with_cursor_rows(width, Self::max_visible_lines(terminal_rows))
    }

    /// Render the editor with an explicit visible-row budget — the dock
    /// sizes the window to its own line budget so the cursor can never be
    /// truncated away (P2-7). Returns (lines, cursor_line_index,
    /// cursor_screen_col) where `cursor_line_index` indexes the cursor line
    /// within the returned `lines` (top border included).
    ///
    /// The rows and the cursor cell come from
    /// [`Editor::render_with_cursor_into`] — the canvas-direct core that
    /// [`Component::render_into`] runs — bridged through a throwaway row
    /// pool, so this owned-line form and the canvas form are byte-identical
    /// by construction. A caller that owns a frame canvas (the dock) should
    /// call the core with its own pool instead of paying this `Vec`.
    pub fn render_with_cursor_rows(
        &self,
        width: usize,
        max_visible_rows: usize,
    ) -> (Vec<String>, usize, usize) {
        // Pre-sized like the pre-migration path
        // (`Vec::with_capacity(visible.len() + 2)`; `visible.len()` never
        // exceeds `max_visible_rows.max(1)`), so the bridge adds no vector
        // regrowth on top of the rows themselves.
        let mut lines: Vec<String> = Vec::with_capacity(max_visible_rows.max(1) + 2);
        let (cursor_row, cursor_col) = {
            let mut out = Rows::new(&mut lines);
            self.render_with_cursor_into(width, max_visible_rows, &mut out)
        };
        (lines, cursor_row, cursor_col)
    }

    /// Canvas-direct core of the editor render: every row — top border, the
    /// visible layout lines, bottom border — is written into the recycled row
    /// buffers of `out` (`push` claims a cleared pooled slot, `line_mut`
    /// builds it in place), and the returned `(cursor_row, cursor_col)` is the
    /// cursor cell in [`Editor::render_with_cursor_rows`]'s convention:
    /// `cursor_row` indexes the rows written to `out` (top border included),
    /// `cursor_col` is the padding-inset visible column.
    ///
    /// Row for row byte-identical to the pre-migration `Vec<String>` path:
    /// the border rows carry the same three pieces as [`push_border_row`],
    /// and each content row writes exactly the spaces / text / padding /
    /// right-padding the `spaces[..n]` slices produced, in the same order.
    pub fn render_with_cursor_into(
        &self,
        width: usize,
        max_visible_rows: usize,
        out: &mut Rows<'_>,
    ) -> (usize, usize) {
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let border = self.border();
        // Padding-aware layout width (upstream editor.ts:523-528): with a
        // horizontal inset the end-of-line cursor may overflow one cell into
        // the right padding, so the text wraps at the full content width;
        // without padding one column is reserved for the cursor. Every
        // rendered row then measures exactly `width`.
        let padding_x = self
            .padding_x
            .load(std::sync::atomic::Ordering::Relaxed)
            .min(width.saturating_sub(1) / 2);
        let content_width = width.saturating_sub(padding_x * 2).max(1);
        let layout_width = if padding_x > 0 {
            content_width
        } else {
            content_width.saturating_sub(1).max(1)
        };
        // Record for cursor navigation — must match the wrapping width
        // (upstream `this.lastWidth = layoutWidth`, editor.ts:531).
        self.last_width
            .store(layout_width, std::sync::atomic::Ordering::Relaxed);
        let valid_ids = self.valid_paste_ids();
        let layout = Self::layout_lines(&state, layout_width, &valid_ids);
        let max_visible = max_visible_rows.max(1);

        let cursor_line_index = layout
            .iter()
            .position(|line| line.cursor_pos.is_some())
            .unwrap_or(0);
        let max_scroll = layout.len().saturating_sub(max_visible);
        let scroll_offset = if cursor_line_index >= max_visible {
            (cursor_line_index + 1)
                .saturating_sub(max_visible)
                .min(max_scroll)
        } else {
            0
        };
        let visible = &layout[scroll_offset..(scroll_offset + max_visible).min(layout.len())];

        // Upstream `renderTopBorder` (editor.ts:511-514): plain `─` run when
        // nothing is hidden above, `↑ N more` scroll label otherwise.
        let hidden_above = scroll_offset;
        let top_border = if hidden_above > 0 {
            Self::scroll_border("↑", hidden_above, width)
        } else {
            "─".repeat(width)
        };
        push_border_row(out, &border, &top_border);
        let mut cursor_row = 0usize;
        let mut cursor_col = 0usize;
        for (visible_index, layout_line) in visible.iter().enumerate() {
            // Only the cursor row rewrites its text (inverse-video cluster or
            // an appended space); every other row borrows the layout text.
            let mut cursor_display: Option<String> = None;
            // End-of-line cursor: appended as a virtual space that may
            // overflow one cell into the right padding; the row still fits
            // `width` (upstream `cursorInPadding`, editor.ts:589-604).
            let mut appended_cursor_space = false;
            if let Some(col) = layout_line.cursor_pos {
                cursor_row = visible_index;
                let text = layout_line.text.as_ref();
                let col = col.min(text.chars().count());
                // Char index to byte offsets, so `before` / `after` are
                // borrows: no `Vec<char>` and no pair of `String`s per frame.
                let split = char_byte_offset(text, col);
                let before = &text[..split];
                let after = &text[split..];
                // The hardware cursor lands on the cell the cluster starts
                // at, so the reported column is a VISIBLE column.
                cursor_col = visible_width(before) + padding_x;
                let hardware_cursor = self
                    .show_hardware_cursor
                    .load(std::sync::atomic::Ordering::Relaxed);
                // The cursor covers one whole grapheme cluster (upstream
                // `afterGraphemes[0]`, `editor.ts:583-592`).
                let cluster_len = first_grapheme_len(after);
                cursor_display = Some(match cluster_len {
                    // On a character: replace the cluster (visible width
                    // unchanged); with the hardware cursor shown the cell
                    // stays plain.
                    Some(len) => {
                        let cut = char_byte_offset(after, len);
                        let mut display = String::with_capacity(text.len() + 8);
                        display.push_str(before);
                        if !hardware_cursor {
                            display.push_str("\x1b[7m");
                        }
                        display.push_str(&after[..cut]);
                        if !hardware_cursor {
                            display.push_str("\x1b[0m");
                        }
                        display.push_str(&after[cut..]);
                        display
                    }
                    None => {
                        appended_cursor_space = true;
                        let mut display = String::with_capacity(text.len() + 9);
                        display.push_str(before);
                        if hardware_cursor {
                            display.push(' ');
                        } else {
                            display.push_str("\x1b[7m \x1b[0m");
                        }
                        display
                    }
                });
            }
            let display: &str = cursor_display
                .as_deref()
                .unwrap_or(layout_line.text.as_ref());
            let line_visible = visible_width(display);
            let cursor_in_padding =
                appended_cursor_space && line_visible > content_width && padding_x > 0;
            let padding = content_width.saturating_sub(line_visible);
            let right_pad = if cursor_in_padding {
                padding_x - 1
            } else {
                padding_x
            };
            // Claim a pooled row and build it in place. The old per-frame
            // `spaces` scratch string is gone: its slices `[..padding_x]`,
            // `[..padding]`, `[..right_pad]` were three runs of spaces, so
            // the row is byte-identical with the same counts pushed directly
            // (and the pooled slot is cleared by `Rows::push`, never
            // appended to).
            let row = claim_row(out);
            push_spaces(row, padding_x);
            row.push_str(display);
            push_spaces(row, padding);
            push_spaces(row, right_pad);
        }
        // Upstream `renderBottomBorder` (editor.ts:516-519) with
        // `linesBelow = layoutLines.length - (scrollOffset + visible.length)`
        // (:615): plain `─` run when nothing is hidden below.
        let hidden_below = layout.len() - (scroll_offset + visible.len());
        let bottom_border = if hidden_below > 0 {
            Self::scroll_border("↓", hidden_below, width)
        } else {
            "─".repeat(width)
        };
        push_border_row(out, &border, &bottom_border);
        // Cursor row must account for the top border row.
        (cursor_row + 1, cursor_col)
    }
}

impl Default for Editor {
    fn default() -> Self {
        Self::new()
    }
}

/// Terminal rows the trait render paths ([`Component::render`] /
/// [`Component::render_into`]) hand to the editor. The pre-migration `render`
/// passed a fixed 24 — which [`Editor::max_visible_lines`] turns into a
/// 7-row visible window — and both paths share the constant so they can never
/// drift onto different windows (and thus different rows).
const COMPONENT_RENDER_TERMINAL_ROWS: usize = 24;

impl Component for Editor {
    fn render(&self, width: usize) -> Vec<String> {
        self.render_with_cursor(width, COMPONENT_RENDER_TERMINAL_ROWS)
            .0
    }

    /// Direct-write path: the rows come from
    /// [`Editor::render_with_cursor_into`] with exactly the budget
    /// [`Component::render`] uses, so the canvas path is byte-identical to the
    /// owned-line path while allocating no per-frame `Vec` and no per-row
    /// `String` of its own. The cursor cell the core returns has no home in
    /// the trait method and is dropped (the dock reads it from
    /// [`Editor::render_with_cursor_into`] on its own canvas).
    fn render_into(&self, width: usize, out: &mut Rows<'_>) {
        let _ = self.render_with_cursor_into(
            width,
            Self::max_visible_lines(COMPONENT_RENDER_TERMINAL_ROWS),
            out,
        );
    }

    fn invalidate(&self) {}
}

/// One painted border row written into a claimed pooled row: the border
/// color, the border text (a `─` run or a scroll label), then a foreground
/// reset — the same three pieces in the same order as the owned-line
/// `paint_border_row` this replaced (whose `with_capacity` only pre-sized the
/// buffer; capacity is not part of the bytes).
fn push_border_row(out: &mut Rows<'_>, border: &str, text: &str) {
    let row = claim_row(out);
    row.push_str(border);
    row.push_str(text);
    row.push_str(super::ansi::FG_RESET);
}

/// Claim the next pooled row and hand back its cleared buffer — the canvas
/// form of the `String::with_capacity` the row builders used to allocate
/// ([`Rows::push`] appends a fresh row and never appends to a recycled slot's
/// content).
fn claim_row<'r>(out: &'r mut Rows<'_>) -> &'r mut String {
    out.push("");
    out.line_mut()
}

/// `" ".repeat(count)` pushed straight into a row buffer — the byte-exact
/// equivalent of the `spaces[..count]` slices the row builder used to copy
/// (the scratch string was one run of spaces, so only the count mattered).
fn push_spaces(row: &mut String, count: usize) {
    row.reserve(count);
    for _ in 0..count {
        row.push(' ');
    }
}

/// Byte offset of char index `col` in `s` — `s.len()` when `col` is at or
/// past the last char. The exact clamp a `Vec<char>` slice would give, but
/// without building the `Vec<char>`.
fn char_byte_offset(s: &str, col: usize) -> usize {
    s.char_indices().nth(col).map(|(i, _)| i).unwrap_or(s.len())
}

/// Length in chars of the first grapheme cluster of `text` (`None` when
/// empty) — `grapheme_spans(text).first()` for callers that only need the
/// cluster under the cursor and must not segment the rest of the line.
fn first_grapheme_len(text: &str) -> Option<usize> {
    UnicodeSegmentation::graphemes(text, true)
        .next()
        .map(|cluster| cluster.chars().count())
}

// ===========================================================================
// Word navigation — port of upstream `word-navigation.ts` over a
// Unicode-word-boundary approximation of `Intl.Segmenter` word granularity.
// ===========================================================================

/// One segmentation unit of a line for word navigation.
#[derive(Debug, Clone, Copy)]
struct WordSegment {
    start: usize,
    len: usize,
    word_like: bool,
    /// Registered paste markers are single atomic units (upstream
    /// `isAtomicSegment`).
    atomic: bool,
}

impl WordSegment {
    fn end(&self) -> usize {
        self.start + self.len
    }

    fn is_whitespace(&self, chars: &[char]) -> bool {
        chars[self.start..self.end()]
            .iter()
            .all(|c| c.is_whitespace())
    }
}

/// Word characters — `Intl.Segmenter` word-like runs are letters and digits
/// (CJK runs included; upstream relies on ICU dictionaries to subdivide
/// long CJK runs, which this approximation deliberately does not).
fn is_word_char(c: char) -> bool {
    c.is_alphanumeric()
}

/// The script class of a word-run character for segmentation purposes.
/// `Intl.Segmenter` word granularity never joins an ideographic / kana /
/// Hangul run to a non-CJK one (`漢字abc` → `漢字` + `abc`, `abc漢` →
/// `abc` + `漢`, `한글abc` → `한글` + `abc`), while letters and digits of all
/// non-CJK scripts share one class (`евроabc` stays a single segment). The
/// four CJK classes do not merge with each other either, matching ICU's
/// per-script runs (its dictionary-based sub-segmentation of mixed Japanese
/// runs is not reproduced — see the module notes).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WordClass {
    Han,
    Hiragana,
    Katakana,
    Hangul,
    Bopomofo,
    Other,
}

/// `Intl.Segmenter` word-run class of a single code point (script blocks per
/// the Unicode `Scripts.txt` blocks ICU uses for word breaking).
fn word_class(c: char) -> WordClass {
    match c as u32 {
        0x2E80..=0x2EFF // CJK Radicals Supplement
        | 0x3005..=0x3007 // ideographic iteration mark / 〆 / 〇
        | 0x3021..=0x3029 // Hangzhou numerals
        | 0x3038..=0x303B
        | 0x3400..=0x4DBF // CJK Extension A
        | 0x4E00..=0x9FFF // CJK Unified Ideographs
        | 0xF900..=0xFAFF // CJK Compatibility Ideographs
        | 0x20000..=0x2FA1F // CJK Extensions B-F
        | 0x30000..=0x323AF // CJK Extensions G-H
        => WordClass::Han,
        0x3041..=0x309F => WordClass::Hiragana,
        0x30A0..=0x30FF | 0x31F0..=0x31FF | 0xFF66..=0xFF9F => WordClass::Katakana,
        0x1100..=0x11FF | 0x3130..=0x318F | 0xA960..=0xA97F | 0xAC00..=0xD7A3 | 0xD7B0..=0xD7FF => {
            WordClass::Hangul
        }
        0x3100..=0x312F | 0x31A0..=0x31BF => WordClass::Bopomofo,
        _ => WordClass::Other,
    }
}

/// `MidLetter` / `MidNumLet` connectors: they stay INSIDE a word run only
/// when flanked by word characters on both sides (verified against
/// `Intl.Segmenter`: `foo.bar`, `src/main.rs`, `can't`, `foo:bar`, `a·b` are
/// single word segments, while `e-mail` splits at the hyphen and `a_.b`
/// splits after the underscore).
fn is_mid_connector(c: char) -> bool {
    matches!(c, '.' | '\'' | '\u{2019}' | ':' | '\u{00B7}')
}

/// `ExtendNumLet` (UAX #29): `_` joins a non-CJK word run on either side, so
/// `_abc`, `abc_` and `a_b` are single word-like segments upstream. It does
/// not join a word character across an ideographic boundary (`漢_abc`
/// segments as `漢` + `_abc`).
fn is_extend_num_let(c: char) -> bool {
    c == '_'
}

/// Where a word-like run starts at cluster `index`, and its script class:
/// at a word character, or at an `ExtendNumLet` that joins the non-CJK word
/// cluster after it (`_abc` is one word-like segment upstream, UAX #29
/// WB13a). `None` when the cluster opens a punctuation or whitespace run.
fn word_run_start(chars: &[char], spans: &[(usize, usize)], index: usize) -> Option<WordClass> {
    let (start, len) = spans[index];
    if is_word_cluster(&chars[start..start + len]) {
        return Some(word_class(chars[start]));
    }
    if len == 1 && is_extend_num_let(chars[start]) {
        if let Some(&(next_start, next_len)) = spans.get(index + 1) {
            if is_word_cluster(&chars[next_start..next_start + next_len]) {
                let class = word_class(chars[next_start]);
                if class == WordClass::Other {
                    return Some(class);
                }
            }
        }
    }
    None
}

/// A grapheme cluster continues a word run when its base character is
/// alphanumeric and every following code point is an ignored joiner
/// (`Extend`/`Format`, UAX #29 WB4) — so `e` + U+0301, `1` + VS16 + U+20E3
/// and `a` + ZWJ + `b` stay inside the word, while emoji clusters (whose
/// base is not a word character) form their own non-word segment, matching
/// `Intl.Segmenter` word granularity.
fn is_word_cluster(cluster: &[char]) -> bool {
    match cluster.split_first() {
        Some((first, rest)) => {
            is_word_char(*first) && rest.iter().all(|c| super::ansi::is_word_joiner(*c))
        }
        None => false,
    }
}

/// ASCII punctuation that forms a navigation boundary inside a word-like
/// segment (upstream `PUNCTUATION_REGEX`, `utils.ts:1006`).
fn is_nav_punctuation(c: char) -> bool {
    matches!(
        c,
        '(' | ')'
            | '{'
            | '}'
            | '['
            | ']'
            | '<'
            | '>'
            | '.'
            | ','
            | ';'
            | ':'
            | '\''
            | '"'
            | '!'
            | '?'
            | '+'
            | '-'
            | '='
            | '*'
            | '/'
            | '\\'
            | '|'
            | '&'
            | '%'
            | '^'
            | '$'
            | '#'
            | '@'
            | '~'
            | '`'
    )
}

/// Segment `chars` into whitespace / punctuation / word-like runs at
/// grapheme-cluster granularity, with registered paste markers merged into
/// single atomic segments (upstream `segmentWithMarkers`, `editor.ts:49-101`,
/// at word granularity — the cluster is the unit the segmenter never splits).
fn word_segments(chars: &[char], valid_ids: &[usize]) -> Vec<WordSegment> {
    let text: String = chars.iter().collect();
    let spans = grapheme_spans(&text);
    let cluster_at = |index: usize| -> &[char] {
        let (start, len) = spans[index];
        &chars[start..start + len]
    };
    let mut segments: Vec<WordSegment> = Vec::new();
    let mut index = 0;
    while index < spans.len() {
        let (start, len) = spans[index];
        // Registered paste markers are atomic units.
        if chars[start] == '[' {
            if let Some(marker) = Editor::parse_paste_marker(chars, start) {
                if valid_ids.contains(&marker.id) {
                    segments.push(WordSegment {
                        start,
                        len: marker.len,
                        word_like: true,
                        atomic: true,
                    });
                    index += 1;
                    while index < spans.len() && spans[index].0 < start + marker.len {
                        index += 1;
                    }
                    continue;
                }
            }
        }
        if cluster_at(index).iter().all(|c| c.is_whitespace()) {
            let mut end = start + len;
            let mut next = index + 1;
            while next < spans.len() && cluster_at(next).iter().all(|c| c.is_whitespace()) {
                end = spans[next].0 + spans[next].1;
                next += 1;
            }
            segments.push(WordSegment {
                start,
                len: end - start,
                word_like: false,
                atomic: false,
            });
            index = next;
        } else if let Some(run_class) = word_run_start(chars, &spans, index) {
            // A word-like run never crosses the boundary between an
            // ideographic/kana/Hangul run and a non-CJK one, and `_` only
            // joins non-CJK runs (`Intl.Segmenter` word granularity).
            let mut end = start + len;
            let mut next = index + 1;
            while next < spans.len() {
                let (next_start, next_len) = spans[next];
                if is_word_cluster(cluster_at(next)) {
                    if word_class(chars[next_start]) != run_class {
                        break;
                    }
                    end = next_start + next_len;
                    next += 1;
                    continue;
                }
                // `ExtendNumLet` joins a non-CJK run on either side
                // (`_abc`, `abc_`, `a_b` — UAX #29 WB13a/WB13b).
                if next_len == 1 && is_extend_num_let(chars[next_start]) {
                    if run_class == WordClass::Other {
                        end = next_start + 1;
                        next += 1;
                        continue;
                    }
                    break;
                }
                // A `MidLetter`/`MidNumLet` connector stays inside the run
                // only when a word character precedes it and a word cluster
                // of the same class follows (`foo.bar`, `src/main.rs`;
                // `a_.b` and `漢.abc` split at the connector).
                if next_len == 1
                    && is_mid_connector(chars[next_start])
                    && run_class == WordClass::Other
                    && end > start
                    && is_word_char(chars[end - 1])
                {
                    if let Some(&(after_start, after_len)) = spans.get(next + 1) {
                        if is_word_cluster(&chars[after_start..after_start + after_len])
                            && word_class(chars[after_start]) == run_class
                        {
                            end = after_start + after_len;
                            next += 2;
                            continue;
                        }
                    }
                }
                break;
            }
            segments.push(WordSegment {
                start,
                len: end - start,
                word_like: true,
                atomic: false,
            });
            index = next;
        } else {
            // Punctuation run (non-word, non-whitespace) at cluster
            // granularity; a registered marker terminates the run.
            let mut end = start + len;
            let mut next = index + 1;
            while next < spans.len() {
                let (next_start, next_len) = spans[next];
                if cluster_at(next).iter().all(|c| c.is_whitespace())
                    || is_word_cluster(cluster_at(next))
                {
                    break;
                }
                if chars[next_start] == '[' {
                    if let Some(marker) = Editor::parse_paste_marker(chars, next_start) {
                        if valid_ids.contains(&marker.id) {
                            break;
                        }
                    }
                }
                end = next_start + next_len;
                next += 1;
            }
            segments.push(WordSegment {
                start,
                len: end - start,
                word_like: false,
                atomic: false,
            });
            index = next;
        }
    }
    segments
}

/// Find the cursor position after moving one word backward from `cursor` —
/// port of upstream `findWordBackward` (`word-navigation.ts:22-70`):
/// trailing whitespace is skipped, then one word-like segment (stopping
/// after its last embedded ASCII punctuation character), punctuation run or
/// atomic marker.
fn find_word_backward(chars: &[char], cursor: usize, valid_ids: &[usize]) -> usize {
    if cursor == 0 {
        return 0;
    }
    let cursor = cursor.min(chars.len());
    let before = &chars[..cursor];
    let mut segments = word_segments(before, valid_ids);
    let mut new_cursor = cursor;

    // Skip trailing whitespace.
    while let Some(last) = segments.last() {
        if last.atomic || !last.is_whitespace(before) {
            break;
        }
        new_cursor -= last.len;
        segments.pop();
    }
    let Some(last) = segments.last().copied() else {
        return new_cursor;
    };
    if last.atomic {
        // Skip one atomic segment.
        new_cursor -= last.len;
    } else if last.word_like {
        // Skip inside one word-like segment, preserving ASCII punctuation
        // boundaries.
        let segment = &before[last.start..last.end()];
        match segment.iter().rposition(|c| is_nav_punctuation(*c)) {
            None => new_cursor -= last.len,
            Some(pos) => new_cursor -= last.len - (pos + 1),
        }
    } else {
        // Skip the non-word non-whitespace run (punctuation).
        while let Some(last) = segments.last().copied() {
            if last.atomic || last.word_like || last.is_whitespace(before) {
                break;
            }
            new_cursor -= last.len;
            segments.pop();
        }
    }
    new_cursor
}

/// Find the cursor position after moving one word forward from `cursor` —
/// port of upstream `findWordForward` (`word-navigation.ts:78-117`):
/// leading whitespace is skipped, then one word-like segment (stopping at
/// its first embedded ASCII punctuation character), punctuation run or
/// atomic marker.
fn find_word_forward(chars: &[char], cursor: usize, valid_ids: &[usize]) -> usize {
    if cursor >= chars.len() {
        return chars.len();
    }
    let after = &chars[cursor..];
    let segments = word_segments(after, valid_ids);
    let mut new_cursor = cursor;
    let mut index = 0;

    // Skip leading whitespace.
    while index < segments.len() && !segments[index].atomic && segments[index].is_whitespace(after)
    {
        new_cursor += segments[index].len;
        index += 1;
    }
    if index >= segments.len() {
        return new_cursor;
    }
    let seg = segments[index];
    if seg.atomic {
        // Skip one atomic segment.
        new_cursor += seg.len;
    } else if seg.word_like {
        // Skip inside one word-like segment, preserving ASCII punctuation
        // boundaries.
        let segment = &after[seg.start..seg.end()];
        new_cursor += segment
            .iter()
            .position(|c| is_nav_punctuation(*c))
            .unwrap_or(seg.len);
    } else {
        // Skip the non-word non-whitespace run (punctuation).
        while index < segments.len() {
            let seg = segments[index];
            if seg.atomic || seg.word_like || seg.is_whitespace(after) {
                break;
            }
            new_cursor += seg.len;
            index += 1;
        }
    }
    new_cursor
}

/// Grapheme-cluster segmentation with registered paste markers as single
/// atomic units (upstream `this.segment(text, "grapheme")` marker
/// awareness); every unit is one cluster, returned as a `(start, len)`
/// char span. Multi-char clusters are the units vertical movement snaps to
/// (`editor.ts:1518-1558`); single-char clusters need no snapping.
fn atomic_char_segments(chars: &[char], valid_ids: &[usize]) -> Vec<(usize, usize)> {
    let text: String = chars.iter().collect();
    let spans = grapheme_spans(&text);
    let mut units = Vec::with_capacity(spans.len());
    let mut index = 0;
    while index < spans.len() {
        let (start, len) = spans[index];
        if chars[start] == '[' {
            if let Some(marker) = Editor::parse_paste_marker(chars, start) {
                if valid_ids.contains(&marker.id) {
                    units.push((start, marker.len));
                    index += 1;
                    while index < spans.len() && spans[index].0 < start + marker.len {
                        index += 1;
                    }
                    continue;
                }
            }
        }
        units.push((start, len));
        index += 1;
    }
    units
}

/// The visual line containing the cursor (upstream `findCurrentVisualLine`,
/// `editor.ts:1888-1892`).
fn find_current_visual_line(visual_lines: &[VisualLine], state: &EditorState) -> usize {
    Editor::find_visual_line_at(visual_lines, state.cursor_line, state.cursor_col)
}

// ===========================================================================
// Word wrap — port of upstream `wordWrapLine` (`editor.ts:124-216`) over
// grapheme clusters with registered paste markers atomic.
// ===========================================================================

/// Split a line into word-wrapped `(start, end)` char chunks. Wraps at word
/// boundaries when possible, falling back to cluster-level wrapping for
/// words longer than the available width; registered paste markers are
/// atomic (they only break visually when wider than `max_width`), and no
/// chunk ever splits a grapheme cluster.
fn word_wrap_line(line: &str, max_width: usize, valid_ids: &[usize]) -> Vec<TextChunk> {
    if line.is_empty() || max_width == 0 {
        return vec![TextChunk { start: 0, end: 0 }];
    }
    let line_width = visible_width(line);
    let chars: Vec<char> = line.chars().collect();
    if line_width <= max_width {
        return vec![TextChunk {
            start: 0,
            end: chars.len(),
        }];
    }

    let segments = atomic_char_segments(&chars, valid_ids);
    // A unit is a registered paste marker when it carries the whole
    // `[paste #N …]` shape (upstream `isPasteMarker`); any other multi-char
    // unit is a plain grapheme cluster.
    let is_marker_unit =
        |unit: &[char]| unit.len() > 1 && Editor::parse_paste_marker(unit, 0).is_some();
    let mut chunks: Vec<TextChunk> = Vec::new();
    let mut current_width = 0usize;
    let mut chunk_start = 0usize;
    // Wrap opportunity: the position after the last whitespace before a
    // non-whitespace unit, or a CJK boundary.
    let mut wrap_opp_index: Option<usize> = None;
    let mut wrap_opp_width = 0usize;

    for (i, (char_index, unit_len)) in segments.iter().copied().enumerate() {
        let unit: String = chars[char_index..char_index + unit_len].iter().collect();
        let unit_width = grapheme_width(&unit);
        let is_marker = is_marker_unit(&chars[char_index..char_index + unit_len]);
        let is_ws = !is_marker && unit.chars().all(char::is_whitespace);

        // Overflow check before advancing (upstream `editor.ts:152-171`).
        if current_width + unit_width > max_width {
            if let Some(opp) = wrap_opp_index {
                if current_width - wrap_opp_width + unit_width <= max_width {
                    // Backtrack to the last wrap opportunity.
                    chunks.push(TextChunk {
                        start: chunk_start,
                        end: opp,
                    });
                    chunk_start = opp;
                    current_width -= wrap_opp_width;
                } else if chunk_start < char_index {
                    // Backtracking would not help: force-break here.
                    chunks.push(TextChunk {
                        start: chunk_start,
                        end: char_index,
                    });
                    chunk_start = char_index;
                    current_width = 0;
                }
            } else if chunk_start < char_index {
                chunks.push(TextChunk {
                    start: chunk_start,
                    end: char_index,
                });
                chunk_start = char_index;
                current_width = 0;
            }
            wrap_opp_index = None;
        }

        if unit_width > max_width {
            // Single atomic unit wider than `max_width` (e.g. a paste marker
            // in a narrow terminal): re-wrap it at cluster granularity. The
            // split is purely visual — the unit stays atomic for cursor
            // movement and editing (upstream `editor.ts:173-189`).
            let sub_chunks = word_wrap_line(&unit, max_width, &[]);
            for sub in &sub_chunks[..sub_chunks.len().saturating_sub(1)] {
                chunks.push(TextChunk {
                    start: char_index + sub.start,
                    end: char_index + sub.end,
                });
            }
            if let Some(last) = sub_chunks.last() {
                chunk_start = char_index + last.start;
                current_width = visible_width(
                    &chars[char_index + last.start..char_index + last.end]
                        .iter()
                        .collect::<String>(),
                );
            }
            wrap_opp_index = None;
            continue;
        }

        // Advance.
        current_width += unit_width;

        // Record wrap opportunity: whitespace followed by non-whitespace
        // (the break point is after the last space), or a boundary where
        // either side is CJK (upstream `editor.ts:194-209`).
        if let Some(&(next_index, next_len)) = segments.get(i + 1) {
            let next_unit = &chars[next_index..next_index + next_len];
            let next_is_marker = is_marker_unit(next_unit);
            let next_is_ws = next_len == 1 && next_unit[0].is_whitespace();
            if is_ws && !next_is_ws {
                wrap_opp_index = Some(next_index);
                wrap_opp_width = current_width;
            } else if !is_ws && !next_is_ws {
                let is_cjk = !is_marker && unit.chars().any(is_cjk_char);
                let next_is_cjk = next_is_marker || next_unit.iter().copied().any(is_cjk_char);
                if is_cjk || next_is_cjk {
                    wrap_opp_index = Some(next_index);
                    wrap_opp_width = current_width;
                }
            }
        }
    }

    // Final chunk.
    chunks.push(TextChunk {
        start: chunk_start,
        end: chars.len(),
    });
    chunks
}

/// CJK character check for wrap opportunities (upstream `cjkBreakRegex`,
/// `utils.ts:54-55`).
fn is_cjk_char(c: char) -> bool {
    // Han, Hiragana, Katakana, Hangul, Bopomofo ranges (script extensions
    // approximated by their principal blocks).
    matches!(c as u32,
        0x2E80..=0x2EFF      // CJK Radicals Supplement
        | 0x2F00..=0x2FDF    // Kangxi Radicals
        | 0x3000..=0x303F    // CJK Symbols and Punctuation (incl. Kana adjuncts)
        | 0x3040..=0x309F    // Hiragana
        | 0x30A0..=0x30FF    // Katakana
        | 0x3105..=0x312F    // Bopomofo
        | 0x3130..=0x318F    // Hangul Compatibility Jamo
        | 0x31A0..=0x31BF    // Bopomofo Extended
        | 0x31F0..=0x31FF    // Katakana Phonetic Extensions
        | 0x3400..=0x4DBF    // CJK Extension A
        | 0x4E00..=0x9FFF    // CJK Unified Ideographs
        | 0xAC00..=0xD7AF    // Hangul Syllables
        | 0xF900..=0xFAFF    // CJK Compatibility Ideographs
        | 0x20000..=0x2A6DF  // CJK Extension B
        | 0x2A700..=0x2EBEF  // CJK Extensions C..F
        | 0x30000..=0x3134F  // CJK Extension G
    )
}
