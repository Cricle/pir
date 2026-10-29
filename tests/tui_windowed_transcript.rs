//! P0.3 — the windowed transcript (huge sessions must not materialize the
//! whole history; memory AND repaint).
//!
//! Pins, end to end:
//!
//! 1. Opening a session with N blocks materializes only the TAIL budget
//!    ([`TranscriptWindow`], `TRANSCRIPT_TAIL_BYTES`): the window's entry
//!    slice is what reaches `ChatContainer::set_entries`, so no block outside
//!    it is ever built and no cached row exists for it.
//! 2. Scrolling up materializes older blocks ON DEMAND
//!    (`TRANSCRIPT_WINDOW_STEP_ENTRIES` per extension step) until the OLDEST
//!    entry is reachable, and the windowed follow frame paints only the
//!    visible window + dock (the `TuiCore::set_windowed_viewport` path reuses
//!    the `render_scrolled` machinery).
//! 3. Returning to the tail releases the blocks the view scrolled past: the
//!    components are dropped (frees their rows — measured as a net-negative
//!    live-bytes delta) and the corpus no longer contains their text.
//! 4. The steady windowed frame keeps the frame-reuse channels quiet
//!    (`ToolMemoWork` zero, `RowsWork::pool_growth` 0, bounded changed rows)
//!    and stays O(visible) in the painted baseline.
//! 5. The selection corpus still resolves the visible rows + the dock
//!    (`screen_row_to_buffer_row` + `selection_text` trimming).
//!
//! Budgets are deliberately small in the fixture (a few MiB of payload, a
//! 30-row terminal) so the same mechanics are exercised without a 100 MB
//! session file: the constants are compared against, not hardcoded.

use std::cell::Cell;
use std::sync::{Arc, Mutex};

use pir::tui::altscreen_search::{max_scroll_offset, selection_text, SelectionSpan};
use pir::tui::app::{ChatContainer, ChatEntry};
use pir::tui::core::{Component, FrameWork, Rows, RowsWork, Terminal, Text, TuiCore};
use pir::tui::messages::ToolMemoWork;
use pir::tui::terminal::{
    advance_window, entry_payload_bytes, tail_window_start, window_payload_bytes, TranscriptWindow,
    TRANSCRIPT_TAIL_BYTES, TRANSCRIPT_TAIL_MIN_ENTRIES, TRANSCRIPT_WINDOW_MAX_BYTES,
    TRANSCRIPT_WINDOW_MAX_ENTRIES, TRANSCRIPT_WINDOW_STEP_ENTRIES,
};
use pir::tui::theme::{with_theme, ThemeColor};

// ---------------------------------------------------------------------------
// Minimal allocation accounting (this standalone binary; see
// `tests/tui_frame_reuse.rs` for the full harness). Only the LIVE delta is
// needed here: "released blocks free their rows" is exactly a net-negative
// live-bytes delta around the window slide.
// ---------------------------------------------------------------------------

use std::alloc::{GlobalAlloc, Layout, System};

struct CountingAllocator;

thread_local! {
    static COUNTING: Cell<bool> = const { Cell::new(false) };
    static LIVE_BYTES: Cell<i64> = const { Cell::new(0) };
    static EVENTS: Cell<u64> = const { Cell::new(0) };
}

#[expect(
    unsafe_code,
    reason = "test-only GlobalAlloc wrapper: delegates straight to System and \
              adds per-thread counters over const-initialized TLS"
)]
unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let _ = COUNTING.try_with(|counting| {
            if counting.get() {
                let _ = LIVE_BYTES.try_with(|live| live.set(live.get() + layout.size() as i64));
                let _ = EVENTS.try_with(|events| events.set(events.get() + 1));
            }
        });
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        let _ = COUNTING.try_with(|counting| {
            if counting.get() {
                let _ = LIVE_BYTES.try_with(|live| live.set(live.get() - layout.size() as i64));
            }
        });
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;

/// One measured section: allocation events plus the net live-bytes delta.
fn measure<T>(f: impl FnOnce() -> T) -> (T, u64, i64) {
    let previous = COUNTING.with(|counting| counting.replace(true));
    LIVE_BYTES.with(|live| live.set(0));
    EVENTS.with(|events| events.set(0));
    let value = f();
    let events = EVENTS.with(Cell::get);
    let live = LIVE_BYTES.with(Cell::get);
    COUNTING.with(|counting| counting.set(previous));
    (value, events, live)
}

fn test_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: Mutex<()> = Mutex::new(());
    LOCK.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn warm_theme() {
    with_theme(|theme| {
        let _ = theme.fg(ThemeColor::Text, "warm");
    });
}

// ---------------------------------------------------------------------------
// Fixture: a transcript of large tool blocks (big payload, few rows) so the
// byte budgets bind long before the entry count does.
// ---------------------------------------------------------------------------

/// Payload bytes per fixture entry (the output dominates).
const ENTRY_BYTES: usize = 16 * 1024;
/// Marker unique to entry `index`'s rendered text (the reachability probe).
fn marker(index: usize) -> String {
    format!("WINDOWMARK{index:05}")
}

fn tool_entry(index: usize) -> ChatEntry {
    // The marker goes LAST: a collapsed bash block keeps the newest
    // `BASH_PREVIEW_LINES` visual lines (see `messages.rs`), so only the tail
    // of the output is guaranteed to be rendered.
    let mut output = String::with_capacity(ENTRY_BYTES + 64);
    while output.len() + marker(index).len() + 1 < ENTRY_BYTES {
        output.push_str("windowed transcript payload line\n");
    }
    output.push_str(&marker(index));
    output.push('\n');
    ChatEntry::ToolExecution {
        call_id: Some(format!("call-{index}")),
        tool_name: "bash".into(),
        args: Arc::new(serde_json::json!({ "command": format!("printf entry-{index}") })),
        output: Arc::from(output),
        is_error: false,
        duration_ms: Some(1200),
        pending: false,
        details: None,
        started_at: None,
        preview: None,
        images: Arc::new(Vec::new()),
    }
}

/// `count` deterministic tool entries, oldest first.
fn transcript(count: usize) -> Vec<ChatEntry> {
    (0..count).map(tool_entry).collect()
}

/// Test-local component wrapper around the live chat container (the production
/// wrapper is private to `terminal.rs`).
struct WindowedChat(Arc<Mutex<ChatContainer>>);

impl Component for WindowedChat {
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
            .render_into(width, out)
    }

    fn invalidate(&self) {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .invalidate();
    }
}

struct Harness {
    core: TuiCore,
    chat: Arc<Mutex<ChatContainer>>,
    window: TranscriptWindow,
    entries: Vec<ChatEntry>,
}

impl Harness {
    /// The production loop's shape: transcript viewport + dock, windowed
    /// viewport on, one render per step.
    fn new(entries: Vec<ChatEntry>, width: usize, height: usize, dock_rows: usize) -> Self {
        let terminal = Terminal::new();
        terminal.set_size(width, height);
        let mut core = TuiCore::new(terminal);
        core.set_windowed_viewport(true);
        let chat = Arc::new(Mutex::new(ChatContainer::new()));
        core.push_content(Box::new(WindowedChat(Arc::clone(&chat))));
        let dock: Vec<String> = (0..dock_rows)
            .map(|row| {
                if row == 0 {
                    "EDITOR row".to_string()
                } else {
                    format!("FOOTER row {row}")
                }
            })
            .collect();
        core.push_content(Box::new(Text::with_padding(dock.join("\n"), 0, 0)));
        core.set_dock_height(dock_rows);
        Self {
            core,
            chat,
            window: TranscriptWindow::new(),
            entries,
        }
    }

    /// One loop iteration: plan the window against the previous frame, push
    /// the slice into the container, re-anchor the pin, render.
    fn step(&mut self) {
        let moved = self.window.sync(&self.entries, &self.core);
        self.chat
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .set_entries(self.window.slice(&self.entries));
        if moved {
            self.core.resync_window();
        }
        self.core.render();
    }

    fn band_rows(&self) -> usize {
        self.core.frame_buffer_length()
    }

    fn viewport_rows(&self) -> usize {
        self.core.rows().max(1) - self.core.dock_height()
    }

    fn max_offset(&self) -> usize {
        max_scroll_offset(
            self.core.frame_buffer_length(),
            self.core.dock_height(),
            self.core.rows(),
        )
    }

    fn corpus_contains(&self, needle: &str) -> bool {
        self.core
            .last_frame_lines()
            .iter()
            .any(|line| line.contains(needle))
    }
}

// ---------------------------------------------------------------------------
// 1. Pure budgets
// ---------------------------------------------------------------------------

#[test]
fn tail_window_keeps_the_byte_budget_below_the_entry_minimum() {
    let _guard = test_lock();
    let entries = transcript(200);
    // 16 KiB per entry plus the tool block's own overhead: the byte budget
    // binds BELOW the 64-entry soft minimum, and the window must stop at the
    // budget anyway (the old minimum-first rule let the window run over).
    let start = tail_window_start(
        &entries,
        TRANSCRIPT_TAIL_MIN_ENTRIES,
        TRANSCRIPT_WINDOW_MAX_ENTRIES,
        TRANSCRIPT_TAIL_BYTES,
    );
    assert!(
        start > 0,
        "the tail budget must skip older entries: start={start}"
    );
    let bytes = window_payload_bytes(&entries, start, entries.len());
    assert!(
        bytes <= TRANSCRIPT_TAIL_BYTES,
        "tail window over budget: {bytes} bytes"
    );
    assert!(
        entries.len() - start < TRANSCRIPT_TAIL_MIN_ENTRIES,
        "the byte budget must win over the soft minimum: {} entries",
        entries.len() - start
    );

    // A tiny transcript always starts at 0 (nothing to window).
    let small = transcript(3);
    assert_eq!(
        tail_window_start(
            &small,
            TRANSCRIPT_TAIL_MIN_ENTRIES,
            TRANSCRIPT_WINDOW_MAX_ENTRIES,
            TRANSCRIPT_TAIL_BYTES
        ),
        0
    );

    // Entry cap: many tiny entries still cannot exceed the block cap.
    let tiny: Vec<ChatEntry> = (0..(TRANSCRIPT_WINDOW_MAX_ENTRIES + 50))
        .map(|index| ChatEntry::User(marker(index)))
        .collect();
    let start = tail_window_start(&tiny, 4, TRANSCRIPT_WINDOW_MAX_ENTRIES, usize::MAX);
    assert!(
        tiny.len() - start <= TRANSCRIPT_WINDOW_MAX_ENTRIES,
        "entry cap must bind: window has {}",
        tiny.len() - start
    );

    // `entry_payload_bytes` counts the payload, not the container.
    assert!(
        entry_payload_bytes(&entries[0]) >= ENTRY_BYTES,
        "the payload proxy must see the tool output"
    );
}

#[test]
fn advance_window_extends_ahead_and_releases_past_the_margin() {
    let viewport = 30;
    // Extend-ahead: the view is at the window top while scrolling up.
    assert_eq!(
        advance_window(300, 400, 1, 0, viewport, 12, true),
        300 - TRANSCRIPT_WINDOW_STEP_ENTRIES
    );
    // Pinned at the end (not scrolled): never extend.
    assert_eq!(advance_window(300, 400, 1, 0, viewport, 12, false), 300);
    // Over the byte cap with the view scrolled well past the window top:
    // release the gap above the two-viewport margin.
    let released = advance_window(
        100,
        400,
        TRANSCRIPT_WINDOW_MAX_BYTES + 1,
        viewport * 4,
        viewport,
        10,
        true,
    );
    assert!(
        released > 100,
        "over the cap the window must release: {released}"
    );
    // Over the cap but the view is at the window top: nothing to release
    // (refusing to drop on-screen rows wins over the soft cap).
    assert_eq!(
        advance_window(
            100,
            400,
            TRANSCRIPT_WINDOW_MAX_BYTES + 1,
            viewport / 2,
            viewport,
            10,
            true
        ),
        100
    );
}

// ---------------------------------------------------------------------------
// 2. Opening materializes only the tail; the follow frame is O(visible)
// ---------------------------------------------------------------------------

#[test]
fn open_materializes_only_the_tail_window_and_paints_the_viewport() {
    let _guard = test_lock();
    warm_theme();
    let (width, height, dock) = (80usize, 30usize, 2usize);
    let mut harness = Harness::new(transcript(200), width, height, dock);
    harness.step();

    // The window is a strict suffix (older entries untouched)...
    assert!(
        harness.window.start() > 0,
        "the whole transcript must not be materialized on open"
    );
    let slice = harness.window.slice(&harness.entries);
    assert!(
        window_payload_bytes(
            &harness.entries,
            harness.window.start(),
            harness.entries.len()
        ) <= TRANSCRIPT_TAIL_BYTES + ENTRY_BYTES,
        "the materialized window must respect the tail budget"
    );
    assert!(!slice.is_empty());
    // ...and the container only ever renders the slice: the band (the
    // undecorated frame / selection corpus) stays bounded by the window, and
    // the oldest entry's text is nowhere in it.
    assert!(
        !harness.corpus_contains(&marker(0)),
        "a block outside the window must not be built"
    );
    assert!(
        harness.band_rows() > harness.viewport_rows(),
        "fixture must be taller than the viewport: {} rows",
        harness.band_rows()
    );

    // The FOLLOW paint is the visible window + dock, on the scrolled
    // machinery: the painted baseline is the terminal height, the raw corpus
    // is the (bounded) band, and the layout says windowed.
    let layout = harness.core.painted_layout();
    assert!(
        layout.windowed,
        "windowed follow must use the scrolled layout"
    );
    assert_eq!(layout.dock_len, dock);
    assert_eq!(
        layout.transcript_end + layout.dock_len,
        harness.band_rows(),
        "the band's end is the dock start"
    );
    assert_eq!(layout.window_len + layout.dock_len, height);
    assert_eq!(harness.core.last_screen_lines().len(), height);
    assert_eq!(
        harness.core.visible_top(),
        layout.window_start,
        "the visible top is the painted window's first band row"
    );

    // A steady frame rebuilds only the band (bounded by the window), writes
    // no pool rows and touches at most the cursor-anchor row.
    FrameWork::reset();
    RowsWork::reset();
    ToolMemoWork::reset();
    harness.step();
    let work = FrameWork::snapshot();
    let rows = RowsWork::snapshot();
    assert_eq!(
        rows.pool_growth, 0,
        "a steady windowed frame must not grow the pool"
    );
    let mut memos = ToolMemoWork::snapshot();
    // The frame counter itself is expected to be 1 (this render).
    memos.frames = 0;
    assert_eq!(
        memos,
        ToolMemoWork::default(),
        "a steady windowed frame must not rebuild block memos"
    );
    assert!(
        work.rebuilt_rows <= harness.band_rows() as u64,
        "the rebuild must stay inside the materialized window: {work:?}"
    );
    assert!(
        work.snapshot_rows <= harness.band_rows() as u64,
        "the corpus snapshot must stay inside the materialized window: {work:?}"
    );
    assert!(
        work.reset_rows <= 32,
        "a steady windowed frame writes at most the cursor anchor: {work:?}"
    );
    assert_eq!(
        work.diff_scanned_rows, 0,
        "the windowed path diffs the painted window inline, not the whole frame"
    );
}

/// The long-session property: a steady windowed frame's WORK is bounded by
/// the materialized window, not by the transcript. Two fixtures with the same
/// per-entry size but twice the transcript have (almost) the same window, so
/// their steady frames must allocate the same, and neither may rebuild the
/// per-frame memo channels.
#[test]
fn steady_windowed_frame_work_does_not_grow_with_the_transcript() {
    let _guard = test_lock();
    warm_theme();
    let (width, height, dock) = (80usize, 30usize, 2usize);
    let mut small = Harness::new(transcript(200), width, height, dock);
    let mut large = Harness::new(transcript(400), width, height, dock);
    small.step();
    large.step();
    // Same budget, same per-entry size -> the same materialized window size.
    let small_window = small.entries.len() - small.window.start();
    let large_window = large.entries.len() - large.window.start();
    assert_eq!(
        small_window, large_window,
        "the fixtures must materialize equally sized windows"
    );

    let (_, small_events, _) = measure(|| small.step());
    let (_, large_events, _) = measure(|| large.step());
    assert!(
        large_events <= small_events + 32,
        "a steady windowed frame must not pay for the transcript: \
         {small_events} events at 200 entries vs {large_events} at 400"
    );
    // The band (the undecorated corpus) is the window, not the transcript.
    assert!(
        small.band_rows() < small.entries.len() * 40 / 2,
        "the band must not be proportional to the whole transcript"
    );
    assert!(large.band_rows() < large.entries.len() * 40 / 2);
}

// ---------------------------------------------------------------------------
// 3. Scrolling up materializes on demand; the oldest entry is reachable
// ---------------------------------------------------------------------------

#[test]
fn scrolling_up_materializes_older_blocks_until_the_oldest_is_reachable() {
    let _guard = test_lock();
    warm_theme();
    let (width, height, dock) = (80usize, 30usize, 2usize);
    let mut harness = Harness::new(transcript(200), width, height, dock);
    harness.step();
    let tail_start = harness.window.start();
    assert!(tail_start > 0);

    // Page up like the loop does: the scroll saturates at the band top, and
    // the next sync extends the window (extend-ahead) — the user can keep
    // scrolling because the band grew.
    let start_before = harness.window.start();
    let mut extensions = 0usize;
    for _ in 0..64 {
        let max = harness.max_offset();
        harness.core.set_scroll_offset(max);
        harness.step();
        if harness.window.start() < start_before {
            extensions += 1;
        }
        if harness.window.start() == 0 {
            break;
        }
    }
    assert!(extensions > 0, "scrolling up must materialize older blocks");
    assert_eq!(
        harness.window.start(),
        0,
        "the oldest entry must be reachable by scrolling up"
    );
    assert!(
        harness.corpus_contains(&marker(0)),
        "the oldest entry's block must be materialized once the window reaches it"
    );
    // The extension steps are the documented chunk size.
    assert_eq!(
        TRANSCRIPT_WINDOW_STEP_ENTRIES % TRANSCRIPT_TAIL_MIN_ENTRIES,
        0,
        "step size sanity"
    );
}

// ---------------------------------------------------------------------------
// 4. Returning to the tail releases the blocks (and their cached rows)
// ---------------------------------------------------------------------------

#[test]
fn returning_to_the_tail_releases_the_older_blocks_and_their_rows() {
    let _guard = test_lock();
    warm_theme();
    let (width, height, dock) = (80usize, 30usize, 2usize);
    let mut harness = Harness::new(transcript(200), width, height, dock);
    harness.step();

    // Materialize a deep window first.
    for _ in 0..64 {
        let max = harness.max_offset();
        harness.core.set_scroll_offset(max);
        harness.step();
        if harness.window.start() == 0 {
            break;
        }
    }
    assert_eq!(
        harness.window.start(),
        0,
        "fixture must reach the oldest entry"
    );
    let deep_rows = harness.band_rows();
    assert!(
        deep_rows > height,
        "the deep band must be taller than the screen"
    );

    // Back to the tail: the window collapses to the tail budget. The slide
    // drops the components the view scrolled past; their rows are freed, so
    // the live-bytes delta is negative.
    harness.core.set_scroll_offset(0);
    let moved = harness.window.sync(&harness.entries, &harness.core);
    assert!(moved, "returning to the tail must move the window back");
    assert!(
        harness.window.start() > 0,
        "the tail window must release everything above the tail budget"
    );
    let (_, _, live) = measure(|| {
        harness
            .chat
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .set_entries(harness.window.slice(&harness.entries));
    });
    assert!(
        live < 0,
        "the released blocks must free live bytes (delta {live})"
    );
    harness.core.resync_window();
    harness.core.render();
    assert!(
        !harness.corpus_contains(&marker(0)),
        "a released block must be gone from the frame"
    );
    assert!(
        harness.band_rows() < deep_rows,
        "the band must shrink back to the tail window: {} vs {deep_rows}",
        harness.band_rows()
    );
    assert!(
        window_payload_bytes(
            &harness.entries,
            harness.window.start(),
            harness.entries.len()
        ) <= TRANSCRIPT_TAIL_BYTES + ENTRY_BYTES,
        "the released window must respect the tail budget"
    );
}

// ---------------------------------------------------------------------------
// 5. The selection corpus resolves the visible rows + the dock
// ---------------------------------------------------------------------------

#[test]
fn selection_mapping_resolves_the_window_and_the_dock_rows() {
    let _guard = test_lock();
    warm_theme();
    let (width, height, dock) = (60usize, 12usize, 2usize);
    let mut harness = Harness::new(transcript(40), width, height, dock);
    harness.step();

    let layout = harness.core.painted_layout();
    assert!(layout.windowed);
    let viewport = harness.viewport_rows();
    assert_eq!(harness.core.visible_top(), layout.window_start);

    // Transcript rows: the painted window's band rows, in screen order.
    for row in 0..layout.window_len {
        assert_eq!(
            harness.core.screen_row_to_buffer_row(row),
            Some(layout.window_start + row),
            "screen row {row}"
        );
    }
    // Dock rows: the band's tail rows (the gap-less formula the scrolled path
    // uses), so a drag over the editor selects the editor's own line.
    for row in 0..dock {
        assert_eq!(
            harness
                .core
                .screen_row_to_buffer_row(layout.window_len + row),
            Some(layout.transcript_end + row),
            "dock row {row}"
        );
    }
    assert_eq!(
        harness.core.screen_row_to_buffer_row(height),
        None,
        "a row below the painted frame has no band row"
    );

    // The corpus is indexed by the mapped band rows, and `selection_text`
    // trims the copy to what is on screen.
    let last_transcript = layout.window_start + layout.window_len - 1;
    let editor_row = layout.transcript_end;
    let span = SelectionSpan {
        start: (last_transcript, 0),
        end: (editor_row + 1, usize::MAX),
    };
    let copied = selection_text(harness.core.last_frame_lines(), &span)
        .expect("a selection over painted rows must copy");
    let lines: Vec<&str> = copied.lines().collect();
    assert_eq!(lines.len(), 3, "two transcript-boundary rows + the dock");
    assert_eq!(lines[1], "EDITOR row");
    assert_eq!(lines[2], "FOOTER row 1");
    assert!(!copied.contains("  "), "per-line padding must be trimmed");
    let _ = viewport;
}

/// The per-token patch addresses the container's committed array, which is the
/// MATERIALIZED WINDOW, not the whole transcript. The live-streaming tail must
/// land on `entries.len() - 1 - window_start`; without the origin the patch
/// missed whenever the tail window had scrolled and every token paid the full
/// two-sided diff of the 512-entry window (the exact cost the fast path exists
/// to avoid, on exactly the long sessions where it matters).
#[test]
fn live_stream_patch_uses_the_materialized_window_origin() {
    let _guard = test_lock();
    warm_theme();
    let mut entries: Vec<ChatEntry> = (0..5)
        .map(|index| ChatEntry::User(format!("older user entry {index}")))
        .collect();
    entries.push(ChatEntry::Streaming {
        live: true,
        thinking: String::new(),
        text: "first".to_string(),
    });
    let mut chat = ChatContainer::new();
    // The container only ever sees the tail window (`window.start() == 3`).
    let window_start = 3;
    chat.set_entries(&entries[window_start..]);
    let _ = chat.render(80);

    if let Some(ChatEntry::Streaming { text, .. }) = entries.last_mut() {
        text.push_str(" and the appended tail");
    }
    assert!(
        chat.patch_last_streaming(&entries, window_start),
        "the live tail is committed at entries.len()-1-window_start"
    );
    let painted = chat.render(80).join("\n");
    assert!(
        painted.contains("and the appended tail"),
        "the patched component must paint the new text"
    );
}
