//! Regression tests for the TUI event-loop review fixes (P1-2 viewport
//! pinning, F6 scroll reset, F13 worker-queue discard). The decision logic
//! under test is the pure extraction surface:
//!
//! - `pir::tui::core::compensated_scroll_offset` (P1-2): the scrolled
//!   render's growth compensation that pins the viewport while the stream
//!   appends;
//! - `pir::tui::terminal::worker_queue_step` (F13): the worker-loop head
//!   decision that discards queued prompts once the UI drops the command
//!   channel;
//! - `TuiCore::set_scroll_offset(0)` (F6): the reset path every session
//!   switch (/clear, /resume, tree navigate, fork) funnels through.

use std::collections::VecDeque;

use pir::tui::core::{compensated_scroll_offset, Terminal, TuiCore};
use pir::tui::terminal::{worker_queue_step, WorkerQueueProbe, WorkerQueueStep};

// ---------------------------------------------------------------------------
// P1-2: viewport pinning under streaming growth
// ---------------------------------------------------------------------------

/// The render-window math copied verbatim from `TuiCore::render_scrolled`
/// (dock=0 for simplicity): the visible transcript slice is
/// `[end - viewport_h, end)` with `end = transcript_end - offset`.
fn window(transcript_end: usize, offset: usize, viewport_h: usize) -> (usize, usize) {
    let max = transcript_end.saturating_sub(viewport_h);
    let offset = offset.min(max);
    let end = transcript_end.saturating_sub(offset);
    let start = end.saturating_sub(viewport_h);
    (start, end)
}

/// Simulate the scrolled-render path over a sequence of frames where the
/// transcript grows (streaming appends) while the user has scrolled back.
/// The viewport window must stay pinned to the same content rows.
#[test]
fn p1_2_pinned_viewport_under_streaming_growth() {
    let viewport_h = 10;
    // The user scrolled to look at buffer rows [70, 80) of a 100-row
    // transcript: window end = 80 → offset = 100 - 80 = 20, and the visible
    // start is end - offset - viewport_h = 70.
    let transcript_end = 100;
    let mut last_transcript_end = Some(transcript_end);
    let mut offset = 20;
    let (start, _) = window(transcript_end, offset, viewport_h);
    assert_eq!(start, 70);

    // Streaming appends rows; each frame re-derives the offset through the
    // compensation exactly like render_scrolled does.
    for new_transcript_end in [110usize, 130, 131, 250] {
        let max = new_transcript_end.saturating_sub(viewport_h);
        offset = compensated_scroll_offset(offset, last_transcript_end, new_transcript_end, max);
        last_transcript_end = Some(new_transcript_end);
        let (new_start, _) = window(new_transcript_end, offset, viewport_h);
        assert_eq!(
            new_start, start,
            "viewport drifted: transcript_end={new_transcript_end}"
        );
    }
}

/// Follow mode (offset 0) is untouched: new content keeps scrolling the
/// window with the end.
#[test]
fn p1_2_follow_mode_stays_at_zero() {
    assert_eq!(compensated_scroll_offset(0, Some(100), 140, 130), 0);
    assert_eq!(compensated_scroll_offset(0, None, 140, 130), 0);
}

/// The clamp fallback survives: a compensated offset can never point the
/// window past the transcript start (max = transcript_end - viewport_h).
#[test]
fn p1_2_offset_clamps_to_max() {
    // offset 95 above a 100-row transcript, then 10 rows appended:
    // 95 + 10 = 105 exceeds max = 110 - 10 = 100 → clamped to 100.
    assert_eq!(compensated_scroll_offset(95, Some(100), 110, 100), 100);
}

/// A missing baseline (first scrolled render, or after an invalidate) keeps
/// the offset as-is: no spurious jump on the first frame after a scroll.
#[test]
fn p1_2_no_baseline_keeps_offset() {
    assert_eq!(compensated_scroll_offset(30, None, 140, 130), 30);
}

/// A shrinking transcript (e.g. entries removed) saturates the growth term
/// instead of underflowing.
#[test]
fn p1_2_shrink_does_not_underflow() {
    // last end 200, new end 100: growth saturates to 0, offset unchanged
    // (then clamped to the new max).
    assert_eq!(compensated_scroll_offset(30, Some(200), 100, 90), 30);
    // An offset that no longer fits the shrunken transcript is clamped.
    assert_eq!(compensated_scroll_offset(95, Some(200), 100, 90), 90);
}

/// With no dock, the compensation reduces to the absolute-window-start
/// invariant: `transcript_end - offset` is constant while content appends.
#[test]
fn p1_2_window_start_invariant_equals_absolute_scroll_top() {
    let viewport_h = 8;
    let mut last_end = Some(50usize);
    let mut offset = 12usize;
    // The invariant anchor is the absolute window START: with the window end
    // at transcript_end - offset, the start is that minus the viewport.
    let start0 = 50 - 12 - viewport_h;
    for end in [51usize, 60, 61, 99, 100] {
        let max = end.saturating_sub(viewport_h);
        offset = compensated_scroll_offset(offset, last_end, end, max);
        last_end = Some(end);
        let (start, _) = window(end, offset, viewport_h);
        assert_eq!(start, start0, "absolute window start moved at end={end}");
    }
}

// ---------------------------------------------------------------------------
// F6: the reset path (set_scroll_offset(0)) restores follow-end
// ---------------------------------------------------------------------------

/// Every session-switch path (/clear, /resume, tree navigate, fork) funnels
/// through `set_scroll_offset(0)`: the offset must return to follow-end and
/// stay reset (repeated resets are idempotent), and a fresh scroll after the
/// reset works from a clean baseline.
#[test]
fn f6_reset_scroll_offset_restores_follow_end() {
    let mut core = TuiCore::new(Terminal::new());
    core.set_scroll_offset(42);
    assert_eq!(core.scroll_offset(), 42);
    core.set_scroll_offset(0);
    assert_eq!(core.scroll_offset(), 0, "reset must return to follow-end");
    // Idempotent: a second reset (e.g. /resume after /clear) stays put.
    core.set_scroll_offset(0);
    assert_eq!(core.scroll_offset(), 0);
    // Scrolling again after the reset is a normal scroll from the end.
    core.set_scroll_offset(7);
    assert_eq!(core.scroll_offset(), 7);
    core.set_scroll_offset(0);
    assert_eq!(core.scroll_offset(), 0);
}

// ---------------------------------------------------------------------------
// F13: the worker discards queued prompts once the UI exits
// ---------------------------------------------------------------------------

#[test]
fn f13_disconnected_channel_discards_queue() {
    let mut queue: VecDeque<String> = ["a".into(), "b".into(), "c".into()].into_iter().collect();
    let step = worker_queue_step(&mut queue, WorkerQueueProbe::Disconnected);
    assert_eq!(step, WorkerQueueStep::ExitDiscardQueue);
    // The queue is dropped, not drained: nothing runs after the UI exits.
    assert!(queue.is_empty());
}

#[test]
fn f13_queue_drains_fifo_while_channel_open() {
    let mut queue: VecDeque<String> = ["first".into(), "second".into()].into_iter().collect();
    assert_eq!(
        worker_queue_step(&mut queue, WorkerQueueProbe::Empty),
        WorkerQueueStep::RunQueued("first".to_string())
    );
    assert_eq!(
        worker_queue_step(&mut queue, WorkerQueueProbe::Empty),
        WorkerQueueStep::RunQueued("second".to_string())
    );
    assert_eq!(
        worker_queue_step(&mut queue, WorkerQueueProbe::Empty),
        WorkerQueueStep::Wait
    );
}

#[test]
fn f13_arrived_command_beats_the_queue() {
    let mut queue: VecDeque<String> = ["queued".into()].into_iter().collect();
    assert_eq!(
        worker_queue_step(&mut queue, WorkerQueueProbe::Arrived),
        WorkerQueueStep::RunArrived
    );
    // The queued prompt is untouched and runs next.
    assert_eq!(queue.len(), 1);
    assert_eq!(
        worker_queue_step(&mut queue, WorkerQueueProbe::Empty),
        WorkerQueueStep::RunQueued("queued".to_string())
    );
}
