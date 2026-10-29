//! Interactive session picker for `-r/--resume`, built on the existing
//! [`pir::tui::selector::SelectList`] component (upstream
//! `cli/session-picker.ts` + `modes/interactive/components/session-selector.ts`).
//!
//! This is a standalone full-screen loop in the coding-agent layer so the
//! frozen `src/tui/**` components are consumed as-is. The management keys of
//! the upstream session selector are ported (upstream `SessionList`
//! handleInput, `session-selector.ts:540-646`): delete (`ctrl+d`), the
//! non-invasive delete alias (`ctrl+backspace`, only with an empty query),
//! rename (`ctrl+r` → inline input), sort-mode cycle (`ctrl+s`:
//! threaded → recent → relevance), path display toggle (`ctrl+p`), the
//! named-only filter toggle (`ctrl+n`), and the scope toggle (`Tab`:
//! current directory ↔ all sessions, `listAll`).

use std::io::Write;
use std::path::{Path, PathBuf};

use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::terminal::{self, EnterAlternateScreen, LeaveAlternateScreen};

use crate::session_manager::search::{
    match_session_search, SessionSearchFields, SessionSearchQuery,
};
use crate::session_manager::{
    SessionSummary, SessionTailPage, DEFAULT_TAIL_PAGE_BYTES, DEFAULT_TAIL_PAGE_ENTRIES,
};

// `crate::tui` re-exports used below.
use crate::tui::truncate_to_width;

/// Sort modes (upstream `SortMode`): threaded (parent-session tree, newest
/// subtree first), recent (flat, newest first), relevance (fuzzy score order
/// for the active query).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortMode {
    Threaded,
    Recent,
    Relevance,
}

impl SortMode {
    /// Upstream cycle order: threaded → recent → relevance → threaded.
    pub fn next(self) -> Self {
        match self {
            SortMode::Threaded => SortMode::Recent,
            SortMode::Recent => SortMode::Relevance,
            SortMode::Relevance => SortMode::Threaded,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            SortMode::Threaded => "Threaded",
            SortMode::Recent => "Recent",
            SortMode::Relevance => "Fuzzy",
        }
    }
}

/// One display row (upstream `SessionInfo`).
#[derive(Debug, Clone)]
pub struct SessionRow {
    pub path: PathBuf,
    pub id: String,
    pub cwd: String,
    pub name: Option<String>,
    pub first_message: String,
    /// Every user/assistant message's text joined with a space: the
    /// selector-search corpus (upstream `SessionInfo.allMessagesText`).
    pub all_messages_text: String,
    pub message_count: usize,
    /// Modified time in milliseconds since the epoch.
    pub modified_ms: u128,
    pub parent_session_path: Option<String>,
}

impl From<&SessionSummary> for SessionRow {
    fn from(summary: &SessionSummary) -> Self {
        Self {
            path: summary.path.clone(),
            id: summary.id.clone(),
            cwd: summary.cwd.clone(),
            name: summary.name.clone(),
            first_message: summary.first_message.clone(),
            all_messages_text: summary.all_messages_text.clone(),
            message_count: summary.message_count,
            modified_ms: summary
                .modified
                .duration_since(std::time::UNIX_EPOCH)
                .map(|duration| duration.as_millis())
                .unwrap_or(0),
            parent_session_path: summary.parent_session_path.clone(),
        }
    }
}

impl SessionRow {
    /// Whether the session carries a user-defined name (upstream
    /// `hasSessionName`).
    pub fn has_name(&self) -> bool {
        self.name
            .as_deref()
            .is_some_and(|name| !name.trim().is_empty())
    }
}

/// One flattened display node of the threaded tree (upstream
/// `FlatSessionNode`): the row index plus the tree-drawing metadata.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThreadedNode {
    pub row: usize,
    pub depth: usize,
    pub is_last: bool,
    /// For each ancestor level, whether there are more siblings after it.
    pub ancestor_continues: Vec<bool>,
}

/// Build the threaded order (upstream `buildSessionTree` + `flattenSessionTree`
/// reduced to path indices): sessions whose `parent_session_path` points at
/// another listed session become its children; roots and subtrees sort by the
/// latest activity in the subtree, descending. Pure; unit-tested.
pub fn build_threaded_order(rows: &[SessionRow]) -> Vec<ThreadedNode> {
    // Node index by canonical path text.
    let index_by_path: std::collections::HashMap<String, usize> = rows
        .iter()
        .enumerate()
        .map(|(index, row)| (row.path.display().to_string(), index))
        .collect();
    let mut children: Vec<Vec<usize>> = vec![Vec::new(); rows.len()];
    let mut roots: Vec<usize> = Vec::new();
    for (index, row) in rows.iter().enumerate() {
        let parent = row
            .parent_session_path
            .as_ref()
            .and_then(|path| index_by_path.get(path.as_str()).copied());
        match parent {
            Some(parent) if parent != index => children[parent].push(index),
            _ => roots.push(index),
        }
    }
    // Latest activity per subtree (upstream `updateLatestActivity`).
    fn subtree_latest(rows: &[SessionRow], children: &[Vec<usize>], index: usize) -> u128 {
        let mut latest = rows[index].modified_ms;
        for child in &children[index] {
            latest = latest.max(subtree_latest(rows, children, *child));
        }
        latest
    }
    let mut order: Vec<ThreadedNode> = Vec::new();
    // Iterative pre-order so deep trees cannot blow the stack.
    let mut stack: Vec<(usize, usize, bool, Vec<bool>)> = Vec::new();
    roots.sort_by(|a, b| {
        subtree_latest(rows, &children, *b).cmp(&subtree_latest(rows, &children, *a))
    });
    for (position, root) in roots.iter().enumerate().rev() {
        stack.push((*root, 0, position + 1 == roots.len(), Vec::new()));
    }
    while let Some((index, depth, is_last, ancestor_continues)) = stack.pop() {
        order.push(ThreadedNode {
            row: index,
            depth,
            is_last,
            ancestor_continues: ancestor_continues.clone(),
        });
        let mut kids = children[index].clone();
        kids.sort_by(|a, b| {
            subtree_latest(rows, &children, *b).cmp(&subtree_latest(rows, &children, *a))
        });
        for (position, child) in kids.iter().enumerate().rev() {
            let child_is_last = position + 1 == kids.len();
            let mut continues = ancestor_continues.clone();
            if depth > 0 {
                continues.push(!is_last);
            }
            stack.push((*child, depth + 1, child_is_last, continues));
        }
    }
    order
}

/// Filter + order the rows for display (upstream `filterSessions` +
/// `filterAndSortSessions`): the named-only filter first, then the query, then
/// the sort mode. The query is the upstream selector search — whitespace
/// tokens (multi-word AND), `"quoted phrases"`, `re:<pattern>` regex, all
/// case-insensitive, matched against id/name/cwd and EVERY user/assistant
/// message (`session_manager::search`). Relevance order uses the upstream
/// lower-is-better score with newest-first as the tie-break.
/// Returns the display order of row indices. Pure; unit-tested.
pub fn filter_rows(
    rows: &[SessionRow],
    query: &str,
    sort_mode: SortMode,
    named_only: bool,
) -> Vec<usize> {
    let trimmed = query.trim();
    let parsed = SessionSearchQuery::parse(query);
    let scored: Vec<(usize, f64)> = rows
        .iter()
        .enumerate()
        .filter(|(_, row)| !named_only || row.has_name())
        .filter_map(|(index, row)| {
            if trimmed.is_empty() {
                return Some((index, 0.0));
            }
            let fields = SessionSearchFields {
                id: &row.id,
                cwd: &row.cwd,
                name: row.name.as_deref(),
                all_messages_text: &row.all_messages_text,
            };
            match_session_search(&fields, &parsed).map(|score| (index, score))
        })
        .collect();
    match sort_mode {
        SortMode::Relevance if !trimmed.is_empty() => {
            // Lower score is better; ties break newest-first (upstream
            // `filterAndSortSessions` relevance mode).
            let mut scored = scored;
            scored.sort_by(|a, b| {
                a.1.partial_cmp(&b.1)
                    .unwrap_or(std::cmp::Ordering::Equal)
                    .then_with(|| rows[b.0].modified_ms.cmp(&rows[a.0].modified_ms))
            });
            scored.into_iter().map(|(index, _)| index).collect()
        }
        SortMode::Recent | SortMode::Relevance => {
            let mut indices: Vec<usize> = scored.into_iter().map(|(index, _)| index).collect();
            indices.sort_by(|a, b| rows[*b].modified_ms.cmp(&rows[*a].modified_ms));
            indices
        }
        SortMode::Threaded if trimmed.is_empty() => {
            let order = build_threaded_order(rows);
            let index_set: std::collections::HashSet<usize> =
                scored.into_iter().map(|(index, _)| index).collect();
            order
                .into_iter()
                .filter(|node| index_set.contains(&node.row))
                .map(|node| node.row)
                .collect()
        }
        SortMode::Threaded => {
            let mut indices: Vec<usize> = scored.into_iter().map(|(index, _)| index).collect();
            indices.sort_by(|a, b| rows[*b].modified_ms.cmp(&rows[*a].modified_ms));
            indices
        }
    }
}

/// The tree-drawing prefix for a threaded node (upstream `buildTreePrefix`).
pub fn tree_prefix(node: &ThreadedNode) -> String {
    if node.depth == 0 {
        return String::new();
    }
    let mut prefix = String::new();
    for continues in &node.ancestor_continues {
        prefix.push_str(if *continues { "│  " } else { "   " });
    }
    prefix.push_str(if node.is_last { "└─ " } else { "├─ " });
    prefix
}

/// Relative session age (upstream `formatSessionDate`).
pub fn format_session_age(modified_ms: u128, now_ms: u128) -> String {
    let diff = now_ms.saturating_sub(modified_ms);
    let minutes = diff / 60_000;
    if minutes < 1 {
        return "now".to_string();
    }
    if minutes < 60 {
        return format!("{minutes}m");
    }
    let hours = minutes / 60;
    if hours < 24 {
        return format!("{hours}h");
    }
    let days = hours / 24;
    if days < 7 {
        return format!("{days}d");
    }
    if days < 30 {
        return format!("{}w", days / 7);
    }
    if days < 365 {
        return format!("{}mo", days / 30);
    }
    format!("{}y", days / 365)
}

/// Shorten an absolute path under the home directory to `~/…` (upstream
/// `shortenPath`).
pub fn shorten_path(path: &str, home: Option<&str>) -> String {
    let Some(home) = home else {
        return path.to_string();
    };
    if home.is_empty() {
        return path.to_string();
    }
    match path.strip_prefix(home) {
        Some(rest) if rest.starts_with('\\') || rest.starts_with('/') => {
            format!("~{rest}")
        }
        Some("") => "~".to_string(),
        _ => path.to_string(),
    }
}

/// Delete a session file, trying the `trash` CLI first and falling back to a
/// permanent unlink (upstream `deleteSessionFile`).
pub fn delete_session_file(path: &PathBuf) -> Result<&'static str, String> {
    // Upstream prefixes `--` when the path starts with `-` so a leading-dash
    // filename is not parsed as a flag by the trash CLI.
    let needs_separator = path.as_os_str().to_string_lossy().starts_with('-');
    let mut trash_args: Vec<&std::ffi::OsStr> = Vec::new();
    if needs_separator {
        trash_args.push(std::ffi::OsStr::new("--"));
    }
    trash_args.push(path.as_os_str());
    let trash = std::process::Command::new("trash")
        .args(&trash_args)
        // Rust's default stdin is inherit (Node's spawn default is a pipe):
        // pin it null so the picker's terminal stdin never reaches the child.
        .stdin(std::process::Stdio::null())
        .output();
    let trashed = match trash {
        Ok(output) => output.status.success(),
        Err(_) => false,
    };
    if trashed || !path.exists() {
        return Ok("trash");
    }
    std::fs::remove_file(path).map_err(|error| error.to_string())?;
    Ok("unlink")
}

/// How a safe session delete removed the file (upstream `deleteSessionFile`'s
/// `method`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionDeleteMethod {
    /// Moved to the OS trash.
    Trash,
    /// Permanently unlinked.
    Unlink,
}

impl SessionDeleteMethod {
    /// The upstream status line (`session-selector.ts:857`).
    pub fn status_message(self) -> &'static str {
        match self {
            SessionDeleteMethod::Trash => "Session moved to trash",
            SessionDeleteMethod::Unlink => "Session deleted",
        }
    }
}

/// Append a display name to the session file (upstream `renameSession` →
/// `mgr.appendSessionInfo(next)`), after proving the target really is a
/// readable pi session file. A blank name is the upstream no-op.
pub fn rename_session(path: &Path, name: &str) -> Result<(), String> {
    let next = name.trim();
    if next.is_empty() {
        return Ok(());
    }
    if !path.is_file() {
        return Err(format!("session file not found: {}", path.display()));
    }
    // A rename only APPENDS a `session_info` entry, so the tail-first reader
    // (which also validates the file's header) is enough; a huge session's
    // whole history is not parsed just to append one line.
    let mut manager = crate::coding_agent::session_resolve::open_session_tail_first(path)
        .map_err(|error| format!("cannot read session file {}: {error}", path.display()))?;
    manager
        .append_session_info(next)
        .map_err(|error| format!("cannot rename session {}: {error}", path.display()))?;
    Ok(())
}

/// The newest stored display name for `path`, read through the bounded tail
/// page (the rename input's upstream "current name" prefill).
pub fn stored_session_name(path: &Path) -> Option<String> {
    let page = SessionTailPage::open(path, DEFAULT_TAIL_PAGE_BYTES, DEFAULT_TAIL_PAGE_ENTRIES)
        .ok()
        .flatten()?;
    page.session_name()
}

/// Safely delete a session file: the target must exist, be a regular file, and
/// parse through [`crate::session_manager::SessionManager`] as a pi session
/// before anything is moved to the trash / unlinked.
///
/// The session-manager read proves the path is a session file (a random
/// `.jsonl` is refused, never trashed); the actual removal then uses the
/// upstream trash-first / unlink fallback. Failures carry the upstream
/// `Failed to delete:` detail.
pub fn delete_session(path: &Path) -> Result<SessionDeleteMethod, String> {
    if !path.exists() {
        return Err(format!("session file not found: {}", path.display()));
    }
    if !path.is_file() {
        return Err(format!("not a session file: {}", path.display()));
    }
    // Validate through the session reader BEFORE deleting: `open` checks the
    // JSONL header/version, so a non-session file is refused untouched.
    crate::coding_agent::session_resolve::open_session_tail_first(path)
        .map_err(|error| format!("not a pi session file: {}: {error}", path.display()))?;
    let path_buf = path.to_path_buf();
    match delete_session_file(&path_buf)? {
        "trash" => Ok(SessionDeleteMethod::Trash),
        _ => Ok(SessionDeleteMethod::Unlink),
    }
}

/// Show the full-screen session picker; returns the selected session path, or
/// `None` when the user cancels (Esc / Ctrl+C / Ctrl+D). Single-list callers
/// (the `-r/--resume` CLI fast path) get one scope; Tab toggles between two
/// identical lists.
pub fn pick_session(sessions: &[SessionSummary]) -> Option<std::path::PathBuf> {
    pick_session_with_all(sessions, sessions)
}

/// Scope-aware picker (upstream `session-selector.ts`): Tab switches the list
/// between the current-directory sessions and all sessions (`listAll`,
/// session-selector.ts:559-564).
pub fn pick_session_with_all(
    scoped: &[SessionSummary],
    all: &[SessionSummary],
) -> Option<std::path::PathBuf> {
    // TUI callers already own a terminal; a failure to enter the picker is
    // indistinguishable from a cancel there, so keep the historical shape.
    pick_session_with_all_checked(scoped, all).ok().flatten()
}

/// [`pick_session_with_all`] that distinguishes "the user cancelled" from
/// "the picker cannot run in this environment". The CLI `-r/--resume` path
/// uses this so a non-TTY invocation (for example `pir interactive -r` with
/// piped stdin) reports a diagnostic and a non-zero exit instead of silently
/// exiting 0 as if the user had pressed Esc.
pub fn pick_session_with_all_checked(
    scoped: &[SessionSummary],
    all: &[SessionSummary],
) -> Result<Option<std::path::PathBuf>, String> {
    use std::io::IsTerminal;

    let scoped_rows: Vec<SessionRow> = scoped.iter().map(SessionRow::from).collect();
    let all_rows: Vec<SessionRow> = all.iter().map(SessionRow::from).collect();
    if scoped_rows.is_empty() && all_rows.is_empty() {
        return Ok(None);
    }
    let mut state = PickerState::with_all(scoped_rows, all_rows);

    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        return Err(
            "the session picker needs an interactive terminal; pass --session <path> or \
             use a non-interactive mode to resume without a prompt"
                .to_owned(),
        );
    }
    if terminal::enable_raw_mode().is_err() {
        return Err("cannot enable raw terminal mode for the session picker".to_owned());
    }
    let mut screen_entered = false;
    if crossterm::execute!(std::io::stdout(), EnterAlternateScreen).is_ok() {
        screen_entered = true;
    }
    let result = picker_loop(&mut state);
    let _ = crossterm::execute!(std::io::stdout(), LeaveAlternateScreen);
    let _ = terminal::disable_raw_mode();
    if screen_entered {
        let _ = std::io::stdout().flush();
    }
    Ok(result)
}

/// The mutable picker interaction state (upstream `SessionList` +
/// `SessionSelectorHeader` fields).
pub struct PickerState {
    /// The current-directory session rows (the default scope).
    pub rows: Vec<SessionRow>,
    /// The all-sessions rows (upstream `listAll` scope; equal to `rows` for
    /// single-list callers).
    pub all_rows: Vec<SessionRow>,
    /// Whether the all-sessions list is active (Tab toggles).
    pub list_all: bool,
    pub query: String,
    pub sort_mode: SortMode,
    pub named_only: bool,
    pub show_path: bool,
    /// Highlighted position in the display order.
    pub selected: usize,
    /// Session awaiting delete confirmation (upstream
    /// `confirmingDeletePath`).
    pub confirm_delete: Option<PathBuf>,
    /// Active rename input (upstream `mode == "rename"`).
    pub rename: Option<(PathBuf, String)>,
    /// Transient status line + whether it is an error.
    pub status: Option<(String, bool)>,
}

impl PickerState {
    pub fn new(rows: Vec<SessionRow>) -> Self {
        Self::with_all(rows.clone(), rows)
    }

    /// Both scopes up front: the current-directory rows and the full list
    /// (upstream session-selector `listAll`).
    pub fn with_all(scoped: Vec<SessionRow>, all: Vec<SessionRow>) -> Self {
        Self {
            rows: scoped,
            all_rows: all,
            list_all: false,
            query: String::new(),
            sort_mode: SortMode::Threaded,
            named_only: false,
            show_path: false,
            selected: 0,
            confirm_delete: None,
            rename: None,
            status: None,
        }
    }

    /// The rows of the active scope (current directory vs all sessions).
    fn active_rows(&self) -> &Vec<SessionRow> {
        if self.list_all {
            &self.all_rows
        } else {
            &self.rows
        }
    }

    /// Toggle the scope (upstream Tab → `listAll`): the display rebuilds from
    /// the other list and the highlight restarts at the top.
    pub fn toggle_list_all(&mut self) {
        self.list_all = !self.list_all;
        self.selected = 0;
    }

    /// Toggle the named-only filter (upstream `app.session.toggleNamedFilter`).
    pub fn toggle_named_filter(&mut self) {
        self.named_only = !self.named_only;
    }

    /// Cycle the sort mode (upstream `app.session.toggleSort`).
    pub fn cycle_sort_mode(&mut self) {
        self.sort_mode = self.sort_mode.next();
    }

    /// Toggle the session-path display (upstream `app.session.togglePath`).
    pub fn toggle_path(&mut self) {
        self.show_path = !self.show_path;
    }

    /// Start delete confirmation for the selected session (upstream
    /// `startDeleteConfirmationForSelectedSession`). `selected` is the path
    /// of the highlighted row; `Ok` rows only.
    pub fn start_delete_confirm(&mut self, selected: Option<PathBuf>) -> Result<(), String> {
        let Some(path) = selected else {
            return Ok(());
        };
        self.confirm_delete = Some(path);
        Ok(())
    }

    /// Apply the rename through the session manager (upstream
    /// `renameSession` → `mgr.appendSessionInfo(next)`) and refresh the row
    /// in both scope lists.
    pub fn apply_rename(&mut self, path: &PathBuf, next_name: &str) -> Result<(), String> {
        let next = next_name.trim();
        if next.is_empty() {
            return Ok(());
        }
        rename_session(path, next)?;
        for row in self.rows.iter_mut().chain(self.all_rows.iter_mut()) {
            if &row.path == path {
                row.name = Some(next.to_string());
            }
        }
        Ok(())
    }

    /// Delete the confirmed session file and drop its row from both scope
    /// lists (upstream `onDeleteSession` → `deleteSessionFile`).
    pub fn apply_delete(&mut self, path: &PathBuf) -> Result<SessionDeleteMethod, String> {
        let method = delete_session(path)?;
        self.rows.retain(|row| &row.path != path);
        self.all_rows.retain(|row| &row.path != path);
        Ok(method)
    }
}

/// One display row: the backing row index plus its threaded-tree metadata
/// when the threaded sort is active.
#[derive(Debug, Clone)]
struct DisplayRow {
    row: usize,
    node: Option<ThreadedNode>,
}

/// The display order for the current query/sort/filter state (upstream
/// `filterSessions`), built from the active scope list.
fn display_rows(state: &PickerState) -> Vec<DisplayRow> {
    let rows = state.active_rows();
    let order = filter_rows(rows, &state.query, state.sort_mode, state.named_only);
    let threaded = state.sort_mode == SortMode::Threaded && state.query.trim().is_empty();
    let threaded_nodes: Vec<ThreadedNode> = if threaded {
        build_threaded_order(rows)
    } else {
        Vec::new()
    };
    order
        .into_iter()
        .map(|row_index| DisplayRow {
            row: row_index,
            node: if threaded {
                threaded_nodes
                    .iter()
                    .find(|node| node.row == row_index)
                    .cloned()
            } else {
                None
            },
        })
        .collect()
}

/// The highlighted session path (or `None` on an empty list).
fn selected_path(state: &PickerState, display: &[DisplayRow]) -> Option<PathBuf> {
    display
        .get(state.selected)
        .map(|entry| state.active_rows()[entry.row].path.clone())
}

/// Clamp the selection after any list change (upstream `filterSessions`).
fn clamp_selection(state: &mut PickerState, display: &[DisplayRow]) {
    let last = display.len().saturating_sub(1);
    state.selected = state.selected.min(last);
}

/// Input/render loop, split out for clarity; `Ok(path)` = selection.
fn picker_loop(state: &mut PickerState) -> Option<std::path::PathBuf> {
    let mut status_at: Option<std::time::Instant> = None;
    loop {
        let display = display_rows(state);
        clamp_selection(state, &display);
        let selected = selected_path(state, &display);
        render(state, &display);
        if let Some(at) = status_at {
            if at.elapsed() > std::time::Duration::from_secs(2) {
                state.status = None;
                status_at = None;
            }
        }
        match crossterm::event::read() {
            Err(_) => return None,
            Ok(Event::Key(key)) if key.kind == KeyEventKind::Press => {
                // Delete confirmation intercepts every key (upstream
                // `session-selector.ts:544-557`).
                if state.confirm_delete.is_some() {
                    match key.code {
                        KeyCode::Enter => {
                            if let Some(path) = state.confirm_delete.take() {
                                match state.apply_delete(&path) {
                                    Ok(method) => {
                                        state.status =
                                            Some((method.status_message().to_string(), false));
                                        status_at = Some(std::time::Instant::now());
                                    }
                                    Err(error) => {
                                        state.status =
                                            Some((format!("Failed to delete: {error}"), true));
                                        status_at = Some(std::time::Instant::now());
                                    }
                                }
                            }
                        }
                        KeyCode::Esc => {
                            state.confirm_delete = None;
                        }
                        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                            clear();
                            return None;
                        }
                        _ => {}
                    }
                    continue;
                }
                // Rename input intercepts every key (upstream `mode ==
                // "rename"`).
                if state.rename.is_some() {
                    match key.code {
                        KeyCode::Enter => {
                            let (path, buffer) = state.rename.take().unwrap_or_default();
                            match state.apply_rename(&path, &buffer) {
                                Ok(()) => {
                                    state.status = Some(("Session renamed".to_string(), false));
                                }
                                Err(error) => {
                                    state.status = Some((format!("Rename failed: {error}"), true));
                                }
                            }
                            status_at = Some(std::time::Instant::now());
                        }
                        KeyCode::Esc => {
                            state.rename = None;
                        }
                        KeyCode::Backspace => {
                            if let Some((_, buffer)) = state.rename.as_mut() {
                                buffer.pop();
                            }
                        }
                        KeyCode::Char(character)
                            if key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT =>
                        {
                            if let Some((_, buffer)) = state.rename.as_mut() {
                                buffer.push(character);
                            }
                        }
                        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                            clear();
                            return None;
                        }
                        _ => {}
                    }
                    continue;
                }
                match key.code {
                    KeyCode::Up => {
                        state.selected = state.selected.saturating_sub(1);
                    }
                    KeyCode::Down => {
                        state.selected = (state.selected + 1).min(display.len().saturating_sub(1));
                    }
                    KeyCode::PageUp => {
                        state.selected = state.selected.saturating_sub(8);
                    }
                    KeyCode::PageDown => {
                        state.selected = (state.selected + 8).min(display.len().saturating_sub(1));
                    }
                    // Sort mode cycle (upstream `app.session.toggleSort`).
                    KeyCode::Char('s') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        state.cycle_sort_mode();
                    }
                    // Scope toggle: current directory ↔ all sessions (upstream
                    // `session-selector.ts:559-564`, listAll).
                    KeyCode::Tab => {
                        state.toggle_list_all();
                    }
                    // Named-only filter (upstream
                    // `app.session.toggleNamedFilter`).
                    KeyCode::Char('n') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        state.toggle_named_filter();
                    }
                    // Path display toggle (upstream `app.session.togglePath`).
                    KeyCode::Char('p') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        state.toggle_path();
                    }
                    // Rename (upstream `app.session.rename`).
                    KeyCode::Char('r') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        if let Some(path) = selected.clone() {
                            let current = state
                                .active_rows()
                                .iter()
                                .find(|row| row.path == path)
                                .and_then(|row| row.name.clone())
                                .unwrap_or_default();
                            state.rename = Some((path, current));
                        }
                    }
                    // Delete (upstream `app.session.delete`).
                    KeyCode::Char('d') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        if let Some(path) = selected.clone() {
                            let _ = state.start_delete_confirm(Some(path));
                        }
                    }
                    // Non-invasive delete alias (upstream
                    // `app.session.deleteNoninvasive`): only triggers with an
                    // empty query, otherwise forwarded to the input.
                    KeyCode::Backspace if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        if state.query.is_empty() {
                            if let Some(path) = selected.clone() {
                                let _ = state.start_delete_confirm(Some(path));
                            }
                        } else {
                            state.query.pop();
                        }
                    }
                    KeyCode::Enter => {
                        clear();
                        return selected;
                    }
                    KeyCode::Esc => {
                        clear();
                        return None;
                    }
                    KeyCode::Char('c') | KeyCode::Char('d')
                        if key.modifiers.contains(KeyModifiers::CONTROL) =>
                    {
                        clear();
                        return None;
                    }
                    KeyCode::Backspace => {
                        state.query.pop();
                    }
                    KeyCode::Char(character) => {
                        state.query.push(character);
                    }
                    _ => {}
                }
            }
            Ok(_) => {}
        }
    }
}

fn clear() {
    let mut stdout = std::io::stdout();
    let _ = crossterm::execute!(stdout, LeaveAlternateScreen);
}

/// Width used for rendering when the terminal size cannot be queried.
const FALLBACK_WIDTH: usize = 100;

/// Max visible rows (upstream `SessionList.maxVisible`).
const MAX_VISIBLE: usize = 10;

fn render(state: &PickerState, display: &[DisplayRow]) {
    let width = crossterm::terminal::size()
        .map(|(columns, _)| columns as usize)
        .unwrap_or(FALLBACK_WIDTH);
    let theme = |color: crate::tui::ThemeColor, text: &str| {
        crate::tui::with_theme(|theme| theme.fg(color, text))
    };
    let mut frame: Vec<String> = Vec::new();

    // Title + right side: scope/name/sort state (upstream
    // `SessionSelectorHeader.render`).
    let sort_label = state.sort_mode.label();
    let name_label = if state.named_only { "Named" } else { "All" };
    let scope_label = if state.list_all {
        "All dirs"
    } else {
        "Current dir"
    };
    let path_state = if state.show_path { "(on)" } else { "(off)" };
    frame.push(theme(crate::tui::ThemeColor::Accent, "Resume a session"));
    frame.push(theme(
        crate::tui::ThemeColor::Muted,
        &format!(
            "Sort: {sort_label} · Name: {name_label} · Scope: {scope_label} · path {path_state}"
        ),
    ));

    // Hint lines (upstream header hint block).
    match (&state.confirm_delete, &state.status, &state.rename) {
        (Some(_), _, _) => {
            frame.push(theme(
                crate::tui::ThemeColor::Error,
                "Delete session? Enter confirm · Esc cancel",
            ));
        }
        (_, Some((message, is_error)), _) => {
            frame.push(theme(
                if *is_error {
                    crate::tui::ThemeColor::Error
                } else {
                    crate::tui::ThemeColor::Accent
                },
                message,
            ));
        }
        (_, _, Some((_, buffer))) => {
            frame.push(theme(crate::tui::ThemeColor::Accent, "Rename Session"));
            frame.push(format!("> {buffer}"));
            frame.push(theme(
                crate::tui::ThemeColor::Muted,
                "Enter to save · Esc to cancel",
            ));
        }
        _ => {
            frame.push(theme(
                crate::tui::ThemeColor::Muted,
                "Type to filter · re:<pattern> regex · \"phrase\" exact (upstream)",
            ));
            frame.push(theme(
                crate::tui::ThemeColor::Muted,
                "ctrl+s sort · ctrl+n named · ctrl+d delete · ctrl+p path · ctrl+r rename · Tab all/dir · Enter resume · Esc cancel",
            ));
        }
    }
    frame.push(String::new());

    if display.is_empty() {
        let message = if state.named_only {
            "No named sessions found. Press ctrl+n to show all."
        } else {
            "No sessions found"
        };
        frame.push(theme(crate::tui::ThemeColor::Muted, message));
    } else {
        let now = crate::ai::util::now_ms() as u128;
        let home =
            crate::session_manager::home_dir().map(|path| path.to_string_lossy().into_owned());
        let start = state
            .selected
            .saturating_sub(MAX_VISIBLE / 2)
            .min(display.len().saturating_sub(MAX_VISIBLE.min(display.len())));
        let end = (start + MAX_VISIBLE).min(display.len());
        for (index, entry) in display[start..end].iter().enumerate() {
            let index = start + index;
            let row = &state.active_rows()[entry.row];
            let is_selected = index == state.selected;
            let confirming = state
                .confirm_delete
                .as_ref()
                .is_some_and(|path| path == &row.path);
            let cursor = if is_selected { "› " } else { "  " };
            let prefix = entry.node.as_ref().map(tree_prefix).unwrap_or_default();
            let display_text = row
                .name
                .clone()
                .unwrap_or_else(|| row.first_message.clone());
            let mut right = format!(
                "{} {}",
                row.message_count,
                format_session_age(row.modified_ms, now)
            );
            if state.show_path {
                right = format!(
                    "{} {}",
                    shorten_path(&row.path.display().to_string(), home.as_deref()),
                    right
                );
            }
            let line = format!("{cursor}{prefix}{display_text}  {right}");
            let styled = if confirming {
                theme(crate::tui::ThemeColor::Error, &line)
            } else if row.has_name() {
                theme(crate::tui::ThemeColor::Warning, &line)
            } else {
                line
            };
            frame.push(truncate_to_width(&styled, width, "…"));
        }
        if start > 0 || end < display.len() {
            frame.push(theme(
                crate::tui::ThemeColor::Muted,
                &format!("  ({}/{})", state.selected + 1, display.len()),
            ));
        }
    }

    let mut stdout = std::io::stdout();
    let _ = crossterm::execute!(
        stdout,
        crossterm::cursor::MoveTo(0, 0),
        crossterm::terminal::Clear(crossterm::terminal::ClearType::All)
    );
    for line in &frame {
        let _ = writeln!(stdout, "{}", truncate_to_width(line, width, "…"));
    }
    let _ = stdout.flush();
}
