//! Upstream-parity pins for every default combination key: pir's effective
//! keybinding tables (defaults + platform branches) must equal upstream pi's.
//!
//! Truth sources, both transcribed verbatim from the local `pi` checkout:
//!
//! 1. **tui-package layer** — `pi/packages/tui/src/keybindings.ts:71-210`
//!    (`TUI_KEYBINDINGS`, 47 ids) → [`UPSTREAM_TUI_KEYBINDINGS`].
//! 2. **coding-agent layer** — `pi/packages/coding-agent/src/core/
//!    keybindings.ts:26-171` (`APP_KEYBINDINGS`, 43 ids) →
//!    [`UPSTREAM_APP_KEYBINDINGS`], plus the `KEYBINDINGS` merge at
//!    `core/keybindings.ts:180-199` which rebinds four tui ids per platform →
//!    [`upstream_tui_effective`].
//!
//! Platform semantics (`core/keybindings.ts:16-21, 29-32, 45-77, 182-196`):
//!
//! - `native_windows` = `process.platform === "win32"`.
//! - `windows_keybindings` = `useWindowsKeybindings()` = win32 OR Linux+WSL.
//!
//! They differ: on WSL `app.suspend` is `ctrl+z` and `tui.editor.undo` is
//! `alt+z`, while on native win32 `app.suspend` is EMPTY (the chord must not
//! respond) and `tui.editor.undo` is `ctrl+z`.
//!
//! **Spelling normalization.** The embedded tables keep upstream's literal
//! spellings (`escape`, `pageUp`, `shift+ctrl+p`, …) while pir's resolved
//! tables use the canonical ones (`esc`, `pageup`, `ctrl+shift+p`, …). Every
//! comparison therefore runs both sides through
//! `tui::keybindings::canonical_key_spec` (parse + canonical render, i.e. the
//! alias table is `parse_key`'s own name table — see [`SPELLING_ALIASES`]).
//! This never relaxes the comparison: same chord count, same order, and two
//! different keys can never canonicalize equal — the module also pins the raw
//! (pre-normalization) tables against the documented alias whitelist.

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use pir::config::keybindings::{
    default_keybinding_definitions_for, use_windows_keybindings, KeybindingPlatform,
};
use pir::tui::keybindings::{
    canonical_key_spec, hint_default_keys_for, parse_key, tui_default_keys_for,
    tui_effective_default_keys_for, tui_keybinding_definition, Keybindings, TuiAction,
    TUI_KEYBINDING_DEFINITIONS,
};
use pir::tui::selector::TreeFilterMode;
use pir::tui::terminal::{
    delete_char_backward_key, delete_char_forward_key, scoped_models_key_action,
    session_action_key, sessions_key_action, tree_filter_key_action, tree_filter_mode_after_key,
    ScopedModelsKeyAction, SessionActionKey, SessionsKeyAction, SessionsViewOptions,
    TreeFilterKeyAction,
};

// ---------------------------------------------------------------------------
// Platform rows
// ---------------------------------------------------------------------------

const WIN32: KeybindingPlatform = KeybindingPlatform {
    native_windows: true,
    windows_keybindings: true,
};
const WSL: KeybindingPlatform = KeybindingPlatform {
    native_windows: false,
    windows_keybindings: true,
};
const OTHER: KeybindingPlatform = KeybindingPlatform {
    native_windows: false,
    windows_keybindings: false,
};

const PLATFORM_ROWS: &[(&str, KeybindingPlatform)] = &[
    ("native win32", WIN32),
    ("wsl", WSL),
    ("linux/macos", OTHER),
];

/// (id, default chords, description) — verbatim, in upstream order.
type UpstreamRow = (&'static str, &'static [&'static str], &'static str);

// ---------------------------------------------------------------------------
// Layer 1: `TUI_KEYBINDINGS` (packages/tui/src/keybindings.ts:71-210)
// ---------------------------------------------------------------------------

const UPSTREAM_TUI_KEYBINDINGS: &[UpstreamRow] = &[
    ("tui.editor.cursorUp", &["up"], "Move cursor up"),
    ("tui.editor.cursorDown", &["down"], "Move cursor down"),
    (
        "tui.editor.historyPrevious",
        &[],
        "Select previous prompt history entry",
    ),
    (
        "tui.editor.historyNext",
        &[],
        "Select next prompt history entry",
    ),
    (
        "tui.editor.cursorLeft",
        &["left", "ctrl+b"],
        "Move cursor left",
    ),
    (
        "tui.editor.cursorRight",
        &["right", "ctrl+f"],
        "Move cursor right",
    ),
    (
        "tui.editor.cursorWordLeft",
        &["alt+left", "ctrl+left", "alt+b"],
        "Move cursor word left",
    ),
    (
        "tui.editor.cursorWordRight",
        &["alt+right", "ctrl+right", "alt+f"],
        "Move cursor word right",
    ),
    (
        "tui.editor.cursorLineStart",
        &["home", "ctrl+a"],
        "Move to line start",
    ),
    (
        "tui.editor.cursorLineEnd",
        &["end", "ctrl+e"],
        "Move to line end",
    ),
    (
        "tui.editor.jumpForward",
        &["ctrl+]"],
        "Jump forward to character",
    ),
    (
        "tui.editor.jumpBackward",
        &["ctrl+alt+]"],
        "Jump backward to character",
    ),
    ("tui.editor.pageUp", &["pageUp", "ctrl+pageUp"], "Page up"),
    (
        "tui.editor.pageDown",
        &["pageDown", "ctrl+pageDown"],
        "Page down",
    ),
    (
        "tui.editor.deleteCharBackward",
        &["backspace"],
        "Delete character backward",
    ),
    (
        "tui.editor.deleteCharForward",
        &["delete", "ctrl+d"],
        "Delete character forward",
    ),
    (
        "tui.editor.deleteWordBackward",
        &["ctrl+w", "alt+backspace"],
        "Delete word backward",
    ),
    (
        "tui.editor.deleteWordForward",
        &["alt+d", "alt+delete"],
        "Delete word forward",
    ),
    (
        "tui.editor.deleteToLineStart",
        &["ctrl+u"],
        "Delete to line start",
    ),
    (
        "tui.editor.deleteToLineEnd",
        &["ctrl+k"],
        "Delete to line end",
    ),
    ("tui.editor.yank", &["ctrl+y"], "Yank"),
    ("tui.editor.yankPop", &["alt+y"], "Yank pop"),
    ("tui.editor.undo", &["ctrl+-"], "Undo"),
    (
        "tui.input.newLine",
        &["shift+enter", "ctrl+j"],
        "Insert newline",
    ),
    ("tui.input.submit", &["enter"], "Submit input"),
    ("tui.input.tab", &["tab"], "Tab / autocomplete"),
    ("tui.input.copy", &["ctrl+c"], "Copy selection"),
    ("tui.select.up", &["up"], "Move selection up"),
    ("tui.select.down", &["down"], "Move selection down"),
    ("tui.select.pageUp", &["pageUp"], "Selection page up"),
    ("tui.select.pageDown", &["pageDown"], "Selection page down"),
    ("tui.select.confirm", &["enter"], "Confirm selection"),
    (
        "tui.select.cancel",
        &["escape", "ctrl+c"],
        "Cancel selection",
    ),
    (
        "tui.altScreen.pageUp",
        &["pageUp"],
        "Scroll viewport up one page",
    ),
    (
        "tui.altScreen.pageDown",
        &["pageDown"],
        "Scroll viewport down one page",
    ),
    (
        "tui.altScreen.halfPageUp",
        &[],
        "Scroll viewport up half a page",
    ),
    (
        "tui.altScreen.halfPageDown",
        &[],
        "Scroll viewport down half a page",
    ),
    ("tui.altScreen.lineUp", &[], "Scroll viewport up one line"),
    (
        "tui.altScreen.lineDown",
        &[],
        "Scroll viewport down one line",
    ),
    (
        "tui.altScreen.previousPrompt",
        &["ctrl+shift+up", "ctrl+up"],
        "Jump to previous semantic prompt",
    ),
    (
        "tui.altScreen.nextPrompt",
        &["ctrl+shift+down", "ctrl+down"],
        "Jump to next semantic prompt",
    ),
    (
        "tui.altScreen.search",
        &["ctrl+shift+f"],
        "Search the primary scroll view",
    ),
    (
        "tui.altScreen.searchNext",
        &["enter", "ctrl+g"],
        "Select the next search match",
    ),
    (
        "tui.altScreen.searchPrevious",
        &["shift+enter", "ctrl+shift+g"],
        "Select the previous search match",
    ),
    (
        "tui.altScreen.searchClose",
        &["escape"],
        "Close transcript search",
    ),
    (
        "tui.altScreen.top",
        &["ctrl+home"],
        "Scroll viewport to top",
    ),
    (
        "tui.altScreen.bottom",
        &["ctrl+end"],
        "Scroll viewport to bottom",
    ),
];

/// `KEYBINDINGS` — the coding-agent layer's four tui-id overrides
/// (`core/keybindings.ts:180-199`). Every other id keeps the layer-1 row.
fn upstream_tui_effective(id: &str, platform: KeybindingPlatform) -> Vec<&'static str> {
    match id {
        // `process.platform === "win32" ? "ctrl+z" : windowsKeybindings ? "alt+z" : "ctrl+-"`
        "tui.editor.undo" if platform.native_windows => vec!["ctrl+z"],
        "tui.editor.undo" if platform.windows_keybindings => vec!["alt+z"],
        "tui.editor.undo" => vec!["ctrl+-"],
        // `windowsKeybindings ? "ctrl+up" : ["ctrl+shift+up", "ctrl+up"]`
        "tui.altScreen.previousPrompt" if platform.windows_keybindings => vec!["ctrl+up"],
        "tui.altScreen.previousPrompt" => vec!["ctrl+shift+up", "ctrl+up"],
        "tui.altScreen.nextPrompt" if platform.windows_keybindings => vec!["ctrl+down"],
        "tui.altScreen.nextPrompt" => vec!["ctrl+shift+down", "ctrl+down"],
        // `windowsKeybindings ? "ctrl+f" : "ctrl+shift+f"`
        "tui.altScreen.search" if platform.windows_keybindings => vec!["ctrl+f"],
        "tui.altScreen.search" => vec!["ctrl+shift+f"],
        _ => upstream_tui_row(id).unwrap_or_default(),
    }
}

fn upstream_tui_row(id: &str) -> Option<Vec<&'static str>> {
    UPSTREAM_TUI_KEYBINDINGS
        .iter()
        .find(|(row_id, _, _)| *row_id == id)
        .map(|(_, keys, _)| keys.to_vec())
}

/// Upstream tui ids pir deliberately does not carry in
/// [`TUI_KEYBINDING_DEFINITIONS`], with where the behavior actually lives:
///
/// - `tui.input.copy` (ctrl+c): upstream's editor returns early and the parent
///   handles ctrl+c — pir dispatches `TuiAction::Clear` (`app.clear`) for it,
///   which is exactly the parent behavior.
/// - the 14 `tui.altScreen.*` ids: pir routes the viewport family from
///   `altscreen_search.rs` (app layer), not through the tui registry; three of
///   them carry their platform variants in
///   `config::keybindings::default_keybinding_definitions_for`.
const NOT_IN_TUI_REGISTRY: &[&str] = &[
    "tui.input.copy",
    "tui.altScreen.pageUp",
    "tui.altScreen.pageDown",
    "tui.altScreen.halfPageUp",
    "tui.altScreen.halfPageDown",
    "tui.altScreen.lineUp",
    "tui.altScreen.lineDown",
    "tui.altScreen.previousPrompt",
    "tui.altScreen.nextPrompt",
    "tui.altScreen.search",
    "tui.altScreen.searchNext",
    "tui.altScreen.searchPrevious",
    "tui.altScreen.searchClose",
    "tui.altScreen.top",
    "tui.altScreen.bottom",
];

/// Upstream tui ids whose default chords pir resolves from the app-layer
/// registry (`config::keybindings`) because that is where their platform
/// branch lives.
const APP_REGISTRY_TUI_IDS: &[&str] = &[
    "tui.altScreen.previousPrompt",
    "tui.altScreen.nextPrompt",
    "tui.altScreen.search",
];

/// Upstream tui ids whose chords the alt-screen router matches as literal key
/// shapes (`altscreen_search::route_alt_screen_key`); the entries are the
/// upstream defaults the router must honor.
const ROUTER_HARDCODED_TUI_IDS: &[(&str, &[&str])] = &[
    ("tui.altScreen.pageUp", &["pageUp"]),
    ("tui.altScreen.pageDown", &["pageDown"]),
    ("tui.altScreen.searchNext", &["enter", "ctrl+g"]),
    (
        "tui.altScreen.searchPrevious",
        &["shift+enter", "ctrl+shift+g"],
    ),
    ("tui.altScreen.searchClose", &["escape"]),
    ("tui.altScreen.top", &["ctrl+home"]),
    ("tui.altScreen.bottom", &["ctrl+end"]),
];

// ---------------------------------------------------------------------------
// Layer 2: `APP_KEYBINDINGS` (core/keybindings.ts:26-171)
// ---------------------------------------------------------------------------

/// The platform-branched rows carry their non-Windows default here and are
/// re-resolved by [`upstream_app_effective`].
const UPSTREAM_APP_KEYBINDINGS: &[UpstreamRow] = &[
    ("app.interrupt", &["escape"], "Cancel or abort"),
    ("app.clear", &["ctrl+c"], "Clear editor"),
    ("app.exit", &["ctrl+d"], "Exit when editor is empty"),
    // win32: EMPTY (keybindings.ts:29-32)
    ("app.suspend", &["ctrl+z"], "Suspend to background"),
    ("app.thinking.cycle", &["shift+tab"], "Cycle thinking level"),
    ("app.thinking.save", &["ctrl+s"], "Save thinking level"),
    ("app.model.cycleForward", &["ctrl+p"], "Cycle to next model"),
    // win: alt+p (keybindings.ts:45-48)
    (
        "app.model.cycleBackward",
        &["shift+ctrl+p"],
        "Cycle to previous model",
    ),
    ("app.model.select", &["ctrl+l"], "Open model selector"),
    ("app.tools.expand", &["ctrl+o"], "Toggle tool output"),
    ("app.thinking.toggle", &["ctrl+t"], "Toggle thinking blocks"),
    (
        "app.session.toggleNamedFilter",
        &["ctrl+n"],
        "Toggle named session filter",
    ),
    ("app.editor.external", &["ctrl+g"], "Open external editor"),
    (
        "app.message.copy",
        &["ctrl+x"],
        "Copy selection or last assistant message",
    ),
    // win: ctrl+q (keybindings.ts:67-70)
    (
        "app.message.followUp",
        &["alt+enter"],
        "Queue follow-up message",
    ),
    // win: alt+q (keybindings.ts:71-74)
    (
        "app.message.dequeue",
        &["alt+up"],
        "Restore queued messages",
    ),
    // win: alt+v (keybindings.ts:75-78)
    (
        "app.clipboard.pasteImage",
        &["ctrl+v"],
        "Paste files on macOS, images, or text from clipboard",
    ),
    ("app.session.new", &[], "Start a new session"),
    ("app.session.tree", &[], "Open session tree"),
    ("app.session.fork", &[], "Fork current session"),
    ("app.session.resume", &[], "Resume a session"),
    // darwin swaps the order only; same chord set (keybindings.ts:83-90)
    (
        "app.tree.foldOrUp",
        &["ctrl+left", "alt+left"],
        "Fold tree branch or move up",
    ),
    (
        "app.tree.unfoldOrDown",
        &["ctrl+right", "alt+right"],
        "Unfold tree branch or move down",
    ),
    ("app.tree.editLabel", &["shift+l"], "Edit tree label"),
    (
        "app.tree.toggleLabelTimestamp",
        &["shift+t"],
        "Toggle tree label timestamps",
    ),
    (
        "app.session.togglePath",
        &["ctrl+p"],
        "Toggle session path display",
    ),
    (
        "app.session.toggleSort",
        &["ctrl+s"],
        "Toggle session sort mode",
    ),
    ("app.session.rename", &["ctrl+r"], "Rename session"),
    ("app.session.delete", &["ctrl+d"], "Delete session"),
    (
        "app.session.deleteNoninvasive",
        &["ctrl+backspace"],
        "Delete session when query is empty",
    ),
    ("app.models.save", &["ctrl+s"], "Save model selection"),
    ("app.models.enableAll", &["ctrl+a"], "Enable all models"),
    ("app.models.clearAll", &["ctrl+x"], "Clear all models"),
    (
        "app.models.toggleProvider",
        &["ctrl+p"],
        "Toggle all models for provider",
    ),
    (
        "app.models.reorderUp",
        &["alt+up"],
        "Move model up in order",
    ),
    (
        "app.models.reorderDown",
        &["alt+down"],
        "Move model down in order",
    ),
    (
        "app.tree.filter.default",
        &["ctrl+d"],
        "Tree filter: default view",
    ),
    (
        "app.tree.filter.noTools",
        &["ctrl+t"],
        "Tree filter: hide tool results",
    ),
    (
        "app.tree.filter.userOnly",
        &["ctrl+u"],
        "Tree filter: user messages only",
    ),
    (
        "app.tree.filter.labeledOnly",
        &["ctrl+l"],
        "Tree filter: labeled entries only",
    ),
    (
        "app.tree.filter.all",
        &["ctrl+a"],
        "Tree filter: show all entries",
    ),
    (
        "app.tree.filter.cycleForward",
        &["ctrl+o"],
        "Tree filter: cycle forward",
    ),
    (
        "app.tree.filter.cycleBackward",
        &["shift+ctrl+o"],
        "Tree filter: cycle backward",
    ),
];

/// The platform-branched rows of `APP_KEYBINDINGS`
/// (`core/keybindings.ts:29-32, 45-46, 67-77, 83-90`).
fn upstream_app_effective(id: &str, platform: KeybindingPlatform) -> Vec<&'static str> {
    match id {
        // `process.platform === "win32" ? [] : "ctrl+z"`
        "app.suspend" if platform.native_windows => vec![],
        "app.suspend" => vec!["ctrl+z"],
        // `windowsKeybindings ? "alt+p" : "shift+ctrl+p"`
        "app.model.cycleBackward" if platform.windows_keybindings => vec!["alt+p"],
        "app.model.cycleBackward" => vec!["shift+ctrl+p"],
        // `windowsKeybindings ? "ctrl+q" : "alt+enter"`
        "app.message.followUp" if platform.windows_keybindings => vec!["ctrl+q"],
        "app.message.followUp" => vec!["alt+enter"],
        // `windowsKeybindings ? "alt+q" : "alt+up"`
        "app.message.dequeue" if platform.windows_keybindings => vec!["alt+q"],
        "app.message.dequeue" => vec!["alt+up"],
        // `windowsKeybindings ? "alt+v" : "ctrl+v"`
        "app.clipboard.pasteImage" if platform.windows_keybindings => vec!["alt+v"],
        "app.clipboard.pasteImage" => vec!["ctrl+v"],
        _ => upstream_app_row(id).unwrap_or_default(),
    }
}

fn upstream_app_row(id: &str) -> Option<Vec<&'static str>> {
    UPSTREAM_APP_KEYBINDINGS
        .iter()
        .find(|(row_id, _, _)| *row_id == id)
        .map(|(_, keys, _)| keys.to_vec())
}

/// App ids whose upstream defaults pir resolves from the startup-hint table
/// (`tui::keybindings::hint_default_keys_for`) instead of the config registry.
const HINT_TABLE_APP_IDS: &[&str] = &[
    "app.interrupt",
    "app.clear",
    "app.exit",
    "app.suspend",
    "app.thinking.cycle",
    "app.thinking.toggle",
    "app.model.cycleForward",
    "app.model.cycleBackward",
    "app.model.select",
    "app.tools.expand",
    "app.message.followUp",
    "app.clipboard.pasteImage",
    // `app.session.*` (core/keybindings.ts:79-118): the four empty navigation
    // ids plus the six session-selector ids, resolved from the same hint
    // table now that they accept live overrides.
    "app.session.new",
    "app.session.tree",
    "app.session.fork",
    "app.session.resume",
    "app.session.toggleNamedFilter",
    "app.session.togglePath",
    "app.session.toggleSort",
    "app.session.rename",
    "app.session.delete",
    "app.session.deleteNoninvasive",
];

/// App ids with NO default-resolution table in pir. Empty since §7 #3 closed:
/// every upstream `APP_KEYBINDINGS` id now resolves from the config registry
/// or the hint table, and the session-navigation ids keep their EMPTY
/// upstream default (`Some(vec![])`) instead of being unresolved.
const UNRESOLVED_APP_IDS: &[&str] = &[];

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn key(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
    KeyEvent {
        code,
        modifiers,
        kind: KeyEventKind::Press,
        state: crossterm::event::KeyEventState::NONE,
    }
}

/// Build the press event for one chord spec. The spec is put through
/// [`canonical`] first, so an upstream literal spelling (`escape`, `pageUp`,
/// `shift+ctrl+p`, …) and pir's canonical spelling (`esc`, `pageup`,
/// `ctrl+shift+p`) build the SAME event: a wiring lookup can never fail on
/// spelling, only on a genuinely different key.
fn event(chord: &str) -> KeyEvent {
    let spec = canonical(chord);
    let combo = parse_key(&spec).unwrap_or_else(|| panic!("chord {spec:?} must parse"));
    let mut modifiers = KeyModifiers::NONE;
    if combo.ctrl {
        modifiers |= KeyModifiers::CONTROL;
    }
    if combo.alt {
        modifiers |= KeyModifiers::ALT;
    }
    if combo.shift {
        modifiers |= KeyModifiers::SHIFT;
    }
    key(combo.code, modifiers)
}

fn strings(keys: &[&str]) -> Vec<String> {
    keys.iter().map(|key| (*key).to_string()).collect()
}

fn registry_defaults(id: &str, platform: KeybindingPlatform) -> Option<Vec<String>> {
    default_keybinding_definitions_for(platform)
        .into_iter()
        .find(|definition| definition.id == id)
        .map(|definition| definition.default_keys)
}

/// The pir-resolved chords for one upstream tui id (registry verbatim, or the
/// app-layer registry for the three platform-branched `tui.altScreen.*` ids,
/// or the documented router-hardcoded upstream row).
fn effective_tui_keys(id: &str, platform: KeybindingPlatform) -> Option<Vec<String>> {
    if tui_keybinding_definition(id).is_some() {
        tui_effective_default_keys_for(id, platform).map(|keys| strings(&keys))
    } else if APP_REGISTRY_TUI_IDS.contains(&id) {
        registry_defaults(id, platform)
    } else {
        upstream_tui_row(id).map(|keys| strings(&keys))
    }
}

/// The pir-resolved chords for one upstream app id, the way the runtime
/// resolves an override-free binding: the config registry first, then the
/// startup-hint table (`app.interrupt`/`clear`/`exit`/`suspend`,
/// `app.thinking.*`, `app.model.*`, `app.tools.expand`, `app.message.followUp`,
/// `app.clipboard.pasteImage` live there). `None` for the documented
/// unresolved ids.
fn effective_app_keys(id: &str, platform: KeybindingPlatform) -> Option<Vec<String>> {
    registry_defaults(id, platform).or_else(|| hint_default_keys_for(id, platform))
}

/// Normalize one chord into pir's canonical spelling through
/// `tui::keybindings::canonical_key_spec` (parse + canonical render). Panics on
/// unparseable chords so a broken table fails loudly instead of comparing
/// unequal.
fn canonical(chord: &str) -> String {
    canonical_key_spec(chord).unwrap_or_else(|| panic!("chord {chord:?} must parse"))
}

/// [`canonical`] over a chord list (order and length preserved).
///
/// This is the ONLY tolerated difference between the verbatim upstream literal
/// tables and pir's resolved tables: upstream writes `escape`, `return`,
/// `pageUp`, `shift+ctrl+p`, … where pir's tables use the canonical `esc`,
/// `enter`, `pageup`, `ctrl+shift+p`. The comparison stays strict — same
/// number of chords, same order, and a different KEY can never canonicalize
/// equal (see [`SPELLING_ALIASES`] for the raw-pair whitelist).
fn canonical_keys(keys: &[String]) -> Vec<String> {
    keys.iter().map(|chord| canonical(chord)).collect()
}

/// Upstream-literal -> pir-canonical spelling pairs that may differ before
/// normalization. Sourced from `parse_key`'s alias table
/// (`src/tui/keybindings.rs`): key names (`escape`/`return`/`del`/`ins`/
/// `pgdn`, case-insensitive `pageUp`), modifier names (`control`, `meta`,
/// `opt`) and the fixed modifier render order (`shift+ctrl+p` ->
/// `ctrl+shift+p`). A raw difference outside this whitelist is a real chord
/// change and must fail.
const SPELLING_ALIASES: &[(&str, &str)] = &[
    ("escape", "esc"),
    ("return", "enter"),
    ("del", "delete"),
    ("ins", "insert"),
    ("pgdn", "pagedown"),
    ("pageUp", "pageup"),
    ("pageDown", "pagedown"),
    ("control", "ctrl"),
    ("meta", "alt"),
    ("opt", "alt"),
];

fn is_documented_alias(upstream: &str, pir: &str) -> bool {
    SPELLING_ALIASES
        .iter()
        .any(|(literal, canonical)| *literal == upstream && *canonical == pir)
}

/// Positional raw comparison: same length, and every differing pair must be a
/// documented spelling alias.
fn assert_raw_differences_are_aliases(what: &str, upstream: &[String], pir: &[String]) {
    assert_eq!(
        upstream.len(),
        pir.len(),
        "{what}: chord count must match (upstream {upstream:?}, pir {pir:?})"
    );
    for (literal, resolved) in upstream.iter().zip(pir) {
        if literal == resolved {
            continue;
        }
        assert!(
            is_documented_alias(literal, resolved),
            "{what}: {literal:?} vs {resolved:?} is not a documented spelling alias"
        );
    }
}

// ---------------------------------------------------------------------------
// 1. Both layers are transcribed verbatim into pir's registries
// ---------------------------------------------------------------------------

#[test]
fn tui_registry_rows_are_upstream_verbatim() {
    for definition in TUI_KEYBINDING_DEFINITIONS {
        let (_, keys, description) = UPSTREAM_TUI_KEYBINDINGS
            .iter()
            .find(|(id, _, _)| *id == definition.id)
            .unwrap_or_else(|| panic!("{} is not an upstream tui id", definition.id));
        assert_eq!(
            definition.default_keys, *keys,
            "{} default chords must be upstream-verbatim",
            definition.id
        );
        assert_eq!(
            definition.description, *description,
            "{} description must be upstream-verbatim",
            definition.id
        );
    }
}

#[test]
fn tui_ids_outside_the_registry_are_the_documented_ones() {
    let registered: Vec<&str> = TUI_KEYBINDING_DEFINITIONS
        .iter()
        .map(|definition| definition.id)
        .collect();
    let mut unregistered: Vec<&str> = UPSTREAM_TUI_KEYBINDINGS
        .iter()
        .map(|(id, _, _)| *id)
        .filter(|id| !registered.contains(id))
        .collect();
    unregistered.sort_unstable();
    let mut expected = NOT_IN_TUI_REGISTRY.to_vec();
    expected.sort_unstable();
    assert_eq!(unregistered, expected);
    // The three platform-branched altScreen ids resolve from the app registry.
    for id in APP_REGISTRY_TUI_IDS {
        assert!(
            registry_defaults(id, OTHER).is_some(),
            "{id} must resolve from the app-layer registry"
        );
    }
    // Every router-hardcoded row is a real upstream id with the documented
    // default.
    for (id, keys) in ROUTER_HARDCODED_TUI_IDS {
        assert_eq!(
            upstream_tui_row(id).as_deref(),
            Some(*keys),
            "{id}: the router must honor the upstream default"
        );
    }
}

// ---------------------------------------------------------------------------
// 2. Effective defaults (platform-resolved) equal upstream
// ---------------------------------------------------------------------------

#[test]
fn tui_effective_defaults_match_upstream_on_every_platform() {
    for (label, platform) in PLATFORM_ROWS {
        for (id, _, _) in UPSTREAM_TUI_KEYBINDINGS {
            let expected = canonical_keys(&strings(&upstream_tui_effective(id, *platform)));
            let actual = effective_tui_keys(id, *platform).map(|keys| canonical_keys(&keys));
            assert_eq!(actual, Some(expected), "{id} on {label}");
        }
    }
    // The tui-package layer stays platform-independent even where the app
    // layer overrides it (upstream `TUI_KEYBINDINGS` vs `KEYBINDINGS`).
    assert_eq!(
        tui_default_keys_for("tui.editor.undo").as_deref(),
        Some(["ctrl+-".to_string()].as_slice()),
        "the tui layer keeps ctrl+-"
    );
}

#[test]
fn app_effective_defaults_match_upstream_on_every_platform() {
    let app_registry: Vec<&str> = default_keybinding_definitions_for(OTHER)
        .iter()
        .map(|definition| definition.id)
        .filter(|id| id.starts_with("app."))
        .collect();
    for (label, platform) in PLATFORM_ROWS {
        for (id, _, _) in UPSTREAM_APP_KEYBINDINGS {
            let expected = canonical_keys(&strings(&upstream_app_effective(id, *platform)));
            if !app_registry.contains(id) && !HINT_TABLE_APP_IDS.contains(id) {
                assert!(
                    UNRESOLVED_APP_IDS.contains(id),
                    "{id} must be resolved or documented as unresolved"
                );
                continue;
            }
            // Classified ids MUST resolve (registry or hint table) and match
            // upstream chord-for-chord after spelling normalization.
            let actual = canonical_keys(
                &effective_app_keys(id, *platform)
                    .unwrap_or_else(|| panic!("{id} must resolve on {label}")),
            );
            assert_eq!(actual, expected, "{id} on {label}");
        }
    }
    // Every upstream app id is classified exactly once.
    let mut classified: Vec<&str> = app_registry.clone();
    classified.extend_from_slice(HINT_TABLE_APP_IDS);
    classified.extend_from_slice(UNRESOLVED_APP_IDS);
    classified.sort_unstable();
    classified.dedup();
    let mut upstream: Vec<&str> = UPSTREAM_APP_KEYBINDINGS
        .iter()
        .map(|(id, _, _)| *id)
        .collect();
    upstream.sort_unstable();
    assert_eq!(classified, upstream);
    // The EMPTY upstream rows resolve to an empty chord list (they are
    // gesture-free until a user binding), not to "unresolved".
    for id in [
        "app.session.new",
        "app.session.tree",
        "app.session.fork",
        "app.session.resume",
    ] {
        assert_eq!(upstream_app_effective(id, WIN32), Vec::<&str>::new());
        assert_eq!(
            effective_app_keys(id, WIN32),
            Some(Vec::<String>::new()),
            "{id} must resolve to an empty default table entry"
        );
    }
}

/// The raw (pre-normalization) comparison: the ONLY raw differences between
/// the verbatim upstream tables and pir's resolved tables are documented
/// spelling aliases — same chord count, same order, and no different key can
/// hide behind normalization.
#[test]
fn raw_tables_differ_from_upstream_only_by_documented_spelling_aliases() {
    for (label, platform) in PLATFORM_ROWS {
        for (id, _, _) in UPSTREAM_TUI_KEYBINDINGS {
            let upstream = strings(&upstream_tui_effective(id, *platform));
            let pir = effective_tui_keys(id, *platform).expect("tui id must resolve");
            assert_raw_differences_are_aliases(&format!("{id} on {label}"), &upstream, &pir);
        }
        for (id, _, _) in UPSTREAM_APP_KEYBINDINGS {
            if UNRESOLVED_APP_IDS.contains(id) {
                continue;
            }
            let upstream = strings(&upstream_app_effective(id, *platform));
            let pir = effective_app_keys(id, *platform).expect("app id must resolve");
            assert_raw_differences_are_aliases(&format!("{id} on {label}"), &upstream, &pir);
        }
    }
}

/// `app.suspend` is the sharpest platform case: EMPTY on native win32 (the
/// chord must NOT respond), `ctrl+z` on WSL and everywhere else.
#[test]
fn app_suspend_platform_rows() {
    assert_eq!(
        upstream_app_effective("app.suspend", WIN32),
        Vec::<&str>::new()
    );
    assert_eq!(hint_default_keys_for("app.suspend", WIN32), Some(vec![]));
    assert_eq!(
        hint_default_keys_for("app.suspend", WSL),
        Some(vec!["ctrl+z".to_string()])
    );
    assert_eq!(
        hint_default_keys_for("app.suspend", OTHER),
        Some(vec!["ctrl+z".to_string()])
    );
    // The live host resolves the same way as the wiring assertions below.
    let host = KeybindingPlatform::host();
    let host_keys = hint_default_keys_for("app.suspend", host).expect("resolvable");
    assert_eq!(host_keys.is_empty(), host.native_windows);
}

// ---------------------------------------------------------------------------
// 3. pir extras / known divergences, pinned explicitly
// ---------------------------------------------------------------------------

/// Every `TuiAction`'s default chords, as the intended (documented) table.
/// Chords beyond upstream's own ids are marked EXTRA/DIVERGENCE so they cannot
/// drift silently.
#[test]
fn action_layer_defaults_are_the_documented_table() {
    let windows = use_windows_keybindings();
    let kb = Keybindings::new();
    let expect: &[(TuiAction, &[&str], &str)] = &[
        // upstream `tui.input.submit`
        (TuiAction::Submit, &["enter"], "tui.input.submit"),
        // upstream `tui.input.newLine`
        (
            TuiAction::Newline,
            &["shift+enter", "ctrl+j"],
            "tui.input.newLine",
        ),
        // upstream `app.interrupt`
        (TuiAction::Abort, &["esc"], "app.interrupt"),
        // upstream `app.clear`
        (TuiAction::Clear, &["ctrl+c"], "app.clear"),
        // upstream `app.exit`
        (TuiAction::Exit, &["ctrl+d"], "app.exit"),
        // EXTRA: upstream's `tui.editor.historyPrevious`/`Next` default EMPTY;
        // these are the legacy pir chords (on Windows the only history browse
        // keys; elsewhere `app.message.dequeue` = alt+up wins first).
        (
            TuiAction::HistoryPrevious,
            &["alt+up"],
            "tui.editor.historyPrevious (EXTRA chord)",
        ),
        (
            TuiAction::HistoryNext,
            &["alt+down"],
            "tui.editor.historyNext (EXTRA chord)",
        ),
        // upstream `tui.input.tab`
        (TuiAction::AutocompleteAccept, &["tab"], "tui.input.tab"),
        // upstream `tui.select.cancel`
        (TuiAction::AutocompleteClose, &["esc"], "tui.select.cancel"),
        // upstream `tui.select.up` / `tui.select.down`
        (TuiAction::AutocompleteUp, &["up"], "tui.select.up"),
        (TuiAction::AutocompleteDown, &["down"], "tui.select.down"),
        // upstream `app.thinking.cycle`; EXTRA spelling "backtab" (crossterm
        // reports shift+tab as BackTab on every platform).
        (
            TuiAction::ThinkingCycle,
            &["shift+tab", "backtab"],
            "app.thinking.cycle (+ backtab spelling)",
        ),
        // upstream `app.model.select`. `ctrl+p` is no longer part of the
        // selector: it dispatches upstream's `app.model.cycleForward` through
        // the `app.*` id table (wired in `handle_key`).
        (TuiAction::ModelSelector, &["ctrl+l"], "app.model.select"),
        // EXTRA: upstream reaches the thinking selector via `/thinking`; on
        // non-Windows this chord shadows upstream's unwired
        // `app.model.cycleBackward` (shift+ctrl+p).
        (
            TuiAction::ThinkingSelector,
            &["ctrl+shift+p"],
            "pir extension (thinking selector)",
        ),
        // upstream `app.tools.expand`
        (TuiAction::ToolExpand, &["ctrl+o"], "app.tools.expand"),
        // upstream `app.clipboard.pasteImage` / `app.message.followUp`
        (
            TuiAction::Paste,
            if windows { &["alt+v"] } else { &["ctrl+v"] },
            "app.clipboard.pasteImage",
        ),
        (
            TuiAction::FollowUp,
            if windows { &["ctrl+q"] } else { &["alt+enter"] },
            "app.message.followUp",
        ),
        // upstream `app.message.copy`
        (TuiAction::CopyMessage, &["ctrl+x"], "app.message.copy"),
    ];
    for (action, chords, source) in expect {
        for chord in *chords {
            assert!(
                kb.matches(*action, &event(chord)),
                "{} must match {chord} ({source})",
                action.name()
            );
        }
    }
    // The platform pairs must NOT cross over.
    if windows {
        assert!(!kb.matches(TuiAction::Paste, &event("ctrl+v")));
        assert!(!kb.matches(TuiAction::FollowUp, &event("alt+enter")));
    } else {
        assert!(!kb.matches(TuiAction::Paste, &event("alt+v")));
        assert!(!kb.matches(TuiAction::FollowUp, &event("ctrl+q")));
    }
}

// ---------------------------------------------------------------------------
// 4. Handler wiring (dumb chord -> action dispatch pins)
// ---------------------------------------------------------------------------

/// The editor ids the main key handler dispatches, chord by chord — the
/// user-named examples first.
#[test]
fn editor_chords_dispatch_to_their_upstream_ids() {
    let kb = Keybindings::new();
    let wired: &[(&str, &str)] = &[
        ("ctrl+a", "tui.editor.cursorLineStart"),
        ("home", "tui.editor.cursorLineStart"),
        ("ctrl+e", "tui.editor.cursorLineEnd"),
        ("end", "tui.editor.cursorLineEnd"),
        ("ctrl+b", "tui.editor.cursorLeft"),
        ("left", "tui.editor.cursorLeft"),
        ("ctrl+f", "tui.editor.cursorRight"),
        ("right", "tui.editor.cursorRight"),
        ("alt+b", "tui.editor.cursorWordLeft"),
        ("alt+left", "tui.editor.cursorWordLeft"),
        ("ctrl+left", "tui.editor.cursorWordLeft"),
        ("alt+f", "tui.editor.cursorWordRight"),
        ("alt+right", "tui.editor.cursorWordRight"),
        ("ctrl+right", "tui.editor.cursorWordRight"),
        ("ctrl+u", "tui.editor.deleteToLineStart"),
        ("ctrl+k", "tui.editor.deleteToLineEnd"),
        ("ctrl+w", "tui.editor.deleteWordBackward"),
        ("alt+backspace", "tui.editor.deleteWordBackward"),
        ("alt+d", "tui.editor.deleteWordForward"),
        ("alt+delete", "tui.editor.deleteWordForward"),
        ("ctrl+y", "tui.editor.yank"),
        ("alt+y", "tui.editor.yankPop"),
        ("pageup", "tui.editor.pageUp"),
        ("ctrl+pageup", "tui.editor.pageUp"),
        ("pagedown", "tui.editor.pageDown"),
        ("ctrl+pagedown", "tui.editor.pageDown"),
        ("up", "tui.editor.cursorUp"),
        ("down", "tui.editor.cursorDown"),
        ("ctrl+]", "tui.editor.jumpForward"),
        ("ctrl+alt+]", "tui.editor.jumpBackward"),
        ("delete", "tui.editor.deleteCharForward"),
        ("backspace", "tui.editor.deleteCharBackward"),
    ];
    for (chord, id) in wired {
        assert!(
            kb.matches_id(id, &event(chord)),
            "{chord} must dispatch to {id}"
        );
    }
    // Ctrl+D is context-split: `app.exit` on an empty editor (and the editor's
    // deleteCharForward once there is text) — both must be wired.
    assert!(kb.matches(TuiAction::Exit, &event("ctrl+d")));
    assert!(kb.matches_id("tui.editor.deleteCharForward", &event("ctrl+d")));
    // Shift+backspace / shift+delete are the hardcoded upstream aliases
    // (editor.ts:841/845).
    assert!(delete_char_backward_key(&kb, &event("shift+backspace")));
    assert!(delete_char_forward_key(&kb, &event("shift+delete")));
    assert!(!delete_char_backward_key(&kb, &event("shift+delete")));
    assert!(!delete_char_forward_key(&kb, &event("shift+backspace")));
    assert!(delete_char_backward_key(&kb, &event("backspace")));
    assert!(delete_char_forward_key(&kb, &event("delete")));
    // Ctrl+Z is NOT the editor undo outside native win32 (it is
    // `app.suspend`); the effective undo chord must dispatch.
    let host = KeybindingPlatform::host();
    let undo = tui_effective_default_keys_for("tui.editor.undo", host).expect("registered");
    assert!(!undo.is_empty());
    assert!(kb.matches_id("tui.editor.undo", &event(undo[0])));
    if !host.native_windows {
        assert!(!kb.matches_id("tui.editor.undo", &event("ctrl+z")));
    }
    // The tui.select ids used by the windowed lists.
    assert!(kb.matches_id("tui.select.up", &event("up")));
    assert!(kb.matches_id("tui.select.down", &event("down")));
    assert!(kb.matches_id("tui.select.pageUp", &event("pageup")));
    assert!(kb.matches_id("tui.select.pageDown", &event("pagedown")));
    assert!(kb.matches_id("tui.select.confirm", &event("enter")));
    assert!(kb.matches_id("tui.select.cancel", &event("escape")));
    assert!(kb.matches_id("tui.select.cancel", &event("ctrl+c")));
}

#[test]
fn app_chords_dispatch_to_their_upstream_ids() {
    let kb = Keybindings::new();
    let host = KeybindingPlatform::host();
    // Every upstream `app.*` id resolves (config registry or hint table) and
    // matches each of its effective default chords. The four session
    // navigation ids have EMPTY defaults, so this loop deliberately has
    // nothing to check for them — their live bindings are covered by
    // `session_navigation_ids_are_empty_by_default_and_rebind`.
    for (id, _, _) in UPSTREAM_APP_KEYBINDINGS {
        let keys = effective_app_keys(id, host).unwrap_or_else(|| panic!("{id} must resolve"));
        for chord in keys {
            let chord = canonical(&chord);
            assert!(
                kb.matches_app_id(id, &event(&chord)),
                "{id} must dispatch its default {chord}"
            );
        }
    }
    // ctrl+x is context-split exactly like upstream: `app.message.copy` in the
    // editor, `app.models.clearAll` in the scoped-model selector.
    assert!(kb.matches(TuiAction::CopyMessage, &event("ctrl+x")));
    assert!(kb.matches_app_id("app.models.clearAll", &event("ctrl+x")));
    // The platform-branched app ids.
    if use_windows_keybindings() {
        assert!(kb.matches(TuiAction::FollowUp, &event("ctrl+q")));
        assert!(kb.matches_app_id("app.message.dequeue", &event("alt+q")));
        assert!(kb.matches(TuiAction::Paste, &event("alt+v")));
        assert!(kb.matches_app_id("app.model.cycleBackward", &event("alt+p")));
    } else {
        assert!(kb.matches(TuiAction::FollowUp, &event("alt+enter")));
        assert!(kb.matches_app_id("app.message.dequeue", &event("alt+up")));
        assert!(kb.matches(TuiAction::Paste, &event("ctrl+v")));
        assert!(kb.matches_app_id("app.model.cycleBackward", &event("shift+ctrl+p")));
    }
    // `app.model.cycleForward` (ctrl+p) is dispatched through its own id now,
    // not through the selector: the wiring in `handle_key` checks it AFTER the
    // thinking selector, so ctrl+p never opens the model selector.
    assert!(kb.matches_app_id("app.model.cycleForward", &event("ctrl+p")));
    assert!(!kb.matches(TuiAction::ModelSelector, &event("ctrl+p")));
    // `app.suspend`: never a default chord on native win32.
    assert_eq!(
        kb.matches_app_id("app.suspend", &event("ctrl+z")),
        !host.native_windows
    );
}

/// The chord the user called out: ctrl+a is `tui.editor.cursorLineStart` in the
/// editor, `app.models.enableAll` in the scoped-model selector and
/// `app.tree.filter.all` in the tree selector — three different owners of one
/// chord, resolved by focus context (never globally).
#[test]
fn ctrl_a_owners_are_context_split() {
    let kb = Keybindings::new();
    assert!(kb.matches_id("tui.editor.cursorLineStart", &event("ctrl+a")));
    assert!(kb.matches_app_id("app.models.enableAll", &event("ctrl+a")));
    assert!(kb.matches_app_id("app.tree.filter.all", &event("ctrl+a")));
    for action in TuiAction::all() {
        assert!(
            !kb.matches(*action, &event("ctrl+a")),
            "{} must not globally own ctrl+a",
            action.name()
        );
    }
}

/// The alt-screen router: the `tui.altScreen.*` scroll/search chords are
/// consumed, while `app.suspend` / `app.thinking.toggle` fall THROUGH (they
/// are editor actions the caller resolves through the keybinding table — the
/// router must not hardcode them, or the native-win32 EMPTY `app.suspend`
/// default would still suspend).
#[test]
fn alt_screen_router_owns_only_the_tui_altscreen_chords() {
    use pir::tui::altscreen_search::{route_alt_screen_key, AltKeyRoute, AltScreenState};
    use pir::tui::core::{Terminal, Text, TuiCore};

    let terminal = Terminal::new();
    terminal.set_size(20, 6);
    let mut core = TuiCore::new(terminal);
    core.push_content(Box::new(Text::new(
        (1..=20)
            .map(|index| format!("line {index}"))
            .collect::<Vec<_>>()
            .join("\n"),
    )));
    core.render();
    let mut state = AltScreenState::default();

    for chord in ["pageup", "pagedown", "ctrl+home", "ctrl+end"] {
        assert!(
            matches!(
                route_alt_screen_key(&event(chord), &mut core, &mut state, 0),
                AltKeyRoute::Consumed
            ),
            "{chord} must be consumed by the router"
        );
    }
    // The search toggle follows `tui.altScreen.search`'s platform branch.
    let search_chord = if use_windows_keybindings() {
        "ctrl+f"
    } else {
        "ctrl+shift+f"
    };
    assert!(matches!(
        route_alt_screen_key(&event(search_chord), &mut core, &mut state, 0),
        AltKeyRoute::Consumed
    ));
    assert!(state.search.active, "{search_chord} toggles search on");
    assert!(matches!(
        route_alt_screen_key(&event(search_chord), &mut core, &mut state, 0),
        AltKeyRoute::Consumed
    ));
    assert!(!state.search.active, "the same chord closes search");

    // app.* actions are NOT the router's business.
    for chord in ["ctrl+t", "ctrl+z"] {
        assert!(
            matches!(
                route_alt_screen_key(&event(chord), &mut core, &mut state, 0),
                AltKeyRoute::Passthrough
            ),
            "{chord} must pass through the router"
        );
    }
}

/// The jump mode itself (the new `tui.editor.jumpForward`/`jumpBackward`
/// handler): mode state plus the ported `jumpToChar` movement — upstream
/// `editor.ts:699-718, 2138-2166`.
#[test]
fn jump_mode_moves_to_the_next_and_previous_character() {
    use pir::tui::editor::Editor;

    let editor = Editor::new();
    editor.apply_state(vec!["abc".into(), "def".into(), "abc".into()], 0, 0);
    assert_eq!(editor.jump_mode(), None);
    editor.set_jump_mode(true);
    assert_eq!(editor.jump_mode(), Some(true));
    editor.cancel_jump_mode();
    assert_eq!(editor.jump_mode(), None);

    // Forward: from (0,0) the next 'c' is (0,2).
    editor.jump_to_char('c', true);
    let state = editor.state_snapshot();
    assert_eq!((state.cursor_line, state.cursor_col), (0, 2));
    // Forward again: the next 'c' after (0,2) is on the third line.
    editor.jump_to_char('c', true);
    let state = editor.state_snapshot();
    assert_eq!((state.cursor_line, state.cursor_col), (2, 2));
    // Backward: the previous 'a' from (2,2) is (2,0).
    editor.jump_to_char('a', false);
    let state = editor.state_snapshot();
    assert_eq!((state.cursor_line, state.cursor_col), (2, 0));
    // No match: the cursor stays where it is.
    editor.jump_to_char('z', true);
    let state = editor.state_snapshot();
    assert_eq!((state.cursor_line, state.cursor_col), (2, 0));
}

// ---------------------------------------------------------------------------
// `<agentDir>/keybindings.json` wiring (audit §7 #2/#3/#4)
// ---------------------------------------------------------------------------

/// The file layer feeds the SAME action map the TUI dispatches, including the
/// `app.*` ids (`app.model.cycleForward` = ctrl+p) and the `tui.*` ids; an
/// explicit empty list turns an id off (upstream `rebuild`).
#[test]
fn keybindings_json_bindings_apply_to_actions_and_app_ids() {
    use pir::config::keybindings::parse_keybindings;

    let file = parse_keybindings(
        r#"{
            "tui.input.submit": "ctrl+j",
            "app.model.cycleForward": "alt+1",
            "tui.editor.yank": []
        }"#,
    );
    assert!(file.errors.is_empty(), "{:?}", file.errors);
    let kb = Keybindings::from_settings_and_file(None, Some(&file)).expect("resolve");
    assert!(kb.matches(TuiAction::Submit, &event("ctrl+j")));
    assert!(!kb.matches(TuiAction::Submit, &event("enter")));
    assert!(kb.matches_app_id("app.model.cycleForward", &event("alt+1")));
    assert!(!kb.matches_app_id("app.model.cycleForward", &event("ctrl+p")));
    // Explicit empty: the default chord no longer matches the id.
    assert!(!kb.matches_id("tui.editor.yank", &event("ctrl+y")));
    assert_eq!(kb.keys_for_id("tui.editor.yank"), Some(Vec::new()));
}

/// settings.json wins over the file even when the file used the canonical
/// `tui.*` id and settings used the legacy camelCase name (the layer overlay
/// is per ACTION, not per raw key).
#[test]
fn keybindings_json_is_layered_under_settings() {
    use pir::config::keybindings::parse_keybindings;

    let file =
        parse_keybindings(r#"{ "tui.input.submit": "ctrl+j", "app.model.cycleForward": "alt+1" }"#);
    let settings = serde_json::json!({
        "submit": "ctrl+n",
        // Not named by the file: the file/default value survives.
        "thinkingCycle": "alt+2"
    });
    let kb = Keybindings::from_settings_and_file(Some(&settings), Some(&file)).expect("resolve");
    assert!(kb.matches(TuiAction::Submit, &event("ctrl+n")));
    assert!(!kb.matches(TuiAction::Submit, &event("ctrl+j")));
    assert!(kb.matches_app_id("app.model.cycleForward", &event("alt+1")));
    assert!(kb.matches(TuiAction::ThinkingCycle, &event("alt+2")));
}

/// No file and no settings keeps every default (a missing keybindings.json is
/// not an error).
#[test]
fn keybindings_json_missing_keeps_defaults() {
    let kb = Keybindings::from_settings_and_file(None, None).expect("resolve");
    assert!(kb.matches(TuiAction::Submit, &event("enter")));
    assert!(kb.matches_app_id("app.model.cycleForward", &event("ctrl+p")));
}

// ---------------------------------------------------------------------------
// §7 #3 — full APP_KEYBINDINGS coverage and its live overrides
// ---------------------------------------------------------------------------

/// Every one of the 43 upstream `APP_KEYBINDINGS` ids resolves through pir's
/// tables (config registry or the session/hint table) and accepts a
/// `keybindings.json` override — including the session ids, which used to be
/// unresolved.
#[test]
fn every_upstream_app_id_resolves_and_accepts_an_override() {
    assert_eq!(UPSTREAM_APP_KEYBINDINGS.len(), 43);
    let host = KeybindingPlatform::host();
    let kb = Keybindings::new();
    for (id, _, _) in UPSTREAM_APP_KEYBINDINGS {
        assert!(
            effective_app_keys(id, host).is_some(),
            "{id} must resolve its defaults"
        );
        assert!(
            kb.keys_for_app_id(id).is_some(),
            "{id} must resolve through the live keybinding map"
        );
    }

    // One override per id at once: no id is silently dropped, and the
    // canonical id always wins over the untouched default.
    let mut object = serde_json::Map::new();
    for (id, _, _) in UPSTREAM_APP_KEYBINDINGS {
        object.insert(
            (*id).to_string(),
            serde_json::Value::String("f12".to_string()),
        );
    }
    let file =
        pir::config::keybindings::parse_keybindings(&serde_json::Value::Object(object).to_string());
    assert!(file.errors.is_empty(), "{:?}", file.errors);
    let kb = Keybindings::from_settings_and_file(None, Some(&file)).expect("resolve");
    for (id, _, _) in UPSTREAM_APP_KEYBINDINGS {
        assert!(
            kb.matches_app_id(id, &event("f12")),
            "{id} must honor its keybindings.json override"
        );
    }
}

/// The four `app.session.*` navigation ids keep upstream's EMPTY default and
/// start dispatching only after a user binding; the six session-selector ids
/// keep their upstream defaults and also accept overrides.
#[test]
fn session_ids_defaults_and_file_overrides() {
    use pir::config::keybindings::parse_keybindings;

    let kb = Keybindings::new();
    for id in [
        "app.session.new",
        "app.session.tree",
        "app.session.fork",
        "app.session.resume",
    ] {
        assert_eq!(
            kb.keys_for_app_id(id),
            Some(Vec::new()),
            "{id} must keep the upstream EMPTY default"
        );
        assert!(!kb.matches_app_id(id, &event("f12")));
    }
    assert_eq!(session_action_key(&kb, &event("enter")), None);

    let selector_defaults: &[(&str, &str, SessionsKeyAction)] = &[
        (
            "app.session.toggleSort",
            "ctrl+s",
            SessionsKeyAction::ToggleSort,
        ),
        (
            "app.session.toggleNamedFilter",
            "ctrl+n",
            SessionsKeyAction::ToggleNamedFilter,
        ),
        (
            "app.session.togglePath",
            "ctrl+p",
            SessionsKeyAction::TogglePath,
        ),
        ("app.session.delete", "ctrl+d", SessionsKeyAction::Delete),
        ("app.session.rename", "ctrl+r", SessionsKeyAction::Rename),
        (
            "app.session.deleteNoninvasive",
            "ctrl+backspace",
            SessionsKeyAction::DeleteNoninvasive,
        ),
    ];
    for (id, chord, action) in selector_defaults {
        assert!(
            kb.matches_app_id(id, &event(chord)),
            "{id} must match its upstream default {chord}"
        );
        assert_eq!(
            sessions_key_action(&kb, &event(chord)),
            Some(*action),
            "{chord} must dispatch {id}"
        );
    }

    let file = parse_keybindings(
        r#"{
            "app.session.new": "f1",
            "app.session.tree": "f2",
            "app.session.fork": "f3",
            "app.session.resume": "f4",
            "app.session.toggleSort": "f5",
            "app.session.toggleNamedFilter": "f6",
            "app.session.togglePath": "f7",
            "app.session.delete": "f8",
            "app.session.rename": "f9",
            "app.session.deleteNoninvasive": "f10"
        }"#,
    );
    assert!(file.errors.is_empty(), "{:?}", file.errors);
    let kb = Keybindings::from_settings_and_file(None, Some(&file)).expect("resolve");
    assert_eq!(
        session_action_key(&kb, &event("f1")),
        Some(SessionActionKey::New)
    );
    assert_eq!(
        session_action_key(&kb, &event("f2")),
        Some(SessionActionKey::Tree)
    );
    assert_eq!(
        session_action_key(&kb, &event("f3")),
        Some(SessionActionKey::Fork)
    );
    assert_eq!(
        session_action_key(&kb, &event("f4")),
        Some(SessionActionKey::Resume)
    );
    assert_eq!(
        sessions_key_action(&kb, &event("f5")),
        Some(SessionsKeyAction::ToggleSort)
    );
    assert_eq!(
        sessions_key_action(&kb, &event("f6")),
        Some(SessionsKeyAction::ToggleNamedFilter)
    );
    assert_eq!(
        sessions_key_action(&kb, &event("f7")),
        Some(SessionsKeyAction::TogglePath)
    );
    assert_eq!(
        sessions_key_action(&kb, &event("f8")),
        Some(SessionsKeyAction::Delete)
    );
    assert_eq!(
        sessions_key_action(&kb, &event("f9")),
        Some(SessionsKeyAction::Rename)
    );
    assert_eq!(
        sessions_key_action(&kb, &event("f10")),
        Some(SessionsKeyAction::DeleteNoninvasive)
    );
    // The replaced defaults no longer dispatch.
    assert!(!kb.matches_app_id("app.session.toggleSort", &event("ctrl+s")));
    assert_eq!(sessions_key_action(&kb, &event("ctrl+s")), None);
}

/// The `/sessions` picker's display toggles follow the three
/// `app.session.toggle*` ids; the three management ids leave the display
/// options untouched (they dispatch the rename/delete flow instead, covered by
/// `tui_session_manage.rs`).
#[test]
fn sessions_view_toggles_follow_the_session_ids() {
    let defaults = SessionsViewOptions::default();
    assert!(defaults.show_path);
    assert!(!defaults.named_only);
    assert!(!defaults.sort_by_name);

    let sorted = defaults.toggled(SessionsKeyAction::ToggleSort);
    assert!(sorted.sort_by_name);
    assert!(!sorted.toggled(SessionsKeyAction::ToggleSort).sort_by_name);

    let named = defaults.toggled(SessionsKeyAction::ToggleNamedFilter);
    assert!(named.named_only);
    assert!(
        !named
            .toggled(SessionsKeyAction::ToggleNamedFilter)
            .named_only
    );

    let hidden = defaults.toggled(SessionsKeyAction::TogglePath);
    assert!(!hidden.show_path);

    for action in [
        SessionsKeyAction::Delete,
        SessionsKeyAction::Rename,
        SessionsKeyAction::DeleteNoninvasive,
    ] {
        assert_eq!(defaults.toggled(action), defaults);
    }
}

/// §7 #9: the `/tree` filter chords are the `app.tree.filter.*` defaults and
/// drive the same toggle/cycle transitions as before the refactor.
#[test]
fn tree_filter_keys_are_app_ids_and_keep_the_default_behavior() {
    let kb = Keybindings::new();
    let expected: &[(&str, TreeFilterKeyAction)] = &[
        ("ctrl+d", TreeFilterKeyAction::Default),
        ("ctrl+t", TreeFilterKeyAction::NoTools),
        ("ctrl+u", TreeFilterKeyAction::UserOnly),
        ("ctrl+l", TreeFilterKeyAction::LabeledOnly),
        ("ctrl+a", TreeFilterKeyAction::All),
        ("ctrl+o", TreeFilterKeyAction::CycleForward),
        ("ctrl+shift+o", TreeFilterKeyAction::CycleBackward),
        ("ctrl+x", TreeFilterKeyAction::Copy),
    ];
    for (chord, action) in expected {
        assert_eq!(
            tree_filter_key_action(&kb, &event(chord)),
            Some(*action),
            "{chord} must dispatch {action:?}"
        );
    }
    // A shifted ctrl+o is the backward cycle even though the chord matcher
    // ignores SHIFT for letter keys; a plain ctrl+o stays forward.
    assert_eq!(
        tree_filter_key_action(&kb, &event("ctrl+o")),
        Some(TreeFilterKeyAction::CycleForward)
    );

    // Toggle semantics: each direct mode toggles back to default, and the
    // cycle pair walks the upstream cycle order.
    use TreeFilterMode as Mode;
    assert_eq!(
        tree_filter_mode_after_key(Mode::Default, TreeFilterKeyAction::NoTools),
        Some(Mode::NoTools)
    );
    assert_eq!(
        tree_filter_mode_after_key(Mode::NoTools, TreeFilterKeyAction::NoTools),
        Some(Mode::Default)
    );
    assert_eq!(
        tree_filter_mode_after_key(Mode::UserOnly, TreeFilterKeyAction::UserOnly),
        Some(Mode::Default)
    );
    assert_eq!(
        tree_filter_mode_after_key(Mode::LabeledOnly, TreeFilterKeyAction::LabeledOnly),
        Some(Mode::Default)
    );
    assert_eq!(
        tree_filter_mode_after_key(Mode::All, TreeFilterKeyAction::All),
        Some(Mode::Default)
    );
    assert_eq!(
        tree_filter_mode_after_key(Mode::All, TreeFilterKeyAction::CycleForward),
        Some(Mode::Default)
    );
    assert_eq!(
        tree_filter_mode_after_key(Mode::Default, TreeFilterKeyAction::CycleBackward),
        Some(Mode::All)
    );
    assert_eq!(
        tree_filter_mode_after_key(Mode::NoTools, TreeFilterKeyAction::Default),
        Some(Mode::Default)
    );
    assert_eq!(
        tree_filter_mode_after_key(Mode::NoTools, TreeFilterKeyAction::Copy),
        None
    );
}

/// §7 #9: a `keybindings.json` rebind moves the tree filter keys; the replaced
/// defaults no longer fire and the new chords do.
#[test]
fn tree_filter_rebind_from_file_moves_the_keys() {
    use pir::config::keybindings::parse_keybindings;

    let file = parse_keybindings(
        r#"{
            "app.tree.filter.noTools": "f2",
            "app.tree.filter.cycleForward": "alt+n",
            "app.tree.filter.cycleBackward": "alt+b",
            "app.message.copy": "f4"
        }"#,
    );
    assert!(file.errors.is_empty(), "{:?}", file.errors);
    let kb = Keybindings::from_settings_and_file(None, Some(&file)).expect("resolve");
    assert_eq!(
        tree_filter_key_action(&kb, &event("f2")),
        Some(TreeFilterKeyAction::NoTools)
    );
    assert_eq!(tree_filter_key_action(&kb, &event("ctrl+t")), None);
    assert_eq!(
        tree_filter_key_action(&kb, &event("alt+n")),
        Some(TreeFilterKeyAction::CycleForward)
    );
    assert_eq!(
        tree_filter_key_action(&kb, &event("alt+b")),
        Some(TreeFilterKeyAction::CycleBackward)
    );
    assert_eq!(tree_filter_key_action(&kb, &event("ctrl+o")), None);
    assert_eq!(
        tree_filter_key_action(&kb, &event("f4")),
        Some(TreeFilterKeyAction::Copy)
    );
    assert_eq!(tree_filter_key_action(&kb, &event("ctrl+x")), None);

    // settings.json is the second live layer and follows the same ids.
    let settings = serde_json::json!({ "app.tree.filter.userOnly": "f11" });
    let kb = Keybindings::from_settings_and_file(Some(&settings), None).expect("resolve");
    assert_eq!(
        tree_filter_key_action(&kb, &event("f11")),
        Some(TreeFilterKeyAction::UserOnly)
    );
    assert_eq!(tree_filter_key_action(&kb, &event("ctrl+u")), None);
}

/// The six `app.models.*` selector ids dispatch their default chords and move
/// with a file override (upstream defaults from
/// `core/keybindings.ts:119-142`).
#[test]
fn scoped_models_keys_dispatch_and_rebind() {
    use pir::config::keybindings::parse_keybindings;

    let kb = Keybindings::new();
    let expected: &[(&str, ScopedModelsKeyAction)] = &[
        ("alt+up", ScopedModelsKeyAction::ReorderUp),
        ("alt+down", ScopedModelsKeyAction::ReorderDown),
        ("ctrl+a", ScopedModelsKeyAction::EnableAll),
        ("ctrl+x", ScopedModelsKeyAction::ClearAll),
        ("ctrl+p", ScopedModelsKeyAction::ToggleProvider),
        ("ctrl+s", ScopedModelsKeyAction::Save),
    ];
    for (chord, action) in expected {
        assert_eq!(
            scoped_models_key_action(&kb, &event(chord)),
            Some(*action),
            "{chord} must dispatch {action:?}"
        );
    }

    let file = parse_keybindings(
        r#"{
            "app.models.reorderUp": "f2",
            "app.models.reorderDown": "f3",
            "app.models.enableAll": "f4",
            "app.models.clearAll": "f5",
            "app.models.toggleProvider": "f6",
            "app.models.save": "f7"
        }"#,
    );
    assert!(file.errors.is_empty(), "{:?}", file.errors);
    let kb = Keybindings::from_settings_and_file(None, Some(&file)).expect("resolve");
    assert_eq!(
        scoped_models_key_action(&kb, &event("f2")),
        Some(ScopedModelsKeyAction::ReorderUp)
    );
    assert_eq!(
        scoped_models_key_action(&kb, &event("f3")),
        Some(ScopedModelsKeyAction::ReorderDown)
    );
    assert_eq!(
        scoped_models_key_action(&kb, &event("f4")),
        Some(ScopedModelsKeyAction::EnableAll)
    );
    assert_eq!(
        scoped_models_key_action(&kb, &event("f5")),
        Some(ScopedModelsKeyAction::ClearAll)
    );
    assert_eq!(
        scoped_models_key_action(&kb, &event("f6")),
        Some(ScopedModelsKeyAction::ToggleProvider)
    );
    assert_eq!(
        scoped_models_key_action(&kb, &event("f7")),
        Some(ScopedModelsKeyAction::Save)
    );
    assert_eq!(scoped_models_key_action(&kb, &event("ctrl+a")), None);
    assert_eq!(scoped_models_key_action(&kb, &event("ctrl+p")), None);
}

/// The canonical `app.*` ids of action-backed TuiActions (`app.interrupt`,
/// `app.clear`, `app.exit`, `app.thinking.cycle`, `app.model.select`,
/// `app.tools.expand`, `app.message.copy`, `app.message.followUp`,
/// `app.clipboard.pasteImage`) move the editor action itself, not just the
/// id table. This is the live half of the §7 #3 fix: before it, a canonical
/// override was stored but never dispatched.
#[test]
fn canonical_app_ids_move_their_actions() {
    use pir::config::keybindings::parse_keybindings;

    let file = parse_keybindings(
        r#"{
            "app.interrupt": "ctrl+u",
            "app.clear": "f2",
            "app.exit": "f3",
            "app.thinking.cycle": "f4",
            "app.model.select": "f5",
            "app.tools.expand": "f6",
            "app.message.copy": "f7",
            "app.message.followUp": "f8",
            "app.clipboard.pasteImage": "f9"
        }"#,
    );
    assert!(file.errors.is_empty(), "{:?}", file.errors);
    let kb = Keybindings::from_settings_and_file(None, Some(&file)).expect("resolve");
    let moved: &[(TuiAction, &str, &str)] = &[
        (TuiAction::Abort, "ctrl+u", "esc"),
        (TuiAction::Clear, "f2", "ctrl+c"),
        (TuiAction::Exit, "f3", "ctrl+d"),
        (TuiAction::ThinkingCycle, "f4", "shift+tab"),
        (TuiAction::ModelSelector, "f5", "ctrl+l"),
        (TuiAction::ToolExpand, "f6", "ctrl+o"),
        (TuiAction::CopyMessage, "f7", "ctrl+x"),
        (
            TuiAction::FollowUp,
            "f8",
            if use_windows_keybindings() {
                "ctrl+q"
            } else {
                "alt+enter"
            },
        ),
        (
            TuiAction::Paste,
            "f9",
            if use_windows_keybindings() {
                "alt+v"
            } else {
                "ctrl+v"
            },
        ),
    ];
    for (action, new_chord, old_chord) in moved {
        assert!(
            kb.matches(*action, &event(new_chord)),
            "{} must move to {new_chord}",
            action.name()
        );
        assert!(
            !kb.matches(*action, &event(old_chord)),
            "{} must no longer match {old_chord}",
            action.name()
        );
    }

    // The settings layer moves the same canonical ids.
    let settings = serde_json::json!({ "app.model.select": "f10" });
    let kb = Keybindings::from_settings_and_file(Some(&settings), None).expect("resolve");
    assert!(kb.matches(TuiAction::ModelSelector, &event("f10")));
    assert!(!kb.matches(TuiAction::ModelSelector, &event("ctrl+l")));
}
