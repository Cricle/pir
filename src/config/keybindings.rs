//! `keybindings.json` support (upstream `core/keybindings.ts` +
//! `core/keybindings-schema.ts`): the `<agentDir>/keybindings.json` file maps
//! action ids to a key chord string or an array of chord strings, `$schema` is
//! accepted but not treated as an action, legacy action names are migrated on
//! startup, and chords that do not parse are dropped.
//!
//! Default keys for the upstream action ids that pir's TUI does not yet
//! mirror in [`crate::tui::keybindings::TuiAction`] are registered in
//! [`default_keybinding_definitions`]. Note the upstream conflict semantics:
//! default chords intentionally collide across action ids (e.g. `ctrl+s` is
//! both `app.thinking.save` and `app.session.toggleSort`; `ctrl+p` is
//! `app.models.toggleProvider`, `app.session.togglePath` and
//! `app.model.cycleForward`) because a match is resolved by focus context —
//! only the focused component queries the ids it owns. The definitions
//! therefore must not be used as a global chord → action table.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde_json::Value;

/// Legacy action-id migrations applied on startup (upstream
/// `KEYBINDING_NAME_MIGRATIONS`, keybindings.ts:201).
pub const KEYBINDING_NAME_MIGRATIONS: &[(&str, &str)] = &[
    ("cursorUp", "tui.editor.cursorUp"),
    ("cursorDown", "tui.editor.cursorDown"),
    ("cursorLeft", "tui.editor.cursorLeft"),
    ("cursorRight", "tui.editor.cursorRight"),
    ("cursorWordLeft", "tui.editor.cursorWordLeft"),
    ("cursorWordRight", "tui.editor.cursorWordRight"),
    ("cursorLineStart", "tui.editor.cursorLineStart"),
    ("cursorLineEnd", "tui.editor.cursorLineEnd"),
    ("jumpForward", "tui.editor.jumpForward"),
    ("jumpBackward", "tui.editor.jumpBackward"),
    ("pageUp", "tui.editor.pageUp"),
    ("pageDown", "tui.editor.pageDown"),
    ("deleteCharBackward", "tui.editor.deleteCharBackward"),
    ("deleteCharForward", "tui.editor.deleteCharForward"),
    ("deleteWordBackward", "tui.editor.deleteWordBackward"),
    ("deleteWordForward", "tui.editor.deleteWordForward"),
    ("deleteToLineStart", "tui.editor.deleteToLineStart"),
    ("deleteToLineEnd", "tui.editor.deleteToLineEnd"),
    ("yank", "tui.editor.yank"),
    ("yankPop", "tui.editor.yankPop"),
    ("undo", "tui.editor.undo"),
    ("newLine", "tui.input.newLine"),
    ("submit", "tui.input.submit"),
    ("tab", "tui.input.tab"),
    ("copy", "tui.input.copy"),
    ("selectUp", "tui.select.up"),
    ("selectDown", "tui.select.down"),
    ("selectPageUp", "tui.select.pageUp"),
    ("selectPageDown", "tui.select.pageDown"),
    ("selectConfirm", "tui.select.confirm"),
    ("selectCancel", "tui.select.cancel"),
    ("interrupt", "app.interrupt"),
    ("clear", "app.clear"),
    ("exit", "app.exit"),
    ("suspend", "app.suspend"),
    ("cycleThinkingLevel", "app.thinking.cycle"),
    ("cycleModelForward", "app.model.cycleForward"),
    ("cycleModelBackward", "app.model.cycleBackward"),
    ("selectModel", "app.model.select"),
    ("expandTools", "app.tools.expand"),
    ("toggleThinking", "app.thinking.toggle"),
    ("toggleSessionNamedFilter", "app.session.toggleNamedFilter"),
    ("externalEditor", "app.editor.external"),
    ("followUp", "app.message.followUp"),
    ("dequeue", "app.message.dequeue"),
    ("pasteImage", "app.clipboard.pasteImage"),
    ("newSession", "app.session.new"),
    ("tree", "app.session.tree"),
    ("fork", "app.session.fork"),
    ("resume", "app.session.resume"),
    ("treeFoldOrUp", "app.tree.foldOrUp"),
    ("treeUnfoldOrDown", "app.tree.unfoldOrDown"),
    ("treeEditLabel", "app.tree.editLabel"),
    ("treeToggleLabelTimestamp", "app.tree.toggleLabelTimestamp"),
    ("toggleSessionPath", "app.session.togglePath"),
    ("toggleSessionSort", "app.session.toggleSort"),
    ("renameSession", "app.session.rename"),
    ("deleteSession", "app.session.delete"),
    ("deleteSessionNoninvasive", "app.session.deleteNoninvasive"),
];

/// Whether the Windows keybinding variants apply (upstream
/// `useWindowsKeybindings`, keybindings.ts:16-21): win32, or a Linux host
/// running under WSL.
pub fn use_windows_keybindings() -> bool {
    cfg!(windows)
        || std::env::var_os("WSL_DISTRO_NAME").is_some()
        || std::env::var_os("WSL_INTEROP").is_some()
}

/// The two upstream platform predicates the default chords branch on
/// (`core/keybindings.ts`):
///
/// - `native_windows`: `process.platform === "win32"` — a native Windows
///   host. `app.suspend` is bound to an EMPTY default there (line 29-32) and
///   `tui.editor.undo` is `ctrl+z` (line 182-185).
/// - `windows_keybindings`: upstream `useWindowsKeybindings()` (line 16-21) —
///   `native_windows` OR a Linux host under WSL. It selects the
///   `app.model.cycleBackward` (`alt+p`), `app.message.followUp` (`ctrl+q`),
///   `app.message.dequeue` (`alt+q`), `app.clipboard.pasteImage` (`alt+v`),
///   the `tui.altScreen.*` prompt/search chords, and `tui.editor.undo`
///   (`alt+z` on WSL).
///
/// The two are NOT the same predicate (WSL is `windows_keybindings` but not
/// `native_windows`), so the resolution functions take the pair explicitly and
/// [`KeybindingPlatform::host`] supplies the live values — which also lets
/// tests pin every platform row on any host.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeybindingPlatform {
    /// `process.platform === "win32"`.
    pub native_windows: bool,
    /// Upstream `useWindowsKeybindings()` (win32 or WSL on Linux).
    pub windows_keybindings: bool,
}

impl KeybindingPlatform {
    /// The live host's predicates.
    pub fn host() -> Self {
        Self {
            native_windows: cfg!(windows),
            windows_keybindings: use_windows_keybindings(),
        }
    }
}

/// One upstream keybinding definition (upstream `KeybindingDefinition`): the
/// action id, its default chords and the user-facing description.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeybindingDefinition {
    pub id: &'static str,
    pub default_keys: Vec<String>,
    pub description: &'static str,
}

/// Default-key definitions for the upstream action ids that pir does not
/// mirror in [`crate::tui::keybindings::TuiAction`] (task G10 / upstream
/// `core/keybindings.ts:37-197`): the thinking/model save actions, the
/// `app.message.copy` id, the scoped model selector (`app.models.*`), the
/// tree filter modes (`app.tree.filter.*`) and the alt-screen prompt
/// navigation (`tui.altScreen.*`). Chords are verbatim from upstream; the
/// three `tui.altScreen.*` ids resolve their Windows variants at call time
/// (keybindings.ts:186-196).
pub fn default_keybinding_definitions() -> Vec<KeybindingDefinition> {
    default_keybinding_definitions_for(KeybindingPlatform::host())
}

/// [`default_keybinding_definitions`] with the platform predicates supplied
/// explicitly (tests pin the win/non-win rows on any host).
pub fn default_keybinding_definitions_for(
    platform: KeybindingPlatform,
) -> Vec<KeybindingDefinition> {
    fn simple(
        id: &'static str,
        default_keys: &[&str],
        description: &'static str,
    ) -> KeybindingDefinition {
        KeybindingDefinition {
            id,
            default_keys: default_keys.iter().map(|key| (*key).to_string()).collect(),
            description,
        }
    }
    let windows = platform.windows_keybindings;
    vec![
        simple("app.editor.external", &["ctrl+g"], "Open external editor"),
        simple(
            "app.message.dequeue",
            if windows { &["alt+q"] } else { &["alt+up"] },
            "Restore queued messages",
        ),
        simple("app.thinking.save", &["ctrl+s"], "Save thinking level"),
        simple(
            "app.message.copy",
            &["ctrl+x"],
            "Copy selection or last assistant message",
        ),
        simple("app.models.save", &["ctrl+s"], "Save model selection"),
        simple("app.tree.editLabel", &["shift+l"], "Edit tree label"),
        simple(
            "app.tree.toggleLabelTimestamp",
            &["shift+t"],
            "Toggle tree label timestamps",
        ),
        // Tree fold navigation (upstream `APP_KEYBINDINGS`, keybindings.ts:
        // 88-97): darwin puts alt before ctrl; pir keeps the non-darwin order
        // on every host (the platform type has no darwin predicate).
        simple(
            "app.tree.foldOrUp",
            &["ctrl+left", "alt+left"],
            "Fold tree branch or move up",
        ),
        simple(
            "app.tree.unfoldOrDown",
            &["ctrl+right", "alt+right"],
            "Unfold tree branch or move down",
        ),
        simple("app.models.enableAll", &["ctrl+a"], "Enable all models"),
        simple("app.models.clearAll", &["ctrl+x"], "Clear all models"),
        simple(
            "app.models.toggleProvider",
            &["ctrl+p"],
            "Toggle all models for provider",
        ),
        simple(
            "app.models.reorderUp",
            &["alt+up"],
            "Move model up in order",
        ),
        simple(
            "app.models.reorderDown",
            &["alt+down"],
            "Move model down in order",
        ),
        simple(
            "app.tree.filter.default",
            &["ctrl+d"],
            "Tree filter: default view",
        ),
        simple(
            "app.tree.filter.noTools",
            &["ctrl+t"],
            "Tree filter: hide tool results",
        ),
        simple(
            "app.tree.filter.userOnly",
            &["ctrl+u"],
            "Tree filter: user messages only",
        ),
        simple(
            "app.tree.filter.labeledOnly",
            &["ctrl+l"],
            "Tree filter: labeled entries only",
        ),
        simple(
            "app.tree.filter.all",
            &["ctrl+a"],
            "Tree filter: show all entries",
        ),
        simple(
            "app.tree.filter.cycleForward",
            &["ctrl+o"],
            "Tree filter: cycle forward",
        ),
        simple(
            "app.tree.filter.cycleBackward",
            &["shift+ctrl+o"],
            "Tree filter: cycle backward",
        ),
        if windows {
            simple(
                "tui.altScreen.previousPrompt",
                &["ctrl+up"],
                "Jump to previous semantic prompt",
            )
        } else {
            simple(
                "tui.altScreen.previousPrompt",
                &["ctrl+shift+up", "ctrl+up"],
                "Jump to previous semantic prompt",
            )
        },
        if windows {
            simple(
                "tui.altScreen.nextPrompt",
                &["ctrl+down"],
                "Jump to next semantic prompt",
            )
        } else {
            simple(
                "tui.altScreen.nextPrompt",
                &["ctrl+shift+down", "ctrl+down"],
                "Jump to next semantic prompt",
            )
        },
        if windows {
            simple(
                "tui.altScreen.search",
                &["ctrl+f"],
                "Search the primary scroll view",
            )
        } else {
            simple(
                "tui.altScreen.search",
                &["ctrl+shift+f"],
                "Search the primary scroll view",
            )
        },
    ]
}

/// The default chords for one action id (upstream
/// `KeybindingsManager.getKeys` over the default definitions).
pub fn default_keys_for(action_id: &str) -> Option<Vec<String>> {
    default_keybinding_definitions()
        .into_iter()
        .find(|definition| definition.id == action_id)
        .map(|definition| definition.default_keys)
}

/// One loaded keybinding file.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct LoadedKeybindings {
    /// Action id -> chord strings. Invalid chords are dropped.
    pub bindings: BTreeMap<String, Vec<String>>,
    /// Non-fatal problems (unparseable chords, invalid values).
    pub errors: Vec<String>,
    /// Whether a legacy action id was migrated (upstream `migrated`).
    pub migrated: bool,
}

/// Validate one chord string with the shared TUI key parser
/// (`crate::tui::keybindings::parse_key`); empty or unparseable chords are
/// rejected.
pub fn is_valid_chord(chord: &str) -> bool {
    let chord = chord.trim();
    if chord.is_empty() {
        return false;
    }
    // The authoritative parser lives in the TUI (it owns the key-name table).
    #[cfg(feature = "tui")]
    {
        crate::tui::keybindings::parse_key(chord).is_some()
    }
    // Without it the chord's *shape* is still validated: at least one part and
    // no empty part around the `+` separators, so junk such as `"ctrl+"`
    // or `"a++b"` is reported instead of being accepted silently.
    #[cfg(not(feature = "tui"))]
    {
        let mut parts = chord.split('+');
        let first = parts.next().unwrap_or_default();
        !first.trim().is_empty() && parts.all(|part| !part.trim().is_empty())
    }
}

/// Parse + migrate a `keybindings.json` body (upstream
/// `migrateKeybindingsConfig` + `toKeybindingsConfig`).
pub fn parse_keybindings(contents: &str) -> LoadedKeybindings {
    let mut loaded = LoadedKeybindings::default();
    let text = contents.strip_prefix('\u{feff}').unwrap_or(contents);
    let Ok(value) = serde_json::from_str::<Value>(text) else {
        loaded
            .errors
            .push("keybindings.json is not valid JSON".to_string());
        return loaded;
    };
    let Some(map) = value.as_object() else {
        loaded
            .errors
            .push("keybindings.json must be an object".to_string());
        return loaded;
    };
    for (key, value) in map {
        // Migrate legacy action names; a legacy name is skipped when the
        // modern name is also present (upstream migrateKeybindingsConfig).
        let migrated_name = KEYBINDING_NAME_MIGRATIONS
            .iter()
            .find(|(from, _)| *from == key)
            .map(|(_, to)| (*to).to_string());
        let (name, migrated) = match migrated_name {
            Some(to) if map.contains_key(&to) => {
                loaded.migrated = true;
                continue;
            }
            Some(to) => (to, true),
            None => (key.clone(), false),
        };
        if migrated {
            loaded.migrated = true;
        }
        if name == "$schema" {
            continue;
        }
        // An EXPLICIT empty list is an override that turns the id's default
        // chords off (upstream `rebuild`: `userKeys === undefined ? defaults :
        // normalizeKeys(userKeys)`), so it is kept even though it yields no
        // chords.
        let explicit_empty = matches!(&value, Value::Array(list) if list.is_empty());
        let chords: Vec<String> = match value {
            Value::String(chord) => vec![chord.clone()],
            Value::Array(list) => list
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect(),
            _ => {
                loaded.errors.push(format!(
                    "keybinding \"{name}\" must be a string or array of strings"
                ));
                continue;
            }
        };
        let mut valid = Vec::new();
        for chord in chords {
            if is_valid_chord(&chord) {
                valid.push(chord);
            } else {
                loaded.errors.push(format!(
                    "keybinding \"{name}\": invalid key chord \"{chord}\""
                ));
            }
        }
        if !valid.is_empty() || explicit_empty {
            loaded.bindings.insert(name, valid);
        }
    }
    loaded
}

/// Load `<agentDir>/keybindings.json`. A missing file yields defaults; a
/// migrated file is rewritten in place (startup migration, upstream
/// `KeybindingsManager.create` semantics).
pub fn load_keybindings_file(agent_dir: &Path) -> LoadedKeybindings {
    let path = keybindings_path(agent_dir);
    let Ok(contents) = std::fs::read_to_string(&path) else {
        return LoadedKeybindings::default();
    };
    let loaded = parse_keybindings(&contents);
    if loaded.migrated {
        // Best-effort startup migration write; failures are non-fatal.
        let mut out = serde_json::Map::new();
        out.insert(
            "$schema".to_string(),
            Value::String(crate::config::schema::SCHEMA_BASE_URL_KEYBINDINGS.to_string()),
        );
        for (name, chords) in &loaded.bindings {
            out.insert(
                name.clone(),
                if chords.len() == 1 {
                    Value::String(chords[0].clone())
                } else {
                    serde_json::to_value(chords).unwrap_or(Value::Null)
                },
            );
        }
        if let Ok(serialized) = serde_json::to_string_pretty(&Value::Object(out)) {
            // Atomic: a crash mid-migration must not leave a truncated
            // keybindings.json behind (the original file is replaced whole).
            let _ = crate::config::settings::write_text_atomic(&path, &serialized);
        }
    }
    loaded
}

/// The `<agentDir>/keybindings.json` path.
pub fn keybindings_path(agent_dir: &Path) -> PathBuf {
    agent_dir.join("keybindings.json")
}
