//! R lane: alt-screen transcript search granularity (§7 #23).
//!
//! - non-ASCII case folding matches upstream's `giu` regex canonicalization
//!   (per-code-point uppercase→lowercase, no full `ß`↔`SS` folding, the
//!   `ch > 0x7F && upper < 0x80` look-alike guard);
//! - double/triple click select word/line granularity and a drag extends by
//!   the same unit (upstream `getClickCount` / `getWordSelection` /
//!   `getLineSelection`, 500 ms window). Crossterm reports no click count, so
//!   the router reconstructs it from successive presses on the same word —
//!   the same rule upstream applies to raw SGR presses.

use std::cell::RefCell;
use std::rc::Rc;

use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use pir::tui::altscreen_search::{
    find_search_matches, handle_mouse_event_route, line_selection_columns, word_selection_columns,
    AltMouseRoute, AltScreenConfig, AltScreenState, SelectionGranularity,
};
use pir::tui::core::{Terminal, Text, TuiCore};
use pir::tui::wheel_scroll::{WheelScrollAccelerator, WheelScrollLines};

/// The match columns of `query` on one plain line.
fn search_columns(line: &str, query: &str) -> Vec<(usize, usize)> {
    find_search_matches(&[line.to_string()], query)
        .into_iter()
        .flat_map(|matched| {
            matched
                .segments
                .into_iter()
                .map(|segment| (segment.start_col, segment.end_col))
        })
        .collect()
}

#[test]
fn search_folds_non_ascii_case_like_the_iu_regex() {
    // Latin-1 / Latin Extended: Ä/ä, Ü/ü fold both ways.
    assert_eq!(search_columns("Ärger über STRASSE", "ärger"), vec![(0, 5)]);
    assert_eq!(search_columns("Ärger über STRASSE", "ÜBER"), vec![(6, 10)]);
    assert_eq!(
        search_columns("\x1b[31mÄRGER\x1b[0m", "ärger"),
        vec![(0, 5)],
        "ANSI decoration does not disturb the folded match columns"
    );
    // Greek final sigma (ς) canonicalizes to σ, so a σ query matches it.
    assert_eq!(search_columns("Σίσυφος", "σίσυφοσ"), vec![(0, 7)]);
    // Full foldings are NOT matched by a `giu` regex: Straße vs STRASSE.
    assert!(search_columns("Straße", "STRASSE").is_empty());
    // The ES look-alike guard: LONG S (U+017F) uppercases to ASCII S, so the
    // canonicalization leaves it alone and it does not match `s`.
    assert!(search_columns("s", "\u{017F}").is_empty());
    // A multi-code-point lowercase keeps the original char: İ ≠ i.
    assert!(search_columns("İstanbul", "istanbul").is_empty());
}

#[test]
fn word_selection_matches_upstream_segments() {
    assert_eq!(word_selection_columns("hello world", 1), Some((0, 5)));
    assert_eq!(word_selection_columns("hello world", 7), Some((6, 11)));
    // A path joins across every upstream terminal joiner.
    assert_eq!(word_selection_columns("src/main.rs", 2), Some((0, 11)));
    assert_eq!(word_selection_columns("foo-bar_baz", 4), Some((0, 11)));
    // A punctuation run without adjacent words is its own segment.
    assert_eq!(word_selection_columns("foo - bar", 4), Some((4, 5)));
    // Column past the line has no segment.
    assert_eq!(word_selection_columns("abc", 9), None);
    // Line granularity covers the line's visible width.
    assert_eq!(line_selection_columns("abc"), (0, 3));
    assert_eq!(line_selection_columns("中文"), (0, 4));
}

/// The rendered-frame fixture the mouse router needs (a `TuiCore` with one
/// transcript line, no dock).
struct Fixture {
    core: TuiCore,
    state: AltScreenState,
    config: AltScreenConfig,
    wheel: WheelScrollAccelerator,
    copied: Rc<RefCell<Vec<String>>>,
}

impl Fixture {
    fn new(text: &str) -> Self {
        let terminal = Terminal::new();
        terminal.set_size(20, 4);
        let mut core = TuiCore::new(terminal);
        core.push_content(Box::new(Text::with_padding(text, 0, 0)));
        core.render();
        Self {
            core,
            state: AltScreenState::default(),
            config: AltScreenConfig {
                scrollbar: pir::tui::altscreen_search::ScrollbarMode::Auto,
                copy_on_select: true,
            },
            wheel: WheelScrollAccelerator::with_accelerate(WheelScrollLines::Lines(1), true),
            copied: Rc::new(RefCell::new(Vec::new())),
        }
    }

    fn route(&mut self, kind: MouseEventKind, column: u16, row: u16) -> AltMouseRoute {
        let copied = Rc::clone(&self.copied);
        let copy = move |text: &str| {
            copied.borrow_mut().push(text.to_string());
            true
        };
        handle_mouse_event_route(
            MouseEvent {
                kind,
                column,
                row,
                modifiers: KeyModifiers::NONE,
            },
            &mut self.core,
            &mut self.state,
            &self.config,
            &mut self.wheel,
            0,
            &copy,
        )
    }

    fn click(&mut self, column: u16, row: u16) {
        self.route(MouseEventKind::Down(MouseButton::Left), column, row);
        self.route(MouseEventKind::Up(MouseButton::Left), column, row);
    }

    fn span(&self) -> Option<((usize, usize), (usize, usize))> {
        self.state.selection.map(|span| (span.start, span.end))
    }
}

#[test]
fn double_click_selects_the_word_triple_click_the_line() {
    let mut fx = Fixture::new("hello world");
    // First click: character granularity, no selection, no copy.
    fx.click(1, 0);
    assert_eq!(
        fx.state.selection_granularity(),
        SelectionGranularity::Character
    );
    assert!(fx.span().is_none());
    assert!(fx.copied.borrow().is_empty());

    // Second click on the same word: the word is selected and copied.
    fx.click(1, 0);
    assert_eq!(fx.state.selection_granularity(), SelectionGranularity::Word);
    assert_eq!(fx.span(), Some(((0, 0), (0, 5))));
    assert_eq!(fx.copied.borrow().as_slice(), ["hello"]);

    // Third click: the whole line.
    fx.click(1, 0);
    assert_eq!(fx.state.selection_granularity(), SelectionGranularity::Line);
    assert_eq!(
        fx.copied.borrow().last().map(String::as_str),
        Some("hello world")
    );

    // Fourth click cycles back to character granularity and clears.
    fx.click(1, 0);
    assert_eq!(
        fx.state.selection_granularity(),
        SelectionGranularity::Character
    );
    assert!(fx.span().is_none());
}

#[test]
fn double_click_on_another_word_selects_that_word() {
    let mut fx = Fixture::new("hello world");
    fx.click(8, 0);
    fx.click(8, 0);
    assert_eq!(fx.span(), Some(((0, 6), (0, 11))));
    assert_eq!(fx.copied.borrow().as_slice(), ["world"]);
}

#[test]
fn double_click_then_drag_extends_by_whole_words() {
    let mut fx = Fixture::new("hello world");
    fx.click(1, 0);
    // Second press starts the word selection; dragging to the next word
    // extends the INITIAL word range to the drag target's word.
    assert!(matches!(
        fx.route(MouseEventKind::Down(MouseButton::Left), 1, 0),
        AltMouseRoute::Repaint
    ));
    assert_eq!(fx.span(), Some(((0, 0), (0, 5))));
    assert!(matches!(
        fx.route(MouseEventKind::Drag(MouseButton::Left), 8, 0),
        AltMouseRoute::Repaint
    ));
    assert_eq!(
        fx.span(),
        Some(((0, 0), (0, 11))),
        "the drag snaps to whole words from the initial range"
    );
    assert!(matches!(
        fx.route(MouseEventKind::Up(MouseButton::Left), 8, 0),
        AltMouseRoute::Repaint
    ));
    assert_eq!(fx.copied.borrow().as_slice(), ["hello world"]);
}

#[test]
fn a_stale_word_click_does_not_chain_after_a_far_away_click() {
    let mut fx = Fixture::new("hello world");
    fx.click(1, 0);
    // A click on another word resets the chain, so returning to the first
    // word starts at character granularity again.
    fx.click(8, 0);
    fx.click(1, 0);
    assert_eq!(
        fx.state.selection_granularity(),
        SelectionGranularity::Character
    );
    assert!(fx.span().is_none());
}

/// §7 #22: the basic component/layout surface is re-exported from `pir::tui`
/// (no rendering change — these are the modules' existing public types), so
/// embedders can build a selector/list/editor without the full session loop.
#[test]
fn reusable_component_and_layout_exports_are_public() {
    use pir::tui::{word_selection_columns, Editor, SelectItem, SelectList, SelectionGranularity};

    let mut list = SelectList::new(vec![SelectItem {
        value: 1u32,
        id: "alpha".to_string(),
        provider: "test".to_string(),
        description: "first".to_string(),
        is_current: true,
        is_default: false,
    }]);
    list.set_query("alpha");
    let editor = Editor::new();
    // The editor implements the reusable Component trait.
    let _component: &dyn pir::tui::Component = &editor;
    assert_eq!(
        word_selection_columns("alpha beta", 0),
        Some((0, 5)),
        "the alt-screen word helper is reachable from the crate root"
    );
    assert_eq!(
        SelectionGranularity::default(),
        SelectionGranularity::Character
    );
}

/// The idle-screen transient contract: the copy flash (and the scrollbar
/// linger) expire on a wall-clock deadline, and the event loop only repaints
/// when `expire_transients` reports that something actually disappeared — no
/// wakeup spins on a transient that is already gone, and an idle screen is not
/// left showing a stale flash forever.
#[test]
fn idle_transients_expire_exactly_once() {
    use std::time::{Duration, Instant};

    let start = Instant::now();
    let mut state = AltScreenState::default();
    assert!(!state.transient_pending(), "a fresh state has no transient");
    assert!(
        !state.expire_transients(start),
        "an idle state must not request a repaint"
    );

    state.flash("Copied!", Duration::from_millis(100));
    assert!(state.transient_pending());
    assert!(
        !state.expire_transients(start + Duration::from_millis(50)),
        "a pending flash before its deadline must not repaint"
    );
    assert_eq!(
        state.flash.as_ref().map(|(message, _, _)| message.as_str()),
        Some("Copied!"),
        "the flash survives until its deadline"
    );
    assert!(
        state.expire_transients(start + Duration::from_millis(101)),
        "a due flash must request exactly one repaint"
    );
    assert!(!state.transient_pending(), "the flash is gone");
    assert!(
        !state.expire_transients(start + Duration::from_secs(5)),
        "an expired transient must not keep repainting"
    );
}
