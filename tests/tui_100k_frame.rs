//! TUI performance bounds at the 100k-entry scale (no external benchmark
//! framework): the materialized transcript window, the painted frame, and the
//! search match collection all stay bounded, and an unchanged frame does no
//! per-row search hashing.

use std::sync::{Arc, Mutex};
use std::time::Instant;

use pir::tui::altscreen_search::{
    find_search_matches, refresh_search_with_hint, AltScreenState, SEARCH_MATCH_LIMIT,
};
use pir::tui::app::{ChatContainer, ChatEntry};
use pir::tui::core::{Component, Rows, Terminal, TerminalSink, TuiCore};
use pir::tui::markdown::Markdown;
use pir::tui::stdin_buffer::WriteLog;
use pir::tui::terminal::{
    regular_window_start, tail_window_start, window_payload_bytes, TranscriptWindow,
    TRANSCRIPT_TAIL_BYTES, TRANSCRIPT_WINDOW_MAX_BYTES, TRANSCRIPT_WINDOW_MAX_ENTRIES,
};

struct SharedChat(Arc<Mutex<ChatContainer>>);

impl Component for SharedChat {
    fn render(&self, width: usize) -> Vec<String> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .render(width)
    }

    fn render_into(&self, width: usize, out: &mut Rows<'_>) {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .render_into(width, out);
    }
}

fn test_core() -> (TuiCore, Arc<Mutex<ChatContainer>>) {
    let terminal = Terminal::with_sink_factory(
        || Box::new(std::io::sink()) as Box<dyn TerminalSink>,
        WriteLog::disabled(),
    );
    terminal.set_size(120, 40);
    let mut core = TuiCore::new(terminal);
    core.set_windowed_viewport(true);
    let chat = Arc::new(Mutex::new(ChatContainer::new()));
    core.push_content(Box::new(SharedChat(Arc::clone(&chat))));
    (core, chat)
}

/// 100k tiny entries must not materialize 100k blocks or a frame proportional
/// to them: `TranscriptWindow::sync` keeps the tail cap and the painted frame
/// stays a screen-window-sized buffer.
#[test]
fn hundred_thousand_entries_materialize_a_bounded_tail() {
    let entries: Vec<ChatEntry> = (0..100_000)
        .map(|index| ChatEntry::User(format!("row {index:06}")))
        .collect();
    let (mut core, chat) = test_core();
    let mut window = TranscriptWindow::new();
    let started = Instant::now();
    window.sync(&entries, &core);
    let band = window.slice(&entries).len();
    assert!(
        band <= TRANSCRIPT_WINDOW_MAX_ENTRIES,
        "the tail window must stay under the entry cap: {band}"
    );
    assert!(
        band > 0,
        "a 100k-entry transcript must still materialize a tail"
    );
    chat.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .set_entries(window.slice(&entries));
    core.render();
    let elapsed = started.elapsed();
    let (rows, bytes) = core.last_frame_stats();
    assert!(
        rows < 4 * 1024,
        "the 100k-entry frame must stay a window, not the transcript: {rows} rows"
    );
    assert!(
        bytes < TRANSCRIPT_WINDOW_MAX_BYTES + 4 * 1024 * 1024,
        "the frame payload must stay near the byte budget: {bytes} bytes"
    );
    // Measurement seam, not a timing gate: print the probe so a perf run can
    // read it with `--nocapture`.
    println!(
        "100k entries: band={band} frame_rows={rows} frame_bytes={bytes} \
         sync+render={elapsed:?}"
    );
}

/// Regular (main-screen) mode keeps ordinary transcripts whole but materializes
/// only a bounded tail past the entry cap; the start is stable until the tail
/// drifts a full window.
#[test]
fn regular_mode_window_caps_a_huge_transcript() {
    let entries: Vec<ChatEntry> = (0..100_000)
        .map(|index| ChatEntry::User(format!("row {index:06}")))
        .collect();
    let start = regular_window_start(&entries, 0);
    assert!(start > 0, "100k entries must not materialize whole");
    assert!(
        entries.len() - start <= 2 * TRANSCRIPT_WINDOW_MAX_ENTRIES,
        "the regular window must stay bounded: {} entries",
        entries.len() - start
    );
    assert_eq!(
        regular_window_start(&entries, start),
        start,
        "a settled regular window must not slide per frame"
    );
}

/// One-character queries cannot allocate a match object per character: the
/// collection stops at the documented cap.
#[test]
fn search_match_collection_is_capped() {
    let line = "x ".repeat(SEARCH_MATCH_LIMIT + 2_000);
    let matches = find_search_matches(&[line], "x");
    assert_eq!(
        matches.len(),
        SEARCH_MATCH_LIMIT,
        "the match scan must stop at SEARCH_MATCH_LIMIT"
    );
}

/// An unchanged frame (hint `None`) does no match scan at all; a changed row
/// outside the matches does not re-run the corpus scan either.
#[test]
fn unchanged_frame_skips_the_full_band_search_scan() {
    let mut state = AltScreenState::default();
    state.search.active = true;
    state.search.query = "needle".to_string();
    let frame = vec!["alpha needle".to_string(), "beta".to_string()];
    assert!(refresh_search_with_hint(
        &mut state,
        &frame,
        0,
        1,
        Some((0, 0))
    ));
    assert_eq!(state.search.matches.len(), 1);
    let runs = state.search.cache.match_runs();
    let rebuilds = state.search.cache.row_rebuilds();
    // Same content, newer frame version: no hashing, no scan.
    assert!(refresh_search_with_hint(&mut state, &frame, 0, 2, None));
    assert_eq!(state.search.cache.match_runs(), runs);
    assert_eq!(state.search.cache.row_rebuilds(), rebuilds);
    // Only row 1 is hinted; it is byte-identical, so it is not re-parsed and
    // the matches remain the cached set.
    assert!(refresh_search_with_hint(
        &mut state,
        &frame,
        0,
        3,
        Some((1, 1))
    ));
    assert_eq!(state.search.cache.match_runs(), runs);
    assert_eq!(state.search.cache.row_rebuilds(), rebuilds);
}

/// The byte budget binds even when that leaves fewer entries than the soft
/// minimum; a single entry larger than the whole budget still wins a
/// one-entry window rather than an empty one.
#[test]
fn tail_budget_binds_below_the_entry_minimum() {
    let entries: Vec<ChatEntry> = (0..80)
        .map(|index| ChatEntry::User(format!("entry {index} {}", "x".repeat(60 * 1024))))
        .collect();
    let start = tail_window_start(
        &entries,
        64,
        TRANSCRIPT_WINDOW_MAX_ENTRIES,
        TRANSCRIPT_TAIL_BYTES,
    );
    assert!(
        entries.len() - start < 64,
        "the byte budget must win over the 64-entry minimum: {} entries",
        entries.len() - start
    );
    assert!(
        window_payload_bytes(&entries, start, entries.len()) <= TRANSCRIPT_TAIL_BYTES,
        "the tail window must stay inside the byte budget"
    );

    let mut oversized = vec![ChatEntry::User("z".repeat(2 * TRANSCRIPT_TAIL_BYTES))];
    oversized.extend((0..3).map(|index| ChatEntry::User(format!("newer {index}"))));
    let start = tail_window_start(
        &oversized,
        64,
        TRANSCRIPT_WINDOW_MAX_ENTRIES,
        TRANSCRIPT_TAIL_BYTES,
    );
    assert_eq!(
        start, 1,
        "the oversized oldest entry stays out; the newer entries fit"
    );

    let only_oversized = vec![ChatEntry::User("z".repeat(2 * TRANSCRIPT_TAIL_BYTES))];
    let start = tail_window_start(
        &only_oversized,
        64,
        TRANSCRIPT_WINDOW_MAX_ENTRIES,
        TRANSCRIPT_TAIL_BYTES,
    );
    assert_eq!(start, 0, "the newest entry is always kept, even oversized");
}

/// The live delta fast path appends exactly the same body as the snapshot
/// path.
#[test]
fn growing_delta_appends_the_same_body() {
    let streamed = Markdown::new("", 0, 0);
    streamed.set_text_growing_delta("alpha\n\n");
    streamed.set_text_growing_delta("beta\n");
    assert_eq!(streamed.get_text(), "alpha\n\nbeta\n");
    streamed.set_text_growing_delta("");
    assert_eq!(streamed.get_text(), "alpha\n\nbeta\n");
}
