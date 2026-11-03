//! TUI performance bounds at the 100k-entry scale (no external benchmark
//! framework): the materialized transcript window, the painted frame, and the
//! search match collection all stay bounded, and an unchanged frame does no
//! per-row search hashing.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use pir::ai::types::{AssistantMessage, AssistantRole, ContentBlock, StopReason, TextContent};
use pir::tui::altscreen_search::{
    find_search_matches, refresh_search_with_hint, AltScreenState, SEARCH_MATCH_LIMIT,
};
use pir::tui::app::{
    ChatContainer, ChatEntry, NoticeKind, RawCustomRender, RawRender, RawToolRender,
};
use pir::tui::core::{
    Component, Container, FrameWork, ReservedScrollbarColumn, Rows, RowsWork, Terminal,
    TerminalSink, Text, TuiCore,
};
use pir::tui::markdown::Markdown;
use pir::tui::renderers::{
    CustomRenderOutcome, RenderInputs, RenderScheduler, RendererKind, ToolRenderShell,
};
use pir::tui::stdin_buffer::WriteLog;
use pir::tui::terminal::{
    apply_custom_render_result, commit_live_stream_batch, entry_render_revision,
    finalize_streaming_segments_tracked, regular_window_start, shared_chat_component,
    sync_windowed_band_tracked, tail_window_start, window_payload_bytes, ChatBlockCache,
    CustomRenderResult, EntryDelta, LiveStreamBatch, LiveStreamCommit, PayloadScanWork,
    TranscriptWindow, TRANSCRIPT_TAIL_BYTES, TRANSCRIPT_WINDOW_MAX_BYTES,
    TRANSCRIPT_WINDOW_MAX_ENTRIES,
};
use pir::tui::theme::Theme;

struct SharedChat {
    chat: Arc<Mutex<ChatContainer>>,
    /// Content revision exposed to the frame cache.
    revision: Arc<AtomicU64>,
    /// Calls into `ChatContainer::render_into` (the expensive band render).
    render_calls: Arc<AtomicU64>,
}

impl Component for SharedChat {
    fn render(&self, width: usize) -> Vec<String> {
        self.chat
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .render(width)
    }

    fn render_into(&self, width: usize, out: &mut Rows<'_>) {
        self.render_calls.fetch_add(1, Ordering::Relaxed);
        self.chat
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .render_into(width, out);
    }

    fn content_revision(&self) -> Option<u64> {
        Some(self.revision.load(Ordering::Relaxed))
    }
}

/// A dock-like component whose one row changes every frame (spinner/cursor
/// tick) and which deliberately reports no content revision: the frame
/// renderer must re-render just this component and reuse the band rows.
struct CursorTick {
    tick: Arc<AtomicU64>,
    render_calls: Arc<AtomicU64>,
}

impl Component for CursorTick {
    fn render(&self, _width: usize) -> Vec<String> {
        vec![format!("cursor {}", self.tick.load(Ordering::Relaxed))]
    }

    fn render_into(&self, _width: usize, out: &mut Rows<'_>) {
        self.render_calls.fetch_add(1, Ordering::Relaxed);
        out.push_fmt(format_args!("cursor {}", self.tick.load(Ordering::Relaxed)));
    }
}

/// A versioned [`Text`] (frame-cache key follows `revision`), used to cover
/// the regular main-screen path (the existing revisioned harness is
/// windowed-only).
struct VersionedText {
    text: Arc<Text>,
    revision: Arc<AtomicU64>,
}

impl Component for VersionedText {
    fn render(&self, width: usize) -> Vec<String> {
        self.text.render(width)
    }

    fn render_into(&self, width: usize, out: &mut Rows<'_>) {
        self.text.render_into(width, out);
    }

    fn content_revision(&self) -> Option<u64> {
        Some(self.revision.load(Ordering::Relaxed))
    }
}

fn test_core() -> (TuiCore, Arc<Mutex<ChatContainer>>) {
    let (core, chat, _, _) = test_core_revisioned();
    (core, chat)
}

fn test_core_revisioned() -> (
    TuiCore,
    Arc<Mutex<ChatContainer>>,
    Arc<AtomicU64>,
    Arc<AtomicU64>,
) {
    let terminal = Terminal::with_sink_factory(
        || Box::new(std::io::sink()) as Box<dyn TerminalSink>,
        WriteLog::disabled(),
    );
    terminal.set_size(120, 40);
    let mut core = TuiCore::new(terminal);
    core.set_windowed_viewport(true);
    let chat = Arc::new(Mutex::new(ChatContainer::new()));
    let revision = Arc::new(AtomicU64::new(0));
    let render_calls = Arc::new(AtomicU64::new(0));
    core.push_content(Box::new(SharedChat {
        chat: Arc::clone(&chat),
        revision: Arc::clone(&revision),
        render_calls: Arc::clone(&render_calls),
    }));
    (core, chat, revision, render_calls)
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

/// An entry with a large recursive payload (24 argument strings + a details
/// body) — the shape `entry_payload_bytes` walks per entry.
fn heavy_tool_entry(index: usize, tag: &str) -> ChatEntry {
    let mut args = serde_json::Map::new();
    for key in 0..24 {
        args.insert(
            format!("key{key}"),
            serde_json::Value::String(format!("{tag}-{}", "x".repeat(512))),
        );
    }
    let mut details = serde_json::Map::new();
    details.insert(
        "body".to_string(),
        serde_json::Value::String("d".repeat(4096)),
    );
    ChatEntry::ToolExecution {
        call_id: Some(format!("call-{index}")),
        tool_name: "write".to_string(),
        args: Arc::new(serde_json::Value::Object(args)),
        output: Arc::from(format!("output {index}").as_str()),
        is_error: false,
        duration_ms: Some(1),
        pending: false,
        details: Some(Arc::new(serde_json::Value::Object(details))),
        started_at: None,
        preview: None,
        images: Arc::new(Vec::new()),
    }
}

/// R1-L4-008: a dirty frame that only moves the cursor/spinner must not
/// re-render the 512-entry materialized band. The frame cache serves the
/// transcript from its revision-keyed rows; only the one-row cursor component
/// re-renders, and the cached rows are compared-and-reused (no writes) instead
/// of being copied into the frame.
#[test]
fn cursor_only_dirty_frame_reuses_the_512_entry_band() {
    let entries: Vec<ChatEntry> = (0..100_000)
        .map(|index| ChatEntry::User(format!("row {index:06}")))
        .collect();
    let (mut core, chat, revision, chat_render_calls) = test_core_revisioned();
    let mut window = TranscriptWindow::new();
    window.sync(&entries, &core);
    let band_len = window.slice(&entries).len();
    assert!(
        band_len > 0 && band_len <= TRANSCRIPT_WINDOW_MAX_ENTRIES,
        "the fixture must materialize a bounded band: {band_len}"
    );
    chat.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .set_entries(window.slice(&entries));
    revision.fetch_add(1, Ordering::Relaxed);
    let tick = Arc::new(AtomicU64::new(0));
    let cursor_calls = Arc::new(AtomicU64::new(0));
    core.push_content(Box::new(CursorTick {
        tick: Arc::clone(&tick),
        render_calls: Arc::clone(&cursor_calls),
    }));
    // First frame builds the cache; the next two let the recycled raw frame
    // settle so the measured frame takes the in-place reuse path.
    for _ in 0..3 {
        core.render();
    }
    let band_rows = core.frame_buffer_length() as u64 - 1;
    assert!(band_rows > 0);
    let chat_renders = chat_render_calls.load(Ordering::Relaxed);
    let cursor_renders = cursor_calls.load(Ordering::Relaxed);

    FrameWork::reset();
    RowsWork::reset();
    tick.fetch_add(1, Ordering::Relaxed);
    core.render();
    let work = core.last_frame_work();
    assert_eq!(
        work.rerendered_rows, 1,
        "only the cursor row may re-render on a spinner tick: {work:?}"
    );
    assert!(
        work.cached_rows >= band_rows,
        "the whole band must come from the revision cache: {work:?}"
    );
    assert_eq!(
        work.rebuilt_rows, 1,
        "only the cursor row may be rebuilt: {work:?}"
    );
    assert_eq!(
        work.rebuilt_blocks, 1,
        "only the cursor component's block may be rebuilt: {work:?}"
    );
    assert_eq!(work.rebuilt_rows + work.cached_rows, band_rows + 1);
    assert_eq!(
        chat_render_calls.load(Ordering::Relaxed),
        chat_renders,
        "the transcript component's render_into must not run on a cursor tick"
    );
    assert_eq!(
        cursor_calls.load(Ordering::Relaxed),
        cursor_renders + 1,
        "the cursor component itself must still render"
    );
    let rows = core.last_frame_rows_work();
    assert!(
        rows.row_compare_bytes <= 16,
        "a same-position cache hit on a clean raw canvas serves the whole band \
         without comparing it; only the one-row cursor component is compared: \
         {rows:?}"
    );
    assert_eq!(
        work.cache_copy_bytes, 0,
        "a steady cache hit writes no cached bytes: every frame row already \
         holds the cached content: {work:?}"
    );
    assert!(
        rows.row_write_bytes <= 16,
        "only the one-row cursor component may write on a cursor tick: {rows:?}"
    );

    // The cached frame is byte-identical to a cold render of the same state.
    let cached_frame = core.last_frame_lines().to_vec();
    let (mut cold_core, cold_chat, cold_revision, _) = test_core_revisioned();
    cold_chat
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .set_entries(window.slice(&entries));
    cold_revision.fetch_add(1, Ordering::Relaxed);
    cold_core.push_content(Box::new(CursorTick {
        tick: Arc::new(AtomicU64::new(1)),
        render_calls: Arc::new(AtomicU64::new(0)),
    }));
    cold_core.render();
    assert_eq!(
        cold_core.last_frame_lines(),
        cached_frame.as_slice(),
        "the cached band frame must match a cold render byte for byte"
    );

    // A real content revision change rebuilds the band exactly once.
    FrameWork::reset();
    RowsWork::reset();
    revision.fetch_add(1, Ordering::Relaxed);
    core.render();
    let work = core.last_frame_work();
    assert_eq!(
        chat_render_calls.load(Ordering::Relaxed),
        chat_renders + 1,
        "a content revision change must re-render the transcript once"
    );
    assert_eq!(work.cached_rows, 0, "no stale row may be served");
    assert_eq!(
        work.rebuilt_blocks, 2,
        "the transcript miss plus the always-rendered cursor component are the \
         only rebuilt blocks: {work:?}"
    );
    assert_eq!(
        work.cache_buffer_allocs, 0,
        "a cache miss must render into the slot's existing row buffer, not a \
         fresh Vec: {work:?}"
    );
    assert!(
        work.cache_buffer_reuses >= 1,
        "the miss must reuse the slot buffer: {work:?}"
    );
    assert_eq!(
        work.cache_copy_bytes, 0,
        "re-rendering identical bytes must not copy them back into a frame \
         that already holds them: {work:?}"
    );
}

/// P2-01/P2-02: one changed tail entry copies only the changed rows into the
/// frame, reusing the recycled slot buffer and the unchanged frame rows.
#[test]
fn content_change_copies_only_changed_rows() {
    let mut entries: Vec<ChatEntry> = (0..100_000)
        .map(|index| ChatEntry::User(format!("row {index:06}")))
        .collect();
    let (mut core, chat, revision, _) = test_core_revisioned();
    let mut window = TranscriptWindow::new();
    window.sync(&entries, &core);
    let band = window.slice(&entries);
    let band_len = band.len();
    assert!(band_len > 8);
    chat.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .set_entries(band);
    revision.fetch_add(1, Ordering::Relaxed);
    core.push_content(Box::new(CursorTick {
        tick: Arc::new(AtomicU64::new(0)),
        render_calls: Arc::new(AtomicU64::new(0)),
    }));
    for _ in 0..3 {
        core.render();
    }
    let frame_bytes = core.last_frame_stats().1;

    // Change the last (in-band) user row, commit it and publish the revision.
    let last = entries.len() - 1;
    entries[last] = ChatEntry::User("row 099999 CHANGED".to_string());
    chat.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .set_entries(window.slice(&entries));
    revision.fetch_add(1, Ordering::Relaxed);

    FrameWork::reset();
    core.render();
    let work = core.last_frame_work();
    assert_eq!(work.cached_rows, 0);
    assert_eq!(work.cache_buffer_allocs, 0, "{work:?}");
    assert!(work.cache_buffer_reuses >= 1, "{work:?}");
    assert!(
        work.cache_copy_bytes > 0,
        "the changed row must be written: {work:?}"
    );
    assert!(
        work.cache_copy_bytes < frame_bytes as u64,
        "unchanged rows must be reused, not copied: copied {} of {} frame bytes",
        work.cache_copy_bytes,
        frame_bytes
    );
}

/// P2-02: the regular main screen uses the frame builder's exact dirty-row
/// markers, so a spinner tick compares only the dock row instead of scanning
/// the whole transcript frame; `diff_skipped_rows` accounts for the rest.
#[test]
fn regular_main_screen_skips_unchanged_rows_via_dirty_markers() {
    let terminal = Terminal::with_sink_factory(
        || Box::new(std::io::sink()) as Box<dyn TerminalSink>,
        WriteLog::disabled(),
    );
    terminal.set_size(100, 30);
    let mut core = TuiCore::new(terminal);
    let body = (0..120)
        .map(|index| format!("line {index:03}"))
        .collect::<Vec<_>>()
        .join("\n");
    let revision = Arc::new(AtomicU64::new(0));
    core.push_content(Box::new(VersionedText {
        text: Arc::new(Text::new(body)),
        revision: Arc::clone(&revision),
    }));
    let tick = Arc::new(AtomicU64::new(0));
    core.push_content(Box::new(CursorTick {
        tick: Arc::clone(&tick),
        render_calls: Arc::new(AtomicU64::new(0)),
    }));
    for _ in 0..3 {
        core.render();
    }
    let height = core.frame_buffer_length() as u64;
    assert!(height > 100, "fixture must outgrow the viewport: {height}");

    tick.fetch_add(1, Ordering::Relaxed);
    core.render();
    let work = core.last_frame_work();
    assert_eq!(
        work.rerendered_rows, 1,
        "only the cursor row re-renders on a spinner tick: {work:?}"
    );
    assert_eq!(
        work.cached_rows + work.rerendered_rows,
        height,
        "the versioned text rows must come from the frame cache: {work:?}"
    );
    assert_eq!(work.cache_copy_bytes, 0, "{work:?}");
    assert_eq!(
        work.diff_scanned_rows, 0,
        "the dirty-row markers already compared every row while building the \
         canvas; the changed-line scan must not walk the unchanged band again: \
         {work:?}"
    );
    assert_eq!(
        work.diff_skipped_rows,
        height - 1,
        "every unchanged FrameWork row except the one-row cursor is accounted \
         for as skipped: {work:?}"
    );
}

/// R1-L4-008: the window's payload-byte planning reads a per-entry revision
/// cache. An unchanged window does zero recursive `entry_payload_bytes`
/// traversals; one changed entry costs exactly one.
#[test]
fn payload_byte_cache_skips_unchanged_bands() {
    let mut entries: Vec<ChatEntry> = (0..512)
        .map(|index| heavy_tool_entry(index, "same"))
        .collect();
    let (core, _chat) = test_core();
    let mut window = TranscriptWindow::new();
    window.sync_tracked(&entries, &core, Some(0));
    let first = window.last_payload_work();
    assert!(
        first.entries_measured >= TRANSCRIPT_WINDOW_MAX_ENTRIES as u64,
        "the first sync must measure the whole tail band: {first:?}"
    );
    assert!(first.revision_scans > 0);
    window.sync_tracked(&entries, &core, Some(0));
    assert_eq!(
        window.last_payload_work(),
        PayloadScanWork::default(),
        "an unchanged window must skip every recursive payload scan AND every \
         per-entry revision/fingerprint scan"
    );
    entries[511] = heavy_tool_entry(511, "moved");
    window.sync_tracked(&entries, &core, Some(1));
    let changed = window.last_payload_work();
    assert_eq!(
        changed.entries_measured, 1,
        "one changed entry must cost exactly one payload measurement: {changed:?}"
    );
    assert!(
        changed.revision_scans >= TRANSCRIPT_WINDOW_MAX_ENTRIES as u64,
        "the new epoch must re-read the band's revisions once: {changed:?}"
    );
}

/// R1-L4-008: regular-mode tail planning over a 100k-entry transcript uses
/// the same cache — the 512-entry tail is measured once, not per frame.
#[test]
fn regular_tail_planning_skips_payload_rescans() {
    let entries: Vec<ChatEntry> = (0..100_000)
        .map(|index| ChatEntry::User(format!("row {index:06}")))
        .collect();
    let mut window = TranscriptWindow::new();
    let start = window.regular_start_tracked(&entries, 0, Some(0));
    assert!(start > 0, "100k entries must materialize a bounded tail");
    let first = window.last_payload_work();
    assert!(
        first.entries_measured > 0
            && first.entries_measured <= TRANSCRIPT_WINDOW_MAX_ENTRIES as u64,
        "the tail planning must stay inside the 512-entry candidate band: {first:?}"
    );
    let settled = window.regular_start_tracked(&entries, start, Some(0));
    assert_eq!(settled, start, "a settled regular window must not slide");
    assert_eq!(
        window.last_payload_work(),
        PayloadScanWork::default(),
        "a settled window must not re-measure or re-fingerprint a single entry"
    );
}

/// R1-L4-008: `sync_windowed_band_tracked` short-circuits the
/// `ChatContainer::set_entries` install when neither the slice nor its
/// revision moved; a changed entry installs exactly once.
#[test]
fn unchanged_window_sync_skips_set_entries() {
    let mut entries: Vec<ChatEntry> = (0..40)
        .map(|index| ChatEntry::User(format!("row {index}")))
        .collect();
    let (mut core, chat, revision, _) = test_core_revisioned();
    let mut window = TranscriptWindow::new();
    let mut alt = AltScreenState::default();
    sync_windowed_band_tracked(
        &entries,
        &mut window,
        &mut core,
        &mut alt,
        &chat,
        Some(&revision),
    );
    let installed_revision = revision.load(Ordering::Relaxed);
    let moved = sync_windowed_band_tracked(
        &entries,
        &mut window,
        &mut core,
        &mut alt,
        &chat,
        Some(&revision),
    );
    assert!(!moved);
    assert_eq!(
        revision.load(Ordering::Relaxed),
        installed_revision,
        "an unchanged slice must not install (or bump the revision) again"
    );
    entries[39] = ChatEntry::User("changed tail".to_string());
    // The production entry-mutation paths publish the transcript epoch
    // before the next sync; the bare test mutation must do the same.
    revision.fetch_add(1, Ordering::Relaxed);
    sync_windowed_band_tracked(
        &entries,
        &mut window,
        &mut core,
        &mut alt,
        &chat,
        Some(&revision),
    );
    assert!(
        revision.load(Ordering::Relaxed) > installed_revision,
        "a changed entry must reinstall the slice"
    );
}

fn pending_tool_entry() -> ChatEntry {
    ChatEntry::ToolExecution {
        call_id: Some("call-pending".to_string()),
        tool_name: "bash".to_string(),
        args: Arc::new(serde_json::json!({"command": "sleep 1"})),
        output: Arc::from(""),
        is_error: false,
        duration_ms: None,
        pending: true,
        details: None,
        started_at: None,
        preview: None,
        images: Arc::new(Vec::new()),
    }
}

/// P2-03: the install identity is the RENDER identity, not the payload-byte
/// identity. Flipping `pending` and setting `duration_ms` changes no payload
/// pointer/length, but must move the identity.
#[test]
fn render_identity_tracks_tool_phase_and_duration() {
    let mut entry = pending_tool_entry();
    let before = entry_render_revision(&entry);
    if let ChatEntry::ToolExecution {
        pending,
        duration_ms,
        ..
    } = &mut entry
    {
        *pending = false;
        *duration_ms = Some(1_500);
    }
    let after = entry_render_revision(&entry);
    assert_ne!(
        before, after,
        "a pending→settled + duration update must move the render identity"
    );

    let mut window = TranscriptWindow::new();
    let entries = vec![pending_tool_entry()];
    assert!(window.should_install(&entries, 0));
    window.mark_installed(&entries, 0);
    // A pure epoch move with identical entries must not reinstall.
    assert!(!window.should_install(&entries, 1));
    let mut changed = entries;
    if let ChatEntry::ToolExecution {
        pending,
        duration_ms,
        ..
    } = &mut changed[0]
    {
        *pending = false;
        *duration_ms = Some(1_500);
    }
    assert!(
        window.should_install(&changed, 2),
        "the in-place render-state change must get past the installed key"
    );
}

/// P2-03 end-to-end: after a pending tool settles in place, the reinstall
/// must reach `ChatContainer` and change the visible rows (the old payload
/// fingerprint would keep the pending paint).
#[test]
fn settled_tool_phase_reinstalls_and_changes_visible_rows() {
    let mut entries = vec![pending_tool_entry()];
    let (mut core, chat, revision, _) = test_core_revisioned();
    let mut window = TranscriptWindow::new();
    let mut alt = AltScreenState::default();
    sync_windowed_band_tracked(
        &entries,
        &mut window,
        &mut core,
        &mut alt,
        &chat,
        Some(&revision),
    );
    let before = chat
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .render(100);

    if let ChatEntry::ToolExecution {
        pending,
        duration_ms,
        ..
    } = &mut entries[0]
    {
        *pending = false;
        *duration_ms = Some(1_500);
    }
    revision.fetch_add(1, Ordering::Relaxed);
    sync_windowed_band_tracked(
        &entries,
        &mut window,
        &mut core,
        &mut alt,
        &chat,
        Some(&revision),
    );
    let after = chat
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .render(100);
    assert_ne!(
        before, after,
        "the settled tool block must repaint (pending background / Took tail)"
    );
}

/// P2-01: a component that decomposes its output into blocks is re-rendered
/// per changed block. `TuiCore` caches each block's rows; a revision move on
/// one block calls `render_block_into` for that block alone and serves every
/// other block without even comparing its rows (the raw canvas is clean).
#[test]
fn changed_block_rebuild_does_not_rerender_the_band() {
    struct BlockText {
        revisions: Arc<Mutex<Vec<u64>>>,
        rows: Arc<Mutex<Vec<Vec<String>>>>,
        calls: Arc<Mutex<Vec<usize>>>,
    }

    impl Component for BlockText {
        fn render(&self, _width: usize) -> Vec<String> {
            self.rows
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .iter()
                .flatten()
                .cloned()
                .collect()
        }

        fn render_into(&self, _width: usize, out: &mut Rows<'_>) {
            for row in self
                .rows
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .iter()
                .flatten()
            {
                out.push(row);
            }
        }

        fn content_blocks(&self) -> Option<Vec<u64>> {
            Some(
                self.revisions
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .clone(),
            )
        }

        fn render_block_into(&self, index: usize, _width: usize, out: &mut Rows<'_>) {
            self.calls
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(index);
            for row in &self
                .rows
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)[index]
            {
                out.push(row);
            }
        }
    }

    let terminal = Terminal::with_sink_factory(
        || Box::new(std::io::sink()) as Box<dyn TerminalSink>,
        WriteLog::disabled(),
    );
    terminal.set_size(80, 20);
    let mut core = TuiCore::new(terminal);
    let revisions = Arc::new(Mutex::new(vec![1u64, 1, 1]));
    let rows = Arc::new(Mutex::new(vec![
        vec!["block-a-1".to_string(), "block-a-2".to_string()],
        vec!["block-b".to_string()],
        vec!["block-c".to_string()],
    ]));
    let calls = Arc::new(Mutex::new(Vec::new()));
    let mut content = Container::new();
    content.add_child(BlockText {
        revisions: Arc::clone(&revisions),
        rows: Arc::clone(&rows),
        calls: Arc::clone(&calls),
    });
    // Exercise the production nesting (`ReservedScrollbarColumn(Container(
    // ...))`): the block decomposition must propagate through both wrappers.
    core.push_content(Box::new(ReservedScrollbarColumn::new(content, false)));
    for _ in 0..3 {
        core.render();
    }

    calls
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clear();
    revisions
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)[1] = 2;
    rows.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)[1][0] = "block-b CHANGED".to_string();
    FrameWork::reset();
    core.render();
    let work = core.last_frame_work();
    assert_eq!(
        calls
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_slice(),
        &[1],
        "only the changed block may render"
    );
    assert_eq!(work.rebuilt_blocks, 1, "{work:?}");
    assert_eq!(work.rerendered_rows, 1, "{work:?}");
    assert_eq!(
        work.cached_rows, 3,
        "the other three rows are cached: {work:?}"
    );
    assert_eq!(
        core.last_frame_rows_work().row_compare_bytes,
        0,
        "a same-position hit on a clean raw canvas skips the band compare: {:?}",
        core.last_frame_rows_work()
    );
}

/// P2-01 correctness: the per-entry block cache must concatenate to exactly
/// the bytes of the full `ChatContainer` render, including the conditional
/// user spacer and a default-shell Raw block.
#[test]
fn chat_block_cache_matches_the_full_container_render() {
    let tools = Arc::new(AtomicBool::new(false));
    let entries = vec![
        ChatEntry::User("alpha".to_string()),
        ChatEntry::User("beta".to_string()),
        ChatEntry::Notice {
            kind: NoticeKind::Status,
            text: "a notice".to_string(),
        },
        ChatEntry::Raw {
            call_id: Some("raw-1".to_string()),
            lines: vec!["raw line".to_string()],
            render: None,
        },
        pending_tool_entry(),
        ChatEntry::Streaming {
            live: true,
            thinking: "thought".to_string(),
            text: "streamed body".to_string(),
        },
    ];
    let mut full = ChatContainer::new();
    full.set_tools_expanded(Arc::clone(&tools));
    full.set_entries(&entries);
    let expected = full.render(80);

    let mut blocks = ChatBlockCache::new(Arc::clone(&tools), false);
    blocks.set_entries(&entries);
    let count = blocks.block_revisions().expect("primed block cache").len();
    let mut actual = Vec::new();
    {
        let mut rows = Rows::new(&mut actual);
        for index in 0..count {
            blocks.render_block(index, 80, &mut rows);
        }
        rows.trim();
    }
    assert_eq!(
        actual, expected,
        "the per-entry block concatenation must be byte-identical"
    );
}

fn raw_tool_entry(is_error: bool, pending: bool) -> ChatEntry {
    ChatEntry::Raw {
        call_id: Some("raw-tool".to_string()),
        lines: vec!["rendered line".to_string()],
        render: Some(RawRender::Tool(RawToolRender {
            call_id: Some("raw-tool".to_string()),
            tool_name: "bash".to_string(),
            args: Arc::new(serde_json::json!({"command": "true"})),
            output: Arc::from("ok"),
            is_error,
            details: None,
            expanded: false,
            width: 80,
            theme_generation: 0,
            shell: Some(ToolRenderShell::Default),
            pending,
            images: Arc::new(Vec::new()),
            image_lines: Arc::new(Vec::new()),
        })),
    }
}

/// P2-03: Raw `pending` AND `is_error` are render inputs. The field flip is
/// performed on the SAME entry object (the `lines` buffer pointer stays
/// identical), so the only moving input is the field under test — the old
/// tests built two independent entries whose distinct line/call-id pointers
/// made them pass even with `is_error` missing from the identity.
#[test]
fn render_identity_tracks_raw_pending_and_is_error() {
    let mut entry = raw_tool_entry(false, false);
    let lines_ptr = match &entry {
        ChatEntry::Raw { lines, .. } => lines.as_ptr(),
        _ => unreachable!(),
    };
    let before = entry_render_revision(&entry);

    let mut window = TranscriptWindow::new();
    assert!(window.should_install(std::slice::from_ref(&entry), 0));
    window.mark_installed(std::slice::from_ref(&entry), 0);

    // Settled success -> settled error: same lines buffer, same everything
    // else; only `RawToolRender::is_error` moves.
    if let ChatEntry::Raw {
        render: Some(RawRender::Tool(tool)),
        ..
    } = &mut entry
    {
        tool.is_error = true;
    }
    assert_eq!(
        match &entry {
            ChatEntry::Raw { lines, .. } => lines.as_ptr(),
            _ => unreachable!(),
        },
        lines_ptr,
        "the isolation fixture must keep the same lines buffer"
    );
    assert_ne!(
        before,
        entry_render_revision(&entry),
        "is_error alone is a render input"
    );
    assert!(
        window.should_install(std::slice::from_ref(&entry), 1),
        "an is_error-only in-place flip must reinstall the Raw slice"
    );
    window.mark_installed(std::slice::from_ref(&entry), 1);

    // Error -> pending: only `RawToolRender::pending` moves (is_error returns
    // to false, lines buffer untouched).
    let before_pending = entry_render_revision(&entry);
    if let ChatEntry::Raw {
        render: Some(RawRender::Tool(tool)),
        ..
    } = &mut entry
    {
        tool.is_error = false;
        tool.pending = true;
    }
    assert_ne!(
        before_pending,
        entry_render_revision(&entry),
        "pending alone is a render input"
    );
    assert!(
        window.should_install(std::slice::from_ref(&entry), 2),
        "a pending-only in-place flip must reinstall the Raw slice"
    );
}

/// P2-03: replacing the ToolExecution `details` allocation (same payload
/// bytes, still on a settled block) is a render identity move because the
/// retained child reads `details` for truncation/diff warnings.
#[test]
fn render_identity_tracks_tool_details() {
    let entry = heavy_tool_entry(0, "same");
    let before = entry_render_revision(&entry);
    let mut changed = entry.clone();
    if let ChatEntry::ToolExecution { details, .. } = &mut changed {
        *details = Some(Arc::new(serde_json::json!({"body": "different"})));
    }
    assert_ne!(
        before,
        entry_render_revision(&changed),
        "a details Arc replacement must move the render identity"
    );
}

/// P2-04: with an exact per-entry delta, a content change re-reads ONLY the
/// changed entry; the settled sync after it reports zero revision scans and
/// zero recursive measurements. The install key uses the same rolling
/// versions (no band walk).
#[test]
fn entry_delta_revalidation_is_constant_and_seam_visible() {
    let mut entries: Vec<ChatEntry> = (0..512)
        .map(|index| heavy_tool_entry(index, "same"))
        .collect();
    let (core, _chat) = test_core();
    let mut window = TranscriptWindow::new();
    window.sync_tracked(&entries, &core, Some(0));
    assert!(window.last_payload_work().revision_scans > 0);

    entries[511] = heavy_tool_entry(511, "moved");
    window.sync_tracked_with_delta(
        &entries,
        &core,
        Some(1),
        Some(EntryDelta {
            changed: Some((511, 512)),
            appended_from: None,
        }),
    );
    let changed = window.last_payload_work();
    assert_eq!(
        changed.revision_scans, 1,
        "only the changed entry may be re-read: {changed:?}"
    );
    assert_eq!(changed.entries_measured, 1, "{changed:?}");
    assert!(
        window.should_install(&entries, 1),
        "the changed entry must install"
    );
    window.mark_installed(&entries, 1);

    window.sync_tracked_with_delta(&entries, &core, Some(1), None);
    assert_eq!(
        window.last_payload_work(),
        PayloadScanWork::default(),
        "a settled sync after the delta must not touch a single entry"
    );
    assert!(
        !window.should_install(&entries, 1),
        "the settled install key must answer from the rolling versions"
    );
    assert_eq!(
        window.last_payload_work(),
        PayloadScanWork::default(),
        "the settled should_install must not add revision scans"
    );
}

fn assistant_message(text: &str) -> AssistantMessage {
    AssistantMessage {
        role: AssistantRole::Assistant,
        content: vec![ContentBlock::Text(TextContent {
            text: text.into(),
            text_signature: None,
        })],
        api: "tui-100k".into(),
        provider: "tui-100k".into(),
        model: "tui-100k".into(),
        stop_reason: StopReason::Stop,
        ..Default::default()
    }
}

/// P2-04: `AssistantDone` finalization reports only the range it actually
/// rewrote (the live segment at the tail), so the payload cache re-reads one
/// entry instead of all 1000. A `0..entries.len()` delta would report 1000
/// revision scans here.
#[test]
fn assistant_done_publishes_only_the_rewritten_tail_range() {
    let mut entries: Vec<ChatEntry> = (0..1000)
        .map(|index| ChatEntry::User(format!("row {index:04}")))
        .collect();
    entries.push(ChatEntry::Streaming {
        live: true,
        thinking: String::new(),
        text: "the reply".to_string(),
    });
    let baseline = entries.clone();

    let (finalized, changed) =
        finalize_streaming_segments_tracked(&mut entries, Some(assistant_message("the reply")));
    assert_eq!(finalized, 1);
    assert_eq!(
        changed,
        Some((1000, 1001)),
        "only the live tail slot moved; the 1000 prefix entries are untouched"
    );

    let (core, _chat) = test_core();
    let mut window = TranscriptWindow::new();
    window.sync_tracked(&baseline, &core, Some(0));
    let mut delta = EntryDelta::default();
    if let Some((start, end)) = changed {
        delta.note_range(start, end);
    }
    window.sync_tracked_with_delta(&entries, &core, Some(1), Some(delta));
    assert_eq!(
        window.last_payload_work().revision_scans,
        1,
        "the finalize delta must re-read only the rewritten tail entry: {:?}",
        window.last_payload_work()
    );
}

/// P2-03 end-to-end: a settled Raw default-shell block whose only change is
/// `is_error` (and then `pending`) must repaint. Every step reuses the SAME
/// entry object, so the lines buffer / call id / payload pointers never move:
/// the old identity (without `is_error`) reuses the slot and keeps the stale
/// success paint, which this test would catch.
#[test]
fn chat_block_cache_repaints_raw_is_error_only_change() {
    let tools = Arc::new(AtomicBool::new(false));
    let mut entry = raw_tool_entry(false, false);
    let lines_ptr = match &entry {
        ChatEntry::Raw { lines, .. } => lines.as_ptr(),
        _ => unreachable!(),
    };

    let mut blocks = ChatBlockCache::new(Arc::clone(&tools), false);
    blocks.set_entries(std::slice::from_ref(&entry));
    let render = |blocks: &mut ChatBlockCache| {
        let mut rows_out = Vec::new();
        {
            let mut rows = Rows::new(&mut rows_out);
            blocks.render_block(0, 80, &mut rows);
            rows.trim();
        }
        rows_out
    };
    let before = render(&mut blocks);

    // Success -> error; only the flag moves.
    if let ChatEntry::Raw {
        render: Some(RawRender::Tool(tool)),
        ..
    } = &mut entry
    {
        tool.is_error = true;
    }
    assert_eq!(
        match &entry {
            ChatEntry::Raw { lines, .. } => lines.as_ptr(),
            _ => unreachable!(),
        },
        lines_ptr,
        "the isolation fixture must keep the same lines buffer"
    );
    blocks.set_entries(std::slice::from_ref(&entry));
    let error_rows = render(&mut blocks);
    let mut fresh = ChatContainer::new();
    fresh.set_tools_expanded(Arc::clone(&tools));
    fresh.set_entries(std::slice::from_ref(&entry));
    assert_eq!(
        error_rows,
        fresh.render(80),
        "the error block must match a fresh container render"
    );
    assert_ne!(
        before, error_rows,
        "an is_error-only flip must change the painted rows"
    );

    // Error -> pending; the same entry keeps its lines buffer.
    if let ChatEntry::Raw {
        render: Some(RawRender::Tool(tool)),
        ..
    } = &mut entry
    {
        tool.is_error = false;
        tool.pending = true;
    }
    blocks.set_entries(std::slice::from_ref(&entry));
    let pending_rows = render(&mut blocks);
    let mut fresh = ChatContainer::new();
    fresh.set_tools_expanded(tools);
    fresh.set_entries(std::slice::from_ref(&entry));
    assert_eq!(
        pending_rows,
        fresh.render(80),
        "the pending block must match a fresh container render"
    );
    assert_ne!(
        error_rows, pending_rows,
        "a pending-only flip must change the painted rows"
    );
}

/// P1-1: text arrived first, thinking arrives later. The retained live
/// component REFUSES the in-place patch (its committed child has no thinking
/// block), but the entry was already mutated — the commit path must publish
/// the entry delta/epoch anyway so the fallback install sees the new thinking
/// row instead of answering from the pre-thinking render memo.
#[test]
fn thinking_after_text_commit_publishes_the_entry_delta() {
    let tools = Arc::new(AtomicBool::new(false));
    let mut entries = vec![ChatEntry::Streaming {
        live: true,
        thinking: String::new(),
        text: "text first".to_string(),
    }];
    let chat = Arc::new(Mutex::new(ChatContainer::new()));
    chat.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .set_entries(&entries);
    let blocks = Arc::new(Mutex::new(ChatBlockCache::new(Arc::clone(&tools), false)));
    blocks
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .set_entries(&entries);
    let mut window = TranscriptWindow::new();
    assert!(window.should_install(&entries, 0));
    window.mark_installed(&entries, 0);

    if let ChatEntry::Streaming { thinking, .. } = &mut entries[0] {
        thinking.push_str("late thinking");
    }
    let batch = LiveStreamBatch {
        index: 0,
        text: String::new(),
        thinking: "late thinking".to_string(),
    };
    let mut delta = EntryDelta::default();
    let epoch = AtomicU64::new(0);
    let revision = AtomicU64::new(0);
    let patched = commit_live_stream_batch(
        LiveStreamCommit {
            chat: &chat,
            chat_blocks: &blocks,
            entries: &entries,
            window_start: 0,
        },
        Some(&batch),
        &mut delta,
        &epoch,
        &revision,
    );
    assert!(
        !patched,
        "the text-only committed child cannot grow a thinking block in place"
    );
    assert_eq!(
        delta.changed,
        Some((0, 1)),
        "the refused patch must still publish the entry delta"
    );
    assert_eq!(epoch.load(Ordering::Relaxed), 1);
    assert!(
        window.should_install(&entries, epoch.load(Ordering::Relaxed)),
        "the fallback install must see the thinking mutation (old render memo would reject it)"
    );

    // The install path (set_entries on both caches) paints the late thinking.
    chat.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .set_entries(&entries);
    let mut blocks = blocks
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    blocks.set_entries(&entries);
    let mut lines = Vec::new();
    {
        let mut rows = Rows::new(&mut lines);
        blocks.render_block(0, 80, &mut rows);
        rows.trim();
    }
    assert!(
        lines.join("\n").contains("late thinking"),
        "the late thinking block must reach the painted rows: {lines:?}"
    );
}

/// P1-2: an async custom renderer result rewrites the Raw entry in place.
/// The install identity must move and publish the entry delta/epoch; the old
/// path only set `dirty`, so the next sync answered from the pre-result render
/// memo and the rendered lines never installed.
#[test]
fn custom_render_result_publishes_the_entry_and_installs() {
    let payload = Arc::new(serde_json::json!({"customType": "demo", "value": 1}));
    let key = Arc::as_ptr(&payload) as u64;
    let generation = Theme::generation();
    let inputs = RenderInputs::entry(80, generation, false);
    let mut entries = vec![ChatEntry::Raw {
        call_id: None,
        lines: vec!["default body".to_string()],
        render: Some(RawRender::Custom(RawCustomRender {
            payload: Arc::clone(&payload),
            custom_type: "demo".to_string(),
            is_message: false,
            expanded: false,
            width: 80,
            theme_generation: generation,
        })),
    }];
    let mut window = TranscriptWindow::new();
    assert!(window.should_install(&entries, 0));
    window.mark_installed(&entries, 0);

    let scheduler = RenderScheduler::<u64>::new(Duration::ZERO);
    scheduler.note_dirty(&key);
    let signal = scheduler.signal(&key);
    assert!(scheduler.request(
        key,
        RendererKind::Entry,
        inputs,
        signal,
        Some(Arc::clone(&payload))
    ));
    let due = scheduler.take_due(Instant::now());
    assert_eq!(due.len(), 1);
    let token = due[0].token;

    let mut delta = EntryDelta::default();
    let epoch = AtomicU64::new(0);
    let changed = apply_custom_render_result(
        &scheduler,
        &mut entries,
        CustomRenderResult {
            entry_key: key,
            token,
            inputs,
            outcome: CustomRenderOutcome::Lines(vec!["rendered body".to_string()]),
        },
        &mut delta,
        &epoch,
    );
    assert!(changed, "the result must replace the cached lines");
    assert_eq!(
        delta.changed,
        Some((0, 1)),
        "the in-place Raw rewrite must publish its entry"
    );
    assert_eq!(epoch.load(Ordering::Relaxed), 1);
    assert!(
        window.should_install(&entries, epoch.load(Ordering::Relaxed)),
        "the custom renderer result must get past the pre-result install key"
    );

    let tools = Arc::new(AtomicBool::new(false));
    let mut blocks = ChatBlockCache::new(tools, false);
    blocks.set_entries(&entries);
    let mut lines = Vec::new();
    {
        let mut rows = Rows::new(&mut lines);
        blocks.render_block(0, 80, &mut rows);
        rows.trim();
    }
    assert!(
        lines.join("\n").contains("rendered body"),
        "the custom renderer output must reach the block rows: {lines:?}"
    );
}

/// P2 (block metadata): a cold frame over N blocks must expand each child's
/// block list a constant number of times, not once per rendered block. The
/// old `Container::render_block_into` called `child_block_revisions` (and a
/// second `content_blocks`) inside its loop, making a 512-block band O(N²).
#[test]
fn container_block_layout_expands_each_child_once_per_frame() {
    struct CountingBlock {
        content_calls: Arc<Mutex<Vec<usize>>>,
        render_calls: Arc<Mutex<Vec<usize>>>,
        row: String,
    }

    impl Component for CountingBlock {
        fn render(&self, _width: usize) -> Vec<String> {
            vec![self.row.clone()]
        }

        fn render_into(&self, _width: usize, out: &mut Rows<'_>) {
            out.push(&self.row);
        }

        fn content_blocks(&self) -> Option<Vec<u64>> {
            self.content_calls
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(0);
            Some(vec![1])
        }

        fn render_block_into(&self, _index: usize, _width: usize, out: &mut Rows<'_>) {
            self.render_calls
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(0);
            out.push(&self.row);
        }
    }

    const CHILDREN: usize = 512;
    let content_calls = Arc::new(Mutex::new(Vec::new()));
    let render_calls = Arc::new(Mutex::new(Vec::new()));
    let mut content = Container::new();
    for index in 0..CHILDREN {
        content.add_child(CountingBlock {
            content_calls: Arc::clone(&content_calls),
            render_calls: Arc::clone(&render_calls),
            row: format!("block-{index:03}"),
        });
    }

    let terminal = Terminal::with_sink_factory(
        || Box::new(std::io::sink()) as Box<dyn TerminalSink>,
        WriteLog::disabled(),
    );
    terminal.set_size(80, 40);
    let mut core = TuiCore::new(terminal);
    core.push_content(Box::new(ReservedScrollbarColumn::new(content, false)));
    core.render();

    let content_calls = content_calls
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    assert_eq!(
        content_calls.len(),
        CHILDREN,
        "each child's block list must be expanded once for the frame, not per block"
    );
    let render_calls = render_calls
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    assert_eq!(
        render_calls.len(),
        CHILDREN,
        "every cold block must render exactly once"
    );
}

/// P2-4: Ctrl+T / hidden-label moves must not leave the block cache unprimed.
/// The old `reset()` fell back to the whole-container path until the next
/// structural install, so every streaming token re-rendered the entire band.
#[test]
fn block_cache_stays_primed_across_thinking_setting_toggles() {
    let tools = Arc::new(AtomicBool::new(false));
    let entries = vec![ChatEntry::Streaming {
        live: true,
        thinking: "formal derivation".to_string(),
        text: "answer".to_string(),
    }];
    let mut blocks = ChatBlockCache::new(Arc::clone(&tools), false);
    blocks.set_entries(&entries);
    assert!(blocks.is_primed());

    blocks.set_hide_thinking(true);
    assert!(
        blocks.is_primed(),
        "a thinking-visibility toggle must keep the block cache primed"
    );
    assert_eq!(
        blocks.block_revisions().map(|revisions| revisions.len()),
        Some(1)
    );
    let mut hidden = Vec::new();
    {
        let mut rows = Rows::new(&mut hidden);
        blocks.render_block(0, 80, &mut rows);
        rows.trim();
    }
    let hidden = hidden.join("\n");
    assert!(
        hidden.contains("Thinking...") && !hidden.contains("formal derivation"),
        "the collapsed thinking row must paint its label without the body: {hidden:?}"
    );

    blocks.set_hidden_label(Some("pondering".to_string()));
    assert!(
        blocks.is_primed(),
        "a hidden-label move must keep the block cache primed"
    );
    let mut relabelled = Vec::new();
    {
        let mut rows = Rows::new(&mut relabelled);
        blocks.render_block(0, 80, &mut rows);
        rows.trim();
    }
    assert!(
        relabelled.join("\n").contains("pondering"),
        "the relabelled thinking row must repaint: {relabelled:?}"
    );
}

/// P2 (streaming block): each delta must EXTEND the slot's own accumulator.
/// A clone-per-delta replacement resets `capacity == len` on every step (and
/// the container then falls back to a full-body re-parse); the incremental
/// path keeps the grown allocation with its spare capacity.
#[test]
fn streaming_block_patch_keeps_its_accumulator_allocation() {
    let tools = Arc::new(AtomicBool::new(false));
    let mut entries = vec![ChatEntry::Streaming {
        live: true,
        thinking: String::new(),
        text: "body-".repeat(64),
    }];
    let mut blocks = ChatBlockCache::new(tools, false);
    blocks.set_entries(&entries);
    let initial = blocks
        .block_streaming_text_tag(0)
        .expect("streaming block slot");

    for delta in ["first ", "second"] {
        if let ChatEntry::Streaming { text, .. } = &mut entries[0] {
            text.push_str(delta);
        }
        blocks.patch_streaming_delta(0, &entries[0], delta, "");
    }
    let (_, len, capacity) = blocks
        .block_streaming_text_tag(0)
        .expect("streaming block slot");
    assert_eq!(
        len,
        initial.1 + "first ".len() + "second".len(),
        "the slot accumulator must contain the appended deltas"
    );
    assert!(
        capacity > len,
        "the incremental patch must keep the slot's grown allocation \
         (capacity {capacity} > len {len}); a clone-per-delta resets it"
    );
}

/// Production integration: the real `SharedChat` wrapper over a real
/// `ChatBlockCache` inside `TuiCore`. A live-stream delta must re-render only
/// that block (the other blocks' cached rows are served untouched) and the
/// painted frame must show the grown text.
#[test]
fn production_shared_chat_rebuilds_only_the_live_block() {
    let tools = Arc::new(AtomicBool::new(false));
    let mut entries = vec![
        ChatEntry::User("first question".to_string()),
        ChatEntry::Notice {
            kind: NoticeKind::Status,
            text: "a notice".to_string(),
        },
        pending_tool_entry(),
        ChatEntry::Streaming {
            live: true,
            thinking: "some thinking".to_string(),
            text: "answer".to_string(),
        },
    ];
    let chat = Arc::new(Mutex::new(ChatContainer::new()));
    {
        let mut chat = chat
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        chat.set_tools_expanded(Arc::clone(&tools));
        chat.set_entries(&entries);
    }
    let mut blocks = ChatBlockCache::new(Arc::clone(&tools), false);
    blocks.set_entries(&entries);
    assert!(blocks.is_primed());
    let blocks = Arc::new(Mutex::new(blocks));
    let revision = Arc::new(AtomicU64::new(1));

    let terminal = Terminal::with_sink_factory(
        || Box::new(std::io::sink()) as Box<dyn TerminalSink>,
        WriteLog::disabled(),
    );
    terminal.set_size(100, 40);
    let mut core = TuiCore::new(terminal);
    core.push_content(shared_chat_component(
        Arc::clone(&chat),
        Arc::clone(&revision),
        Arc::clone(&blocks),
    ));
    core.render();
    let baseline = core.last_frame_lines().join("\n");
    assert!(
        baseline.contains("answer") && baseline.contains("first question"),
        "the production block path must paint the mixed band: {baseline:?}"
    );

    let last = entries.len() - 1;
    if let ChatEntry::Streaming { text, .. } = &mut entries[last] {
        text.push_str(" grows");
    }
    blocks
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .patch_streaming_delta(last, &entries[last], " grows", "");
    revision.fetch_add(1, Ordering::Relaxed);
    FrameWork::reset();
    core.render();
    let work = core.last_frame_work();
    assert_eq!(
        work.rebuilt_blocks, 1,
        "only the changed live block may re-render: {work:?}"
    );
    assert!(
        work.cached_rows > 0,
        "the unchanged blocks must be served from their row caches: {work:?}"
    );
    let after = core.last_frame_lines().join("\n");
    assert!(
        after.contains("answer grows"),
        "the incremental block patch must reach the frame: {after:?}"
    );
}

/// P3-02 defense-in-depth: the `start > total` reset branch must drop the
/// installed-slice key too, or an identical empty/short incoming list would
/// short-circuit its first install against the outgoing session's key.
#[test]
fn reset_window_defensive_branch_clears_installed_key() {
    let (core, _chat) = test_core();
    let mut window = TranscriptWindow::new();
    window.set_start(1);
    window.mark_installed(&[], 0);
    window.sync_tracked(&[], &core, Some(0));
    assert!(
        window.should_install(&[], 0),
        "the reset branch must forget the outgoing installed key"
    );
}
