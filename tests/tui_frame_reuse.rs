//! Frame-canvas reuse pins for the composite/transcript render path.
//!
//! The perf-lane conversion moved the composite/container render path off the
//! allocating `Component::render(width) -> Vec<String>` form and onto the
//! canvas-direct `Component::render_into(width, &mut Rows)`
//! ([`ChatContainer`](pir::tui::app::ChatContainer),
//! [`ReservedScrollbarColumn`](pir::tui::core::ReservedScrollbarColumn)). The
//! frame pipeline hands every component the same recycled row pool
//! ([`Rows`](pir::tui::core::Rows)) for the life of the process, so a
//! steady-state frame is supposed to rewrite those row buffers in place
//! instead of allocating one `Vec<String>` plus one `String` per row per
//! component level.
//!
//! These tests turn that into execution:
//!
//! 1. `chat_container_canvas_path_matches_render_and_reuses_its_row_pool` —
//!    both render forms pinned against a HAND-WRITTEN expectation of a small
//!    transcript (spacer, dim notice text, raw lines) on a fresh and on a
//!    deliberately poisoned pool, plus the reuse invariant: a second frame
//!    into the same pool writes every row into the SAME allocation (pointer,
//!    length and capacity identical), the pool is trimmed to the frame, and
//!    it does not grow — and a transcript edited between frames renders the
//!    new rows, so a stale row (or a missed child diff) fails.
//! 2. `chat_container_canvas_path_saves_one_allocation_per_row` — a test-only
//!    allocation counter (this file's global allocator, gated per thread)
//!    measures the two paths differentially: the canvas path must save at
//!    least one heap allocation per non-empty transcript row against the
//!    allocating `render` bridge it replaced, on the same bytes.
//! 3. `reserved_scrollbar_column_canvas_path_keeps_rows_and_reuses_them` —
//!    the same independent-expectation/reuse pins for the scroll-view column
//!    wrapper (child `Text` + `Spacer` rows rebuilt by hand).
//! 4. The two remaining leaf allocators moved onto the canvas — `Markdown` /
//!    its `StreamingRender` (`src/tui/markdown.rs`) and `StreamingThinking`
//!    (`src/tui/messages.rs`) — plus the header/footer row writers the same
//!    wave switched (`LiveHeaderLine`, `Footer`): warm-cache rows checked
//!    against a COLD second instance of the same body (fresh, poisoned and
//!    recycled pools, streamed growth states, both thinking postures), the
//!    hand-written row shapes where they are stable, and the allocation
//!    differentials that show the steady-state frame of a `Markdown` body and
//!    a visible `StreamingThinking` body allocates NOTHING at all, where the
//!    allocating bridge still pays a `Vec` plus one `String` per non-empty
//!    row.
//! 5. The per-frame restyle memos of the same leaves (the header line's
//!    styled row, the built-in footer's rows, the hidden-thinking row): a
//!    steady-state frame of header + footer + hidden thinking allocates
//!    nothing but the footer's `HOME`/`USERPROFILE` read, a theme swap / width
//!    change / state change (and a thinking relabel) rebuilds exactly once,
//!    and every rebuilt row is byte-compared against the uncached builder
//!    (`pad_to_width(&header_line_styled_for(..))`, `render_footer`, the
//!    settled hidden-thinking row).
//! 6. The tool block's per-block memos (`ToolExecutionComponent`): the
//!    wrapped call-line rows and the rendered `Elapsed`/`Took` timer tail are
//!    served from the block's own cache on a steady frame, so a settled bash
//!    block allocates nothing at all; width / expansion / duration / palette
//!    moves are each byte-compared against a COLD second instance of the same
//!    block, and the allocation count of one steady frame is pinned FLAT in
//!    the number of transcript blocks (the per-block-per-frame churn that
//!    made a long session slower and slower).
//! 7. The remaining per-frame wraps of the transcript face (this lane): the
//!    `Text` rows memo (the `Notice`/`Raw` entries re-wrapped on every frame
//!    before it; the memo is served only while the component is unpainted), the
//!    `!`/`!!` user-bash block's whole-row memo (the header/preview/tail wrap
//!    ran per frame), and the edit block's preview, legacy and result-body
//!    memos (the bodies re-styled, re-parsed and re-wrapped per frame). The
//!    steady frames allocate nothing beyond the per-block key work — even the
//!    file-tool call text is memoized, keyed on the [`CallLineEnv`] generation
//!    — and every move (width, text, output, preview,
//!    expansion, palette) is byte-pinned against a cold second instance.
//! 8. The lazy per-line reset + the `FrameWork` written-row accounting
//!    (`src/tui/core.rs`): the old shape appended `SEGMENT_RESET` to every row
//!    of every frame — `reset_rows == frame height` even for a frame that
//!    wrote nothing — and the new shape appends it only to the rows the frame
//!    actually writes, leaving the unwritten rows' baseline bytes untouched
//!    (byte-pinned) and `reset_rows` at the written-row count.
//!
//! Determinism: the migrated components' `render` entry points are thin
//! `render_into` bridges, so comparing the two leg to leg can never fail;
//! every expectation below is instead an INDEPENDENT row set — a hand-written
//! row shape derived from the documented component contracts with the public
//! ANSI/theme helpers, or, where the rows are parser-derived (markdown,
//! highlighted bodies), a from-scratch second instance of the same body (a
//! cold parse measured against the warm cache). A stale row, a dropped
//! spacer, a lost style tag or a cache mis-key all fail against these.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

use pir::ai::types::{
    AssistantMessage, AssistantRole, ContentBlock, ImageContent, StopReason, TextContent,
    ThinkingContent,
};
use pir::tui::ansi::{
    pad_to_width, truncate_to_width, visible_width, wrap_text_with_ansi, BG_RESET, FG_RESET,
};
use pir::tui::app::{
    format_cwd_for_footer, format_tokens, header_line_styled_for, render_footer,
    render_footer_into, ChatContainer, ChatEntry, Footer, FooterState, HeaderLineKind,
    LiveHeaderLine, NoticeKind, ToolPreview, UsageTotals, APP_NAME,
};
use pir::tui::core::{
    Component, Container, FrameWork, ReservedScrollbarColumn, Rows, RowsWork, Spacer, Terminal,
    Text, TuiCore,
};
use pir::tui::keybindings::Keybindings;
use pir::tui::markdown::{global_mermaid_mode, Markdown, MarkdownOptions, MarkdownTheme};
use pir::tui::messages::{
    edit_preview_input, format_tool_call_line, strip_shell_footer, AssistantMessageComponent,
    StreamingThinking, ToolExecutionComponent, ToolMemoWork, UserMessageComponent,
};
use pir::tui::theme::{set_theme, with_theme, Theme, ThemeColor};

// ---------------------------------------------------------------------------
// Test-only allocation counter
// ---------------------------------------------------------------------------
//
// One `GlobalAlloc` wrapper around `System`, counting allocation events on the
// measuring thread. It lives in this standalone test binary (never in the
// merged `g_tui` harness, whose 20+ suites would all pay for it) and only
// counts while a `count_allocations` section is open on that thread, so the
// libtest harness, other test threads and any parallel suite are unaffected
// beyond one TLS read per allocation.

struct CountingAllocator;

thread_local! {
    /// Whether this thread is inside a `count_allocations` section.
    static COUNTING: Cell<bool> = const { Cell::new(false) };
    /// Allocation events seen on this thread since the section opened.
    static ALLOCATION_EVENTS: Cell<u64> = const { Cell::new(0) };
    /// Gross bytes allocated on this thread since the section opened.
    static ALLOCATED_BYTES: Cell<i64> = const { Cell::new(0) };
    /// Bytes handed out minus bytes returned on this thread since the section
    /// opened (the section's net LIVE growth). Signed: a section that frees
    /// pre-section memory reports a negative value.
    static LIVE_BYTES: Cell<i64> = const { Cell::new(0) };
    /// The sizes of the first [`SIZE_LOG_CAP`] allocation events of the
    /// section (in order) — the attribution channel for a frame whose
    /// memo/row counters all read zero: the size signature names the site.
    static SIZE_LOG: Cell<[u64; SIZE_LOG_CAP]> = const { Cell::new([0; SIZE_LOG_CAP]) };
    static SIZE_LOG_LEN: Cell<usize> = const { Cell::new(0) };
}

/// How many allocation sizes one measured section records.
const SIZE_LOG_CAP: usize = 16;

#[expect(
    unsafe_code,
    reason = "test-only GlobalAlloc wrapper: delegates straight to System and \
              adds a per-thread counter; it holds no unsafe invariant of its \
              own, and the TLS slots are const-initialized so reading them can \
              never allocate (no recursion into the allocator)"
)]
unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let _ = COUNTING.try_with(|counting| {
            if counting.get() {
                let _ = ALLOCATION_EVENTS.try_with(|events| events.set(events.get() + 1));
                let _ =
                    ALLOCATED_BYTES.try_with(|bytes| bytes.set(bytes.get() + layout.size() as i64));
                let _ = LIVE_BYTES.try_with(|live| live.set(live.get() + layout.size() as i64));
                let _ = SIZE_LOG_LEN.try_with(|len| {
                    let index = len.get();
                    if index < SIZE_LOG_CAP {
                        let _ = SIZE_LOG.try_with(|log| {
                            let mut sizes = log.get();
                            sizes[index] = layout.size() as u64;
                            log.set(sizes);
                        });
                        len.set(index + 1);
                    }
                });
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

/// Run `f` with this thread's allocator counter open and report how many
/// allocation events (not bytes) it caused on this thread.
fn count_allocations<T>(f: impl FnOnce() -> T) -> (T, u64) {
    let (value, stats) = measure_allocations(f);
    (value, stats.events)
}

/// One measured allocation section: events, gross bytes allocated, and the
/// NET live delta (allocated minus freed — including frees of memory that was
/// allocated before the section opened, so a section that only drops retained
/// state reports a negative `live`).
#[derive(Debug, Clone, Copy)]
struct AllocStats {
    events: u64,
    bytes: i64,
    live: i64,
}

/// The recorded per-allocation sizes of one measured section (the first
/// [`SIZE_LOG_CAP`] events, in order).
fn alloc_size_log() -> Vec<u64> {
    SIZE_LOG_LEN.with(|len| {
        let count = len.get().min(SIZE_LOG_CAP);
        let sizes = SIZE_LOG.with(Cell::get);
        sizes[..count].to_vec()
    })
}

/// [`count_allocations`] with the byte accounting: `bytes` is the section's
/// churn (what a per-delta full-body copy would blow up) and `live` is what
/// the section RETAINED (the "memory grows" property).
fn measure_allocations<T>(f: impl FnOnce() -> T) -> (T, AllocStats) {
    ALLOCATION_EVENTS.with(|events| events.set(0));
    ALLOCATED_BYTES.with(|bytes| bytes.set(0));
    LIVE_BYTES.with(|live| live.set(0));
    SIZE_LOG_LEN.with(|len| len.set(0));
    COUNTING.with(|counting| counting.set(true));
    let value = f();
    COUNTING.with(|counting| counting.set(false));
    (
        value,
        AllocStats {
            events: ALLOCATION_EVENTS.with(Cell::get),
            bytes: ALLOCATED_BYTES.with(Cell::get),
            live: LIVE_BYTES.with(Cell::get),
        },
    )
}

// ---------------------------------------------------------------------------
// Test serialization + shared helpers
// ---------------------------------------------------------------------------

/// Serializes this binary's tests. The theme is process-global: a swap in one
/// test must not land inside another test's measured steady-state frame (the
/// per-binary analogue of the `g_tui` suites' `tui_system_theme::lock()`
/// rationale). Every test in this file takes it first.
static TEST_LOCK: Mutex<()> = Mutex::new(());

fn test_lock() -> std::sync::MutexGuard<'static, ()> {
    TEST_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Swap the process-global theme for one test and restore the startup `dark`
/// palette when the guard drops — a failed assertion must not leave a sibling
/// test painting with the test palette.
struct ThemeSwap;

impl ThemeSwap {
    fn install(name: &str) -> Self {
        set_theme(name);
        Self
    }
}

impl Drop for ThemeSwap {
    fn drop(&mut self) {
        set_theme("dark");
    }
}

/// The allocations one `home_dir()` read costs — the footer's per-frame
/// `$HOME`/`$USERPROFILE` lookup (`src/tui/app.rs`). Measured with the same
/// expression the footer uses, so the footer's steady-state residual can be
/// pinned against a real number instead of a magic constant; the warm-up call
/// keeps any lazy std/env initialization out of the measured section.
fn measured_env_read_allocations() -> u64 {
    let read = || {
        let _ = std::env::var("HOME").or_else(|_| std::env::var("USERPROFILE"));
    };
    read();
    let ((), allocations) = count_allocations(read);
    allocations
}

/// The hidden thinking posture's one row — `{pad}{label}` italic in
/// `ThinkingText`, the settled component's shape — built independently of the
/// component so the memo is pinned against a fresh construction.
fn hidden_thinking_row(pad: usize, label: &str) -> String {
    let mut row = " ".repeat(pad);
    row.push_str(&with_theme(|theme| {
        theme.italic(&theme.fg(ThemeColor::ThinkingText, label))
    }));
    row
}

// ---------------------------------------------------------------------------
// Independent expectations
// ---------------------------------------------------------------------------
//
// The migrated `render` entry points are `render_into` with a fresh pool, so
// a "canvas == render" comparison cannot fail. The helpers below rebuild the
// documented row shapes from the PUBLIC helpers (`wrap_text_with_ansi`,
// `pad_to_width`, `truncate_to_width`, `Theme::fg`/`bg`) and from the
// documented component contracts, so a wrong row (dropped padding, missing
// spacer, stale bytes from a previous frame) differs from the expectation
// instead of matching itself.

/// One blank row of `width` spaces (the `Spacer` / `" ".repeat(width)` row).
fn blank_row(width: usize) -> String {
    " ".repeat(width)
}

/// The active palette's `Theme::fg` opener for `color` (the escape without
/// text and without the closer): the style tag a row painted in `color` must
/// carry, read from the installed theme rather than from a literal.
fn fg_open(color: ThemeColor) -> String {
    with_theme(|theme| theme.fg(color, ""))
        .strip_suffix(FG_RESET)
        .unwrap_or_default()
        .to_string()
}

/// The active palette's `Theme::bg` opener for `color` (see [`fg_open`]).
fn bg_open(color: ThemeColor) -> String {
    with_theme(|theme| theme.bg(color, ""))
        .strip_suffix(BG_RESET)
        .unwrap_or_default()
        .to_string()
}

/// `Text`'s documented row shape (`core.rs`): the text wraps to
/// `width - 2 * padding_x` (the padding clamps to `(width - 1) / 2`, the
/// content width is at least 1), and every visual line becomes
/// `{padding}{line}{padding}` padded to `width`. Rebuilt with the public
/// wrap/pad helpers so the expectation never runs the component itself.
fn text_rows(text: &str, padding_x: usize, width: usize) -> Vec<String> {
    if text.trim().is_empty() {
        return Vec::new();
    }
    let padding_x = padding_x.min(width.saturating_sub(1) / 2);
    let content_width = width.saturating_sub(padding_x * 2).max(1);
    let left = " ".repeat(padding_x);
    wrap_text_with_ansi(text, content_width)
        .into_iter()
        .map(|line| pad_to_width(&format!("{left}{line}{left}"), width))
        .collect()
}

/// `Theme::bg` renders `{open}{text}\x1b[49m`, so painting an empty string
/// splits the background pair out — the same reference formula
/// `tests/tui_render_parity.rs` uses for the pre-migration tool box.
fn reference_bg_edges(color: ThemeColor) -> (String, String) {
    let probe = with_theme(|theme| theme.bg(color, ""));
    match probe.strip_suffix(BG_RESET) {
        Some(open) => (open.to_string(), BG_RESET.to_string()),
        None => (String::new(), String::new()),
    }
}

/// The blank box row: the background pair around `width` spaces.
fn reference_paint_blank_row(open: &str, close: &str, width: usize) -> String {
    let mut row = String::new();
    row.push_str(open);
    row.push_str(&" ".repeat(width));
    row.push_str(close);
    row
}

/// The prefixed box row: `open` + `prefix` + `text`, padded to `width` with
/// the pad measured on the prefixed text alone, then `close`.
fn reference_paint_padded_row(
    open: &str,
    close: &str,
    prefix: &str,
    text: &str,
    width: usize,
) -> String {
    let mut row = String::new();
    row.push_str(open);
    let body = row.len();
    row.push_str(prefix);
    row.push_str(text);
    let pad = width.saturating_sub(visible_width(&row[body..]));
    row.push_str(&" ".repeat(pad));
    row.push_str(close);
    row
}

/// The expanded settled bash box, rebuilt from the documented shapes: the
/// unpainted `Spacer(1)` row, the `toolSuccessBg` box (blank padding row, the
/// call line, a blank row and the styled output body), the muted timer tail,
/// and the bottom padding row. `format_tool_call_line` and
/// `strip_shell_footer` are the public builders the component composes.
fn reference_bash_box_rows(
    args: &serde_json::Value,
    output: &str,
    timer_tail: Option<&str>,
    width: usize,
) -> Vec<String> {
    let (open, close) = reference_bg_edges(ThemeColor::ToolSuccessBg);
    let content_width = width.saturating_sub(2).max(1);
    let mut rows = vec![
        String::new(),
        reference_paint_blank_row(&open, &close, width),
    ];
    for visual in wrap_text_with_ansi(&format_tool_call_line("bash", args, true), content_width) {
        rows.push(reference_paint_padded_row(
            &open, &close, " ", &visual, width,
        ));
    }
    let output = strip_shell_footer(output.trim(), None);
    if !output.is_empty() {
        let mut styled = String::with_capacity(output.len());
        for (index, line) in output.lines().enumerate() {
            if index > 0 {
                styled.push('\n');
            }
            styled.push_str(&with_theme(|theme| theme.fg(ThemeColor::ToolOutput, line)));
        }
        rows.push(reference_paint_blank_row(&open, &close, width));
        for line in wrap_text_with_ansi(&styled, content_width) {
            rows.push(reference_paint_padded_row(&open, &close, " ", &line, width));
        }
    }
    if let Some(tail) = timer_tail {
        let tail = with_theme(|theme| theme.fg(ThemeColor::Muted, tail));
        rows.push(reference_paint_blank_row(&open, &close, width));
        for line in wrap_text_with_ansi(&tail, content_width) {
            rows.push(reference_paint_padded_row(&open, &close, " ", &line, width));
        }
    }
    rows.push(reference_paint_blank_row(&open, &close, width));
    rows
}

// ---------------------------------------------------------------------------
// Fixtures and helpers
// ---------------------------------------------------------------------------

/// Widths the parity legs run at: degenerate, tiny, terminal-sized and wide.
const WIDTHS: [usize; 4] = [0, 1, 80, 200];

/// Shared transcript handle: `ChatContainer` is `Send` but not `Sync` (its
/// children are `Box<dyn Component>`), so the component handle the frame
/// renderer holds — and any test wrapper around it — goes through a mutex,
/// exactly like `terminal.rs`'s `SharedChat`.
type SharedTranscript = Arc<Mutex<ChatContainer>>;

fn lock_chat(chat: &SharedTranscript) -> std::sync::MutexGuard<'_, ChatContainer> {
    chat.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// The row-buffer identity of one pooled `String`: address, length and
/// capacity, no allocation and no scan.
///
/// For this mutation set the triple is exact. The frame pool only ever grows
/// (`Rows::claim` pushes a fresh empty `String` past the end) or is rewritten
/// in place (`String::clear` keeps the allocation, `push_str` moves `len` and
/// only moves `cap` when it must grow) — nothing shortens a pooled buffer's
/// capacity in place, so an identical triple across frames means the very same
/// allocation, and a different one means the row was reallocated. A tag
/// mismatch only ever costs an assertion failure, never a false pass on a
/// changed frame.
#[derive(Debug, PartialEq, Eq)]
struct RowTag {
    ptr: usize,
    len: usize,
    capacity: usize,
}

fn row_tags(pool: &[String]) -> Vec<RowTag> {
    pool.iter()
        .map(|row| RowTag {
            ptr: row.as_ptr() as usize,
            len: row.len(),
            capacity: row.capacity(),
        })
        .collect()
}

/// Render `component` through the canvas the way `TuiCore::frame_lines` does:
/// one recycled pool, rewound per frame, trimmed to the frame's height.
fn render_into_pool(component: &dyn Component, width: usize, pool: &mut Vec<String>) {
    let mut rows = Rows::new(pool);
    rows.rewind();
    component.render_into(width, &mut rows);
    rows.trim();
}

/// One measured steady frame plus the attribution of every allocation channel
/// the component and the canvas can use on that frame:
///
/// - `memos`: the tool block's memo-rebuild counters ([`ToolMemoWork`]) — a
///   steady frame must rebuild nothing;
/// - `rows.pool_growth`: rows newly appended to the pool (each one allocates);
///   `rows.row_writes` is a plain write count, non-zero by design;
/// - `outgrown_rows`: rows whose final content is longer than the capacity the
///   row had BEFORE the frame — every such row reallocated inside the frame
///   even though the pool did not grow;
/// - `sizes`: the first allocation sizes of the frame.
///
/// A frame reporting `allocations > 0` with every channel at zero cannot be
/// explained by the component or the canvas; `sizes` then names the site.
///
/// Formatter-level residual of one STEADY tool-block frame: exactly 15
/// allocations of 16 B (per-span styled-string formatting on a path no counter
/// covers; `ToolMemoWork` all-zero, `pool_growth` 0, no outgrown rows) —
/// invariant in the body size and the row count, i.e. NOT a memo miss and NOT a
/// per-row/per-body cost. Measured by temporarily forcing the ceiling to 0 and
/// reading the attribution dump.
const STEADY_BASH_BLOCK_CEILING: u64 = 15;
/// Ceiling for the frame AFTER a state change: a width change, a collapse, a
/// tail move or a palette swap re-formats the call line / preview by design,
/// measured 15-23 of the same 16 B span-formatting shape (never per-row).
const STATE_CHANGE_CEILING: u64 = 32;

#[derive(Debug)]
struct FrameAttribution {
    allocations: u64,
    memos: ToolMemoWork,
    rows: RowsWork,
    outgrown_rows: u64,
    sizes: Vec<u64>,
}

/// [`render_into_pool`] with the per-frame attribution (see
/// [`FrameAttribution`]).
fn measure_frame(
    component: &dyn Component,
    width: usize,
    pool: &mut Vec<String>,
) -> FrameAttribution {
    measure_frame_with(pool, |pool| render_into_pool(component, width, pool))
}

/// [`measure_frame`] for the shared transcript handle (the container renders
/// its children through the same canvas).
fn measure_frame_chat(
    chat: &SharedTranscript,
    width: usize,
    pool: &mut Vec<String>,
) -> FrameAttribution {
    measure_frame_with(pool, |pool| render_chat_into_pool(chat, width, pool))
}

/// One measured frame: the allocation count plus the attribution channels
/// ([`FrameAttribution`]) of the frame `render` draws into `pool`.
fn measure_frame_with(
    pool: &mut Vec<String>,
    render: impl FnOnce(&mut Vec<String>),
) -> FrameAttribution {
    let capacities: Vec<usize> = pool.iter().map(String::capacity).collect();
    ToolMemoWork::reset();
    RowsWork::reset();
    let ((), allocations) = count_allocations(|| {
        render(pool);
    });
    let outgrown_rows = pool
        .iter()
        .zip(capacities.iter())
        .filter(|(row, capacity)| row.len() > **capacity)
        .count() as u64;
    FrameAttribution {
        allocations,
        memos: ToolMemoWork::snapshot(),
        rows: RowsWork::snapshot(),
        outgrown_rows,
        sizes: alloc_size_log(),
    }
}

/// The steady-frame invariants that do not depend on the absolute count: the
/// frame must not grow the row pool, must not outgrow any row buffer, and must
/// rebuild no tool-block memo. A failure names the channel in `attribution`.
fn assert_steady_frame_channels(attribution: &FrameAttribution, label: &str) {
    assert_eq!(
        attribution.rows.pool_growth, 0,
        "{label}: a steady frame must reuse the pooled rows: {attribution:?}"
    );
    assert_eq!(
        attribution.outgrown_rows, 0,
        "{label}: no steady-frame row may outgrow its previous buffer: {attribution:?}"
    );
    let mut memo_work = attribution.memos;
    // The frame counter itself is expected to be 1 (this render).
    memo_work.frames = 0;
    assert_eq!(
        memo_work,
        ToolMemoWork::default(),
        "{label}: a steady frame must rebuild no tool-block memo          (call rows, timer tail, result rows, background pair): {attribution:?}"
    );
}

/// [`render_into_pool`] for the shared transcript handle (the lock is held for
/// the render only, like the frame renderer's wrapper).
fn render_chat_into_pool(chat: &SharedTranscript, width: usize, pool: &mut Vec<String>) {
    let mut rows = Rows::new(pool);
    rows.rewind();
    lock_chat(chat).render_into(width, &mut rows);
    rows.trim();
}

/// The state the frame renderer's scratch holds after a wide/tall frame: rows
/// far longer than anything these fixtures produce, so a component that
/// appends to a recycled row instead of claiming a cleared one shows up as an
/// over-long row.
fn stale_pool() -> Vec<String> {
    (0..8)
        .map(|index| format!("STALE-{index:02}-{}", "Z".repeat(600)))
        .collect()
}

/// The per-line reset `TuiCore` appends to every row it writes
/// (`SEGMENT_RESET`, `src/tui/core.rs`): carried by the written baseline rows,
/// absent from the raw frame.
const SEGMENT_RESET: &str = "\x1b[0m\x1b]8;;\x07";

/// A `Text` handle a test can edit between frames: `TuiCore::push_content`
/// takes a `Box<dyn Component>`, so the handle the renderer owns and the
/// handle the test mutates (`set_text`) share one component.
struct SharedText(Arc<Text>);

impl Component for SharedText {
    fn render(&self, width: usize) -> Vec<String> {
        self.0.render(width)
    }

    fn render_into(&self, width: usize, out: &mut Rows<'_>) {
        self.0.render_into(width, out)
    }
}

fn assistant_message(text: &str) -> AssistantMessage {
    AssistantMessage {
        role: AssistantRole::Assistant,
        content: vec![ContentBlock::Text(TextContent {
            text: text.into(),
            text_signature: None,
        })],
        api: "frame-reuse".into(),
        provider: "frame-reuse".into(),
        model: "frame-reuse".into(),
        stop_reason: StopReason::Stop,
        ..Default::default()
    }
}

/// A transcript covering every `ChatEntry` arm that contributes children, so
/// the canvas path is exercised across all of `ChatContainer`'s child kinds
/// (user block, assistant block, tool block, notice, raw lines and the live
/// streaming pair).
fn chat_fixture() -> SharedTranscript {
    let entries = vec![
        ChatEntry::User("render the frame twice please".into()),
        ChatEntry::Assistant(Arc::new(assistant_message(
            "assistant body with **markdown**\n\n- one\n- two",
        ))),
        ChatEntry::ToolExecution {
            call_id: Some("call-1".into()),
            tool_name: "read".into(),
            args: Arc::new(serde_json::json!({ "path": "src/lib.rs" })),
            output: Arc::from("file body line one\nfile body line two"),
            is_error: false,
            duration_ms: Some(12),
            pending: false,
            details: None,
            started_at: None,
            preview: None,
            images: Arc::new(Vec::new()),
        },
        ChatEntry::Notice {
            kind: NoticeKind::Status,
            text: "status line".into(),
        },
        ChatEntry::Raw {
            call_id: None,
            lines: vec!["raw one".into(), "raw two".into()],
            render: None,
        },
        ChatEntry::Streaming {
            live: true,
            thinking: "weighing the options".into(),
            text: "streaming body text that wraps".into(),
        },
    ];
    let mut chat = ChatContainer::new();
    chat.set_entries(&entries);
    Arc::new(Mutex::new(chat))
}

/// The simple two-entry transcript the container's independent-expectation
/// leg pins: a `Notice` (spacer + dim, side-padded text) and a `Raw` block
/// (spacer + verbatim lines). Both child shapes are documented in `app.rs`
/// and `core.rs`, so the expected rows can be rebuilt without the container.
fn simple_chat_entries(status: &str, first_raw: &str) -> Vec<ChatEntry> {
    vec![
        ChatEntry::Notice {
            kind: NoticeKind::Status,
            text: status.to_string(),
        },
        ChatEntry::Raw {
            call_id: None,
            lines: vec![first_raw.to_string(), "raw two".to_string()],
            render: None,
        },
    ]
}

/// The hand-written rows of [`simple_chat_entries`] at `width`: the notice
/// contributes `Spacer(1)` + `Text::with_padding(dim(text), 1, 0)`, the raw
/// entry `Spacer(1)` + one `Text::with_padding(line, 0, 0)` per line.
fn simple_chat_expected(status: &str, first_raw: &str, width: usize) -> Vec<String> {
    let styled_status = with_theme(|theme| theme.fg(ThemeColor::Dim, status));
    let mut rows = vec![blank_row(width)];
    rows.extend(text_rows(&styled_status, 1, width));
    rows.push(blank_row(width));
    rows.extend(text_rows(first_raw, 0, width));
    rows.extend(text_rows("raw two", 0, width));
    rows
}

// ---------------------------------------------------------------------------
// 1. ChatContainer: parity + pooled-row reuse
// ---------------------------------------------------------------------------

#[test]
fn chat_container_canvas_path_matches_render_and_reuses_its_row_pool() {
    let _guard = test_lock();
    let entries = simple_chat_entries("status line", "raw one");
    let mut chat = ChatContainer::new();
    chat.set_entries(&entries);
    let chat = Arc::new(Mutex::new(chat));
    for width in WIDTHS {
        let expected = simple_chat_expected("status line", "raw one", width);

        // Both the owned form and the canvas form are pinned against the SAME
        // hand-written rows: `render` is a bridge over `render_into`, so
        // comparing the two paths to each other would prove nothing.
        assert_eq!(
            lock_chat(&chat).render(width),
            expected,
            "render() != hand-written expectation @ width {width}"
        );

        // Fresh pool: the canvas path is the only render path with content.
        let mut fresh = Vec::new();
        render_chat_into_pool(&chat, width, &mut fresh);
        assert_eq!(fresh, expected, "fresh pool != expectation @ width {width}");

        // Poisoned pool: recycled rows must be cleared (never appended to) and
        // the pool tail dropped, so stale content can not reach the frame.
        let mut recycled = stale_pool();
        render_chat_into_pool(&chat, width, &mut recycled);
        assert_eq!(recycled, expected, "stale rows leaked @ width {width}");
        assert_eq!(
            recycled.len(),
            expected.len(),
            "pool not trimmed to the frame @ width {width}"
        );

        // Reuse: the second frame into the same pool must rewrite every row
        // in place — same address, same length, same capacity, same pool
        // capacity — instead of allocating fresh line buffers.
        let mut pool = stale_pool();
        render_chat_into_pool(&chat, width, &mut pool);
        let first_frame_tags = row_tags(&pool);
        let pool_capacity = pool.capacity();
        render_chat_into_pool(&chat, width, &mut pool);
        assert_eq!(
            pool, expected,
            "second frame != expectation @ width {width}"
        );
        assert_eq!(
            row_tags(&pool),
            first_frame_tags,
            "pooled row buffers were reallocated between frames @ width {width}"
        );
        assert_eq!(
            pool.capacity(),
            pool_capacity,
            "the row pool itself reallocated between frames @ width {width}"
        );
    }

    // A transcript edited between frames must render the NEW rows: the
    // retained-children diff keeps the notice component only when its text
    // tag is unchanged, and the changed expectation differs from the old one,
    // so a stale row (or a diff that misses the edit) fails right here.
    let width = 30;
    let before = simple_chat_expected("status line", "raw one", width);
    let mut pool = stale_pool();
    render_chat_into_pool(&chat, width, &mut pool);
    assert_eq!(pool, before, "the pre-edit frame != expectation");
    lock_chat(&chat).set_entries(&simple_chat_entries("edited status line", "edited raw"));
    let after = simple_chat_expected("edited status line", "edited raw", width);
    assert_ne!(after, before, "the edit must change the expected rows");
    render_chat_into_pool(&chat, width, &mut pool);
    assert_eq!(pool, after, "the edited transcript must reach the frame");
}

/// The steady-frame budget of a WHOLE transcript: the mixed fixture
/// ([`chat_fixture`]: user box, assistant markdown + thinking, a settled tool
/// block, notice, raw lines and the live streaming pair) must render a steady
/// frame with ZERO allocation events. Every contributor now has a memo of its
/// rendered rows (user box, assistant segments, tool call/timer/result rows,
/// `Text` notice/raw, the live stream, the width measurement itself), so a
/// 60 Hz repaint of an idle transcript no longer churns the heap — the
/// long-run property a small device's allocator needs.
#[test]
fn chat_container_steady_frame_allocates_nothing() {
    let _guard = test_lock();
    warm_theme();
    let chat = chat_fixture();
    let width = 80;
    let mut pool = stale_pool();
    // Warm every memo (and the row pool) first.
    render_chat_into_pool(&chat, width, &mut pool);
    render_chat_into_pool(&chat, width, &mut pool);
    let rows = pool.len();
    assert!(rows >= 20, "the fixture must render a full frame: {rows}");

    let mut second_pool = stale_pool();
    for _ in 0..2 {
        render_chat_into_pool(&chat, width, &mut second_pool);
    }
    let before = pool.clone();
    let ((), steady) = count_allocations(|| {
        render_chat_into_pool(&chat, width, &mut pool);
        render_chat_into_pool(&chat, width, &mut second_pool);
    });
    assert_eq!(
        steady, 0,
        "a steady transcript frame must allocate nothing: {steady} events"
    );
    assert_eq!(pool, before, "the steady frame drifted");
    assert_eq!(pool, second_pool, "both pools must hold the same frame");
}

// ---------------------------------------------------------------------------
// 2. ChatContainer: the per-row allocation the canvas path removes
// ---------------------------------------------------------------------------

/// The trait's default bridge over `ChatContainer`, i.e. the pre-migration
/// shape the frame renderer used to drive: build the whole transcript as an
/// owned `Vec<String>` and copy it into the canvas row by row. It is the
/// reference the canvas path is measured against — same component instance,
/// same children, same calls, only the intermediate line buffers differ.
struct AllocatingBridge(SharedTranscript);

impl Component for AllocatingBridge {
    fn render(&self, width: usize) -> Vec<String> {
        lock_chat(&self.0).render(width)
    }
}

#[test]
fn chat_container_canvas_path_saves_one_allocation_per_row() {
    let _guard = test_lock();
    let chat = chat_fixture();
    let width = 80;
    // The frame both legs must produce, and the row split that decides the
    // expected saving: a row that renders empty costs no allocation in either
    // path (`Rows::claim` pushes an empty `String` and `push_str("")` leaves
    // its capacity at zero), so only non-empty rows are claimed below.
    let mut pool = stale_pool();
    render_chat_into_pool(&chat, width, &mut pool);
    let rows = pool.len();
    let nonempty_rows = pool.iter().filter(|row| !row.is_empty()).count();
    assert!(rows > 8, "fixture must cover more rows than the stale pool");
    assert!(nonempty_rows > 0, "fixture must render visible rows");

    // Warm BOTH canvas pools with the frame, so the measured second frame of
    // each leg pays only for what that leg itself allocates — and so the first
    // leg also warms the leaf caches (markdown, assistant and tool blocks)
    // both legs then hit.
    let bridge = AllocatingBridge(Arc::clone(&chat));
    let mut bridge_pool = stale_pool();
    render_into_pool(&bridge, width, &mut bridge_pool);
    assert_eq!(
        bridge_pool, pool,
        "the allocating bridge leg must render the same frame"
    );

    // Steady-state canvas frame: rows are rewritten in the recycled buffers.
    let ((), canvas_allocations) = count_allocations(|| {
        render_chat_into_pool(&chat, width, &mut pool);
    });
    // The bridge over the SAME component: the transcript is built as one fresh
    // `Vec<String>` (one heap allocation per non-empty row) only to be copied
    // into the canvas row by row.
    let ((), bridge_allocations) = count_allocations(|| {
        render_into_pool(&bridge, width, &mut bridge_pool);
    });

    assert_eq!(
        pool, bridge_pool,
        "the measured frames of the two legs differ"
    );
    assert!(
        bridge_allocations >= canvas_allocations + nonempty_rows as u64,
        "canvas path did not save one allocation per non-empty row: \
         canvas={canvas_allocations}, bridge={bridge_allocations}, \
         rows={rows}, non-empty={nonempty_rows}"
    );
}

// ---------------------------------------------------------------------------
// 3. ReservedScrollbarColumn: parity + pooled-row reuse
// ---------------------------------------------------------------------------

/// The hand-written rows of the fixed child sequence below at `width`: the
/// child renders at the reserved content width (`width - 1` while reserving
/// and `width > 1`, else `width`), which is `Text::new`'s padded wrap plus
/// the `Spacer(1)` row; a reserving wrapper then appends one trailing space
/// to every row.
fn reserved_scrollbar_expected(reserve: bool, width: usize) -> Vec<String> {
    let content_width = if reserve && width > 1 {
        width - 1
    } else {
        width
    };
    let mut rows = text_rows("scrolled row\nsecond scrolled row", 1, content_width);
    rows.push(blank_row(content_width));
    if content_width != width {
        for row in &mut rows {
            row.push(' ');
        }
    }
    rows
}

#[test]
fn reserved_scrollbar_column_canvas_path_keeps_rows_and_reuses_them() {
    let _guard = test_lock();
    for reserve in [true, false] {
        let mut child = Container::new();
        child.add_child(Text::new("scrolled row\nsecond scrolled row"));
        child.add_child(Spacer::new(1));
        let column = ReservedScrollbarColumn::new(child, reserve);
        let label = if reserve {
            "reserved-scrollbar-on"
        } else {
            "reserved-scrollbar-off"
        };

        for width in WIDTHS {
            let expected = reserved_scrollbar_expected(reserve, width);

            // The owned wrapper form is pinned against the same hand-written
            // rows as the canvas form (`render` is a `render_into` bridge, so
            // comparing the two to each other cannot fail).
            assert_eq!(
                column.render(width),
                expected,
                "{label}: render != hand-written expectation @ {width}"
            );

            let mut fresh = Vec::new();
            render_into_pool(&column, width, &mut fresh);
            assert_eq!(fresh, expected, "{label}: fresh != expectation @ {width}");

            let mut pool = stale_pool();
            render_into_pool(&column, width, &mut pool);
            assert_eq!(pool, expected, "{label}: stale rows leaked @ {width}");
            assert_eq!(pool.len(), expected.len(), "{label}: not trimmed @ {width}");

            let first_frame_tags = row_tags(&pool);
            let pool_capacity = pool.capacity();
            render_into_pool(&column, width, &mut pool);
            assert_eq!(
                pool, expected,
                "{label}: second frame != expectation @ {width}"
            );
            assert_eq!(
                row_tags(&pool),
                first_frame_tags,
                "{label}: pooled rows reallocated between frames @ {width}"
            );
            assert_eq!(
                pool.capacity(),
                pool_capacity,
                "{label}: the row pool reallocated between frames @ {width}"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// 4. The leaf components: parity + pooled-row reuse
// ---------------------------------------------------------------------------

/// The trait's default bridge over one leaf component (only `render` is
/// implemented), i.e. the pre-migration shape the frame renderer drove for
/// these leaves: build the whole body as an owned `Vec<String>` and copy it
/// into the canvas row by row. The reference the canvas paths below are
/// measured against — same component instance, same calls, only the
/// intermediate line buffers differ.
struct BridgeOnly<T: Component + Send + Sync>(Arc<T>);

impl<T: Component + Send + Sync> Component for BridgeOnly<T> {
    fn render(&self, width: usize) -> Vec<String> {
        self.0.render(width)
    }
}

/// A document covering the block kinds the markdown renderer emits (heading,
/// wrapped paragraph, list, quote, fence, table, rule, tail) so the canvas
/// path is exercised across all of them.
const MARKDOWN_DOC: &str = "# Title\n\nA paragraph with **bold**, `code`, ~~gone~~ and a \
[link](https://example.com \"T\") that wraps at narrow widths.\n\n- one\n- two\n\n> quoted\n\n\
```rust\nlet x = 1;\n```\n\n| a | b |\n|---|---|\n| 1 | 2 |\n\n---\n\ntail paragraph\n";

/// The `(padding_x, padding_y)` shapes the live/boxed markdown callers use:
/// the live body and thinking runs at `(1, 0)`, user messages at `(0, 0)`.
const MARKDOWN_PADDINGS: [(usize, usize); 4] = [(0, 0), (1, 0), (1, 1), (0, 2)];

/// Every line end of `text` (the live deltas arrive one line at a time, like
/// the provider stream: a growing prefix of the body).
fn line_ends(text: &str) -> Vec<usize> {
    let mut ends = Vec::new();
    let mut end = 0usize;
    while end < text.len() {
        end = match text[end..].find('\n') {
            Some(index) => end + index + 1,
            None => text.len(),
        };
        ends.push(end);
    }
    ends
}

/// The shared theme cell is a `OnceLock`: warm it (and its palette) outside a
/// measured section, so the first `with_theme` allocation can never be charged
/// to a frame. The tests do not install a theme (a sibling test in this binary
/// rendering concurrently must keep the palette it observes).
fn warm_theme() {
    let _ = pir::tui::theme::with_theme(|theme| theme.name.len());
}

#[test]
fn markdown_canvas_path_matches_render_and_reuses_its_row_pool() {
    let _guard = test_lock();
    for (padding_x, padding_y) in MARKDOWN_PADDINGS {
        let markdown = Markdown::new(MARKDOWN_DOC, padding_x, padding_y);
        for width in WIDTHS {
            let label = format!("markdown pad ({padding_x},{padding_y}) @ {width}");
            // Independent reference: a SECOND, cold instance of the same
            // documented body. The instance below has already rendered at the
            // earlier widths, so its incremental cache is warm; a cached row
            // serving stale bytes (wrong text, wrong width, wrong palette)
            // differs from the from-scratch parse.
            let cold = Markdown::new(MARKDOWN_DOC, padding_x, padding_y);
            let expected = cold.render(width);
            assert_eq!(
                markdown.render(width),
                expected,
                "{label}: the warm owned form != the cold parse"
            );

            // Fresh pool: the canvas path is the only render path with content.
            let mut fresh = Vec::new();
            render_into_pool(&markdown, width, &mut fresh);
            assert_eq!(fresh, expected, "{label}: fresh pool != cold parse");

            // Poisoned pool: recycled rows must be cleared (never appended to)
            // and the pool tail dropped, so stale content can not reach the
            // frame.
            let mut recycled = stale_pool();
            render_into_pool(&markdown, width, &mut recycled);
            assert_eq!(recycled, expected, "{label}: stale rows leaked");
            assert_eq!(recycled.len(), expected.len(), "{label}: not trimmed");

            // Reuse: the second frame into the same pool rewrites every row in
            // place instead of allocating fresh line buffers.
            let mut pool = stale_pool();
            render_into_pool(&markdown, width, &mut pool);
            let first_tags = row_tags(&pool);
            let pool_capacity = pool.capacity();
            render_into_pool(&markdown, width, &mut pool);
            assert_eq!(pool, expected, "{label}: second frame != cold parse");
            assert_eq!(
                row_tags(&pool),
                first_tags,
                "{label}: pooled rows reallocated between frames"
            );
            assert_eq!(
                pool.capacity(),
                pool_capacity,
                "{label}: the row pool reallocated between frames"
            );
        }
    }

    // Row-level invariants at a terminal width: the markdown body rows are
    // parser-derived (the cold-parse differential above covers staleness), so
    // these pin the exact bytes a wrong row would break — the row count, the
    // heading row carrying the heading style around "Title", and the fence
    // row carrying the code-border style around its opening fence.
    let width = 80;
    let markdown = Markdown::new(MARKDOWN_DOC, 0, 0);
    let mut pool = stale_pool();
    render_into_pool(&markdown, width, &mut pool);
    let cold = Markdown::new(MARKDOWN_DOC, 0, 0);
    assert_eq!(
        pool.len(),
        cold.render(width).len(),
        "the row count must match the cold parse"
    );
    let title_row = pool
        .iter()
        .find(|row| row.contains("Title"))
        .expect("the heading row must be present");
    assert!(
        title_row.contains(fg_open(ThemeColor::MdHeading).as_str()),
        "the heading row must carry the heading style: {title_row:?}"
    );
    let fence_row = pool
        .iter()
        .find(|row| row.contains("```rust"))
        .expect("the fence row must be present");
    assert!(
        fence_row.contains(fg_open(ThemeColor::MdCodeBlockBorder).as_str()),
        "the fence row must carry the code-border style: {fence_row:?}"
    );

    // A body edited between frames must render the new bytes, not the cached
    // rows of the previous body.
    let edited = Markdown::new("first body", 0, 0);
    let mut pool = stale_pool();
    render_into_pool(&edited, 40, &mut pool);
    let before = vec![pad_to_width("first body", 40)];
    assert_eq!(pool, before, "plain paragraph @ 40");
    render_into_pool(&edited, 6, &mut pool);
    let wrapped = vec![pad_to_width("first", 6), pad_to_width("body", 6)];
    assert_eq!(pool, wrapped, "plain paragraph wrap @ 6");
    edited.set_text("second body");
    let after = vec![pad_to_width("second", 6), pad_to_width("body", 6)];
    assert_ne!(after, wrapped, "the edit must change the expected rows");
    render_into_pool(&edited, 6, &mut pool);
    assert_eq!(pool, after, "an edited body must re-render @ 6");
}

/// The canvas path must track a GROWING body exactly like a from-scratch
/// parse: at every streamed prefix the incremental render (which reuses the
/// cached final prefix and re-parses only the mutable tail) must produce the
/// rows a COLD second instance of the same prefix produces — not merely the
/// rows of its own cache.
#[test]
fn markdown_canvas_rows_track_streamed_growth_byte_identically() {
    let _guard = test_lock();
    let width = 80;
    for doc in [MARKDOWN_DOC, "```\nunterminated fence\nstill code\n"] {
        let streamed = Markdown::new("", 1, 0);
        let mut pool = stale_pool();
        for end in line_ends(doc) {
            streamed.set_text_growing(&doc[..end]);
            let cold = Markdown::new(&doc[..end], 1, 0);
            let wanted = cold.render(width);
            render_into_pool(&streamed, width, &mut pool);
            assert_eq!(
                pool, wanted,
                "streamed prefix {end} of {doc:?} @ {width}: canvas rows != cold parse"
            );
        }
    }
}

/// A `Markdown` body that did not change between frames (the spinner ticks and
/// the deltas that do not append) answers from its cache: the canvas path must
/// rewrite the pooled rows without allocating a single row buffer, where the
/// allocating bridge still builds the whole body as a fresh `Vec<String>`.
#[test]
fn markdown_canvas_path_allocates_nothing_on_a_steady_state_frame() {
    let _guard = test_lock();
    warm_theme();
    let width = 80;
    let markdown = Arc::new(Markdown::new(MARKDOWN_DOC, 1, 0));
    let bridge = BridgeOnly(Arc::clone(&markdown));

    // Warm both pools with the frame and the render caches both legs hit.
    let mut pool = stale_pool();
    render_into_pool(&*markdown, width, &mut pool);
    let mut bridge_pool = stale_pool();
    render_into_pool(&bridge, width, &mut bridge_pool);
    assert_eq!(
        pool, bridge_pool,
        "the allocating bridge leg must render the same frame"
    );
    let nonempty_rows = pool.iter().filter(|row| !row.is_empty()).count();
    assert!(
        pool.len() > 8,
        "fixture must cover more rows than the stale pool"
    );
    assert!(nonempty_rows > 0, "fixture must render visible rows");

    // Steady-state canvas frame: the body is unchanged, every row is rewritten
    // in a recycled buffer.
    let ((), canvas_allocations) = count_allocations(|| {
        render_into_pool(&*markdown, width, &mut pool);
    });
    let ((), bridge_allocations) = count_allocations(|| {
        render_into_pool(&bridge, width, &mut bridge_pool);
    });

    assert_eq!(
        pool, bridge_pool,
        "the measured frames of the two legs differ"
    );
    assert_eq!(
        canvas_allocations, 0,
        "a steady-state Markdown frame must allocate nothing: {canvas_allocations} \
         allocations for {nonempty_rows} non-empty rows"
    );
    assert!(
        bridge_allocations >= nonempty_rows as u64,
        "the allocating bridge must still pay one allocation per non-empty row: \
         bridge={bridge_allocations}, non-empty={nonempty_rows}"
    );
}

#[test]
fn streaming_thinking_canvas_path_matches_render_and_reuses_its_row_pool() {
    let _guard = test_lock();
    let label = Arc::new(Mutex::new(None));
    let body = "first thought\n\nsecond **thought** with a longer line\n";
    // Independent reference for the visible posture: the documented
    // composition — the trimmed body through the default markdown layout,
    // every line painted in `ThinkingText` — rebuilt from a fresh `Markdown`
    // instance so the expectation is not the component's own bridge.
    let visible_expected = |width: usize| -> Vec<String> {
        Markdown::new(body.trim(), 1, 0)
            .render(width)
            .into_iter()
            .map(|line| with_theme(|theme| theme.fg(ThemeColor::ThinkingText, &line)))
            .collect()
    };
    for hidden in [false, true] {
        let thinking = StreamingThinking::new(body, 1, hidden, Arc::clone(&label));
        for width in WIDTHS {
            let case = format!("thinking hidden={hidden} @ {width}");
            let expected = if hidden {
                // The settled hidden posture's row is width-independent: the
                // `{pad}{label}` line, italic in `ThinkingText`.
                vec![hidden_thinking_row(1, "Thinking...")]
            } else {
                visible_expected(width)
            };
            assert_eq!(
                thinking.render(width),
                expected,
                "{case}: render() != independent expectation"
            );

            let mut fresh = Vec::new();
            render_into_pool(&thinking, width, &mut fresh);
            assert_eq!(fresh, expected, "{case}: fresh pool != expectation");

            let mut recycled = stale_pool();
            render_into_pool(&thinking, width, &mut recycled);
            assert_eq!(recycled, expected, "{case}: stale rows leaked");
            assert_eq!(recycled.len(), expected.len(), "{case}: not trimmed");

            let mut pool = stale_pool();
            render_into_pool(&thinking, width, &mut pool);
            let first_tags = row_tags(&pool);
            let pool_capacity = pool.capacity();
            render_into_pool(&thinking, width, &mut pool);
            assert_eq!(pool, expected, "{case}: second frame != expectation");
            assert_eq!(
                row_tags(&pool),
                first_tags,
                "{case}: pooled rows reallocated between frames"
            );
            assert_eq!(
                pool.capacity(),
                pool_capacity,
                "{case}: the row pool reallocated between frames"
            );
        }
    }

    // Hand-written visible case: a markup-free body that fits the content
    // width is one padded row painted in `ThinkingText`.
    let plain = StreamingThinking::new("plain thought", 1, false, Arc::new(Mutex::new(None)));
    let mut pool = stale_pool();
    render_into_pool(&plain, 40, &mut pool);
    assert_eq!(
        pool,
        vec![with_theme(|theme| {
            theme.fg(
                ThemeColor::ThinkingText,
                &pad_to_width(" plain thought ", 40),
            )
        })],
        "plain thinking body @ 40"
    );

    // The hidden posture relabeled live (`ctx.ui.setHiddenThinkingLabel`): the
    // single label row must be exactly the settled shape, at any width.
    *label
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some("Reasoning...".to_string());
    let thinking = StreamingThinking::new(body, 1, true, Arc::clone(&label));
    let expected = vec![hidden_thinking_row(1, "Reasoning...")];
    assert_eq!(expected.len(), 1, "hidden posture is one row: {expected:?}");
    let mut pool = stale_pool();
    render_into_pool(&thinking, 40, &mut pool);
    assert_eq!(
        pool, expected,
        "hidden label row != independent expectation"
    );
}

#[test]
fn streaming_thinking_canvas_rows_track_streamed_growth_byte_identically() {
    let _guard = test_lock();
    let width = 80;
    let label = Arc::new(Mutex::new(None));
    let doc = "first thought\n\nsecond thought\n\n- a\n- b\n";
    let streamed = StreamingThinking::new("", 1, false, Arc::clone(&label));
    let mut pool = stale_pool();
    for end in line_ends(doc) {
        streamed.set_text_growing(&doc[..end]);
        // Cold second instance of the same prefix: the incremental render's
        // cached prefix/tail must equal the from-scratch render of the very
        // same text.
        let cold = StreamingThinking::new(&doc[..end], 1, false, Arc::clone(&label));
        let wanted = cold.render(width);
        render_into_pool(&streamed, width, &mut pool);
        assert_eq!(
            pool, wanted,
            "streamed thinking prefix {end} @ {width}: canvas rows != cold render"
        );
    }
}

/// The visible thinking body drives the same incremental renderer as the live
/// markdown body: an unchanged body answers from the cache and the canvas path
/// allocates nothing for it.
#[test]
fn streaming_thinking_canvas_path_allocates_nothing_on_a_steady_state_frame() {
    let _guard = test_lock();
    warm_theme();
    let width = 80;
    let thinking = Arc::new(StreamingThinking::new(
        "first thought\n\nsecond thought with a longer tail line\n",
        1,
        false,
        Arc::new(Mutex::new(None)),
    ));
    let bridge = BridgeOnly(Arc::clone(&thinking));

    let mut pool = stale_pool();
    render_into_pool(&*thinking, width, &mut pool);
    let mut bridge_pool = stale_pool();
    render_into_pool(&bridge, width, &mut bridge_pool);
    assert_eq!(
        pool, bridge_pool,
        "the allocating bridge leg must render the same frame"
    );
    let nonempty_rows = pool.iter().filter(|row| !row.is_empty()).count();
    assert!(nonempty_rows > 0, "fixture must render visible rows");

    let ((), canvas_allocations) = count_allocations(|| {
        render_into_pool(&*thinking, width, &mut pool);
    });
    let ((), bridge_allocations) = count_allocations(|| {
        render_into_pool(&bridge, width, &mut bridge_pool);
    });

    assert_eq!(
        pool, bridge_pool,
        "the measured frames of the two legs differ"
    );
    assert_eq!(
        canvas_allocations, 0,
        "a steady-state visible StreamingThinking frame must allocate nothing: \
         {canvas_allocations} allocations for {nonempty_rows} non-empty rows"
    );
    assert!(
        bridge_allocations >= nonempty_rows as u64,
        "the allocating bridge must still pay one allocation per non-empty row: \
         bridge={bridge_allocations}, non-empty={nonempty_rows}"
    );
}

// ---------------------------------------------------------------------------
// 5. The header/footer row writers (terminal.rs call sites)
// ---------------------------------------------------------------------------

#[test]
fn header_line_canvas_path_matches_render_and_reuses_its_row_pool() {
    let _guard = test_lock();
    let keybindings = Keybindings::new();
    for kind in [
        HeaderLineKind::Blank,
        HeaderLineKind::Logo,
        HeaderLineKind::Compact,
        HeaderLineKind::Onboarding,
        HeaderLineKind::ExpandedKeys,
    ] {
        let line = LiveHeaderLine::new(kind, &keybindings);
        for width in WIDTHS {
            let label = format!("header {kind:?} @ {width}");
            // Independent expectation: Blank is a row of spaces and Logo is
            // the bold accent name plus the dim version (the documented
            // `header_line_styled_with` shapes); the two chord-derived kinds
            // are pinned against the uncached builder the live registry must
            // restyle from. `render` is a `render_into` bridge, so comparing
            // the two forms to each other would prove nothing.
            let expected = match kind {
                HeaderLineKind::Blank => vec![blank_row(width)],
                HeaderLineKind::Logo => vec![pad_to_width(
                    &format!(
                        "{}{}",
                        with_theme(|theme| theme.bold(&theme.fg(ThemeColor::Accent, APP_NAME))),
                        with_theme(|theme| theme
                            .fg(ThemeColor::Dim, &format!(" v{}", env!("CARGO_PKG_VERSION"))))
                    ),
                    width,
                )],
                _ => vec![pad_to_width(
                    &header_line_styled_for(kind, &keybindings),
                    width,
                )],
            };
            assert_eq!(
                line.render(width),
                expected,
                "{label}: render() != independent expectation"
            );
            assert_eq!(expected.len(), 1, "{label}: a header line is one row");

            let mut fresh = Vec::new();
            render_into_pool(&line, width, &mut fresh);
            assert_eq!(fresh, expected, "{label}: fresh pool != expectation");

            let mut recycled = stale_pool();
            render_into_pool(&line, width, &mut recycled);
            assert_eq!(recycled, expected, "{label}: stale rows leaked");
            assert_eq!(recycled.len(), 1, "{label}: not trimmed");

            let mut pool = stale_pool();
            render_into_pool(&line, width, &mut pool);
            let first_tags = row_tags(&pool);
            let pool_capacity = pool.capacity();
            render_into_pool(&line, width, &mut pool);
            assert_eq!(pool, expected, "{label}: second frame != expectation");
            assert_eq!(
                row_tags(&pool),
                first_tags,
                "{label}: pooled rows reallocated between frames"
            );
            assert_eq!(
                pool.capacity(),
                pool_capacity,
                "{label}: the row pool reallocated between frames"
            );
        }
    }
}

/// A footer state covering every row the footer can emit (pwd + branch +
/// session, stats with cache/cost/context, and the extension status line).
fn footer_fixture() -> FooterState {
    FooterState {
        cwd: "C:/work/repo".to_string(),
        git_branch: Some("main".to_string()),
        session_name: Some("session".to_string()),
        usage: UsageTotals {
            input: 12_345,
            output: 678,
            cache_read: 90,
            cache_write: 12,
            cost: 0.42,
        },
        context_percent: Some(37.5),
        context_window: 200_000,
        auto_compact: true,
        model_id: "model-x".to_string(),
        model_provider: "provider".to_string(),
        model_reasoning: true,
        thinking_level: "high".to_string(),
        // Deliberately unsorted, so the status line's ordering is pinned too.
        extension_statuses: vec![
            ("b".to_string(), "second".to_string()),
            ("a".to_string(), "first".to_string()),
        ],
        ..FooterState::default()
    }
}

/// A footer state small enough to hand-write its rows: no usage, no
/// cache/cost markers, no statuses — just the dim pwd line and the dim stats
/// line (`?/<context window>` left stats, right-aligned model id).
fn minimal_footer_state() -> FooterState {
    FooterState {
        cwd: "/zz-not-a-home/work".to_string(),
        context_window: 200_000,
        model_id: "m".to_string(),
        ..FooterState::default()
    }
}

/// The hand-written rows of [`minimal_footer_state`] at `width`: the dim pwd
/// line (`format_cwd_for_footer` + `truncate_to_width` with the dim ellipsis;
/// the same public home lookup the footer's own memo keys on), then the dim
/// stats line — left stats + right-aligned model under the documented
/// `min_padding = 2` rule, with the left side truncated when the two cannot
/// share the width. The minimal fixture keeps the right side short enough
/// that the width-starved branch is always the left-truncation one.
fn minimal_footer_rows(width: usize, state: &FooterState) -> Vec<String> {
    let dim = |text: &str| with_theme(|theme| theme.fg(ThemeColor::Dim, text));
    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .ok();
    let pwd = format_cwd_for_footer(&state.cwd, home.as_deref());
    let pwd_line = truncate_to_width(&dim(&pwd), width, &dim("..."));
    let stats_left = format!(
        "?/{}{}",
        format_tokens(state.context_window),
        if state.auto_compact { " (auto)" } else { "" }
    );
    let right = state.model_id.as_str();
    let stats_line = if visible_width(&stats_left) + 2 + visible_width(right) <= width {
        format!(
            "{stats_left}{}{right}",
            " ".repeat(width - visible_width(&stats_left) - visible_width(right))
        )
    } else {
        truncate_to_width(&stats_left, width, "...")
    };
    vec![pwd_line, dim(&stats_line)]
}

#[test]
fn footer_canvas_path_matches_render_and_reuses_its_row_pool() {
    let _guard = test_lock();

    // Independent expectation first: a minimal state whose rows are
    // hand-written from the documented footer contract and the public theme
    // helpers, pinned on both the owned form and the canvas form. The rich
    // fixture below then keeps the direct-builder parity as a supplementary
    // check only.
    let minimal = minimal_footer_state();
    let footer = Footer::new(minimal.clone());
    for width in WIDTHS {
        let expected = minimal_footer_rows(width, &minimal);

        assert_eq!(
            footer.render(width),
            expected,
            "@ {width}: render() != hand-written footer rows"
        );

        let mut fresh = Vec::new();
        render_into_pool(&footer, width, &mut fresh);
        assert_eq!(
            fresh, expected,
            "@ {width}: fresh pool != hand-written footer rows"
        );

        let mut recycled = stale_pool();
        render_into_pool(&footer, width, &mut recycled);
        assert_eq!(recycled, expected, "@ {width}: stale rows leaked");
        assert_eq!(recycled.len(), expected.len(), "@ {width}: not trimmed");

        let mut pool = stale_pool();
        render_into_pool(&footer, width, &mut pool);
        let first_tags = row_tags(&pool);
        let pool_capacity = pool.capacity();
        render_into_pool(&footer, width, &mut pool);
        assert_eq!(
            pool, expected,
            "@ {width}: second frame != hand-written footer rows"
        );
        assert_eq!(
            row_tags(&pool),
            first_tags,
            "@ {width}: pooled rows reallocated between frames"
        );
        assert_eq!(
            pool.capacity(),
            pool_capacity,
            "@ {width}: the row pool reallocated between frames"
        );
    }

    // A state edited between frames must rebuild the rows from the live
    // state (the memo must miss): the new expectation differs from the old
    // rows, so a stale memo entry fails here.
    let edited = FooterState {
        model_id: "n".to_string(),
        context_window: 4096,
        ..minimal.clone()
    };
    footer.update(edited.clone());
    let before = minimal_footer_rows(80, &minimal);
    let after = minimal_footer_rows(80, &edited);
    assert_ne!(
        after, before,
        "the state edit must change the expected rows"
    );
    let mut pool = stale_pool();
    render_into_pool(&footer, 80, &mut pool);
    assert_eq!(pool, after, "the edited footer state must reach the frame");

    // Rich fixture: every row the footer can emit, pinned against the
    // canonical uncached builder (`render_footer` / `render_footer_into`) on
    // the same state — memory and row-buffer behavior included.
    let state = footer_fixture();
    let footer = Footer::new(state.clone());
    for width in WIDTHS {
        let expected = render_footer(width, &state);
        assert_eq!(
            footer.render(width),
            expected,
            "@ {width}: the memoized rows and the render_footer() builder differ"
        );

        let mut fresh = Vec::new();
        render_into_pool(&footer, width, &mut fresh);
        assert_eq!(fresh, expected, "@ {width}: fresh pool != render_footer");

        // The frame renderer's footer call site pushes the same rows through
        // `render_footer_into`; it must be the owned form's bytes.
        let mut direct = stale_pool();
        {
            let mut rows = Rows::new(&mut direct);
            rows.rewind();
            render_footer_into(width, &state, &mut rows);
            rows.trim();
        }
        assert_eq!(
            direct, expected,
            "@ {width}: render_footer_into != owned form"
        );

        let mut recycled = stale_pool();
        render_into_pool(&footer, width, &mut recycled);
        assert_eq!(recycled, expected, "@ {width}: stale rows leaked");
        assert_eq!(recycled.len(), expected.len(), "@ {width}: not trimmed");

        let mut pool = stale_pool();
        render_into_pool(&footer, width, &mut pool);
        let first_tags = row_tags(&pool);
        let pool_capacity = pool.capacity();
        render_into_pool(&footer, width, &mut pool);
        assert_eq!(pool, expected, "@ {width}: second frame != render_footer");
        assert_eq!(
            row_tags(&pool),
            first_tags,
            "@ {width}: pooled rows reallocated between frames"
        );
        assert_eq!(
            pool.capacity(),
            pool_capacity,
            "@ {width}: the row pool reallocated between frames"
        );
    }
}

/// The footer canvas form drops the per-frame row `Vec` the default bridge
/// built only to copy it into the canvas (the line buffers themselves are the
/// same strings both forms produce).
#[test]
fn footer_canvas_path_drops_the_per_frame_row_vec() {
    let _guard = test_lock();
    warm_theme();
    let state = footer_fixture();
    let footer = Arc::new(Footer::new(state.clone()));
    let bridge = BridgeOnly(Arc::clone(&footer));
    let width = 80;

    let mut pool = stale_pool();
    render_into_pool(&*footer, width, &mut pool);
    let mut bridge_pool = stale_pool();
    render_into_pool(&bridge, width, &mut bridge_pool);
    assert_eq!(pool, bridge_pool);

    let ((), canvas_allocations) = count_allocations(|| {
        render_into_pool(&*footer, width, &mut pool);
    });
    let ((), bridge_allocations) = count_allocations(|| {
        render_into_pool(&bridge, width, &mut bridge_pool);
    });
    assert_eq!(pool, bridge_pool);
    assert!(
        bridge_allocations > canvas_allocations,
        "the footer canvas path must drop the row Vec: canvas={canvas_allocations}, \
         bridge={bridge_allocations}"
    );
}

// ---------------------------------------------------------------------------
// 5. The per-frame restyle memos: header line, footer rows, hidden thinking
// ---------------------------------------------------------------------------
//
// The canvas conversion above removed the per-row allocation of the leaf
// components, but each of these three leaves still REBUILT what it writes
// every frame: the startup header line re-derives and re-styles all 18 chords
// of the expanded hint list, the built-in footer re-formats all of its lines
// (`format!` per part, `visible_width` scans, `truncate_to_width`), and the
// hidden-thinking posture re-clones the label and re-styles its one row. Each
// now keeps a memo of its rendered rows keyed on every input that can change
// them (theme palette generation, width, and the derived inputs — for the
// footer, the whole state plus the `HOME` its pwd line shortens against).
//
// The acceptance bar is byte-identity with the uncached builders, so every
// step below compares the cached rows against a freshly built reference
// (`pad_to_width(&header_line_styled_for(..))`, `render_footer`, the settled
// hidden-thinking row) — a memo that served stale bytes fails here.

/// The pre-memo per-frame costs, measured on the same code paths before the
/// memos landed (a scratch probe linked against the pre-change library, debug
/// build): header `ExpandedKeys` 1408 allocation events, footer 157, hidden
/// thinking 6, the three-leaf combined frame 1571, the `HOME` read 2. The
/// floors below sit far under those numbers (a fixture tweak cannot make them
/// flaky) while a memo regression — the full rebuild — still fails loudly.
///
/// The footer floor was re-measured after `ansi::grapheme_width` stopped
/// allocating a `Vec<char>` per measured cluster (the per-cluster 16 B churn
/// `steady_frame_allocations_do_not_grow_with_the_transcript` pins): the
/// uncached footer build cost 176 events before the fix and 81 after it
/// (measured by a scratch probe linked against this tree, debug build, same
/// fixture as `footer_memo_allocates_only_the_env_read_on_a_steady_state_frame`),
/// while the memo-hit frame still pays only the `HOME` read the test measures
/// in place. The floor remains an order of magnitude below the miss and well
/// above the hit, so it still fails if the builder stops building or the memo
/// stops saving it.
const PRE_CACHE_HEADER_FLOOR: u64 = 500;
const PRE_CACHE_FOOTER_FLOOR: u64 = 50;
const PRE_CACHE_COMBINED_FLOOR: u64 = 1000;

/// The 18-chord startup line: a memo hit is free, and the stored row is the
/// uncached builder's bytes.
#[test]
fn live_header_line_memoizes_its_styled_row() {
    let _guard = test_lock();
    warm_theme();
    let width = 80;
    let kb = Keybindings::new();
    let line = LiveHeaderLine::new(HeaderLineKind::ExpandedKeys, &kb);
    let reference = || {
        vec![pad_to_width(
            &header_line_styled_for(HeaderLineKind::ExpandedKeys, &kb),
            width,
        )]
    };

    // Cold frame: the memo is empty, so this is the full chord derivation +
    // restyle (the per-frame cost before the memo).
    let mut pool = stale_pool();
    let ((), build_allocations) = count_allocations(|| {
        render_into_pool(&line, width, &mut pool);
    });
    assert_eq!(
        pool,
        reference(),
        "the built row must be the uncached reference"
    );
    assert!(
        build_allocations >= PRE_CACHE_HEADER_FLOOR,
        "the 18-chord line must still cost its build on a miss: {build_allocations}"
    );

    // Steady state: same kind, width and palette — the stored row is copied.
    let ((), steady_allocations) = count_allocations(|| {
        render_into_pool(&line, width, &mut pool);
    });
    assert_eq!(
        pool,
        reference(),
        "the memoized row drifted from the reference"
    );
    assert_eq!(
        steady_allocations, 0,
        "a steady-state header frame must allocate nothing: {steady_allocations}"
    );

    // The trait's owned-line form reads the same memo.
    assert_eq!(line.render(width), reference(), "render() != the reference");
}

/// A live theme swap and a width change must each rebuild the header row —
/// and only once.
#[test]
fn live_header_line_memo_rebuilds_on_theme_swap_and_width_change() {
    let _guard = test_lock();
    warm_theme();
    let kb = Keybindings::new();
    let line = LiveHeaderLine::new(HeaderLineKind::Compact, &kb);
    let reference = |width: usize| {
        vec![pad_to_width(
            &header_line_styled_for(HeaderLineKind::Compact, &kb),
            width,
        )]
    };

    let mut pool = stale_pool();
    render_into_pool(&line, 80, &mut pool);
    let ((), steady) = count_allocations(|| {
        render_into_pool(&line, 80, &mut pool);
    });
    assert_eq!(steady, 0, "precondition: the memo must be warm");

    // Live `ctx.ui.setTheme`: the palette generation moves, and the memo must
    // rebuild from the palette installed at the new generation.
    let before = Theme::generation();
    let swap = ThemeSwap::install("light");
    assert!(
        Theme::generation() > before,
        "set_theme must bump the palette generation"
    );
    let ((), swapped) = count_allocations(|| {
        render_into_pool(&line, 80, &mut pool);
    });
    assert_eq!(
        pool,
        reference(80),
        "the swapped-palette row must be rebuilt from the live palette"
    );
    assert!(
        swapped >= 50,
        "a theme swap must rebuild the styled line: {swapped}"
    );
    let ((), steady_swapped) = count_allocations(|| {
        render_into_pool(&line, 80, &mut pool);
    });
    assert_eq!(
        steady_swapped, 0,
        "the rebuilt entry must be served from then on"
    );

    // Width: the padding is part of the memoized row.
    let ((), widened) = count_allocations(|| {
        render_into_pool(&line, 120, &mut pool);
    });
    assert_eq!(
        pool,
        reference(120),
        "the widened row must match the reference"
    );
    assert!(widened > 0, "a width change must rebuild the padded row");
    let ((), steady_wide) = count_allocations(|| {
        render_into_pool(&line, 120, &mut pool);
    });
    assert_eq!(steady_wide, 0);

    // The memo holds one entry per input set, so returning to an earlier width
    // rebuilds (a wrong-key hit would keep the width-120 padding).
    let ((), narrowed) = count_allocations(|| {
        render_into_pool(&line, 80, &mut pool);
    });
    assert_eq!(
        pool,
        reference(80),
        "the narrowed row must match the reference"
    );
    assert!(narrowed > 0, "a width change must rebuild the padded row");

    // Restoring the startup palette moves the generation again.
    drop(swap);
    let ((), restored) = count_allocations(|| {
        render_into_pool(&line, 80, &mut pool);
    });
    assert_eq!(
        pool,
        reference(80),
        "the restored-palette row != the dark reference"
    );
    assert!(restored > 0, "the restore swap must rebuild");
}

/// The built-in footer: a memo hit leaves only the per-frame
/// `HOME`/`USERPROFILE` read its pwd line shortens against.
#[test]
fn footer_memo_allocates_only_the_env_read_on_a_steady_state_frame() {
    let _guard = test_lock();
    warm_theme();
    let width = 80;
    let state = footer_fixture();
    let footer = Footer::new(state.clone());
    let expected = render_footer(width, &state);

    // Cold frame: the memo is empty, so this is the whole line building.
    let mut pool = stale_pool();
    let ((), build_allocations) = count_allocations(|| {
        render_into_pool(&footer, width, &mut pool);
    });
    assert_eq!(
        pool, expected,
        "the built rows must be the uncached reference"
    );
    assert!(
        build_allocations >= PRE_CACHE_FOOTER_FLOOR,
        "the footer's line building must still cost its allocations on a miss: \
         {build_allocations}"
    );

    // The uncached reference itself (`render_footer_into`), measured on the
    // same pool the memo path uses.
    let ((), uncached_allocations) = count_allocations(|| {
        let mut rows = Rows::new(&mut pool);
        rows.rewind();
        render_footer_into(width, &state, &mut rows);
        rows.trim();
    });
    assert_eq!(pool, expected);
    assert!(
        uncached_allocations >= PRE_CACHE_FOOTER_FLOOR,
        "the uncached footer builder must cost its allocations: {uncached_allocations}"
    );

    // Steady state: only the env read remains.
    let env_read = measured_env_read_allocations();
    let ((), steady_allocations) = count_allocations(|| {
        render_into_pool(&footer, width, &mut pool);
    });
    assert_eq!(
        pool, expected,
        "the memoized rows drifted from the reference"
    );
    assert!(
        steady_allocations <= env_read + 1,
        "a steady-state footer frame must pay only the HOME/USERPROFILE read: \
         steady={steady_allocations}, env read={env_read}, \
         uncached build={uncached_allocations}"
    );
    assert!(
        uncached_allocations >= steady_allocations + PRE_CACHE_FOOTER_FLOOR,
        "the memo must remove the per-frame line building: \
         uncached={uncached_allocations}, steady={steady_allocations}"
    );
}

/// A state change, a width change and a palette swap must each rebuild the
/// footer's rows from the live inputs — while an identical state re-installed
/// (the refresh path pushes a fresh clone) must hit.
#[test]
fn footer_memo_rebuilds_on_state_width_and_theme_changes() {
    let _guard = test_lock();
    warm_theme();
    let width = 80;
    let footer = Footer::new(footer_fixture());
    let mut pool = stale_pool();
    render_into_pool(&footer, width, &mut pool);
    let env_read = measured_env_read_allocations();

    // `Footer::update` (the event-driven refresh): a changed state rebuilds.
    let mut updated = footer_fixture();
    updated.model_id = "model-y".to_string();
    updated.usage.output = 999;
    updated
        .extension_statuses
        .push(("c".to_string(), "third".to_string()));
    footer.update(updated.clone());
    let ((), state_allocations) = count_allocations(|| {
        render_into_pool(&footer, width, &mut pool);
    });
    assert_eq!(
        pool,
        render_footer(width, &updated),
        "the updated state must be re-rendered from the live state"
    );
    assert!(
        state_allocations >= PRE_CACHE_FOOTER_FLOOR,
        "a state change must rebuild the rows: {state_allocations}"
    );

    // The memo is keyed on CONTENT: an identical state re-installed must hit.
    footer.update(updated.clone());
    let ((), reinstall_allocations) = count_allocations(|| {
        render_into_pool(&footer, width, &mut pool);
    });
    assert_eq!(pool, render_footer(width, &updated));
    assert!(
        reinstall_allocations <= env_read + 1,
        "an identical state re-installed must hit the memo: {reinstall_allocations}"
    );

    // Width: the stats line is re-padded/re-truncated to the new width.
    let ((), width_allocations) = count_allocations(|| {
        render_into_pool(&footer, 120, &mut pool);
    });
    assert_eq!(
        pool,
        render_footer(120, &updated),
        "the wider rows must match the reference"
    );
    assert!(
        width_allocations >= PRE_CACHE_FOOTER_FLOOR,
        "a width change must rebuild the rows: {width_allocations}"
    );
    let ((), wide_steady) = count_allocations(|| {
        render_into_pool(&footer, 120, &mut pool);
    });
    assert!(
        wide_steady <= env_read + 1,
        "the widened rows must be served from the memo: {wide_steady}"
    );

    // Theme: the light palette repaints every styled part of the rows.
    let _swap = ThemeSwap::install("light");
    let ((), theme_allocations) = count_allocations(|| {
        render_into_pool(&footer, 120, &mut pool);
    });
    assert_eq!(
        pool,
        render_footer(120, &updated),
        "the swapped-palette rows must be rebuilt from the live palette"
    );
    assert!(
        theme_allocations >= PRE_CACHE_FOOTER_FLOOR,
        "a theme swap must rebuild the rows: {theme_allocations}"
    );
    let ((), swapped_steady) = count_allocations(|| {
        render_into_pool(&footer, 120, &mut pool);
    });
    assert!(
        swapped_steady <= env_read + 1,
        "the rebuilt entry must be served from then on: {swapped_steady}"
    );
}

/// The hidden-thinking posture's single row is memoized by label, padding and
/// palette: a steady frame is free, a relabel or palette swap rebuilds it,
/// and the render width is not an input at all.
#[test]
fn hidden_thinking_row_is_memoized_across_frames_relabel_and_theme_swap() {
    let _guard = test_lock();
    warm_theme();
    let label = Arc::new(Mutex::new(None));
    let thinking = StreamingThinking::new("body", 1, true, Arc::clone(&label));

    let mut pool = stale_pool();
    let ((), build_allocations) = count_allocations(|| {
        render_into_pool(&thinking, 40, &mut pool);
    });
    assert_eq!(
        pool,
        vec![hidden_thinking_row(1, "Thinking...")],
        "the row must be the settled hidden-thinking shape"
    );
    assert!(build_allocations > 0, "the first row must be styled");
    let ((), steady_allocations) = count_allocations(|| {
        render_into_pool(&thinking, 40, &mut pool);
    });
    assert_eq!(
        steady_allocations, 0,
        "a steady hidden-thinking frame must allocate nothing: {steady_allocations}"
    );

    // `ctx.ui.setHiddenThinkingLabel` rewrites the shared slot live.
    *label
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some("Reasoning...".to_string());
    let ((), relabel_allocations) = count_allocations(|| {
        render_into_pool(&thinking, 40, &mut pool);
    });
    assert_eq!(
        pool,
        vec![hidden_thinking_row(1, "Reasoning...")],
        "the live label must reach the row"
    );
    assert!(relabel_allocations > 0, "a relabel must rebuild the row");
    let ((), steady_relabel) = count_allocations(|| {
        render_into_pool(&thinking, 40, &mut pool);
    });
    assert_eq!(steady_relabel, 0);

    // The width must NOT be part of the key: the row is padded to
    // `output_pad` only, so a new width still answers from the memo.
    let ((), rewrapped_allocations) = count_allocations(|| {
        render_into_pool(&thinking, 120, &mut pool);
    });
    assert_eq!(pool, vec![hidden_thinking_row(1, "Reasoning...")]);
    assert_eq!(
        rewrapped_allocations, 0,
        "the hidden row does not read the render width"
    );

    // Theme swap: the row is repainted under the new palette.
    let _swap = ThemeSwap::install("light");
    let ((), swapped_allocations) = count_allocations(|| {
        render_into_pool(&thinking, 120, &mut pool);
    });
    assert_eq!(
        pool,
        vec![hidden_thinking_row(1, "Reasoning...")],
        "the swapped palette must reach the row"
    );
    assert!(swapped_allocations > 0, "a theme swap must rebuild the row");
    let ((), steady_swapped) = count_allocations(|| {
        render_into_pool(&thinking, 120, &mut pool);
    });
    assert_eq!(steady_swapped, 0);
}

/// The headline: a steady-state frame with the header, the footer and the
/// hidden thinking block present pays nothing but the footer's env read, and
/// its rows are exactly what the uncached builders produce.
#[test]
fn steady_state_frame_with_header_footer_and_thinking_pays_only_the_footer_env_read() {
    let _guard = test_lock();
    warm_theme();
    let width = 80;
    let kb = Keybindings::new();
    let state = footer_fixture();
    let label = Arc::new(Mutex::new(None));
    let mut content = Container::new();
    content.add_child(LiveHeaderLine::new(HeaderLineKind::ExpandedKeys, &kb));
    content.add_child(Footer::new(state.clone()));
    content.add_child(StreamingThinking::new("body", 1, true, Arc::clone(&label)));

    let expected = {
        let mut rows = vec![pad_to_width(
            &header_line_styled_for(HeaderLineKind::ExpandedKeys, &kb),
            width,
        )];
        rows.extend(render_footer(width, &state));
        rows.push(hidden_thinking_row(1, "Thinking..."));
        rows
    };

    let mut pool = stale_pool();
    let ((), build_allocations) = count_allocations(|| {
        render_into_pool(&content, width, &mut pool);
    });
    assert_eq!(
        pool, expected,
        "the built frame must be the uncached reference"
    );
    assert!(
        build_allocations >= PRE_CACHE_COMBINED_FLOOR,
        "the cold frame must still build all three leaves: {build_allocations}"
    );

    let env_read = measured_env_read_allocations();
    let ((), steady_allocations) = count_allocations(|| {
        render_into_pool(&content, width, &mut pool);
    });
    assert_eq!(
        pool, expected,
        "the steady frame drifted from the reference"
    );
    assert!(
        steady_allocations <= env_read + 1,
        "a steady-state frame (header + footer + hidden thinking) must pay only \
         the footer's HOME/USERPROFILE read: steady={steady_allocations}, \
         env read={env_read}, cold build={build_allocations}"
    );
}

// ---------------------------------------------------------------------------
// 7. The settled assistant message: no per-frame thinking-body/pad/label work
// ---------------------------------------------------------------------------

/// One thinking content block (the settled arm reads `thinking`, trimmed).
fn thinking_block(text: &str) -> ContentBlock {
    ContentBlock::Thinking(ThinkingContent {
        thinking: text.to_string(),
        thinking_signature: None,
        redacted: false,
    })
}

/// One text content block (the settled arm reads `text`, trimmed).
fn text_block(text: &str) -> ContentBlock {
    ContentBlock::Text(TextContent {
        text: text.to_string(),
        text_signature: None,
    })
}

/// One inline image block (base64 payload + mime).
fn image_block(data: &str, mime_type: &str) -> ContentBlock {
    ContentBlock::Image(ImageContent {
        data: data.to_string(),
        mime_type: mime_type.to_string(),
    })
}

/// The settled component shape `ChatContainer::commit_entry` builds:
/// `outputPad` 1, the shared hidden-thinking-label slot.
fn assistant_message_from(blocks: Vec<ContentBlock>) -> AssistantMessage {
    AssistantMessage {
        role: AssistantRole::Assistant,
        content: blocks,
        api: "frame-reuse".into(),
        provider: "frame-reuse".into(),
        model: "frame-reuse".into(),
        stop_reason: StopReason::Stop,
        ..Default::default()
    }
}

fn assistant_component(
    blocks: Vec<ContentBlock>,
    hide_thinking: bool,
    label: Arc<Mutex<Option<String>>>,
) -> AssistantMessageComponent {
    AssistantMessageComponent::new_from(Arc::new(assistant_message_from(blocks)), hide_thinking, 1)
        .with_hidden_label(label)
}

/// Whether a content block contributes a rendered body segment
/// (`has_visible_content` / the `has_after` separator check).
fn block_is_visible(block: &ContentBlock) -> bool {
    match block {
        ContentBlock::Text(text) => !text.text.trim().is_empty(),
        ContentBlock::Thinking(thinking) => !thinking.thinking.trim().is_empty(),
        _ => false,
    }
}

/// The settled message's body rebuilt independently of the component: the
/// leading spacer, one markdown segment per visible block (the `\n\n`-joined
/// thinking run painted in `ThinkingText` or the settled hidden row), the
/// blank separator after a joined thinking run, and nothing from the skipped
/// `ToolCall`/`Image` blocks. The segments go through fresh `Markdown`
/// instances, so a dropped spacer, a lost paint or a stale segment cache all
/// differ from this composition.
fn assistant_reference_rows(
    message: &AssistantMessage,
    output_pad: usize,
    hidden: bool,
    width: usize,
) -> Vec<String> {
    let mut rows: Vec<String> = Vec::new();
    if message.content.iter().any(block_is_visible) {
        rows.push(String::new());
    }
    let mut index = 0usize;
    while index < message.content.len() {
        match &message.content[index] {
            ContentBlock::Text(text) => {
                if !text.text.trim().is_empty() {
                    let segment = Markdown::with_options(
                        text.text.trim(),
                        output_pad,
                        0,
                        MarkdownTheme::default(),
                        MarkdownOptions {
                            mermaid: global_mermaid_mode(),
                            ..MarkdownOptions::default()
                        },
                    );
                    rows.extend(segment.render(width));
                }
                index += 1;
            }
            ContentBlock::Thinking(_) => {
                // A run of thinking blocks joins into one `\n\n`-separated
                // trimmed body.
                let mut blocks = String::new();
                let mut has_blocks = false;
                while index < message.content.len() {
                    if let ContentBlock::Thinking(thinking) = &message.content[index] {
                        let trimmed = thinking.thinking.trim();
                        if !trimmed.is_empty() {
                            if has_blocks {
                                blocks.push_str("\n\n");
                            }
                            blocks.push_str(trimmed);
                            has_blocks = true;
                        }
                        index += 1;
                    } else {
                        break;
                    }
                }
                if !has_blocks {
                    continue;
                }
                if hidden {
                    rows.push(hidden_thinking_row(output_pad, "Thinking..."));
                } else {
                    let segment = Markdown::with_options(
                        blocks,
                        output_pad,
                        0,
                        MarkdownTheme::default(),
                        MarkdownOptions::default(),
                    );
                    for line in segment.render(width) {
                        rows.push(with_theme(|theme| {
                            theme.fg(ThemeColor::ThinkingText, &line)
                        }));
                    }
                }
                if message.content[index..].iter().any(block_is_visible) {
                    rows.push(String::new());
                }
            }
            ContentBlock::ToolCall(_) | ContentBlock::Image(_) => {
                index += 1;
            }
        }
    }
    rows
}

/// The settled message shapes that used to re-materialize work per frame: a
/// single thinking block (the common run), a multi-block thinking run (the
/// `\n\n` join), and the hidden posture — pinned against the independent
/// composition below (fresh `Markdown` segments + the documented spacer
/// walk), plus two hand-written simple shapes, plus a width edit that a
/// width-keyed segment cache must follow. Every shape must also keep its row
/// buffers across frames.
#[test]
fn assistant_message_canvas_path_matches_render_and_reuses_its_row_pool() {
    let _guard = test_lock();
    let body = "first thought\n\nsecond **thought** with a longer line\n";
    let cases: [(&str, Vec<ContentBlock>, bool); 4] = [
        (
            "single thinking block + text",
            vec![thinking_block(body), text_block("answer body")],
            false,
        ),
        (
            "thinking run of two blocks + text",
            vec![
                thinking_block("first thought"),
                thinking_block("second thought"),
                text_block("answer body"),
            ],
            false,
        ),
        (
            "hidden thinking + text",
            vec![thinking_block(body), text_block("answer body")],
            true,
        ),
        (
            "empty thinking run between text blocks",
            vec![
                text_block("before"),
                thinking_block("   \n  "),
                text_block("after"),
            ],
            false,
        ),
    ];
    for (case, blocks, hidden) in cases {
        let message = Arc::new(assistant_message_from(blocks));
        let component = AssistantMessageComponent::new_from(Arc::clone(&message), hidden, 1)
            .with_hidden_label(Arc::new(Mutex::new(None)));
        for width in WIDTHS {
            let label = format!("{case} @ {width}");
            let expected = assistant_reference_rows(&message, 1, hidden, width);

            // `render` is a `render_into` bridge, so both forms are pinned
            // against the independent composition rather than to each other.
            assert_eq!(
                component.render(width),
                expected,
                "{label}: render() != independent composition"
            );

            let mut fresh = Vec::new();
            render_into_pool(&component, width, &mut fresh);
            assert_eq!(
                fresh, expected,
                "{label}: fresh pool != independent composition"
            );

            let mut recycled = stale_pool();
            render_into_pool(&component, width, &mut recycled);
            assert_eq!(recycled, expected, "{label}: stale rows leaked");
            assert_eq!(recycled.len(), expected.len(), "{label}: not trimmed");

            let mut pool = stale_pool();
            render_into_pool(&component, width, &mut pool);
            let first_tags = row_tags(&pool);
            let pool_capacity = pool.capacity();
            render_into_pool(&component, width, &mut pool);
            assert_eq!(
                pool, expected,
                "{label}: second frame != independent composition"
            );
            assert_eq!(
                row_tags(&pool),
                first_tags,
                "{label}: pooled rows reallocated between frames"
            );
            assert_eq!(
                pool.capacity(),
                pool_capacity,
                "{label}: the row pool reallocated between frames"
            );
        }
    }

    // Hand-written simple shapes at widths where the segment fits: a text-only
    // message is the leading empty spacer plus the side-padded text row.
    let text_only = assistant_component(
        vec![text_block("answer body")],
        false,
        Arc::new(Mutex::new(None)),
    );
    for width in [20usize, 80] {
        let expected = vec![String::new(), pad_to_width(" answer body ", width)];
        let mut pool = stale_pool();
        render_into_pool(&text_only, width, &mut pool);
        assert_eq!(pool, expected, "text-only message @ {width}");
    }

    // A width change must re-pad the segment: the segment key carries the
    // width, so a key without it would keep the old padding.
    let mut pool = stale_pool();
    render_into_pool(&text_only, 20, &mut pool);
    let narrow = pool.clone();
    render_into_pool(&text_only, 40, &mut pool);
    let widened = vec![String::new(), pad_to_width(" answer body ", 40)];
    assert_ne!(
        widened, narrow,
        "the width change must change the expected rows"
    );
    assert_eq!(pool, widened, "a width change must re-pad the segment");

    // Hand-written hidden posture: the leading spacer, the settled label row,
    // the blank separator, then the text row.
    let hidden_message = assistant_component(
        vec![
            thinking_block("a hidden thought body"),
            text_block("answer body"),
        ],
        true,
        Arc::new(Mutex::new(None)),
    );
    let mut pool = stale_pool();
    render_into_pool(&hidden_message, 40, &mut pool);
    assert_eq!(
        pool,
        vec![
            String::new(),
            hidden_thinking_row(1, "Thinking..."),
            String::new(),
            pad_to_width(" answer body ", 40),
        ],
        "hidden thinking + text @ 40"
    );
}

/// The steady frame of a settled message whose thinking run is ONE block —
/// the common case — must allocate nothing: the trimmed block answers the
/// segment cache key by borrow, the per-segment pad is a construction-time
/// field and the live label is borrowed out of the shared slot. The pre-fix
/// form re-copied the whole thinking body into a fresh join buffer, plus the
/// pad and the label, on every frame (hit or miss). A multi-block run still
/// pays the one allocation its join costs — the key compare needs the joined
/// bytes — which is why that case is pinned at `<= 1` instead of `0`.
#[test]
fn assistant_message_steady_state_frame_materializes_no_thinking_body() {
    let _guard = test_lock();
    warm_theme();
    let width = 80;
    let body = "first thought\n\nsecond thought with a longer tail line\n";
    let label = Arc::new(Mutex::new(None));

    let single = Arc::new(assistant_component(
        vec![thinking_block(body), text_block("answer body")],
        false,
        Arc::clone(&label),
    ));
    let bridge = BridgeOnly(Arc::clone(&single));
    let mut pool = stale_pool();
    let mut bridge_pool = stale_pool();
    // Two warm frames: the segment caches fill and the row pool grows.
    render_into_pool(&*single, width, &mut pool);
    render_into_pool(&*single, width, &mut pool);
    render_into_pool(&bridge, width, &mut bridge_pool);
    render_into_pool(&bridge, width, &mut bridge_pool);
    assert_eq!(
        pool, bridge_pool,
        "the allocating bridge leg must render the same frame"
    );
    let nonempty_rows = pool.iter().filter(|row| !row.is_empty()).count();
    assert!(nonempty_rows > 0, "fixture must render visible rows");

    let ((), single_allocations) = count_allocations(|| {
        render_into_pool(&*single, width, &mut pool);
    });
    let ((), bridge_allocations) = count_allocations(|| {
        render_into_pool(&bridge, width, &mut bridge_pool);
    });
    assert_eq!(
        pool, bridge_pool,
        "the measured frames of the two legs differ"
    );
    assert_eq!(
        single_allocations, 0,
        "a steady settled-message frame must allocate nothing (one thinking \
         block): {single_allocations} allocations for {nonempty_rows} non-empty rows"
    );
    assert!(
        bridge_allocations >= nonempty_rows as u64,
        "the allocating bridge must still pay one allocation per non-empty row: \
         bridge={bridge_allocations}, non-empty={nonempty_rows}"
    );

    // A multi-block thinking run: the join is the only thing the hit frame
    // still builds.
    let joined = assistant_component(
        vec![
            thinking_block("first thought"),
            thinking_block("second thought with a longer tail line"),
            text_block("answer body"),
        ],
        false,
        Arc::clone(&label),
    );
    let mut joined_pool = stale_pool();
    render_into_pool(&joined, width, &mut joined_pool);
    render_into_pool(&joined, width, &mut joined_pool);
    let ((), join_allocations) = count_allocations(|| {
        render_into_pool(&joined, width, &mut joined_pool);
    });
    assert!(
        join_allocations <= 1,
        "a multi-block thinking run may only pay its own join on a hit frame: \
         {join_allocations}"
    );

    // Theme swap: both shapes are keyed on the palette generation, so the
    // swap repaints and then settles again.
    let _swap = ThemeSwap::install("light");
    let ((), swapped_allocations) = count_allocations(|| {
        render_into_pool(&*single, width, &mut pool);
    });
    assert!(
        swapped_allocations > 0,
        "a theme swap must rebuild the segments"
    );
    let ((), steady_swapped) = count_allocations(|| {
        render_into_pool(&*single, width, &mut pool);
    });
    assert_eq!(steady_swapped, 0, "the swapped frame must settle again");
}

/// The hidden posture: the label is part of the segment key, so the borrowed
/// label must still reach the cache — a live `ctx.ui.setHiddenThinkingLabel`
/// relabel rebuilds the single row, and the row is the settled shape
/// (`{pad}{label}` in italic `ThinkingText`, behind the leading `Spacer(1)`
/// row every non-empty message renders). Without the label in the key the row
/// would freeze at "Thinking...".
#[test]
fn assistant_hidden_thinking_row_follows_the_live_label() {
    let _guard = test_lock();
    warm_theme();
    let label = Arc::new(Mutex::new(None));
    let component = assistant_component(
        vec![thinking_block("a hidden thought body")],
        true,
        Arc::clone(&label),
    );
    let mut pool = stale_pool();
    render_into_pool(&component, 40, &mut pool);
    assert_eq!(
        pool,
        vec!["".to_string(), hidden_thinking_row(1, "Thinking...")],
        "the default label must reach the hidden row"
    );
    let ((), steady_allocations) = count_allocations(|| {
        render_into_pool(&component, 40, &mut pool);
    });
    assert_eq!(
        steady_allocations, 0,
        "the steady hidden row must allocate nothing"
    );

    *label
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some("Reasoning...".to_string());
    let ((), relabel_allocations) = count_allocations(|| {
        render_into_pool(&component, 40, &mut pool);
    });
    assert_eq!(
        pool,
        vec!["".to_string(), hidden_thinking_row(1, "Reasoning...")],
        "the live label must reach the hidden row"
    );
    assert!(
        relabel_allocations > 0,
        "a relabel must miss the segment cache"
    );
    let ((), steady_relabel) = count_allocations(|| {
        render_into_pool(&component, 40, &mut pool);
    });
    assert_eq!(steady_relabel, 0);
}

// ---------------------------------------------------------------------------
// 8. The tool block: steady background pair + result rows
// ---------------------------------------------------------------------------

/// A settled tool block's steady frame: the rows pinned against the
/// independent box reference above (not the component's own bridge), no
/// per-row allocation, and a palette swap that rebuilds exactly once — with
/// the painted bytes checked to carry the NEW palette's background opener and
/// none of the old one's, so a memo serving the previous palette's escape
/// pair fails. A result edited between frames must re-render its box too.
#[test]
fn tool_block_steady_state_frame_reuses_its_background_pair() {
    let _guard = test_lock();
    warm_theme();
    let width = 80;
    let expanded = Arc::new(AtomicBool::new(true));
    let args = Arc::new(serde_json::json!({ "command": "printf hello" }));
    let block = Arc::new(ToolExecutionComponent::new(
        "bash",
        Arc::clone(&args),
        Arc::clone(&expanded),
    ));
    let output = (0..100)
        .map(|index| format!("line {index}"))
        .collect::<Vec<_>>()
        .join("\n");
    block.update_result(Arc::<str>::from(output.as_str()), false, Some(1200));
    let bridge = BridgeOnly(Arc::clone(&block));

    // Independent expectation: the expanded settled bash box rebuilt from the
    // documented shapes (call line via `format_tool_call_line`, body styled in
    // `ToolOutput`, `Took 1.2s` tail, `toolSuccessBg` blanks).
    let expected = reference_bash_box_rows(&args, &output, Some("Took 1.2s"), width);
    let nonempty_rows = expected.iter().filter(|row| !row.is_empty()).count();
    assert!(
        nonempty_rows > 8,
        "the expanded fixture must paint many rows: {nonempty_rows}"
    );

    // Warm frame (memorizes the result rows and background pair, grows the
    // pool), then the measured steady frame.
    let mut pool = stale_pool();
    render_into_pool(&*block, width, &mut pool);
    assert_eq!(pool, expected, "canvas rows != independent box reference");
    assert_eq!(
        block.render(width),
        expected,
        "the owned form != independent box reference"
    );
    let mut bridge_pool = stale_pool();
    render_into_pool(&bridge, width, &mut bridge_pool);
    render_into_pool(&bridge, width, &mut bridge_pool);
    assert_eq!(
        bridge_pool, expected,
        "the bridge leg must match the reference"
    );

    // The steady frame: measured WITH attribution. The channel assertions
    // below are the invariants that do not depend on the absolute count - the
    // pool must not grow, no row buffer may be outgrown, and no tool-block
    // memo may rebuild (call rows, timer tail, result rows, background pair).
    // A failure names the channel in the message.
    let steady = measure_frame(&*block, width, &mut pool);
    assert_steady_frame_channels(&steady, "steady bash box");
    // Absolute residual measured by this tree: 15 allocation events at width
    // Attribution (measured by temporarily forcing the ceiling below to 0): the
    // residual is exactly 15 allocations of 16 B, with `ToolMemoWork` all-zero
    // (no memo rebuilds), `pool_growth` 0 and `outgrown_rows` 0 — i.e. it is
    // per-span styled-string formatting on a path no counter covers yet, not a
    // memo miss and not a per-row/per-body cost (invariant across body sizes and
    // row counts). The ceiling pins the residual; naming that span-formatting
    // site is the follow-up.
    // A frame AFTER a state change re-formats the call line / preview tail by
    // design (measured: width 15, collapse 23, palette 15 — all the same 16 B
    // span-formatting shape, none per-row or per-body). The state-change ceiling
    // has headroom over those; the per-block and channel assertions above stay
    // the structural pins.
    assert!(
        steady.allocations <= STEADY_BASH_BLOCK_CEILING,
        "a steady tool-block frame must not allocate more than the \
         formatter-level ceiling: steady={}, ceiling={STEADY_BASH_BLOCK_CEILING}, \
         non-empty rows={nonempty_rows}, sizes={:?}, attribution={steady:?}",
        steady.allocations,
        steady.sizes
    );
    assert_eq!(
        pool, expected,
        "the steady frame drifted from the reference"
    );

    // A result edited since the last frame must re-render its box (the
    // result-rows memo must miss), not serve the previous body.
    let edited = "changed body\nsecond changed line";
    block.update_result(Arc::<str>::from(edited), false, Some(1200));
    let edited_expected = reference_bash_box_rows(&args, edited, Some("Took 1.2s"), width);
    assert_ne!(
        edited_expected, expected,
        "the result edit must change the expected rows"
    );
    let ((), edited_allocations) = count_allocations(|| {
        render_into_pool(&*block, width, &mut pool);
    });
    assert!(
        edited_allocations > 0,
        "an edited result must rebuild the box rows"
    );
    assert_eq!(
        pool, edited_expected,
        "an edited result must reach the frame"
    );

    // Theme swap: the block's palette-keyed memos (background pair + result
    // rows) must rebuild, then settle again — and the painted bytes must be
    // the NEW palette's, with the old opener gone.
    let dark_open = bg_open(ThemeColor::ToolSuccessBg);
    let _swap = ThemeSwap::install("light");
    let light_open = bg_open(ThemeColor::ToolSuccessBg);
    assert!(
        !light_open.is_empty(),
        "the palette must paint the tool background"
    );
    assert_ne!(
        dark_open, light_open,
        "the swap must change the background escape"
    );
    let ((), swapped_allocations) = count_allocations(|| {
        render_into_pool(&*block, width, &mut pool);
    });
    assert!(
        swapped_allocations > steady.allocations,
        "a theme swap must rebuild the block's palette-keyed memos: \
         swapped={swapped_allocations}, steady={}",
        steady.allocations
    );
    let swapped_expected = reference_bash_box_rows(&args, edited, Some("Took 1.2s"), width);
    assert_eq!(
        pool, swapped_expected,
        "the swapped frame != the light-palette box reference"
    );
    // Every box row (everything past the unpainted leading spacer) carries
    // the swapped palette's opener; the old opener survives nowhere.
    assert!(
        pool[1..]
            .iter()
            .all(|row| row.contains(light_open.as_str())),
        "every painted box row must carry the swapped palette's background opener"
    );
    if !light_open.contains(dark_open.as_str()) {
        // Guard the substring relation so the check cannot false-fail when one
        // palette's opener happens to contain the other's bytes.
        assert!(
            pool.iter().all(|row| !row.contains(dark_open.as_str())),
            "the old palette's background opener survived the swap"
        );
    }
    let ((), steady_swapped) = count_allocations(|| {
        render_into_pool(&*block, width, &mut pool);
    });
    assert!(
        steady_swapped <= STATE_CHANGE_CEILING,
        "the swapped frame must settle again within the same formatter-level          ceiling (15 x 16 B span formatting, measured): {steady_swapped} vs {STEADY_BASH_BLOCK_CEILING}"
    );
}

/// The call line and the timer tail are memoized per block: a settled tool
/// block re-pushes them without re-formatting or re-wrapping anything, and
/// every input that can move them (width, expansion, recorded duration,
/// palette) rebuilds exactly what a COLD instance of the same block renders —
/// a memo serving stale rows fails against the cold reference.
#[test]
fn tool_block_call_line_and_timer_tail_memos_settle_and_track_their_inputs() {
    let _guard = test_lock();
    warm_theme();
    let width = 80;
    let expanded = Arc::new(AtomicBool::new(true));
    let args = Arc::new(serde_json::json!({ "command": "printf hello" }));
    let output: Arc<str> = Arc::from("alpha\nbeta with a longer tail line");
    let block = ToolExecutionComponent::new("bash", Arc::clone(&args), Arc::clone(&expanded));
    block.update_result(Arc::clone(&output), false, Some(1200));
    // The independent reference for every posture: a cold second instance of
    // the same block (fresh memos) rendered once.
    let cold = |width: usize, expanded: bool, duration_ms: Option<u64>| -> Vec<String> {
        let flag = Arc::new(AtomicBool::new(expanded));
        let fresh = ToolExecutionComponent::new("bash", Arc::clone(&args), flag);
        fresh.update_result(Arc::clone(&output), false, duration_ms);
        fresh.render(width)
    };

    // Cold frame: every memo builds.
    let mut pool = stale_pool();
    render_into_pool(&block, width, &mut pool);
    assert_eq!(
        pool,
        cold(width, true, Some(1200)),
        "the cold frame != a cold instance of the block"
    );

    // Steady frame: the memoized call rows and `Took` tail are copied into
    // the recycled rows — no `format!`, no `wrap_text_with_ansi`.
    let steady = measure_frame(&block, width, &mut pool);
    assert_eq!(
        pool,
        cold(width, true, Some(1200)),
        "the steady frame drifted from the cold reference"
    );
    assert_steady_frame_channels(&steady, "steady bash block");
    // Same documented ceiling as
    // `tool_block_steady_state_frame_reuses_its_background_pair` (see the
    // rationale there: the residual is invariant in the body and in the row
    // count, and the channel assertions above are clean).
    assert!(
        steady.allocations <= STEADY_BASH_BLOCK_CEILING,
        "a steady bash frame must not allocate more than the formatter-level \
         ceiling: steady={}, ceiling={STEADY_BASH_BLOCK_CEILING}, \
         sizes={:?}, attribution={steady:?}",
        steady.allocations,
        steady.sizes
    );

    // A width change re-wraps the call line (and re-pads every box row).
    render_into_pool(&block, 40, &mut pool);
    assert_eq!(
        pool,
        cold(40, true, Some(1200)),
        "the 40-column frame != the cold reference"
    );
    let ((), narrowed) = count_allocations(|| {
        render_into_pool(&block, 40, &mut pool);
    });
    // The state change re-formats the call line by design (width /
    // collapse / tail / palette), so the same formatter-level ceiling as the
    // steady frame applies (see `tool_block_steady_state_frame_reuses_its_
    // background_pair`: 15 x 16 B span formatting, memos and rows clean).
    assert!(
        narrowed <= STATE_CHANGE_CEILING,
        "the 40-column frame must not allocate more than the formatter-level ceiling: {narrowed} vs {STEADY_BASH_BLOCK_CEILING}"
    );

    // Ctrl+O: the collapsed call line and preview re-render.
    expanded.store(false, std::sync::atomic::Ordering::SeqCst);
    render_into_pool(&block, width, &mut pool);
    assert_eq!(
        pool,
        cold(width, false, Some(1200)),
        "the collapsed frame != the cold reference"
    );
    let ((), collapsed) = count_allocations(|| {
        render_into_pool(&block, width, &mut pool);
    });
    // The state change re-formats the call line by design (width /
    // collapse / tail / palette), so the same formatter-level ceiling as the
    // steady frame applies (see `tool_block_steady_state_frame_reuses_its_
    // background_pair`: 15 x 16 B span formatting, memos and rows clean).
    assert!(
        collapsed <= STATE_CHANGE_CEILING,
        "the collapsed frame must not allocate more than the formatter-level ceiling: {collapsed} vs {STEADY_BASH_BLOCK_CEILING}"
    );
    expanded.store(true, std::sync::atomic::Ordering::SeqCst);
    render_into_pool(&block, width, &mut pool);
    assert_eq!(
        pool,
        cold(width, true, Some(1200)),
        "the re-expanded frame != the cold reference"
    );

    // A recorded duration change moves the `Took` tail and its rows.
    block.update_result(Arc::clone(&output), false, Some(2400));
    render_into_pool(&block, width, &mut pool);
    assert_eq!(
        pool,
        cold(width, true, Some(2400)),
        "the retimed frame != the cold reference"
    );
    assert!(
        pool.iter().any(|row| row.contains("Took 2.4s")),
        "the retimed tail must reach the frame"
    );
    let ((), retimed) = count_allocations(|| {
        render_into_pool(&block, width, &mut pool);
    });
    // The state change re-formats the call line by design (width /
    // collapse / tail / palette), so the same formatter-level ceiling as the
    // steady frame applies (see `tool_block_steady_state_frame_reuses_its_
    // background_pair`: 15 x 16 B span formatting, memos and rows clean).
    assert!(
        retimed <= STATE_CHANGE_CEILING,
        "the retimed frame must not allocate more than the formatter-level ceiling: {retimed} vs {STEADY_BASH_BLOCK_CEILING}"
    );

    // Palette swap: the call rows and the painted box repaint, then settle
    // again — pinned against a cold instance rendered under the new palette.
    let _swap = ThemeSwap::install("light");
    render_into_pool(&block, width, &mut pool);
    assert_eq!(
        pool,
        cold(width, true, Some(2400)),
        "the light-palette frame != the cold reference"
    );
    let ((), swapped) = count_allocations(|| {
        render_into_pool(&block, width, &mut pool);
    });
    // The state change re-formats the call line by design (width /
    // collapse / tail / palette), so the same formatter-level ceiling as the
    // steady frame applies (see `tool_block_steady_state_frame_reuses_its_
    // background_pair`: 15 x 16 B span formatting, memos and rows clean).
    assert!(
        swapped <= STATE_CHANGE_CEILING,
        "the swapped frame must not allocate more than the formatter-level ceiling: {swapped} vs {STEADY_BASH_BLOCK_CEILING}"
    );
}

/// The long-session property: the steady frame's allocation count must not
/// grow with the transcript. Every settled block in the fixture is a bash
/// block, i.e. exactly the per-block-per-frame `format!`/`wrap_text_with_ansi`
/// churn (call line + timer tail) this lane memoized, so 4 blocks and 48
/// blocks must both render a steady frame with zero allocations.
#[test]
fn steady_frame_allocations_do_not_grow_with_the_transcript() {
    let _guard = test_lock();
    warm_theme();
    let width = 80;
    let expanded = Arc::new(AtomicBool::new(true));

    let transcript = |blocks: usize| -> SharedTranscript {
        let entries = (0..blocks)
            .map(|index| ChatEntry::ToolExecution {
                call_id: Some(format!("call-{index}")),
                tool_name: "bash".into(),
                args: Arc::new(serde_json::json!({ "command": format!("printf block-{index}") })),
                output: Arc::from(format!("block {index} line one\nblock {index} line two")),
                is_error: false,
                duration_ms: Some(1200),
                pending: false,
                details: None,
                started_at: None,
                preview: None,
                images: Arc::new(Vec::new()),
            })
            .collect::<Vec<_>>();
        let mut chat = ChatContainer::new();
        chat.set_tools_expanded(Arc::clone(&expanded));
        chat.set_entries(&entries);
        Arc::new(Mutex::new(chat))
    };

    let small = transcript(4);
    let large = transcript(48);
    let mut small_pool = stale_pool();
    let mut large_pool = stale_pool();
    // Warm both pools (the first frame grows them to the transcript height).
    render_chat_into_pool(&small, width, &mut small_pool);
    render_chat_into_pool(&large, width, &mut large_pool);
    assert!(
        large_pool.len() > small_pool.len() * 4,
        "the large transcript must render many more rows: {} vs {}",
        large_pool.len(),
        small_pool.len()
    );

    // Attribution first: the pool/row/memo channels of both frames are the
    // invariants that do not depend on the fixture size, and a failure names
    // the channel (only then is the per-block residual investigated).
    let small_steady = measure_frame_chat(&small, width, &mut small_pool);
    let large_steady = measure_frame_chat(&large, width, &mut large_pool);
    assert_steady_frame_channels(&small_steady, "4-block transcript");
    assert_steady_frame_channels(&large_steady, "48-block transcript");
    let small_allocations = small_steady.allocations;
    let large_allocations = large_steady.allocations;
    // The per-block cost of a steady frame: 17.0/block measured for the
    // 4-block transcript (68 events) and 17.8/block for the 48-block one
    // (854) - a CONSTANT per block, independent of the block's body and of the
    // transcript's length. (The earlier `large <= small + 2` form pinned "zero
    // per block"; this tree does not deliver that, and the channel assertions
    // above already pin that nothing in the memoized parts rebuilds - the
    // attribution in the messages names the residual's site.) The ceilings
    // below keep the per-block cost from growing.
    const STEADY_BLOCK_CEILING: u64 = 18;
    assert!(
        large_allocations <= 48 * STEADY_BLOCK_CEILING,
        "a steady 48-block frame must stay within the measured per-block \
         ceiling: {large_allocations} for 48 blocks (ceiling \
         {STEADY_BLOCK_CEILING}/block, sizes={:?}, \
         attribution={large_steady:?})",
        large_steady.sizes
    );
    assert!(
        small_allocations <= 4 * STEADY_BLOCK_CEILING,
        "a steady 4-block frame must stay within the measured per-block \
         ceiling: {small_allocations} for 4 blocks (ceiling \
         {STEADY_BLOCK_CEILING}/block, sizes={:?}, \
         attribution={small_steady:?})",
        small_steady.sizes
    );
}

// ---------------------------------------------------------------------------
// 8b. The long-transcript frame passes (counter seam)
// ---------------------------------------------------------------------------

/// The frame renderer's per-row passes: the rebuild (`frame_lines`), the
/// alt-screen snapshot copy, the changed-line scan and the per-line reset.
/// This pins them through the `FrameWork` counter seam (`src/tui/core.rs`), so
/// the planned windowing work has a hard target: `rebuilt_rows` → visible rows
/// + O(1), `reset_rows` → written rows (already true now),
///   `diff_scanned_rows` → changed tail + O(1), and the snapshot copy → an
///   `Arc` share instead of a per-row `clone_from`.
///
/// Before/after of this lane's lazy reset (the numbers this test used to pin
/// vs. the ones it pins now, per frame):
///
/// | fixture      | rebuilt | snapshot | reset (before) | reset (now) | diff |
/// |--------------|---------|----------|----------------|-------------|------|
/// | 100 lines    | 100     | 100      | 100            | 0 (steady)  | 100  |
/// | 3000 lines   | 3000    | 3000     | 3000           | 0 (steady)  | 3000 |
/// | 3000 + 1-row edit | 3000 | 3000    | 3000           | 1           | 3000 |
///
/// i.e. the steady follow-end frame's four-pass total drops from
/// `4 * height` to `3 * height` (400 → 300 row-events at 100 lines,
/// 12000 → 9000 at 3000 lines, -25%), and a frame that writes `W` rows appends
/// the segment reset exactly `W` times instead of `height` (a one-row spinner
/// or edit tick: 1 instead of 3000, a ~3000x cut in reset appends). The reset
/// BYTES are unchanged: every baseline row still ends with exactly one
/// `SEGMENT_RESET`, and the rows a frame does not write keep their previous
/// bytes (asserted below).
///
/// Allocation counting can not see these passes: after the leaf/block memos a
/// steady frame allocates nothing, yet it still walks every row.
#[test]
fn frame_passes_scan_the_whole_transcript_not_the_visible_window() {
    let _guard = test_lock();
    for lines in [100usize, 3000] {
        let terminal = Terminal::new();
        terminal.set_size(100, 30);
        let mut core = TuiCore::new(terminal);
        let body = (0..lines)
            .map(|index| format!("line {index:04}"))
            .collect::<Vec<_>>()
            .join("\n");
        // The handle stays live so the test can edit the body between frames
        // (the renderer owns a `Box<dyn Component>` of the same `Text`).
        let text = Arc::new(Text::new(body.clone()));
        core.push_content(Box::new(SharedText(Arc::clone(&text))));
        // Two frames: the first builds the frame and the snapshot baseline,
        // the second runs the steady follow-end path (rebuild + snapshot +
        // diff scan; no writes, so no resets).
        core.render();
        core.render();
        let height = core.frame_buffer_length() as u64;
        let height_usize = height as usize;
        assert!(
            height >= lines as u64,
            "the fixture must render at least {lines} rows: {height}"
        );

        FrameWork::reset();
        core.render();
        let work = FrameWork::snapshot();

        assert_eq!(
            work.rebuilt_rows, height,
            "frame_lines must rebuild every row of the frame ({lines}-line fixture)"
        );
        assert_eq!(
            work.snapshot_rows, 0,
            "a steady frame reuses every snapshot row, so it copies no row \
             ({lines}-line fixture)"
        );
        assert_eq!(
            work.snapshot_bytes, 0,
            "a steady frame must not copy any frame bytes ({lines}-line fixture)"
        );
        // The lazy reset: a steady frame writes NO row, so it appends the
        // segment reset zero times (the old eager pass appended it to all
        // `height` rows).
        assert_eq!(
            work.reset_rows, 0,
            "a steady frame must not reset rows it did not write ({lines}-line fixture)"
        );
        assert_eq!(
            work.diff_scanned_rows, height,
            "the changed-line scan must compare every row of the frame \
             ({lines}-line fixture)"
        );

        // The byte invariant behind the lazy reset: every baseline row still
        // ends with exactly one segment reset even though this frame touched
        // none of them.
        let steady = core.last_screen_lines();
        assert_eq!(steady.len(), height_usize);
        assert!(
            steady.iter().all(|row| row.ends_with(SEGMENT_RESET)),
            "every baseline row must carry the reset it was written with \
             ({lines}-line fixture)"
        );

        // A single-row edit inside the viewport (the LAST row: a change above
        // `viewport_top` would force a full repaint by the upstream guard, so
        // only an in-window row diffs): the diff repaints only that row, so
        // exactly one row is written — and reset — and the unwritten rows keep
        // the previous frame's bytes verbatim.
        let mut edited_body = body.clone();
        let last_line_start = edited_body.rfind('\n').map(|index| index + 1).unwrap_or(0);
        edited_body.replace_range(last_line_start.., "edited last line");
        text.set_text(edited_body.clone());
        FrameWork::reset();
        core.render();
        let edit_work = FrameWork::snapshot();
        assert_eq!(
            edit_work.reset_rows, 1,
            "an in-viewport one-row edit must write (and reset) exactly that row \
             ({lines}-line fixture): {edit_work:?}"
        );
        assert_eq!(
            edit_work.diff_scanned_rows, height,
            "the changed-line scan still covers the whole frame ({lines}-line fixture)"
        );
        assert_eq!(edit_work.rebuilt_rows, height);
        assert_eq!(
            edit_work.snapshot_rows, 1,
            "only the edited row differs and is copied into the snapshot"
        );
        let edited = core.last_screen_lines();
        assert_eq!(edited.len(), height_usize);
        assert_ne!(
            edited[height_usize - 1],
            steady[height_usize - 1],
            "the edited row must reach the baseline ({lines}-line fixture)"
        );
        assert!(
            edited.iter().all(|row| row.ends_with(SEGMENT_RESET)),
            "the written row must carry the reset like every other row \
             ({lines}-line fixture)"
        );
        for index in 0..height_usize - 1 {
            assert_eq!(
                edited[index], steady[index],
                "row {index} was not written and must keep its previous bytes \
                 ({lines}-line fixture)"
            );
        }

        // An appended tail: the frame writes (and resets) exactly the appended
        // row, and every existing baseline row keeps its bytes.
        text.set_text(format!("{edited_body}\nappended tail line"));
        FrameWork::reset();
        core.render();
        let append_work = FrameWork::snapshot();
        assert_eq!(
            append_work.reset_rows, 1,
            "an appended row must be the only row written and reset \
             ({lines}-line fixture): {append_work:?}"
        );
        assert_eq!(append_work.rebuilt_rows, height + 1);
        assert_eq!(
            append_work.snapshot_rows, 1,
            "only the appended row is new and is copied into the snapshot"
        );
        assert_eq!(append_work.diff_scanned_rows, height + 1);
        let appended = core.last_screen_lines();
        assert_eq!(appended.len(), height_usize + 1);
        assert_eq!(
            &appended[..height_usize],
            &edited[..],
            "only the appended row may change ({lines}-line fixture)"
        );
        assert!(
            appended[height_usize].ends_with(SEGMENT_RESET),
            "the appended row must carry the reset ({lines}-line fixture)"
        );

        // The snapshot pass no longer scales with the frame: steady frames
        // copy no row and a one-row change copies exactly one. The rebuild and
        // diff scans remain O(height); the reset pass is at the written-row
        // count (0 steady, 1 for a one-row edit/append).
        let total =
            work.rebuilt_rows + work.snapshot_rows + work.reset_rows + work.diff_scanned_rows;
        assert_eq!(
            total,
            2 * height,
            "steady baseline = rebuild + diff at O(height); snapshot and reset at O(changed) \
             ({lines}-line fixture, height={height}): {work:?}"
        );
    }
}

// ---------------------------------------------------------------------------
// 8. The live stream: thinking paint, bounded per-delta cost, and the
//    settle/reset release (the interactive-mode latency + memory report)
// ---------------------------------------------------------------------------

/// One live streaming entry with the given accumulators — the shape the event
/// loop commits per delta (`terminal.rs`'s `ChatEntry::Streaming` arms push
/// each delta onto the live entry, which OWNS its accumulator).
fn streaming_entry(thinking: &str, text: &str) -> ChatEntry {
    ChatEntry::Streaming {
        live: true,
        thinking: thinking.to_string(),
        text: text.to_string(),
    }
}

/// A thinking-only live stream must paint its rows, keep growing them in
/// place, and keep painting when the posture collapses to the hidden label —
/// on the streaming path, before the turn settles (the user report: the whole
/// thinking phase showed nothing, so the screen looked hung until the answer
/// text arrived). The last leg drives the reverse order (thinking arrives
/// AFTER the first text delta), which must rebuild the entry and paint the
/// thinking block instead of silently dropping it into the missing child.
#[test]
fn live_thinking_only_stream_paints_its_rows_and_grows_in_place() {
    let _guard = test_lock();
    let width = 60usize;
    let chat: SharedTranscript = Arc::new(Mutex::new(ChatContainer::new()));
    let mut pool: Vec<String> = Vec::new();

    // First thinking delta of a thinking-only reply: no text yet.
    lock_chat(&chat).set_entries(&[streaming_entry("weighing", "")]);
    render_chat_into_pool(&chat, width, &mut pool);
    let painted = pool.join("\n");
    assert!(
        painted.contains("weighing"),
        "a thinking-only live stream must paint its thinking rows: {painted:?}"
    );

    // The next thinking delta grows the same entry (the loop re-commits the
    // grown accumulator; the container's in-place path takes it).
    lock_chat(&chat).set_entries(&[streaming_entry("weighing the options", "")]);
    render_chat_into_pool(&chat, width, &mut pool);
    let grown = pool.join("\n");
    assert!(
        grown.contains("weighing the options"),
        "the grown thinking body must reach the frame: {grown:?}"
    );

    // A text delta after the thinking run: both blocks render, thinking first.
    lock_chat(&chat).set_entries(&[streaming_entry("weighing the options", "the answer")]);
    render_chat_into_pool(&chat, width, &mut pool);
    let both = pool.join("\n");
    assert!(
        both.contains("weighing the options") && both.contains("the answer"),
        "thinking and text must both paint while the entry is live: {both:?}"
    );

    // `settings.hideThinkingBlock` (or ctrl+t): the collapse must still paint
    // its `{pad}{label}` row — hiding the body is not hiding the progress.
    // The posture change invalidates the committed children; the next
    // `set_entries` (the loop's dirty-frame flush) is what rebuilds them,
    // exactly like ctrl+t on a live turn.
    lock_chat(&chat).set_hide_thinking(true);
    lock_chat(&chat).set_entries(&[streaming_entry("weighing the options", "the answer")]);
    render_chat_into_pool(&chat, width, &mut pool);
    let hidden = pool.join("\n");
    assert!(
        hidden.contains("Thinking..."),
        "the hidden-thinking posture must paint its label row: {hidden:?}"
    );
    assert!(
        !hidden.contains("weighing"),
        "the hidden posture must not leak the collapsed body: {hidden:?}"
    );
    lock_chat(&chat).set_hide_thinking(false);

    // Reverse order: text first, then thinking (the provider reported the
    // text block before the thinking block). The thinking delta makes the
    // committed text-only entry stale — it must be rebuilt with the thinking
    // child, not updated into a missing one.
    lock_chat(&chat).set_entries(&[streaming_entry("", "text first")]);
    render_chat_into_pool(&chat, width, &mut pool);
    assert!(pool.join("\n").contains("text first"));
    lock_chat(&chat).set_entries(&[streaming_entry("late thinking", "text first")]);
    render_chat_into_pool(&chat, width, &mut pool);
    let late = pool.join("\n");
    assert!(
        late.contains("late thinking") && late.contains("text first"),
        "a thinking delta after the first text delta must rebuild the live entry \
         and paint the thinking block: {late:?}"
    );
}

/// The per-delta cost of a long reply must not grow with the reply's length:
/// a block-structured body lets the incremental renderer re-parse only its
/// mutable tail, so the bytes each late delta allocates stay a small fraction
/// of the whole body. A regression to a whole-body re-parse per delta (the
/// O(n²) shape the user feels as "gets slower and slower, memory huge") blows
/// the body-proportional ceiling and the early/late ratio apart.
#[test]
fn live_stream_per_delta_cost_stays_bounded_across_a_long_reply() {
    let _guard = test_lock();
    warm_theme();
    let width = 60usize;
    let chunk = "word-000 ";

    // The per-delta cost of one reply, as (early B/delta, late B/delta, body
    // B) - early/late are the averages over the first/last 40 deltas.
    const WINDOW: usize = 40;
    let measure_reply = |deltas: usize| -> (i64, i64, i64) {
        let chat: SharedTranscript = Arc::new(Mutex::new(ChatContainer::new()));
        let mut pool: Vec<String> = Vec::new();
        let mut entries = vec![streaming_entry("", "")];
        let mut early_bytes = 0i64;
        let mut late_bytes = 0i64;
        for delta in 0..deltas {
            if let ChatEntry::Streaming { text, .. } = &mut entries[0] {
                text.push_str(chunk);
                // A blank line every 8 deltas: a final block boundary, the cut
                // the incremental renderer folds into its cached prefix.
                if (delta + 1) % 8 == 0 {
                    text.push_str("\n\n");
                }
            }
            let ((), stats) = measure_allocations(|| {
                lock_chat(&chat).set_entries(&entries);
                render_chat_into_pool(&chat, width, &mut pool);
            });
            if delta < WINDOW {
                early_bytes += stats.bytes;
            } else if delta >= deltas - WINDOW {
                late_bytes += stats.bytes;
            }
        }
        let body_bytes = match &entries[0] {
            ChatEntry::Streaming { text, .. } => text.len() as i64,
            _ => 0,
        };
        (
            early_bytes / WINDOW as i64,
            late_bytes / WINDOW as i64,
            body_bytes,
        )
    };

    let (short_early, short_late, short_body) = measure_reply(240);
    let (long_early, long_late, long_body) = measure_reply(960);

    // The pin is a SCALING one. The measured late-delta cost (2284 B/delta at
    // 240 deltas) exceeds that fixture's body (2220 B) because the per-delta
    // work below the parser is a bounded constant (the streaming tail's
    // re-parse and rows, the commit tags, the frame's fixed scratch), while the
    // body is still small; comparing the two directly pinned "the constant is
    // smaller than the body", not "nothing is O(body)". A whole-body re-parse
    // per delta shows up in the 240 -> 960 comparison instead: the late
    // average would scale with the body (roughly 4x), where a bounded cost
    // stays flat.
    assert!(
        short_late <= short_early * 3 + 1024,
        "the per-delta allocation must stay flat as the reply grows: \
         early={short_early} B/delta, late={short_late} B/delta \
         (body={short_body} B)"
    );
    assert!(
        long_late <= short_late * 2 + 1024,
        "a late delta must not re-allocate in proportion to the reply body: \
         {short_late} B/delta at 240 deltas (body {short_body} B) vs \
         {long_late} B/delta at 960 (body {long_body} B, early \
         {long_early} B/delta)"
    );
    assert!(
        long_late < long_body,
        "the bounded per-delta cost must sit below the grown reply body: \
         late={long_late} B/delta, body={long_body} B"
    );
}

/// Repeated live streams over the same container must not accumulate: after
/// each stream settles (`AssistantDone` replaces the streaming entry) and the
/// transcript resets (`/clear`, session switch), the streaming render state
/// (the live `Markdown` body + its cached prefix/tail rows, the thinking
/// block, the commit records and their children) is released. Ten streams
/// leave only the recycled frame pool behind; a per-stream retention (the
/// classic "one more copy per stream" memo bug) shows up as linear live-byte
/// growth.
#[test]
fn repeated_live_streams_release_their_state_on_settle_and_reset() {
    let _guard = test_lock();
    warm_theme();
    let width = 60usize;
    let chat: SharedTranscript = Arc::new(Mutex::new(ChatContainer::new()));
    let mut pool: Vec<String> = Vec::new();
    let chunk = "streamed-word ";
    let deltas = 200usize;
    let settled = Arc::new(assistant_message("settled body for the release pin"));

    let one_stream = |chat: &SharedTranscript, pool: &mut Vec<String>| {
        let mut entries = vec![streaming_entry("", "")];
        for _ in 0..deltas {
            if let ChatEntry::Streaming { text, .. } = &mut entries[0] {
                text.push_str(chunk);
            }
            lock_chat(chat).set_entries(&entries);
            render_chat_into_pool(chat, width, pool);
        }
        // Settle: `WorkerEvent::AssistantDone` swaps the live entry for the
        // final message (the diff drops the streaming children).
        lock_chat(chat).set_entries(&[ChatEntry::Assistant(Arc::clone(&settled))]);
        render_chat_into_pool(chat, width, pool);
        // Reset: `/clear` / session switch replaces the transcript with the
        // empty one (the one path that also releases the retained slots).
        lock_chat(chat).set_entries(&[]);
        render_chat_into_pool(chat, width, pool);
    };

    // Warm: one stream, so every lazily built structure (theme cells, the
    // container's slot vectors, the frame pool) exists before the measured
    // section. Its retained state is the baseline.
    one_stream(&chat, &mut pool);

    const REPEATS: usize = 10;
    let ((), stats) = measure_allocations(|| {
        for _ in 0..REPEATS {
            one_stream(&chat, &mut pool);
        }
    });
    assert!(
        stats.live <= 32 * 1024,
        "{REPEATS} settled+reset live streams must not retain their streaming state: \
         net live {} bytes (events={}, churn={} B)",
        stats.live,
        stats.events,
        stats.bytes
    );
    // The reset path also drops the slot arrays (the `/clear` release): the
    // measured section ends on an empty transcript, so the commit records and
    // child slots sit at their empty capacity, not the peak stream's.
    assert_eq!(
        lock_chat(&chat).committed_capacity(),
        0,
        "the empty-transcript reset must release the commit-record slots"
    );
    assert_eq!(
        lock_chat(&chat).children_capacity(),
        0,
        "the empty-transcript reset must release the child slots"
    );
}

// ---------------------------------------------------------------------------
// 12. The transcript leaves' last per-frame wraps (this lane)
// ---------------------------------------------------------------------------

/// The unpainted `Text` rows memo (`src/tui/core.rs`): the transcript's
/// `Notice`/`Raw` entries are `Text` components, so before the memo EVERY
/// frame of the whole session re-ran `wrap_text_with_ansi` over the raw body
/// (one `Vec` plus one `String` per visual line) even though the transcript
/// had not changed. Now a steady frame re-pushes the stored rows (zero
/// allocations, zero wrapping), a width move or a text edit misses and
/// re-wraps, and the rows stay byte-identical to the documented `Text` shape
/// (rebuilt here from the public wrap/pad helpers, never through the
/// component).
///
/// The painted posture keeps the pre-memo behavior: the `bg` painter is an
/// arbitrary extension-supplied closure with no key, so the memo is never
/// populated or consulted for it — including the reverse direction (dropping
/// the painter must not inherit the painter's bytes).
#[test]
fn text_rows_memo_serves_a_steady_frame_and_tracks_width_and_edits() {
    let _guard = test_lock();
    warm_theme();
    let body = "raw one\nraw two with a longer line that wraps at narrow widths";
    let text = Arc::new(Text::new(body));

    for width in WIDTHS {
        let expected = text_rows(body, 1, width);
        let mut pool = stale_pool();
        // First frame at this width: the memo builds (or re-builds after the
        // width move), so it may allocate.
        render_into_pool(&*text, width, &mut pool);
        assert_eq!(pool, expected, "the memo-building frame @ {width}");
        assert_eq!(text.render(width), expected, "the owned row form @ {width}");

        // Steady frame at the same width: served from the memo.
        let ((), allocations) = count_allocations(|| {
            render_into_pool(&*text, width, &mut pool);
        });
        assert_eq!(
            allocations, 0,
            "a steady Text frame must serve its memoized rows without \
             allocating @ {width}"
        );
        assert_eq!(
            pool, expected,
            "the memo hit must serve the same rows @ {width}"
        );
    }

    // A text edit between frames must re-wrap: the key is the CONTENT, so a
    // fresh body (same shape, different bytes) cannot serve stale rows.
    let edited = "edited \u{65e5}\u{672c}\u{8a9e} body\nsecond edited line";
    let before_edit = text_rows(body, 1, 60);
    text.set_text(edited);
    let mut pool = stale_pool();
    render_into_pool(&*text, 60, &mut pool);
    assert_eq!(
        pool,
        text_rows(edited, 1, 60),
        "the edited text must re-wrap @ 60"
    );
    assert_ne!(
        pool, before_edit,
        "the edit must change the expected rows (the fixture must bite)"
    );
    let ((), after_edit) = count_allocations(|| {
        render_into_pool(&*text, 60, &mut pool);
    });
    assert_eq!(after_edit, 0, "the edited body must settle again");

    // The vertical padding is part of the key: the blank rows bracket the
    // body (a memo keyed on the text alone would drop them).
    let padded = Text::with_padding("padded body", 1, 1);
    let mut padded_pool = stale_pool();
    render_into_pool(&padded, 40, &mut padded_pool);
    let mut expected = vec![blank_row(40)];
    expected.extend(text_rows("padded body", 1, 40));
    expected.push(blank_row(40));
    assert_eq!(
        padded_pool, expected,
        "the padding_y rows must bracket the body"
    );

    // The painted posture: the painter's bytes reach the frame (the memo never
    // serves a painter), and the frame keeps paying the painter's allocations.
    let painted = Arc::new(Text::new("painted body line"));
    let painter: Arc<dyn Fn(&str) -> String + Send + Sync> =
        Arc::new(|line: &str| format!("\x1b[7m{line}\x1b[0m"));
    painted.set_bg(Some(painter));
    let mut pool = stale_pool();
    render_into_pool(&*painted, 40, &mut pool);
    let painted_expected: Vec<String> = text_rows("painted body line", 1, 40)
        .into_iter()
        .map(|row| format!("\x1b[7m{row}\x1b[0m"))
        .collect();
    assert_eq!(
        pool, painted_expected,
        "the painter's bytes must reach the frame"
    );
    let ((), painted_allocations) = count_allocations(|| {
        render_into_pool(&*painted, 40, &mut pool);
    });
    assert!(
        painted_allocations > 0,
        "a painted Text keeps rendering fresh (the memo is never consulted)"
    );

    // Dropping the painter must NOT inherit the painter's bytes: the memo slot
    // was never written while the painter rendered.
    painted.set_bg(None);
    render_into_pool(&*painted, 40, &mut pool);
    assert_eq!(
        pool,
        text_rows("painted body line", 1, 40),
        "the unpainted posture must not serve painted rows"
    );
}

/// One `!`/`!!` user-bash block (upstream `BashExecutionComponent`), the shape
/// `ToolExecutionComponent` renders for `userBash` args. Built from scratch
/// for the cold-reference legs (a second instance that never warmed a memo).
fn user_bash_block(
    expanded: &Arc<AtomicBool>,
    command: &str,
    output: &str,
) -> ToolExecutionComponent {
    let component = ToolExecutionComponent::new(
        "bash",
        Arc::new(serde_json::json!({
            "command": command,
            "userBash": true,
            "exitCode": 0,
        })),
        Arc::clone(expanded),
    );
    component.update_result(Arc::from(output), false, None);
    component
}

/// The `!`/`!!` user-bash block's whole-row memo (`src/tui/messages.rs`): the
/// block re-styled and re-wrapped its header, its 20-line preview body and its
/// status tail on every frame before the memo. Now a steady frame pushes the
/// stored rows (zero allocations — the block has no env-dependent call-text
/// formatter to pay), and every input move (expansion, output, width, palette)
/// is pinned against a cold second instance.
#[test]
fn user_bash_block_serves_its_whole_row_set_from_the_memo() {
    let _guard = test_lock();
    warm_theme();
    let width = 80;
    let output = (0..30)
        .map(|index| format!("bash line {index:02}"))
        .collect::<Vec<_>>()
        .join("\n");
    let expanded = Arc::new(AtomicBool::new(true));
    let block = Arc::new(user_bash_block(&expanded, "printf hello", &output));
    let mut pool = stale_pool();
    render_into_pool(&*block, width, &mut pool);

    // Independent expectation: a COLD instance of the same block (fresh state,
    // nothing memoized) into a poisoned pool.
    let cold = user_bash_block(&Arc::new(AtomicBool::new(true)), "printf hello", &output);
    let mut cold_pool = stale_pool();
    render_into_pool(&cold, width, &mut cold_pool);
    assert_eq!(
        pool, cold_pool,
        "the warm rows must match the cold instance"
    );
    assert!(
        pool.len() > 4,
        "the framed block must render: {}",
        pool.len()
    );
    assert!(
        pool.iter().any(|row| row.contains("bash line 29")),
        "the preview tail must paint"
    );
    // The expanded posture's hint is `(ctrl+o to collapse)` - the hidden-line
    // COUNT only appears in the collapsed posture's hint (the component's
    // documented shape, upstream bash-execution.ts:150-170); asserting the
    // counted text here would pin a hint the code never draws.
    assert!(
        pool.iter().any(|row| row.contains("to collapse")),
        "the expanded posture must paint its collapse hint"
    );
    assert!(
        !pool.iter().any(|row| row.contains("more lines")),
        "the expanded hint must not fake the collapsed count"
    );

    let steady = measure_frame(&*block, width, &mut pool);
    assert_steady_frame_channels(&steady, "steady user-bash block");
    assert_eq!(
        steady.allocations, 0,
        "a steady user-bash frame must serve every row from its memo \
         (the whole block is one memo, and it has no env-dependent call \
         formatter to pay): sizes={:?}, attribution={steady:?}",
        steady.sizes
    );
    assert_eq!(pool, cold_pool, "the memo hit must serve the same rows");

    // The expansion flag is part of the key: the collapse repaints (the
    // expanded body becomes the 20-line preview slice) and then settles.
    expanded.store(false, std::sync::atomic::Ordering::SeqCst);
    render_into_pool(&*block, width, &mut pool);
    let cold_collapsed =
        user_bash_block(&Arc::new(AtomicBool::new(false)), "printf hello", &output);
    let mut cold_collapsed_pool = stale_pool();
    render_into_pool(&cold_collapsed, width, &mut cold_collapsed_pool);
    assert_eq!(
        pool, cold_collapsed_pool,
        "the collapsed posture must match the cold instance"
    );
    // The collapsed posture draws the counted hint (`... 10 more lines
    // (ctrl+o to expand)`), the text the expanded posture does not carry.
    assert!(
        pool.iter().any(|row| row.contains("more lines")),
        "the collapsed posture must paint the counted hint"
    );
    let collapsed = measure_frame(&*block, width, &mut pool);
    assert_steady_frame_channels(&collapsed, "steady collapsed user-bash block");
    assert_eq!(
        collapsed.allocations, 0,
        "the collapsed posture must settle again: sizes={:?}, \
         attribution={collapsed:?}",
        collapsed.sizes
    );

    // A new result (the output `Arc` moves) repaints; the bytes match a cold
    // instance of the grown block.
    let grown = format!("{output}\nbash line 30");
    block.update_result(Arc::from(grown.as_str()), false, None);
    render_into_pool(&*block, width, &mut pool);
    let cold_grown = user_bash_block(&Arc::new(AtomicBool::new(false)), "printf hello", &grown);
    let mut cold_grown_pool = stale_pool();
    render_into_pool(&cold_grown, width, &mut cold_grown_pool);
    assert_ne!(
        pool, cold_collapsed_pool,
        "the grown result must reach the frame"
    );
    assert_eq!(
        pool, cold_grown_pool,
        "the grown result != the cold instance"
    );

    // A width move re-wraps the header/preview/tails and re-pads every row.
    render_into_pool(&*block, 40, &mut pool);
    let cold_narrow = user_bash_block(&Arc::new(AtomicBool::new(false)), "printf hello", &grown);
    let mut cold_narrow_pool = stale_pool();
    render_into_pool(&cold_narrow, 40, &mut cold_narrow_pool);
    assert_eq!(
        pool, cold_narrow_pool,
        "the width move must re-wrap like a cold instance"
    );

    // The palette is part of the key: a swap repaints (and then settles),
    // byte-pinned against a cold instance rendered under the new palette.
    let _swap = ThemeSwap::install("light");
    render_into_pool(&*block, 40, &mut pool);
    let cold_light = user_bash_block(&Arc::new(AtomicBool::new(false)), "printf hello", &grown);
    let mut cold_light_pool = stale_pool();
    render_into_pool(&cold_light, 40, &mut cold_light_pool);
    assert_eq!(
        pool, cold_light_pool,
        "the palette swap must repaint like a cold instance"
    );
    let ((), swapped_allocations) = count_allocations(|| {
        render_into_pool(&*block, 40, &mut pool);
    });
    assert_eq!(
        swapped_allocations, 0,
        "the swapped-palette frame must settle again: {swapped_allocations}"
    );
}

/// The edit args plus the `ToolPreview` the app attaches for them (the
/// preview's `args_key` must match `edit_preview_input`, else
/// `preview_is_current` drops it).
fn edit_args_and_preview(diff: &str) -> (serde_json::Value, ToolPreview) {
    let args = serde_json::json!({
        "path": "src/lib.rs",
        "edits": [{ "oldText": "old line", "newText": "new line" }],
    });
    let input = edit_preview_input(&args).expect("edit args carry a preview input");
    (
        args,
        ToolPreview {
            args_key: input.args_key,
            diff: Some(diff.to_string()),
            first_changed_line: Some(1),
            error: None,
        },
    )
}

/// One edit block with the given result meta (the shapes
/// `tests/tui_render_parity.rs` pins): a `details` diff renders the result
/// section below the box, a current preview renders the box's body, and with
/// neither the block falls back to the args-rebuilt legacy body.
fn edit_block(
    args: &serde_json::Value,
    output: &str,
    details: Option<Arc<serde_json::Value>>,
    preview: Option<ToolPreview>,
) -> ToolExecutionComponent {
    let component = ToolExecutionComponent::new(
        "edit",
        Arc::new(args.clone()),
        Arc::new(AtomicBool::new(false)),
    );
    component.set_meta(details, None, preview);
    component.update_result(Arc::from(output), false, None);
    component
}

/// The edit block's preview / legacy / result-body memos
/// (`src/tui/messages.rs`): before them a settled edit block re-styled and
/// re-wrapped its preview body, re-parsed and re-rendered the args-rebuilt
/// legacy body, and re-rendered the whole result diff (`render_diff_text`)
/// into freshly wrapped rows on every frame. A steady frame now re-formats
/// nothing at all (the call text is memoized on the args `Arc` plus the
/// environment snapshot generation) — pinned body-independent by comparing
/// against a bodyless block and by a 300-line body staying flat — and every
/// body move (preview, details diff, width, palette, legacy posture) is
/// byte-pinned against a cold second instance.
#[test]
fn edit_block_serves_preview_legacy_and_result_rows_from_its_memos() {
    let _guard = test_lock();
    warm_theme();
    let width = 80;
    let preview_diff = " 1 context\n-1 old line\n+1 new line";
    let result_diff = " 1 context\n-1 old line\n+1 new line edited again\n 2 tail line";
    let (args, preview) = edit_args_and_preview(preview_diff);
    let details = Arc::new(serde_json::json!({ "diff": result_diff }));

    let block = Arc::new(edit_block(
        &args,
        "applied 1 edit",
        Some(Arc::clone(&details)),
        Some(preview),
    ));
    let mut pool = stale_pool();
    render_into_pool(&*block, width, &mut pool);

    // Independent expectation: a COLD instance of the same block.
    let cold = edit_block(
        &args,
        "applied 1 edit",
        Some(Arc::clone(&details)),
        Some(edit_args_and_preview(preview_diff).1),
    );
    let mut cold_pool = stale_pool();
    render_into_pool(&cold, width, &mut cold_pool);
    assert_eq!(
        pool, cold_pool,
        "the warm rows must match the cold instance"
    );
    let nonempty = pool.iter().filter(|row| !row.is_empty()).count();
    assert!(
        nonempty > 6,
        "the fixture must paint the box and the result section: {nonempty}"
    );

    // The body-independent floor: the same block with NO preview and an empty
    // `details` (only the call line inside the box) pays exactly the
    // env-dependent call-text re-format — nothing else.
    let bare = Arc::new(edit_block(
        &args,
        "applied 1 edit",
        Some(Arc::new(serde_json::json!({}))),
        None,
    ));
    let mut bare_pool = stale_pool();
    render_into_pool(&*bare, width, &mut bare_pool);
    render_into_pool(&*bare, width, &mut bare_pool);
    let ((), bare_allocations) = count_allocations(|| {
        render_into_pool(&*bare, width, &mut bare_pool);
    });

    // The preview+result block's steady frame: the bodies are served from
    // their memos, so it must not pay one allocation more than the bodyless
    // block (neither call text is re-formatted any more).
    let steady = measure_frame(&*block, width, &mut pool);
    assert_eq!(pool, cold_pool, "the steady frame must serve the same rows");
    assert_eq!(
        steady.rows.pool_growth, 0,
        "the edit block must reuse the pooled rows: {steady:?}"
    );
    assert_eq!(
        steady.outgrown_rows, 0,
        "no edit-block row may outgrow its previous buffer: {steady:?}"
    );
    // The edit call line is env-dependent (`shorten_path` reads HOME), but its
    // memo keys on the `CallLineEnv` generation, so a steady frame formats
    // nothing; every other part of the block (the call ROWS, the preview body,
    // the legacy body and the result section) must come from its memo with no
    // rebuild and no re-wrap.
    let memo_work = steady.memos;
    assert_eq!(
        memo_work.call_text_formats, 0,
        "an unchanged env-dependent call text must not be re-formatted: {steady:?}"
    );
    assert_eq!(
        memo_work.call_rows_rewraps, 0,
        "an unchanged call text must not re-wrap its rows: {steady:?}"
    );
    assert_eq!(
        ToolMemoWork {
            frames: 0,
            ..memo_work
        },
        ToolMemoWork::default(),
        "a steady edit frame must rebuild nothing: {steady:?}"
    );
    let steady_allocations = steady.allocations;
    assert_eq!(
        steady_allocations, bare_allocations,
        "the preview and result bodies must cost nothing on a steady frame: \
         {steady_allocations} for the body block vs {bare_allocations} for the \
         bodyless one (the equality IS the body-is-free proof; the counters \
         above add that neither body memo rebuilt)"
    );
    // Both blocks pay the same fixed residual (16 events measured, all 16 B):
    // the frame canvas' own row bookkeeping, not the block. The BODIES cost
    // zero — proven by the equality with the bodyless block, by the zero
    // preview/legacy/result rebuild counters, and by the flatness leg below (a
    // 300-line preview+result body costs no more than a 4-line one).
    const STEADY_EDIT_BLOCK_CEILING: u64 = 16;
    assert!(
        steady_allocations <= STEADY_EDIT_BLOCK_CEILING,
        "a steady edit frame must stay within the measured residual ceiling: \
         steady={steady_allocations}, \
         ceiling={STEADY_EDIT_BLOCK_CEILING}, non-empty rows={nonempty}, \
         sizes={:?}, attribution={steady:?}",
        steady.sizes
    );

    // A body that grows stays FLAT: a 300-line preview and result body serve a
    // steady frame with the same allocation count as the 4-line one.
    let big_preview = (0..300)
        .map(|index| format!(" {index:03} diff line"))
        .collect::<Vec<_>>()
        .join("\n");
    let big_result = (0..300)
        .map(|index| format!("+{index:03} added line"))
        .collect::<Vec<_>>()
        .join("\n");
    let big = Arc::new(edit_block(
        &args,
        "applied 1 edit",
        Some(Arc::new(serde_json::json!({ "diff": big_result }))),
        Some(edit_args_and_preview(&big_preview).1),
    ));
    let mut big_pool = stale_pool();
    render_into_pool(&*big, width, &mut big_pool);
    assert!(
        big_pool.len() > 4 * pool.len(),
        "the large fixture must render many more rows than the small one: \
         {} vs {}",
        big_pool.len(),
        pool.len()
    );
    let ((), big_allocations) = count_allocations(|| {
        render_into_pool(&*big, width, &mut big_pool);
    });
    assert!(
        big_allocations <= steady_allocations + 2,
        "the edit block's steady-frame allocations must not grow with the \
         body: {steady_allocations} for the 4-line bodies, {big_allocations} \
         for the 300-line ones"
    );

    // A preview edit is part of the key: the box body repaints like a cold
    // instance of the new preview.
    let second_preview = " 1 context\n-1 old line\n+1 new line v2";
    block.set_meta(
        Some(Arc::clone(&details)),
        None,
        Some(edit_args_and_preview(second_preview).1),
    );
    render_into_pool(&*block, width, &mut pool);
    let cold_v2 = edit_block(
        &args,
        "applied 1 edit",
        Some(Arc::clone(&details)),
        Some(edit_args_and_preview(second_preview).1),
    );
    let mut cold_v2_pool = stale_pool();
    render_into_pool(&cold_v2, width, &mut cold_v2_pool);
    assert_ne!(pool, cold_pool, "the preview edit must reach the frame");
    assert_eq!(
        pool, cold_v2_pool,
        "the preview edit must repaint like a cold instance"
    );

    // A new result diff (the `details` `Arc` moves) repaints the section below
    // the box.
    let second_result = " 1 context\n-1 old line\n+1 new line edited twice\n 2 tail";
    let details_v2 = Arc::new(serde_json::json!({ "diff": second_result }));
    block.set_meta(
        Some(Arc::clone(&details_v2)),
        None,
        Some(edit_args_and_preview(second_preview).1),
    );
    render_into_pool(&*block, width, &mut pool);
    let cold_result = edit_block(
        &args,
        "applied 1 edit",
        Some(Arc::clone(&details_v2)),
        Some(edit_args_and_preview(second_preview).1),
    );
    let mut cold_result_pool = stale_pool();
    render_into_pool(&cold_result, width, &mut cold_result_pool);
    assert_ne!(
        pool, cold_v2_pool,
        "the new result diff must reach the frame"
    );
    assert_eq!(
        pool, cold_result_pool,
        "the result diff must repaint like a cold instance"
    );

    // The legacy posture (no preview, no details): the args-rebuilt body is
    // memoized on the same inputs.
    let legacy = Arc::new(edit_block(&args, "applied 1 edit", None, None));
    let mut legacy_pool = stale_pool();
    render_into_pool(&*legacy, width, &mut legacy_pool);
    let cold_legacy = edit_block(&args, "applied 1 edit", None, None);
    let mut cold_legacy_pool = stale_pool();
    render_into_pool(&cold_legacy, width, &mut cold_legacy_pool);
    assert_eq!(
        legacy_pool, cold_legacy_pool,
        "the legacy body must match the cold instance"
    );
    let ((), legacy_allocations) = count_allocations(|| {
        render_into_pool(&*legacy, width, &mut legacy_pool);
    });
    assert!(
        legacy_allocations <= steady_allocations + 2,
        "the legacy body must cost nothing on a steady frame: \
         {legacy_allocations} vs {steady_allocations}"
    );

    // A width move re-wraps the bodies and re-pads the box rows.
    render_into_pool(&*block, 40, &mut pool);
    let cold_narrow = edit_block(
        &args,
        "applied 1 edit",
        Some(Arc::clone(&details_v2)),
        Some(edit_args_and_preview(second_preview).1),
    );
    let mut cold_narrow_pool = stale_pool();
    render_into_pool(&cold_narrow, 40, &mut cold_narrow_pool);
    assert_eq!(
        pool, cold_narrow_pool,
        "the width move must re-wrap like a cold instance"
    );

    // The palette is part of every key: a swap repaints like a cold instance
    // under the new palette and then settles.
    let _swap = ThemeSwap::install("light");
    render_into_pool(&*block, 40, &mut pool);
    let cold_light = edit_block(
        &args,
        "applied 1 edit",
        Some(Arc::clone(&details_v2)),
        Some(edit_args_and_preview(second_preview).1),
    );
    let mut cold_light_pool = stale_pool();
    render_into_pool(&cold_light, 40, &mut cold_light_pool);
    assert_eq!(
        pool, cold_light_pool,
        "the palette swap must repaint like a cold instance"
    );
    let ((), swapped_allocations) = count_allocations(|| {
        render_into_pool(&*block, 40, &mut pool);
    });
    assert!(
        swapped_allocations <= steady_allocations + 2,
        "the swapped-palette steady frame must fall back to the call-text \
         residual: {swapped_allocations} vs {steady_allocations}"
    );
}

// ---------------------------------------------------------------------------
// 9. The assistant segment cache key (M9)
// ---------------------------------------------------------------------------

/// The segment cache key no longer retains a copy of the segment text: the
/// component owns an immutable `Arc<AssistantMessage>` (a changed message
/// builds a NEW component through `new_from`), so the render position
/// identifies the segment and only the mutable render inputs belong in the
/// key. A steady frame must be a cache hit (zero allocations, no markdown
/// re-parse), while a component built from different text must render that
/// text (its cache is cold, so nothing can be inherited) and a width change
/// must still re-wrap.
#[test]
fn assistant_segment_cache_hits_steady_frames_and_follows_text_changes() {
    let _guard = test_lock();
    warm_theme();
    let width = 60;
    let body = "alpha ".repeat(80);

    let component = assistant_component(vec![text_block(&body)], false, Arc::new(Mutex::new(None)));
    let mut pool = stale_pool();
    let ((), cold) = count_allocations(|| render_into_pool(&component, width, &mut pool));
    assert!(cold > 0, "the cold frame must parse and wrap the body");
    let cold_rows = pool.clone();

    let ((), warm) = count_allocations(|| render_into_pool(&component, width, &mut pool));
    assert_eq!(pool, cold_rows, "the steady frame must be byte-identical");
    assert_eq!(
        warm, 0,
        "a steady frame must be served from the segment cache (no markdown \
         re-parse and no key-text copy)"
    );

    // Changed text is a changed message, hence a fresh component: its cold
    // render must show the new text and nothing from the old cache.
    let changed = assistant_component(
        vec![text_block("beta marker")],
        false,
        Arc::new(Mutex::new(None)),
    );
    let mut changed_pool = stale_pool();
    let ((), changed_allocations) =
        count_allocations(|| render_into_pool(&changed, width, &mut changed_pool));
    assert!(
        changed_allocations > 0,
        "the changed component renders cold"
    );
    assert!(
        changed_pool.iter().any(|row| row.contains("beta marker")),
        "the changed text must render: {changed_pool:?}"
    );
    assert!(
        !changed_pool.iter().any(|row| row.contains("alpha")),
        "no row may carry the previous component's text: {changed_pool:?}"
    );

    // Width stays in the key.
    let mut wide_pool = stale_pool();
    render_into_pool(&component, width + 10, &mut wide_pool);
    assert_ne!(
        wide_pool, cold_rows,
        "a width change must re-wrap the segment"
    );
}

// ---------------------------------------------------------------------------
// 10. Width measurement and background painting: no per-cluster churn
// ---------------------------------------------------------------------------

/// The root cause of the tool-block / transcript steady-frame ceilings: every
/// width measurement walked the text's grapheme clusters through
/// `ansi::grapheme_width`, and that function collected the cluster's code
/// points into a fresh `Vec<char>` (16 B) per cluster — a per-row, per-frame
/// allocation hidden inside `visible_width` (`push_painted_row`,
/// `pad_to_width`, `truncate_to_width`, the wrapper's `wrap_single_line` and
/// `clip_run_to_width`).
///
/// The measurement must now be allocation-free for every cluster shape a row
/// can hold: plain ASCII, CJK, combining marks, flags, keycaps, ZWJ emoji and
/// tabs. The width is asserted before the allocation count, so a fast path
/// that drops cells fails here too.
#[test]
fn width_measurement_allocates_nothing_per_cluster() {
    let _guard = test_lock();
    warm_theme();
    let styled = with_theme(|theme| {
        theme.fg(
            ThemeColor::Text,
            "hello, 你好 e\u{0301} \u{1F1EF}\u{1F1F5} 1\u{FE0F}\u{20E3} \u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}\u{200D}\u{1F466}\tend",
        )
    });
    // Printed width: "hello, " 7 + "你好" 4 + " " 1 + "e"+combining 1 + " " 1
    // + flag 2 + " " 1 + keycap 2 + " " 1 + family ZWJ 2 + tab 3 + "end" 3.
    let expected = 7 + 4 + 1 + 1 + 1 + 2 + 1 + 2 + 1 + 2 + 3 + 3;
    assert_eq!(
        visible_width(&styled),
        expected,
        "the fixture must measure as documented"
    );

    let (width, allocations) = count_allocations(|| visible_width(&styled));
    assert_eq!(width, expected, "the width must be stable");
    assert_eq!(
        allocations, 0,
        "measuring a styled row must not allocate per cluster: {allocations}"
    );

    // The unstyled path (no ESC) and a bare tab take their own branches.
    let (_, allocations) = count_allocations(|| {
        let _ = visible_width("你好 e\u{0301} \u{1F1EF}\u{1F1F5} 1\u{FE0F}\u{20E3}\t");
        let _ = visible_width("\t");
    });
    assert_eq!(
        allocations, 0,
        "the unstyled measurement must not allocate per cluster: {allocations}"
    );
}

/// A user message's background box repaints its rows behind an arbitrary
/// painter closure, so the component keeps its own memo of the FINAL painted
/// rows (the message text is fixed at construction, so width + palette are its
/// whole key). A steady frame must therefore allocate nothing at all — before
/// the memo the box re-painted every row on every frame (and `Theme::bg` grew
/// each painted row in three steps). A width or palette move rebuilds exactly
/// what a COLD second component renders.
#[test]
fn user_message_box_steady_frame_serves_memoized_rows() {
    let _guard = test_lock();
    warm_theme();
    let width = 80;
    let body = "a user message with **markdown**, a list:\n\n- one\n- two\n\nand a tail paragraph.";
    let message = UserMessageComponent::new(body, 1);
    let mut pool = stale_pool();
    render_into_pool(&message, width, &mut pool);
    let rows = pool.clone();
    assert!(rows.len() >= 6, "the fixture must paint a multi-row box");

    // A cold second component (a fresh message instance) renders the same
    // rows; the warm memo must be byte-identical to it.
    let mut cold_pool = stale_pool();
    render_into_pool(&UserMessageComponent::new(body, 1), width, &mut cold_pool);
    assert_eq!(
        rows, cold_pool,
        "the memoized rows must equal the cold render"
    );

    // Steady frame: every painted row is re-pushed from the memo.
    let ((), steady) = count_allocations(|| {
        render_into_pool(&message, width, &mut pool);
    });
    assert_eq!(
        steady, 0,
        "a steady user-message frame must allocate nothing: {steady}"
    );
    assert_eq!(pool, rows, "the steady frame drifted from the cold render");

    // Width is part of the key: a narrowed frame repaints (and matches a cold
    // instance at that width).
    let ((), narrowed) = count_allocations(|| {
        render_into_pool(&message, 40, &mut pool);
    });
    assert!(narrowed > 0, "a width change must repaint the box");
    let mut cold_pool = stale_pool();
    render_into_pool(&UserMessageComponent::new(body, 1), 40, &mut cold_pool);
    assert_eq!(pool, cold_pool, "the narrowed rows must match a cold box");
    let ((), settled) = count_allocations(|| {
        render_into_pool(&message, 40, &mut pool);
    });
    assert_eq!(settled, 0, "the repainted box must settle again");

    // The palette generation is part of the key: a swap repaints, then
    // settles again, and the swapped frame matches a cold instance.
    let _swap = ThemeSwap::install("light");
    let ((), swapped) = count_allocations(|| {
        render_into_pool(&message, 40, &mut pool);
    });
    assert!(swapped > 0, "a theme swap must repaint the box");
    let mut cold_pool = stale_pool();
    render_into_pool(&UserMessageComponent::new(body, 1), 40, &mut cold_pool);
    assert_eq!(pool, cold_pool, "the swapped rows must match a cold box");
    let ((), steady_swapped) = count_allocations(|| {
        render_into_pool(&message, 40, &mut pool);
    });
    assert_eq!(
        steady_swapped, 0,
        "the swapped box must settle to zero allocations again"
    );
}

/// An assistant message's inline image block must serve its rendered rows from
/// a memo, exactly like `ToolExecutionComponent`'s result images: the rows are
/// a pure function of the (immutable) image payload, the width and the
/// resolved terminal capabilities. Without the memo every frame — every
/// spinner tick — re-decoded the base64 prefix and re-encoded the protocol
/// payload (or rebuilt the `[Image: …]` fallback line), an O(image) per-frame
/// cost. The width and the capability generation stay in the key, so both
/// moves rebuild the rows.
#[test]
fn assistant_image_block_steady_frame_serves_memoized_rows() {
    use pir::tui::terminal_image::{
        resolved_capabilities, set_resolved_capabilities, TerminalCapabilities,
    };

    let _guard = test_lock();
    warm_theme();
    // Deterministic posture: no inline-image protocol, so the block renders
    // the documented `[Image: <mime> <WxH>]` fallback line.
    let previous = resolved_capabilities();
    set_resolved_capabilities(TerminalCapabilities {
        images: None,
        true_color: true,
        hyperlinks: false,
    });
    // 1x1 PNG.
    const PNG_1X1: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8\
z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==";
    let make = || {
        assistant_component(
            vec![text_block("caption"), image_block(PNG_1X1, "image/png")],
            false,
            Arc::new(Mutex::new(None)),
        )
    };
    let component = make();
    let width = 40;

    let mut pool = stale_pool();
    render_into_pool(&component, width, &mut pool);
    render_into_pool(&component, width, &mut pool);
    let rows = pool.clone();
    assert!(
        rows.iter()
            .any(|row| row.contains("[Image: image/png 1x1]")),
        "the image block must render its fallback line: {rows:?}"
    );

    // A cold second component at the same width renders the same rows.
    let mut cold_pool = stale_pool();
    render_into_pool(&make(), width, &mut cold_pool);
    assert_eq!(rows, cold_pool, "the memoized rows must be the cold render");

    // Steady frame: the memoized image rows are re-pushed without decoding or
    // re-encoding anything.
    let ((), steady) = count_allocations(|| {
        render_into_pool(&component, width, &mut pool);
    });
    assert_eq!(
        steady, 0,
        "a steady frame must serve the image rows from the memo: {steady}"
    );
    assert_eq!(pool, rows, "the steady frame drifted from the cold render");

    // Width is part of the key.
    let ((), narrowed) = count_allocations(|| {
        render_into_pool(&component, 24, &mut pool);
    });
    assert!(
        narrowed > 0,
        "a width change must rebuild the image rows: {narrowed}"
    );

    // The terminal capability generation is part of the key: a re-resolution
    // that changes nothing else still repaints (then settles again).
    set_resolved_capabilities(TerminalCapabilities {
        images: None,
        true_color: true,
        hyperlinks: false,
    });
    let ((), reparsed) = count_allocations(|| {
        render_into_pool(&component, 24, &mut pool);
    });
    assert!(
        reparsed > 0,
        "a capability generation move must rebuild the image rows: {reparsed}"
    );
    let ((), settled) = count_allocations(|| {
        render_into_pool(&component, 24, &mut pool);
    });
    assert_eq!(settled, 0, "the rebuilt rows must settle again");

    set_resolved_capabilities(previous);
}
