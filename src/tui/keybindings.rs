//! Configurable keybindings — a minimal port of upstream
//! `packages/tui/src/keybindings.ts`.
//!
//! Scope is intentionally tight (not a full keymap DSL): a fixed set of
//! [`TuiAction`]s, each with one or more [`KeyCombo`] defaults matching the
//! previously hardcoded pir behavior, plus the upstream `tui.*` namespace
//! registry ([`TUI_KEYBINDING_DEFINITIONS`], `packages/tui/src/keybindings.ts`)
//! holding the ids pir's TUI owns a behavior for with upstream-verbatim
//! defaults. Users override bindings per action in `settings.json` under the
//! `keybindings` object, keyed either by the legacy camelCase name or the
//! canonical `tui.*` id (canonical wins); names that are neither, and key
//! strings that do not parse, are ignored.
//!
//! ```jsonc
//! // ~/.pi/agent/settings.json
//! {
//!   "keybindings": {
//!     "submit": "enter",
//!     "newline": ["shift+enter", "ctrl+j"],
//!     "followUp": "alt+enter",
//!     "abort": "esc",
//!     "historyPrevious": "alt+up",
//!     "autocompleteAccept": "tab"
//!   }
//! }
//! ```
//!
//! Key string grammar: `mod+mod+key` where mods are `ctrl`/`shift`/`alt`
//! and key is `enter`/`return`, `esc`/`escape`, `tab`, `backspace`,
//! `delete`/`del`, `up|down|left|right`, `home`, `end`, `pageup`,
//! `pagedown`, `space`, or a single character (`v`, `1`, …). The settings
//! value may be one string or an array of strings.

use std::collections::BTreeMap;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// Every bindable TUI action. The name string is the key used in the
/// settings `keybindings` object (camelCase).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum TuiAction {
    Submit,
    Newline,
    Abort,
    Clear,
    Exit,
    HistoryPrevious,
    HistoryNext,
    AutocompleteAccept,
    AutocompleteClose,
    AutocompleteUp,
    AutocompleteDown,
    ThinkingCycle,
    ModelSelector,
    ThinkingSelector,
    ToolExpand,
    Paste,
    /// Alt+Enter: queue the input as a follow-up while streaming (upstream
    /// `app.message.followUp`); behaves like submit when idle.
    FollowUp,
    /// Ctrl+X: copy the last assistant message to the clipboard (upstream
    /// `app.message.copy`; pir has no editor selection, so the
    /// prefer-selection half does not apply).
    CopyMessage,
}

impl TuiAction {
    /// Settings key for this action.
    pub fn name(self) -> &'static str {
        match self {
            TuiAction::Submit => "submit",
            TuiAction::Newline => "newline",
            TuiAction::Abort => "abort",
            TuiAction::Clear => "clear",
            TuiAction::Exit => "exit",
            TuiAction::HistoryPrevious => "historyPrevious",
            TuiAction::HistoryNext => "historyNext",
            TuiAction::AutocompleteAccept => "autocompleteAccept",
            TuiAction::AutocompleteClose => "autocompleteClose",
            TuiAction::AutocompleteUp => "autocompleteUp",
            TuiAction::AutocompleteDown => "autocompleteDown",
            TuiAction::ThinkingCycle => "thinkingCycle",
            TuiAction::ModelSelector => "modelSelector",
            TuiAction::ThinkingSelector => "thinkingSelector",
            TuiAction::ToolExpand => "toolExpand",
            TuiAction::Paste => "paste",
            TuiAction::FollowUp => "followUp",
            TuiAction::CopyMessage => "messageCopy",
        }
    }

    /// All actions, in settings order.
    pub fn all() -> &'static [TuiAction] {
        &[
            TuiAction::Submit,
            TuiAction::Newline,
            TuiAction::Abort,
            TuiAction::Clear,
            TuiAction::Exit,
            TuiAction::HistoryPrevious,
            TuiAction::HistoryNext,
            TuiAction::AutocompleteAccept,
            TuiAction::AutocompleteClose,
            TuiAction::AutocompleteUp,
            TuiAction::AutocompleteDown,
            TuiAction::ThinkingCycle,
            TuiAction::ModelSelector,
            TuiAction::ThinkingSelector,
            TuiAction::ToolExpand,
            TuiAction::Paste,
            TuiAction::FollowUp,
            TuiAction::CopyMessage,
        ]
    }

    /// Defaults equal to the previously hardcoded pir key handling — with
    /// the upstream Windows/WSL variants for the two ids whose win32 chords
    /// differ (core/keybindings.ts:67-78): follow-up `ctrl+q` (win) /
    /// `alt+enter`, paste `alt+v` (win) / `ctrl+v`.
    fn default_bindings(self) -> Vec<KeyCombo> {
        let windows = crate::config::keybindings::use_windows_keybindings();
        let combos = match self {
            TuiAction::Submit => vec!["enter"],
            TuiAction::Newline => vec!["shift+enter", "ctrl+j"],
            TuiAction::Abort => vec!["esc"],
            TuiAction::Clear => vec!["ctrl+c"],
            TuiAction::Exit => vec!["ctrl+d"],
            TuiAction::HistoryPrevious => vec!["alt+up"],
            TuiAction::HistoryNext => vec!["alt+down"],
            TuiAction::AutocompleteAccept => vec!["tab"],
            TuiAction::AutocompleteClose => vec!["esc"],
            TuiAction::AutocompleteUp => vec!["up"],
            TuiAction::AutocompleteDown => vec!["down"],
            // "backtab": crossterm normalizes Shift+Tab to BackTab on both
            // Windows (VK_TAB+SHIFT) and Unix (`\x1b[Z`), so the literal
            // "shift+tab" chord alone never matches a real event.
            TuiAction::ThinkingCycle => vec!["shift+tab", "backtab"],
            // `app.model.select` is `ctrl+l` upstream. `ctrl+p` is NOT part
            // of the selector defaults: it is upstream's
            // `app.model.cycleForward` and is dispatched as a model cycle
            // through the `app.*` id table (`matches_app_id`), so a user
            // override of either id applies independently.
            TuiAction::ModelSelector => vec!["ctrl+l"],
            // pir extension, no upstream id: upstream reaches the
            // thinking-level selector through `/thinking`. DELIBERATE: the
            // chord also shadows upstream's non-Windows
            // `app.model.cycleBackward` (`shift+ctrl+p`) — since
            // cycle-backward is unwired (see above), the chord stays usable
            // instead of dead.
            TuiAction::ThinkingSelector => vec!["ctrl+shift+p"],
            TuiAction::ToolExpand => vec!["ctrl+o"],
            TuiAction::Paste if windows => vec!["alt+v"],
            TuiAction::Paste => vec!["ctrl+v"],
            TuiAction::FollowUp if windows => vec!["ctrl+q"],
            TuiAction::FollowUp => vec!["alt+enter"],
            TuiAction::CopyMessage => vec!["ctrl+x"],
        };
        combos.into_iter().filter_map(parse_key).collect()
    }

    /// Resolve an action from its settings name.
    pub fn from_name(name: &str) -> Option<TuiAction> {
        TuiAction::all().iter().copied().find(|a| a.name() == name)
    }

    /// The upstream `tui.*` action id this action mirrors, when the semantics
    /// match one-to-one (`packages/tui/src/keybindings.ts`). Pir-specific
    /// actions and the app-layer actions have none; the latter map through
    /// [`TuiAction::app_upstream_id`]. Overrides under the canonical id win
    /// over the legacy camelCase name.
    pub fn upstream_id(self) -> Option<&'static str> {
        match self {
            TuiAction::Submit => Some("tui.input.submit"),
            TuiAction::Newline => Some("tui.input.newLine"),
            TuiAction::AutocompleteAccept => Some("tui.input.tab"),
            // Upstream routes autocomplete navigation through the generic
            // selection ids too (editor.ts:764).
            TuiAction::AutocompleteUp => Some("tui.select.up"),
            TuiAction::AutocompleteDown => Some("tui.select.down"),
            TuiAction::HistoryPrevious => Some("tui.editor.historyPrevious"),
            TuiAction::HistoryNext => Some("tui.editor.historyNext"),
            _ => None,
        }
    }

    /// Resolve an action from its canonical upstream `tui.*` id (the inverse
    /// of [`TuiAction::upstream_id`]).
    pub fn from_upstream_id(id: &str) -> Option<TuiAction> {
        TuiAction::all()
            .iter()
            .copied()
            .find(|action| action.upstream_id() == Some(id))
    }

    /// The upstream `app.*` id this action mirrors when the app-layer
    /// keybinding owns the same behavior (`core/keybindings.ts:26-78`).
    ///
    /// These ids are not cosmetic: the app layer dispatches `app.interrupt`,
    /// `app.clear`, `app.exit`, `app.thinking.cycle`, `app.model.select`,
    /// `app.tools.expand`, `app.message.followUp`, `app.message.copy` and
    /// `app.clipboard.pasteImage` through the default editor, so a
    /// `keybindings.json` (or settings) override for the canonical id must
    /// move the action — exactly like [`TuiAction::upstream_id`] for the
    /// `tui.*` namespace. Actions whose behavior is registered separately
    /// (`app.suspend`, `app.editor.external`, `app.message.dequeue`, the
    /// selector ids) stay out and are matched by `matches_app_id` at their
    /// dispatch site.
    pub fn app_upstream_id(self) -> Option<&'static str> {
        match self {
            TuiAction::Abort => Some("app.interrupt"),
            TuiAction::Clear => Some("app.clear"),
            TuiAction::Exit => Some("app.exit"),
            TuiAction::ThinkingCycle => Some("app.thinking.cycle"),
            TuiAction::ModelSelector => Some("app.model.select"),
            TuiAction::ToolExpand => Some("app.tools.expand"),
            TuiAction::Paste => Some("app.clipboard.pasteImage"),
            TuiAction::FollowUp => Some("app.message.followUp"),
            TuiAction::CopyMessage => Some("app.message.copy"),
            _ => None,
        }
    }
}

/// One upstream `tui.*` keybinding definition (upstream `TUI_KEYBINDINGS`,
/// `packages/tui/src/keybindings.ts:71-210`): the action id, its default
/// chords verbatim from upstream and the user-facing description.
///
/// The registry only carries ids pir's TUI actually owns a behavior for —
/// editor navigation/deletion, the character jumps, kill-ring/yank/undo, page
/// scrolling, prompt-history browsing, generic input and the windowed
/// selection lists (`src/tui/selector.rs`). Ids whose behavior pir does not
/// implement (`tui.input.copy`, `tui.altScreen.searchNext`/`searchPrevious`
/// (routed through `altscreen_search.rs` rather than this registry) and the
/// whole `tui.altScreen.*` viewport family, which is an app-layer concern in
/// pir) are deliberately absent.
///
/// Note the upstream conflict semantics apply here too: default chords
/// intentionally collide across ids (e.g. `ctrl+d` is both
/// `tui.editor.deleteCharForward` and pir's `exit`; `enter` is
/// `tui.input.submit` and `tui.select.confirm`) because a match is resolved
/// by focus context — only the focused component queries the ids it owns.
/// The registry must not be used as a global chord → action table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TuiKeybindingDefinition {
    /// The canonical `tui.*` action id (upstream-verbatim).
    pub id: &'static str,
    /// The upstream-verbatim default chords (empty when upstream keeps an
    /// empty default, as for the two history ids).
    pub default_keys: &'static [&'static str],
    /// The user-facing description (upstream-verbatim).
    pub description: &'static str,
}

/// The `tui.*` namespace registry (upstream-verbatim defaults; the two empty
/// history ids keep upstream's empty default — pir's live `alt+up`/`alt+down`
/// defaults stay on the [`TuiAction`] layer).
///
/// The `tui.editor.undo` row carries the **tui-package** default (`ctrl+-`,
/// keybindings.ts:142). The coding-agent layer rebinds it per platform
/// (`ctrl+z` on native win32, `alt+z` on WSL, `ctrl+-` elsewhere) — that
/// override lives in [`tui_effective_default_keys_for`], exactly like
/// upstream's `KEYBINDINGS = { ...TUI_KEYBINDINGS, "tui.editor.undo": … }`
/// (core/keybindings.ts:180-199).
pub const TUI_KEYBINDING_DEFINITIONS: &[TuiKeybindingDefinition] = &[
    TuiKeybindingDefinition {
        id: "tui.editor.cursorUp",
        default_keys: &["up"],
        description: "Move cursor up",
    },
    TuiKeybindingDefinition {
        id: "tui.editor.cursorDown",
        default_keys: &["down"],
        description: "Move cursor down",
    },
    TuiKeybindingDefinition {
        id: "tui.editor.historyPrevious",
        default_keys: &[],
        description: "Select previous prompt history entry",
    },
    TuiKeybindingDefinition {
        id: "tui.editor.historyNext",
        default_keys: &[],
        description: "Select next prompt history entry",
    },
    TuiKeybindingDefinition {
        id: "tui.editor.cursorLeft",
        default_keys: &["left", "ctrl+b"],
        description: "Move cursor left",
    },
    TuiKeybindingDefinition {
        id: "tui.editor.cursorRight",
        default_keys: &["right", "ctrl+f"],
        description: "Move cursor right",
    },
    TuiKeybindingDefinition {
        id: "tui.editor.cursorWordLeft",
        default_keys: &["alt+left", "ctrl+left", "alt+b"],
        description: "Move cursor word left",
    },
    TuiKeybindingDefinition {
        id: "tui.editor.cursorWordRight",
        default_keys: &["alt+right", "ctrl+right", "alt+f"],
        description: "Move cursor word right",
    },
    TuiKeybindingDefinition {
        id: "tui.editor.cursorLineStart",
        default_keys: &["home", "ctrl+a"],
        description: "Move to line start",
    },
    TuiKeybindingDefinition {
        id: "tui.editor.cursorLineEnd",
        default_keys: &["end", "ctrl+e"],
        description: "Move to line end",
    },
    TuiKeybindingDefinition {
        id: "tui.editor.jumpForward",
        default_keys: &["ctrl+]"],
        description: "Jump forward to character",
    },
    TuiKeybindingDefinition {
        id: "tui.editor.jumpBackward",
        default_keys: &["ctrl+alt+]"],
        description: "Jump backward to character",
    },
    TuiKeybindingDefinition {
        id: "tui.editor.deleteCharBackward",
        default_keys: &["backspace"],
        description: "Delete character backward",
    },
    TuiKeybindingDefinition {
        id: "tui.editor.deleteCharForward",
        default_keys: &["delete", "ctrl+d"],
        description: "Delete character forward",
    },
    TuiKeybindingDefinition {
        id: "tui.editor.deleteWordBackward",
        default_keys: &["ctrl+w", "alt+backspace"],
        description: "Delete word backward",
    },
    TuiKeybindingDefinition {
        id: "tui.editor.deleteWordForward",
        default_keys: &["alt+d", "alt+delete"],
        description: "Delete word forward",
    },
    TuiKeybindingDefinition {
        id: "tui.editor.deleteToLineStart",
        default_keys: &["ctrl+u"],
        description: "Delete to line start",
    },
    TuiKeybindingDefinition {
        id: "tui.editor.deleteToLineEnd",
        default_keys: &["ctrl+k"],
        description: "Delete to line end",
    },
    TuiKeybindingDefinition {
        id: "tui.editor.yank",
        default_keys: &["ctrl+y"],
        description: "Yank",
    },
    TuiKeybindingDefinition {
        id: "tui.editor.yankPop",
        default_keys: &["alt+y"],
        description: "Yank pop",
    },
    TuiKeybindingDefinition {
        id: "tui.editor.undo",
        default_keys: &["ctrl+-"],
        description: "Undo",
    },
    TuiKeybindingDefinition {
        id: "tui.editor.pageUp",
        default_keys: &["pageUp", "ctrl+pageUp"],
        description: "Page up",
    },
    TuiKeybindingDefinition {
        id: "tui.editor.pageDown",
        default_keys: &["pageDown", "ctrl+pageDown"],
        description: "Page down",
    },
    TuiKeybindingDefinition {
        id: "tui.input.newLine",
        default_keys: &["shift+enter", "ctrl+j"],
        description: "Insert newline",
    },
    TuiKeybindingDefinition {
        id: "tui.input.submit",
        default_keys: &["enter"],
        description: "Submit input",
    },
    TuiKeybindingDefinition {
        id: "tui.input.tab",
        default_keys: &["tab"],
        description: "Tab / autocomplete",
    },
    TuiKeybindingDefinition {
        id: "tui.select.up",
        default_keys: &["up"],
        description: "Move selection up",
    },
    TuiKeybindingDefinition {
        id: "tui.select.down",
        default_keys: &["down"],
        description: "Move selection down",
    },
    TuiKeybindingDefinition {
        id: "tui.select.pageUp",
        default_keys: &["pageUp"],
        description: "Selection page up",
    },
    TuiKeybindingDefinition {
        id: "tui.select.pageDown",
        default_keys: &["pageDown"],
        description: "Selection page down",
    },
    TuiKeybindingDefinition {
        id: "tui.select.confirm",
        default_keys: &["enter"],
        description: "Confirm selection",
    },
    TuiKeybindingDefinition {
        id: "tui.select.cancel",
        default_keys: &["escape", "ctrl+c"],
        description: "Cancel selection",
    },
];

/// Look up one registered `tui.*` definition.
pub fn tui_keybinding_definition(id: &str) -> Option<&'static TuiKeybindingDefinition> {
    TUI_KEYBINDING_DEFINITIONS.iter().find(|d| d.id == id)
}

/// The upstream-verbatim default chords for one registered `tui.*` id
/// (upstream `KeybindingsManager.getKeys` over `TUI_KEYBINDINGS`).
///
/// This is the **tui-package** layer (`packages/tui/src/keybindings.ts`). The
/// coding-agent layer rebinds four ids on top of it
/// (`core/keybindings.ts:180-199`); for the live/effective chords use
/// [`tui_effective_default_keys_for`].
pub fn tui_default_keys_for(id: &str) -> Option<Vec<String>> {
    tui_keybinding_definition(id).map(|definition| {
        definition
            .default_keys
            .iter()
            .map(|key| (*key).to_string())
            .collect()
    })
}

/// The effective default chords for one registered `tui.*` id after the
/// coding-agent layer's overrides — upstream
/// `KEYBINDINGS = { ...TUI_KEYBINDINGS, overrides }`
/// (`core/keybindings.ts:180-199`). Only one id is rebound there:
///
/// ```text
/// tui.editor.undo:
///   native win32            -> "ctrl+z"   (process.platform === "win32")
///   WSL (windowsKeybindings)-> "alt+z"
///   otherwise               -> "ctrl+-"   (the tui default)
/// ```
///
/// The three `tui.altScreen.*` ids upstream also overrides
/// (`previousPrompt` / `nextPrompt` / `search`) are resolved in
/// [`crate::config::keybindings::default_keybinding_definitions_for`], because
/// pir routes that family from the app layer (`altscreen_search.rs`).
///
/// `None` for unregistered ids.
pub fn tui_effective_default_keys_for(
    id: &str,
    platform: crate::config::keybindings::KeybindingPlatform,
) -> Option<Vec<&'static str>> {
    match id {
        // keybindings.ts:182-185 — native win32 wins over the WSL branch.
        "tui.editor.undo" if platform.native_windows => Some(vec!["ctrl+z"]),
        "tui.editor.undo" if platform.windows_keybindings => Some(vec!["alt+z"]),
        _ => tui_keybinding_definition(id).map(|definition| definition.default_keys.to_vec()),
    }
}

/// [`tui_effective_default_keys_for`] for the live host.
pub fn tui_effective_default_keys(id: &str) -> Option<Vec<&'static str>> {
    tui_effective_default_keys_for(id, crate::config::keybindings::KeybindingPlatform::host())
}

/// The parsed effective default chords for one registered `tui.*` id (the
/// platform-resolved [`tui_effective_default_keys_for`] through the shared
/// key parser).
fn tui_effective_default_combos_for(id: &str) -> Option<Vec<KeyCombo>> {
    tui_effective_default_keys(id).map(|keys| {
        keys.iter()
            .filter_map(|key| parse_key(key))
            .collect::<Vec<_>>()
    })
}

/// One key chord: a `crossterm` key code plus modifier flags.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyCombo {
    pub code: KeyCode,
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
    /// The `super`/`cmd`/`win` modifier (upstream grammar accepts `super`).
    pub super_mod: bool,
}

impl KeyCombo {
    /// This combo with `BackTab` normalized to `Tab + SHIFT`: crossterm
    /// reports Shift+Tab as `BackTab` on every platform (Windows VK_TAB+SHIFT,
    /// Unix `\x1b[Z`), so both spellings must compare equal.
    fn normalized(&self) -> KeyCombo {
        if self.code == KeyCode::BackTab {
            KeyCombo {
                code: KeyCode::Tab,
                shift: true,
                ..self.clone()
            }
        } else {
            self.clone()
        }
    }

    /// Does this combo match a key event? Shift is ignored for letter
    /// characters (terminals differ on whether Ctrl+Shift+X reports SHIFT).
    pub fn matches(&self, key: &KeyEvent) -> bool {
        let combo = self.normalized();
        // Normalize the event the same way (`BackTab` → `Tab + SHIFT`).
        let (code, shift) = if key.code == KeyCode::BackTab {
            (KeyCode::Tab, true)
        } else {
            (key.code, key.modifiers.contains(KeyModifiers::SHIFT))
        };
        let code_matches = code == combo.code
            || match (code, combo.code) {
                (KeyCode::Char(left), KeyCode::Char(right)) => left.eq_ignore_ascii_case(&right),
                _ => false,
            };
        if !code_matches {
            return false;
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        let super_mod = key.modifiers.contains(KeyModifiers::SUPER);
        if ctrl != combo.ctrl || alt != combo.alt || super_mod != combo.super_mod {
            return false;
        }
        match combo.code {
            // Letter keys: a declared SHIFT is meaningful (otherwise
            // ctrl+shift+p would also swallow the ctrl+p model-cycle chord);
            // an unshifted combo still tolerates terminals that omit/extra-
            // report SHIFT for ctrl/alt+letter.
            KeyCode::Char(c) if c.is_ascii_alphabetic() => !combo.shift || shift,
            _ => shift == combo.shift,
        }
    }
}

/// `f1`..=`f12` (the function-key name set both the settings schema and the
/// upstream key grammar accept).
fn is_function_key(key: &str) -> bool {
    key.len() >= 2
        && key.starts_with('f')
        && key[1..]
            .parse::<u8>()
            .is_ok_and(|number| (1..=12).contains(&number))
}

/// Parse a key string (`"ctrl+shift+p"`, `"enter"`, `"v"`, …).
pub fn parse_key(spec: &str) -> Option<KeyCombo> {
    let mut ctrl = false;
    let mut alt = false;
    let mut shift = false;
    let mut super_mod = false;
    let mut code = None;
    let parts: Vec<&str> = spec
        .split('+')
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .collect();
    for (index, part) in parts.iter().enumerate() {
        let is_last = index + 1 == parts.len();
        match part.to_ascii_lowercase().as_str() {
            "ctrl" | "control" if !is_last => ctrl = true,
            "alt" | "meta" | "opt" if !is_last => alt = true,
            "super" | "cmd" | "command" | "win" if !is_last => super_mod = true,
            "shift" if !is_last => shift = true,
            key => {
                if !is_last {
                    // Modifier appearing after a key: reject.
                    return None;
                }
                code = Some(match key {
                    "enter" | "return" => KeyCode::Enter,
                    "esc" | "escape" => KeyCode::Esc,
                    "tab" => KeyCode::Tab,
                    "backtab" => KeyCode::BackTab,
                    "backspace" => KeyCode::Backspace,
                    "delete" | "del" => KeyCode::Delete,
                    "insert" | "ins" => KeyCode::Insert,
                    "up" => KeyCode::Up,
                    "down" => KeyCode::Down,
                    "left" => KeyCode::Left,
                    "right" => KeyCode::Right,
                    "home" => KeyCode::Home,
                    "end" => KeyCode::End,
                    "pageup" => KeyCode::PageUp,
                    "pagedown" | "pgdn" => KeyCode::PageDown,
                    "space" => KeyCode::Char(' '),
                    // Function keys f1-f12 (upstream named-key grammar; the
                    // settings schema already accepts them).
                    _ if is_function_key(key) => KeyCode::F(key[1..].parse::<u8>().ok()?),
                    _ => {
                        let mut chars = part.chars();
                        let first = chars.next()?;
                        if chars.next().is_some() {
                            return None;
                        }
                        // Settings are case-insensitive for single letters;
                        // `shift` remains the explicit modifier.
                        KeyCode::Char(first.to_ascii_lowercase())
                    }
                });
            }
        }
    }
    Some(KeyCombo {
        code: code?,
        ctrl,
        alt,
        shift,
        super_mod,
    })
}

/// The canonical spelling of one chord spec: parse through [`parse_key`] and
/// render through the shared chord renderer (`terminal::combo_text`).
///
/// Upstream's literal tables spell some keys differently from pir's canonical
/// tables; the alias table is exactly `parse_key`'s name table, so only pure
/// spellings collapse and two different keys can never canonicalize equal:
///
/// | upstream literal (pi) | pir canonical | alias source |
/// |---|---|---|
/// | `escape` | `esc` | `parse_key` key names |
/// | `return` | `enter` | idem |
/// | `del` / `ins` / `pgdn` | `delete` / `insert` / `pagedown` | idem |
/// | `pageUp` / `pageDown` | `pageup` / `pagedown` | idem (case-insensitive) |
/// | `control` / `meta` / `opt` | `ctrl` / `alt` | idem |
/// | `shift+ctrl+p` | `ctrl+shift+p` | fixed render order `ctrl+alt+shift+key` |
///
/// `None` for unparseable specs. Used by the upstream-parity suite to compare
/// pir's resolved tables against the verbatim upstream tables without relaxing
/// the comparison to set-size or alias-blind equality.
pub fn canonical_key_spec(spec: &str) -> Option<String> {
    parse_key(spec).map(|combo| super::terminal::combo_text(&combo))
}

/// The resolved keybinding map (upstream `KeybindingsManager`, minimized).
#[derive(Debug, Clone)]
pub struct Keybindings {
    map: BTreeMap<TuiAction, Vec<KeyCombo>>,
    /// Settings overrides keyed by the canonical upstream `tui.*` id
    /// (registered ids only, upstream `rebuild`). The per-id dispatch wiring
    /// into `terminal.rs` is a recorded follow-up; today this carries the
    /// override data plane, and the ids that map onto a [`TuiAction`]
    /// additionally apply through `map` (see [`Keybindings::from_settings`]).
    id_overrides: BTreeMap<&'static str, Vec<KeyCombo>>,
    /// Settings overrides for the `app.*` id namespace
    /// (`crate::config::keybindings::default_keybinding_definitions` ids:
    /// `app.editor.external`, `app.message.dequeue`, `app.models.save`, …).
    /// Unregistered ids never match, mirroring the `tui.*` behavior.
    app_overrides: BTreeMap<&'static str, Vec<KeyCombo>>,
    /// Settings overrides for the `tui.altScreen.*` viewport family (W11-C).
    /// The family is routed from the app layer (`altscreen_search.rs`), not
    /// the frozen `tui.*` registry, so its overrides are kept separately and
    /// read through [`Keybindings::alt_id_combos`]. An explicit empty list is
    /// an override (it turns the id's upstream default off).
    alt_overrides: BTreeMap<&'static str, Vec<KeyCombo>>,
    /// Actions an input source explicitly named (used to layer
    /// `settings.json` on top of `keybindings.json`: only explicitly named
    /// actions are copied, never the untouched defaults).
    explicit: std::collections::BTreeSet<TuiAction>,
}

impl Default for Keybindings {
    fn default() -> Self {
        Self::new()
    }
}

impl Keybindings {
    /// Defaults only (equals the previously hardcoded behavior).
    pub fn new() -> Self {
        let mut map = BTreeMap::new();
        for action in TuiAction::all() {
            map.insert(*action, action.default_bindings());
        }
        Self {
            map,
            id_overrides: BTreeMap::new(),
            app_overrides: BTreeMap::new(),
            alt_overrides: BTreeMap::new(),
            explicit: std::collections::BTreeSet::new(),
        }
    }

    /// Resolve from the raw settings `keybindings` value (an object mapping
    /// [`TuiAction::name`] or a registered upstream `tui.*` id to a key
    /// string or array of key strings). Unknown names and unparseable key
    /// strings are ignored; a non-object value returns the error (settings
    /// are still usable via [`Keybindings::new`]).
    ///
    /// When both the legacy camelCase name and the canonical `tui.*` id of
    /// one action are present, the canonical id wins (upstream treats the
    /// dotted ids as the primary namespace).
    pub fn from_settings(value: Option<&serde_json::Value>) -> Result<Self, String> {
        let mut bindings = Self::new();
        let Some(value) = value else {
            return Ok(bindings);
        };
        let Some(object) = value.as_object() else {
            return Err(format!(
                "keybindings must be an object, got {}",
                type_name_of(value)
            ));
        };
        // First pass: parse every entry to chords, keyed by the raw settings
        // name (two passes keep the canonical-over-legacy rule independent of
        // the settings object's entry order). An EXPLICIT EMPTY LIST is kept:
        // upstream `rebuild` resolves `userKeys === undefined ? defaults :
        // normalizeKeys(userKeys)`, so `[]` is an override that turns the id's
        // default chords off (keybindings.ts:263-267). A non-empty list whose
        // chords all fail to parse keeps the pre-existing fallback (a typo
        // must not silently unbind a default).
        let mut parsed: BTreeMap<&str, Vec<KeyCombo>> = BTreeMap::new();
        for (name, spec) in object {
            let specs: Vec<&str> = match spec {
                serde_json::Value::String(text) => vec![text.as_str()],
                serde_json::Value::Array(list) => {
                    list.iter().filter_map(serde_json::Value::as_str).collect()
                }
                _ => Vec::new(),
            };
            let combos: Vec<KeyCombo> = specs.iter().filter_map(|key| parse_key(key)).collect();
            let explicit_empty = matches!(spec, serde_json::Value::Array(list) if list.is_empty());
            if !combos.is_empty() || explicit_empty {
                parsed.insert(name.as_str(), combos);
            }
        }
        // Second pass: registered `tui.*` ids land in the id-keyed override
        // map (upstream `if (!(keybinding in this.definitions)) continue;`),
        // `app.*` ids land in the app-override map, and each action takes its
        // canonical id's chords when present.
        for (name, combos) in &parsed {
            if let Some(id) = tui_keybinding_definition(name).map(|definition| definition.id) {
                bindings.id_overrides.insert(id, combos.clone());
            }
            // W11-C: the routed `tui.altScreen.*` family accepts overrides
            // through the app-layer table too (the frozen `tui.*` registry
            // deliberately omits it).
            if let Some(id) = super::altscreen_search::alt_screen_id(name) {
                bindings.alt_overrides.insert(id, combos.clone());
            }
            // Every id in the app table accepts an override — including the
            // startup-hint ids (`app.model.cycleForward`, `app.interrupt`, …)
            // whose defaults live in [`hint_default_keys`] rather than the
            // config definitions registry. Upstream has ONE `APP_KEYBINDINGS`
            // map for both, so a keybindings.json entry for any of them must
            // win over the default.
            if let Some(id) = default_app_ids().iter().copied().find(|id| *id == *name) {
                bindings.app_overrides.insert(id, combos.clone());
            }
        }
        for action in TuiAction::all() {
            // Canonical ids win over the legacy camelCase name. An action has
            // at most one canonical id, in the `tui.*` or the `app.*`
            // namespace (`app.interrupt`, `app.clear`, …), and both spellings
            // are resolved at their own dispatch site.
            let canonical = action
                .upstream_id()
                .and_then(|id| parsed.get(id))
                .or_else(|| action.app_upstream_id().and_then(|id| parsed.get(id)));
            let legacy = parsed.get(action.name());
            if let Some(combos) = canonical.or(legacy) {
                bindings.map.insert(*action, combos.clone());
                bindings.explicit.insert(*action);
            }
        }
        Ok(bindings)
    }

    /// Resolve from `<agentDir>/keybindings.json` plus the settings
    /// `keybindings` object (upstream `KeybindingsManager.create` reads the
    /// file; pir additionally accepts the settings object as a per-scope
    /// override).
    ///
    /// Precedence, lowest to highest:
    ///
    /// 1. the hardcoded [`TuiAction`] / registry defaults;
    /// 2. every action named in `keybindings.json` (the file loader has
    ///    already migrated legacy names and dropped invalid chords);
    /// 3. the `settings.json` `keybindings` object.
    ///
    /// Settings win over the file because they are the more specific (project
    /// or user settings) scope in pir's configuration model; upstream only
    /// has the file, so a pi user's file reproduces upstream behavior
    /// exactly.
    pub fn from_settings_and_file(
        settings: Option<&serde_json::Value>,
        file: Option<&crate::config::keybindings::LoadedKeybindings>,
    ) -> Result<Self, String> {
        if let Some(settings) = settings {
            if !settings.is_object() && !settings.is_null() {
                return Err(format!(
                    "keybindings must be an object, got {}",
                    type_name_of(settings)
                ));
            }
        }
        // Build the file layer through the same resolver (it migrates legacy
        // names, keeps explicit empty lists and drops unknown names), then
        // overlay the settings layer. Overlaying — instead of merging the raw
        // objects — keeps the documented precedence: a settings entry wins
        // for its ACTION even when the file spelled the canonical id and the
        // settings entry used the legacy camelCase name.
        let file_value = file.map(|file| {
            let mut object = serde_json::Map::new();
            for (name, chords) in &file.bindings {
                let value = if chords.len() == 1 {
                    serde_json::Value::String(chords[0].clone())
                } else {
                    serde_json::Value::Array(
                        chords
                            .iter()
                            .map(|chord| serde_json::Value::String(chord.clone()))
                            .collect(),
                    )
                };
                object.insert(name.clone(), value);
            }
            serde_json::Value::Object(object)
        });
        let mut bindings = match file_value.as_ref() {
            Some(value) => Self::from_settings(Some(value))?,
            None => Self::new(),
        };
        if let Some(settings) = settings.filter(|value| !value.is_null()) {
            let overrides = Self::from_settings(Some(settings))?;
            bindings.apply_layer(&overrides);
        }
        Ok(bindings)
    }

    /// Overlay one resolved layer on this one. Only actions the layer
    /// explicitly named are copied (its untouched defaults must not clobber
    /// this layer's values); every id-keyed override is merged because the
    /// override maps only ever contain explicit entries.
    fn apply_layer(&mut self, layer: &Self) {
        for action in &layer.explicit {
            if let Some(combos) = layer.map.get(action) {
                self.map.insert(*action, combos.clone());
                self.explicit.insert(*action);
            }
        }
        self.id_overrides.extend(
            layer
                .id_overrides
                .iter()
                .map(|(id, combos)| (*id, combos.clone())),
        );
        self.app_overrides.extend(
            layer
                .app_overrides
                .iter()
                .map(|(id, combos)| (*id, combos.clone())),
        );
        self.alt_overrides.extend(
            layer
                .alt_overrides
                .iter()
                .map(|(id, combos)| (*id, combos.clone())),
        );
    }

    /// Does `key` trigger `action`?
    pub fn matches(&self, action: TuiAction, key: &KeyEvent) -> bool {
        self.map
            .get(&action)
            .is_some_and(|combos| combos.iter().any(|combo| combo.matches(key)))
    }

    /// The resolved chords for one registered `tui.*` id: the settings
    /// override when present, otherwise the platform-resolved effective
    /// default (upstream `KEYBINDINGS` over `TUI_KEYBINDINGS` — see
    /// [`tui_effective_default_keys_for`]; empty for the two history ids).
    /// `None` for unregistered ids.
    pub fn keys_for_id(&self, id: &str) -> Option<Vec<KeyCombo>> {
        if let Some(overrides) = self.id_overrides.get(id) {
            return Some(overrides.clone());
        }
        tui_effective_default_combos_for(id)
    }

    /// The settings override for one `tui.altScreen.*` viewport id, if the
    /// settings value named it (W11-C). `None` means "no override": the
    /// alt-screen router falls back to the upstream-verbatim defaults in
    /// `altscreen_search::ALT_SCREEN_KEYBINDINGS`.
    pub fn alt_id_combos(&self, id: &str) -> Option<Vec<KeyCombo>> {
        self.alt_overrides.get(id).cloned()
    }

    /// Does `key` match one registered `tui.*` id? (`None`-safe: unregistered
    /// ids never match.)
    pub fn matches_id(&self, id: &str, key: &KeyEvent) -> bool {
        self.keys_for_id(id)
            .is_some_and(|combos| combos.iter().any(|combo| combo.matches(key)))
    }

    /// Does the settings value install an EXPLICIT override for `id`? `None`
    /// for ids no override names. The `tui.altScreen.*` viewport family
    /// resolves its ids from the app layer (`altscreen_search.rs`), which
    /// needs to tell "no override, use the upstream default" from "override
    /// wins, even when it is empty".
    pub fn has_id_override(&self, id: &str) -> bool {
        self.id_overrides.contains_key(id)
    }

    /// The resolved chords for one `app.*` id: the settings override when
    /// present, otherwise the upstream-verbatim default from
    /// [`crate::config::keybindings::default_keybinding_definitions`], else
    /// the startup-hint default table ([`hint_default_keys`]) for the ids
    /// pir's config registry does not register (upstream `APP_KEYBINDINGS`).
    /// `None` for unregistered ids.
    pub fn keys_for_app_id(&self, id: &str) -> Option<Vec<KeyCombo>> {
        if let Some(overrides) = self.app_overrides.get(id) {
            return Some(overrides.clone());
        }
        if let Some(keys) = crate::config::keybindings::default_keys_for(id) {
            return Some(keys.iter().filter_map(|key| parse_key(key)).collect());
        }
        hint_default_keys(id).map(|keys| {
            keys.iter()
                .filter_map(|key| parse_key(key))
                .collect::<Vec<_>>()
        })
    }

    /// Does `key` match one `app.*` id? (`None`-safe: unregistered ids never
    /// match.)
    pub fn matches_app_id(&self, id: &str, key: &KeyEvent) -> bool {
        self.keys_for_app_id(id)
            .is_some_and(|combos| combos.iter().any(|combo| combo.matches(key)))
    }

    /// The display text for one `app.*` id's first binding (the startup
    /// header / `/hotkeys` labels; empty when unregistered or unbound).
    pub fn app_id_display(&self, id: &str) -> String {
        self.keys_for_app_id(id)
            .and_then(|combos| combos.first().map(super::terminal::combo_text))
            .unwrap_or_default()
    }

    /// The startup-header key text for one hint id (upstream `keyText` over
    /// `getKeybindings().getKeys(id)`, keybinding-hints.ts:34-36): the
    /// settings override when the id is registered, else the upstream
    /// platform default; multiple chords join with `/`. Empty when unbound.
    ///
    /// Delta vs upstream: only ids registered in
    /// [`crate::config::keybindings::default_keybinding_definitions`]
    /// (`app.editor.external`, `app.message.dequeue`) honor settings
    /// overrides; the remaining hint ids resolve from the platform default
    /// table because pir's settings plumbing registers only those.
    pub fn hint_key_text(&self, id: &str) -> String {
        if let Some(combos) = self.app_overrides.get(id) {
            return combos
                .iter()
                .map(super::terminal::combo_text)
                .collect::<Vec<_>>()
                .join("/");
        }
        if let Some(keys) = crate::config::keybindings::default_keys_for(id) {
            let combos: Vec<_> = keys.iter().filter_map(|key| parse_key(key)).collect();
            if !combos.is_empty() {
                return combos
                    .iter()
                    .map(super::terminal::combo_text)
                    .collect::<Vec<_>>()
                    .join("/");
            }
            return String::new();
        }
        hint_default_combos(id)
            .iter()
            .map(super::terminal::combo_text)
            .collect::<Vec<_>>()
            .join("/")
    }
}

/// Default chords for the startup-header hint ids and the `app.*` ids that
/// pir's config registry does not register (upstream `APP_KEYBINDINGS`,
/// `core/keybindings.ts:26-171`):
/// `app.interrupt`/`clear`/`exit`/`suspend`, the thinking-cycle/toggle/save
/// ids, the model cycle/select ids, `app.tools.expand`, the message
/// follow-up/dequeue/copy ids, `app.clipboard.pasteImage`, the tree label/fold
/// ids and the whole `app.session.*` family (new/tree/fork/resume, the
/// session-selector toggles and the session mutation ids). Windows/WSL
/// variants resolve at call time (keybindings.ts:29-32, 45-48, 67-78):
/// follow-up `ctrl+q`, dequeue `alt+q`, paste `alt+v`, model backward `alt+p`;
/// `app.suspend` keeps an EMPTY default on native win32 (its hint renders
/// with an empty key text).
///
/// The four `app.session.new/tree/fork/resume` ids keep upstream's EMPTY
/// default (keybindings.ts:79-82): they are gesture-free until the user binds
/// them in `keybindings.json` / settings, but the ids resolve and the override
/// is live.
///
/// The `cfg!(windows)` gate on `app.suspend` mirrors upstream's
/// `process.platform === "win32"` (NOT `useWindowsKeybindings()`): a WSL host
/// keeps `ctrl+z` (core/keybindings.ts:29-32).
pub fn hint_default_keys(id: &str) -> Option<Vec<String>> {
    hint_default_keys_for(id, crate::config::keybindings::KeybindingPlatform::host())
}

/// [`hint_default_keys`] with the platform predicates supplied explicitly
/// (tests pin the win32 / WSL / other rows on any host).
pub fn hint_default_keys_for(
    id: &str,
    platform: crate::config::keybindings::KeybindingPlatform,
) -> Option<Vec<String>> {
    let windows = platform.windows_keybindings;
    let keys: &[&str] = match id {
        "app.interrupt" => &["esc"],
        "app.clear" => &["ctrl+c"],
        "app.exit" => &["ctrl+d"],
        "app.suspend" if platform.native_windows => &[],
        "app.suspend" => &["ctrl+z"],
        "app.thinking.cycle" => &["shift+tab"],
        "app.thinking.toggle" => &["ctrl+t"],
        "app.model.cycleForward" => &["ctrl+p"],
        "app.model.cycleBackward" if windows => &["alt+p"],
        "app.model.cycleBackward" => &["shift+ctrl+p"],
        "app.model.select" => &["ctrl+l"],
        "app.tools.expand" => &["ctrl+o"],
        "app.message.followUp" if windows => &["ctrl+q"],
        "app.message.followUp" => &["alt+enter"],
        "app.clipboard.pasteImage" if windows => &["alt+v"],
        "app.clipboard.pasteImage" => &["ctrl+v"],
        // `app.session.*` (core/keybindings.ts:79-118). Upstream registers
        // these on the session selector / default editor; pir resolves the
        // same defaults here because they are outside the config registry.
        // The four navigation ids are bound to nothing by default.
        "app.session.new" | "app.session.tree" | "app.session.fork" | "app.session.resume" => &[],
        "app.session.toggleNamedFilter" => &["ctrl+n"],
        "app.session.togglePath" => &["ctrl+p"],
        "app.session.toggleSort" => &["ctrl+s"],
        "app.session.rename" => &["ctrl+r"],
        "app.session.delete" => &["ctrl+d"],
        "app.session.deleteNoninvasive" => &["ctrl+backspace"],
        _ => return None,
    };
    Some(keys.iter().map(|key| (*key).to_string()).collect())
}

/// The resolved default chords for one startup-header hint id
/// ([`hint_default_keys`] parsed through the shared key parser).
fn hint_default_combos(id: &str) -> Vec<super::keybindings::KeyCombo> {
    hint_default_keys(id)
        .map(|keys| keys.iter().filter_map(|key| parse_key(key)).collect())
        .unwrap_or_default()
}

fn type_name_of(value: &serde_json::Value) -> &'static str {
    match value {
        serde_json::Value::Null => "null",
        serde_json::Value::Bool(_) => "boolean",
        serde_json::Value::Number(_) => "number",
        serde_json::Value::String(_) => "string",
        serde_json::Value::Array(_) => "array",
        serde_json::Value::Object(_) => "object",
    }
}

/// Every `app.*` id the TUI accepts an override for: the ids in
/// [`crate::config::keybindings::default_keybinding_definitions`] plus the
/// ids whose defaults live in [`hint_default_keys`] (upstream has one
/// `APP_KEYBINDINGS` table covering both, 43 ids). The id strings are
/// `'static` so they can key the override map.
///
/// `app.session.rename` / `app.session.delete` / `app.session.deleteNoninvasive`
/// resolve and accept overrides and are dispatched by the `/sessions` tab
/// picker (`terminal::sessions_key_action`), which implements the upstream
/// rename/delete flows.
fn default_app_ids() -> &'static [&'static str] {
    &[
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
        "app.editor.external",
        "app.message.dequeue",
        "app.thinking.save",
        "app.message.copy",
        "app.models.save",
        "app.tree.editLabel",
        "app.tree.toggleLabelTimestamp",
        "app.tree.foldOrUp",
        "app.tree.unfoldOrDown",
        "app.models.enableAll",
        "app.models.clearAll",
        "app.models.toggleProvider",
        "app.models.reorderUp",
        "app.models.reorderDown",
        "app.tree.filter.default",
        "app.tree.filter.noTools",
        "app.tree.filter.userOnly",
        "app.tree.filter.labeledOnly",
        "app.tree.filter.all",
        "app.tree.filter.cycleForward",
        "app.tree.filter.cycleBackward",
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
        "tui.altScreen.previousPrompt",
        "tui.altScreen.nextPrompt",
        "tui.altScreen.search",
    ]
}
