//! Alt-screen interaction family parity tests (终审 #3/#7/#11/#17/#26).
//!
//! Pure-function coverage only — terminal-facing behavior (mouse capture,
//! windowed rendering, suspend) is exercised by the event loop and guarded by
//! `cargo check`; the geometry/matching/highlight logic mirrored from upstream
//! `wheel-scroll.ts`, `alt-screen-search.ts`, `tui-alt-screen.ts` and
//! `layout.ts::getScrollbarGeometry` is tested here.

use pir::tui::altscreen_search::{
    find_search_matches, match_key, max_scroll_offset, normalize_query, refresh_search,
    slice_columns, splice_columns, AltScreenState, SearchState, SelectionSpan,
};
use pir::tui::wheel_scroll::{WheelScrollAccelerator, WheelScrollLines};

// ---------------------------------------------------------------------------
// wheel-scroll.ts parity (终审 #3)
// ---------------------------------------------------------------------------

#[test]
fn fixed_wheel_lines_move_that_many_lines() {
    let mut wheel = WheelScrollAccelerator::with_accelerate(WheelScrollLines::Lines(5), true);
    assert_eq!(wheel.next(-1, 0), 5);
    assert_eq!(wheel.next(1, 10), 5);
}

#[test]
fn auto_wheel_moves_one_line_per_isolated_notch() {
    let mut wheel = WheelScrollAccelerator::with_accelerate(WheelScrollLines::Auto, true);
    assert_eq!(wheel.next(1, 0), 1);
    // A pause longer than the gesture gap ends the gesture: still one line.
    assert_eq!(wheel.next(1, 5_000), 1);
}

#[test]
fn auto_wheel_burst_events_do_not_accelerate() {
    // Upstream BURST_GAP_MS = 5: events closer than that belong to one
    // physical notch and move one line each.
    let mut wheel = WheelScrollAccelerator::with_accelerate(WheelScrollLines::Auto, true);
    assert_eq!(wheel.next(1, 0), 1);
    assert_eq!(wheel.next(1, 2), 1);
    assert_eq!(wheel.next(1, 4), 1);
}

#[test]
fn auto_wheel_fast_spin_accelerates_up_to_six_lines() {
    // Notches 20 ms apart move ~5 lines per event (100/20); the carry keeps
    // the fractional part. Upstream example: 20 ms → 5 lines.
    let mut wheel = WheelScrollAccelerator::with_accelerate(WheelScrollLines::Auto, true);
    assert_eq!(wheel.next(1, 0), 1);
    let first = wheel.next(1, 20);
    assert_eq!(first, 5, "100ms/20ms average = 5 lines");
    let second = wheel.next(1, 40);
    assert!(
        (5..=6).contains(&second),
        "sustained 20ms spin stays in 5..6 lines, got {second}"
    );
    for tick in 2..200 {
        let lines = wheel.next(1, 20 + tick * 20);
        assert!(lines <= 6, "auto mode caps at 6 lines, got {lines}");
        assert!(lines >= 1);
    }
}

#[test]
fn auto_wheel_direction_change_resets_the_gesture() {
    let mut wheel = WheelScrollAccelerator::with_accelerate(WheelScrollLines::Auto, true);
    assert_eq!(wheel.next(1, 0), 1);
    assert_eq!(wheel.next(1, 20), 5);
    // Opposite direction inside the gesture window: back to one line.
    assert_eq!(wheel.next(-1, 40), 1);
}

#[test]
fn auto_wheel_without_acceleration_always_moves_one_line() {
    // macOS local terminals receive pre-accelerated deltas.
    let mut wheel = WheelScrollAccelerator::with_accelerate(WheelScrollLines::Auto, false);
    assert_eq!(wheel.next(1, 0), 1);
    assert_eq!(wheel.next(1, 1), 1);
    assert_eq!(wheel.next(1, 2), 1);
}

// ---------------------------------------------------------------------------
// alt-screen-search.ts parity (终审 #7)
// ---------------------------------------------------------------------------

#[test]
fn query_normalization_collapses_whitespace() {
    assert_eq!(normalize_query("  a \t b  "), "a b");
    assert_eq!(normalize_query("   "), "");
}

#[test]
fn search_matches_across_rows_join_spans() {
    // "hello world" split over two rows matches as ONE match with two
    // segments (upstream multi-row segments joined on the corpus separator).
    let lines = vec!["hello".to_string(), "world".to_string()];
    let matches = find_search_matches(&lines, "hello world");
    assert_eq!(matches.len(), 1);
    let segments = &matches[0].segments;
    assert_eq!(segments.len(), 2);
    assert_eq!(
        segments[0],
        pir::tui::altscreen_search::SearchSegment {
            row: 0,
            start_col: 0,
            end_col: 5
        }
    );
    assert_eq!(
        segments[1],
        pir::tui::altscreen_search::SearchSegment {
            row: 1,
            start_col: 0,
            end_col: 5
        }
    );
}

#[test]
fn search_respects_ansi_and_column_positions() {
    let lines = vec!["\x1b[31mfoo\x1b[0m bar foo".to_string()];
    let matches = find_search_matches(&lines, "foo");
    // Both visible "foo" occurrences, columns in visible cells.
    assert_eq!(matches.len(), 2);
    assert_eq!(matches[0].segments[0].start_col, 0);
    assert_eq!(matches[0].segments[0].end_col, 3);
    assert_eq!(matches[1].segments[0].start_col, 8);
    assert_eq!(matches[1].segments[0].end_col, 11);
}

#[test]
fn search_match_key_is_stable_across_identical_frames() {
    let lines = vec!["alpha beta".to_string()];
    let first = find_search_matches(&lines, "beta");
    let second = find_search_matches(&lines, "beta");
    assert_eq!(match_key(&first[0]), match_key(&second[0]));
}

#[test]
fn refresh_search_keeps_the_selected_match_by_key() {
    let frame = vec!["one match here".to_string(), "one match there".to_string()];
    let mut state = AltScreenState::default();
    state.search.active = true;
    state.search.query = "match".to_string();
    state.search.frame_version = u64::MAX; // force refresh
    assert!(refresh_search(&mut state, &frame, 0, 1));
    assert_eq!(state.search.matches.len(), 2);
    // Simulate navigating to the second match, then a new frame version with
    // the same content: the key retains the selection.
    state.search.selected = Some(1);
    let key = match_key(&state.search.matches[1]);
    assert!(refresh_search(&mut state, &frame, 0, 2));
    assert_eq!(
        state.search.selected,
        Some(1),
        "selected match survives by key"
    );
    assert_eq!(match_key(&state.search.matches[1]), key);
}

#[test]
fn refresh_search_skips_unchanged_frames() {
    let frame = vec!["stable".to_string()];
    let mut state = AltScreenState::default();
    state.search.active = true;
    state.search.query = "stable".to_string();
    state.search.frame_version = u64::MAX;
    assert!(refresh_search(&mut state, &frame, 0, 7));
    // Same version → no rebuild.
    assert!(!refresh_search(&mut state, &frame, 0, 7));
    // Inactive search → never.
    state.search.active = false;
    assert!(!refresh_search(&mut state, &frame, 0, 8));
}

#[test]
fn cached_search_matches_equal_the_one_shot_corpus() {
    // ANSI rows, tabs, a multi-row match and non-ASCII text exercise every
    // corpus path the content-keyed cache reuses from row parses.
    let frame = vec![
        "\x1b[31mfoo\x1b[0m\tbar".to_string(),
        "日本語 foo bar".to_string(),
        "  baz  foo ".to_string(),
        String::new(),
        "tail".to_string(),
    ];
    let one_shot = find_search_matches(&frame, "foo bar");
    assert_eq!(one_shot.len(), 2, "one match per row: {one_shot:?}");
    let mut state = AltScreenState::default();
    state.search.active = true;
    state.search.query = "foo bar".to_string();
    assert!(refresh_search(&mut state, &frame, 0, 1));
    assert_eq!(
        state.search.matches, one_shot,
        "cached matches must keep the one-shot matches' spans and order"
    );
}

#[test]
fn refresh_search_reuses_unchanged_content_across_versions() {
    let frame = vec![
        "\x1b[31malpha one\x1b[0m".to_string(),
        "beta two".to_string(),
        "gamma three".to_string(),
    ];
    let mut state = AltScreenState::default();
    state.search.active = true;
    state.search.query = "two".to_string();
    assert!(refresh_search(&mut state, &frame, 0, 1));
    assert_eq!(state.search.matches.len(), 1);
    assert_eq!(state.search.cache.row_rebuilds(), 3);
    assert_eq!(state.search.cache.corpus_builds(), 1);
    assert_eq!(state.search.cache.match_runs(), 1);
    let matches = state.search.matches.clone();

    // `TuiCore::render` advances the frame version on every render; identical
    // content must reuse the corpus AND the match set.
    for version in 2..8 {
        assert!(refresh_search(&mut state, &frame, 0, version));
        assert_eq!(state.search.matches, matches, "match set changed");
    }
    assert_eq!(state.search.cache.row_rebuilds(), 3);
    assert_eq!(state.search.cache.corpus_builds(), 1);
    assert_eq!(state.search.cache.match_runs(), 1);
}

#[test]
fn refresh_search_rebuilds_only_the_changed_row() {
    let mut frame = vec![
        "alpha one".to_string(),
        "beta two".to_string(),
        "gamma three".to_string(),
    ];
    let mut state = AltScreenState::default();
    state.search.active = true;
    state.search.query = "two".to_string();
    assert!(refresh_search(&mut state, &frame, 0, 1));
    let row_rebuilds = state.search.cache.row_rebuilds();
    let corpus_builds = state.search.cache.corpus_builds();
    let match_runs = state.search.cache.match_runs();

    // One row changed (a spinner tick); unchanged rows keep their cached
    // stripped parse.
    frame[2] = "gamma three \u{280b}".to_string();
    assert!(refresh_search(&mut state, &frame, 0, 2));
    assert_eq!(
        state.search.cache.row_rebuilds(),
        row_rebuilds + 1,
        "only row 2 may be re-stripped"
    );
    assert_eq!(state.search.cache.corpus_builds(), corpus_builds + 1);
    assert_eq!(state.search.cache.match_runs(), match_runs + 1);
    assert_eq!(state.search.matches.len(), 1);
    assert_eq!(state.search.matches[0].segments[0].row, 1);
}

#[test]
fn search_cache_clears_on_query_change_and_close() {
    let frame = vec!["alpha".to_string(), "beta two".to_string()];
    let mut state = AltScreenState::default();
    state.search.active = true;
    state.search.query = "alpha".to_string();
    assert!(refresh_search(&mut state, &frame, 0, 1));
    assert_eq!(state.search.cache.cached_rows(), 2);
    let row_rebuilds = state.search.cache.row_rebuilds();

    // A query edit clears the row cache; the next refresh re-strips the frame
    // (query-independent rows are dropped too, by design).
    state.search.query = "two".to_string();
    assert!(refresh_search(&mut state, &frame, 0, 2));
    assert_eq!(state.search.matches.len(), 1);
    assert_eq!(state.search.matches[0].segments[0].row, 1);
    assert_eq!(state.search.cache.row_rebuilds(), row_rebuilds + 2);

    // Closing the overlay drops the cache.
    state.search.close();
    assert_eq!(state.search.cache.cached_rows(), 0);
    assert_eq!(state.search.cache.row_rebuilds(), row_rebuilds + 2);
}

#[test]
fn search_result_text_shows_count_or_no_matches() {
    let mut state = SearchState::default();
    assert_eq!(state.result_text(), "");
    state.query = "x".to_string();
    assert_eq!(state.result_text(), "No matches");
    state.matches = vec![
        pir::tui::altscreen_search::SearchMatch {
            segments: vec![pir::tui::altscreen_search::SearchSegment {
                row: 0,
                start_col: 0,
                end_col: 1,
            }],
        },
        pir::tui::altscreen_search::SearchMatch {
            segments: vec![pir::tui::altscreen_search::SearchSegment {
                row: 1,
                start_col: 0,
                end_col: 1,
            }],
        },
    ];
    assert_eq!(state.result_text(), "/2");
    state.selected = Some(1);
    assert_eq!(state.result_text(), "2/2");
}

// ---------------------------------------------------------------------------
// Column slicing / highlight splicing (upstream applySearchTextHighlight)
// ---------------------------------------------------------------------------

#[test]
fn splice_columns_wraps_and_restores_active_sgr() {
    // Real alt-screen rows are padded to terminal width (upstream
    // highlights operate on padded rows via `sliceByColumn`); the padded
    // trailing space closes the highlight, re-emits the active SGR, then
    // the verbatim `\x1b[0m` follows.
    let line = "\x1b[31mhello world \x1b[0m";
    let painted = splice_columns(line, &[(6, 11, "\x1b[4m", "\x1b[24m")]);
    assert_eq!(
        painted,
        "\x1b[31mhello \x1b[4mworld\x1b[24m\x1b[31m \x1b[0m"
    );
    // Visible width is unchanged.
    assert_eq!(
        pir::tui::visible_width(&painted),
        pir::tui::visible_width(line)
    );
}

#[test]
fn splice_columns_handles_plain_lines() {
    let painted = splice_columns("abc def", &[(0, 3, "\x1b[1;7m", "\x1b[22;27m")]);
    assert_eq!(painted, "\x1b[1;7mabc\x1b[22;27m def");
}

#[test]
fn splice_columns_with_no_ranges_is_identity() {
    let line = "\x1b[2mdim\x1b[22m";
    assert_eq!(splice_columns(line, &[]), line);
}

#[test]
fn slice_columns_cuts_visible_cells() {
    assert_eq!(slice_columns("hello world", 6, 11), "world");
    assert_eq!(slice_columns("hello world", 0, 5), "hello");
    assert_eq!(slice_columns("hello", 3, 3), "");
    // Full-width characters occupy two cells and are included when their
    // starting column is inside the range.
    assert_eq!(slice_columns("中文x", 0, 2), "中");
    assert_eq!(slice_columns("中文x", 2, 5), "文x");
}

// ---------------------------------------------------------------------------
// Selection (upstream getActiveSelectionText, 终审 #7 copy-on-select)
// ---------------------------------------------------------------------------

#[test]
fn selection_text_joins_rows_and_trims() {
    let frame = vec![
        "first line".to_string(),
        "second line".to_string(),
        "third".to_string(),
    ];
    let selection = SelectionSpan {
        start: (0, 6),
        end: (2, 3),
    };
    let text = pir::tui::altscreen_search::selection_text(&frame, &selection)
        .expect("non-empty selection");
    // Middle rows (row ≠ start.row and ≠ end.row) contribute their FULL
    // text (upstream `getSelectionColumns` returns [0, lineWidth] for them).
    assert_eq!(text, "line\nsecond line\nthi");
}

#[test]
fn selection_text_of_empty_span_is_none() {
    let frame = vec!["abc".to_string()];
    let selection = SelectionSpan {
        start: (0, 1),
        end: (0, 1),
    };
    assert!(pir::tui::altscreen_search::selection_text(&frame, &selection).is_none());
}

#[test]
fn selection_text_ignores_ansi() {
    let frame = vec!["\x1b[1mbold tail\x1b[22m".to_string()];
    let selection = SelectionSpan {
        start: (0, 5),
        end: (0, 9),
    };
    assert_eq!(
        pir::tui::altscreen_search::selection_text(&frame, &selection).as_deref(),
        Some("tail")
    );
}

// ---------------------------------------------------------------------------
// Scroll geometry (upstream getScrollbarGeometry / ScrollView clamps)
// ---------------------------------------------------------------------------

#[test]
fn max_scroll_offset_clamps_to_transcript_minus_viewport() {
    // 100 transcript rows, 10-row dock, 40-row terminal → 30-row transcript
    // viewport, 70 scrollable rows.
    assert_eq!(max_scroll_offset(110, 10, 40), 70);
    // 40 transcript rows in a 30-row viewport: the 10-row overflow scrolls.
    assert_eq!(max_scroll_offset(50, 10, 40), 10);
    // Degenerate dock larger than the screen: dock clamps to rows-1 = 9, so
    // the transcript viewport is 1 row and the 10-row overflow scrolls.
    assert_eq!(max_scroll_offset(20, 100, 10), 10);
    // A transcript that fits the viewport never scrolls.
    assert_eq!(max_scroll_offset(40, 10, 40), 0);
}

// ---------------------------------------------------------------------------
// Flash timing (upstream AltScreenFlashContainer durations)
// ---------------------------------------------------------------------------

#[test]
fn flash_expires_after_its_duration() {
    use std::time::{Duration, Instant};
    let mut state = AltScreenState::default();
    state.flash("Copied!", Duration::from_millis(50));
    assert!(state.flash_active(Instant::now()));
    // tick() after expiry drops the message and reports the change.
    std::thread::sleep(Duration::from_millis(60));
    assert!(state.tick(Instant::now()), "expiry is reported to the loop");
    assert!(state.flash.is_none());
    assert!(!state.tick(Instant::now()));
}

// ---------------------------------------------------------------------------
// Viewport scroll direction: `scroll_offset` counts lines back from the
// transcript end (0 = live follow), so upstream's top-anchored
// `ScrollView.scrollBy(-n)` maps to `+n` here. Regression for the key/wheel
// sign mixup surfaced while routing the D18 wheel.
// ---------------------------------------------------------------------------

#[test]
fn alt_screen_scroll_keys_follow_upstream_direction() {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use pir::tui::altscreen_search::{route_alt_screen_key, AltKeyRoute};
    use pir::tui::core::{Terminal, Text, TuiCore};

    let terminal = Terminal::new();
    terminal.set_size(20, 6);
    let mut core = TuiCore::new(terminal);
    let transcript: String = (1..=30)
        .map(|index| format!("line {index}"))
        .collect::<Vec<_>>()
        .join("\n");
    core.push_content(Box::new(Text::new(transcript)));
    core.render();
    let mut state = AltScreenState::default();

    // Live follow (offset 0) is the end of the transcript: PageUp must jump
    // back into history, not stay pinned.
    core.set_scroll_offset(0);
    let page_up = KeyEvent::new(KeyCode::PageUp, KeyModifiers::NONE);
    assert!(matches!(
        route_alt_screen_key(&page_up, &mut core, &mut state, 0),
        AltKeyRoute::Consumed
    ));
    let after_up = core.scroll_offset();
    assert!(after_up > 0, "pageUp shows earlier content");

    let page_down = KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE);
    assert!(matches!(
        route_alt_screen_key(&page_down, &mut core, &mut state, 0),
        AltKeyRoute::Consumed
    ));
    assert!(
        core.scroll_offset() < after_up,
        "pageDown returns toward the live end"
    );

    // ctrl+home = earliest content (upstream `tui.altScreen.top`).
    let ctrl_home = KeyEvent::new(KeyCode::Home, KeyModifiers::CONTROL);
    assert!(matches!(
        route_alt_screen_key(&ctrl_home, &mut core, &mut state, 0),
        AltKeyRoute::Consumed
    ));
    assert_eq!(
        core.scroll_offset(),
        max_scroll_offset(core.frame_buffer_length(), 0, core.rows()),
        "ctrl+home reaches the top of the transcript"
    );

    // ctrl+end = live follow again (upstream `tui.altScreen.bottom`).
    let ctrl_end = KeyEvent::new(KeyCode::End, KeyModifiers::CONTROL);
    assert!(matches!(
        route_alt_screen_key(&ctrl_end, &mut core, &mut state, 0),
        AltKeyRoute::Consumed
    ));
    assert_eq!(core.scroll_offset(), 0, "ctrl+end returns to live follow");
}

#[test]
fn alt_screen_wheel_up_shows_earlier_content() {
    use crossterm::event::{KeyModifiers as Mods, MouseEvent, MouseEventKind};
    use pir::tui::altscreen_search::{handle_mouse_event, AltScreenConfig};
    use pir::tui::core::{Terminal, Text, TuiCore};

    let terminal = Terminal::new();
    terminal.set_size(20, 6);
    let mut core = TuiCore::new(terminal);
    let transcript: String = (1..=30)
        .map(|index| format!("line {index}"))
        .collect::<Vec<_>>()
        .join("\n");
    core.push_content(Box::new(Text::new(transcript)));
    core.render();
    let mut state = AltScreenState::default();
    let config = AltScreenConfig::default();
    let mut wheel = WheelScrollAccelerator::with_accelerate(WheelScrollLines::Lines(2), true);
    let copy = |_text: &str| false;
    let event = |kind| MouseEvent {
        kind,
        column: 3,
        row: 2,
        modifiers: Mods::NONE,
    };

    core.set_scroll_offset(5);
    assert!(handle_mouse_event(
        event(MouseEventKind::ScrollUp),
        &mut core,
        &mut state,
        &config,
        &mut wheel,
        0,
        &copy,
    ));
    assert_eq!(
        core.scroll_offset(),
        7,
        "wheel-up moves away from the live end"
    );
    assert!(handle_mouse_event(
        event(MouseEventKind::ScrollDown),
        &mut core,
        &mut state,
        &config,
        &mut wheel,
        0,
        &copy,
    ));
    assert_eq!(
        core.scroll_offset(),
        5,
        "wheel-down returns toward the live end"
    );
}

// ---------------------------------------------------------------------------
// Semantic prompt navigation (`tui.altScreen.previousPrompt` / `nextPrompt`,
// upstream `tui-alt-screen.ts:506-517 scrollToPrompt` and its test
// "scrolls to the previous/next semantic prompt").
// ---------------------------------------------------------------------------

/// A fullscreen core rendering one user message per entry (each an own
/// `userMessageBg` block — the prompt anchor pir can see) at the given
/// terminal height.
fn prompt_core(messages: &[&str], rows: usize) -> pir::tui::core::TuiCore {
    use pir::tui::app::{ChatContainer, ChatEntry};
    use pir::tui::core::{Terminal, TuiCore};

    let terminal = Terminal::new();
    terminal.set_size(24, rows);
    let mut core = TuiCore::new(terminal);
    let mut chat = ChatContainer::new();
    let entries: Vec<ChatEntry> = messages
        .iter()
        .map(|text| ChatEntry::User((*text).to_string()))
        .collect();
    chat.set_entries(&entries);
    core.push_content(Box::new(chat));
    core.render();
    core
}

/// The transcript row currently painted at the top of the viewport: the
/// follow position (`max`) minus the offset-from-end (0 = live follow).
fn viewport_top_row(core: &pir::tui::core::TuiCore, dock: usize) -> usize {
    let max = max_scroll_offset(core.frame_buffer_length(), dock, core.rows().max(1));
    max - core.scroll_offset().min(max)
}

fn test_key(
    code: crossterm::event::KeyCode,
    modifiers: crossterm::event::KeyModifiers,
) -> crossterm::event::KeyEvent {
    crossterm::event::KeyEvent::new(code, modifiers)
}

fn route(
    key: &crossterm::event::KeyEvent,
    core: &mut pir::tui::core::TuiCore,
    state: &mut AltScreenState,
) -> pir::tui::altscreen_search::AltKeyRoute {
    pir::tui::altscreen_search::route_alt_screen_key(key, core, state, 0)
}

#[test]
fn prompt_rows_anchor_each_user_message_block() {
    use pir::tui::altscreen_search::prompt_start_rows;
    use pir::tui::ansi::strip_ansi;

    let core = prompt_core(&["first", "second", "third", "fourth"], 6);
    let frame = core.last_frame_lines();
    let rows = prompt_start_rows(frame);
    assert_eq!(rows.len(), 4, "one anchor per user message, got {rows:?}");

    // The anchor opens the block (its top padding row); the message text
    // renders on the row right below, exactly like upstream's OSC 133 `A`
    // zone sits on the component's first line.
    for (index, expected) in ["first", "second", "third", "fourth"].iter().enumerate() {
        let row = rows[index];
        let text = strip_ansi(&frame[row + 1]);
        assert!(
            text.contains(expected),
            "anchor {row} opens the \"{expected}\" block, row {} is {text:?}",
            row + 1
        );
    }
    // Blocks never overlap and stay ordered.
    assert!(rows.windows(2).all(|pair| pair[0] < pair[1]), "{rows:?}");
}

#[test]
fn prompt_rows_recognize_osc133_zones_and_ignore_plain_lines() {
    use pir::tui::altscreen_search::prompt_start_rows;

    // Upstream's native marker (a line opening with the OSC 133 `A` zone) is
    // recognized even without the background paint, and a run of block rows
    // yields one anchor at the run start.
    let lines = vec![
        "\x1b]133;A\x07prompt".to_string(),
        "answer".to_string(),
        "\x1b]133;A\x1b\\second".to_string(),
    ];
    assert_eq!(prompt_start_rows(&lines), vec![0, 2]);
    // No marker at all: no anchors, no panic.
    assert!(prompt_start_rows(&["plain".to_string(), "lines".to_string()]).is_empty());
    assert!(prompt_start_rows(&[]).is_empty());
}

#[test]
fn prompt_navigation_follows_upstream_direction_and_clamps() {
    use crossterm::event::{KeyCode, KeyModifiers};
    use pir::tui::altscreen_search::{prompt_start_rows, AltKeyRoute};

    // Four prompts, 6 terminal rows: the anchors are 0/4/8/12 (1-row spacer +
    // 3-row block each) and the follow position is row 9 (15 - 6).
    let mut core = prompt_core(&["first", "second", "third", "fourth"], 6);
    let frame = core.last_frame_lines();
    let anchors = prompt_start_rows(frame);
    assert_eq!(anchors, vec![0, 4, 8, 12], "frame: {frame:#?}");
    let max = max_scroll_offset(core.frame_buffer_length(), 0, core.rows());
    assert_eq!(max, 9, "15 transcript rows in a 6-row viewport");
    let mut state = AltScreenState::default();

    // Live follow starts at the bottom of the transcript.
    core.set_scroll_offset(0);
    assert_eq!(viewport_top_row(&core, 0), max);

    // previousPrompt: land on the last prompt above the viewport top (row 8),
    // not on the one further up (upstream scans row by row).
    let previous = test_key(KeyCode::Up, KeyModifiers::CONTROL);
    assert!(matches!(
        route(&previous, &mut core, &mut state),
        AltKeyRoute::Consumed
    ));
    assert_eq!(viewport_top_row(&core, 0), anchors[2]);
    assert_eq!(core.scroll_offset(), max - anchors[2]);

    // previousPrompt again: the next prompt above (row 4).
    assert!(matches!(
        route(&previous, &mut core, &mut state),
        AltKeyRoute::Consumed
    ));
    assert_eq!(viewport_top_row(&core, 0), anchors[1]);

    // ... then the first prompt: the top of the transcript.
    assert!(matches!(
        route(&previous, &mut core, &mut state),
        AltKeyRoute::Consumed
    ));
    assert_eq!(viewport_top_row(&core, 0), anchors[0]);
    assert_eq!(core.scroll_offset(), max);

    // No prompt above the first: the viewport stays put.
    let parked = core.scroll_offset();
    assert!(matches!(
        route(&previous, &mut core, &mut state),
        AltKeyRoute::Consumed
    ));
    assert_eq!(core.scroll_offset(), parked, "clamped at the first prompt");

    // nextPrompt walks back down: row 4, row 8, then the follow position.
    let next = test_key(KeyCode::Down, KeyModifiers::CONTROL);
    assert!(matches!(
        route(&next, &mut core, &mut state),
        AltKeyRoute::Consumed
    ));
    assert_eq!(viewport_top_row(&core, 0), anchors[1]);
    assert!(matches!(
        route(&next, &mut core, &mut state),
        AltKeyRoute::Consumed
    ));
    assert_eq!(viewport_top_row(&core, 0), anchors[2]);
    assert!(matches!(
        route(&next, &mut core, &mut state),
        AltKeyRoute::Consumed
    ));
    // The next anchor (row 12) sits below the follow position, so scrolling
    // there clamps to the end — upstream `scrollTo` + "following output".
    assert_eq!(
        core.scroll_offset(),
        0,
        "next prompt past the end re-follows"
    );
    assert_eq!(viewport_top_row(&core, 0), max);

    // At the follow position there is no anchor below that is not already
    // visible: the key is consumed but nothing moves.
    assert!(matches!(
        route(&next, &mut core, &mut state),
        AltKeyRoute::Consumed
    ));
    assert_eq!(core.scroll_offset(), 0);
}

#[test]
fn prompt_navigation_consumes_only_the_alt_screen_chords() {
    use crossterm::event::{KeyCode, KeyModifiers};
    use pir::tui::altscreen_search::{route_alt_screen_key, AltKeyRoute};

    let mut core = prompt_core(&["first", "second", "third", "fourth"], 6);
    let mut state = AltScreenState::default();
    let windows = pir::config::keybindings::use_windows_keybindings();

    // The shifted chord is a default except on Windows (upstream's windows
    // branch binds the unshifted chord only).
    let shifted = test_key(KeyCode::Up, KeyModifiers::CONTROL | KeyModifiers::SHIFT);
    let consumed = matches!(
        route_alt_screen_key(&shifted, &mut core, &mut state, 0),
        AltKeyRoute::Consumed
    );
    assert_eq!(
        !windows, consumed,
        "ctrl+shift+up default is platform-specific"
    );

    // Unmodified arrows and alt/shift arrows stay with the editor (cursor
    // movement, history) — the alt-screen family only shadows ctrl chords.
    for modifiers in [KeyModifiers::NONE, KeyModifiers::ALT, KeyModifiers::SHIFT] {
        for code in [KeyCode::Up, KeyCode::Down] {
            let key = test_key(code, modifiers);
            assert!(
                matches!(
                    route_alt_screen_key(&key, &mut core, &mut state, 0),
                    AltKeyRoute::Passthrough
                ),
                "{code:?} with {modifiers:?} must reach the editor"
            );
        }
    }
}

#[test]
fn prompt_navigation_without_prompts_is_a_no_op() {
    use crossterm::event::{KeyCode, KeyModifiers};
    use pir::tui::altscreen_search::AltKeyRoute;
    use pir::tui::core::{Terminal, Text, TuiCore};

    // A transcript with no user message (plain text output): the keys are
    // still the alt-screen family's, but there is nothing to jump to.
    let terminal = Terminal::new();
    terminal.set_size(20, 4);
    let mut core = TuiCore::new(terminal);
    let transcript: String = (1..=20)
        .map(|index| format!("line {index}"))
        .collect::<Vec<_>>()
        .join("\n");
    core.push_content(Box::new(Text::new(transcript)));
    core.render();
    let mut state = AltScreenState::default();
    core.set_scroll_offset(0);

    for code in [KeyCode::Up, KeyCode::Down] {
        let key = test_key(code, KeyModifiers::CONTROL);
        assert!(matches!(
            route(&key, &mut core, &mut state),
            AltKeyRoute::Consumed
        ));
        assert_eq!(core.scroll_offset(), 0, "no prompt rows: the view holds");
    }
}

// ---------------------------------------------------------------------------
// ScrollView semantics: the wheel chain (upstream `tui-alt-screen.ts`
// `routeWheel` + `layout.ts getScrollViewsAt`), the `scrollbar: "always"`
// width reservation (`scroll-view.ts` `getContentWidth`/`render`) and
// `scrollTo(top, { disableFollow })` (`scroll-view.ts` `scrollTo` /
// `updateLayout`).
//
// The values below were probed against upstream pi `7d75a0e3e` with a node
// harness driving the real TS sources (`packages/tui/test/virtual-terminal.ts`
// terminal double over `TuiAltScreen`): the chain sequence reproduces
// upstream's own `tui-alt-screen.test.ts` "chains unused wheel delta to an
// outer scroll view", and the containment / follow transitions come from the
// same harness.
// ---------------------------------------------------------------------------

fn chain_view(
    overscroll: pir::tui::altscreen_search::Overscroll,
    hit: bool,
    scroll_top: usize,
    max_scroll_top: usize,
) -> pir::tui::altscreen_search::WheelScrollView {
    pir::tui::altscreen_search::WheelScrollView {
        overscroll,
        hit,
        scroll_top,
        max_scroll_top,
        follow_end: true,
        following: false,
        follow_suppressed_at_end: false,
    }
}

/// Upstream `tui-alt-screen.test.ts:532` (20x4 terminal, `wheelScrollLines`
/// 3): the inner view (6 content rows in a 2-row viewport, `maxScrollTop` 4)
/// takes three rows per notch and chains what it cannot use to the primary
/// outer view (`maxScrollTop` 3). Probed sequence:
/// `{inner 3, outer 0}`, `{inner 4, outer 2}`, `{inner 4, outer 3}`.
#[test]
fn wheel_chain_hands_the_leftover_to_the_outer_view() {
    use pir::tui::altscreen_search::{route_wheel, Overscroll};

    let mut views = [
        chain_view(Overscroll::Chain, true, 0, 4),
        chain_view(Overscroll::Chain, true, 0, 3),
    ];

    assert_eq!(route_wheel(&mut views, 1, 3), 0, "the inner view absorbs 3");
    assert_eq!((views[0].scroll_top, views[1].scroll_top), (3, 0));
    assert_eq!(
        route_wheel(&mut views, 1, 3),
        0,
        "the inner view moves 1 and the outer absorbs the leftover 2"
    );
    assert_eq!((views[0].scroll_top, views[1].scroll_top), (4, 2));
    // Third notch: the inner view is at its end, so the whole delta chains.
    assert_eq!(
        route_wheel(&mut views, 1, 3),
        2,
        "outer clamps at its max 3"
    );
    assert_eq!((views[0].scroll_top, views[1].scroll_top), (4, 3));
}

/// A `contain` view stops the chain: its ancestor receives nothing (probed
/// against upstream — with `chain` the ancestor ends at rows 2 then 5, with
/// `contain` it stays at 0). Upstream's separate primary fallback still hands
/// the leftover to the primary when the chain never reached it
/// (`!seen.has(primary)`): the probed primary rows are 2 then 5.
#[test]
fn chain_stops_at_a_containing_view_and_the_ancestor_takes_nothing() {
    use pir::tui::altscreen_search::{route_wheel, Overscroll};

    let run = |inner_overscroll: Overscroll| {
        // inner: 6 content rows / 2-row viewport (max 4); ancestor: 7 content
        // rows / 1-row viewport (max 6); primary: 10 content rows / 5-row
        // viewport (max 5) — the probed upstream layout.
        let mut views = [
            chain_view(inner_overscroll, true, 0, 4),
            chain_view(Overscroll::Chain, true, 0, 6),
            chain_view(Overscroll::Chain, true, 0, 5),
        ];
        let mut steps = Vec::new();
        for _ in 0..3 {
            route_wheel(&mut views, 2, 3);
            steps.push((
                views[0].scroll_top,
                views[1].scroll_top,
                views[2].scroll_top,
            ));
        }
        steps
    };

    assert_eq!(
        run(Overscroll::Chain),
        vec![(3, 0, 0), (4, 2, 0), (4, 5, 0)],
        "a chaining inner view feeds its ancestor"
    );
    assert_eq!(
        run(Overscroll::Contain),
        vec![(3, 0, 0), (4, 0, 2), (4, 0, 5)],
        "a contained inner view leaves the ancestor alone while the primary \
         still takes the leftover it never saw"
    );
}

/// Upstream's primary fallback (`!seen.has(primary)`): a wheel outside the
/// transcript's rows — over the dock — still scrolls it (probed: the primary
/// absorbs the whole delta; D18's wheel routing relies on this). A wheel
/// inside the transcript at its end is not applied twice.
#[test]
fn wheel_outside_the_transcript_scrolls_it_through_the_primary_fallback() {
    use pir::tui::altscreen_search::{route_wheel, Overscroll};

    // Pointer over the dock: it missed the only scrollable, so the primary
    // takes the whole delta.
    let mut views = [chain_view(Overscroll::Chain, false, 0, 20)];
    assert_eq!(route_wheel(&mut views, 0, 3), 0);
    assert_eq!(views[0].scroll_top, 3);

    // Pointer over the transcript at its end: `seen.has(primary)`, so the
    // leftover is dropped instead of wrapping or scrolling twice.
    let mut views = [chain_view(Overscroll::Chain, true, 5, 5)];
    route_wheel(&mut views, 0, 3);
    assert_eq!(
        views[0].scroll_top, 5,
        "at the end the wheel has nowhere to go"
    );
}

/// Upstream `ScrollView.scrollBy` re-enables follow when it lands on the end
/// even with no movement (probed: `scrollTo(end, { disableFollow: true })`
/// then `scrollBy(+1)` → `following` true, `scrollTop` unchanged, leftover
/// 1), and a wheel that moves away from the end keeps follow off.
#[test]
fn scroll_by_at_the_pinned_end_re_enables_follow_without_moving() {
    use pir::tui::altscreen_search::{scroll_view_scroll_by, scroll_view_scroll_to, Overscroll};

    let mut view = chain_view(Overscroll::Chain, true, 7, 7);
    view.following = true;
    scroll_view_scroll_to(&mut view, 7, true);
    assert!(!view.following && view.follow_suppressed_at_end);

    assert_eq!(
        scroll_view_scroll_by(&mut view, 1),
        1,
        "no rows are left to move"
    );
    assert_eq!(view.scroll_top, 7);
    assert!(view.following, "a scroll toward the end resumes follow");
    assert!(!view.follow_suppressed_at_end);

    // Re-pin, then scroll away from the end: suppression is cleared and the
    // view stays off the end.
    scroll_view_scroll_to(&mut view, 7, true);
    assert_eq!(scroll_view_scroll_by(&mut view, -1), 0);
    assert_eq!(view.scroll_top, 6);
    assert!(!view.following && !view.follow_suppressed_at_end);
}

/// The `scrollTo(disableFollow)` state machine probed against upstream:
/// pinning at the end, growth keeping the pinned rows (`scrollTop` frozen),
/// `scrollToEnd` resuming follow, and a shrink that lands a scrolled view on
/// the end resuming follow too.
#[test]
fn scroll_to_disable_follow_pins_the_end_until_follow_resumes() {
    use pir::tui::altscreen_search::{
        scroll_view_scroll_to, scroll_view_update_layout, Overscroll,
    };

    let mut view = chain_view(Overscroll::Chain, true, 0, 0);
    view.following = true;
    scroll_view_update_layout(&mut view, 10, 3);
    assert_eq!((view.scroll_top, view.following), (7, true));

    // A search reveal at the end pins it: at the end, not following.
    scroll_view_scroll_to(&mut view, 7, true);
    assert_eq!((view.scroll_top, view.following), (7, false));
    assert!(view.follow_suppressed_at_end);

    // Streaming rows grow the content: the pinned window keeps its rows.
    scroll_view_update_layout(&mut view, 14, 3);
    assert_eq!(view.scroll_top, 7, "growth must not move a pinned view");
    assert!(!view.following);
    scroll_view_update_layout(&mut view, 18, 3);
    assert_eq!(view.scroll_top, 7);
    assert!(!view.following);

    // `scrollToEnd` (ctrl+end / the jump-to-end indicator) resumes follow and
    // the window re-anchors at the new end.
    scroll_view_scroll_to(&mut view, 15, false);
    assert_eq!((view.scroll_top, view.following), (15, true));
    scroll_view_update_layout(&mut view, 24, 3);
    assert_eq!((view.scroll_top, view.following), (21, true));

    // A view scrolled off the end that a shrink brings back to the end
    // resumes follow (upstream `updateLayout`'s clamp).
    let mut view = chain_view(Overscroll::Chain, true, 1, 2);
    scroll_view_update_layout(&mut view, 3, 3);
    assert_eq!(
        (view.scroll_top, view.following),
        (0, true),
        "content now fits the viewport"
    );
}

/// The `scrollbar: "always"` column reservation (`scroll-view.ts`
/// `getContentWidth` + `render`): the transcript renders one column narrower
/// and pads the freed column, which the scrollbar then paints — the dock is a
/// sibling and keeps the full width. Probed upstream frame at width 12:
/// `abcdefghij ┃` over `DOCK-1234567`.
#[test]
fn scrollbar_always_reserves_the_transcript_column_only() {
    use pir::tui::altscreen_search::{decorate_frame, AltScreenConfig, ScrollbarMode};
    use pir::tui::core::{ReservedScrollbarColumn, Terminal, Text, TuiCore};

    fn render_frame(mode: ScrollbarMode, reserve: bool) -> Vec<String> {
        let terminal = Terminal::new();
        terminal.set_size(12, 5);
        let mut core = TuiCore::new(terminal);
        // A 12-column transcript line: reserved, it wraps at 11 columns; not
        // reserved, it keeps all 12 and the scrollbar would overwrite a cell.
        core.push_content(Box::new(ReservedScrollbarColumn::new(
            Text::with_padding("abcdefghijkl", 0, 0),
            reserve,
        )));
        core.push_content(Box::new(Text::with_padding("DOCK-1234567", 0, 0)));
        core.set_dock_height(1);
        let state = std::sync::Arc::new(std::sync::Mutex::new(AltScreenState::default()));
        let config = AltScreenConfig {
            scrollbar: mode,
            copy_on_select: true,
        };
        core.set_decorator(Some(Box::new(move |lines, ctx| {
            decorate_frame(
                lines,
                ctx,
                &mut state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner),
                &config,
            );
        })));
        core.render();
        core.last_screen_lines()
            .iter()
            .map(|line| {
                pir::tui::ansi::strip_ansi(line)
                    .replace("\u{1b}]8;;\u{7}", "")
                    .trim_end()
                    .to_string()
            })
            .collect()
    }

    let reserved = render_frame(ScrollbarMode::Always, true);
    assert_eq!(
        reserved[0], "abcdefghijk┃",
        "always narrows the transcript to 11 columns and paints the freed one"
    );
    assert_eq!(
        reserved[1],
        format!("{:<11}┃", "l"),
        "the wrapped remainder starts the next row"
    );
    assert_eq!(
        reserved[2], "DOCK-1234567",
        "the dock is not narrowed and carries no scrollbar"
    );

    // Without the reservation (`auto`, content fits) the same line keeps all
    // twelve columns and no column is freed for a scrollbar.
    let automatic = render_frame(ScrollbarMode::Auto, false);
    assert_eq!(automatic[0], "abcdefghijkl");
    assert_eq!(automatic[1], "DOCK-1234567");
}

/// The reservation holds in the windowed (scrolled) path too: the painted
/// window rows carry the scrollbar in the freed column while the dock row
/// (outside the scroll view) keeps the full width — the upstream
/// `getScrollbarGeometry` track covers the ScrollView's own rect only.
#[test]
fn scrollbar_always_reserves_the_windowed_transcript_column() {
    use pir::tui::altscreen_search::{decorate_frame, AltScreenConfig, ScrollbarMode};
    use pir::tui::core::{ReservedScrollbarColumn, Terminal, Text, TuiCore};

    let terminal = Terminal::new();
    terminal.set_size(12, 5);
    let mut core = TuiCore::new(terminal);
    // Three 12-column lines: reserved, each wraps into two 11-column rows.
    core.push_content(Box::new(ReservedScrollbarColumn::new(
        Text::with_padding("abcdefghijkl\nabcdefghijkl\nabcdefghijkl", 0, 0),
        true,
    )));
    core.push_content(Box::new(Text::with_padding("DOCK-1234567", 0, 0)));
    core.set_dock_height(1);
    let state = std::sync::Arc::new(std::sync::Mutex::new(AltScreenState::default()));
    let config = AltScreenConfig {
        scrollbar: ScrollbarMode::Always,
        copy_on_select: true,
    };
    core.set_decorator(Some(Box::new(move |lines, ctx| {
        decorate_frame(
            lines,
            ctx,
            &mut state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
            &config,
        );
    })));
    core.render();
    core.set_scroll_offset(1);
    core.render();

    let painted: Vec<String> = core
        .last_screen_lines()
        .iter()
        .map(|line| {
            pir::tui::ansi::strip_ansi(line)
                .replace("\u{1b}]8;;\u{7}", "")
                .trim_end()
                .to_string()
        })
        .collect();
    assert_eq!(painted.len(), 5, "four window rows over the dock row");
    let scrollbar_glyph = |line: &str| line.ends_with(['│', '┃', '█']);
    assert!(
        painted[..4].iter().all(|line| scrollbar_glyph(line)),
        "every window row carries the scrollbar: {painted:?}"
    );
    assert_eq!(
        painted[4], "DOCK-1234567",
        "the dock row has no scrollbar and keeps the full width"
    );
    // The window content is the 11-column wrap, one column short of the
    // source line: each even row is the first eleven characters.
    assert_eq!(
        painted[1].trim_end_matches(['│', '┃', '█']).trim_end(),
        "abcdefghijk"
    );
}

// ---------------------------------------------------------------------------
// `scrollTo(disableFollow)` through the real viewport: the search reveal pins
// the transcript at the end so streaming rows grow below the window, and a
// scroll toward the end resumes follow.
// ---------------------------------------------------------------------------

struct SharedText(std::sync::Arc<pir::tui::core::Text>);

impl pir::tui::core::Component for SharedText {
    fn render(&self, width: usize) -> Vec<String> {
        self.0.render(width)
    }

    fn invalidate(&self) {}
}

/// A search reveal always scrolls with `disableFollow: true` (upstream
/// `refreshSearch` → `scrollView.scrollTo(target, { disableFollow: true })`),
/// including when the match is already visible at the end: the view then
/// stays put while new rows stream in, and a scroll toward the end (the
/// wheel, or ctrl+end) resumes following.
#[test]
fn search_reveal_at_the_end_pins_follow_until_a_scroll_toward_the_end() {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use pir::tui::altscreen_search::{handle_mouse_event, AltKeyRoute, AltScreenConfig};
    use pir::tui::core::{Terminal, Text, TuiCore};
    use std::sync::Arc;

    let transcript = |lines: usize| {
        (1..=lines)
            .map(|index| format!("line {index}"))
            .collect::<Vec<_>>()
            .join("\n")
    };
    let terminal = Terminal::new();
    terminal.set_size(20, 8);
    let mut core = TuiCore::new(terminal);
    let text = Arc::new(Text::new(transcript(30)));
    core.push_content(Box::new(SharedText(Arc::clone(&text))));
    core.render();
    let mut state = AltScreenState::default();
    let config = AltScreenConfig::default();
    let mut wheel = WheelScrollAccelerator::with_accelerate(WheelScrollLines::Lines(3), true);
    let copy = |_text: &str| false;
    assert_eq!(core.scroll_offset(), 0, "the fixture starts at live follow");

    // Search for the last line: the match is already visible, yet upstream's
    // `disableFollow` reveal pins the view at the end.
    //
    // `tui.altScreen.search` is platform-rebound (core/keybindings.ts): `ctrl+f`
    // where `useWindowsKeybindings()` holds (native Windows/WSL), `ctrl+shift+f`
    // elsewhere — in fullscreen the Windows branch deliberately shadows the
    // editor's `ctrl+f`.
    let search_modifiers = if pir::config::keybindings::use_windows_keybindings() {
        KeyModifiers::CONTROL
    } else {
        KeyModifiers::CONTROL | KeyModifiers::SHIFT
    };
    let open = KeyEvent::new(KeyCode::Char('f'), search_modifiers);
    assert!(matches!(
        pir::tui::altscreen_search::route_alt_screen_key(&open, &mut core, &mut state, 0),
        AltKeyRoute::Consumed
    ));
    for ch in "line 30".chars() {
        let key = KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE);
        pir::tui::altscreen_search::route_alt_screen_key(&key, &mut core, &mut state, 0);
    }
    assert_eq!(
        core.scroll_offset(),
        0,
        "the match sits at the transcript end"
    );
    assert!(
        !core.is_following(),
        "the reveal pins the end (upstream disableFollow)"
    );

    // Streaming rows must grow below the window, not push it.
    text.set_text(transcript(50));
    core.render();
    assert_eq!(
        core.scroll_offset(),
        20,
        "20 new rows push the offset away from the end"
    );
    assert!(!core.is_following());
    let pinned_last =
        pir::tui::ansi::strip_ansi(core.last_screen_lines().last().expect("a painted frame"));
    assert!(
        pinned_last.contains("line 30"),
        "the pinned window still ends on line 30, got {pinned_last:?}"
    );

    // Wheel notches toward the end walk the view back to the live end and
    // re-arm follow (upstream `ScrollView.scrollBy`: reaching the end sets
    // `followingEnd`), after which the next rows follow again.
    let wheel_down = crossterm::event::MouseEvent {
        kind: crossterm::event::MouseEventKind::ScrollDown,
        column: 3,
        row: 2,
        modifiers: KeyModifiers::NONE,
    };
    for _ in 0..8 {
        assert!(handle_mouse_event(
            wheel_down, &mut core, &mut state, &config, &mut wheel, 0, &copy,
        ));
        if core.is_following() {
            break;
        }
    }
    assert!(core.is_following(), "scrolling to the end resumes follow");
    assert_eq!(core.scroll_offset(), 0);
    text.set_text(transcript(60));
    core.render();
    assert_eq!(core.scroll_offset(), 0, "follow re-anchors at the new end");
    let live_last =
        pir::tui::ansi::strip_ansi(core.last_screen_lines().last().expect("a painted frame"));
    assert!(
        live_last.contains("line 60"),
        "the live end is visible again"
    );
}

/// Regression for the ConPTY play test's no-match viewport shift (D2): from
/// the follow-end, typing `zzz` scrolled the transcript 11 rows back while the
/// box said "No matches". The fixture's `zeta` makes `z` match EVERY row (the
/// play test's transcript), so the first keystroke legitimately revealed and
/// jumped (the upstream one-third-viewport placement). The zero-match edits
/// that follow must put the viewport back where the search began instead of
/// leaving it at that stale prefix reveal; a matching query still jumps.
#[test]
fn no_match_query_keeps_the_follow_end_instead_of_the_prefix_reveal() {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use pir::tui::altscreen_search::{route_alt_screen_key, AltKeyRoute};
    use pir::tui::core::{Terminal, Text, TuiCore};
    use std::sync::Arc;

    let transcript = |lines: usize| {
        (1..=lines)
            .map(|index| format!("line {index} zeta"))
            .collect::<Vec<_>>()
            .join("\n")
    };
    let terminal = Terminal::new();
    terminal.set_size(20, 8);
    let mut core = TuiCore::new(terminal);
    core.push_content(Box::new(SharedText(Arc::new(Text::new(transcript(30))))));
    core.render();
    let mut state = AltScreenState::default();

    let top_line = |core: &pir::tui::core::TuiCore| {
        let top = viewport_top_row(core, 0);
        pir::tui::ansi::strip_ansi(&core.last_frame_lines()[top])
            .trim()
            .to_string()
    };
    assert_eq!(core.scroll_offset(), 0, "the fixture starts at live follow");
    assert!(core.is_following());
    let top_before = viewport_top_row(&core, 0);
    let first_before = top_line(&core);
    assert_eq!(
        first_before, "line 23 zeta",
        "sanity: the follow-end window"
    );

    let search_modifiers = if pir::config::keybindings::use_windows_keybindings() {
        KeyModifiers::CONTROL
    } else {
        KeyModifiers::CONTROL | KeyModifiers::SHIFT
    };
    let open = KeyEvent::new(KeyCode::Char('f'), search_modifiers);
    assert!(matches!(
        route_alt_screen_key(&open, &mut core, &mut state, 0),
        AltKeyRoute::Consumed
    ));

    // The play test's shape: `z` alone matches `zeta` on every row and jumps,
    // so the viewport does move on the first keystroke.
    let type_key = |core: &mut TuiCore, state: &mut AltScreenState, c: char| {
        let key = KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE);
        route_alt_screen_key(&key, core, state, 0);
        core.render();
    };
    type_key(&mut core, &mut state, 'z');
    assert!(
        viewport_top_row(&core, 0) < top_before,
        "sanity: `z` matches and reveals above the follow-end"
    );
    type_key(&mut core, &mut state, 'z');
    type_key(&mut core, &mut state, 'z');

    // `zzz` matches nothing: the viewport is back at the pre-search position,
    // following again, with nothing selected and the honest empty notice.
    assert_eq!(state.search.result_text(), "No matches");
    assert!(state.search.matches.is_empty());
    assert_eq!(state.search.selected, None);
    assert_eq!(
        core.scroll_offset(),
        0,
        "a no-match query must not leave the viewport at the prefix's reveal"
    );
    assert!(core.is_following(), "the follow-end follows again");
    assert_eq!(
        viewport_top_row(&core, 0),
        top_before,
        "the first visible row is unchanged"
    );
    assert_eq!(top_line(&core), first_before);

    // A matching query still jumps: backspace to empty (which keeps the view
    // where it is), then search for a line far above the viewport.
    for _ in 0..3 {
        let key = KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE);
        route_alt_screen_key(&key, &mut core, &mut state, 0);
        core.render();
    }
    for ch in "line 5".chars() {
        type_key(&mut core, &mut state, ch);
    }
    assert!(!state.search.matches.is_empty());
    assert!(state.search.selected.is_some());
    assert!(
        core.scroll_offset() > 0,
        "the matching query is revealed above the end"
    );
    let screen: Vec<String> = core
        .last_screen_lines()
        .iter()
        .map(|line| pir::tui::ansi::strip_ansi(line).trim().to_string())
        .collect();
    assert!(
        screen.iter().any(|line| line == "line 5 zeta"),
        "the revealed match line is on screen: {screen:?}"
    );
}

// ---------------------------------------------------------------------------
// Full-screen selection: screen-row → buffer-row mapping incl. the dock
//
// The selection corpus (`TuiCore::last_frame_lines`) is the whole undecorated
// frame (header + transcript + dock), and the mapping now resolves every
// painted row to its own buffer row — transcript rows through the painted
// window, the dock rows (editor, footer, open dialog/selector) through the
// dock's buffer rows. Before this, dock rows clamped to the last transcript
// row, so a drag over "the content below the transcript" selected nothing of
// it.
// ---------------------------------------------------------------------------

/// The follow-path mapping: screen rows are `visible_top + row` for all
/// painted rows, and the dock rows map to the frame's last `dock_len` rows.
#[test]
fn screen_rows_map_to_their_buffer_rows_including_the_dock() {
    use pir::tui::core::{Terminal, Text, TuiCore};

    let terminal = Terminal::new();
    terminal.set_size(20, 6);
    let mut core = TuiCore::new(terminal);
    let transcript: String = (1..=8)
        .map(|index| format!("line {index}"))
        .collect::<Vec<_>>()
        .join("\n");
    core.push_content(Box::new(Text::with_padding(transcript, 0, 0)));
    core.push_content(Box::new(Text::with_padding(
        "EDITOR row\nFOOTER status",
        0,
        0,
    )));
    core.set_dock_height(2);
    core.render();

    let frame_len = core.frame_buffer_length();
    assert_eq!(frame_len, 10, "8 transcript rows + 2 dock rows");
    assert_eq!(core.visible_top(), 4, "the frame bottoms out at 6 rows");
    // The corpus is the *painted* frame: rows are padded to the terminal width,
    // and the copy path trims that padding per line (`selection_text`), so the
    // clipboard gets exactly what the user sees.
    assert_eq!(core.last_frame_lines()[8], format!("{:<20}", "EDITOR row"));
    assert_eq!(
        core.last_frame_lines()[9],
        format!("{:<20}", "FOOTER status")
    );
    for row in 0..6 {
        assert_eq!(
            core.screen_row_to_buffer_row(row),
            Some(4 + row),
            "screen row {row}"
        );
    }
    // A row below the painted frame has no buffer row.
    assert_eq!(core.screen_row_to_buffer_row(6), None);

    // A frame shorter than the screen: the rows below it have no line, and
    // the dock is still mapped to its own rows.
    let terminal = Terminal::new();
    terminal.set_size(20, 8);
    let mut core = TuiCore::new(terminal);
    core.push_content(Box::new(Text::with_padding("one\ntwo", 0, 0)));
    core.push_content(Box::new(Text::with_padding("EDITOR", 0, 0)));
    core.set_dock_height(1);
    core.render();
    assert_eq!(core.screen_row_to_buffer_row(0), Some(0));
    assert_eq!(core.screen_row_to_buffer_row(1), Some(1));
    assert_eq!(core.screen_row_to_buffer_row(2), Some(2), "the dock row");
    assert_eq!(core.screen_row_to_buffer_row(3), None);
}

/// A drag that reaches the dock rows selects their text and paints the
/// highlight there: the copied text is exactly the line on screen.
#[test]
fn drag_selection_covers_the_dock_rows_and_copies_the_screen_text() {
    use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
    use pir::tui::altscreen_search::{handle_mouse_event, AltScreenConfig};
    use pir::tui::core::{Terminal, Text, TuiCore};
    use std::cell::RefCell;

    let terminal = Terminal::new();
    terminal.set_size(20, 6);
    let mut core = TuiCore::new(terminal);
    core.push_content(Box::new(Text::with_padding(
        "one\ntwo\nthree\nfour\nfive\nsix\nseven\neight",
        0,
        0,
    )));
    core.push_content(Box::new(Text::with_padding(
        "EDITOR row\nFOOTER status",
        0,
        0,
    )));
    core.set_dock_height(2);
    core.render();
    assert_eq!(core.visible_top(), 4);
    assert_eq!(
        core.last_frame_lines()[9],
        format!("{:<20}", "FOOTER status")
    );

    // The selection paint runs through the alt-screen frame decorator (the app
    // installs it while the alt screen is active, see `src/tui/terminal.rs`),
    // so the test wires the same shared state into it before rendering.
    let state = std::sync::Arc::new(std::sync::Mutex::new(AltScreenState::default()));
    let config = AltScreenConfig::default();
    {
        let decorated = std::sync::Arc::clone(&state);
        core.set_decorator(Some(Box::new(move |lines, ctx| {
            pir::tui::altscreen_search::decorate_frame(
                lines,
                ctx,
                &mut decorated
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner),
                &config,
            );
        })));
    }
    let mut wheel = WheelScrollAccelerator::with_accelerate(WheelScrollLines::Lines(1), true);
    let copied = RefCell::new(None);
    let copy = |text: &str| {
        *copied.borrow_mut() = Some(text.to_string());
        true
    };
    let event = |kind, column, row| MouseEvent {
        kind,
        column,
        row,
        modifiers: KeyModifiers::NONE,
    };
    // Screen row 5 is the dock's second row (buffer row 9): press at column 0
    // and drag across "FOOTER status". The gesture is split — the highlight is
    // painted while the drag is live, and the release copies.
    let drive = |core: &mut TuiCore,
                 state: &std::sync::Arc<std::sync::Mutex<AltScreenState>>,
                 wheel: &mut WheelScrollAccelerator,
                 kind,
                 column| {
        handle_mouse_event(
            event(kind, column, 5),
            core,
            &mut state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
            &config,
            wheel,
            2,
            &copy,
        )
    };
    assert!(
        drive(
            &mut core,
            &state,
            &mut wheel,
            MouseEventKind::Down(MouseButton::Left),
            0
        ),
        "the press over the dock row must be handled"
    );
    assert!(
        drive(
            &mut core,
            &state,
            &mut wheel,
            MouseEventKind::Drag(MouseButton::Left),
            13
        ),
        "the drag over the dock row must be handled"
    );
    assert!(
        copied.borrow().is_none(),
        "a live drag copies nothing yet: {:?}",
        copied.borrow()
    );

    // The painted frame highlights the dock row (and no transcript row). The
    // follow path writes the whole undecorated frame — the terminal shows its
    // bottom `rows`, so buffer row 9 *is* screen row 5 here — which is why the
    // dock's footer row is the frame's last row in the paint buffer.
    core.render();
    let painted = core.last_screen_lines();
    let footer = painted.len() - 1;
    assert_eq!(footer, 9, "the footer row is the frame's last row");
    assert!(
        painted[footer].contains("\x1b[7m"),
        "the dock row carries the selection highlight: {:?}",
        painted[footer]
    );
    assert!(
        !painted[footer - 1].contains("\x1b[7m"),
        "the transcript row above is not selected: {:?}",
        painted[footer - 1]
    );
    assert_eq!(
        pir::tui::ansi::strip_ansi(&painted[footer])
            .replace("\u{1b}]8;;\u{7}", "")
            .trim_end(),
        "FOOTER status"
    );

    // Release: the selection is copied and the copy flash is painted over the
    // dock row (upstream's `Copied!` banner), which is why the highlight check
    // above runs while the gesture is still live.
    assert!(
        drive(
            &mut core,
            &state,
            &mut wheel,
            MouseEventKind::Up(MouseButton::Left),
            13
        ),
        "the release over the dock row must be handled"
    );
    assert_eq!(copied.borrow().as_deref(), Some("FOOTER status"));
    assert_eq!(
        state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .selection
            .map(|span| (span.start, span.end)),
        Some(((9, 0), (9, 13))),
        "the span lives in buffer coordinates on the dock row"
    );
    core.render();
    let flashed = pir::tui::ansi::strip_ansi(&core.last_screen_lines()[footer])
        .replace("\u{1b}]8;;\u{7}", "")
        .trim_end()
        .to_string();
    assert!(
        flashed.contains("Copied!"),
        "the copy flash lands on the dock row: {flashed:?}"
    );
}

/// Wide (CJK) cells: a column span selects whole grapheme clusters, so the
/// copied text is exactly the cells the user dragged over — on transcript
/// rows and on dock rows alike.
#[test]
fn selection_columns_agree_with_wide_characters() {
    use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
    use pir::tui::altscreen_search::{handle_mouse_event, AltScreenConfig};
    use pir::tui::core::{Terminal, Text, TuiCore};
    use std::cell::RefCell;

    let terminal = Terminal::new();
    terminal.set_size(20, 4);
    let mut core = TuiCore::new(terminal);
    // Row 0: 中(0-1) 文(2-3) x(4) a(5) b(6) c(7). Row 1 (the dock):
    // 编(0-1) 辑(2-3) 器(4-5) 行(6-7).
    core.push_content(Box::new(Text::with_padding("中文xabc", 0, 0)));
    core.push_content(Box::new(Text::with_padding("编辑器行", 0, 0)));
    core.set_dock_height(1);
    core.render();

    let mut state = AltScreenState::default();
    let config = AltScreenConfig::default();
    let mut wheel = WheelScrollAccelerator::with_accelerate(WheelScrollLines::Lines(1), true);
    let copied = RefCell::new(String::new());
    let copy = |text: &str| {
        *copied.borrow_mut() = text.to_string();
        true
    };
    let event = |kind, column: u16, row: u16| MouseEvent {
        kind,
        column,
        row,
        modifiers: KeyModifiers::NONE,
    };

    // Transcript row 0, columns 2..4: the second wide cell ("文") — dragging
    // over a wide cell keeps or drops it whole, never a half fragment.
    for kind in [
        MouseEventKind::Down(MouseButton::Left),
        MouseEventKind::Drag(MouseButton::Left),
        MouseEventKind::Up(MouseButton::Left),
    ] {
        let column = if kind == MouseEventKind::Down(MouseButton::Left) {
            2
        } else {
            4
        };
        assert!(handle_mouse_event(
            event(kind, column, 0),
            &mut core,
            &mut state,
            &config,
            &mut wheel,
            1,
            &copy,
        ));
    }
    assert_eq!(copied.borrow().as_str(), "文");
    assert_eq!(
        state.selection.map(|span| (span.start, span.end)),
        Some(((0, 2), (0, 4)))
    );

    // Dock row 1, columns 2..6: "辑器" (two whole wide cells).
    for kind in [
        MouseEventKind::Down(MouseButton::Left),
        MouseEventKind::Drag(MouseButton::Left),
        MouseEventKind::Up(MouseButton::Left),
    ] {
        let column = if kind == MouseEventKind::Down(MouseButton::Left) {
            2
        } else {
            6
        };
        assert!(handle_mouse_event(
            event(kind, column, 1),
            &mut core,
            &mut state,
            &config,
            &mut wheel,
            1,
            &copy,
        ));
    }
    assert_eq!(copied.borrow().as_str(), "辑器");
    assert_eq!(
        state.selection.map(|span| (span.start, span.end)),
        Some(((1, 2), (1, 6)))
    );
}

/// The scrolled (windowed) path: the painted frame is a transcript window
/// with the dock directly under it, so the screen rows past the window map to
/// the dock's buffer rows through the gap — a drag on the dock's last row
/// copies that line, not a transcript row.
#[test]
fn windowed_selection_maps_the_dock_through_the_gap() {
    use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
    use pir::tui::altscreen_search::{handle_mouse_event, AltScreenConfig};
    use pir::tui::core::{Terminal, Text, TuiCore};
    use std::cell::RefCell;

    let terminal = Terminal::new();
    terminal.set_size(20, 8);
    let mut core = TuiCore::new(terminal);
    let transcript: String = (1..=30)
        .map(|index| format!("line {index}"))
        .collect::<Vec<_>>()
        .join("\n");
    core.push_content(Box::new(Text::with_padding(transcript, 0, 0)));
    core.push_content(Box::new(Text::with_padding("EDITOR\nFOOTER", 0, 0)));
    core.set_dock_height(2);
    core.render();
    core.set_scroll_offset(5);
    core.render();

    // 30 transcript rows + 2 dock rows; a 6-row window scrolled 5 rows above
    // the transcript end starts at row 19, the dock sits on screen rows 6-7.
    let layout = core.painted_layout();
    assert!(layout.windowed);
    assert_eq!(layout.window_start, 19);
    assert_eq!(layout.window_len, 6);
    assert_eq!(layout.transcript_end, 30);
    assert_eq!(layout.dock_len, 2);
    assert_eq!(core.screen_row_to_buffer_row(5), Some(24));
    assert_eq!(core.screen_row_to_buffer_row(6), Some(30), "EDITOR");
    assert_eq!(core.screen_row_to_buffer_row(7), Some(31), "FOOTER");

    let mut state = AltScreenState::default();
    let config = AltScreenConfig::default();
    let mut wheel = WheelScrollAccelerator::with_accelerate(WheelScrollLines::Lines(1), true);
    let copied = RefCell::new(None);
    let copy = |text: &str| {
        *copied.borrow_mut() = Some(text.to_string());
        true
    };
    let event = |kind, column: u16, row: u16| MouseEvent {
        kind,
        column,
        row,
        modifiers: KeyModifiers::NONE,
    };
    for kind in [
        MouseEventKind::Down(MouseButton::Left),
        MouseEventKind::Drag(MouseButton::Left),
        MouseEventKind::Up(MouseButton::Left),
    ] {
        let column = if kind == MouseEventKind::Down(MouseButton::Left) {
            0
        } else {
            6
        };
        assert!(handle_mouse_event(
            event(kind, column, 7),
            &mut core,
            &mut state,
            &config,
            &mut wheel,
            2,
            &copy,
        ));
    }
    assert_eq!(copied.borrow().as_deref(), Some("FOOTER"));
    assert_eq!(
        state.selection.map(|span| (span.start, span.end)),
        Some(((31, 0), (31, 6)))
    );
}

// ---------------------------------------------------------------------------
// P2 mouse/UX parity (upstream `tui-alt-screen.ts` + `interactive-mode.ts`):
// scrollbar click/drag scrubbing, the hover-active thumb (including the
// `"auto"` reveal), the jump-to-end indicator, the Windows right-click paste
// route, and the search box's hover-styled prev/next buttons.
// ---------------------------------------------------------------------------

/// A fullscreen fixture with the alt-screen decorator installed on a shared
/// state (the same wiring the event loop does in `src/tui/terminal.rs`):
/// `transcript_lines` numbered rows plus an optional two-row dock, rendered
/// once at the given scroll offset.
struct MouseFixture {
    core: pir::tui::core::TuiCore,
    state: std::sync::Arc<std::sync::Mutex<AltScreenState>>,
    config: pir::tui::altscreen_search::AltScreenConfig,
    wheel: WheelScrollAccelerator,
    copied: std::rc::Rc<std::cell::RefCell<Vec<String>>>,
    dock: usize,
}

impl MouseFixture {
    fn new(
        width: u16,
        rows: u16,
        transcript_lines: usize,
        dock: Option<&str>,
        scrollbar: pir::tui::altscreen_search::ScrollbarMode,
        copy_on_select: bool,
        scroll_offset: usize,
    ) -> Self {
        use pir::tui::core::{Terminal, Text, TuiCore};

        let terminal = Terminal::new();
        terminal.set_size(width as usize, rows as usize);
        let mut core = TuiCore::new(terminal);
        let transcript: String = (1..=transcript_lines)
            .map(|index| format!("line {index}"))
            .collect::<Vec<_>>()
            .join("\n");
        core.push_content(Box::new(Text::with_padding(transcript, 0, 0)));
        let dock_len = match dock {
            Some(text) => {
                core.push_content(Box::new(Text::with_padding(text, 0, 0)));
                2
            }
            None => 0,
        };
        core.set_dock_height(dock_len);
        let config = pir::tui::altscreen_search::AltScreenConfig {
            scrollbar,
            copy_on_select,
        };
        let state = std::sync::Arc::new(std::sync::Mutex::new(AltScreenState::default()));
        {
            let decorated = std::sync::Arc::clone(&state);
            core.set_decorator(Some(Box::new(move |lines, ctx| {
                pir::tui::altscreen_search::decorate_frame(
                    lines,
                    ctx,
                    &mut decorated
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner),
                    &config,
                );
            })));
        }
        core.render();
        if scroll_offset > 0 {
            core.set_scroll_offset(scroll_offset);
            core.render();
        }
        Self {
            core,
            state,
            config,
            wheel: WheelScrollAccelerator::with_accelerate(WheelScrollLines::Lines(1), true),
            copied: std::rc::Rc::new(std::cell::RefCell::new(Vec::new())),
            dock: dock_len,
        }
    }

    fn render(&mut self) {
        self.core.render();
    }

    /// Route one mouse event through the alt-screen family, recording any
    /// copy-on-select into `copied`.
    fn route(
        &mut self,
        event: crossterm::event::MouseEvent,
    ) -> pir::tui::altscreen_search::AltMouseRoute {
        let copied = std::rc::Rc::clone(&self.copied);
        let copy = move |text: &str| {
            copied.borrow_mut().push(text.to_string());
            true
        };
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        pir::tui::altscreen_search::handle_mouse_event_route(
            event,
            &mut self.core,
            &mut state,
            &self.config,
            &mut self.wheel,
            self.dock,
            &copy,
        )
    }

    /// Borrow the shared interaction state (read or write).
    fn with_state<T>(&self, f: impl FnOnce(&mut AltScreenState) -> T) -> T {
        f(&mut self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner))
    }

    /// The painted screen rows, ANSI-stripped and right-trimmed: the follow
    /// path paints every frame row (the ones above the viewport scroll into
    /// the terminal's own scrollback), so the screen is the frame's tail.
    fn screen(&self) -> Vec<String> {
        let rows = self.core.rows().max(1);
        let lines = self.core.last_screen_lines();
        let start = lines.len().saturating_sub(rows);
        lines[start..]
            .iter()
            .map(|line| {
                pir::tui::ansi::strip_ansi(line)
                    .replace("\u{1b}]8;;\u{7}", "")
                    .trim_end()
                    .to_string()
            })
            .collect()
    }
}

/// One mouse event at `(column, row)` with no modifiers.
fn mouse_event(
    kind: crossterm::event::MouseEventKind,
    column: u16,
    row: u16,
) -> crossterm::event::MouseEvent {
    crossterm::event::MouseEvent {
        kind,
        column,
        row,
        modifiers: crossterm::event::KeyModifiers::NONE,
    }
}

fn left_press(column: u16, row: u16) -> crossterm::event::MouseEvent {
    mouse_event(
        crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left),
        column,
        row,
    )
}

fn left_drag(column: u16, row: u16) -> crossterm::event::MouseEvent {
    mouse_event(
        crossterm::event::MouseEventKind::Drag(crossterm::event::MouseButton::Left),
        column,
        row,
    )
}

fn left_release(column: u16, row: u16) -> crossterm::event::MouseEvent {
    mouse_event(
        crossterm::event::MouseEventKind::Up(crossterm::event::MouseButton::Left),
        column,
        row,
    )
}

fn moved(column: u16, row: u16) -> crossterm::event::MouseEvent {
    mouse_event(crossterm::event::MouseEventKind::Moved, column, row)
}

// --- Scrollbar click/drag scrubbing (upstream `handleScrollbarMouseEvent`) ---

/// Upstream `tui-alt-screen.test.ts` "jumps to a scrollbar track position and
/// continues dragging from there": 10x10 terminal, 50 rows of content,
/// `scrollbar: "always"` — track height 10, `maxScrollTop` 40, thumb 2 rows.
/// A press on the track (not the thumb) grabs with the CENTER offset
/// (`floor(thumbHeight / 2)`) and jumps the view to the pointer row; the drag
/// that follows keeps scrubbing with the same grab offset.
#[test]
fn scrollbar_track_press_jumps_to_the_pointer_and_the_drag_scrubs() {
    use pir::tui::altscreen_search::{AltMouseRoute, ScrollbarMode};

    let mut fx = MouseFixture::new(10, 10, 50, None, ScrollbarMode::Always, false, 0);
    assert_eq!(fx.core.scroll_offset(), 0, "live follow");
    assert_eq!(
        pir::tui::altscreen_search::clip_geometry(&fx.core.painted_layout()).clip_height,
        10
    );

    // Press at track row 5 (the thumb sits at rows 8-9 while following):
    // grab = floor(2/2) = 1, thumbOffset = clamp(5-1, 0, 8) = 4,
    // scrollTop = round(4/8 * 40) = 20 → pir offset 20.
    assert!(matches!(fx.route(left_press(9, 5)), AltMouseRoute::Repaint));
    assert_eq!(
        fx.core.scroll_offset(),
        20,
        "the press jumps to the pointer"
    );
    assert!(fx.with_state(|state| state.scrollbar_dragging()));
    assert!(
        fx.copied.borrow().is_empty(),
        "no selection/copy on a scrollbar press"
    );

    // Drag to the last track row: thumbOffset 8 → scrollTop 40 → the view
    // reaches the end.
    assert!(matches!(fx.route(left_drag(9, 9)), AltMouseRoute::Repaint));
    assert_eq!(fx.core.scroll_offset(), 0, "the drag scrubs with the grab");

    // A drag back up scrubs the other way without any per-event render need.
    assert!(matches!(fx.route(left_drag(9, 2)), AltMouseRoute::Repaint));
    assert_eq!(
        fx.core.scroll_offset(),
        35,
        "clamp(2-1) = 1 → round(1/8*40)"
    );
}

/// Regression: a drag that reaches the TOP of the track must keep scrubbing
/// when the pointer moves back down (the thumb grab is relative to the
/// press point; reaching the top must not pin or end the gesture).
#[test]
fn scrollbar_drag_to_the_top_then_back_down_scrubs_both_ways() {
    use pir::tui::altscreen_search::{AltMouseRoute, ScrollbarMode};

    let mut fx = MouseFixture::new(10, 10, 50, None, ScrollbarMode::Always, true, 0);
    // Following at the bottom: the thumb sits on rows 8-9. Grab it at row 9.
    assert!(matches!(fx.route(left_press(9, 9)), AltMouseRoute::Repaint));
    assert!(fx.with_state(|state| state.scrollbar_dragging()));

    // Up to the first track row: the view reaches the oldest content.
    assert!(matches!(fx.route(left_drag(9, 0)), AltMouseRoute::Repaint));
    assert_eq!(fx.core.scroll_offset(), 40, "the drag reaches the top");

    // Back down to the middle: the gesture must still own the pointer.
    assert!(matches!(fx.route(left_drag(9, 5)), AltMouseRoute::Repaint));
    assert_eq!(
        fx.core.scroll_offset(),
        20,
        "dragging back down after hitting the top scrubs again"
    );

    // And back to the bottom.
    assert!(matches!(fx.route(left_drag(9, 9)), AltMouseRoute::Repaint));
    assert_eq!(fx.core.scroll_offset(), 0, "the drag returns to the end");
    assert!(fx.with_state(|state| state.scrollbar_dragging()));
}

/// Regression for the real event path's second half: an `"auto"` bar fades
/// after [`SCROLLBAR_LINGER`], and the next left press on the SAME track
/// column used to fall through to text selection (the press hit-test only
/// accepted a VISIBLE bar, and the hover update runs after the press). The
/// press itself proves the pointer is on the track, so it must grab the bar
/// and the following drag must scrub.
#[test]
fn scrollbar_press_after_linger_grabs_the_hidden_auto_bar_and_drags_down() {
    use pir::tui::altscreen_search::{AltMouseRoute, ScrollbarMode, SCROLLBAR_LINGER};

    let mut fx = MouseFixture::new(10, 10, 50, None, ScrollbarMode::Auto, true, 0);
    let bar_glyph = |line: &str| line.ends_with(['\u{2502}', '\u{2503}', '\u{2588}']);
    let bars = |fx: &MouseFixture| {
        fx.screen()
            .into_iter()
            .filter(|line| bar_glyph(line))
            .count()
    };

    // A wheel gesture reveals the transient auto bar (and moves the view).
    assert!(matches!(
        fx.route(mouse_event(
            crossterm::event::MouseEventKind::ScrollUp,
            5,
            5
        )),
        AltMouseRoute::Repaint
    ));
    assert!(fx.core.scroll_offset() > 0);
    fx.render();
    assert!(bars(&fx) > 0, "the wheel gesture shows the bar");

    // Grab the thumb at the bottom and drag it to the first track row: the
    // view reaches the oldest content.
    assert!(matches!(fx.route(left_press(9, 9)), AltMouseRoute::Repaint));
    assert!(fx.with_state(|state| state.scrollbar_dragging()));
    assert!(matches!(fx.route(left_drag(9, 0)), AltMouseRoute::Repaint));
    assert_eq!(fx.core.scroll_offset(), 40, "the drag reaches the top");
    assert!(matches!(
        fx.route(left_release(9, 0)),
        AltMouseRoute::Repaint
    ));
    assert!(!fx.with_state(|state| state.scrollbar_dragging()));

    // Leave the track and wait out the linger: the auto bar is hidden.
    assert!(matches!(fx.route(moved(5, 0)), AltMouseRoute::Repaint));
    assert!(!fx.with_state(|state| state.scrollbar_hover()));
    fx.with_state(|state| {
        state.tick(
            std::time::Instant::now() + SCROLLBAR_LINGER + std::time::Duration::from_millis(1),
        )
    });
    fx.render();
    assert_eq!(
        bars(&fx),
        0,
        "the linger expired and the bar is hidden: {:?}",
        fx.screen()
    );

    // The fresh press on the same column must grab the hidden bar (track
    // press: center grab + jump), NOT start a text selection. Before the fix
    // this returned a selection gesture and the drag below did nothing.
    assert!(matches!(fx.route(left_press(9, 5)), AltMouseRoute::Repaint));
    assert!(
        fx.with_state(|state| state.scrollbar_dragging()),
        "the hidden auto bar is grabbable at its column"
    );
    assert_eq!(
        fx.core.scroll_offset(),
        20,
        "track press jumps: grab 1, thumb offset 4 → scrollTop 20"
    );

    // Dragging back down scrubs again after the top.
    assert!(matches!(fx.route(left_drag(9, 9)), AltMouseRoute::Repaint));
    assert_eq!(
        fx.core.scroll_offset(),
        0,
        "the drag returns to the end after the press"
    );
    assert!(matches!(
        fx.route(left_release(9, 9)),
        AltMouseRoute::Repaint
    ));
    assert!(!fx.with_state(|state| state.scrollbar_dragging()));
    assert!(
        fx.with_state(|state| state.selection.is_none()),
        "the bar gesture never becomes a text selection"
    );
    assert!(
        fx.copied.borrow().is_empty(),
        "no copy-on-select was triggered"
    );
}

/// A press on the last column of a transcript that FITS the viewport is not
/// a scrollbar target (`"auto"` has no overflow, so no bar exists): it stays
/// a normal text-selection gesture. This is the boundary the hidden-auto
/// press deviation must not cross.
#[test]
fn scrollbar_press_on_a_non_overflow_transcript_still_selects() {
    use pir::tui::altscreen_search::{AltMouseRoute, ScrollbarMode};

    let mut fx = MouseFixture::new(10, 10, 5, None, ScrollbarMode::Auto, true, 0);
    assert!(matches!(fx.route(left_press(9, 3)), AltMouseRoute::Repaint));
    assert!(
        !fx.with_state(|state| state.scrollbar_dragging()),
        "no bar exists over a fitting transcript"
    );
    assert_eq!(fx.core.scroll_offset(), 0, "no jump");
    assert!(matches!(fx.route(left_drag(0, 3)), AltMouseRoute::Repaint));
    assert!(matches!(
        fx.route(left_release(0, 3)),
        AltMouseRoute::Repaint
    ));
    assert_eq!(fx.copied.borrow().as_slice(), ["line 4"]);
    assert!(fx.with_state(|state| state.selection.is_some()));
}

/// The release ends the drag (upstream `stopScrollbarDrag`): later pointer
/// motion must not scrub, and the ended gesture leaves no text selection.
#[test]
fn scrollbar_release_ends_the_drag_and_never_selects() {
    use pir::tui::altscreen_search::{AltMouseRoute, ScrollbarMode};

    let mut fx = MouseFixture::new(10, 10, 50, None, ScrollbarMode::Always, true, 0);
    assert!(matches!(fx.route(left_press(9, 5)), AltMouseRoute::Repaint));
    assert_eq!(fx.core.scroll_offset(), 20);
    assert!(matches!(fx.route(left_drag(9, 8)), AltMouseRoute::Repaint));
    assert_eq!(
        fx.core.scroll_offset(),
        5,
        "clamp(8-1) = 7 → scrollTop round(7/8*40) = 35 → offset 5"
    );
    assert!(matches!(
        fx.route(left_release(9, 8)),
        AltMouseRoute::Repaint
    ));
    assert!(!fx.with_state(|state| state.scrollbar_dragging()));

    // Motion over the top of the track would scrub to the top if the drag
    // were still live. After the release it is a plain hover: the pointer is
    // already on the track (the press set the hover), so nothing changes and
    // no repaint is requested (upstream `setScrollbarActive` only requests a
    // render when the active flag flips) — but the view must not move.
    assert!(matches!(fx.route(moved(9, 0)), AltMouseRoute::Ignore));
    assert_eq!(
        fx.core.scroll_offset(),
        5,
        "a released drag no longer scrubs"
    );
    assert!(fx.with_state(|state| state.scrollbar_hover()));
    // A motion that DOES change the hover reports the repaint (and ends the
    // active glyph).
    assert!(matches!(fx.route(moved(5, 0)), AltMouseRoute::Repaint));
    assert!(!fx.with_state(|state| state.scrollbar_hover()));
    assert_eq!(fx.core.scroll_offset(), 5, "still no scrub");
    assert!(
        fx.with_state(|state| state.selection.is_none()),
        "the bar gesture never becomes a text selection"
    );
    assert!(
        fx.copied.borrow().is_empty(),
        "no copy-on-select was triggered"
    );
}

/// On the thumb the press keeps the relative grab offset (upstream:
/// `grabOffset = event.y - thumbTop`, no jump): the drag moves the view by the
/// pointer's delta, not by re-centering the thumb.
#[test]
fn scrollbar_thumb_press_drags_relative_to_the_grab_offset() {
    use pir::tui::altscreen_search::{AltMouseRoute, ScrollbarMode};

    let mut fx = MouseFixture::new(10, 10, 50, None, ScrollbarMode::Always, false, 0);
    // Following: scrollTop 40 → thumb at track rows 8-9.
    assert!(matches!(fx.route(left_press(9, 9)), AltMouseRoute::Repaint));
    assert_eq!(
        fx.core.scroll_offset(),
        0,
        "a thumb press does not jump the view"
    );
    // Drag to track row 3: grab = 9 - 8 = 1 → thumbOffset = 2 → scrollTop 10.
    assert!(matches!(fx.route(left_drag(9, 3)), AltMouseRoute::Repaint));
    assert_eq!(
        fx.core.scroll_offset(),
        30,
        "relative drag: round((3-1)/8 * 40) = 10 → offset 30"
    );
    assert!(matches!(
        fx.route(left_release(9, 3)),
        AltMouseRoute::Repaint
    ));
}

/// The track is the transcript clip only: a press on the dock's last column
/// (same column, row past the track) or on another column is not a grab and
/// stays a text-selection gesture.
#[test]
fn scrollbar_press_outside_the_track_is_not_a_grab() {
    use pir::tui::altscreen_search::{AltMouseRoute, ScrollbarMode};

    // 12x8, 30 transcript rows + a 2-row dock → the track is 6 rows
    // (screen rows 0-5), the dock rows 6-7.
    let mut fx = MouseFixture::new(
        12,
        8,
        30,
        Some("EDITOR\nFOOTER"),
        ScrollbarMode::Always,
        true,
        0,
    );
    assert_eq!(fx.core.scroll_offset(), 0);

    // The dock's last column (same column, row past the track): no grab, so
    // the press starts a selection and a drag/release copies the dock row.
    assert!(matches!(
        fx.route(left_press(11, 7)),
        AltMouseRoute::Repaint
    ));
    assert!(!fx.with_state(|state| state.scrollbar_dragging()));
    assert_eq!(fx.core.scroll_offset(), 0, "no jump");
    assert!(matches!(fx.route(left_drag(0, 7)), AltMouseRoute::Repaint));
    assert!(matches!(
        fx.route(left_release(0, 7)),
        AltMouseRoute::Repaint
    ));
    assert_eq!(fx.copied.borrow().as_slice(), ["FOOTER"]);
    assert!(fx.with_state(|state| state.selection.is_some()));

    // A press inside the transcript but away from the bar column is a
    // selection too.
    assert!(matches!(fx.route(left_press(4, 2)), AltMouseRoute::Repaint));
    assert!(!fx.with_state(|state| state.scrollbar_dragging()));
    assert_eq!(fx.core.scroll_offset(), 0);
}

/// Upstream `hasOverlay()` (and pir's `transcript_mouse_route` gate): while a
/// dialog/selector owns the focus the bar cannot be grabbed or hovered and the
/// search buttons do not react.
#[test]
fn overlay_focus_blocks_the_scrollbar_and_the_search_buttons() {
    use crossterm::event::{MouseButton, MouseEventKind};
    use pir::tui::altscreen_search::{AltMouseRoute, ScrollbarMode};

    let mut fx = MouseFixture::new(
        80,
        12,
        20,
        Some("EDITOR\nFOOTER"),
        ScrollbarMode::Always,
        false,
        3,
    );
    fx.with_state(|state| state.set_overlay_focus(true));
    let before = fx.core.scroll_offset();

    assert!(matches!(fx.route(left_press(79, 3)), AltMouseRoute::Ignore));
    assert!(!fx.with_state(|state| state.scrollbar_dragging()));
    assert_eq!(fx.core.scroll_offset(), before, "no scrub under an overlay");
    assert!(matches!(fx.route(moved(79, 3)), AltMouseRoute::Ignore));
    assert!(
        !fx.with_state(|state| state.scrollbar_hover()),
        "the overlay clears the hover"
    );

    // The search box's buttons are inert too.
    let mut fx2 = MouseFixture::new(
        80,
        12,
        20,
        Some("EDITOR\nFOOTER"),
        ScrollbarMode::Always,
        false,
        0,
    );
    let search_modifiers = if pir::config::keybindings::use_windows_keybindings() {
        crossterm::event::KeyModifiers::CONTROL
    } else {
        crossterm::event::KeyModifiers::CONTROL | crossterm::event::KeyModifiers::SHIFT
    };
    let mut state = fx2
        .state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    pir::tui::altscreen_search::route_alt_screen_key(
        &crossterm::event::KeyEvent::new(crossterm::event::KeyCode::Char('f'), search_modifiers),
        &mut fx2.core,
        &mut state,
        2,
    );
    state.search.query = "line".to_string();
    state.search.matches = vec![pir::tui::altscreen_search::SearchMatch {
        segments: vec![pir::tui::altscreen_search::SearchSegment {
            row: 0,
            start_col: 2,
            end_col: 6,
        }],
    }];
    state.search.selected = None;
    state.set_overlay_focus(true);
    drop(state);
    let layout = pir::tui::altscreen_search::search_box_layout(80).expect("a wide box");
    let next_column = (layout.next.0 + 1) as u16;
    assert!(matches!(
        fx2.route(left_press(next_column, 3)),
        AltMouseRoute::Ignore
    ));
    assert_eq!(
        fx2.with_state(|state| state.search.selected),
        None,
        "the overlay blocks the button click"
    );

    // The event loop's own gate: only the wheel passes an open overlay.
    assert!(!pir::tui::terminal::transcript_mouse_route(
        MouseEventKind::Down(MouseButton::Left),
        true,
        false
    ));
    assert!(pir::tui::terminal::transcript_mouse_route(
        MouseEventKind::ScrollUp,
        true,
        true
    ));
}

// --- Hover-active thumb (upstream `updateScrollbarHover` / `setScrollbarHover`) ---

/// Upstream "reveals an auto scrollbar when the pointer enters its hidden
/// track" + the `█` active glyph: a hidden `"auto"` bar has no painted cells
/// and no press target, yet merely moving the pointer over its track reveals
/// it and paints the active thumb; it stays up while the pointer is on the
/// track (`markScrollbarActivity` installs no hide timer while
/// `scrollbarActive`) and fades once the pointer leaves and the linger
/// expires.
#[test]
fn scrollbar_hover_paints_the_active_thumb_and_reveals_a_hidden_auto_bar() {
    use pir::tui::altscreen_search::{AltMouseRoute, ScrollbarMode};

    let mut fx = MouseFixture::new(10, 10, 50, None, ScrollbarMode::Auto, false, 0);
    let bar_glyph = |line: &str| line.ends_with(['\u{2502}', '\u{2503}', '\u{2588}']);
    let bars = |fx: &MouseFixture| {
        fx.screen()
            .into_iter()
            .filter(|line| bar_glyph(line))
            .count()
    };

    // At live follow with no scroll gesture the transient flag is unset: the
    // hidden auto bar paints nothing.
    assert_eq!(
        bars(&fx),
        0,
        "a fresh view shows no auto bar: {:?}",
        fx.screen()
    );

    // Entering the track reveals it: hover + linger, repaint requested.
    assert!(matches!(fx.route(moved(9, 3)), AltMouseRoute::Repaint));
    assert!(fx.with_state(|state| state.scrollbar_hover()));
    fx.render();
    let painted = fx.screen();
    // Following: scrollTop == maxScrollTop → the thumb covers the last two
    // track rows (8, 9) and the active glyph is the full block; the rest is
    // the light track.
    assert!(
        painted[8].ends_with('\u{2588}') && painted[9].ends_with('\u{2588}'),
        "the hovered thumb paints the active block: {painted:?}"
    );
    assert!(
        painted[0].ends_with('\u{2502}'),
        "the track above the thumb is the light rule: {:?}",
        painted[0]
    );

    // While the pointer stays on the track the linger cannot expire.
    fx.with_state(|state| {
        state.tick(std::time::Instant::now() + std::time::Duration::from_secs(60))
    });
    fx.render();
    assert!(bars(&fx) > 0, "an active bar has no hide timer");

    // Leaving the track clears the active state; the linger expires and the
    // bar paints nothing again (upstream: the hide delay after the pointer
    // leaves).
    assert!(matches!(fx.route(moved(5, 3)), AltMouseRoute::Repaint));
    assert!(!fx.with_state(|state| state.scrollbar_hover()));
    fx.with_state(|state| {
        state.tick(
            std::time::Instant::now()
                + pir::tui::altscreen_search::SCROLLBAR_LINGER
                + std::time::Duration::from_millis(1),
        )
    });
    fx.render();
    assert_eq!(bars(&fx), 0, "the linger expired: {:?}", fx.screen());

    // `"always"` ignores the transient flag but still switches the glyph on
    // hover.
    let mut always = MouseFixture::new(10, 10, 50, None, ScrollbarMode::Always, false, 0);
    always.render();
    assert!(always.screen()[8].ends_with('\u{2503}'), "idle thumb");
    assert!(matches!(always.route(moved(9, 8)), AltMouseRoute::Repaint));
    always.render();
    assert!(always.screen()[8].ends_with('\u{2588}'), "hovered thumb");
}

// --- Jump-to-end indicator (upstream `compositeScrollToEndIndicator`) ---

/// Upstream "shows a clickable jump-to-end indicator on the transcript's last
/// row while scrolled up": the label renders only while the view is away from
/// the end, a press inside its rect scrolls to the bottom (resuming follow),
/// and a press next to it stays a selection.
#[test]
fn jump_to_end_indicator_renders_only_when_scrolled_away_and_click_scrolls_to_bottom() {
    use pir::tui::altscreen_search::{clip_geometry, AltMouseRoute, ScrollbarMode};

    let mut fx = MouseFixture::new(
        80,
        12,
        20,
        Some("EDITOR\nFOOTER"),
        ScrollbarMode::Always,
        false,
        3,
    );
    let label = pir::tui::altscreen_search::scroll_to_end_indicator_label();
    let label_width = pir::tui::ansi::visible_width(&label);
    assert!(label.contains("Jump to latest message"), "label: {label:?}");
    assert!(label.contains("Ctrl+End"), "the bottom shortcut: {label:?}");

    // Scrolled away: the indicator sits on the transcript clip's last screen
    // row, centered, clamped at the scrollbar's column.
    let clip = clip_geometry(&fx.core.painted_layout());
    assert_eq!(clip.clip_height, 10, "12 rows - the 2-row dock");
    fx.render();
    let painted = fx.screen();
    let row = clip.clip_height - 1;
    assert!(
        painted[row].contains("Jump to latest message"),
        "row {row} carries the label: {:?}",
        painted[row]
    );
    let rect = fx
        .with_state(|state| state.scroll_to_end_rect())
        .expect("the label rect is recorded");
    assert_eq!(rect.row, row);
    assert_eq!(rect.column, (80 - label_width) / 2);
    assert_eq!(rect.width, label_width);
    assert!(
        rect.column + rect.width <= 79,
        "the label stops at the scrollbar column"
    );

    // A press inside the label scrolls to the bottom and resumes follow.
    let click_column = (rect.column + rect.width / 2) as u16;
    assert!(matches!(
        fx.route(left_press(click_column, row as u16)),
        AltMouseRoute::Repaint
    ));
    assert_eq!(fx.core.scroll_offset(), 0);
    assert!(fx.core.is_following(), "scrollToBottom resumes follow");
    assert!(matches!(
        fx.route(left_release(click_column, row as u16)),
        AltMouseRoute::Ignore
    ));
    fx.render();
    assert!(
        !fx.screen()
            .iter()
            .any(|line| line.contains("Jump to latest message")),
        "the indicator is gone while following"
    );
    assert!(fx.with_state(|state| state.scroll_to_end_rect()).is_none());

    // A press NEXT TO the label starts a selection instead (upstream's
    // "pressing next to the label starts a selection instead of jumping").
    fx.core.set_scroll_offset(3);
    fx.core.render();
    assert!(matches!(
        fx.route(left_press(5, row as u16)),
        AltMouseRoute::Repaint
    ));
    assert_eq!(fx.core.scroll_offset(), 3, "the click missed the label");
    assert!(matches!(
        fx.route(left_drag(12, row as u16)),
        AltMouseRoute::Repaint
    ));
    assert!(matches!(
        fx.route(left_release(12, row as u16)),
        AltMouseRoute::Repaint
    ));
    assert!(fx.with_state(|state| state.selection.is_some()));
}

/// The follow path hands the decorator `scroll_offset: 0` whether or not the
/// view is following, so a search reveal's `scrollTo(disableFollow)` pin is
/// mirrored into the state: the indicator shows while the pinned end is held,
/// and clicking it resumes follow.
#[test]
fn jump_to_end_indicator_shows_for_a_search_pinned_end() {
    use pir::tui::altscreen_search::{AltMouseRoute, ScrollbarMode};

    let mut fx = MouseFixture::new(
        80,
        12,
        20,
        Some("EDITOR\nFOOTER"),
        ScrollbarMode::Auto,
        false,
        0,
    );
    let search_modifiers = if pir::config::keybindings::use_windows_keybindings() {
        crossterm::event::KeyModifiers::CONTROL
    } else {
        crossterm::event::KeyModifiers::CONTROL | crossterm::event::KeyModifiers::SHIFT
    };
    {
        let mut state = fx
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        pir::tui::altscreen_search::route_alt_screen_key(
            &crossterm::event::KeyEvent::new(
                crossterm::event::KeyCode::Char('f'),
                search_modifiers,
            ),
            &mut fx.core,
            &mut state,
            2,
        );
        for ch in "line 20".chars() {
            pir::tui::altscreen_search::route_alt_screen_key(
                &crossterm::event::KeyEvent::new(
                    crossterm::event::KeyCode::Char(ch),
                    crossterm::event::KeyModifiers::NONE,
                ),
                &mut fx.core,
                &mut state,
                2,
            );
        }
    }
    assert_eq!(fx.core.scroll_offset(), 0);
    assert!(
        !fx.core.is_following(),
        "the reveal pins the end (disableFollow)"
    );

    fx.render();
    let rect = fx
        .with_state(|state| state.scroll_to_end_rect())
        .expect("the pinned end shows the indicator");
    assert_eq!(
        rect.row, 9,
        "the rect is a SCREEN row: the pin invalidates the frame (the ctx \
         `visible_top` is zeroed for the full repaint), the decorator \
         recovers the bottom anchor"
    );
    assert!(
        fx.screen()[rect.row].contains("Jump to latest message"),
        "the label paints on the screen's last transcript row: {:?}",
        fx.screen()
    );
    assert!(matches!(
        fx.route(left_press((rect.column + 1) as u16, rect.row as u16)),
        AltMouseRoute::Repaint
    ));
    assert!(fx.core.is_following(), "the click resumes follow");
    fx.render();
    assert!(fx.with_state(|state| state.scroll_to_end_rect()).is_none());
}

// --- Keybinding-backed labels (upstream `keyDisplayText` / `formatKey`) ---

/// The jump-to-end indicator and the search buttons must carry the RESOLVED
/// `tui.altScreen.*` chords (upstream `tui-renderer.ts` `keyDisplayText(...)`
/// and the search component's `formatKey(getKeys(id)[0])`). pir's frozen
/// registries omit the viewport family, so the upstream-verbatim rows live in
/// the alt-screen module; this pins them and the labels they produce.
#[test]
fn indicator_and_search_button_labels_resolve_the_upstream_chords() {
    use pir::tui::altscreen_search::{
        alt_screen_default_keys, scroll_to_end_indicator_label, scroll_to_end_indicator_text,
        search_box_layout,
    };

    assert_eq!(
        alt_screen_default_keys("tui.altScreen.bottom"),
        Some(["ctrl+end"].as_slice())
    );
    assert_eq!(
        alt_screen_default_keys("tui.altScreen.searchNext"),
        Some(["enter", "ctrl+g"].as_slice())
    );
    assert_eq!(
        alt_screen_default_keys("tui.altScreen.searchPrevious"),
        Some(["shift+enter", "ctrl+shift+g"].as_slice())
    );
    assert_eq!(
        alt_screen_default_keys("tui.altScreen.top"),
        Some(["ctrl+home"].as_slice())
    );
    assert_eq!(
        alt_screen_default_keys("tui.altScreen.searchClose"),
        Some(["escape"].as_slice())
    );
    assert_eq!(alt_screen_default_keys("tui.editor.undo"), None);

    // `formatKeys(..., { capitalize: true })` of `ctrl+end`.
    assert_eq!(
        scroll_to_end_indicator_text(),
        " \u{2193} Jump to latest message \u{b7} Ctrl+End "
    );
    assert!(scroll_to_end_indicator_label().contains("Ctrl+End"));

    // The component's `formatKey` of the first chord of each id.
    let layout = search_box_layout(80).expect("a wide box");
    assert_eq!(
        layout.previous_label.as_deref(),
        Some("\u{2191} Shift+Enter")
    );
    assert_eq!(layout.next_label.as_deref(), Some("\u{2193} Enter"));
}

/// The follow path's screen top (`follow_screen_top`): a taller frame is
/// bottom-anchored, and `TuiCore::invalidate_frame` — which every return from a
/// scrolled view runs — zeroes the ctx's `visible_top` for one full repaint, so
/// the decorator falls back to `frame_len - screen_rows` using the height the
/// interaction layer last saw. `0` still means "unknown" (the first frame) and
/// a short frame stays top-anchored.
#[test]
fn follow_screen_top_recovers_the_bottom_anchor_after_an_invalidate() {
    use pir::tui::altscreen_search::follow_screen_top;

    assert_eq!(follow_screen_top(10, 22, 12), 10, "a fresh ctx value");
    assert_eq!(
        follow_screen_top(0, 22, 12),
        10,
        "invalidated: the taller frame is bottom-anchored"
    );
    assert_eq!(
        follow_screen_top(0, 22, 0),
        0,
        "unknown height keeps today's behavior"
    );
    assert_eq!(
        follow_screen_top(0, 5, 12),
        0,
        "a short frame is top-anchored"
    );
    assert_eq!(follow_screen_top(3, 5, 12), 3, "a ctx value always wins");
}

// --- Windows right-click paste (upstream `handleRightClickPaste`) ---

/// The gate is upstream's exact condition set (`win32`, not VS Code, a bare
/// secondary-button PRESS), and the router reports the paste route for it
/// (the event loop injects the clipboard; see `AltMouseRoute::PasteClipboard`).
#[test]
fn windows_right_click_paste_gate_and_route() {
    use crossterm::event::{KeyModifiers, MouseButton, MouseEventKind};
    use pir::tui::altscreen_search::{
        is_right_click_paste, right_click_paste_enabled, AltMouseRoute, ScrollbarMode,
    };

    let down_right = mouse_event(MouseEventKind::Down(MouseButton::Right), 4, 4);
    let up_right = mouse_event(MouseEventKind::Up(MouseButton::Right), 4, 4);
    let down_left = mouse_event(MouseEventKind::Down(MouseButton::Left), 4, 4);
    assert!(is_right_click_paste(&down_right, true, None));
    assert!(is_right_click_paste(
        &down_right,
        true,
        Some("Windows Terminal")
    ));
    assert!(
        !is_right_click_paste(&down_right, true, Some("vscode")),
        "VS Code's integrated terminal is excluded"
    );
    assert!(!is_right_click_paste(&down_right, true, Some("VSCODE")));
    assert!(
        !is_right_click_paste(&down_right, false, None),
        "Windows only"
    );
    assert!(!is_right_click_paste(&up_right, true, None), "press only");
    assert!(
        !is_right_click_paste(&down_left, true, None),
        "secondary only"
    );
    let mut modified = down_right;
    modified.modifiers = KeyModifiers::CONTROL;
    assert!(
        !is_right_click_paste(&modified, true, None),
        "a modified button is a different SGR code (upstream `button !== 2`)"
    );
    assert!(right_click_paste_enabled(true, None));
    assert!(!right_click_paste_enabled(false, None));

    // The router wires the same gate: on a Windows host (and outside VS
    // Code) the right press yields the paste route; elsewhere it stays inert
    // with no repaint.
    let mut fx = MouseFixture::new(20, 6, 30, None, ScrollbarMode::Auto, false, 0);
    let expected = is_right_click_paste(
        &down_right,
        cfg!(windows),
        std::env::var("TERM_PROGRAM").ok().as_deref(),
    );
    let route = fx.route(down_right);
    if expected {
        assert_eq!(route, AltMouseRoute::PasteClipboard);
    } else {
        assert_eq!(route, AltMouseRoute::Ignore);
    }
    assert!(
        fx.with_state(|state| state.selection.is_none()),
        "a right press never starts a selection"
    );
}

// --- Search box navigation buttons (upstream `searchNavigationButtonStyle`) ---

/// Upstream `AltScreenSearchComponent.render`: the `↑`/`↓` affordances sit on
/// the box's bottom rule, hover underlines them, and a left press navigates
/// exactly like the keyboard chords (`enter` / `ctrl+g` and `shift+enter` /
/// `ctrl+shift+g`).
#[test]
fn search_navigation_buttons_hover_underline_and_click() {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use pir::tui::altscreen_search::{
        search_box_layout, search_navigation_direction_at, AltMouseRoute, ScrollbarMode, SearchNav,
    };

    // The pure layout: 80 columns → the upstream 40% box (32 wide) at the
    // top-right with a 1-cell margin, buttons on the bottom rule.
    let layout = search_box_layout(80).expect("a wide box");
    assert_eq!(
        (layout.top, layout.left, layout.width, layout.inner),
        (1, 47, 32, 30)
    );
    assert_eq!(
        layout.previous_label.as_deref(),
        Some("\u{2191} Shift+Enter")
    );
    assert_eq!(layout.next_label.as_deref(), Some("\u{2193} Enter"));
    assert_eq!(layout.previous, (53, 66));
    assert_eq!(layout.next, (69, 76));
    assert_eq!(search_navigation_direction_at(&layout, 2, 53), None);
    assert_eq!(
        search_navigation_direction_at(&layout, 3, 60),
        Some(SearchNav::Previous)
    );
    assert_eq!(
        search_navigation_direction_at(&layout, 3, 70),
        Some(SearchNav::Next)
    );
    assert_eq!(search_navigation_direction_at(&layout, 3, 30), None);
    assert_eq!(search_navigation_direction_at(&layout, 4, 60), None);
    // A narrow box falls back to the bare arrows (upstream's short labels).
    let narrow = search_box_layout(10).expect("the minimum width");
    assert_eq!((narrow.top, narrow.left, narrow.width), (1, 1, 8));
    assert_eq!(narrow.previous_label.as_deref(), Some("\u{2191}"));
    assert_eq!(narrow.next_label.as_deref(), Some("\u{2193}"));
    assert_eq!(narrow.previous, (3, 4));
    assert_eq!(narrow.next, (5, 6));

    // A real box: open the search, type a query, render, hover and click.
    let mut fx = MouseFixture::new(
        80,
        12,
        20,
        Some("EDITOR\nFOOTER"),
        ScrollbarMode::Auto,
        false,
        0,
    );
    let search_modifiers = if pir::config::keybindings::use_windows_keybindings() {
        KeyModifiers::CONTROL
    } else {
        KeyModifiers::CONTROL | KeyModifiers::SHIFT
    };
    {
        let mut state = fx
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        pir::tui::altscreen_search::route_alt_screen_key(
            &KeyEvent::new(KeyCode::Char('f'), search_modifiers),
            &mut fx.core,
            &mut state,
            2,
        );
        for ch in "line".chars() {
            pir::tui::altscreen_search::route_alt_screen_key(
                &KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE),
                &mut fx.core,
                &mut state,
                2,
            );
        }
        assert_eq!(state.search.matches.len(), 20, "every transcript row");
    }
    fx.render();
    let painted = fx.screen();
    assert!(
        painted[1].contains(&"\u{2500}".to_string().repeat(30)),
        "the top rule: {:?}",
        painted[1]
    );
    assert!(painted[1].trim_end().ends_with('\u{2510}'));
    assert!(painted[1].contains('\u{250c}'));
    assert!(
        painted[3].contains("\u{2191} Shift+Enter \u{b7} \u{2193} Enter"),
        "the bottom rule carries both buttons: {:?}",
        painted[3]
    );
    assert!(painted[3].trim_end().ends_with('\u{2518}'));
    assert!(
        painted[2].contains("line"),
        "the query row: {:?}",
        painted[2]
    );
    assert!(
        painted[2].contains("/20"),
        "the result count: {:?}",
        painted[2]
    );

    // Hovering the previous button underlines it (upstream
    // `searchNavigationButtonStyle(text, hovered)` → `theme.underline`).
    assert!(matches!(fx.route(moved(60, 3)), AltMouseRoute::Repaint));
    assert_eq!(
        fx.with_state(|state| state.search.hovered_nav),
        Some(SearchNav::Previous)
    );
    fx.render();
    let hovered = fx.screen();
    assert!(
        hovered[3].contains("\u{2191} Shift+Enter"),
        "the label survives the styling"
    );
    let raw_hovered = &fx.core.last_screen_lines()[3];
    assert!(
        raw_hovered.contains("\x1b[4m\u{2191} Shift+Enter\x1b[24m"),
        "the hovered button is underlined: {raw_hovered:?}"
    );

    // Clicking the next button walks the match list exactly like `ctrl+g`:
    // one full rotation of the 20 matches returns to where it started.
    let start = fx
        .with_state(|state| state.search.selected)
        .expect("typing targets a current match");
    assert!(matches!(
        fx.route(left_press(72, 3)),
        AltMouseRoute::Repaint
    ));
    let after_one = fx.with_state(|state| state.search.selected);
    assert_eq!(after_one, Some((start + 1) % 20), "the next button");
    for _ in 0..19 {
        assert!(matches!(
            fx.route(left_press(72, 3)),
            AltMouseRoute::Repaint
        ));
    }
    assert_eq!(
        fx.with_state(|state| state.search.selected),
        Some(start),
        "a full mouse rotation wraps back"
    );
    // The previous button steps back (and wraps from the first match).
    assert!(matches!(
        fx.route(left_press(60, 3)),
        AltMouseRoute::Repaint
    ));
    assert_eq!(
        fx.with_state(|state| state.search.selected),
        Some((start + 19) % 20)
    );

    // The keyboard chords navigate the same list (upstream `searchNext` /
    // `searchPrevious`: ctrl+g / ctrl+shift+g).
    {
        let mut state = fx
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        pir::tui::altscreen_search::route_alt_screen_key(
            &KeyEvent::new(KeyCode::Char('g'), KeyModifiers::CONTROL),
            &mut fx.core,
            &mut state,
            2,
        );
        assert_eq!(state.search.selected, Some(start), "ctrl+g = next");
        pir::tui::altscreen_search::route_alt_screen_key(
            &KeyEvent::new(
                KeyCode::Char('g'),
                KeyModifiers::CONTROL | KeyModifiers::SHIFT,
            ),
            &mut fx.core,
            &mut state,
            2,
        );
        assert_eq!(
            state.search.selected,
            Some((start + 19) % 20),
            "ctrl+shift+g = previous"
        );
    }

    // A press on the box's bottom rule but NOT on a button is not a
    // navigation: it falls through to the selection gesture (upstream's
    // overlay-hit dispatch).
    let before_off_button = fx.with_state(|state| state.search.selected);
    assert!(matches!(
        fx.route(left_press(49, 3)),
        AltMouseRoute::Repaint
    ));
    assert_eq!(
        fx.with_state(|state| state.search.selected),
        before_off_button,
        "no navigation off the buttons"
    );
    assert!(matches!(fx.route(left_drag(14, 3)), AltMouseRoute::Repaint));
    assert!(fx.with_state(|state| state.selection.is_some()));

    // Leaving the box clears the hover.
    assert!(matches!(fx.route(moved(5, 3)), AltMouseRoute::Ignore));
    assert_eq!(fx.with_state(|state| state.search.hovered_nav), None);
}

// ---------------------------------------------------------------------------
// OSC 8 hyperlink clicks (upstream `utils.ts::getOsc8LinkAtColumn` +
// `tui-alt-screen.ts::handleSelectionMouseEvent`'s `openUrl`)
// ---------------------------------------------------------------------------

/// The three URLs of the upstream OSC 8 fixture (`tui-alt-screen.test.ts`
/// "opens an OSC 8 hyperlink with specific or generic release codes, but not
/// on drag"): an ST-terminated link, a BEL-terminated link and an emoji link.
const OSC8_URL: &str = "https://example.com/path?q=1";
const OSC8_BEL_URL: &str = "https://example.com/bel";
const OSC8_EMOJI_URL: &str = "https://example.com/emoji";

/// A 20x3 fullscreen core carrying the upstream OSC 8 fixture rows: row 0 and
/// row 1 spell "link" (ST- and BEL-terminated runs), row 2 a two-cell emoji
/// link, and column 10+ of every row is unlinked padding.
struct Osc8Fixture {
    core: pir::tui::core::TuiCore,
    state: AltScreenState,
    config: pir::tui::altscreen_search::AltScreenConfig,
    wheel: WheelScrollAccelerator,
    copied: std::rc::Rc<std::cell::RefCell<Vec<String>>>,
    /// Upstream's `openedUrls`: every URL the router asked the caller to open.
    opened: Vec<String>,
}

impl Osc8Fixture {
    fn new() -> Self {
        use pir::tui::altscreen_search::{AltScreenConfig, ScrollbarMode};
        use pir::tui::core::{Terminal, Text, TuiCore};

        let terminal = Terminal::new();
        terminal.set_size(20, 3);
        let mut core = TuiCore::new(terminal);
        let transcript = format!(
            "\x1b]8;;{OSC8_URL}\x1b\\link\x1b]8;;\x1b\\\n\
             \x1b]8;;{OSC8_BEL_URL}\x07link\x1b]8;;\x07\n\
             \x1b]8;;{OSC8_EMOJI_URL}\x1b\\\u{1f642}\x1b]8;;\x1b\\"
        );
        core.push_content(Box::new(Text::with_padding(transcript, 0, 0)));
        core.render();
        Self {
            core,
            state: AltScreenState::default(),
            config: AltScreenConfig {
                scrollbar: ScrollbarMode::Auto,
                // Upstream's default: a URL click must still not copy.
                copy_on_select: true,
            },
            wheel: WheelScrollAccelerator::with_accelerate(WheelScrollLines::Lines(1), true),
            copied: std::rc::Rc::new(std::cell::RefCell::new(Vec::new())),
            opened: Vec::new(),
        }
    }

    /// Route one mouse event, recording the URL side effect like upstream's
    /// `openUrl: (url) => openedUrls.push(url)`.
    fn route(
        &mut self,
        event: crossterm::event::MouseEvent,
    ) -> pir::tui::altscreen_search::AltMouseRoute {
        use pir::tui::altscreen_search::{handle_mouse_event_route, AltMouseRoute};

        let copied = std::rc::Rc::clone(&self.copied);
        let copy = move |text: &str| {
            copied.borrow_mut().push(text.to_string());
            true
        };
        let route = handle_mouse_event_route(
            event,
            &mut self.core,
            &mut self.state,
            &self.config,
            &mut self.wheel,
            0,
            &copy,
        );
        if let AltMouseRoute::OpenUrl(url) = &route {
            self.opened.push(url.clone());
        }
        route
    }
}

/// Upstream's OSC 8 walk (`utils.ts::getOsc8LinkAtColumn`): the link covers
/// exactly the cells between its open and close sequences, a close ends it,
/// SGR and other escapes consume no columns, tabs count three cells, wide
/// clusters cover two, and a non-OSC-8 or unterminated sequence is plain text.
#[test]
fn osc8_link_at_column_matches_upstream_walks() {
    use pir::tui::altscreen_search::osc8_link_at_column;

    // ST-terminated run: "link" covers columns 0-3; the close sequence ends
    // the link, so the padding after it is unlinked.
    let st = format!("\x1b]8;;{OSC8_URL}\x1b\\link\x1b]8;;\x1b\\");
    assert_eq!(osc8_link_at_column(&st, 0).as_deref(), Some(OSC8_URL));
    assert_eq!(osc8_link_at_column(&st, 3).as_deref(), Some(OSC8_URL));
    assert_eq!(osc8_link_at_column(&st, 4), None);

    // BEL terminator (the second upstream fixture row).
    let bel = format!("\x1b]8;;{OSC8_BEL_URL}\x07link\x1b]8;;\x07");
    assert_eq!(osc8_link_at_column(&bel, 1).as_deref(), Some(OSC8_BEL_URL));

    // Params before the URL (`\x1b]8;id=1;<url>`), and a close followed by
    // more text: the text after the close is unlinked.
    assert_eq!(
        osc8_link_at_column("\x1b]8;id=1;https://x\x07a\x1b]8;;\x07", 0).as_deref(),
        Some("https://x")
    );
    assert_eq!(
        osc8_link_at_column("\x1b]8;;u\x07ab\x1b]8;;\x07c", 1).as_deref(),
        Some("u")
    );
    assert_eq!(osc8_link_at_column("\x1b]8;;u\x07ab\x1b]8;;\x07c", 2), None);

    // Non-OSC-8 escapes consume no columns: the SGR pair around "ab" leaves
    // the link active on both cells, and a title OSC is not an OSC 8 (it
    // must not clobber the active URL either).
    assert_eq!(
        osc8_link_at_column("\x1b]8;;u\x07\x1b[31mab\x1b[0m\x1b]8;;\x07", 1).as_deref(),
        Some("u")
    );
    assert_eq!(
        osc8_link_at_column("\x1b]8;;real\x07a\x1b]0;title\x07b", 1).as_deref(),
        Some("real")
    );
    // The split is at the FIRST `;` after `8;` and the URL group is
    // `[^\x07\x1b]*` — semicolons INCLUDED. Upstream's regex
    // (`utils.ts:366`, probed with node): `ESC]8;a=b;c;real BEL` activates
    // `c;real` (params `a=b`), and `ESC]8;a;b;c BEL` activates `b;c` — a later
    // code simply replaces the active URL.
    assert_eq!(
        osc8_link_at_column("\x1b]8;a=b;c;real\x07link", 0).as_deref(),
        Some("c;real")
    );
    assert_eq!(
        osc8_link_at_column("\x1b]8;;real\x07a\x1b]8;a;b;c\x07b", 1).as_deref(),
        Some("b;c")
    );
    // An `8;` code with no second `;` matches nothing: the active URL stays.
    assert_eq!(
        osc8_link_at_column("\x1b]8;;real\x07a\x1b]8;x\x07b", 1).as_deref(),
        Some("real")
    );

    // A tab occupies three cells; a wide (emoji) cluster covers two.
    assert_eq!(
        osc8_link_at_column("\x1b]8;;t\x07\t\x1b]8;;\x07", 2).as_deref(),
        Some("t")
    );
    assert_eq!(osc8_link_at_column("\x1b]8;;t\x07\t\x1b]8;;\x07", 3), None);
    let emoji = "\x1b]8;;e\x1b\\\u{1f642}\x1b]8;;\x1b\\";
    assert_eq!(osc8_link_at_column(emoji, 0).as_deref(), Some("e"));
    assert_eq!(osc8_link_at_column(emoji, 1).as_deref(), Some("e"));
    assert_eq!(osc8_link_at_column(emoji, 2), None);

    // Unterminated OSC and plain text: no active link, no panic.
    assert_eq!(osc8_link_at_column("\x1b]8;;u", 0), None);
    assert_eq!(osc8_link_at_column("plain", 4), None);
    assert_eq!(osc8_link_at_column("", 0), None);
}

/// The click gate and the route, on Upstream's fixture: a left click released
/// on the same cell as its press opens the link under that cell; a drag, a
/// non-left button, the wheel, an unlinked cell and the absence of a link all
/// fall through to the existing behavior (selection / no-op) without opening
/// anything.
#[test]
fn osc8_hyperlink_click_opens_and_a_drag_does_not() {
    use crossterm::event::{KeyModifiers, MouseButton, MouseEventKind};
    use pir::tui::altscreen_search::AltMouseRoute;

    let mut fx = Osc8Fixture::new();
    // The fixture frame really carries the runs (a renderer regression would
    // otherwise make every assertion below trivially "no link").
    assert!(
        fx.core.last_frame_lines()[0].contains(&format!("\x1b]8;;{OSC8_URL}\x1b\\")),
        "row 0 carries the ST link: {:?}",
        fx.core.last_frame_lines()[0]
    );
    assert!(fx.core.last_frame_lines()[1].contains("\x1b]8;;\x07"));

    // Row 0: press and release on the same linked cell open the URL. (Both
    // the specific release code `0;…m` and the generic `3;…m` normalize to
    // `Up(Left)` in crossterm, so the router sees one shape.)
    assert!(matches!(fx.route(left_press(1, 0)), AltMouseRoute::Repaint));
    assert_eq!(fx.state.pressed_url(), Some(OSC8_URL));
    assert_eq!(
        fx.route(left_release(1, 0)),
        AltMouseRoute::OpenUrl(OSC8_URL.to_string())
    );
    assert_eq!(fx.opened, [OSC8_URL.to_string()]);
    assert!(
        fx.copied.borrow().is_empty(),
        "a URL click is consumed: no copy-on-select"
    );
    assert!(fx.state.selection.is_none(), "and no selection is kept");

    // Row 1: the BEL-terminated run opens just the same.
    assert!(matches!(fx.route(left_press(1, 1)), AltMouseRoute::Repaint));
    assert_eq!(
        fx.route(left_release(1, 1)),
        AltMouseRoute::OpenUrl(OSC8_BEL_URL.to_string())
    );

    // Row 2: the emoji is ONE two-cell cluster, so column 1 is over the link.
    assert!(matches!(fx.route(left_press(1, 2)), AltMouseRoute::Repaint));
    assert_eq!(
        fx.route(left_release(1, 2)),
        AltMouseRoute::OpenUrl(OSC8_EMOJI_URL.to_string())
    );
    assert_eq!(
        fx.opened,
        [
            OSC8_URL.to_string(),
            OSC8_BEL_URL.to_string(),
            OSC8_EMOJI_URL.to_string()
        ]
    );

    // A drag drops the remembered link (upstream's motion branch clears
    // `pressedUrl`): the gesture stays a selection and copies on release.
    assert!(matches!(fx.route(left_press(0, 0)), AltMouseRoute::Repaint));
    assert!(matches!(fx.route(left_drag(4, 0)), AltMouseRoute::Repaint));
    assert_eq!(fx.state.pressed_url(), None);
    assert!(matches!(
        fx.route(left_release(4, 0)),
        AltMouseRoute::Repaint
    ));
    assert_eq!(fx.opened.len(), 3, "a drag never opens the link");
    assert_eq!(
        fx.copied.borrow().as_slice(),
        ["link"],
        "the drag still copies the dragged text"
    );

    // Modifiers do not gate the click: upstream masks the SGR button with
    // `& 3`, so a Shift+left click is still button 0 (crossterm reports the
    // modifier bits separately, and the router ignores them here).
    let mut shift_press = left_press(1, 0);
    shift_press.modifiers = KeyModifiers::SHIFT;
    let mut shift_release = left_release(1, 0);
    shift_release.modifiers = KeyModifiers::SHIFT;
    assert!(matches!(fx.route(shift_press), AltMouseRoute::Repaint));
    assert_eq!(
        fx.route(shift_release),
        AltMouseRoute::OpenUrl(OSC8_URL.to_string())
    );

    // A non-left button never opens: neither press nor release is the
    // `button & 3 === 0` family (the right button is also the Windows
    // paste route, so the middle button keeps this host-independent).
    assert!(!matches!(
        fx.route(mouse_event(MouseEventKind::Down(MouseButton::Middle), 1, 0)),
        AltMouseRoute::OpenUrl(_)
    ));
    assert!(!matches!(
        fx.route(mouse_event(MouseEventKind::Up(MouseButton::Middle), 1, 0)),
        AltMouseRoute::OpenUrl(_)
    ));
    // The wheel is routed before the selection family: a notch over a link
    // scrolls, it never opens.
    assert!(!matches!(
        fx.route(mouse_event(MouseEventKind::ScrollUp, 1, 0)),
        AltMouseRoute::OpenUrl(_)
    ));

    // An unlinked cell falls through to the selection gesture: the press
    // records no URL, the release is the plain-click no-op, and the existing
    // selection behavior is unchanged.
    assert!(matches!(
        fx.route(left_press(10, 0)),
        AltMouseRoute::Repaint
    ));
    assert_eq!(fx.state.pressed_url(), None, "padding carries no link");
    assert!(matches!(
        fx.route(left_release(10, 0)),
        AltMouseRoute::Ignore
    ));
    assert_eq!(
        fx.opened.len(),
        4,
        "only the four gated clicks opened a URL"
    );
    assert!(fx.state.selection.is_none());
}
