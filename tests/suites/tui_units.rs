//! Moved inline unit tests for TUI subsystem helpers: slash-command and path
//! autocomplete, newline normalization, the editor buffer, fuzzy matching,
//! keybinding defaults and parsing, and the theme JSON loader.

use std::collections::VecDeque;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crossterm::event::{KeyCode, KeyEvent, KeyEventState, KeyModifiers};

use pir::tui::autocomplete::{
    register_extension_command_provider, scan_directory_bounded, AutocompleteItem,
    AutocompletePopup, AutocompleteSuggestions, CombinedAutocompleteProvider, CommandProvider,
    MAX_SCAN_DEPTH, MAX_SCAN_ENTRIES,
};
use pir::tui::clipboard::{normalize_newlines, run_read, ReaderExitGuard};
use pir::tui::editor::Editor;
use pir::tui::fuzzy::{fuzzy_filter, fuzzy_match};
use pir::tui::keybindings::{parse_key, Keybindings, TuiAction};
use pir::tui::theme::{
    custom_theme_names, load_custom_themes, parse_theme_json, set_theme, with_theme, ColorValue,
    ThemeColor, DARK_PALETTE,
};

// ---------------------------------------------------------------------------
// autocomplete.rs
// ---------------------------------------------------------------------------

fn temp_tree(name: &str) -> PathBuf {
    let base = std::env::temp_dir().join(format!("pir-autocomplete-{name}"));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(base.join("src/nested")).expect("mkdir src/nested");
    std::fs::create_dir_all(base.join("node_modules/pkg")).expect("mkdir node_modules");
    std::fs::create_dir_all(base.join(".git")).expect("mkdir .git");
    std::fs::create_dir_all(base.join("target/debug")).expect("mkdir target");
    std::fs::write(base.join("src/main.rs"), "fn main() {}").expect("write main.rs");
    std::fs::write(base.join("src/nested/deep.rs"), "").expect("write deep.rs");
    std::fs::write(base.join("README.md"), "# t").expect("write README");
    std::fs::write(base.join("node_modules/pkg/index.js"), "").expect("write js");
    base
}

#[test]
fn slash_command_completion_fuzzy_orders() {
    let provider = CombinedAutocompleteProvider::with_builtin_commands(PathBuf::from("."));
    let lines = vec!["/mo".to_string()];
    let suggestions = provider
        .suggestions(&lines, 0, 3, false)
        .expect("slash suggestions");
    assert_eq!(suggestions.prefix, "/mo");
    assert!(
        suggestions
            .items
            .first()
            .is_some_and(|item| item.value == "model"),
        "best match should be `model`, got {:?}",
        suggestions.items
    );
    // Unknown command: no suggestions.
    let lines = vec!["/zzz".to_string()];
    assert!(provider.suggestions(&lines, 0, 4, false).is_none());
    // Mid-line slash does not trigger command completion.
    let lines = vec!["say /mo".to_string()];
    assert!(provider.suggestions(&lines, 0, 7, false).is_none());
}

#[test]
fn extension_command_provider_is_injected_and_merged() {
    let provider = CombinedAutocompleteProvider::with_builtin_commands(PathBuf::from("."));
    let lines = vec!["/".to_string()];
    let before = provider.suggestions(&lines, 0, 1, false).expect("builtins");
    assert!(before.items.iter().any(|item| item.value == "model"));

    struct Extension;
    impl CommandProvider for Extension {
        fn commands(&self) -> Vec<pir::tui::autocomplete::SlashCommand> {
            vec![pir::tui::autocomplete::SlashCommand::new(
                "ext-cmd",
                "From an extension",
            )]
        }
    }
    register_extension_command_provider(Some(Arc::new(Extension)));
    let after = provider.suggestions(&lines, 0, 1, false).expect("merged");
    assert!(after.items.iter().any(|item| item.value == "ext-cmd"));

    register_extension_command_provider(None);
    let restored = provider.suggestions(&lines, 0, 1, false).expect("builtins");
    assert!(!restored.items.iter().any(|item| item.value == "ext-cmd"));
}

#[test]
fn apply_completion_for_slash_commands() {
    let provider = CombinedAutocompleteProvider::with_builtin_commands(PathBuf::from("."));
    let lines = vec!["/mo".to_string()];
    let (new_lines, cursor_line, cursor_col) = provider.apply_completion(
        &lines,
        0,
        3,
        &AutocompleteItem::new("model", "model"),
        "/mo",
    );
    assert_eq!(new_lines, vec!["/model ".to_string()]);
    assert_eq!((cursor_line, cursor_col), (0, 7));
}

#[test]
fn slash_argument_completions_follow_the_command() {
    // `/thinking` has the built-in level vocabulary.
    let provider = CombinedAutocompleteProvider::with_builtin_commands(PathBuf::from("."));
    let lines = vec!["/thinking hi".to_string()];
    let suggestions = provider
        .suggestions(&lines, 0, lines[0].chars().count(), false)
        .expect("thinking arguments");
    assert_eq!(suggestions.prefix, "hi");
    assert_eq!(
        suggestions.items.first().map(|item| item.value.as_str()),
        Some("high")
    );
    // The selected value replaces only the typed argument text.
    let item = suggestions.items[0].clone();
    let (new_lines, _, cursor_col) = provider.apply_completion(
        &lines,
        0,
        lines[0].chars().count(),
        &item,
        &suggestions.prefix,
    );
    assert_eq!(new_lines, vec!["/thinking high".to_string()]);
    assert_eq!(cursor_col, "/thinking high".chars().count());

    // `/sessions` and `/debug` vocabularies.
    let lines = vec!["/sessions ".to_string()];
    let sessions = provider
        .suggestions(&lines, 0, 10, false)
        .expect("sessions");
    assert_eq!(sessions.prefix, "");
    assert_eq!(sessions.items.len(), 4);
    let debug = provider.suggestions(&["/debug en".to_string()], 0, 9, false);
    assert_eq!(
        debug.expect("debug arguments").items[0].value.as_str(),
        "enable"
    );

    // A command without a completer (or an unknown command) stays closed.
    assert!(provider
        .suggestions(&["/clear now".to_string()], 0, 10, false)
        .is_none());
    assert!(provider
        .suggestions(&["/nope now".to_string()], 0, 9, false)
        .is_none());
}

#[test]
fn model_argument_completions_are_provider_prefixed() {
    let provider = CombinedAutocompleteProvider::with_builtin_commands(PathBuf::from("."));
    let pairs = vec![
        ("anthropic".to_string(), "claude-opus-4.5".to_string()),
        ("anthropic".to_string(), "claude-sonnet-4.5".to_string()),
        ("openai".to_string(), "gpt-5.2".to_string()),
    ];
    provider.set_argument_candidates(
        "model",
        pir::tui::autocomplete::model_argument_items(&pairs),
    );
    // "claude-opus" is specific enough that only the opus model matches (a
    // bare "opus" subsequence also reaches `anthropic/claude-sonnet-4.5`).
    let lines = vec!["/model claude-opus".to_string()];
    let suggestions = provider
        .suggestions(&lines, 0, lines[0].chars().count(), false)
        .expect("model arguments");
    assert_eq!(suggestions.prefix, "claude-opus");
    assert_eq!(suggestions.items.len(), 1);
    assert_eq!(suggestions.items[0].value, "anthropic/claude-opus-4.5");
    assert_eq!(suggestions.items[0].label, "claude-opus-4.5");
    assert_eq!(
        suggestions.items[0].description.as_deref(),
        Some("anthropic")
    );

    // An empty candidate list removes the entry.
    provider.set_argument_candidates("model", Vec::new());
    assert!(provider
        .suggestions(&lines, 0, lines[0].chars().count(), false)
        .is_none());
}

#[test]
fn bounded_scan_skips_and_caps() {
    let base = temp_tree("scan");
    let entries = scan_directory_bounded(&base, MAX_SCAN_ENTRIES, MAX_SCAN_DEPTH);
    let paths: Vec<&str> = entries.iter().map(|entry| entry.path.as_str()).collect();
    assert!(paths.contains(&"src/main.rs"), "{paths:?}");
    assert!(paths.contains(&"src/nested"));
    assert!(paths.contains(&"src/nested/deep.rs"));
    assert!(paths.contains(&"README.md"));
    assert!(
        !paths.iter().any(|p| p.contains("node_modules")),
        "{paths:?}"
    );
    assert!(!paths.iter().any(|p| p.contains("target")), "{paths:?}");
    assert!(!paths.iter().any(|p| p.contains(".git")), "{paths:?}");
    // Depth cap: a chain deeper than MAX_SCAN_DEPTH is not reached.
    let mut deep_dir = base.join("src/nested");
    for level in 0..(MAX_SCAN_DEPTH + 3) {
        deep_dir = deep_dir.join(format!("l{level}"));
    }
    std::fs::create_dir_all(&deep_dir).expect("mkdir deep");
    let entries = scan_directory_bounded(&base, MAX_SCAN_ENTRIES, MAX_SCAN_DEPTH);
    let deepest = entries
        .iter()
        .map(|entry| entry.path.as_str())
        .max_by_key(|p| p.len());
    assert!(
        deepest.is_some_and(|p| !p.contains("l7")),
        "depth cap exceeded: {deepest:?}"
    );
    // Entry cap is honored.
    let entries = scan_directory_bounded(&base, 2, MAX_SCAN_DEPTH);
    assert_eq!(entries.len(), 2);
    let _ = std::fs::remove_dir_all(&base);
}

#[test]
fn path_completion_lists_directory_contents() {
    let base = temp_tree("paths");
    let provider = CombinedAutocompleteProvider::with_builtin_commands(base.clone());
    // Typing a path-like token suggests cwd-relative entries.
    let lines = vec!["src/".to_string()];
    let suggestions = provider
        .suggestions(&lines, 0, 4, false)
        .expect("path suggestions");
    let values: Vec<&str> = suggestions
        .items
        .iter()
        .map(|item| item.value.as_str())
        .collect();
    assert!(values.contains(&"src/nested/"), "{values:?}");
    assert!(values.contains(&"src/main.rs"), "{values:?}");
    // Prefix matching on the last segment.
    let lines = vec!["src/ma".to_string()];
    let suggestions = provider
        .suggestions(&lines, 0, 6, false)
        .expect("prefix suggestions");
    assert_eq!(suggestions.items.len(), 1);
    assert_eq!(suggestions.items[0].value, "src/main.rs");
    // Applying a file completion appends the value.
    let (new_lines, _line, col) =
        provider.apply_completion(&lines, 0, 6, &suggestions.items[0], "src/ma");
    assert_eq!(new_lines, vec!["src/main.rs".to_string()]);
    assert_eq!(col, 11);
    let _ = std::fs::remove_dir_all(&base);
}

#[test]
fn at_prefix_triggers_fuzzy_file_search() {
    let base = temp_tree("at");
    let provider = CombinedAutocompleteProvider::with_builtin_commands(base.clone());
    let lines = vec!["see @main".to_string()];
    let suggestions = provider
        .suggestions(&lines, 0, 9, false)
        .expect("at suggestions");
    assert_eq!(suggestions.prefix, "@main");
    assert!(
        suggestions
            .items
            .iter()
            .any(|item| item.value.contains("src/main.rs")),
        "{:?}",
        suggestions.items
    );
    // node_modules is never suggested.
    assert!(!suggestions
        .items
        .iter()
        .any(|item| item.value.contains("node_modules")));
    let _ = std::fs::remove_dir_all(&base);
}

#[test]
fn popup_render_and_navigation() {
    let suggestions = AutocompleteSuggestions {
        items: vec![
            AutocompleteItem::new("a", "a"),
            AutocompleteItem::new("b", "b"),
            AutocompleteItem::new("c", "c"),
        ],
        prefix: "/".to_string(),
    };
    let mut popup = AutocompletePopup::new(suggestions);
    assert_eq!(
        popup.selected_item().map(|item| item.value.as_str()),
        Some("a")
    );
    popup.move_down();
    assert_eq!(
        popup.selected_item().map(|item| item.value.as_str()),
        Some("b")
    );
    popup.move_up();
    popup.move_up();
    // Wraps to the end.
    assert_eq!(
        popup.selected_item().map(|item| item.value.as_str()),
        Some("c")
    );
    let lines = popup.render_lines(40);
    assert_eq!(lines.len(), 3);
    // The selection cursor marks exactly the selected row.
    assert!(
        lines
            .iter()
            .filter(|line| line.contains('\u{2192}'))
            .count()
            == 1
    );
    assert!(lines[2].contains('\u{2192}'));
}

// ---------------------------------------------------------------------------
// clipboard.rs
// ---------------------------------------------------------------------------

#[test]
fn normalizes_windows_and_old_mac_newlines() {
    assert_eq!(normalize_newlines("a\r\nb\rc\nd"), "a\nb\nc\nd");
    assert_eq!(normalize_newlines("plain"), "plain");
}

#[test]
fn panicking_reader_still_delivers_none() {
    let mut sink: Vec<Option<String>> = Vec::new();
    let result = catch_unwind(AssertUnwindSafe(|| {
        run_read(|| panic!("clipboard read failed"), |text| sink.push(text));
    }));
    assert!(result.is_err(), "the reader body must unwind");
    assert_eq!(
        sink,
        vec![None],
        "the unwinding guard delivers exactly once"
    );
}

#[test]
fn normal_reader_delivers_the_text_once() {
    let mut sink: Vec<Option<String>> = Vec::new();
    run_read(|| Some("copied".to_string()), |text| sink.push(text));
    assert_eq!(sink, vec![Some("copied".to_string())]);
}

#[test]
fn exit_guard_runs_while_the_reader_unwinds() {
    let exited = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&exited);
    let result = catch_unwind(AssertUnwindSafe(|| {
        let _guard = ReaderExitGuard::new(move || flag.store(true, Ordering::SeqCst));
        panic!("reader body failed");
    }));
    assert!(result.is_err(), "the reader body must unwind");
    assert!(
        exited.load(Ordering::SeqCst),
        "the exit guard must run while the thread unwinds"
    );
}

// ---------------------------------------------------------------------------
// editor.rs
// ---------------------------------------------------------------------------

#[test]
fn paste_inserts_multiline_text_at_cursor() {
    let editor = Editor::new();
    editor.set_text("hello world");
    // cursor between "hello" and " world" (col 5)
    for _ in 0..6 {
        editor.move_cursor_left();
    }
    let inserted = editor.paste_text("X\r\nY\r");
    assert_eq!(inserted, "X\nY\n");
    assert_eq!(editor.get_text(), "helloX\nY\n world");
    let state = editor.state_snapshot();
    // Cursor rests at the end of the pasted text (empty last piece).
    assert_eq!(state.cursor_line, 2);
    assert_eq!(state.cursor_col, 0);
}

#[test]
fn paste_empty_and_plain_text_are_noops_or_simple_inserts() {
    let editor = Editor::new();
    assert_eq!(editor.paste_text(""), "");
    assert_eq!(editor.get_text(), "");
    editor.set_text("ab");
    editor.move_cursor_left();
    editor.paste_text("-");
    assert_eq!(editor.get_text(), "a-b");
    let state = editor.state_snapshot();
    assert_eq!((state.cursor_line, state.cursor_col), (0, 2));
}

#[test]
fn apply_state_replaces_lines_and_clamps_cursor() {
    let editor = Editor::new();
    editor.set_text("old");
    editor.apply_state(vec!["a".to_string(), "bc".to_string()], 9, 99);
    assert_eq!(editor.get_text(), "a\nbc");
    let state = editor.state_snapshot();
    assert_eq!(state.cursor_line, 1);
    assert_eq!(state.cursor_col, 2);
    // Empty replacement is refused.
    editor.apply_state(Vec::new(), 0, 0);
    assert_eq!(editor.get_text(), "a\nbc");
}

// ---------------------------------------------------------------------------
// fuzzy.rs
// ---------------------------------------------------------------------------

#[test]
fn empty_query_matches_everything_with_zero_score() {
    let m = fuzzy_match("", "anything");
    assert!(m.matches);
    assert_eq!(m.score, 0.0);
}

#[test]
fn longer_query_than_text_never_matches() {
    assert!(!fuzzy_match("abcdef", "abc").matches);
}

#[test]
fn subsequence_matches_out_of_order() {
    let m = fuzzy_match("fb", "foobar");
    assert!(m.matches);
    assert!(m.score < 0.0);
}

#[test]
fn missing_characters_do_not_match() {
    assert!(!fuzzy_match("fz", "foobar").matches);
}

#[test]
fn word_boundary_beats_later_match() {
    let boundary = fuzzy_match("f", "foo_bar").score;
    let mid = fuzzy_match("_", "foo_bar").score;
    assert!(boundary < mid);
}

#[test]
fn consecutive_matches_score_better_than_gappy() {
    let consecutive = fuzzy_match("ab", "abc").score;
    let gappy = fuzzy_match("ac", "abc").score;
    assert!(consecutive < gappy);
}

#[test]
fn exact_match_is_best() {
    let exact = fuzzy_match("foo", "foo").score;
    let prefix = fuzzy_match("foo", "foobar").score;
    assert!(exact < prefix);
}

#[test]
fn case_insensitive() {
    assert!(fuzzy_match("FOO", "foobar").matches);
}

#[test]
fn swapped_digits_fallback_matches_with_penalty() {
    // The plain query fails; the swapped letters/digits form matches.
    let swapped = fuzzy_match("ab12", "foobar 12ab");
    assert!(swapped.matches);
    // A direct (unswapped) subsequence match of the same shape scores
    // better — the swap costs +5.
    let direct = fuzzy_match("12ab", "foobar 12ab");
    assert!(direct.matches);
    assert!(swapped.score > direct.score);
    // Unrelated text still does not match.
    assert!(!fuzzy_match("12xy", "foobar ab12").matches);
}

#[test]
fn fuzzy_filter_orders_best_first_and_requires_all_tokens() {
    let items = vec!["model", "thinking", "clear", "help", "mod-t"];
    let filtered = fuzzy_filter(&items, "mo", |item| item);
    assert_eq!(filtered.first().copied(), Some("model"));
    // Both tokens must match.
    let multi = fuzzy_filter(&items, "mo t", |item| item);
    assert!(multi
        .iter()
        .all(|item| item.contains('m') || item.contains('t')));
    assert!(multi.contains(&"mod-t"));
    assert!(!multi.contains(&"model"));
    // Empty query returns everything in original order.
    let all = fuzzy_filter(&items, "  ", |item| item);
    assert_eq!(all, items);
}

// ---------------------------------------------------------------------------
// keybindings.rs
// ---------------------------------------------------------------------------

fn key(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
    KeyEvent {
        code,
        modifiers,
        kind: crossterm::event::KeyEventKind::Press,
        state: KeyEventState::NONE,
    }
}

#[test]
fn keybinding_defaults_match_previous_hardcoded_behavior() {
    let kb = Keybindings::new();
    assert!(kb.matches(TuiAction::Submit, &key(KeyCode::Enter, KeyModifiers::NONE)));
    assert!(!kb.matches(TuiAction::Submit, &key(KeyCode::Enter, KeyModifiers::SHIFT)));
    assert!(kb.matches(
        TuiAction::Newline,
        &key(KeyCode::Enter, KeyModifiers::SHIFT)
    ));
    // Upstream win32 variant (core/keybindings.ts:67-70): follow-up is ctrl+q.
    if pir::config::keybindings::use_windows_keybindings() {
        assert!(kb.matches(
            TuiAction::FollowUp,
            &key(KeyCode::Char('q'), KeyModifiers::CONTROL)
        ));
    } else {
        assert!(kb.matches(TuiAction::FollowUp, &key(KeyCode::Enter, KeyModifiers::ALT)));
    }
    assert!(!kb.matches(TuiAction::Newline, &key(KeyCode::Enter, KeyModifiers::ALT)));
    assert!(kb.matches(TuiAction::Abort, &key(KeyCode::Esc, KeyModifiers::NONE)));
    assert!(kb.matches(
        TuiAction::Clear,
        &key(KeyCode::Char('c'), KeyModifiers::CONTROL)
    ));
    assert!(kb.matches(
        TuiAction::Exit,
        &key(KeyCode::Char('d'), KeyModifiers::CONTROL)
    ));
    assert!(kb.matches(
        TuiAction::ThinkingCycle,
        &key(KeyCode::Tab, KeyModifiers::SHIFT)
    ));
    assert!(kb.matches(
        TuiAction::ModelSelector,
        &key(KeyCode::Char('l'), KeyModifiers::CONTROL)
    ));
    // ctrl+p is upstream `app.model.cycleForward`, dispatched through the
    // `app.*` id table (it no longer opens the model selector).
    assert!(kb.matches_app_id(
        "app.model.cycleForward",
        &key(KeyCode::Char('p'), KeyModifiers::CONTROL)
    ));
    assert!(!kb.matches(
        TuiAction::ModelSelector,
        &key(KeyCode::Char('p'), KeyModifiers::CONTROL)
    ));
    assert!(kb.matches(
        TuiAction::ThinkingSelector,
        &key(
            KeyCode::Char('p'),
            KeyModifiers::CONTROL | KeyModifiers::SHIFT
        )
    ));
    assert!(kb.matches(
        TuiAction::ToolExpand,
        &key(KeyCode::Char('o'), KeyModifiers::CONTROL)
    ));
    assert!(kb.matches(
        TuiAction::HistoryPrevious,
        &key(KeyCode::Up, KeyModifiers::ALT)
    ));
    // Upstream win32 variant (core/keybindings.ts:75-78): paste is alt+v.
    if pir::config::keybindings::use_windows_keybindings() {
        assert!(kb.matches(
            TuiAction::Paste,
            &key(KeyCode::Char('v'), KeyModifiers::ALT)
        ));
    } else {
        assert!(kb.matches(
            TuiAction::Paste,
            &key(KeyCode::Char('v'), KeyModifiers::CONTROL)
        ));
    }
}

#[test]
fn parse_key_supports_function_keys_and_super() {
    // The settings schema accepts f1-f12 and the `super` modifier; the TUI
    // parser must accept the same grammar instead of silently dropping it.
    let f5 = parse_key("f5").expect("f5 parses");
    assert_eq!(f5.code, KeyCode::F(5));
    assert_eq!(
        parse_key("ctrl+f12").expect("ctrl+f12 parses").code,
        KeyCode::F(12)
    );
    let super_k = parse_key("super+k").expect("super+k parses");
    assert!(super_k.super_mod);
    assert_eq!(super_k.code, KeyCode::Char('k'));
    assert!(parse_key("cmd+shift+p").is_some());
    assert!(parse_key("f13").is_none(), "f13 is outside 1..=12");
}

#[test]
fn parse_key_handles_modifiers_and_chars() {
    let combo = parse_key("ctrl+shift+p").expect("parses");
    assert!(combo.ctrl && combo.shift);
    assert_eq!(combo.code, KeyCode::Char('p'));
    assert!(parse_key("shift+enter").is_some());
    assert!(parse_key("esc").is_some());
    assert!(parse_key("v").is_some());
    assert!(parse_key("ctrl+").is_none());
    assert!(parse_key("ctrl+ctrl").is_none());
    assert!(parse_key("").is_none());
}

#[test]
fn settings_override_and_unknown_names_are_ignored() {
    let settings = serde_json::json!({
        "submit": ["ctrl+enter", "enter"],
        "newline": "ctrl+j",
        "nonexistent": "f1",
        "abort": "not a key!!",
        "paste": ["ctrl+b", "shift+insert"]
    });
    let kb = Keybindings::from_settings(Some(&settings)).expect("valid object");
    assert!(kb.matches(
        TuiAction::Submit,
        &key(KeyCode::Enter, KeyModifiers::CONTROL)
    ));
    assert!(kb.matches(TuiAction::Submit, &key(KeyCode::Enter, KeyModifiers::NONE)));
    assert!(kb.matches(
        TuiAction::Newline,
        &key(KeyCode::Char('j'), KeyModifiers::CONTROL)
    ));
    assert!(!kb.matches(
        TuiAction::Newline,
        &key(KeyCode::Enter, KeyModifiers::SHIFT)
    ));
    // Unparseable spec falls back to the default binding.
    assert!(kb.matches(TuiAction::Abort, &key(KeyCode::Esc, KeyModifiers::NONE)));
    assert!(kb.matches(
        TuiAction::Paste,
        &key(KeyCode::Char('b'), KeyModifiers::CONTROL)
    ));
    assert!(kb.matches(TuiAction::Paste, &key(KeyCode::Insert, KeyModifiers::SHIFT)));
}

#[test]
fn scroll_actions_are_not_bindable() {
    // The viewport-scroll wiring was removed; the phantom scrollUp/scrollDown
    // actions are gone, so settings keys that name them are simply ignored:
    // no action ends up bound to their chords.
    assert!(TuiAction::from_name("scrollUp").is_none());
    assert!(TuiAction::from_name("scrollDown").is_none());
    let settings = serde_json::json!({ "scrollUp": "ctrl+up", "scrollDown": "ctrl+down" });
    let kb = Keybindings::from_settings(Some(&settings)).expect("valid object");
    let ctrl_up = key(KeyCode::Up, KeyModifiers::CONTROL);
    let ctrl_down = key(KeyCode::Down, KeyModifiers::CONTROL);
    for action in TuiAction::all() {
        assert!(
            !kb.matches(*action, &ctrl_up) && !kb.matches(*action, &ctrl_down),
            "{} unexpectedly bound to a scroll chord",
            action.name()
        );
    }
}

#[test]
fn missing_settings_means_defaults() {
    let kb = Keybindings::from_settings(None).expect("none");
    assert!(kb.matches(TuiAction::Submit, &key(KeyCode::Enter, KeyModifiers::NONE)));
    let err = Keybindings::from_settings(Some(&serde_json::json!("nope")));
    assert!(err.is_err());
}

// ---------------------------------------------------------------------------
// theme.rs
// ---------------------------------------------------------------------------

fn valid_theme_json() -> String {
    // Start from the built-in dark palette and rename it.
    let mut vars = serde_json::Map::new();
    for (name, hex) in DARK_PALETTE.vars {
        vars.insert((*name).to_string(), serde_json::json!(hex));
    }
    let mut colors = serde_json::Map::new();
    for (name, value) in DARK_PALETTE.colors {
        colors.insert((*name).to_string(), serde_json::json!(value));
    }
    serde_json::json!({ "name": "test-dark", "vars": vars, "colors": colors }).to_string()
}

#[test]
fn theme_parses_valid_theme_with_required_colors() {
    let theme = parse_theme_json(&valid_theme_json()).expect("valid theme");
    assert_eq!(theme.name, "test-dark");
    // Var reference resolution: the upstream dark palette's accent is the
    // `violet` var.
    assert_eq!(
        theme.colors.get("accent"),
        Some(&ColorValue::Var("violet".to_string()))
    );
}

#[test]
fn theme_rejects_missing_required_colors() {
    let mut json: serde_json::Value = serde_json::from_str(&valid_theme_json()).expect("json");
    json["colors"].as_object_mut().unwrap().remove("accent");
    let error = parse_theme_json(&json.to_string()).expect_err("missing accent");
    assert!(error.contains("accent"), "{error}");
    assert!(error.contains("missing required color tokens"));
}

#[test]
fn theme_rejects_bad_name_and_bad_values() {
    let mut json: serde_json::Value = serde_json::from_str(&valid_theme_json()).expect("json");
    json["name"] = serde_json::json!("a/b");
    let error = parse_theme_json(&json.to_string()).expect_err("slash name");
    assert!(error.contains("cannot contain"));

    json["name"] = serde_json::json!("ok");
    json["colors"]["accent"] = serde_json::json!("#12g45z");
    assert!(parse_theme_json(&json.to_string()).is_err());

    json["colors"]["accent"] = serde_json::json!(300);
    assert!(parse_theme_json(&json.to_string()).is_err());

    json["colors"]["accent"] = serde_json::json!(42);
    let theme = parse_theme_json(&json.to_string()).expect("256-color index ok");
    assert_eq!(theme.colors.get("accent"), Some(&ColorValue::Index(42)));

    // Missing colors object / missing name / non-object body.
    assert!(parse_theme_json("{\"name\": \"x\"}").is_err());
    assert!(parse_theme_json("{\"colors\": {}}").is_err());
    assert!(parse_theme_json("[]").is_err());
    assert!(parse_theme_json("not json").is_err());
}

#[test]
fn theme_custom_themes_load_register_and_select() {
    // The process-global theme: serialize with the sibling suites' swaps.
    let _guard = super::tui_system_theme::lock();
    let dir = std::env::temp_dir().join("pir-custom-themes");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("mkdir");

    // One valid theme (256-color index accent) and two invalid ones.
    let mut json: serde_json::Value = serde_json::from_str(&valid_theme_json()).expect("json");
    json["name"] = serde_json::json!("custom-one");
    json["colors"]["accent"] = serde_json::json!(196);
    std::fs::write(dir.join("custom-one.json"), json.to_string()).expect("write valid");
    std::fs::write(dir.join("broken.json"), "{ not json").expect("write broken");
    let mut missing: serde_json::Value = serde_json::from_str(&valid_theme_json()).expect("json");
    missing["name"] = serde_json::json!("missing-accent");
    missing["colors"].as_object_mut().unwrap().remove("accent");
    std::fs::write(dir.join("missing.json"), missing.to_string()).expect("write missing");
    // Non-json files are ignored.
    std::fs::write(dir.join("notes.txt"), "ignore me").expect("write txt");

    let loaded = load_custom_themes(&dir).expect("load");
    assert_eq!(loaded.names, vec!["custom-one".to_string()]);
    assert_eq!(loaded.errors.len(), 2, "{:?}", loaded.errors);
    assert!(custom_theme_names().contains(&"custom-one".to_string()));

    // set_theme selects the registered custom theme.
    set_theme("custom-one");
    let accent = with_theme(|theme| theme.fg(ThemeColor::Accent, "x"));
    assert!(accent.starts_with("\x1b[38;5;196m"), "{accent:?}");
    // Unknown names fall back to the built-in dark palette.
    set_theme("dark");
    let dark_accent = with_theme(|theme| theme.fg(ThemeColor::Accent, "x"));
    // Truecolor or 256-color cube, depending on the environment.
    assert!(
        dark_accent.starts_with("\x1b[38;2;") || dark_accent.starts_with("\x1b[38;5;"),
        "{dark_accent:?}"
    );
    assert_ne!(dark_accent, accent);

    // Missing directory is not an error.
    let loaded = load_custom_themes(&dir.join("does-not-exist")).expect("missing dir");
    assert!(loaded.names.is_empty() && loaded.errors.is_empty());
    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------
// terminal.rs — the `bg: N` status-line marker (`TOOL-BACKGROUND-SCOPE.md` §4)
// ---------------------------------------------------------------------------

/// The four shapes of the shared working/background status slot: the marker
/// rides the working message while a run is active, stands on its own while
/// the session is idle, and vanishes at zero so no frame row is ever added.
#[test]
fn background_status_marker_rides_the_working_slot() {
    use pir::tui::terminal::background_status_message;

    assert_eq!(
        background_status_message(Some("Working".to_string()), 0),
        Some("Working".to_string()),
        "no background calls: the working message is untouched"
    );
    assert_eq!(
        background_status_message(Some("Working".to_string()), 2),
        Some("Working · bg: 2".to_string()),
        "running calls append the marker to the working message"
    );
    assert_eq!(
        background_status_message(None, 1),
        Some("bg: 1".to_string()),
        "idle with a running call: the marker stands on its own"
    );
    assert_eq!(
        background_status_message(None, 0),
        None,
        "nothing running: the slot stays blank"
    );
}

// ---------------------------------------------------------------------------
// terminal.rs — extension-status and dialog-admission helpers
// ---------------------------------------------------------------------------

use pir::tui::dialogs::{DialogBody, DialogOverlay};
use pir::tui::terminal::{
    admit_dialog, apply_extension_status, MAX_EXTENSION_STATUSES, MAX_QUEUED_INTERNAL_DIALOGS,
};

fn input_body() -> DialogBody {
    DialogBody::Input {
        title: "test".to_string(),
        placeholder: String::new(),
        default_value: String::new(),
    }
}

#[test]
fn extension_status_cap_evicts_oldest_key() {
    let mut statuses: Vec<(String, String)> = Vec::new();
    for index in 0..(MAX_EXTENSION_STATUSES + 4) {
        apply_extension_status(
            &mut statuses,
            format!("key-{index}"),
            Some(format!("status-{index}")),
        );
    }
    assert_eq!(statuses.len(), MAX_EXTENSION_STATUSES);
    assert_eq!(statuses[0].0, "key-4", "the oldest four keys are evicted");

    // Updating an existing key replaces its text in place without reordering.
    apply_extension_status(
        &mut statuses,
        "key-10".to_string(),
        Some("updated".to_string()),
    );
    assert_eq!(statuses.len(), MAX_EXTENSION_STATUSES);
    let updated = statuses
        .iter()
        .find(|(key, _)| key == "key-10")
        .map(|(_, text)| text.as_str());
    assert_eq!(updated, Some("updated"));

    // `None` clears the key.
    apply_extension_status(&mut statuses, "key-10".to_string(), None);
    assert!(!statuses.iter().any(|(key, _)| key == "key-10"));
}

#[test]
fn internal_dialog_queue_is_capped() {
    // One dialog occupies the visible slot so every admission below queues.
    let slot = Mutex::new(Some(DialogOverlay::internal(0, input_body())));
    let queue = Mutex::new(VecDeque::new());

    for id in 0..MAX_QUEUED_INTERNAL_DIALOGS {
        let outcome = admit_dialog(
            &slot,
            &queue,
            DialogOverlay::internal(id as u64 + 1, input_body()),
        );
        assert!(
            matches!(outcome, Ok(false)),
            "internal dialog {id} should queue"
        );
    }
    assert_eq!(
        queue.lock().unwrap().len(),
        MAX_QUEUED_INTERNAL_DIALOGS,
        "the queue is full at the cap"
    );
    let rejected = admit_dialog(&slot, &queue, DialogOverlay::internal(99, input_body()));
    assert!(rejected.is_err(), "one dialog over the cap is rejected");
    assert_eq!(queue.lock().unwrap().len(), MAX_QUEUED_INTERNAL_DIALOGS);
}

#[test]
fn admission_sweeps_expired_queued_dialogs() {
    let slot = Mutex::new(Some(DialogOverlay::internal(0, input_body())));
    let expired = DialogOverlay::from_request(
        1,
        "input",
        &serde_json::json!({"title": "expired", "timeout": 1}),
    );
    let queue = Mutex::new(VecDeque::from([expired]));
    std::thread::sleep(Duration::from_millis(5));

    let outcome = admit_dialog(&slot, &queue, DialogOverlay::internal(2, input_body()));
    assert!(matches!(outcome, Ok(false)), "the newcomer should queue");
    let queue = queue.lock().unwrap();
    assert_eq!(queue.len(), 1, "the expired dialog is swept");
    assert_eq!(queue.front().map(|dialog| dialog.id), Some(2));
}
