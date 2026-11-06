//! DIRTY2: dirty block/row intervals and their frame-deadline contract.
//!
//! Pins:
//! 1. a live streaming block's per-delta frame work touches only its tail
//!    rows (`dirty_rows` / `rerendered_rows` stay tail-sized while the block
//!    is hundreds of rows long);
//! 2. 100 adjacent dirty marks coalesce into one interval (`dirty_rows ==
//!    100`, `dirty_ranges_merged == 99`), both in the pure set and through
//!    the `TuiCore` producer API;
//! 3. resize and scroll fall back to a full-frame dirty row count;
//! 4. a frame that hits its row budget carries the deferred dirty ranges to
//!    the next frame and eventually paints the exact latest state — no token
//!    is lost;
//! 5. the app-level `DirtyBlockLog` coalesces repeated deltas per entry
//!    (latest state only), and `DirtyInbox` bounds its latest-wins queue.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use pir::tui::app::{ChatContainer, ChatEntry};
use pir::tui::core::{Component, DirtyInbox, DirtyRanges, Rows, Terminal, TerminalSink, TuiCore};
use pir::tui::markdown::Markdown;
use pir::tui::stdin_buffer::WriteLog;

fn test_core(width: usize, height: usize) -> TuiCore {
    let terminal = Terminal::with_sink_factory(
        || Box::new(std::io::sink()) as Box<dyn TerminalSink>,
        WriteLog::disabled(),
    );
    terminal.set_size(width, height);
    TuiCore::new(terminal)
}

/// A component with N independently revisioned blocks of `rows_per_block`
/// rows each (the transcript shape the frame block cache decomposes).
struct BlockList {
    revisions: Arc<Vec<AtomicU64>>,
    rows_per_block: usize,
}

impl Component for BlockList {
    fn render(&self, width: usize) -> Vec<String> {
        let mut lines = Vec::new();
        {
            let mut out = Rows::new(&mut lines);
            self.render_into(width, &mut out);
            out.trim();
        }
        lines
    }

    fn render_into(&self, width: usize, out: &mut Rows<'_>) {
        for index in 0..self.revisions.len() {
            self.render_block_into(index, width, out);
        }
    }

    fn content_blocks(&self) -> Option<Vec<u64>> {
        Some(
            self.revisions
                .iter()
                .map(|revision| revision.load(Ordering::Relaxed))
                .collect(),
        )
    }

    fn render_block_into(&self, index: usize, _width: usize, out: &mut Rows<'_>) {
        let revision = self.revisions[index].load(Ordering::Relaxed);
        for row in 0..self.rows_per_block {
            out.push_fmt(format_args!("block {index:02} row {row:02} rev {revision}"));
        }
    }
}

/// A single streaming Markdown block whose revision moves with every delta
/// (the live transcript tail).
struct StreamingMarkdownBlock {
    markdown: Arc<Markdown>,
    revision: Arc<AtomicU64>,
}

impl Component for StreamingMarkdownBlock {
    fn render(&self, width: usize) -> Vec<String> {
        self.markdown.render(width)
    }

    fn render_into(&self, width: usize, out: &mut Rows<'_>) {
        self.markdown.render_into(width, out);
    }

    fn content_blocks(&self) -> Option<Vec<u64>> {
        Some(vec![self.revision.load(Ordering::Relaxed)])
    }

    fn render_block_into(&self, _index: usize, width: usize, out: &mut Rows<'_>) {
        self.markdown.render_into(width, out);
    }
}

/// N deltas only repaint the streaming tail: the block grows past 200 rows,
/// yet every measured frame's dirty set and rerendered rows stay a few rows.
#[test]
fn streaming_deltas_only_touch_the_tail_dirty_rows() {
    let mut core = test_core(100, 40);
    let markdown = Arc::new(Markdown::new("", 0, 0));
    let revision = Arc::new(AtomicU64::new(0));
    core.push_content(Box::new(StreamingMarkdownBlock {
        markdown: Arc::clone(&markdown),
        revision: Arc::clone(&revision),
    }));

    for index in 0..120 {
        markdown.set_text_growing_delta(&format!("line-{index:04} body text\n\n"));
        revision.fetch_add(1, Ordering::Relaxed);
        core.render();
    }
    let block_rows = core.frame_buffer_length() as u64;
    assert!(
        block_rows >= 200,
        "fixture must grow a long streaming block: {block_rows} rows"
    );

    let mut max_dirty = 0u64;
    let mut max_rerendered = 0u64;
    for index in 120..160 {
        markdown.set_text_growing_delta(&format!("line-{index:04} body text\n\n"));
        revision.fetch_add(1, Ordering::Relaxed);
        core.render();
        let work = core.last_frame_work();
        max_dirty = max_dirty.max(work.dirty_rows);
        max_rerendered = max_rerendered.max(work.rerendered_rows);
        assert!(
            work.dirty_rows <= 8,
            "a delta may only dirty its tail interval, not the {block_rows}-row block: {work:?}"
        );
    }
    assert!(max_dirty > 0, "the measured deltas must dirty something");
    assert!(
        max_rerendered <= 4,
        "a delta may only re-render its few new final rows: {max_rerendered}"
    );
    assert!(
        max_dirty <= max_rerendered + 4,
        "the dirty interval should track the rerendered tail: dirty {max_dirty}, \
         rerendered {max_rerendered}"
    );
}

/// 100 adjacent single-row marks coalesce into one interval: one range, 100
/// distinct rows, 99 merges. The same holds through `TuiCore`'s producer API.
#[test]
fn hundred_adjacent_dirty_marks_merge_into_one_range() {
    let mut ranges = DirtyRanges::new();
    for row in 0..100 {
        ranges.mark(7, row, row + 1);
    }
    assert_eq!(ranges.len(), 1, "adjacent rows must coalesce: {ranges:?}");
    assert_eq!(ranges.rows(), 100);
    assert_eq!(ranges.merges(), 99);
    assert_eq!(ranges.marks(), 100);
    let range = ranges.ranges()[0];
    assert_eq!((range.block, range.start, range.end), (7, 0, 100));

    // Bridging two existing intervals absorbs both.
    let mut bridged = DirtyRanges::new();
    bridged.mark(2, 10, 20);
    bridged.mark(1, 0, 5);
    bridged.mark(3, 5, 10);
    assert_eq!(bridged.len(), 1);
    assert_eq!(bridged.rows(), 20);
    assert_eq!(bridged.merges(), 2);
    assert_eq!(bridged.ranges()[0].block, 1);

    // Producer staging area: N marks for one block are latest-wins coalesced.
    let mut core = test_core(80, 20);
    for row in 0..100 {
        core.mark_dirty_rows(7, row, row + 1);
    }
    assert_eq!(core.pending_dirty_ranges(), 1);
    assert_eq!(core.pending_dirty_rows(), 100);
    assert_eq!(core.pending_dirty_merges(), 99);
    assert!(core.has_pending_dirty());
}

/// Width resize and a scrolled viewport both publish a full-frame dirty row
/// count (every painted row is rewritten by those paths).
#[test]
fn resize_and_scroll_fall_back_to_full_frame_dirty_rows() {
    let revisions = Arc::new((0..4).map(|_| AtomicU64::new(0)).collect::<Vec<_>>());
    let mut core = test_core(80, 20);
    core.push_content(Box::new(BlockList {
        revisions,
        rows_per_block: 5,
    }));
    for _ in 0..2 {
        core.render();
    }
    let full_rows = core.frame_buffer_length() as u64;
    assert!(full_rows > 0);

    // Width resize: the full-repaint path writes every row.
    core.terminal().set_size(60, 20);
    core.render();
    let resized = core.last_frame_work();
    assert_eq!(
        resized.dirty_rows,
        core.frame_buffer_length() as u64,
        "a width resize must report the full frame dirty: {resized:?}"
    );
    assert_eq!(resized.dirty_ranges_merged, 0);

    // Scrolled viewport: first scrolled frame is a full repaint of the
    // painted window.
    let revisions = Arc::new((0..20).map(|_| AtomicU64::new(0)).collect::<Vec<_>>());
    let mut scrolled = test_core(80, 10);
    scrolled.push_content(Box::new(BlockList {
        revisions,
        rows_per_block: 5,
    }));
    scrolled.render();
    let follow_rows = scrolled.frame_buffer_length();
    assert!(follow_rows > 10, "fixture must outgrow the viewport");
    scrolled.set_scroll_offset(5);
    scrolled.render();
    let painted_rows = scrolled.last_screen_lines().len() as u64;
    let work = scrolled.last_frame_work();
    assert_eq!(
        work.dirty_rows, painted_rows,
        "a scrolled full repaint must report every painted row dirty: {work:?}"
    );
    assert_eq!(work.dirty_ranges_merged, 0);
}

/// A frame that hits its row budget keeps the deferred ranges pending; the
/// next frames finish them and the final frame is byte-identical to a cold
/// render of the latest state.
#[test]
fn frame_row_budget_defers_without_losing_tokens() {
    let revisions = Arc::new((0..12).map(|_| AtomicU64::new(0)).collect::<Vec<_>>());
    let mut core = test_core(80, 20);
    core.push_content(Box::new(BlockList {
        revisions: Arc::clone(&revisions),
        rows_per_block: 4,
    }));
    for _ in 0..2 {
        core.render();
    }

    for revision in revisions.iter() {
        revision.store(1, Ordering::Relaxed);
    }
    core.set_frame_row_budget(20);
    let mut frames = 0;
    let mut saw_deferred = false;
    loop {
        core.render();
        frames += 1;
        if core.has_pending_dirty() {
            saw_deferred = true;
            // A change above the viewport takes the renderer's full-repaint
            // branch, so its dirty_rows is the whole frame; the deferred-work
            // proof is the builder's rerendered row count, which must stay
            // within one budget plus one block.
            assert!(
                core.last_frame_work().rerendered_rows <= 24,
                "a budgeted frame may rebuild one block past its budget at most: {:?}",
                core.last_frame_work()
            );
        } else {
            break;
        }
        assert!(frames < 20, "deferred dirty ranges must drain");
    }
    assert!(saw_deferred, "the fixture must actually defer a range");
    assert!(frames > 1);
    assert!(!core.has_pending_dirty());

    // The state the user ends on is the latest one: compare against a cold
    // render built from the final revisions.
    let cold_revisions = Arc::new((0..12).map(|_| AtomicU64::new(1)).collect::<Vec<_>>());
    let mut cold = test_core(80, 20);
    cold.push_content(Box::new(BlockList {
        revisions: cold_revisions,
        rows_per_block: 4,
    }));
    cold.render();
    assert_eq!(
        core.last_frame_lines(),
        cold.last_frame_lines(),
        "a budgeted multi-frame repaint must converge to the exact latest frame"
    );
}

/// App layer: 100 streaming deltas in one drain interval coalesce to the one
/// touched entry block (latest state only), and the bounded inbox does the
/// same for off-thread producers.
#[test]
fn app_dirty_blocks_and_inbox_keep_latest_state_only() {
    let mut chat = ChatContainer::new();
    let mut text = String::from("a");
    chat.set_entries(&[ChatEntry::Streaming {
        live: true,
        thinking: String::new(),
        text: text.clone(),
    }]);
    for _ in 0..100 {
        text.push('b');
        chat.set_entries(&[ChatEntry::Streaming {
            live: true,
            thinking: String::new(),
            text: text.clone(),
        }]);
    }
    let batch = chat.take_dirty_blocks();
    assert_eq!(batch.blocks, vec![0], "only the live tail entry is dirty");
    assert!(!batch.overflow);
    assert_eq!(
        chat.dirty_block_coalesced(),
        chat.dirty_block_pushes() - 1,
        "the first commit pushed one block, every later delta coalesced into it"
    );

    let inbox = DirtyInbox::new(4);
    for revision in 0..100 {
        inbox.note_block(7, revision);
    }
    assert_eq!(inbox.len(), 1);
    assert_eq!(inbox.coalesced(), 99);
    let batch = inbox.drain();
    assert_eq!(
        batch.blocks,
        vec![(7, 99)],
        "only the newest revision is kept"
    );
    assert!(!batch.overflow);

    // Over the bound: one full-frame marker replaces the block list.
    for block in 0..5u64 {
        inbox.note_block(block, 1);
    }
    let overflow = inbox.drain();
    assert!(overflow.overflow);
    assert!(overflow.blocks.is_empty());
}
