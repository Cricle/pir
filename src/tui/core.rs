//! Core TUI: components + differential main-screen renderer.
//!
//! Port of `pi-tui`'s rendering model: components produce ANSI line
//! buffers; [`TuiCore`] compares them against the previous frame and
//! repaints only the changed rows (synchronized output brackets each
//! update). First render streams everything without clearing; width or
//! height changes trigger a full repaint; appended lines write
//! incrementally — exactly the upstream `TuiMainScreen.doRender` rules.

// Pre-existing baseline lints (tui is frozen for the parity batch).
#![allow(clippy::type_complexity)]
#![allow(clippy::unnecessary_min_or_max)]
#![allow(clippy::needless_range_loop)]
#![allow(clippy::repeat_once)]

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use super::ansi;
use super::program_status::{
    format_program_status, is_program_status_reply, ProgramStatus, ProgramStatusNegotiation,
    ProgramStatusReporter, ProgramStatusState,
};

// ---------------------------------------------------------------------------
// Frame-work counters (a measurement seam, `parser.copy_count()` precedent)
// ---------------------------------------------------------------------------

/// How much work one frame's full-frame passes did, counted at the four
/// per-row passes of the follow-end render path:
///
/// - `rebuilt_rows`: rows the content components wrote in
///   [`TuiCore::frame_lines`] — the WHOLE transcript on the main-screen
///   (non-windowed) path, the materialized window band on the windowed
///   viewport (P0.3), never the visible window alone;
/// - `snapshot_rows` / `snapshot_bytes`: rows/bytes actually copied when the
///   fresh raw frame hands its row buffers over to the alt-screen search
///   snapshot. The displaced snapshot buffer becomes the frame's work buffer
///   and already holds the previous raw rows, so only rows whose content
///   changed are copied — a steady frame copies zero rows and bytes;
/// - `diff_scanned_rows`: rows the changed-line scan compared against the
///   previous frame;
/// - `reset_rows`: rows [`TuiCore`] appended the segment reset to — i.e. the
///   rows the frame actually WROTE (the reset is lazy: an unwritten row keeps
///   the baseline bytes it was written with, reset included).
///
/// Each pass walks O(frame height) rows (`snapshot_rows`/`snapshot_bytes`
/// only the changed ones); the counters exist so tests can pin that baseline
/// (and, later, a windowed/dirty-row target) without timing — allocation
/// counts cannot see a `clone_from`/compare/`push_str` pass.
///
/// Cost: one `Relaxed` `fetch_add` per pass per frame (the per-row values are
/// accumulated in a local and published once), no allocation, no lock. In
/// release this is a handful of instructions on a path that already touches
/// every row; [`FrameWork::snapshot`] is the only reader.
#[doc(hidden)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FrameWork {
    pub rebuilt_rows: u64,
    pub snapshot_rows: u64,
    pub snapshot_bytes: u64,
    pub diff_scanned_rows: u64,
    pub reset_rows: u64,
}

impl FrameWork {
    /// The process-wide counters as of now (tests / probes).
    #[doc(hidden)]
    pub fn snapshot() -> Self {
        Self {
            rebuilt_rows: FRAME_WORK.rebuilt_rows.load(Ordering::Relaxed),
            snapshot_rows: FRAME_WORK.snapshot_rows.load(Ordering::Relaxed),
            snapshot_bytes: FRAME_WORK.snapshot_bytes.load(Ordering::Relaxed),
            diff_scanned_rows: FRAME_WORK.diff_scanned_rows.load(Ordering::Relaxed),
            reset_rows: FRAME_WORK.reset_rows.load(Ordering::Relaxed),
        }
    }

    /// Zero every counter (tests read a per-frame delta around a render).
    #[doc(hidden)]
    pub fn reset() {
        FRAME_WORK.rebuilt_rows.store(0, Ordering::Relaxed);
        FRAME_WORK.snapshot_rows.store(0, Ordering::Relaxed);
        FRAME_WORK.snapshot_bytes.store(0, Ordering::Relaxed);
        FRAME_WORK.diff_scanned_rows.store(0, Ordering::Relaxed);
        FRAME_WORK.reset_rows.store(0, Ordering::Relaxed);
    }
}

/// The atomics behind [`FrameWork`] (relaxed: counters, not synchronization).
struct FrameWorkCounters {
    rebuilt_rows: AtomicU64,
    snapshot_rows: AtomicU64,
    snapshot_bytes: AtomicU64,
    diff_scanned_rows: AtomicU64,
    reset_rows: AtomicU64,
}

static FRAME_WORK: FrameWorkCounters = FrameWorkCounters {
    rebuilt_rows: AtomicU64::new(0),
    snapshot_rows: AtomicU64::new(0),
    snapshot_bytes: AtomicU64::new(0),
    diff_scanned_rows: AtomicU64::new(0),
    reset_rows: AtomicU64::new(0),
};

/// Canvas work counters behind [`Rows`] — the second half of the
/// [`FrameWork`] measurement seam (alongside
/// [`ToolMemoWork`](crate::tui::messages::ToolMemoWork) for the tool block's
/// memos): relaxed adds per row claimed, per row the row POOL had to grow for,
/// and per whole-row copy/reuse. A steady frame must have `pool_growth == 0`
/// and `row_write_bytes == 0`: the pool is already sized and every row still
/// holds its previous bytes, so the claim path only compares and reuses.
/// Any allocation a frame reports while its memos did not rebuild is
/// attributable here.
#[doc(hidden)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RowsWork {
    /// Rows claimed through [`Rows`] (the frame height; a claimed row may be
    /// served by the exact-content reuse path instead of a write).
    pub row_writes: u64,
    /// Bytes actually copied into pooled row buffers by the whole-row writes
    /// ([`Rows::push`] / [`Rows::push_fmt`]) since the last reset. This is the
    /// transcript memcpy the steady frame must make disappear: before the
    /// copy-on-change fast path, every cached body line was copied out of its
    /// cache slot on every frame.
    ///
    /// Construction through [`Rows::line_mut`] / [`Rows::line_at`] is not
    /// counted here — those rows are built in place by the component itself
    /// (the editor/dock path), not copied from a cache.
    pub row_write_bytes: u64,
    /// Bytes of whole-row writes served by the pooled row that already held
    /// exactly those bytes (no clear, no copy). A steady frame's cached lines
    /// land here; it is the avoided half of `row_write_bytes`.
    pub row_reuse_bytes: u64,
    /// Rows served by the exact-content reuse path (subset of `row_writes`).
    pub rows_reused: u64,
    /// Rows newly appended to the pool (`String::new()` + a `Vec` push) —
    /// zero on a frame whose row count did not grow past the pool.
    pub pool_growth: u64,
}

impl RowsWork {
    /// The process-wide counters as of now (tests / probes).
    #[doc(hidden)]
    pub fn snapshot() -> Self {
        Self {
            row_writes: ROWS_WORK.row_writes.load(Ordering::Relaxed),
            row_write_bytes: ROWS_WORK.row_write_bytes.load(Ordering::Relaxed),
            row_reuse_bytes: ROWS_WORK.row_reuse_bytes.load(Ordering::Relaxed),
            rows_reused: ROWS_WORK.rows_reused.load(Ordering::Relaxed),
            pool_growth: ROWS_WORK.pool_growth.load(Ordering::Relaxed),
        }
    }

    /// Zero every counter.
    #[doc(hidden)]
    pub fn reset() {
        ROWS_WORK.row_writes.store(0, Ordering::Relaxed);
        ROWS_WORK.row_write_bytes.store(0, Ordering::Relaxed);
        ROWS_WORK.row_reuse_bytes.store(0, Ordering::Relaxed);
        ROWS_WORK.rows_reused.store(0, Ordering::Relaxed);
        ROWS_WORK.pool_growth.store(0, Ordering::Relaxed);
    }
}

/// The atomics behind [`RowsWork`].
struct RowsWorkCounters {
    row_writes: AtomicU64,
    row_write_bytes: AtomicU64,
    row_reuse_bytes: AtomicU64,
    rows_reused: AtomicU64,
    pool_growth: AtomicU64,
}

static ROWS_WORK: RowsWorkCounters = RowsWorkCounters {
    row_writes: AtomicU64::new(0),
    row_write_bytes: AtomicU64::new(0),
    row_reuse_bytes: AtomicU64::new(0),
    rows_reused: AtomicU64::new(0),
    pool_growth: AtomicU64::new(0),
};

/// Reusable row canvas for [`Component::render_into`].
///
/// The renderer owns one pool of row buffers per frame slot and rewinds it
/// every frame: a row whose content did not change is left untouched, and a
/// changed row is rewritten in place (`clear` + write) instead of being
/// allocated, so a component that migrated off the `Vec<String>` `render`
/// path allocates nothing per frame and a steady frame copies no transcript
/// bytes back into the pool. Only the `next` rows appended since
/// [`Rows::rewind`] belong to the frame — any tail a longer earlier frame left
/// in the pool is [`Rows::trim`]med off, and no accessor reaches past `next`,
/// so stale pooled content can never reach the diff.
pub struct Rows<'a> {
    lines: &'a mut Vec<String>,
    next: usize,
    /// This canvas's own work counters (the process-wide [`RowsWork`] also
    /// aggregates every canvas; a frame test reads this instance's copy so it
    /// is unaffected by other tests/frames running in parallel).
    work: RowsWork,
}

impl<'a> Rows<'a> {
    /// Wrap a row pool. The pool starts rewound (the frame is empty); its
    /// existing rows are only ever reused by index.
    pub fn new(lines: &'a mut Vec<String>) -> Self {
        Self {
            lines,
            next: 0,
            work: RowsWork::default(),
        }
    }

    /// This canvas's counters as of now (rows claimed, bytes copied, bytes
    /// served by exact-content reuse).
    pub fn work(&self) -> RowsWork {
        self.work
    }

    /// Start a new frame: the pool keeps every row buffer it holds.
    pub fn rewind(&mut self) {
        self.next = 0;
    }

    /// Rows appended since the last [`Rows::rewind`] (the frame height).
    pub fn len(&self) -> usize {
        self.next
    }

    /// Whether the frame has no rows yet.
    pub fn is_empty(&self) -> bool {
        self.next == 0
    }

    /// Drop the pool's rows past the frame end (a shorter frame than the one
    /// the pool was recycled from must not leave unreachable rows behind).
    pub fn trim(&mut self) {
        self.lines.truncate(self.next);
    }

    /// Append one row, copying `s` into the pooled slot unless the slot
    /// already holds exactly `s`.
    ///
    /// The exact-content check is the steady-frame fast path: the frame pool
    /// is recycled across frames (and the component caches hand back the same
    /// line every frame), so a row that did not change keeps its pooled bytes
    /// instead of being cleared and copied again. Byte-for-byte the row is
    /// still exactly `s` — the check only skips a redundant write. A changed
    /// or first-time row is cleared and copied as before.
    pub fn push(&mut self, s: &str) {
        let index = self.next;
        if index == self.lines.len() {
            ROWS_WORK.pool_growth.fetch_add(1, Ordering::Relaxed);
            self.work.pool_growth += 1;
            self.lines.push(String::new());
        }
        self.next = index + 1;
        ROWS_WORK.row_writes.fetch_add(1, Ordering::Relaxed);
        self.work.row_writes += 1;
        let row = &mut self.lines[index];
        if row == s {
            ROWS_WORK.rows_reused.fetch_add(1, Ordering::Relaxed);
            ROWS_WORK
                .row_reuse_bytes
                .fetch_add(s.len() as u64, Ordering::Relaxed);
            self.work.rows_reused += 1;
            self.work.row_reuse_bytes += s.len() as u64;
            return;
        }
        row.clear();
        row.push_str(s);
        self.work.row_write_bytes += s.len() as u64;
        ROWS_WORK
            .row_write_bytes
            .fetch_add(s.len() as u64, Ordering::Relaxed);
    }

    /// [`Rows::push`] without the throwaway formatted `String`. The formatted
    /// row is always built from scratch, so the pooled slot is cleared first.
    pub fn push_fmt(&mut self, args: std::fmt::Arguments<'_>) {
        let row = self.claim();
        row.clear();
        let _ = std::fmt::Write::write_fmt(row, args);
        let written = row.len() as u64;
        self.work.row_write_bytes += written;
        ROWS_WORK
            .row_write_bytes
            .fetch_add(written, Ordering::Relaxed);
    }

    /// Mutable access to the row the canvas cursor sits on — the row most
    /// recently appended by [`Rows::push`] / [`Rows::push_fmt`] — for
    /// padding/painting it in place. Appends an empty row first when the
    /// frame has none yet (the recycled slot is cleared), so a component can
    /// claim a fresh row with `out.push("")` and then build it here from
    /// scratch.
    pub fn line_mut(&mut self) -> &mut String {
        if self.next == 0 {
            let row = self.claim();
            row.clear();
            return row;
        }
        &mut self.lines[self.next - 1]
    }

    /// Mutable access to a row already written in the current frame
    /// (containers paint a child's rows in place once the child rendered).
    ///
    /// # Panics
    ///
    /// Panics when `index` names a row outside the current frame — reaching
    /// a pooled row from an earlier frame would surface stale content.
    pub fn line_at(&mut self, index: usize) -> &mut String {
        assert!(
            index < self.next,
            "row {index} is not part of the current frame"
        );
        &mut self.lines[index]
    }

    /// Claim the next pooled row: grow the pool when needed and bump the
    /// counters, but leave the row's bytes untouched — [`Rows::push`] reuses
    /// them when they already match, and the from-scratch builders
    /// ([`Rows::push_fmt`], [`Rows::line_mut`]) clear explicitly.
    fn claim(&mut self) -> &mut String {
        let index = self.next;
        if index == self.lines.len() {
            ROWS_WORK.pool_growth.fetch_add(1, Ordering::Relaxed);
            self.work.pool_growth += 1;
            self.lines.push(String::new());
        }
        self.next = index + 1;
        ROWS_WORK.row_writes.fetch_add(1, Ordering::Relaxed);
        self.work.row_writes += 1;
        &mut self.lines[index]
    }
}

/// A component renders itself to ANSI lines for the given width.
/// Invalidate clears cached state; the trait matches upstream
/// `Component { render, invalidate }` (input handling stays in the event
/// loop for the pir port).
pub trait Component: Send {
    fn render(&self, width: usize) -> Vec<String>;

    /// Render into the recycled row canvas. The default bridge copies the
    /// [`Component::render`] lines into `out`, so a component that has not
    /// migrated keeps byte-identical output; migrating it means writing the
    /// rows here directly (no per-frame `Vec`/`String` of its own).
    fn render_into(&self, width: usize, out: &mut Rows<'_>) {
        for line in self.render(width) {
            out.push(&line);
        }
    }

    fn invalidate(&self) {}
}

/// Append one blank row of `width` spaces — the `" ".repeat(width)` row the
/// pre-migration components allocated per frame.
fn push_blank(out: &mut Rows<'_>, width: usize) {
    out.push("");
    let row = out.line_mut();
    row.reserve(width);
    for _ in 0..width {
        row.push(' ');
    }
}

/// `ansi::pad_to_width` / `ansi::apply_background_to_line` in place: pad
/// `row` to `width` visible cells and, when a painter is installed, replace
/// the row with the painted copy. Byte-identical to the allocating helpers
/// (which the `render` methods keep using); only the row buffer is reused.
fn paint_row(row: &mut String, width: usize, bg: Option<&(dyn Fn(&str) -> String + Send + Sync)>) {
    let pad = width.saturating_sub(ansi::visible_width(row.as_str()));
    row.reserve(pad);
    for _ in 0..pad {
        row.push(' ');
    }
    if let Some(bg) = bg {
        let painted = bg(row.as_str());
        row.clear();
        row.push_str(&painted);
    }
}

// ---------------------------------------------------------------------------
// W11-C: main-screen mouse regions
// ---------------------------------------------------------------------------

/// One addressable rectangle of the main (regular-mode) screen, in 0-based
/// screen cells. The main screen has no layout tree of hit-tested components
/// (the alt-screen family owns that), so the event loop publishes what the last
/// frame painted — the editor block, an open selector, a dialog — as these
/// regions and the click router resolves a press by the smallest region that
/// contains it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseRegion {
    /// The text editor's border-framed block: `row` is the top border,
    /// `row + 1` the first CONTENT row.
    Editor {
        row: usize,
        height: usize,
        col: usize,
        width: usize,
    },
    /// An open selector/dialog overlay painted into the editor slot: a click
    /// selects the row at `row + y`.
    Selector { row: usize, height: usize },
    /// The autocomplete popup above the editor: a click selects an item.
    Autocomplete { row: usize, height: usize },
}

impl MouseRegion {
    /// Whether `(row, col)` falls inside this region. `Selector` /
    /// `Autocomplete` regions span the full width (their rows are what the
    /// click targets); the editor region is column-bounded.
    pub fn contains(&self, row: usize, col: usize) -> bool {
        match self {
            MouseRegion::Editor {
                row: top,
                height,
                col: left,
                width,
            } => row >= *top && row < top + height && col >= *left && col < left + width,
            MouseRegion::Selector { row: top, height }
            | MouseRegion::Autocomplete { row: top, height } => row >= *top && row < top + height,
        }
    }
}

/// The painted dock's row geometry for the mouse router (W11-C): where the
/// editor block and any overlay in its slot landed in the LAST frame, in
/// absolute FRAME rows (the event loop maps a screen cell through
/// [`TuiCore::screen_row_to_buffer_row`] first). `None` when the frame has not
/// painted them yet (the first frame, or an overlay layout the router does
/// not hit-test).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DockMouseLayout {
    /// Frame row of the editor block's top border (the region spans
    /// `editor_height` rows below it).
    pub editor_row: usize,
    /// Rows the editor block painted (top border + content + bottom border).
    pub editor_height: usize,
    /// Frame row of the first row painted INTO the editor slot when an
    /// overlay replaced the editor (`None` while the editor is shown).
    pub overlay_row: Option<usize>,
    /// Rows that overlay painted.
    pub overlay_height: usize,
    /// Frame row of the autocomplete popup's first item row, when one is
    /// painted below the editor. The popup paints BELOW the editor block
    /// (upstream's editor component appends it after the bottom border), so
    /// its rows never overlap the editor region.
    pub autocomplete_row: Option<usize>,
    /// Item rows the popup painted (the trailing `(n/m)` scroll counter is
    /// not an item and is excluded).
    pub autocomplete_height: usize,
}

impl DockMouseLayout {
    /// The regions of the last frame, in hit-test priority order (an open
    /// overlay replaces the editor block entirely; the autocomplete popup
    /// paints below the editor).
    pub fn regions(&self) -> Vec<MouseRegion> {
        let mut regions = Vec::new();
        if let Some(row) = self.overlay_row {
            if self.overlay_height > 0 {
                regions.push(MouseRegion::Selector {
                    row,
                    height: self.overlay_height,
                });
            }
            return regions;
        }
        if self.editor_height > 0 {
            regions.push(MouseRegion::Editor {
                row: self.editor_row,
                height: self.editor_height,
                col: 0,
                width: usize::MAX,
            });
        }
        if let Some(row) = self.autocomplete_row {
            if self.autocomplete_height > 0 {
                regions.push(MouseRegion::Autocomplete {
                    row,
                    height: self.autocomplete_height,
                });
            }
        }
        regions
    }

    /// Resolve a click at absolute screen `(row, col)` to the region it lands
    /// in (first match wins, matching [`DockMouseLayout::regions`] order).
    pub fn region_at(&self, row: usize, col: usize) -> Option<MouseRegion> {
        self.regions()
            .into_iter()
            .find(|region| region.contains(row, col))
    }

    /// The editor CONTENT row (0-based, the top border excluded) `row` maps
    /// to, or `None` when the click is on a border row / outside.
    pub fn editor_content_row(&self, row: usize) -> Option<usize> {
        if self.overlay_row.is_some() || self.editor_height < 3 {
            return None;
        }
        // Rows: top border at `editor_row`, content in between, bottom border
        // last.
        if row <= self.editor_row || row + 1 >= self.editor_row + self.editor_height {
            return None;
        }
        Some(row - self.editor_row - 1)
    }

    /// The overlay list row (0-based from the overlay's first painted row)
    /// `row` maps to; `None` when no overlay is painted or the click is
    /// outside it.
    pub fn overlay_row_at(&self, row: usize) -> Option<usize> {
        let overlay_row = self.overlay_row?;
        (row >= overlay_row && row < overlay_row + self.overlay_height).then_some(row - overlay_row)
    }
}

/// A vertical container: children render top-to-bottom
/// (upstream `Container`).
#[derive(Default)]
pub struct Container {
    children: Vec<Box<dyn Component>>,
}

impl Container {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add_child(&mut self, component: impl Component + 'static) {
        self.children.push(Box::new(component));
    }

    pub fn remove_child(&mut self, index: usize) {
        if index < self.children.len() {
            self.children.remove(index);
        }
    }

    /// Number of children currently held (the transcript diff indexes into
    /// this; see `ChatContainer::set_entries`).
    pub fn child_count(&self) -> usize {
        self.children.len()
    }

    /// Insert one child at `index` (clamped to the end). The transcript diff
    /// rebuilds a middle band while keeping the children after it, so the
    /// new block has to land *before* the retained tail instead of appending.
    pub fn insert_child(&mut self, index: usize, component: impl Component + 'static) {
        let index = index.min(self.children.len());
        self.children.insert(index, Box::new(component));
    }

    /// Replace `remove` children starting at `index` with `insert`, in one
    /// splice (the transcript diff's only structural operation: the changed
    /// entries' children are rebuilt, every retained child keeps its
    /// position and its render caches).
    pub fn splice_children(
        &mut self,
        index: usize,
        remove: usize,
        insert: Vec<Box<dyn Component>>,
    ) {
        let index = index.min(self.children.len());
        let end = (index + remove).min(self.children.len());
        self.children.splice(index..end, insert);
    }

    pub fn clear(&mut self) {
        self.children.clear();
    }

    /// Release the child-vector allocation after the children were dropped.
    ///
    /// Invariant: the vector is only a list of handles; the memory that
    /// matters is behind the children, which `remove_child`/`clear` already
    /// drop. `shrink_children` exists for the transcript-reset path
    /// (`/clear`, `/new`, `/resume`, fork), where the container held one child
    /// per transcript entry for a whole session — `clear` kept the slot array
    /// sized for the outgoing session. Only call it on an empty container:
    /// clearing the slots of live children would not free them anyway.
    pub fn shrink_children(&mut self) {
        self.children.shrink_to_fit();
    }

    /// Capacity of the child-vector slots (0 on a fresh container). Test seam
    /// for the transcript-reset release above; the slot array is what the
    /// release can measure without touching the boxed components.
    #[doc(hidden)]
    pub fn children_capacity(&self) -> usize {
        self.children.capacity()
    }
}

impl Component for Container {
    fn render(&self, width: usize) -> Vec<String> {
        let mut lines = Vec::new();
        for child in &self.children {
            lines.extend(child.render(width));
        }
        lines
    }

    /// Direct-write path: the children append their rows straight into the
    /// canvas, in the same order the `render` path concatenates them. Row for
    /// row byte-identical to [`Component::render`]: a non-migrated child
    /// bridges its `render` lines through the default `render_into`, and a
    /// migrated child (e.g. [`Text`], [`BoxContainer`]) already produces the
    /// same rows.
    fn render_into(&self, width: usize, out: &mut Rows<'_>) {
        for child in &self.children {
            child.render_into(width, out);
        }
    }

    fn invalidate(&self) {
        for child in &self.children {
            child.invalidate();
        }
    }
}

/// Background painter shared by `Text` (upstream style fn).
type BgPainter = Arc<dyn Fn(&str) -> String + Send + Sync>;

/// The transcript side of upstream `ScrollView`'s width handling: with
/// `scrollbar: "always"` the view renders its child at `width - 1` and pads
/// every line with one trailing space, reserving the last column for the
/// scrollbar (`ScrollView.getContentWidth` + `ScrollView.render`); every other
/// mode renders the child at the full width. At `width == 1` upstream keeps
/// the full width (`getContentWidth`'s `width > 1` guard) and pads nothing.
///
/// The fullscreen viewport wraps its transcript content in this component
/// (`terminal.rs`, mirroring upstream `createChatViewport`, which wraps the
/// document container); the dock stays a sibling and keeps the full width,
/// exactly like the upstream `VStack` above the transcript.
pub struct ReservedScrollbarColumn {
    child: Box<dyn Component>,
    reserve: bool,
    /// Runtime-mutable reserve (the interactive loop's `switchTuiMode`): when
    /// present it overrides `reserve`, so a mode switch takes effect on the
    /// next frame without rebuilding the component tree.
    live_reserve: Option<Arc<std::sync::atomic::AtomicBool>>,
}

impl ReservedScrollbarColumn {
    pub fn new(child: impl Component + 'static, reserve: bool) -> Self {
        Self {
            child: Box::new(child),
            reserve,
            live_reserve: None,
        }
    }

    /// [`ReservedScrollbarColumn::new`] with a live reserve flag
    /// (`fullscreen && fullscreenScrollbar == always`), toggled by a runtime
    /// TUI-mode switch.
    pub fn with_live_flag(
        child: impl Component + 'static,
        reserve: bool,
        live: Arc<std::sync::atomic::AtomicBool>,
    ) -> Self {
        Self {
            child: Box::new(child),
            reserve,
            live_reserve: Some(live),
        }
    }

    fn reserves(&self) -> bool {
        match &self.live_reserve {
            Some(flag) => flag.load(Ordering::SeqCst),
            None => self.reserve,
        }
    }
}

impl Component for ReservedScrollbarColumn {
    fn render(&self, width: usize) -> Vec<String> {
        let content_width = if self.reserves() && width > 1 {
            width - 1
        } else {
            width
        };
        let lines = self.child.render(content_width);
        if content_width == width {
            return lines;
        }
        lines.into_iter().map(|line| format!("{line} ")).collect()
    }

    /// Direct-write path: the child renders straight into the canvas at the
    /// reserved content width and every row it wrote gains the one trailing
    /// space the `render` path appends (`format!("{line} ")`) — appended in
    /// place, so the wrapper allocates no line buffer of its own. Row for row
    /// byte-identical to [`Component::render`]: the same `content_width`
    /// guard, and a non-reserving wrapper (`content_width == width`) leaves
    /// the child's rows untouched exactly like the early return above.
    fn render_into(&self, width: usize, out: &mut Rows<'_>) {
        let content_width = if self.reserves() && width > 1 {
            width - 1
        } else {
            width
        };
        let start = out.len();
        self.child.render_into(content_width, out);
        if content_width == width {
            return;
        }
        for index in start..out.len() {
            out.line_at(index).push(' ');
        }
    }

    fn invalidate(&self) {
        self.child.invalidate();
    }
}

/// The unpainted [`Text`] rows memo ([`Text::render_into`]).
///
/// The rows are a pure function of exactly four inputs — the text CONTENT, the
/// frame `width` and the two padding fields — so a steady frame re-pushes the
/// stored rows instead of re-wrapping the text (one `Vec` plus one `String`
/// per visual line, per frame; the transcript's `Notice`/`Raw` entries pay
/// this on every frame of the whole session). The text is keyed by CONTENT:
/// the stored buffer's address could be recycled for a later, different text,
/// while a content compare cannot alias. The key holds the text (one copy) so
/// the compare never touches the component's lock twice.
///
/// Only populated while the component has no background painter: a `bg`
/// closure is an arbitrary, extension-supplied function whose result cannot be
/// keyed, so a painted `Text` keeps re-rendering every frame (the pre-memo
/// behavior) and never reads this slot. A `bg` installed after an unpainted
/// frame therefore cannot serve stale unpainted rows; a later `set_bg(None)`
/// may serve them again, which is byte-exact (the rows never depended on the
/// painter).
struct TextRowsCache {
    text: String,
    width: usize,
    padding_x: usize,
    padding_y: usize,
    rows: Vec<String>,
}

/// Static text with padding and an optional background painter
/// (upstream `Text`).
pub struct Text {
    text: Mutex<String>,
    padding_x: usize,
    padding_y: usize,
    bg: Mutex<Option<BgPainter>>,
    /// Memoized unpainted rows (see [`TextRowsCache`]).
    rows: Mutex<Option<TextRowsCache>>,
}

impl Text {
    pub fn new(text: impl Into<String>) -> Self {
        Self::with_padding(text, 1, 0)
    }

    pub fn with_padding(text: impl Into<String>, padding_x: usize, padding_y: usize) -> Self {
        Self {
            text: Mutex::new(text.into()),
            padding_x,
            padding_y,
            bg: Mutex::new(None),
            rows: Mutex::new(None),
        }
    }

    pub fn set_text(&self, text: impl Into<String>) {
        *self
            .text
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = text.into();
    }

    pub fn get_text(&self) -> String {
        self.text
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    pub fn set_bg(&self, bg: Option<BgPainter>) {
        *self
            .bg
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = bg;
    }

    fn bg_fn(&self) -> Option<BgPainter> {
        self.bg
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

/// The uncached row builder behind [`TextRowsCache`]: the same rows the
/// painted path writes into the canvas, materialized as owned lines for the
/// memo slot.
fn text_rows_uncached(text: &str, width: usize, padding_x: usize, padding_y: usize) -> Vec<String> {
    let padding_x = padding_x.min(width.saturating_sub(1) / 2);
    let content_width = width.saturating_sub(padding_x * 2).max(1);
    let normalized: std::borrow::Cow<'_, str> = if text.contains('\t') {
        std::borrow::Cow::Owned(text.replace('\t', "   "))
    } else {
        std::borrow::Cow::Borrowed(text)
    };
    let wrapped = ansi::wrap_text_with_ansi(&normalized, content_width);
    let left = " ".repeat(padding_x);
    let right = " ".repeat(padding_x);
    let mut lines = Vec::with_capacity(padding_y * 2 + wrapped.len());
    for _ in 0..padding_y {
        lines.push(" ".repeat(width));
    }
    for wrapped_line in wrapped {
        let mut row = String::with_capacity(width);
        row.push_str(&left);
        row.push_str(&wrapped_line);
        row.push_str(&right);
        paint_row(&mut row, width, None);
        lines.push(row);
    }
    for _ in 0..padding_y {
        lines.push(" ".repeat(width));
    }
    lines
}

impl Component for Text {
    /// The trait's owned-line form: the same rows, built through
    /// [`Component::render_into`] in a fresh pool (the canvas path is the
    /// component's only row builder, so the two forms cannot drift).
    fn render(&self, width: usize) -> Vec<String> {
        let mut lines = Vec::new();
        {
            let mut rows = Rows::new(&mut lines);
            self.render_into(width, &mut rows);
            rows.trim();
        }
        lines
    }

    /// Direct-write path: same rows as [`Component::render`], each one built
    /// in a pooled row buffer instead of a fresh `String` (and no `Vec` for
    /// the frame). An UNPAINTED component answers from the rows memo
    /// ([`TextRowsCache`]): a steady frame copies the stored rows into the
    /// canvas without re-wrapping the text at all. A painter is an arbitrary
    /// closure with no cheap key, so the painted posture renders fresh exactly
    /// as before.
    fn render_into(&self, width: usize, out: &mut Rows<'_>) {
        let padding_x = self.padding_x.min(width.saturating_sub(1) / 2);
        let content_width = width.saturating_sub(padding_x * 2).max(1);
        // Read the painter (and release its guard) before the text lock: no
        // lock is held across a (possibly extension-supplied) bg closure.
        let bg = self.bg_fn();
        // Borrow the stored text for the wrap instead of `get_text()`'s
        // clone.
        let text = self
            .text
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if text.trim().is_empty() {
            return;
        }
        if bg.is_none() {
            let mut rows = self
                .rows
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let hit = rows.as_ref().is_some_and(|cache| {
                cache.width == width
                    && cache.padding_x == self.padding_x
                    && cache.padding_y == self.padding_y
                    && cache.text == *text
            });
            if !hit {
                *rows = Some(TextRowsCache {
                    text: text.clone(),
                    width,
                    padding_x: self.padding_x,
                    padding_y: self.padding_y,
                    rows: text_rows_uncached(&text, width, self.padding_x, self.padding_y),
                });
            }
            if let Some(cache) = rows.as_ref() {
                for row in &cache.rows {
                    out.push(row);
                }
            }
            return;
        }
        // Painted posture (memo never serves a painter): the pre-memo
        // direct-write row loop, byte for byte.
        // Only a text that really contains a tab pays for the three-space
        // expansion; the common case borrows the stored buffer.
        let wrapped = {
            let normalized: std::borrow::Cow<'_, str> = if text.contains('\t') {
                std::borrow::Cow::Owned(text.replace('\t', "   "))
            } else {
                std::borrow::Cow::Borrowed(text.as_str())
            };
            ansi::wrap_text_with_ansi(&normalized, content_width)
        };
        // The wrap is owned; release the text lock before the (possibly
        // extension-supplied) painter runs.
        drop(text);
        let left = " ".repeat(padding_x);
        let right = " ".repeat(padding_x);
        let bg = bg.as_deref();
        for _ in 0..self.padding_y {
            push_blank(out, width);
        }
        for wrapped_line in wrapped {
            out.push("");
            let row = out.line_mut();
            row.push_str(&left);
            row.push_str(&wrapped_line);
            row.push_str(&right);
            paint_row(row, width, bg);
        }
        for _ in 0..self.padding_y {
            push_blank(out, width);
        }
    }
}

/// An empty spacer of N rows (upstream `Spacer`).
pub struct Spacer {
    rows: usize,
}

impl Spacer {
    pub fn new(rows: usize) -> Self {
        Self { rows }
    }
}

impl Component for Spacer {
    fn render(&self, width: usize) -> Vec<String> {
        vec![" ".repeat(width); self.rows]
    }

    fn render_into(&self, width: usize, out: &mut Rows<'_>) {
        for _ in 0..self.rows {
            push_blank(out, width);
        }
    }
}

/// A full-width horizontal rule (upstream `DynamicBorder`).
pub struct DynamicBorder;

impl DynamicBorder {
    pub fn new() -> Self {
        Self
    }
}

impl Default for DynamicBorder {
    fn default() -> Self {
        Self::new()
    }
}

impl Component for DynamicBorder {
    fn render(&self, width: usize) -> Vec<String> {
        let border = "─".repeat(width.max(1));
        vec![super::with_theme(|t| {
            t.fg(super::ThemeColor::Border, &border)
        })]
    }
}

/// A padded, background-painted container (upstream `Box`).
pub struct BoxContainer {
    children: Vec<Box<dyn Component>>,
    padding_x: usize,
    padding_y: usize,
    bg: Arc<dyn Fn(&str) -> String + Send + Sync>,
}

impl BoxContainer {
    pub fn new(
        padding_x: usize,
        padding_y: usize,
        bg: impl Fn(&str) -> String + Send + Sync + 'static,
    ) -> Self {
        Self {
            children: Vec::new(),
            padding_x,
            padding_y,
            bg: Arc::new(bg),
        }
    }

    pub fn add_child(&mut self, component: impl Component + 'static) {
        self.children.push(Box::new(component));
    }
}

impl Component for BoxContainer {
    fn render(&self, width: usize) -> Vec<String> {
        let content_width = width.saturating_sub(self.padding_x * 2).max(1);
        let left = " ".repeat(self.padding_x);
        let right = " ".repeat(self.padding_x);
        let mut lines = Vec::new();
        for _ in 0..self.padding_y {
            lines.push(ansi::apply_background_to_line("", width, self.bg.as_ref()));
        }
        for child in &self.children {
            for line in child.render(content_width) {
                let with_margins = format!("{left}{line}{right}");
                lines.push(ansi::apply_background_to_line(
                    &with_margins,
                    width,
                    self.bg.as_ref(),
                ));
            }
        }
        for _ in 0..self.padding_y {
            lines.push(ansi::apply_background_to_line("", width, self.bg.as_ref()));
        }
        lines
    }

    /// Direct-write path: children write their rows straight into the canvas
    /// (migrated ones without any intermediate line buffer) and this
    /// container then margins + paints each of those rows in place. Row for
    /// row byte-identical to [`Component::render`].
    fn render_into(&self, width: usize, out: &mut Rows<'_>) {
        let content_width = width.saturating_sub(self.padding_x * 2).max(1);
        let left = " ".repeat(self.padding_x);
        let right = " ".repeat(self.padding_x);
        let bg = Some(self.bg.as_ref());
        for _ in 0..self.padding_y {
            out.push("");
            paint_row(out.line_mut(), width, bg);
        }
        for child in &self.children {
            let start = out.len();
            child.render_into(content_width, out);
            let end = out.len();
            for index in start..end {
                let row = out.line_at(index);
                row.insert_str(0, &left);
                row.push_str(&right);
                paint_row(row, width, bg);
            }
        }
        for _ in 0..self.padding_y {
            out.push("");
            paint_row(out.line_mut(), width, bg);
        }
    }

    fn invalidate(&self) {
        for child in &self.children {
            child.invalidate();
        }
    }
}

// ---------------------------------------------------------------------------
// Asynchronous terminal output (W10-I)
// ---------------------------------------------------------------------------
//
// W9-A: `Terminal::write` used to do `stdout.write_all` + `flush` while the
// caller (the UI thread, from `app.core.render()`) held the output mutex. A
// console in QuickEdit/selection, or a blocked ConPTY pipe, blocks the write
// syscall forever and freezes the whole event loop with it. Output now goes
// through a dedicated `pir-tui-writer` thread and a bounded frame queue:
// `write` only enqueues owned bytes (never performs I/O), so a stalled sink
// can no longer stall rendering. When the queue is full the OLDEST pending
// frame is dropped — the newest frame is what the user needs to see.
// Setup/teardown sequences use [`Terminal::write_sync`], which bypasses the
// queue, so a congested writer can never swallow a terminal restore.

/// Frames the asynchronous writer queue holds before it starts dropping the
/// oldest pending frame. One frame is one [`Terminal::write`] chunk (the
/// renderer emits one chunk per repaint): eight frames cover the ~16ms render
/// cadence across a console stall of roughly 130ms, and past that the newest
/// frame wins.
#[doc(hidden)]
pub const WRITER_QUEUE_CAPACITY: usize = 8;

/// Bound on the teardown wait for the async queue ([`TuiCore::stop`]): the
/// final cursor placement must not be written before the last frame, but the
/// stop path itself must never block indefinitely.
const STOP_FLUSH_TIMEOUT: Duration = Duration::from_millis(250);

/// Any byte sink the terminal writer can own: the process stdout in
/// production, a recording or blocking sink in the output-writer tests.
pub trait TerminalSink: std::io::Write + Send + 'static {}

impl<T: std::io::Write + Send + 'static> TerminalSink for T {}

/// One owned chunk waiting for the writer thread.
struct PendingFrame {
    /// Monotonically increasing enqueue sequence (1-based).
    seq: u64,
    bytes: Vec<u8>,
}

/// Shared state between [`Terminal`] (producer) and the `pir-tui-writer`
/// thread (consumer). The mutex only guards queue bookkeeping — no I/O is
/// ever performed while it is held, so a stalled sink cannot block a
/// producer's `enqueue`.
struct OutputWriter {
    state: Mutex<WriterState>,
    /// Signalled when a frame is queued, a write completes, or the queue is
    /// closed. Only [`OutputWriter::flush_bounded`] waits here.
    drained: Condvar,
    capacity: usize,
    /// Bumped whenever a queued frame is dropped. A dropped frame never
    /// reached the terminal, so the renderer's diff baseline (which describes
    /// the last frame it BUILT) no longer describes the screen; the next
    /// render must full-repaint. Kept outside `state` so the renderer can
    /// poll it without the writer lock.
    drop_epoch: AtomicU64,
}

struct WriterState {
    queue: std::collections::VecDeque<PendingFrame>,
    /// Last sequence handed out (0 = nothing enqueued yet).
    enqueued_seq: u64,
    /// Every frame with `seq <= settled_seq` has been written or dropped.
    settled_seq: u64,
    /// Frames dropped because the queue was full (diagnostics / tests).
    dropped_frames: u64,
    /// The owning [`Terminal`] is gone; the writer drains the queue and exits.
    closed: bool,
}

impl OutputWriter {
    fn new(capacity: usize) -> Self {
        Self {
            state: Mutex::new(WriterState {
                queue: std::collections::VecDeque::new(),
                enqueued_seq: 0,
                settled_seq: 0,
                dropped_frames: 0,
                closed: false,
            }),
            drained: Condvar::new(),
            capacity,
            drop_epoch: AtomicU64::new(0),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, WriterState> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Queue `bytes`, dropping the OLDEST pending frames when the queue is
    /// full. The caller holds the mutex only for O(queue) bookkeeping — no
    /// I/O — so this never blocks on a stalled console.
    fn enqueue(&self, bytes: Vec<u8>) {
        let mut state = self.lock();
        while state.queue.len() >= self.capacity {
            let Some(dropped) = state.queue.pop_front() else {
                break;
            };
            state.dropped_frames += 1;
            state.settled_seq = state.settled_seq.max(dropped.seq);
            self.drop_epoch.fetch_add(1, Ordering::Relaxed);
        }
        state.enqueued_seq += 1;
        let seq = state.enqueued_seq;
        state.queue.push_back(PendingFrame { seq, bytes });
        drop(state);
        // `notify_all`, not `notify_one`: the waiter set holds both the
        // writer thread (waiting for work) and any `flush_bounded` caller
        // (waiting for drain); waking only the flusher would leave the new
        // frame unclaimed.
        self.drained.notify_all();
    }

    /// Wait until every frame enqueued before this call has been written or
    /// dropped, or `timeout` elapses. Returns whether the queue drained
    /// within the bound. Never waits indefinitely.
    fn flush_bounded(&self, timeout: Duration) -> bool {
        let started = Instant::now();
        let mut state = self.lock();
        let target = state.enqueued_seq;
        while state.settled_seq < target {
            let elapsed = started.elapsed();
            if elapsed >= timeout {
                return false;
            }
            let (guard, _) = self
                .drained
                .wait_timeout(state, timeout - elapsed)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            state = guard;
        }
        true
    }

    /// Tell the writer to finish the queued frames and exit.
    fn close(&self) {
        self.lock().closed = true;
        self.drained.notify_all();
    }

    fn dropped_frames(&self) -> u64 {
        self.lock().dropped_frames
    }

    fn pending_frames(&self) -> usize {
        self.lock().queue.len()
    }

    fn enqueued_seq(&self) -> u64 {
        self.lock().enqueued_seq
    }
}

/// Body of the `pir-tui-writer` thread: write + flush each chunk in queue
/// order, publishing the settled sequence after every chunk so
/// [`OutputWriter::flush_bounded`] can observe progress.
fn writer_loop(sink: &mut dyn TerminalSink, writer: &OutputWriter) {
    loop {
        let frame = {
            let mut state = writer.lock();
            loop {
                if let Some(frame) = state.queue.pop_front() {
                    break Some(frame);
                }
                if state.closed {
                    break None;
                }
                state = writer
                    .drained
                    .wait(state)
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
            }
        };
        let Some(frame) = frame else {
            break;
        };
        let _ = sink.write_all(&frame.bytes);
        let _ = sink.flush();
        let mut state = writer.lock();
        state.settled_seq = state.settled_seq.max(frame.seq);
        drop(state);
        writer.drained.notify_all();
    }
}

/// Spawn the dedicated writer thread. `Err` means the thread could not be
/// created; the terminal then falls back to the synchronous direct path for
/// every write (the same bounded semantics as before W10-I, never a panic).
fn spawn_writer(sink: Box<dyn TerminalSink>, writer: Arc<OutputWriter>) -> std::io::Result<()> {
    std::thread::Builder::new()
        .name("pir-tui-writer".to_string())
        .spawn(move || {
            let mut sink = sink;
            writer_loop(sink.as_mut(), &writer);
        })
        .map(|_handle| ())
}

// ---------------------------------------------------------------------------
// TuiCore: the differential renderer + terminal driver.
// ---------------------------------------------------------------------------

/// Terminal geometry + raw mode handle.
pub struct Terminal {
    /// Cached columns; updated via [`Terminal::set_size`] from the
    /// `Event::Resize` handler (P1-5).
    columns: Mutex<usize>,
    rows: Mutex<usize>,
    /// Bounded async frame queue + `pir-tui-writer` thread. `None` only when
    /// the writer thread could not be spawned; [`Terminal::write`] then uses
    /// the direct path.
    writer: Option<Arc<OutputWriter>>,
    /// Synchronous direct-write sink for setup/teardown sequences (and the
    /// no-thread fallback). Separate from the writer thread's sink so the
    /// restore path never queues behind a stalled frame write.
    direct_out: Mutex<Box<dyn TerminalSink>>,
    /// OSC 7501 program-status negotiation (upstream `ProcessTerminal`'s
    /// `programStatus` / `programStatusSupported` / `programStatusQueryPending`).
    program_status: Mutex<ProgramStatusNegotiation>,
    /// `PI_TUI_WRITE_LOG` transcript of everything written to the terminal
    /// (upstream `ProcessTerminal.write`'s `appendFileSync`); disabled unless
    /// the environment names a path or directory.
    write_log: super::stdin_buffer::WriteLog,
}

impl Terminal {
    fn detect_size() -> (usize, usize) {
        // Best-effort: crossterm size query without requiring a full event
        // loop.
        match crossterm::terminal::size() {
            Ok((cols, rows)) => (cols as usize, rows as usize),
            Err(_) => (80, 24),
        }
    }

    pub fn new() -> Self {
        Self::with_sink_factory(
            || Box::new(std::io::stdout()),
            super::stdin_buffer::WriteLog::from_env(),
        )
    }

    /// [`Terminal::new`] with an injectable output-sink factory: `make_sink`
    /// is called twice, once for the async writer thread and once for the
    /// synchronous direct path. The output-writer tests use it to observe (or
    /// deliberately stall) the byte stream without touching process stdout.
    #[doc(hidden)]
    pub fn with_sink_factory(
        mut make_sink: impl FnMut() -> Box<dyn TerminalSink>,
        write_log: super::stdin_buffer::WriteLog,
    ) -> Self {
        let (cols, rows) = Self::detect_size();
        let writer_state = Arc::new(OutputWriter::new(WRITER_QUEUE_CAPACITY));
        // A failed spawn leaves `writer` empty; the direct path still works
        // (and `write` falls back to it) instead of panicking at startup.
        let writer = if spawn_writer(make_sink(), Arc::clone(&writer_state)).is_ok() {
            Some(writer_state)
        } else {
            None
        };
        Self {
            columns: Mutex::new(cols),
            rows: Mutex::new(rows),
            writer,
            direct_out: Mutex::new(make_sink()),
            program_status: Mutex::new(ProgramStatusNegotiation::default()),
            write_log,
        }
    }

    /// [`Terminal::new`] with an EXPLICIT write log (the `PI_TUI_WRITE_LOG`
    /// sink, injected). The render tests use it to observe the exact byte
    /// stream the terminal receives (first paint vs. forced repaint) without
    /// touching the process-global environment.
    pub fn with_write_log(log: super::stdin_buffer::WriteLog) -> Self {
        Self::with_sink_factory(|| Box::new(std::io::stdout()), log)
    }

    pub fn columns(&self) -> usize {
        *self
            .columns
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    pub fn rows(&self) -> usize {
        *self
            .rows
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Update the cached terminal geometry (P1-5: the event loop forwards
    /// `Event::Resize` here; the renderer reads the new size next frame and
    /// full-repaints on the width change).
    pub fn set_size(&self, cols: usize, rows: usize) {
        *self
            .columns
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = cols.max(1);
        *self
            .rows
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = rows.max(1);
    }

    /// Queue one output chunk for the dedicated writer thread.
    ///
    /// Never performs console I/O on the calling thread: the bytes are moved
    /// into the bounded queue and written by `pir-tui-writer` in order. When
    /// the queue is full the OLDEST pending frame is dropped, so the newest
    /// frame always wins and a stalled console cannot freeze the UI.
    /// `PI_TUI_WRITE_LOG` keeps its upstream semantics and is appended
    /// synchronously at call time, before the chunk is queued.
    pub fn write(&self, data: &str) {
        // Upstream `ProcessTerminal.write`: every chunk written to the
        // terminal is appended to `PI_TUI_WRITE_LOG` when that is configured.
        // Logged at call time so the transcript follows caller order (and
        // stays readable immediately after a render returns).
        self.write_log.append(data);
        match &self.writer {
            Some(writer) => writer.enqueue(data.as_bytes().to_vec()),
            // Writer thread unavailable (spawn failure): keep the pre-W10-I
            // behavior rather than dropping output.
            None => self.write_direct(data),
        }
    }

    /// [`Terminal::write`] taking the already-built output buffer by value:
    /// `String::into_bytes` hands the renderer's frame allocation to the
    /// writer thread instead of copying the whole frame into a fresh `Vec`
    /// every repaint. The render path builds one `String` per frame, so this
    /// removes a full-frame allocation + memcpy from the hot path.
    #[doc(hidden)]
    pub fn write_owned(&self, data: String) {
        self.write_log.append(&data);
        match &self.writer {
            Some(writer) => writer.enqueue(data.into_bytes()),
            None => self.write_direct(&data),
        }
    }

    /// Synchronous direct-write path: write + flush on the calling thread,
    /// bypassing the frame queue. Setup/teardown sequences (alternate screen,
    /// raw-mode restore, the final cursor placement) use this so a congested
    /// queue can never swallow or delay the terminal restore. Logged exactly
    /// like [`Terminal::write`].
    pub fn write_sync(&self, data: &str) {
        self.write_log.append(data);
        self.write_direct(data);
    }

    fn write_direct(&self, data: &str) {
        use std::io::Write;
        let mut out = self
            .direct_out
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let _ = out.write_all(data.as_bytes());
        let _ = out.flush();
    }

    /// Wait at most `timeout` for every frame enqueued before this call to be
    /// written (or dropped by the coalescing rule). Returns `true` when the
    /// queue drained within the bound — never blocks indefinitely. Used at
    /// startup/cleanup/exit so the final frame is on screen before teardown.
    pub fn flush_bounded(&self, timeout: Duration) -> bool {
        match &self.writer {
            Some(writer) => writer.flush_bounded(timeout),
            None => true,
        }
    }

    /// Tell the async writer to drain what is queued and exit (idempotent).
    /// Teardown routes through this after [`TuiCore::stop`]'s final direct
    /// write so no queued frame can outlive the terminal restore; `Terminal`'s
    /// `Drop` is the fallback for every other exit path.
    pub fn close_writer(&self) {
        if let Some(writer) = &self.writer {
            writer.close();
        }
    }

    /// Frames dropped by the oldest-first coalescing rule (diagnostics/tests).
    #[doc(hidden)]
    pub fn dropped_frames(&self) -> u64 {
        self.writer
            .as_ref()
            .map(|writer| writer.dropped_frames())
            .unwrap_or(0)
    }

    /// Count of QUEUED frames dropped so far. Unlike [`Self::dropped_frames`]
    /// this is lock-free: the renderer reads it once per frame to detect that
    /// its in-memory diff baseline no longer matches the screen and the next
    /// frame must be a full repaint.
    #[doc(hidden)]
    pub fn drop_epoch(&self) -> u64 {
        self.writer
            .as_ref()
            .map(|writer| writer.drop_epoch.load(Ordering::Relaxed))
            .unwrap_or(0)
    }

    /// Frames still waiting for the writer thread (diagnostics/tests).
    #[doc(hidden)]
    pub fn pending_frames(&self) -> usize {
        self.writer
            .as_ref()
            .map(|writer| writer.pending_frames())
            .unwrap_or(0)
    }

    /// Monotonically increasing count of chunks enqueued so far (the write
    /// sequence; diagnostics/tests).
    #[doc(hidden)]
    pub fn write_sequence(&self) -> u64 {
        self.writer
            .as_ref()
            .map(|writer| writer.enqueued_seq())
            .unwrap_or(0)
    }

    /// Terminal title (upstream `terminal.setTitle` → OSC 0).
    pub fn set_title(&self, title: &str) {
        self.write(&format!("\x1b]0;{title}\x07"));
    }

    // -----------------------------------------------------------------------
    // OSC 7501 program status (upstream ProcessTerminal's setProgramStatus /
    // writeProgramStatus / stop-clear path).
    // -----------------------------------------------------------------------

    fn program_status_state(&self) -> std::sync::MutexGuard<'_, ProgramStatusNegotiation> {
        self.program_status
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Send the OSC 7501 support query once per connection (upstream
    /// `queryAndEnableKittyProtocol`'s program-status half). `PI_PROGRAM_STATUS=1`
    /// always reports, `0` never reports; otherwise the report is sent only
    /// after the terminal answers the query
    /// (see [`Terminal::handle_program_status_reply`]).
    ///
    /// pir deviation: the support query itself is deliberately NOT written to
    /// the terminal. crossterm owns stdin parsing and would mangle the
    /// terminal's reply into key events, so support can never be confirmed
    /// in-process; `query_pending` is cleared by the event loop's grace
    /// timeout instead (upstream clears it on the DA1 answer). Reports
    /// therefore flow only with `PI_PROGRAM_STATUS=1` unless a future
    /// raw-stdin path re-enables the probe by writing
    /// [`super::program_status::PROGRAM_STATUS_QUERY`] and feeding
    /// [`Terminal::handle_program_status_reply`].
    pub fn begin_program_status_negotiation(&self) {
        let mut state = self.program_status_state();
        if state.negotiation_started {
            return;
        }
        let query = state.begin();
        let resend = if state.supported {
            state.latest.as_ref().map(format_program_status)
        } else {
            None
        };
        drop(state);
        // The query is computed by `begin()` (state machine parity with
        // upstream) but intentionally not written — see the doc comment.
        let _ = query;
        if let Some(resend) = resend {
            self.write(&resend);
        }
    }

    /// Feed a raw input sequence into the negotiation: returns `true` when the
    /// sequence is the OSC 7501 support reply, in which case support is
    /// confirmed and the latest status is re-sent (upstream stdin branch on
    /// `isProgramStatusReply`).
    pub fn handle_program_status_reply(&self, sequence: &str) -> bool {
        if !is_program_status_reply(sequence) {
            return false;
        }
        let mut state = self.program_status_state();
        if state.query_pending {
            state.query_pending = false;
            state.supported = true;
            let resend = state.latest.as_ref().map(format_program_status);
            drop(state);
            if let Some(resend) = resend {
                self.write(&resend);
            }
        }
        true
    }

    /// The last owed DA1 answer arrived without a preceding OSC 7501 reply: the
    /// terminal does not support program status (upstream clears
    /// `programStatusQueryPending` when `pendingKeyboardProtocolDeviceAttributes`
    /// reaches 0).
    pub fn program_status_query_timeout(&self) {
        self.program_status_state().query_pending = false;
    }

    /// Report what the program is doing (OSC 7501). Sent only to terminals
    /// that support it (confirmed reply or `PI_PROGRAM_STATUS=1`); the latest
    /// status is re-sent when support is confirmed or the terminal restarts
    /// (upstream `setProgramStatus`).
    pub fn set_program_status(&self, status: &ProgramStatus) {
        let mut state = self.program_status_state();
        if !state.negotiation_started {
            // Upstream negotiates during start(); pir's start path lives
            // outside this module, so the first report lazily starts it. The
            // support query itself is not written — see
            // [`Terminal::begin_program_status_negotiation`].
            let query = state.begin();
            let _ = query;
            drop(state);
            state = self.program_status_state();
        }
        state.latest = if status.state == ProgramStatusState::Clear {
            None
        } else {
            Some(status.clone())
        };
        if !state.supported {
            return;
        }
        let encoded = format_program_status(status);
        drop(state);
        self.write(&encoded);
    }

    /// Remove the status while stopped, for example after exit or while
    /// suspended; a restart negotiates and reports again (upstream `stop()`).
    /// The latest status is kept so it can be re-sent.
    pub fn stop_program_status(&self) {
        let mut state = self.program_status_state();
        let clear = if state.supported && state.latest.is_some() {
            Some(format_program_status(&ProgramStatus::new(
                ProgramStatusState::Clear,
            )))
        } else {
            None
        };
        state.supported = false;
        state.query_pending = false;
        drop(state);
        if let Some(clear) = clear {
            // Teardown-adjacent control sequence: direct path, so a busy
            // frame queue cannot delay it.
            self.write_sync(&clear);
        }
    }

    /// Expose the negotiated state for tests and future stdin wiring.
    pub fn program_status_negotiation(
        &self,
    ) -> std::sync::MutexGuard<'_, ProgramStatusNegotiation> {
        self.program_status_state()
    }
}

impl Default for Terminal {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for Terminal {
    fn drop(&mut self) {
        // Let the writer drain what is already queued and exit. Never block
        // the dropper: a stalled sink only keeps the detached thread alive.
        if let Some(writer) = &self.writer {
            writer.close();
        }
    }
}

/// Upstream `SEGMENT_RESET` (tui.ts:423): the per-line reset the frame appends
/// to every row it writes (see [`TuiCore::append_line_reset`]). Also the
/// suffix the diff compares strip from the baseline, which carries exactly one
/// copy of it per written row.
const SEGMENT_RESET: &str = "\x1b[0m\x1b]8;;\x07";

/// The differential main-screen renderer.
pub struct TuiCore {
    terminal: Arc<Terminal>,
    /// Full-frame component tree (chat content and the dock area).
    content: Mutex<Vec<Box<dyn Component>>>,
    /// OSC 7501 program-status reporter (upstream
    /// `interactive-mode.programStatus`); its sink writes through
    /// [`Terminal::set_program_status`].
    program_status_reporter: Mutex<ProgramStatusReporter>,
    previous_lines: Vec<String>,
    /// Recycle pool for the raw (undecorated) frame ([`TuiCore::frame_lines`]):
    /// the row buffers a frame that died this iteration retires, reused row by
    /// row through [`Rows`] instead of one fresh `String` per frame row. Never
    /// the live snapshot (`last_frame`) nor the live diff baseline
    /// (`previous_lines`) — both stay distinct buffers.
    frame_scratch: Vec<String>,
    /// Recycle pool for the windowed render path ([`TuiCore::render_scrolled`]):
    /// the buffer `previous_lines` retired one scrolled frame earlier, reused
    /// row by row (`clone_from`) instead of allocating a fresh painted frame
    /// every frame. Never the live diff baseline — that stays `previous_lines`.
    scrolled_scratch: Vec<String>,
    previous_width: usize,
    previous_height: usize,
    /// Where the hardware cursor actually sits, in buffer-row coordinates
    /// (upstream `hardwareCursorRow`). Diff rows are reached with relative
    /// moves computed against this.
    hardware_cursor_row: usize,
    /// Buffer row shown at the top of the viewport after the last frame
    /// (upstream `previousViewportTop`; 0 until the frame exceeds the
    /// terminal height and the main screen starts scrolling).
    previous_viewport_top: usize,
    last_render: Mutex<Instant>,
    min_frame: Duration,
    /// Last [`Terminal::drop_epoch`] this renderer observed. A change means a
    /// queued frame was dropped, so the terminal missed bytes the in-memory
    /// diff baseline assumes are on screen.
    seen_drop_epoch: u64,
    stopped: bool,
    // -- Alt-screen viewport (upstream TuiAltScreen's application-owned
    // -- scroll view; scrollback is read through a windowed render path).
    /// Scrollback rows the viewport is held above the frame end
    /// (upstream `ScrollView.scrollTop` inverted; 0 = follow the end).
    scroll_offset: usize,
    /// At the transcript end but NOT following (upstream
    /// `ScrollView.followSuppressedAtEnd`): a search reveal pinned the view
    /// with `scrollTo(end, { disableFollow: true })`, so streaming rows push
    /// the transcript past the window instead of moving it. Only meaningful
    /// while `scroll_offset == 0`; any ordinary scroll clears it.
    follow_suppressed: bool,
    /// Transcript length at the previous render (P1-2 viewport pinning):
    /// the scrolled render path compensates `scroll_offset` by the frame's
    /// transcript growth so a viewport scrolled into history stays put
    /// while streaming appends rows (upstream alt-screen keeps an absolute
    /// `scrollTop` and disables follow instead).
    last_transcript_end: Option<usize>,
    /// Bottom dock height in rows (drives the windowed render path, which
    /// keeps the dock visible while the transcript scrolls).
    dock_height: usize,
    /// Per-frame decoration hook: the alt-screen interaction layer (search
    /// highlight, selection, scrollbar, search box, flash) applied to the
    /// painted lines before diffing.
    decorator: Option<FrameDecorator>,
    /// The last frame's UNDECORATED lines (search corpus / selection source).
    last_frame: Vec<String>,
    /// Absolute buffer row painted at the top of the visible transcript area
    /// (screen-row → buffer-row mapping for mouse selection).
    last_window_start: usize,
    /// How the last frame was painted: the screen-row → buffer-row mapping
    /// the mouse family uses (transcript window + dock, see
    /// [`TuiCore::screen_row_to_buffer_row`]).
    painted_layout: PaintedLayout,
    /// Bumped on every render; search matches key off it to avoid rebuilding
    /// the corpus when the frame is unchanged.
    frame_version: u64,
    /// Inclusive row range of the last frame that differs from the previous
    /// one (`None` = byte-identical, or the whole frame on a length change).
    /// The search cache re-fingerprints only these rows instead of hashing the
    /// whole band every frame.
    last_frame_changed_rows: Option<(usize, usize)>,
    /// [`RowsWork`] of the most recent [`TuiCore::frame_lines`] (this core's
    /// canvas only): tests pin the steady-frame row-copy bytes through this
    /// per-instance copy, so parallel tests cannot pollute the measurement.
    last_rows_work: RowsWork,
    /// Set by [`TuiCore::invalidate_frame`]: the next full render must clear
    /// the screen (`2J` + home + `3J`) — the caller knows rows below the new
    /// content are stale (e.g. after an overlay swapped the alternate screen
    /// out), and a re-write over the existing content would duplicate the
    /// frame head on the main screen's scrollback (startup header twice for
    /// one app start).
    force_clear: bool,
    /// Editor cursor cell (dock-relative row, column), written by the dock
    /// component on every render (upstream `extractCursorPosition` over the
    /// editor's CURSOR_MARKER).
    dock_cursor_cell: Option<std::sync::Arc<std::sync::Mutex<Option<(usize, usize)>>>>,
    /// Upstream `showHardwareCursor` (settings / PI_HARDWARE_CURSOR): with
    /// the cursor visible, the renderer moves it to the editor cell after
    /// every frame; hidden (default), it is hidden once and stays put.
    show_hardware_cursor: bool,
    /// Whether the terminal cursor is currently shown (tracks the emitted
    /// `?25h`/`?25l` state so repeated frames stay byte-thrifty).
    cursor_visible: bool,
    /// Windowed viewport (P0.3, huge sessions): the app owns the alternate
    /// screen and the transcript component materializes only a bounded band
    /// of blocks, so the FOLLOW path (offset 0) must paint only the visible
    /// window + dock exactly like the scrolled path does — never the whole
    /// band, and never a frame buffer proportional to the transcript.
    ///
    /// With this on, every render takes the `render_scrolled` path (the same
    /// machinery the scrolled view already uses: a full-screen window with
    /// absolute row addressing, valid because the alt screen is active), so
    /// the painted frame, the diff baseline (`previous_lines`), the cursor
    /// bookkeeping and the `FrameWork` per-row passes are all O(visible rows
    /// + dock). Off by default: the main-screen path (tuiMode regular) and
    ///   every existing test keep the byte-exact pre-windowing behavior.
    windowed_viewport: bool,
}

/// Per-frame decoration context (see [`super::altscreen_search`]).
pub type FrameDecorateContext = super::altscreen_search::FrameDecorateContext;

/// The decoration hook installed by the alt-screen interaction layer.
pub type FrameDecorator = Box<dyn Fn(&mut Vec<String>, FrameDecorateContext) + Send>;

/// How the last frame was painted, as far as the mouse family needs it: the
/// screen-row → buffer-row mapping of [`TuiCore::screen_row_to_buffer_row`].
///
/// The follow path writes the whole undecorated frame with its top row at
/// `window_start` (rows above it live in the terminal's own scrollback); the
/// scrolled path paints a transcript window (`window_start..`, `window_len`
/// rows) and the dock directly under it — a gap in buffer coordinates, since
/// the dock is always the frame's tail.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PaintedLayout {
    /// `true` when the last frame was the windowed (scrolled) render path.
    pub windowed: bool,
    /// Absolute buffer row painted at the top of the transcript area
    /// (screen row 0).
    pub window_start: usize,
    /// Transcript rows painted (screen rows `0..window_len`).
    pub window_len: usize,
    /// Absolute buffer row one past the last transcript row (the dock start).
    pub transcript_end: usize,
    /// Dock rows painted directly below the window (screen rows
    /// `window_len..window_len + dock_len`). The dock carries the editor,
    /// footer and any open dialog/selector, so those rows are selectable.
    pub dock_len: usize,
    /// Total rows of the undecorated frame (the selection corpus length).
    pub frame_len: usize,
}

/// P1-2 (pure; regression-tested): the scrolled-render scroll offset after
/// compensating for transcript growth. `scroll_offset` is an offset from
/// the frame end, so a viewport scrolled into history would slide down as
/// streaming appends rows (upstream alt-screen keeps an absolute
/// `scrollTop` and disables follow instead). While the viewport is
/// scrolled (`offset > 0`), the offset grows by the transcript's growth
/// since the previous frame — the window start stays pinned to the same
/// content rows. Follow mode (`offset == 0`) keeps following the end, and
/// the result clamps to `max`, the largest in-bounds offset for the new
/// transcript (the render path's clamp fallback).
pub fn compensated_scroll_offset(
    offset: usize,
    last_transcript_end: Option<usize>,
    transcript_end: usize,
    max: usize,
) -> usize {
    if offset == 0 {
        return 0;
    }
    pinned_scroll_offset(offset, last_transcript_end, transcript_end, max)
}

/// Growth compensation for a viewport that is NOT following (upstream
/// `ScrollView.updateLayout`'s clamp over a non-following `scrollTop`): the
/// offset grows by the transcript's growth since the previous frame so the
/// window keeps the same content rows, then clamps to `max`.
///
/// [`compensated_scroll_offset`] short-circuits at `offset == 0` because
/// that is the follow position; a view held at the end by a search reveal
/// (`followSuppressedAtEnd`) is at offset 0 *without* following, so its
/// growth must pin it exactly like a scrolled view — that is what this entry
/// point is for.
pub fn pinned_scroll_offset(
    offset: usize,
    last_transcript_end: Option<usize>,
    transcript_end: usize,
    max: usize,
) -> usize {
    let grown = match last_transcript_end {
        Some(previous) => offset.saturating_add(transcript_end.saturating_sub(previous)),
        None => offset,
    };
    grown.min(max)
}

/// Pure row-diff for the scrolled render path: indices of `painted` rows
/// whose content differs from the previous frame's row at the same position.
/// Frames of different row counts (or against an empty previous frame) are
/// not diffable — every painted row counts as changed. Exercised by
/// `tests/optimization_parity.rs` and `tests/render_diff.rs`.
pub fn scrolled_changed_rows(previous: &[String], painted: &[String]) -> Vec<usize> {
    if previous.len() != painted.len() {
        return (0..painted.len()).collect();
    }
    previous
        .iter()
        .zip(painted.iter())
        .enumerate()
        .filter_map(|(row, (old, new))| (old != new).then_some(row))
        .collect()
}

/// Pure output-buffer builder for the scrolled render path's diff branch
/// (upstream tui-alt-screen.ts doRender per-row rewrite): rewrite each
/// changed row with absolute addressing (`ESC[{row};1H` + clear-line), then
/// unconditionally rewrite the last row so the frame ends in exactly the
/// full-repaint end state — the cursor sits just after the last row's
/// content, keeping `stop()`'s relative cursor move anchored (rewriting an
/// unchanged last row is a visual no-op). Byte-for-byte identical to the
/// inline writer this extracts; exercised by `tests/render_diff.rs`.
pub fn scrolled_diff_output(painted: &[String], changed: &[usize]) -> String {
    // Rows are written with `write!` into the output buffer: the per-row
    // address prefix used to be a `format!` temporary (one `String`
    // allocation and drop per changed row per frame) even though the bytes are
    // identical.
    use std::fmt::Write as _;
    let mut out = String::from("\x1b[?2026h");
    for &row in changed {
        if let Some(line) = painted.get(row) {
            let _ = write!(out, "\x1b[{};1H\x1b[2K", row + 1);
            out.push_str(line);
        }
    }
    if let Some(last) = painted.len().checked_sub(1) {
        let _ = write!(out, "\x1b[{};1H\x1b[2K", last + 1);
        out.push_str(&painted[last]);
    }
    out.push_str("\x1b[?2026l");
    out
}

/// Pure output-buffer builder for the scrolled render path's full-repaint
/// branch (non-diffable frames: first scrolled frame, resize, invalidate,
/// follow-mode frames whose buffer does not line up with the screen):
/// clear + home, then every window row. Byte-for-byte identical to the
/// inline writer this extracts; exercised by `tests/render_diff.rs`.
pub fn scrolled_full_output(painted: &[String]) -> String {
    let mut out = String::from("\x1b[?2026h\x1b[2J\x1b[H");
    for (index, line) in painted.iter().enumerate() {
        if index > 0 {
            out.push_str("\r\n");
        }
        out.push_str("\x1b[2K");
        out.push_str(line);
    }
    out.push_str("\x1b[?2026l");
    out
}

impl TuiCore {
    pub fn new(terminal: Terminal) -> Self {
        let terminal = Arc::new(terminal);
        let sink_terminal = Arc::clone(&terminal);
        let program_status_reporter = Mutex::new(ProgramStatusReporter::new(
            Arc::new(move |status: &ProgramStatus| sink_terminal.set_program_status(status)),
            Arc::new(|| -> Option<String> { None }),
        ));
        Self {
            terminal,
            program_status_reporter,
            content: Mutex::new(Vec::new()),
            previous_lines: Vec::new(),
            frame_scratch: Vec::new(),
            scrolled_scratch: Vec::new(),
            previous_width: 0,
            previous_height: 0,
            hardware_cursor_row: 0,
            previous_viewport_top: 0,
            last_render: Mutex::new(Instant::now() - Duration::from_secs(1)),
            min_frame: Duration::from_millis(16),
            seen_drop_epoch: 0,
            stopped: false,
            scroll_offset: 0,
            follow_suppressed: false,
            last_transcript_end: None,
            dock_height: 0,
            decorator: None,
            last_frame: Vec::new(),
            last_window_start: 0,
            painted_layout: PaintedLayout::default(),
            frame_version: 0,
            last_frame_changed_rows: None,
            last_rows_work: RowsWork::default(),
            force_clear: false,
            dock_cursor_cell: None,
            show_hardware_cursor: false,
            cursor_visible: true,
            windowed_viewport: false,
        }
    }

    pub fn terminal(&self) -> &Terminal {
        &self.terminal
    }

    /// Set the terminal title without the render lock (`&self`; extension
    /// `ctx.ui.setTitle` notifications land here from the drain loop).
    pub fn set_title(&self, title: &str) {
        self.terminal.set_title(title);
    }

    /// Current terminal columns without the render lock (`&self`; renderer
    /// option payloads read the width from the drain loop).
    pub fn columns(&self) -> usize {
        self.terminal.columns()
    }

    /// Current terminal rows without the render lock (`&self`; the alt-screen
    /// scroll geometry reads the height the same way it reads the width).
    pub fn rows(&self) -> usize {
        self.terminal.rows()
    }

    /// Report what the program is doing (OSC 7501) without any other lock
    /// (`&self`; mirrors [`TuiCore::set_title`]).
    pub fn set_program_status(&self, status: &ProgramStatus) {
        self.terminal.set_program_status(status);
    }

    /// The OSC 7501 program-status reporter (upstream
    /// `interactive-mode.programStatus`). The agent event loop drives the
    /// upstream transitions through it: `agent_start` when a run begins,
    /// `assistant_message_end` for each assistant response, `compaction_start`
    /// / `compaction_end` around compaction, `agent_settled` when the run
    /// settles, `set_blocked` while a dialog waits for the user, and `reset`
    /// after a session switch.
    pub fn program_status_reporter(&self) -> std::sync::MutexGuard<'_, ProgramStatusReporter> {
        self.program_status_reporter
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    pub fn push_content(&mut self, component: Box<dyn Component>) {
        self.content
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(component);
    }

    // -----------------------------------------------------------------------
    // Alt-screen viewport: scrollback + interaction-layer hooks.
    // -----------------------------------------------------------------------

    /// Scroll the viewport `offset` rows above the frame end (upstream
    /// fullscreen `ScrollView` scrollback; 0 restores follow-end). Returning
    /// to 0 drops the windowed cache so the next render full-repaints.
    ///
    /// This is upstream `ScrollView.scrollTo(top)` / `scrollBy(n)` without
    /// `disableFollow`: the view follows again at the end, so a pending
    /// search pin ([`TuiCore::scroll_to`] with `disable_follow`) is cleared —
    /// including when the offset does not move (upstream `scrollBy` at the
    /// suppressed end re-enables follow with `moved === 0`).
    pub fn set_scroll_offset(&mut self, offset: usize) {
        self.scroll_to(offset, false);
    }

    /// Upstream `ScrollView.scrollTo(top, { disableFollow })` in the
    /// viewport's inverted coordinates: move to `offset` rows above the frame
    /// end and, with `disable_follow` and an offset that lands on the end
    /// (`offset == 0`), hold the view there *without* following (upstream
    /// `followSuppressedAtEnd`) — the search reveal uses this so streaming
    /// rows grow below the window instead of pushing it. Any later scroll
    /// toward the end ([`TuiCore::set_scroll_offset`], the wheel/keys) or
    /// `scrollToEnd` clears the pin.
    pub fn scroll_to(&mut self, offset: usize, disable_follow: bool) {
        if offset == 0 && self.scroll_offset > 0 {
            self.invalidate_frame();
            // Leaving the windowed path: the pinning baseline becomes the end
            // the view is held at (the last rendered frame's transcript end),
            // so a search reveal that lands on the end keeps pinning the rows
            // that followed it instead of starting from no baseline.
            self.last_transcript_end = Some(self.last_frame_transcript_end());
        }
        self.scroll_offset = offset;
        self.follow_suppressed = disable_follow && offset == 0;
    }

    /// The transcript end of the last rendered frame (undocked), for the
    /// viewport paths that re-anchor between renders.
    fn last_frame_transcript_end(&self) -> usize {
        super::altscreen_search::viewport_geometry(
            self.last_frame.len(),
            self.dock_height,
            self.terminal.rows().max(1),
        )
        .transcript_end
    }

    /// Whether the viewport tracks new transcript rows (upstream
    /// `ScrollView.isFollowingEnd`): at the end and not search-pinned.
    pub fn is_following(&self) -> bool {
        self.scroll_offset == 0 && !self.follow_suppressed
    }

    /// Whether the view is held at the transcript end by a search reveal
    /// (upstream `ScrollView.followSuppressedAtEnd`).
    pub fn follow_suppressed(&self) -> bool {
        self.follow_suppressed
    }

    /// The current scrollback offset in rows (0 = follow the end).
    pub fn scroll_offset(&self) -> usize {
        self.scroll_offset
    }

    /// Upstream `ScrollView.updateLayout` for the fullscreen viewport, run
    /// once per frame from [`TuiCore::render`] before the render path is
    /// chosen: a following viewport re-anchors at the transcript end, while a
    /// non-following one (scrolled into history, or held at the end by a
    /// search reveal) keeps its absolute window and absorbs the transcript's
    /// growth ([`pinned_scroll_offset`]). Landing back at the end resumes
    /// follow unless the search pin holds it.
    fn update_viewport_layout(&mut self, transcript_end: usize, max: usize) {
        if self.is_following() {
            self.last_transcript_end = Some(transcript_end);
            self.scroll_offset = 0;
            return;
        }
        self.scroll_offset = pinned_scroll_offset(
            self.scroll_offset,
            self.last_transcript_end,
            transcript_end,
            max,
        );
        self.last_transcript_end = Some(transcript_end);
        // Leaving the end drops the search pin (upstream `updateLayout`).
        if self.scroll_offset > 0 {
            self.follow_suppressed = false;
        }
    }

    /// Bottom dock height in rows (see [`TuiCore::set_dock_height`]).
    pub fn dock_height(&self) -> usize {
        self.dock_height
    }

    /// Enable/disable the windowed viewport (see the field docs). The
    /// caller (the interactive loop) turns it on exactly when the app owns
    /// the alternate screen: with the alt screen active the frame IS the
    /// screen, so the follow path may paint a window of the materialized
    /// transcript instead of the whole frame.
    pub fn set_windowed_viewport(&mut self, on: bool) {
        self.windowed_viewport = on;
    }

    /// Whether the windowed viewport is on.
    pub fn windowed_viewport(&self) -> bool {
        self.windowed_viewport
    }

    /// Re-anchor the scroll-pinning baseline after the caller changed the
    /// materialized transcript window (P0.3): the frame's row count changed
    /// at the TOP (blocks materialized/released), and unlike a live append
    /// that must NOT move the view, the offset-from-the-end is invariant
    /// under such a change. Clearing the baseline makes the next
    /// [`TuiCore::render`] trust the caller's [`TuiCore::set_scroll_offset`]
    /// instead of compensating against the previous frame's length.
    pub fn resync_window(&mut self) {
        self.last_transcript_end = None;
    }

    /// Report the dock's height so the windowed scroll path can keep the
    /// editor/footer visible while the transcript scrolls (set every frame
    /// from the event loop).
    pub fn set_dock_height(&mut self, height: usize) {
        self.dock_height = height;
    }

    /// Install the shared editor-cursor cell (written by the dock component
    /// during each frame's component render).
    pub fn set_dock_cursor_cell(
        &mut self,
        cell: std::sync::Arc<std::sync::Mutex<Option<(usize, usize)>>>,
    ) {
        self.dock_cursor_cell = Some(cell);
    }

    /// `showHardwareCursor` (settings / `PI_HARDWARE_CURSOR=1`): when true
    /// the terminal cursor is moved to the editor cell after every frame
    /// (IME candidate positioning, upstream `positionHardwareCursor`); when
    /// false (default) it is hidden and the editor's reverse-video cell is
    /// the visible cursor.
    pub fn set_show_hardware_cursor(&mut self, show: bool) {
        self.show_hardware_cursor = show;
    }

    /// Whether the terminal cursor is currently shown.
    pub fn cursor_visible(&self) -> bool {
        self.cursor_visible
    }

    /// Install the per-frame decoration hook (search highlight, selection,
    /// scrollbar, flash — see [`super::altscreen_search`]).
    pub fn set_decorator(&mut self, decorator: Option<FrameDecorator>) {
        self.decorator = decorator;
    }

    /// The last frame's undecorated lines (search corpus / selection source).
    /// Borrowed: callers must not hold it across a `&mut TuiCore` call.
    pub fn last_frame_lines(&self) -> &[String] {
        &self.last_frame
    }

    /// The lines actually painted last frame (post-decoration), one per
    /// terminal row — the transcript dump for `fullscreenExitOutput=transcript`.
    pub fn last_screen_lines(&self) -> Vec<String> {
        self.previous_lines.clone()
    }

    /// Absolute buffer row painted at the top of the visible transcript area
    /// (screen-row → buffer-row mapping for mouse selection).
    pub fn visible_top(&self) -> usize {
        self.last_window_start
    }

    /// How the last frame was painted (see [`PaintedLayout`]).
    pub fn painted_layout(&self) -> PaintedLayout {
        self.painted_layout
    }

    /// [`RowsWork`] of the last [`TuiCore::render`]'s component canvas: rows
    /// claimed, bytes copied from component caches, and bytes served by the
    /// exact-content reuse path. This core's own measurement — safe to read in
    /// a parallel test binary (unlike the process-wide
    /// [`RowsWork::snapshot`]).
    pub fn last_frame_rows_work(&self) -> RowsWork {
        self.last_rows_work
    }

    /// Map a screen row (0-based from the top of the terminal) to the absolute
    /// buffer row whose undecorated line is shown there; `None` for a row
    /// below the painted content (a frame shorter than the screen).
    ///
    /// This is the mouse family's screen → buffer mapping: the transcript
    /// rows resolve through the painted window and the dock rows (editor,
    /// footer, dialogs) resolve through [`PaintedLayout::transcript_end`], so
    /// a drag that reaches "the content below the transcript" selects exactly
    /// the lines on screen and [`last_frame_lines`](Self::last_frame_lines)
    /// holds their text.
    pub fn screen_row_to_buffer_row(&self, screen_row: usize) -> Option<usize> {
        let layout = self.painted_layout;
        if layout.frame_len == 0 {
            return None;
        }
        if screen_row < layout.window_len {
            let row = layout.window_start + screen_row;
            return if row < layout.frame_len {
                Some(row)
            } else {
                None
            };
        }
        let dock_row = screen_row - layout.window_len;
        if dock_row >= layout.dock_len {
            return None;
        }
        let row = layout.transcript_end + dock_row;
        if row < layout.frame_len {
            Some(row)
        } else {
            None
        }
    }

    /// Rows of the last undecorated frame (0 before the first render).
    pub fn frame_buffer_length(&self) -> usize {
        self.last_frame.len()
    }

    /// Monotonic frame counter; the search layer keys its match cache on it.
    pub fn frame_version(&self) -> u64 {
        self.frame_version
    }

    /// Inclusive row range of the last frame whose raw content changed since
    /// the previous render. `None` = the frames were byte-identical (after a
    /// frame-length change the whole frame is reported). The search cache uses
    /// this to avoid fingerprinting the whole band on every frame.
    pub fn last_frame_changed_rows(&self) -> Option<(usize, usize)> {
        self.last_frame_changed_rows
    }

    /// Test/benchmark seam: `(rows, bytes)` of the last undecorated frame.
    /// `bytes` is the sum of the row payloads (ANSI included), the frame's
    /// resident size proxy; a 100k-line transcript test pins the bounded
    /// window's frame against it.
    pub fn last_frame_stats(&self) -> (usize, usize) {
        (
            self.last_frame.len(),
            self.last_frame.iter().map(String::len).sum(),
        )
    }

    /// Apply the decoration hook to `lines` (no-op when none installed).
    fn apply_decorator(&mut self, lines: &mut Vec<String>, context: FrameDecorateContext) {
        if let Some(decorator) = &self.decorator {
            decorator(lines, context);
        }
    }

    /// Build the raw (undecorated) frame off the recycle pool: components
    /// write their rows into `frame_scratch` — a component that has not
    /// migrated to [`Component::render_into`] bridges its `render` lines into
    /// the same canvas, so every path yields the identical lines. The pool is
    /// refilled wherever a frame buffer is retired (see the `frame_scratch`
    /// assignments in [`TuiCore::render`]).
    fn frame_lines(&mut self, width: usize) -> Vec<String> {
        let mut scratch = std::mem::take(&mut self.frame_scratch);
        {
            let mut rows = Rows::new(&mut scratch);
            rows.rewind();
            for component in self
                .content
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .iter()
            {
                component.render_into(width, &mut rows);
            }
            rows.trim();
            self.last_rows_work = rows.work();
            FRAME_WORK
                .rebuilt_rows
                .fetch_add(rows.len() as u64, Ordering::Relaxed);
        }
        scratch
    }

    /// Upstream `SEGMENT_RESET` (tui.ts:423), applied per line by
    /// `applyLineResets` (tui-main-screen.ts:275). The diff renderer erases
    /// rows with `2K`, which clears cells but NOT attributes, so a line
    /// ending with an open SGR run (or an OSC-8 hyperlink) would bleed its
    /// style into the next row's clear — visible as colored/underlined
    /// leftovers after content shrinks or streams.
    ///
    /// The reset is LAZY (this lane): it is appended to a row at the moment
    /// the frame writes that row, never to rows the frame did not write. A
    /// row that is not written keeps the baseline bytes it was written with —
    /// reset included — so the terminal never needs a whole-frame pass, the
    /// unpainted rows are no longer re-touched every frame, and
    /// [`FrameWork::reset_rows`] equals the frame's WRITTEN row count.
    fn append_line_reset(line: &mut String) {
        line.push_str(SEGMENT_RESET);
    }

    /// Clamp one row to `width` visible cells (upstream alt-screen's
    /// `sliceByColumn(line, 0, width, true)`).
    ///
    /// A row wider than the terminal wraps onto a second physical line. The
    /// renderer's next `\r\n` then moves one row too far, so every repaint
    /// scrolls the viewport — the reported "screen keeps drifting upward".
    /// No component should emit such a row (they pad/wrap to width), but a
    /// factory renderer, decorator or a width mis-measurement can; the write
    /// paths clamp as a guard instead of letting one bad row corrupt the
    /// whole frame.
    fn clamp_row_width(line: &mut String, width: usize) {
        if width == 0 {
            line.clear();
            return;
        }
        // Image payload rows are escape-sequence framing, not text: their
        // visible width is meaningless here (upstream skips `isImageLine` in
        // the same clamp).
        if super::terminal_image::is_image_line(line) {
            return;
        }
        if ansi::visible_width(line.as_str()) > width {
            *line = ansi::truncate_to_width(line, width, "");
        }
    }

    /// Publish a frame branch's written-row count to [`FrameWork::reset_rows`]
    /// (one relaxed add of the branch's locally counted rows).
    fn count_reset_rows(rows: u64) {
        FRAME_WORK.reset_rows.fetch_add(rows, Ordering::Relaxed);
    }

    /// Append the hardware-cursor sync for one written frame (upstream
    /// `positionHardwareCursor`, tui-main-screen.ts:624-655): the cursor is
    /// ALWAYS moved to the focused component's cell — the terminal's cursor
    /// is the IME candidate-window anchor even while it is hidden — and
    /// `showHardwareCursor` only decides whether it is left visible. Returns
    /// the buffer row the hard cursor ends on when the sync moved it —
    /// callers store that as `hardware_cursor_row`, keeping the next frame's
    /// relative moves anchored. `absolute` addresses the cell with CUP (the
    /// scrolled path, where the frame IS the screen and the written frame's
    /// end position is not tracked); otherwise a relative move from
    /// `from_row` is used.
    ///
    /// This is load-bearing for Windows IME: when the cursor is merely
    /// hidden and never positioned, the terminal anchors the candidate
    /// window at the last byte the frame wrote (the footer's end), and any
    /// cursor movement the IME/terminal performs leaves `hardware_cursor_row`
    /// stale — the next frame's relative moves then start from the wrong
    /// row.
    fn append_cursor_sync(
        &mut self,
        out: &mut String,
        from_row: usize,
        total_lines: usize,
        viewport_top: usize,
        absolute: bool,
    ) -> Option<usize> {
        if let Some((row_in_dock, col)) = self.dock_cursor_cell.as_ref().and_then(|cell| {
            *cell
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
        }) {
            // The dock bottoms out the frame; its rows start at
            // `total - dock_height`. Clamp into the written frame (a
            // stale dock height on the first frame must not address a
            // row that was never painted).
            let dock = self.dock_height.min(total_lines);
            let target = (total_lines.saturating_sub(dock) + row_in_dock)
                .min(total_lines.saturating_sub(1))
                .max(viewport_top.min(total_lines.saturating_sub(1)));
            if absolute {
                out.push_str(&format!("\x1b[{};{}H", target + 1, col + 1));
            } else {
                let delta = target as isize - from_row as isize;
                if delta > 0 {
                    out.push_str(&format!("\x1b[{delta}B"));
                } else if delta < 0 {
                    out.push_str(&format!("\x1b[{}A", -delta));
                }
                out.push_str(&format!("\x1b[{}G", col + 1));
            }
            if self.show_hardware_cursor {
                if !self.cursor_visible {
                    out.push_str("\x1b[?25h");
                    self.cursor_visible = true;
                }
            } else if self.cursor_visible {
                out.push_str("\x1b[?25l");
                self.cursor_visible = false;
            }
            return Some(target);
        }
        if self.cursor_visible {
            out.push_str("\x1b[?25l");
            self.cursor_visible = false;
        }
        None
    }

    /// Render one frame with upstream diff rules (pi-tui
    /// `tui-main-screen.ts` doRender). The frame lives on the main screen:
    /// once it is taller than the terminal the screen scrolls naturally, so
    /// a buffer row's screen position is `row - viewport_top`. Changed rows
    /// are reached with relative cursor moves computed against the tracked
    /// hardware-cursor row (absolute row addressing silently corrupts the
    /// screen after the first scroll); a change above the scrolled-away
    /// viewport, or a width/height change, forces a full repaint.
    pub fn render(&mut self) {
        if self.stopped {
            return;
        }
        // A dropped writer frame never reached the terminal. The diff
        // baseline (`previous_lines`) describes the last frame BUILT, not what
        // the screen shows, so diffing against it would skip the dropped
        // frame's rows forever. Force one full repaint instead.
        let drop_epoch = self.terminal.drop_epoch();
        if drop_epoch != self.seen_drop_epoch {
            self.seen_drop_epoch = drop_epoch;
            self.invalidate_frame();
        }
        // Send the OSC 7501 support query with the first frame (upstream sends
        // it during start(); the negotiation itself runs at most once).
        self.terminal.begin_program_status_negotiation();
        let width = self.terminal.columns();
        let height = self.terminal.rows().max(1);
        let width_changed = self.previous_width != 0 && self.previous_width != width;
        let height_changed = self.previous_height != 0 && self.previous_height != height;
        // Buffer length of the previous frame (upstream `previousBufferLength`).
        let previous_buffer_length = if self.previous_height > 0 {
            self.previous_viewport_top + self.previous_height
        } else {
            height
        };
        let mut prev_viewport_top = if height_changed {
            previous_buffer_length.saturating_sub(height)
        } else {
            self.previous_viewport_top
        };
        let mut viewport_top = prev_viewport_top;
        let mut hardware_cursor_row = self.hardware_cursor_row;
        let new_lines = self.frame_lines(width);

        // Snapshot the undecorated frame for the alt-screen interaction layer
        // (search corpus, selection extraction) and bump the frame version so
        // the search cache can skip unchanged frames.
        //
        // The fresh frame is MOVED into the snapshot slot; the buffer it
        // displaces (the previous frame's raw rows) becomes this frame's work
        // buffer, which the decorator and diff paths use. The two frame pools
        // therefore swap roles every frame — snapshot this frame, work buffer
        // next frame — and neither is ever re-allocated per row.
        //
        // Only rows whose raw content actually changed are copied into the
        // work buffer (`clone_from` reuses each `String`'s allocation). An
        // unchanged row already holds the exact bytes the snapshot wants (it
        // is the same row of the previous raw frame), so it is reused in
        // place. The old code copied every row of the frame here — an
        // O(frame bytes) memcpy on top of the diff scan, whole-transcript
        // sized in regular mode — while a steady frame now copies zero rows.
        // The equality test costs the same O(frame) scan the diff pass below
        // already pays, but writes nothing and allocates nothing.
        self.frame_version = self.frame_version.wrapping_add(1);
        let new_frame_len = new_lines.len();
        let mut new_lines = std::mem::replace(&mut self.last_frame, new_lines);
        let old_frame_len = new_lines.len();
        new_lines.resize_with(self.last_frame.len(), String::new);
        // The counters take one relaxed add of the locally accumulated values
        // (see [`FrameWork`]); they count actual row copies, not rows visited.
        let mut snapshot_rows = 0u64;
        let mut snapshot_bytes = 0u64;
        let mut changed_rows: Option<(usize, usize)> = None;
        for (index, (line, snapshot)) in
            new_lines.iter_mut().zip(self.last_frame.iter()).enumerate()
        {
            if line == snapshot {
                continue;
            }
            line.clone_from(snapshot);
            snapshot_rows += 1;
            snapshot_bytes += snapshot.len() as u64;
            changed_rows = Some(match changed_rows {
                Some((first, _)) => (first, index),
                None => (index, index),
            });
        }
        // A frame-length change realigns every row from the shorter end, so
        // the search cache must not trust a narrow range; mark the whole
        // (new) frame changed.
        if old_frame_len != new_frame_len {
            changed_rows = Some((0, new_frame_len.saturating_sub(1)));
        }
        self.last_frame_changed_rows = changed_rows;
        FRAME_WORK
            .snapshot_rows
            .fetch_add(snapshot_rows, Ordering::Relaxed);
        FRAME_WORK
            .snapshot_bytes
            .fetch_add(snapshot_bytes, Ordering::Relaxed);

        // Viewport layout update (upstream `ScrollView.updateLayout`): the
        // frame's transcript extent decides whether a following viewport
        // re-anchors at the end or a pinned one absorbs the growth. Runs
        // before the render path is chosen, so a pinned view at offset 0 that
        // just grew switches to the windowed path in the same frame.
        let geometry =
            super::altscreen_search::viewport_geometry(new_lines.len(), self.dock_height, height);
        self.update_viewport_layout(geometry.transcript_end, geometry.max_offset);

        // Scrolled (windowed) render: paint the transcript slice `offset`
        // rows above the frame end plus the dock (upstream fullscreen
        // ScrollView scrollback with a follow-end dock). Always a full
        // repaint — the window replaces the whole screen.
        //
        // P0.3: with the windowed viewport on (the app owns the alt screen
        // and the transcript materializes only a bounded band), the FOLLOW
        // case takes the exact same path at `offset == 0`: the band frame
        // assembled by `frame_lines` is already bounded by the caller's
        // window budgets, and painting its last viewport is O(visible rows)
        // instead of O(transcript).
        if self.scroll_offset > 0 || self.windowed_viewport {
            self.render_scrolled(&new_lines, height);
            // The windowed path paints its own (scratch-recycled) buffer; the
            // full-frame copy it sliced from is dead and returns to the pool.
            self.frame_scratch = new_lines;
            return;
        }

        // Decoration pass for the follow-end path. Only visible rows are
        // decorated: the diff path cannot reach rows above the viewport
        // without a full repaint, and a change there would force one.
        let dock = self.dock_height.min(new_lines.len());
        let transcript_end = new_lines.len().saturating_sub(dock);
        self.apply_decorator(
            &mut new_lines,
            FrameDecorateContext {
                width,
                windowed: false,
                window_start: prev_viewport_top,
                dock_start: transcript_end,
                visible_top: prev_viewport_top,
                transcript_end,
                scroll_offset: 0,
            },
        );
        // No whole-frame reset pass any more: every write path appends the
        // per-line reset lazily to the rows it writes (see
        // [`TuiCore::append_line_reset`]), and the baseline keeps exactly the
        // bytes the terminal received, so change detection and the stored
        // frame still agree.

        // Full repaint + shared hardware-cursor/viewport bookkeeping
        // (upstream `fullRender`): after writing every line the cursor sits
        // on the last content row in buffer coordinates, and the viewport
        // bottoms out at the end of the (possibly taller-than-screen) frame.
        // Takes the frame by value: every caller returns right after, so the
        // written frame moves into `previous_lines` instead of being copied.
        // Every row is written here, so every row gets the lazy reset and the
        // baseline carries it.
        let full_render = |core: &mut Self, mut new_lines: Vec<String>, clear: bool| {
            let mut out = String::from("\x1b[?2026h");
            if clear {
                out.push_str("\x1b[2J\x1b[H\x1b[3J");
            } else {
                // First render onto a clean screen: the cursor is assumed to
                // be at home already (upstream comment), but emitting `H`
                // keeps this correct if it is not.
                out.push_str("\x1b[H");
            }
            for (i, line) in new_lines.iter_mut().enumerate() {
                if i > 0 {
                    out.push_str("\r\n");
                }
                out.push_str("\x1b[2K");
                Self::clamp_row_width(line, width);
                Self::append_line_reset(line);
                out.push_str(line);
            }
            Self::count_reset_rows(new_lines.len() as u64);
            let buffer_length = height.max(new_lines.len());
            let viewport_top = buffer_length.saturating_sub(height);
            let moved = core.append_cursor_sync(
                &mut out,
                new_lines.len().saturating_sub(1),
                new_lines.len(),
                viewport_top,
                false,
            );
            out.push_str("\x1b[?2026l");
            core.terminal.write_owned(out);
            let last_row = new_lines.len().saturating_sub(1);
            let frame_len = new_lines.len();
            // The written frame becomes the baseline; the baseline it retires
            // is dead and goes back to the raw-frame pool. The retired rows
            // carry the lazy per-line reset this path appended — strip it so
            // the pool holds the raw content the next frame's exact-content
            // compare expects (otherwise every row would look changed again).
            let mut retired = std::mem::replace(&mut core.previous_lines, new_lines);
            for row in &mut retired {
                if row.ends_with(SEGMENT_RESET) {
                    let raw_len = row.len() - SEGMENT_RESET.len();
                    row.truncate(raw_len);
                }
            }
            core.frame_scratch = retired;
            core.previous_width = width;
            core.previous_height = height;
            core.hardware_cursor_row = moved.unwrap_or(last_row);
            core.previous_viewport_top = viewport_top;
            core.last_window_start = core.previous_viewport_top;
            core.publish_follow_layout(frame_len);
        };

        if self.previous_lines.is_empty() && !width_changed && !height_changed {
            // `invalidate_frame` was called (overlay swapped the alternate
            // screen out): rows below the new content are stale even though
            // the renderer has no cached frame, so the first repaint must
            // clear.
            let clear = std::mem::take(&mut self.force_clear);
            full_render(self, new_lines, clear);
            return;
        }
        if width_changed || height_changed {
            full_render(self, new_lines, true);
            return;
        }

        // Find first and last changed lines. The scan covers both sides of
        // the frame (`max_lines`), so an unchanged 3000-row frame still costs
        // 3000 compares; the counter pins that (see [`FrameWork`]). The
        // baseline rows carry the lazy per-line reset (see
        // [`TuiCore::append_line_reset`]), so the compare strips that suffix:
        // a row that is unchanged modulo the reset is NOT a change (the reset
        // is a write-time artifact, not content).
        let max_lines = new_lines.len().max(self.previous_lines.len());
        let mut first_changed: isize = -1;
        let mut last_changed: isize = -1;
        for i in 0..max_lines {
            let old = self.previous_lines.get(i).map(String::as_str).unwrap_or("");
            let new = new_lines.get(i).map(String::as_str).unwrap_or("");
            let old = old.strip_suffix(SEGMENT_RESET).unwrap_or(old);
            if old != new {
                if first_changed == -1 {
                    first_changed = i as isize;
                }
                last_changed = i as isize;
            }
        }
        FRAME_WORK
            .diff_scanned_rows
            .fetch_add(max_lines as u64, Ordering::Relaxed);
        let appended_lines = new_lines.len() > self.previous_lines.len();
        if appended_lines {
            if first_changed == -1 {
                first_changed = self.previous_lines.len() as isize;
            }
            last_changed = new_lines.len() as isize - 1;
        }
        if first_changed == -1 {
            // Nothing changed: keep the frame cache and refresh only the
            // viewport bookkeeping (upstream no-changes path). The unchanged
            // copy is dead — park it in the raw-frame pool.
            let frame_len = new_lines.len();
            self.frame_scratch = new_lines;
            self.previous_viewport_top = prev_viewport_top;
            self.previous_height = height;
            // Upstream's no-changes branch still calls
            // `positionHardwareCursor`: the terminal can move the cursor
            // without producing input (Windows IME composition/candidate UI),
            // so re-anchor it to the focused cell even when no row changed.
            // Otherwise the next frame's relative moves start from a stale
            // `hardware_cursor_row` and the frame drifts.
            let mut cursor_out = String::new();
            let moved = self.append_cursor_sync(
                &mut cursor_out,
                frame_len.saturating_sub(1),
                frame_len,
                prev_viewport_top,
                false,
            );
            if !cursor_out.is_empty() {
                self.terminal.write_owned(cursor_out);
            }
            if let Some(row) = moved {
                self.hardware_cursor_row = row;
            }
            self.publish_follow_layout(frame_len);
            return;
        }
        let first_changed = first_changed as usize;

        // All changes are in deleted lines: clear the extra rows below the
        // new content without scrolling, then move back (upstream branch).
        if first_changed >= new_lines.len() {
            if self.previous_lines.len() > new_lines.len() {
                let target_row = new_lines.len().saturating_sub(1);
                if target_row < prev_viewport_top {
                    // The end of the new content scrolled away: the removed
                    // tail is not reachable on screen.
                    full_render(self, new_lines, true);
                    return;
                }
                let extra_lines = self.previous_lines.len() - new_lines.len();
                if extra_lines > height {
                    full_render(self, new_lines, true);
                    return;
                }
                let mut out = String::from("\x1b[?2026h");
                // Move to the last new row (relative move; both rows are in
                // buffer coordinates).
                let line_diff = (target_row as isize - viewport_top as isize)
                    - (hardware_cursor_row as isize - prev_viewport_top as isize);
                if line_diff > 0 {
                    out.push_str(&format!("\x1b[{line_diff}B"));
                } else if line_diff < 0 {
                    out.push_str(&format!("\x1b[{}A", -line_diff));
                }
                out.push('\r');
                let clear_start_offset: isize = if new_lines.is_empty() { 0 } else { 1 };
                if clear_start_offset > 0 {
                    out.push_str(&format!("\x1b[{clear_start_offset}B"));
                }
                for i in 0..extra_lines {
                    out.push_str("\r\x1b[2K");
                    if i < extra_lines - 1 {
                        out.push_str("\x1b[1B");
                    }
                }
                let move_back = (extra_lines as isize - 1 + clear_start_offset).max(0);
                if move_back > 0 {
                    out.push_str(&format!("\x1b[{move_back}A"));
                }
                let moved = self.append_cursor_sync(
                    &mut out,
                    target_row,
                    new_lines.len(),
                    prev_viewport_top,
                    false,
                );
                out.push_str("\x1b[?2026l");
                self.terminal.write_owned(out);
                self.hardware_cursor_row = moved.unwrap_or(target_row);
                // No content row is written on this branch (only the extra
                // rows below the new end are cleared), so no reset is
                // appended: the surviving baseline rows keep the bytes they
                // were written with. The raw frame returns to the pool.
                self.previous_lines.truncate(new_lines.len());
                let frame_len = new_lines.len();
                self.frame_scratch = new_lines;
                self.previous_width = width;
                self.previous_height = height;
                self.previous_viewport_top = prev_viewport_top;
                self.publish_follow_layout(frame_len);
            }
            return;
        }

        // Differential rendering can only touch rows that were actually
        // visible; a change above the previous viewport needs a full repaint
        // (upstream guard).
        if first_changed < prev_viewport_top {
            full_render(self, new_lines, true);
            return;
        }

        let render_end = (last_changed as usize).min(new_lines.len() - 1);
        // The baseline keeps one row per frame row: rows the frame grew past
        // the old end are appended here (every appended row sits inside the
        // written range below), and rows below a shrunk frame's end are
        // dropped after the screen cleaned them up.
        let previous_len = self.previous_lines.len();
        self.previous_lines
            .resize_with(new_lines.len(), String::new);
        // Write the rows this path repaints into the baseline as well — they
        // are exactly the bytes the terminal receives, lazy reset included —
        // and leave every row outside the written range untouched (its screen
        // bytes ARE the baseline bytes of the frame that wrote it).
        // `clone_from` reuses each baseline row's allocation, so a one-row
        // spinner tick copies one row, not the whole frame.
        for i in first_changed..=render_end {
            Self::clamp_row_width(&mut new_lines[i], width);
            self.previous_lines[i].clone_from(&new_lines[i]);
            Self::append_line_reset(&mut self.previous_lines[i]);
        }
        Self::count_reset_rows((render_end - first_changed + 1) as u64);
        let append_start = appended_lines && first_changed == previous_len && first_changed > 0;
        let prev_viewport_bottom = prev_viewport_top + height - 1;
        let move_target_row = if append_start {
            first_changed - 1
        } else {
            first_changed
        };

        let mut out = String::from("\x1b[?2026h");
        if move_target_row > prev_viewport_bottom {
            // The target row sits below the old viewport: scroll the screen
            // down to it first (upstream scroll-by-`\r\n` branch).
            let current_screen_row = (hardware_cursor_row as isize - prev_viewport_top as isize)
                .clamp(0, height as isize - 1) as usize;
            let move_to_bottom = height - 1 - current_screen_row;
            if move_to_bottom > 0 {
                out.push_str(&format!("\x1b[{move_to_bottom}B"));
            }
            let scroll = move_target_row - prev_viewport_bottom;
            out.push_str(&"\r\n".repeat(scroll));
            prev_viewport_top += scroll;
            viewport_top += scroll;
            hardware_cursor_row = move_target_row;
        }

        // Move the cursor to the row the repaint starts at (relative move
        // from the tracked hardware position; both are buffer rows).
        let line_diff = (move_target_row as isize - viewport_top as isize)
            - (hardware_cursor_row as isize - prev_viewport_top as isize);
        if line_diff > 0 {
            out.push_str(&format!("\x1b[{line_diff}B"));
        } else if line_diff < 0 {
            out.push_str(&format!("\x1b[{}A", -line_diff));
        }
        if append_start {
            out.push_str("\r\n");
        } else {
            out.push('\r');
        }

        // Repaint only the changed range (reduces flicker when a single line
        // changes, e.g. the spinner). The rows come from the baseline, which
        // holds byte-identical content plus this frame's lazy reset.
        for i in first_changed..=render_end {
            if i > first_changed {
                out.push_str("\r\n");
            }
            out.push_str("\x1b[2K");
            out.push_str(&self.previous_lines[i]);
        }

        // Track where the cursor ended up after rendering.
        let mut final_cursor_row = render_end;
        // If the frame shrank, clear the leftover rows below the new content
        // and move back to the end of the new content (upstream shrink
        // cleanup inside the diff path). `previous_len` is the baseline
        // length BEFORE this frame's write (the rows still on screen).
        if previous_len > new_lines.len() {
            if render_end < new_lines.len() - 1 {
                let move_down = new_lines.len() - 1 - render_end;
                out.push_str(&format!("\x1b[{move_down}B"));
                final_cursor_row = new_lines.len() - 1;
            }
            let extra_lines = previous_len - new_lines.len();
            for _ in new_lines.len()..previous_len {
                out.push_str("\r\n\x1b[2K");
            }
            out.push_str(&format!("\x1b[{extra_lines}A"));
        }

        let new_viewport_top = prev_viewport_top.max(final_cursor_row.saturating_sub(height - 1));
        let moved = self.append_cursor_sync(
            &mut out,
            final_cursor_row,
            new_lines.len(),
            new_viewport_top,
            false,
        );
        out.push_str("\x1b[?2026l");
        self.terminal.write_owned(out);
        self.hardware_cursor_row = moved.unwrap_or(final_cursor_row);
        self.previous_viewport_top = new_viewport_top;
        // The baseline stays in place (it holds the live rows for the next
        // diff, reset included); the rows the frame shrank past are dropped,
        // and the raw frame returns to the pool.
        self.previous_lines.truncate(new_lines.len());
        let frame_len = new_lines.len();
        self.frame_scratch = new_lines;
        self.previous_width = width;
        self.previous_height = height;
        self.last_window_start = self.previous_viewport_top;
        self.publish_follow_layout(frame_len);
    }

    /// Publish the follow path's painted layout for the mouse mapping (see
    /// [`PaintedLayout`]). The follow frame writes the whole undecorated frame
    /// with its top row at `last_window_start`, so every on-screen row's
    /// buffer index is `last_window_start + screen_row` — the dock rows
    /// included, which is what makes the editor/footer rows selectable.
    ///
    /// `window_len` counts the TRANSCRIPT rows only (`transcript_end` may sit
    /// below `window_start` when the frame is shorter than the screen, in
    /// which case the window starts inside the dock and the dock's own rows do
    /// not follow it); the dock is always the last `dock_len` frame rows, so a
    /// screen row past the painted frame maps to `None` instead of wrapping
    /// into the dock.
    fn publish_follow_layout(&mut self, frame_len: usize) {
        let dock = self.dock_height.min(frame_len);
        let transcript_end = frame_len.saturating_sub(dock);
        let window_start = self.last_window_start.min(transcript_end);
        self.painted_layout = PaintedLayout {
            windowed: false,
            window_start,
            window_len: transcript_end.saturating_sub(window_start),
            transcript_end,
            dock_len: dock,
            frame_len,
        };
    }

    /// Publish the scrolled path's painted layout: a transcript window
    /// (`window_start`, `window_len` rows) with the dock painted directly
    /// below it, so screen rows past `window_len` map to
    /// `transcript_end + (row - window_len)`.
    fn publish_scrolled_layout(
        &mut self,
        frame_len: usize,
        window_start: usize,
        window_len: usize,
        dock_len: usize,
    ) {
        self.painted_layout = PaintedLayout {
            windowed: true,
            window_start,
            window_len,
            transcript_end: frame_len.saturating_sub(dock_len),
            dock_len,
            frame_len,
        };
    }

    /// The scrolled render path: paint `[transcript_end - offset - viewport_h,
    /// transcript_end - offset)` plus the dock as a full repaint (upstream
    /// fullscreen mode paints the ScrollView window over the static dock).
    /// The offset is first compensated for transcript growth (see
    /// [`compensated_scroll_offset`]) so the viewport stays pinned while the
    /// stream appends.
    fn render_scrolled(&mut self, new_lines: &[String], height: usize) {
        let width = self.terminal.columns();
        // Same geometry `render` used for the viewport update
        // (`viewport_geometry`): the offset is already compensated for this
        // frame, so only the bounds clamp is left.
        let geometry =
            super::altscreen_search::viewport_geometry(new_lines.len(), self.dock_height, height);
        let transcript_end = geometry.transcript_end;
        let viewport_h = geometry.viewport_height;
        let max = geometry.max_offset;
        let offset = self.scroll_offset.min(max);
        let end = transcript_end.saturating_sub(offset);
        let start = end.saturating_sub(viewport_h);

        // Recycle the scratch buffer (the baseline `previous_lines` retired
        // one scrolled frame earlier) instead of allocating a fresh painted
        // frame every frame: `clone_from` reuses each `String`'s allocation,
        // and every slot below is overwritten, so the pool's stale contents
        // never reach the frame.
        let mut painted = std::mem::take(&mut self.scrolled_scratch);
        let window = &new_lines[start..end];
        let dock = &new_lines[transcript_end..];
        painted.resize_with(window.len() + dock.len(), String::new);
        for (line, source) in painted.iter_mut().zip(window.iter().chain(dock.iter())) {
            line.clone_from(source);
        }
        let dock_start = window.len();

        self.apply_decorator(
            &mut painted,
            FrameDecorateContext {
                width,
                windowed: true,
                window_start: start,
                dock_start,
                visible_top: 0,
                transcript_end,
                scroll_offset: offset,
            },
        );
        // The alt screen paints a fixed `height`-row viewport with no scroll
        // margin: one over-wide row would wrap and scroll every later frame
        // upward (upstream truncates here before diffing). Clamp after the
        // decorator so overlays/scrollbars compose into the same guard.
        for row in painted.iter_mut() {
            Self::clamp_row_width(row, width);
        }
        // No whole-window reset pass: the write branches below append the
        // per-line reset to the rows they actually write (see
        // [`TuiCore::append_line_reset`]), and the diff branch keeps the
        // baseline rows (reset included) for every row it does not write.

        // Line-level diff against the previous frame (upstream
        // tui-alt-screen.ts doRender): the scrolled window is a fixed
        // full-screen viewport, so a row's screen position equals its index
        // in `painted` and changed rows are reached with absolute addressing
        // (`ESC[{row};1H`), matching upstream's per-row diff. A full repaint
        // is kept whenever the previous frame's rows do not line up with the
        // screen: a follow-mode frame taller than the terminal scrolls away
        // (its `previous_viewport_top` is > 0, which also makes its line
        // count differ from the window), and width/height/invalidate changes
        // re-anchor the screen. In every diffable case the previous frame is
        // itself top-anchored (a scrolled frame or a follow frame that fit
        // the screen), so the mapping painted[i] <-> screen row i holds for
        // both frames.
        // The baseline is read before the end-of-frame buffer swap below, and
        // lives in a different buffer than `painted` (the scratch pool never
        // aliases `previous_lines`).
        let prev_len = self.previous_lines.len();
        let diffable = !painted.is_empty()
            && self.previous_width == width
            && self.previous_height == height
            && prev_len == painted.len();
        // The scrolled frame IS the screen: the dock bottoms it out, so the
        // hardware cursor sync addresses its cell row directly (CUP).
        let from_row = painted.len().saturating_sub(1);
        let total = painted.len();
        let moved;
        if diffable {
            // Reset-aware compare against the baseline (whose rows carry the
            // lazy reset of the frame that wrote them): rows that match are
            // NOT rewritten and keep their baseline bytes; changed rows get
            // the reset and are carried into the baseline, exactly the bytes
            // the diff output writes.
            let mut changed: Vec<usize> = Vec::new();
            for (index, row) in painted.iter_mut().enumerate() {
                let baseline = &self.previous_lines[index];
                if baseline.strip_suffix(SEGMENT_RESET).unwrap_or(baseline) == *row {
                    continue;
                }
                changed.push(index);
                Self::append_line_reset(row);
                self.previous_lines[index].clone_from(row);
            }
            // `scrolled_diff_output` unconditionally rewrites the last row
            // (cursor anchoring), so that row is always written too — with
            // the reset even when unchanged (its baseline row already carries
            // the identical content + reset).
            let last = painted.len() - 1;
            let last_unchanged = changed.last() != Some(&last);
            if last_unchanged {
                Self::append_line_reset(&mut painted[last]);
            }
            Self::count_reset_rows(changed.len() as u64 + u64::from(last_unchanged));
            let mut out = scrolled_diff_output(&painted, &changed);
            moved = self.append_cursor_sync(&mut out, from_row, total, 0, true);
            self.terminal.write_owned(out);
            // The window copy is dead; the baseline stays in place.
            self.scrolled_scratch = painted;
        } else {
            // Every row is written: every row gets the lazy reset, and the
            // painted window becomes the baseline.
            for row in painted.iter_mut() {
                Self::append_line_reset(row);
            }
            Self::count_reset_rows(painted.len() as u64);
            let mut out = String::new();
            if std::mem::take(&mut self.force_clear) {
                out.push_str("\x1b[2J\x1b[H\x1b[3J");
            }
            out.push_str(&scrolled_full_output(&painted));
            moved = self.append_cursor_sync(&mut out, from_row, total, 0, true);
            self.terminal.write_owned(out);
            // Park the retired baseline in the recycle pool (the buffer that
            // was `previous_lines` two frames ago becomes this frame's
            // scratch); the live baseline and the scratch stay distinct
            // buffers.
            self.scrolled_scratch = std::mem::replace(&mut self.previous_lines, painted);
        }
        self.previous_width = width;
        self.previous_height = height;
        self.hardware_cursor_row = moved.unwrap_or(from_row);
        // The window bottoms out at `height` rows; the follow-end bookkeeping
        // resumes from a full repaint when the offset returns to 0.
        self.previous_viewport_top = 0;
        self.last_window_start = start;
        self.publish_scrolled_layout(new_lines.len(), start, dock_start, dock.len());
    }

    /// Request a render; coalesces to at most one frame per min_frame
    /// interval (upstream requestRender + 16ms clamp).
    pub fn request_render(&mut self) {
        let elapsed = self
            .last_render
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .elapsed();
        if elapsed >= self.min_frame {
            *self
                .last_render
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = Instant::now();
            self.render();
        }
        // else: skip; the next event loop tick re-renders anyway.
    }

    /// Drop the cached frame so the next [`TuiCore::render`] is a full
    /// repaint. Used after an overlay flow swapped the alternate screen out
    /// (e.g. the `/resume` picker) and the diff renderer's cache is stale.
    ///
    /// The repaint CLEARS first (`2J` + home + `3J`, upstream
    /// `fullRender(true)` — the exact bytes a width change emits). Without
    /// the clear the whole frame is re-written on top of the existing
    /// content: on the main screen (tuiMode regular) the earlier copy is not
    /// erased, so once the frame is taller than the viewport the re-write
    /// scrolls the frame's head — the startup header included — back into the
    /// terminal's scrollback (startup block printed twice for one app start;
    /// see the `force_clear` field doc). Only the first paint of a run
    /// (`previous_lines` empty and no `invalidate_frame`) skips the clear.
    pub fn invalidate_frame(&mut self) {
        self.previous_lines.clear();
        self.previous_width = 0;
        self.previous_height = 0;
        self.hardware_cursor_row = 0;
        self.previous_viewport_top = 0;
        // The clear contract documented on `force_clear`: this call is the
        // only way to drop the cached frame, and the caller knows rows below
        // the new content are stale.
        self.force_clear = true;
        // P1-2: the pinning baseline is meaningless for the next (full)
        // repaint — the next frame re-anchors it instead of compensating
        // against a stale transcript length.
        self.last_transcript_end = None;
    }

    pub fn stop(&mut self) {
        if self.stopped {
            return;
        }
        self.stopped = true;
        // Teardown is a synchronous direct-write path: give the async queue a
        // bounded chance to drain (the last frame must be on screen before
        // the cursor placement below), then write the stop sequences on the
        // calling thread so a congested queue cannot lose them.
        let _ = self.terminal.flush_bounded(STOP_FLUSH_TIMEOUT);
        // Remove the OSC 7501 status while stopped (upstream `stop()`); a
        // restart re-negotiates and reports again.
        self.terminal.stop_program_status();
        // Leave the output in place and move the cursor just below the
        // content so the shell prompt lands there (upstream
        // `beforeTerminalStop`: relative move from the tracked hardware row,
        // then a newline).
        if self.previous_lines.is_empty() {
            self.terminal.close_writer();
            return;
        }
        let target_row = self.previous_lines.len() as isize;
        let line_diff = target_row - self.hardware_cursor_row as isize;
        let mut out = String::from(" ");
        if line_diff > 0 {
            out.push_str(&format!("\x1b[{line_diff}B"));
        } else if line_diff < 0 {
            out.push_str(&format!("\x1b[{}A", -line_diff));
        }
        out.push_str("\r\n");
        self.terminal.write_sync(&out);
        // The cursor placement above was the last teardown write; stop the
        // writer thread so no queued frame can land after the surface restore.
        self.terminal.close_writer();
    }

    /// Width the last frame was rendered at (0 before the first render).
    /// Test-observable for the resize path (P1-5).
    pub fn last_render_width(&self) -> usize {
        self.previous_width
    }
}
