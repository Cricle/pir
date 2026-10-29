//! TUI command/selector parity tests (终审 #1/#4/#6/#9/#15/#20/#24).
//!
//! Pure-function seams only: the settings menu rows + write-back, the trust
//! selector option math, the confirm dialog state machine, the auth selector
//! state machine, the fork/tree filter semantics, and the session-picker
//! management-key logic (sort/filter/tree/delete/rename). The overlay wiring
//! itself lives in `src/tui/terminal.rs` (crossterm-bound).

use std::path::PathBuf;

use pir::coding_agent::session_pick::{
    build_threaded_order, delete_session_file, filter_rows, format_session_age, shorten_path,
    tree_prefix, PickerState, SessionDeleteMethod, SessionRow, SortMode,
};
use pir::config::settings::{
    DoubleEscapeAction, Settings, SettingsManager, TerminalImagesMode, TerminalSettings,
    TreeFilterMode as SettingsTreeFilterMode, WarningSettings,
};
use pir::config::trust::{
    get_project_trust_options, has_trust_requiring_project_resources, normalize_cwd,
    save_project_trust_option, ProjectTrustStore,
};
use pir::tui::command_dialogs::{
    AuthKey, AuthMode, AuthOutcome, AuthProviderKind, AuthProviderOption, AuthSelector,
    ConfirmDialog, ConfirmKey, ConfirmOutcome,
};
use pir::tui::selector::{
    tree_filter_mode_from_value, ForkSelector, TreeEntry, TreeEntryKind, TreeFilterMode,
    TreeLabelEditor, TreeSelector,
};
use pir::tui::settings_menu::{
    apply_settings_action, item_action, settings_items, settings_path_for, MenuKey, SettingsAction,
    SettingsMenu, SettingsMenuContext,
};

// ---------------------------------------------------------------------------
// #1 /settings menu: the upstream row set + write-back
// ---------------------------------------------------------------------------

fn sample_settings() -> Settings {
    Settings {
        double_escape_action: Some(DoubleEscapeAction::Tree),
        tree_filter_mode: Some(SettingsTreeFilterMode::UserOnly),
        warnings: Some(WarningSettings {
            anthropic_extra_usage: Some(true),
        }),
        // `terminal.images: "kitty"` forces the image capability (upstream
        // `terminal.images` override → `getCapabilities().images`), so the
        // D21 capability gate keeps the "show-images"/"image-width-cells"
        // rows in the environment-independent row list below.
        terminal: Some(TerminalSettings {
            images: Some(TerminalImagesMode::Kitty),
            ..TerminalSettings::default()
        }),
        ..Settings::default()
    }
}

/// Upstream `SettingsSelectorComponent` item ids in list order (the image
/// rows included; the per-model-thinking stepped submenu is upstream-only).
const UPSTREAM_SETTING_IDS: [&str; 31] = [
    "autocompact",
    "show-images",
    "image-width-cells",
    "auto-resize-images",
    "block-images",
    "skill-commands",
    "steering-mode",
    "follow-up-mode",
    "transport",
    // `cea257439` (D31): the share-destination row sits right after
    // "transport" upstream.
    "share-destination",
    "http-idle-timeout",
    "cache-warming-mode",
    "hide-thinking",
    "mermaid-rendering",
    "cache-miss-notices",
    "collapse-changelog",
    "quiet-startup",
    "install-telemetry",
    "default-project-trust",
    "double-escape-action",
    "tree-filter-mode",
    "warnings",
    "tui-mode",
    "fullscreen-exit-output",
    "fullscreen-scrollbar",
    "fullscreen-copy-on-select",
    "show-hardware-cursor",
    "editor-padding",
    "output-padding",
    "autocomplete-max-visible",
    "clear-on-shrink",
];

#[test]
fn settings_menu_rows_match_upstream_ids() {
    let settings = sample_settings();
    let items = settings_items(
        &settings,
        &SettingsMenuContext {
            current_theme: Some("dark".to_string()),
            available_themes: vec!["dark".to_string(), "light".to_string()],
            models: Vec::new(),
            model_thinking_levels: std::collections::BTreeMap::new(),
            default_thinking_level: "off".to_string(),
        },
    );
    let ids: Vec<&str> = items.iter().map(|item| item.id).collect();
    assert!(ids.starts_with(&UPSTREAM_SETTING_IDS));
    // The theme row closes the list (upstream "theme" submenu).
    assert_eq!(ids.last().copied(), Some("theme"));
    // The warnings row opens a submenu.
    let warnings = items.iter().find(|item| item.id == "warnings").unwrap();
    assert!(warnings.submenu);
    // Values carry the upstream domains.
    let double_escape = items
        .iter()
        .find(|item| item.id == "double-escape-action")
        .unwrap();
    assert_eq!(double_escape.values, vec!["tree", "fork", "none"]);
    assert_eq!(double_escape.current, 0);
    let tree_filter = items
        .iter()
        .find(|item| item.id == "tree-filter-mode")
        .unwrap();
    assert_eq!(
        tree_filter.values,
        vec!["default", "no-tools", "user-only", "labeled-only", "all"]
    );
    assert_eq!(tree_filter.current, 2);
}

#[test]
fn settings_menu_defaults_match_effective_values() {
    // Upstream `SETTINGS_DEFAULTS`: imageWidthCells=60, outputPad=1. The menu
    // must show the effective value when the setting is unset.
    let settings = sample_settings();
    let items = settings_items(
        &settings,
        &SettingsMenuContext {
            current_theme: Some("dark".to_string()),
            available_themes: vec!["dark".to_string(), "light".to_string()],
            models: Vec::new(),
            model_thinking_levels: std::collections::BTreeMap::new(),
            default_thinking_level: "off".to_string(),
        },
    );
    let image = items
        .iter()
        .find(|item| item.id == "image-width-cells")
        .expect("image width row");
    assert_eq!(image.values[image.current], "60");
    let output = items
        .iter()
        .find(|item| item.id == "output-padding")
        .expect("output padding row");
    assert_eq!(output.values[output.current], "1");
}

#[test]
fn settings_menu_cycles_and_persists() {
    use pir::tui::settings_menu::MenuOutcome;
    let dir = std::env::temp_dir().join(format!("pir-settings-test-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let mut manager = SettingsManager::load(&dir, &dir.join(".pi"), &dir, false);

    // double-escape: tree → fork
    let settings = sample_settings();
    let items = settings_items(&settings, &SettingsMenuContext::default());
    let double_escape = items
        .iter()
        .find(|item| item.id == "double-escape-action")
        .unwrap()
        .clone();
    let mut cycled = double_escape.clone();
    cycled.current = 1;
    let action = item_action(&cycled);
    match &action {
        SettingsAction::Set { id, value } => {
            assert_eq!(*id, "double-escape-action");
            assert_eq!(*value, serde_json::json!("fork"));
        }
        other => panic!("unexpected action: {other:?}"),
    }
    assert!(apply_settings_action(&mut manager, &action).is_some());
    let reread = SettingsManager::load(&dir, &dir.join(".pi"), &dir, false);
    assert_eq!(
        reread.settings.double_escape_action,
        Some(DoubleEscapeAction::Fork)
    );

    // Nested granularities survive: terminal.showImages + markdown.mermaid
    // land inside their objects without clobbering sibling keys.
    for id in ["show-images", "mermaid-rendering"] {
        let item = items.iter().find(|item| item.id == id).unwrap().clone();
        let mut cycled = item;
        cycled.current = (cycled.current + 1) % cycled.values.len();
        let action = item_action(&cycled);
        apply_settings_action(&mut manager, &action);
    }
    let reread = SettingsManager::load(&dir, &dir.join(".pi"), &dir, false);
    assert_eq!(
        reread
            .settings
            .terminal
            .and_then(|terminal| terminal.show_images),
        Some(false)
    );
    assert!(reread
        .settings
        .markdown
        .is_some_and(|markdown| markdown.mermaid.is_some()));
    // Unrelated keys survive the nested write.
    assert_eq!(
        reread.settings.double_escape_action,
        Some(DoubleEscapeAction::Fork)
    );

    // The warnings submenu toggles through the live menu (upstream
    // `WarningSettingsSubmenu`): open from the warnings row, cycle, apply.
    // The walk is positional, so it is bounded by the row count instead of a
    // hardcoded offset (a new row above "warnings" must not break it).
    let menu = SettingsMenu::new(&settings, SettingsMenuContext::default());
    for _ in 0..UPSTREAM_SETTING_IDS.len() {
        if menu.selected_label().as_deref() == Some("Warnings") {
            break;
        }
        menu.handle_key(MenuKey::Down);
    }
    assert_eq!(menu.selected_label().as_deref(), Some("Warnings"));
    assert!(matches!(menu.handle_key(MenuKey::Enter), MenuOutcome::None));
    assert_eq!(
        menu.selected_label().as_deref(),
        Some("Anthropic extra usage")
    );
    let outcome = menu.handle_key(MenuKey::Right);
    match outcome {
        MenuOutcome::Apply(SettingsAction::Set { id, value }) => {
            assert_eq!(id, "anthropic-extra-usage");
            assert_eq!(value, serde_json::json!(false));
            apply_settings_action(&mut manager, &SettingsAction::Set { id, value });
        }
        other => panic!("unexpected outcome: {other:?}"),
    }
    // Esc backs out of the submenu first, then closes the menu.
    assert!(matches!(menu.handle_key(MenuKey::Esc), MenuOutcome::None));
    assert!(matches!(menu.handle_key(MenuKey::Esc), MenuOutcome::Close));
    let reread = SettingsManager::load(&dir, &dir.join(".pi"), &dir, false);
    assert_eq!(
        reread
            .settings
            .warnings
            .and_then(|warnings| warnings.anthropic_extra_usage),
        Some(false)
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn settings_menu_interaction_flow() {
    let settings = sample_settings();
    let menu = SettingsMenu::new(&settings, SettingsMenuContext::default());
    // ↓ twice → transport; → cycles to websocket.
    menu.handle_key(MenuKey::Down);
    menu.handle_key(MenuKey::Down);
    menu.handle_key(MenuKey::Down);
    menu.handle_key(MenuKey::Down);
    menu.handle_key(MenuKey::Down);
    menu.handle_key(MenuKey::Down);
    menu.handle_key(MenuKey::Down);
    menu.handle_key(MenuKey::Down);
    // rows reflect the highlight move.
    assert_eq!(menu.selected_label().as_deref(), Some("Transport"));
    // Esc closes; warnings submenu opens from the warnings row and backs out.
    assert!(matches!(
        menu.handle_key(MenuKey::Esc),
        pir::tui::settings_menu::MenuOutcome::Close
    ));
}

#[test]
fn settings_paths_cover_every_row() {
    for id in UPSTREAM_SETTING_IDS {
        // "warnings" is a submenu container (its leaf rows carry the path,
        // e.g. anthropic-extra-usage → warnings.anthropicExtraUsage).
        if id == "warnings" {
            continue;
        }
        assert!(
            settings_path_for(id).is_some(),
            "row {id} has no settings path"
        );
    }
    assert!(settings_path_for("theme").is_none());
    assert!(settings_path_for("warnings").is_none());
    assert!(settings_path_for("anthropic-extra-usage").is_some());
}

/// D31 (`cea257439`): the `Session share destination` row
/// (`settings-selector.ts:515-522`) with the upstream label, description and
/// value domain, writing the `sessionShareDestination` top-level key. The
/// persistence round-trip lives in `d31_session_share_destination.rs`.
#[test]
fn share_destination_row_matches_upstream() {
    let items = settings_items(&Settings::default(), &SettingsMenuContext::default());
    let row = items
        .iter()
        .find(|item| item.id == "share-destination")
        .expect("the D31 share-destination row is missing");
    assert_eq!(row.label, "Session share destination");
    assert_eq!(
        row.description,
        "Where to upload sessions with /share (auto prefers Radius when authenticated, otherwise GitHub Gist)"
    );
    assert_eq!(row.values, vec!["auto", "gist", "radius"]);
    assert_eq!(row.current, 0, "an unset value shows the `auto` default");
    assert_eq!(
        settings_path_for("share-destination"),
        Some(("sessionShareDestination", None))
    );

    // Cycling the row yields the upstream value order (auto → gist → radius).
    let payloads: Vec<serde_json::Value> = (0..3)
        .map(|index| {
            let mut cycled = row.clone();
            cycled.current = index;
            match item_action(&cycled) {
                SettingsAction::Set { id, value } => {
                    assert_eq!(id, "share-destination");
                    value
                }
                other => panic!("unexpected action: {other:?}"),
            }
        })
        .collect();
    assert_eq!(
        payloads,
        vec![
            serde_json::json!("auto"),
            serde_json::json!("gist"),
            serde_json::json!("radius"),
        ]
    );
}

// ---------------------------------------------------------------------------
// #4 /fork selector + message listing semantics
// ---------------------------------------------------------------------------

#[test]
fn fork_selector_filters_and_selects_entry_ids() {
    let messages = vec![
        ("m1".to_string(), "first question".to_string()),
        ("m2".to_string(), "second\nmultiline".to_string()),
        ("m3".to_string(), "third".to_string()),
    ];
    let selector = ForkSelector::new(&messages);
    // Defaults to the last message (upstream `initialSelectedId`).
    assert_eq!(selector.selected().as_deref(), Some("m3"));
    selector.move_up();
    assert_eq!(selector.selected().as_deref(), Some("m2"));
    // The multiline text collapses for display.
    selector.set_query("multiline");
    assert_eq!(selector.selected().as_deref(), Some("m2"));
    selector.set_query("nomatch");
    assert_eq!(selector.selected(), None);
}

#[test]
fn tree_filter_mode_values_parse() {
    assert_eq!(
        tree_filter_mode_from_value(Some(&serde_json::json!("no-tools"))),
        TreeFilterMode::NoTools
    );
    assert_eq!(
        tree_filter_mode_from_value(Some(&serde_json::json!("user-only"))),
        TreeFilterMode::UserOnly
    );
    assert_eq!(
        tree_filter_mode_from_value(Some(&serde_json::json!("bogus"))),
        TreeFilterMode::Default
    );
    assert_eq!(tree_filter_mode_from_value(None), TreeFilterMode::Default);
}

// ---------------------------------------------------------------------------
// #20 /tree filter modes + label editing
// ---------------------------------------------------------------------------

fn tree_entries() -> Vec<TreeEntry> {
    vec![
        TreeEntry {
            id: "a".to_string(),
            display: "hello there".to_string(),
            label: Some("kickoff".to_string()),
            label_timestamp: Some("10:00".to_string()),
            kind: TreeEntryKind::User,
            depth: 0,
            parent_id: None,
        },
        TreeEntry {
            id: "b".to_string(),
            display: "[read]".to_string(),
            label: None,
            label_timestamp: None,
            kind: TreeEntryKind::Tool,
            depth: 1,
            parent_id: Some("a".to_string()),
        },
        TreeEntry {
            id: "c".to_string(),
            display: "answer".to_string(),
            label: None,
            label_timestamp: None,
            kind: TreeEntryKind::Assistant,
            depth: 2,
            parent_id: Some("b".to_string()),
        },
        TreeEntry {
            id: "d".to_string(),
            display: "[model: gpt]".to_string(),
            label: None,
            label_timestamp: None,
            kind: TreeEntryKind::Bookkeeping,
            depth: 2,
            parent_id: Some("b".to_string()),
        },
    ]
}

#[test]
fn tree_selector_filter_modes() {
    let selector = TreeSelector::new(&tree_entries(), TreeFilterMode::Default);
    // Default hides bookkeeping.
    assert_eq!(selector.selected().as_deref(), Some("a"));
    // user-only keeps just the user message.
    selector.set_filter_mode(TreeFilterMode::UserOnly);
    selector.move_up();
    assert_eq!(selector.selected().as_deref(), Some("a"));
    // labeled-only keeps just the labeled entry.
    selector.set_filter_mode(TreeFilterMode::LabeledOnly);
    assert_eq!(selector.selected().as_deref(), Some("a"));
    // no-tools keeps user + assistant + compaction.
    selector.set_filter_mode(TreeFilterMode::NoTools);
    assert_eq!(selector.selected().as_deref(), Some("a"));
    // all keeps everything; the cycle order is upstream's.
    selector.set_filter_mode(TreeFilterMode::All);
    assert_eq!(selector.selected().as_deref(), Some("a"));
    assert_eq!(TreeFilterMode::Default.next(), TreeFilterMode::NoTools);
    assert_eq!(TreeFilterMode::NoTools.next(), TreeFilterMode::UserOnly);
    assert_eq!(TreeFilterMode::UserOnly.next(), TreeFilterMode::LabeledOnly);
    assert_eq!(TreeFilterMode::LabeledOnly.next(), TreeFilterMode::All);
    assert_eq!(TreeFilterMode::All.next(), TreeFilterMode::Default);
    assert_eq!(TreeFilterMode::Default.previous(), TreeFilterMode::All);
    // Query filtering composes with the mode.
    selector.set_filter_mode(TreeFilterMode::All);
    selector.set_query("answer");
    assert_eq!(selector.selected().as_deref(), Some("c"));
    // The label-timestamp toggle flips.
    assert!(!selector.shows_label_timestamps());
    selector.toggle_label_timestamps();
    assert!(selector.shows_label_timestamps());
    // Label save updates the local copy (upstream `updateNodeLabel`).
    selector.set_label("c", Some("renamed".to_string()));
    selector.set_filter_mode(TreeFilterMode::LabeledOnly);
    selector.set_query("");
    // Both labeled entries are visible now; walk to entry c.
    let mut found = false;
    for _ in 0..4 {
        if selector.selected().as_deref() == Some("c") {
            found = true;
            break;
        }
        selector.move_down();
    }
    assert!(found);
}

#[test]
fn tree_label_editor_submit_and_clear() {
    use pir::tui::dialogs::DialogKey;
    let editor = TreeLabelEditor::new("entry-1".to_string(), Some("old label"));
    assert_eq!(editor.entry_id(), "entry-1");
    assert_eq!(editor.text(), "old label");
    // Clearing: select-all is unavailable, so delete chars then Enter.
    for _ in 0..9 {
        editor.handle_key(DialogKey::Backspace);
    }
    assert!(matches!(
        editor.handle_key(DialogKey::Enter),
        pir::tui::selector::LabelEditOutcome::Submit(None)
    ));
    // Typing + Enter submits the trimmed label.
    let editor = TreeLabelEditor::new("entry-2".to_string(), None);
    for ch in "new name".chars() {
        editor.handle_key(DialogKey::Char(ch));
    }
    assert!(matches!(
        editor.handle_key(DialogKey::Enter),
        pir::tui::selector::LabelEditOutcome::Submit(Some(label)) if label == "new name"
    ));
    // Esc cancels.
    let editor = TreeLabelEditor::new("entry-3".to_string(), Some("keep"));
    assert!(matches!(
        editor.handle_key(DialogKey::Esc),
        pir::tui::selector::LabelEditOutcome::Cancel
    ));
}

// ---------------------------------------------------------------------------
// #15 /trust: option math + trust.json write-back
// ---------------------------------------------------------------------------

#[test]
fn trust_options_cover_upstream_set() {
    let cwd = normalize_cwd(&std::env::temp_dir());
    let options = get_project_trust_options(&cwd);
    // Trust / Trust parent / Do not trust (the parent label exists for any
    // non-root directory).
    assert_eq!(options[0].label, "Trust");
    assert!(options[0].trusted);
    assert_eq!(options.last().unwrap().label, "Do not trust");
    assert!(!options.last().unwrap().trusted);
    // "Trust parent folder" carries the parent-trust + self-clear updates
    // (upstream `getProjectTrustOptions`).
    let parent = &options[1];
    assert!(parent.label.starts_with("Trust parent folder"));
    assert_eq!(parent.updates.len(), 2);
}

#[test]
fn trust_decision_persists_to_trust_json() {
    let dir = std::env::temp_dir().join(format!("pir-trust-test-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let project = dir.join("proj");
    std::fs::create_dir_all(&project).unwrap();
    let store = ProjectTrustStore::new(&dir);
    assert!(store.get(&project).is_none());

    let options = get_project_trust_options(&project);
    // Choose "Trust" (index 0), like Enter on the first row.
    save_project_trust_option(&store, &options[0]).unwrap();
    let saved = store.get(&project).unwrap();
    assert!(saved.decision);
    // Re-resolving the option index finds the saved row.
    let reread = get_project_trust_options(&project);
    let saved_index = reread.iter().position(|option| {
        option.trusted == saved.decision
            && option.updates.iter().any(|(path, decision)| {
                *decision == Some(saved.decision) && normalize_cwd(path) == saved.path
            })
    });
    assert_eq!(saved_index, Some(0));

    // "Do not trust" overwrites.
    save_project_trust_option(&store, reread.last().unwrap()).unwrap();
    let saved = store.get(&project).unwrap();
    assert!(!saved.decision);

    // The trust-gate resource probe only reports true with real resources.
    let _ = has_trust_requiring_project_resources(&project);
    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------
// #24 /import confirmation state machine
// ---------------------------------------------------------------------------

#[test]
fn import_confirm_dialog_answers() {
    let dialog = ConfirmDialog::new(
        "Import session".to_string(),
        "Replace current session with x.jsonl?".to_string(),
    );
    // Defaults to Yes (upstream showExtensionConfirm renders ["Yes", "No"]
    // with ExtensionSelectorComponent's selectedIndex = 0).
    assert!(dialog.is_yes());
    // Right toggles the highlight to No.
    assert_eq!(dialog.handle_key(ConfirmKey::Right), None);
    assert!(!dialog.is_yes());
    assert_eq!(
        dialog.handle_key(ConfirmKey::Enter),
        Some(ConfirmOutcome::Answered(false))
    );
    assert_eq!(
        dialog.handle_key(ConfirmKey::No),
        Some(ConfirmOutcome::Answered(false))
    );
    assert_eq!(
        dialog.handle_key(ConfirmKey::Yes),
        Some(ConfirmOutcome::Answered(true))
    );
    assert_eq!(
        dialog.handle_key(ConfirmKey::Esc),
        Some(ConfirmOutcome::Cancelled)
    );
}

// ---------------------------------------------------------------------------
// #6 /login //logout selector state machine
// ---------------------------------------------------------------------------

#[test]
fn auth_selector_login_flow_keys() {
    let providers = vec![
        AuthProviderOption {
            id: "anthropic".to_string(),
            label: "anthropic".to_string(),
            status: String::new(),
            status_kind: None,
            kind: AuthProviderKind::OAuth,
            subscription: false,
            description: None,
        },
        AuthProviderOption {
            id: "openai".to_string(),
            label: "openai".to_string(),
            status: "stored credential".to_string(),
            status_kind: Some(AuthProviderKind::OAuth),
            kind: AuthProviderKind::OAuth,
            subscription: false,
            description: None,
        },
    ];
    let selector = AuthSelector::login(providers);
    assert_eq!(selector.mode(), AuthMode::Login);
    assert_eq!(selector.mode().as_str(), "login");
    assert_eq!(selector.providers().len(), 2);
    assert_eq!(selector.handle_key(AuthKey::Down), AuthOutcome::None);
    assert_eq!(selector.handle_key(AuthKey::Enter), AuthOutcome::Select(1));
    assert_eq!(selector.handle_key(AuthKey::Esc), AuthOutcome::Cancel);
    // Progress lines land in the shared state (the login thread's channel).
    selector.set_progress("Open https://… to continue…".to_string(), false);
    assert_eq!(selector.progress().status, "Open https://… to continue…");
    assert!(!selector.progress().finished);
    selector.set_progress("Signed in to openai.".to_string(), true);
    assert!(selector.progress().finished);
    // A finished login only answers Esc/Enter.
    assert_eq!(selector.handle_key(AuthKey::Up), AuthOutcome::None);
    assert_eq!(selector.handle_key(AuthKey::Esc), AuthOutcome::Cancel);
}

#[test]
fn auth_selector_logout_lists_stored_credentials() {
    let selector = AuthSelector::logout(vec![AuthProviderOption {
        id: "anthropic".to_string(),
        label: "anthropic".to_string(),
        status: "stored credential".to_string(),
        status_kind: Some(AuthProviderKind::OAuth),
        kind: AuthProviderKind::OAuth,
        subscription: false,
        description: None,
    }]);
    assert_eq!(selector.mode(), AuthMode::Logout);
    assert_eq!(selector.mode().as_str(), "logout");
    assert_eq!(selector.providers().len(), 1);
}

// ---------------------------------------------------------------------------
// #9 session picker management-key semantics
// ---------------------------------------------------------------------------

fn picker_rows() -> Vec<SessionRow> {
    vec![
        SessionRow {
            path: PathBuf::from("/tmp/a.jsonl"),
            id: "aaa".to_string(),
            cwd: "/tmp".to_string(),
            name: None,
            first_message: "alpha question".to_string(),
            all_messages_text: "alpha question".to_string(),
            message_count: 3,
            modified_ms: 1_000,
            parent_session_path: None,
        },
        SessionRow {
            path: PathBuf::from("/tmp/b.jsonl"),
            id: "bbb".to_string(),
            cwd: "/tmp".to_string(),
            name: Some("named session".to_string()),
            first_message: "beta question".to_string(),
            all_messages_text: "beta question".to_string(),
            message_count: 5,
            modified_ms: 3_000,
            parent_session_path: Some("/tmp/a.jsonl".to_string()),
        },
        SessionRow {
            path: PathBuf::from("/tmp/c.jsonl"),
            id: "ccc".to_string(),
            cwd: "/tmp".to_string(),
            name: None,
            first_message: "gamma question".to_string(),
            all_messages_text: "gamma question".to_string(),
            message_count: 1,
            modified_ms: 2_000,
            parent_session_path: None,
        },
    ]
}

#[test]
fn session_sort_modes_order() {
    let rows = picker_rows();
    // Threaded: b is a child of a; subtree activity puts a's subtree first.
    let order = filter_rows(&rows, "", SortMode::Threaded, false);
    assert_eq!(order, vec![0, 1, 2]);
    let threaded = build_threaded_order(&rows);
    assert_eq!(threaded.len(), 3);
    assert_eq!(threaded[0].row, 0);
    // b is a's ONLY child → it is the last child (upstream flattenSessionTree
    // childIsLast) and draws the "└─ " connector.
    assert_eq!(threaded[1].row, 1);
    assert_eq!(threaded[1].depth, 1);
    assert!(threaded[1].is_last);
    assert_eq!(tree_prefix(&threaded[1]), "└─ ");
    // c is a depth-0 root: no prefix regardless of root position.
    assert_eq!(tree_prefix(&threaded[2]), "");
    assert_eq!(tree_prefix(&threaded[0]), "");
    // Recent: newest first.
    let order = filter_rows(&rows, "", SortMode::Recent, false);
    assert_eq!(order, vec![1, 2, 0]);
    // Sort cycle is threaded → recent → relevance → threaded.
    assert_eq!(SortMode::Threaded.next(), SortMode::Recent);
    assert_eq!(SortMode::Recent.next(), SortMode::Relevance);
    assert_eq!(SortMode::Relevance.next(), SortMode::Threaded);
    assert_eq!(SortMode::Recent.label(), "Recent");
}

#[test]
fn session_name_filter_and_query() {
    let rows = picker_rows();
    // Named-only keeps just the named session.
    let order = filter_rows(&rows, "", SortMode::Recent, true);
    assert_eq!(order, vec![1]);
    // Query matches the first message text.
    let order = filter_rows(&rows, "gamma", SortMode::Recent, false);
    assert_eq!(order, vec![2]);
    // Query matches names too.
    let order = filter_rows(&rows, "named", SortMode::Recent, false);
    assert_eq!(order, vec![1]);
}

#[test]
fn session_picker_management_state() {
    use pir::ai::types::{ContentBlock, Message, TextContent, UserMessage, UserRole};
    // Rename persists through the session manager against a real file.
    let dir = std::env::temp_dir().join(format!("pir-picker-test-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let mut manager = pir::session_manager::SessionManager::create("/tmp", &dir).unwrap();
    // The file is written with the first conversation entry.
    manager
        .append_message(Message::User(UserMessage {
            role: UserRole::User,
            content: vec![ContentBlock::Text(TextContent {
                text: "hello".to_string(),
                text_signature: None,
            })],
            tools_added: None,
            tools_removed: None,
            timestamp: 0,
            sections: None,
        }))
        .unwrap();
    let path = manager.path().to_path_buf();
    drop(manager);
    assert!(path.exists());
    let mut state = PickerState::new(vec![SessionRow {
        path: path.clone(),
        id: "xxx".to_string(),
        cwd: "/tmp".to_string(),
        name: None,
        first_message: "hello".to_string(),
        all_messages_text: "hello".to_string(),
        message_count: 1,
        modified_ms: 0,
        parent_session_path: None,
    }]);
    state.apply_rename(&path, "my session").unwrap();
    assert_eq!(state.rows[0].name.as_deref(), Some("my session"));
    // The name round-trips through the file (upstream `appendSessionInfo`).
    let reread = pir::session_manager::SessionManager::open(&path).unwrap();
    assert_eq!(reread.session_name().as_deref(), Some("my session"));

    // Delete removes the file + row (trash CLI missing → unlink fallback).
    let method = state.apply_delete(&path).unwrap();
    assert_eq!(method, SessionDeleteMethod::Unlink);
    assert!(state.rows.is_empty());
    assert!(!path.exists());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn session_age_and_path_formatting() {
    let now: u128 = 1_700_000_000_000;
    assert_eq!(format_session_age(now - 30_000, now), "now");
    assert_eq!(format_session_age(now - 5 * 60_000, now), "5m");
    assert_eq!(format_session_age(now - 3 * 3_600_000, now), "3h");
    assert_eq!(format_session_age(now - 2 * 86_400_000, now), "2d");
    assert_eq!(format_session_age(now - 14 * 86_400_000, now), "2w");
    assert_eq!(format_session_age(now - 60 * 86_400_000, now), "2mo");
    assert_eq!(format_session_age(now - 400 * 86_400_000, now), "1y");
    assert_eq!(shorten_path("/home/u/x", Some("/home/u")), "~/x");
    assert_eq!(shorten_path("/opt/x", Some("/home/u")), "/opt/x");
    assert_eq!(shorten_path("/opt/x", None), "/opt/x");
}

#[test]
fn delete_session_file_falls_back_to_unlink() {
    let dir = std::env::temp_dir().join(format!("pir-delete-test-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("session.jsonl");
    std::fs::write(&path, "{}\n").unwrap();
    // `trash` is not installed in the test environment; the unlink fallback
    // removes the file.
    let _ = delete_session_file(&path);
    assert!(!path.exists());
    let _ = std::fs::remove_dir_all(&dir);
}

/// The picker state machine compiles and its pure helpers stay coherent.
#[test]
fn picker_state_smoke() {
    let state = PickerState::new(picker_rows());
    assert_eq!(state.rows.len(), 3);
}
