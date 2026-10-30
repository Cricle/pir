//! Regression tests for the autocomplete popup accept path.
//!
//! Reported bug 1: after selecting a slash-command completion with Enter the
//! popup stayed on screen. `accept_autocomplete` applied the item and then
//! re-ran the provider (`update_autocomplete`); slash commands complete to
//! `/name ` (with a trailing space), so the re-query immediately produced the
//! command's argument candidates and re-opened the popup. Upstream cancels
//! autocomplete on accept and does not re-query (`editor.ts:771-810`).
//!
//! Reported bug 2: Tab with the popup open inserted a literal tab instead of
//! accepting the highlighted item. `handle_key`'s priority was correct
//! (`tui.input.tab` → `AutocompleteAccept`, checked before Submit), but the
//! paste-burst coalescer (`stdin_buffer::coalesce_paste_burst`) classified
//! Tab as an internal line break and rewrote any Tab key run — including a
//! lone Tab — into `Event::Paste("\t")`, which never reaches `handle_key`.
//!
//! `handle_key` is only reachable through the full `App`/terminal harness, so
//! the accept step is pinned at the shared seam it calls:
//! [`accept_autocomplete_selection`] takes the same `Arc<Mutex<Option<..>>>`
//! slot, provider and editor as the TUI's key handler.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::Mutex;

use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};

use pir::tui::autocomplete::{
    model_argument_items, AutocompletePopup, CombinedAutocompleteProvider,
};
use pir::tui::editor::Editor;
use pir::tui::stdin_buffer::coalesce_paste_burst;
use pir::tui::terminal::accept_autocomplete_selection;

fn temp_base(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "pir-autocomplete-accept-{tag}-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn popup_for(provider: &CombinedAutocompleteProvider, line: &str) -> AutocompletePopup {
    let suggestions = provider
        .suggestions(&[line.to_string()], 0, line.chars().count(), false)
        .expect("the typed line must offer suggestions");
    AutocompletePopup::new(suggestions)
}

/// Enter on a slash-command item (`/mo` → `/model`) must close the popup and
/// keep the completed command line, even though the completed text itself is
/// a context that offers follow-up (argument) suggestions.
#[test]
fn accepting_command_completion_closes_popup_and_keeps_text() {
    let dir = temp_base("command");
    let provider = CombinedAutocompleteProvider::with_builtin_commands(dir.clone());
    // The runtime installs `/model` candidates from the model registry; seed
    // one so the post-accept line has follow-up suggestions (the reopen
    // source the bug depended on).
    provider.set_argument_candidates(
        "model",
        model_argument_items(&[("anthropic".to_string(), "claude-sonnet-4".to_string())]),
    );
    let editor = Editor::new();
    editor.set_text("/mo");

    let mut popup = popup_for(&provider, "/mo");
    let model = popup
        .suggestions
        .items
        .iter()
        .position(|item| item.value == "model")
        .expect("`/mo` must offer the `/model` command");
    popup.selected = model;
    let slot = Mutex::new(Some(popup));

    let accepted = accept_autocomplete_selection(&slot, &provider, &editor);

    assert_eq!(
        accepted.as_deref(),
        Some("/mo"),
        "the typed prefix drives the Enter slash-command fall-through"
    );
    assert!(
        slot.lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_none(),
        "accepting a command completion must close the popup"
    );
    let state = editor.state_snapshot();
    assert_eq!(state.lines, vec!["/model ".to_string()]);
    assert_eq!(state.cursor_col, "/model ".chars().count());
    // Guard: the accepted text still triggers suggestions, so a re-query on
    // accept (the old behavior) would leave the popup visible.
    assert!(
        provider
            .suggestions(&state.lines, state.cursor_line, state.cursor_col, false)
            .is_some(),
        "the completed command must offer argument suggestions"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Enter on an argument item (`/thinking me` → `/thinking medium`) closes the
/// popup and preserves the inserted value.
#[test]
fn accepting_argument_completion_closes_popup_and_keeps_value() {
    let dir = temp_base("argument");
    let provider = CombinedAutocompleteProvider::with_builtin_commands(dir.clone());
    let editor = Editor::new();
    editor.set_text("/thinking me");

    let mut popup = popup_for(&provider, "/thinking me");
    let medium = popup
        .suggestions
        .items
        .iter()
        .position(|item| item.value == "medium")
        .expect("`/thinking me` must offer `medium`");
    popup.selected = medium;
    let slot = Mutex::new(Some(popup));

    let accepted = accept_autocomplete_selection(&slot, &provider, &editor);

    assert_eq!(accepted.as_deref(), Some("me"));
    assert!(
        slot.lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_none(),
        "accepting an argument completion must close the popup"
    );
    let state = editor.state_snapshot();
    assert_eq!(state.lines, vec!["/thinking medium".to_string()]);
    assert_eq!(state.cursor_col, "/thinking medium".chars().count());
    let _ = std::fs::remove_dir_all(&dir);
}

/// Accepting with no popup open is a no-op (the slot stays empty and the
/// editor is untouched).
#[test]
fn accept_without_popup_is_a_no_op() {
    let dir = temp_base("none");
    let provider = CombinedAutocompleteProvider::with_builtin_commands(dir.clone());
    let editor = Editor::new();
    editor.set_text("hello");
    let slot: Mutex<Option<AutocompletePopup>> = Mutex::new(None);

    assert_eq!(
        accept_autocomplete_selection(&slot, &provider, &editor),
        None
    );
    assert!(slot
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .is_none());
    assert_eq!(editor.get_text(), "hello");
    let _ = std::fs::remove_dir_all(&dir);
}

fn tab_key() -> Event {
    Event::Key(KeyEvent {
        code: KeyCode::Tab,
        modifiers: KeyModifiers::NONE,
        kind: KeyEventKind::Press,
        state: KeyEventState::NONE,
    })
}

fn char_key(character: char) -> Event {
    Event::Key(KeyEvent {
        code: KeyCode::Char(character),
        modifiers: KeyModifiers::NONE,
        kind: KeyEventKind::Press,
        state: KeyEventState::NONE,
    })
}

/// A single Tab press owns the editor action: it must reach `handle_key`
/// (`tui.input.tab` → autocomplete accept / Tab completion), never be
/// rewritten into `Event::Paste("\t")` by the paste-burst coalescer. The old
/// coalescer marked Tab as an internal line break, which made a run of ANY
/// length — including one lone Tab — coalesce into a literal-tab paste, so
/// the popup never saw the accept key.
#[test]
fn lone_tab_key_survives_paste_coalescing() {
    let mut queue: VecDeque<Event> = vec![tab_key()].into();
    assert!(
        !coalesce_paste_burst(&mut queue),
        "a lone Tab is a key press, not a paste"
    );
    assert_eq!(queue.len(), 1);
    assert!(
        matches!(&queue[0], Event::Key(key) if key.code == KeyCode::Tab),
        "the Tab must stay a Key event: {queue:?}"
    );
}

/// The realistic popup path: the typed prefix and the Tab land in one input
/// drain. The coalescer must leave both as keys so the popup opens on the
/// prefix and then accepts the highlighted item on Tab.
#[test]
fn typed_prefix_then_tab_stays_key_events() {
    let mut queue: VecDeque<Event> =
        vec![char_key('/'), char_key('m'), char_key('o'), tab_key()].into();
    assert!(
        !coalesce_paste_burst(&mut queue),
        "prefix + Tab must not become a paste"
    );
    assert_eq!(queue.len(), 4);
    assert_eq!(queue.back(), Some(&tab_key()));
}

/// A real multi-line paste keeps its Tab payload: the line break is what
/// marks the run as paste content, not the Tab itself.
#[test]
fn multiline_paste_keeps_its_tab_payload() {
    let mut queue: VecDeque<Event> = vec![
        char_key('a'),
        tab_key(),
        Event::Key(KeyEvent {
            code: KeyCode::Enter,
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        }),
    ]
    .into();
    assert!(coalesce_paste_burst(&mut queue));
    assert_eq!(
        queue.into_iter().collect::<Vec<_>>(),
        vec![Event::Paste("a\t\n".to_string())]
    );
}
