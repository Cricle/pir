//! TUI core tests: component tree, text layout, scroll region, and the
//! differential main-screen renderer (upstream `pi-tui` contracts).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use pir::tui::ansi::strip_ansi;
use pir::tui::core::{
    BoxContainer, Component, Container, DynamicBorder, ReservedScrollbarColumn, Rows, Spacer,
    Terminal, TerminalSink, Text, TuiCore,
};
use pir::tui::stdin_buffer::WriteLog;
use pir::tui::theme::{set_theme, ThemeColor};

// ---------------------------------------------------------------------------
// Container
// ---------------------------------------------------------------------------

struct StaticLines(&'static [&'static str]);

impl Component for StaticLines {
    fn render(&self, _width: usize) -> Vec<String> {
        self.0.iter().map(|s| s.to_string()).collect()
    }
}

#[test]
fn container_renders_children_in_order() {
    let mut container = Container::new();
    container.add_child(StaticLines(&["one"]));
    container.add_child(StaticLines(&["two", "three"]));
    assert_eq!(container.render(10), vec!["one", "two", "three"]);
}

#[test]
fn container_clear_and_remove() {
    let mut container = Container::new();
    container.add_child(StaticLines(&["a"]));
    container.add_child(StaticLines(&["b"]));
    container.remove_child(0);
    assert_eq!(container.render(5), vec!["b"]);
    container.clear();
    assert!(container.render(5).is_empty());
}

// ---------------------------------------------------------------------------
// Text
// ---------------------------------------------------------------------------

#[test]
fn text_wraps_pads_and_trims() {
    let text = Text::with_padding("hello world", 0, 0);
    let lines = text.render(8);
    assert_eq!(lines, vec!["hello   ", "world   "]);
}

#[test]
fn text_empty_renders_nothing() {
    let text = Text::with_padding("   ", 1, 0);
    assert!(text.render(10).is_empty());
}

#[test]
fn text_side_padding_shrinks_content_width() {
    let text = Text::with_padding("abcdefgh", 1, 0);
    let lines = text.render(8);
    // content width = 8 - 2 = 6 → "abcdef" / "gh", each line padded with
    // one space per margin then to full width.
    assert_eq!(lines.len(), 2);
    assert_eq!(strip_ansi(&lines[0]), " abcdef ");
}

#[test]
fn text_set_text_updates_output() {
    let text = Text::with_padding("before", 0, 0);
    assert_eq!(text.render(20), vec!["before              "]);
    text.set_text("after");
    assert_eq!(text.render(20), vec!["after               "]);
}

#[test]
fn text_applies_background_painter() {
    let text = Text::with_padding("ab", 0, 0);
    text.set_bg(Some(Arc::new(|s: &str| format!("«{s}»"))));
    let lines = text.render(4);
    assert_eq!(lines, vec!["«ab  »"]);
}

// ---------------------------------------------------------------------------
// Spacer / DynamicBorder / Box
// ---------------------------------------------------------------------------

#[test]
fn spacer_produces_blank_rows() {
    assert_eq!(Spacer::new(2).render(6), vec!["      ", "      "]);
}

#[test]
fn dynamic_border_spans_full_width() {
    // The process-global theme: serialize with the sibling suites' swaps.
    let _guard = super::tui_system_theme::lock();
    set_theme("dark");
    let border = DynamicBorder::new();
    let lines = border.render(5);
    assert_eq!(lines.len(), 1);
    assert_eq!(strip_ansi(&lines[0]), "─────");
}

#[test]
fn box_container_applies_background_to_all_lines() {
    let mut boxed = BoxContainer::new(1, 1, |s: &str| format!("<{s}>"));
    boxed.add_child(Text::with_padding("hi", 0, 0));
    let lines = boxed.render(8);
    // paddingY blank rows + content, each fully painted.
    assert_eq!(lines[0], "<        >");
    assert_eq!(strip_ansi(&lines[1]), "< hi     >");
    assert_eq!(lines[2], "<        >");
}

// ---------------------------------------------------------------------------
// TuiCore differential renderer
// ---------------------------------------------------------------------------

fn test_terminal() -> Terminal {
    // Size detection runs against the real stdout in tests; the diff logic
    // only needs consistent columns/rows within one test, so query once.
    Terminal::new()
}

#[test]
fn first_render_emits_all_lines_without_clear() {
    let mut core = TuiCore::new(test_terminal());
    core.push_content(Box::new(StaticLines(&["alpha", "beta"])));
    core.render();
    // The undecorated frame is exactly the component's lines (`last_frame_lines`
    // is the accessor's shape: unpadded, no reset suffix). A first render that
    // ran the clear path is fine; it must still emit every line.
    let expected = vec!["alpha".to_string(), "beta".to_string()];
    assert_eq!(core.last_frame_lines(), expected.as_slice());
    // Second render with no change is a no-op diff (still safe) and keeps the
    // same frame.
    core.render();
    assert_eq!(core.last_frame_lines(), expected.as_slice());
    core.stop();
}

#[test]
fn render_updates_after_content_change() {
    let mut core = TuiCore::new(test_terminal());
    core.push_content(Box::new(StaticLines(&["one"])));
    core.render();
    assert_eq!(
        core.last_frame_lines(),
        ["one".to_string()].as_slice(),
        "the first frame holds the first content"
    );
    core.push_content(Box::new(StaticLines(&["two"])));
    core.render();
    // `push_content` appends: the updated frame is both components' lines, not
    // the stale first frame.
    assert_eq!(
        core.last_frame_lines(),
        ["one".to_string(), "two".to_string()].as_slice(),
        "the frame must be re-rendered from the updated content"
    );
    core.stop();
}

#[test]
fn request_render_coalesces_rapid_calls() {
    let mut core = TuiCore::new(test_terminal());
    core.push_content(Box::new(StaticLines(&["x"])));
    let before = core.frame_version();
    core.request_render();
    assert_eq!(
        core.frame_version(),
        before + 1,
        "the first request paints a frame"
    );
    // `request_render` skips when less than min_frame (16ms) elapsed since the
    // previous paint, so the rapid follow-ups coalesce into the first frame
    // instead of rendering twice more.
    core.request_render();
    core.request_render();
    assert_eq!(
        core.frame_version(),
        before + 1,
        "rapid requests must coalesce into one frame"
    );
    assert_eq!(
        core.last_frame_lines(),
        ["x".to_string()].as_slice(),
        "the coalesced frame carries the content"
    );
    core.stop();
}

/// A mutable single-row component with no revision cache: every render must
/// re-read the text, exactly like the unversioned dock/editor path.
struct MutableLine(Arc<Mutex<String>>);

impl Component for MutableLine {
    fn render(&self, _width: usize) -> Vec<String> {
        vec![self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()]
    }
}

/// TUI-P1: a high event rate may not translate into a render per event. The
/// frame budget (16 ms) caps the render count while the final content is
/// still painted once the burst ends.
#[test]
fn frame_budget_caps_high_event_rate_and_flushes_final_content() {
    let mut core = TuiCore::new(counting_terminal());
    core.terminal().set_size(80, 24);
    let text = Arc::new(Mutex::new(String::from("frame 0")));
    core.push_content(Box::new(MutableLine(Arc::clone(&text))));
    core.render();
    let before = core.frame_version();

    const REQUESTS: u32 = 5_000;
    let started = Instant::now();
    for index in 0..REQUESTS {
        *text
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = format!("frame {index}");
        core.request_render();
    }
    let elapsed = started.elapsed();
    let rendered = core.frame_version() - before;
    // Time-based bound plus a small scheduling allowance. The point is that
    // 5k content updates produce frames proportional to elapsed time, not to
    // the event count.
    let allowed = (elapsed.as_millis() / 16) as u64 + 2;
    assert!(
        rendered <= allowed,
        "{REQUESTS} updates rendered {rendered} frames in {elapsed:?}; the 16ms budget allows at most {allowed}"
    );
    assert!(
        rendered < u64::from(REQUESTS) / 10,
        "the frame budget did not coalesce: {rendered} frames"
    );

    // A burst must not lose the last delta: after the frame interval the
    // final content is painted.
    *text
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = "final frame".to_string();
    std::thread::sleep(core.min_frame_interval() + Duration::from_millis(2));
    assert!(
        core.request_render(),
        "the frame deadline must paint the final content after the burst"
    );
    assert_eq!(
        core.last_frame_lines(),
        ["final frame".to_string()].as_slice(),
        "the final frame carries the newest content"
    );
    core.stop();
}

/// TUI-P1: real terminal input bypasses the frame budget (low input latency),
/// while a non-input dirty frame waits out the remainder of the 16 ms window
/// and becomes due exactly at `next_frame_deadline`.
#[test]
fn frame_budget_bypasses_for_input_and_exposes_the_deadline() {
    let mut core = TuiCore::new(counting_terminal());
    core.push_content(Box::new(StaticLines(&["latency"])));
    core.render();
    let now = Instant::now();
    assert!(
        !core.frame_due_at(now, false),
        "a non-input frame must respect the 16ms render interval"
    );
    assert!(
        core.frame_due_at(now, true),
        "a key/paste/mouse event must paint immediately (no input latency regression)"
    );
    let deadline = core.next_frame_deadline();
    assert!(
        core.frame_due_at(deadline, false),
        "the poll deadline must coincide with the next paintable frame"
    );
    core.stop();
}

// ---------------------------------------------------------------------------
// Theme integration in components (border color from the global theme)
// ---------------------------------------------------------------------------

#[test]
fn border_uses_theme_border_color() {
    let _guard = super::tui_system_theme::lock();
    set_theme("dark");
    pir::tui::with_theme(|theme| {
        let border_sgr = theme.fg(ThemeColor::Border, "");
        assert!(border_sgr.starts_with("\x1b[38;"));
    });
}

// ---------------------------------------------------------------------------
// Runtime TUI-mode switch: the transcript scrollbar reservation is live
// ---------------------------------------------------------------------------

/// `switchTuiMode` (interactive-mode.ts:892-928) flips the alt-screen
/// scrollbar reservation without rebuilding the component tree; the wrapper
/// reads its flag on every frame.
#[test]
fn reserved_scrollbar_column_reads_the_live_flag_each_frame() {
    let flag = Arc::new(AtomicBool::new(false));
    let column = ReservedScrollbarColumn::with_live_flag(
        Text::with_padding("abcdefghij", 0, 0),
        false,
        Arc::clone(&flag),
    );
    // Regular mode: no reserved column, the text fits one 10-cell row.
    let regular = column.render(10);
    assert_eq!(regular.len(), 1, "{regular:?}");
    assert_eq!(strip_ansi(&regular[0]), "abcdefghij");

    // Fullscreen + scrollbar "always": the child renders at width - 1 and the
    // freed column is padded, so the same text wraps to two rows.
    flag.store(true, Ordering::SeqCst);
    let fullscreen = column.render(10);
    assert_eq!(fullscreen.len(), 2, "{fullscreen:?}");
    assert_eq!(strip_ansi(&fullscreen[0]), "abcdefghi ");
    assert_eq!(strip_ansi(&fullscreen[1]), "j         ");

    // Back to regular mode: the reservation disappears on the next frame.
    flag.store(false, Ordering::SeqCst);
    assert_eq!(column.render(10).len(), 1);
}

// ---------------------------------------------------------------------------
// Steady-frame row-copy budget (R5 measurement seam)
// ---------------------------------------------------------------------------

/// A component that renders from its own cached lines directly through the
/// canvas — the shape `AssistantMessageComponent`'s segment cache uses (a hit
/// pushes the cached lines into the pooled rows).
struct CachedRows(Vec<String>);

impl Component for CachedRows {
    fn render(&self, _width: usize) -> Vec<String> {
        self.0.clone()
    }

    fn render_into(&self, _width: usize, out: &mut Rows<'_>) {
        for line in &self.0 {
            out.push(line);
        }
    }
}

fn counting_terminal() -> Terminal {
    Terminal::with_sink_factory(
        || Box::new(std::io::sink()) as Box<dyn TerminalSink>,
        WriteLog::disabled(),
    )
}

fn fixture_lines() -> Vec<String> {
    (0..120)
        .map(|index| format!("cached row {index:03} {}", "x".repeat(40)))
        .collect()
}

/// The first frame fills the frame pool, the second fills the retired
/// baseline (a pool swap), and the third is steady state: zero bytes copied.
/// (A full repaint mid-stream hands the stripped baseline back immediately —
/// see `full_repaint_hands_back_a_reset_free_scratch_pool`.)
#[test]
fn steady_frame_copies_zero_cached_row_bytes() {
    let lines = fixture_lines();
    let total_bytes: u64 = lines.iter().map(|line| line.len() as u64).sum();

    for windowed in [false, true] {
        let mut core = TuiCore::new(counting_terminal());
        core.terminal().set_size(80, 24);
        core.set_windowed_viewport(windowed);
        core.push_content(Box::new(CachedRows(lines.clone())));
        let mut frames = Vec::new();
        for _ in 0..3 {
            core.render();
            frames.push(core.last_frame_rows_work());
        }
        assert_eq!(
            frames[0].row_writes,
            lines.len() as u64,
            "windowed={windowed}: first frame must claim every row: {:?}",
            frames[0]
        );
        assert_eq!(
            frames[0].row_write_bytes, total_bytes,
            "windowed={windowed}: first frame copies the whole transcript: {:?}",
            frames[0]
        );
        let steady = frames[2];
        assert_eq!(
            steady.row_writes,
            lines.len() as u64,
            "windowed={windowed}: steady frame still visits every row: {steady:?}"
        );
        assert_eq!(
            steady.pool_growth, 0,
            "windowed={windowed}: steady frame must not grow the row pool: {steady:?}"
        );
        assert_eq!(
            steady.row_write_bytes, 0,
            "windowed={windowed}: steady frame copied cached rows: {steady:?}"
        );
        assert_eq!(
            steady.row_reuse_bytes, total_bytes,
            "windowed={windowed}: steady frame must reuse every cached byte: {steady:?}"
        );
        assert_eq!(
            steady.rows_reused,
            lines.len() as u64,
            "windowed={windowed}: every row comes from the exact-content path: {steady:?}"
        );
        assert_eq!(
            core.last_frame_lines(),
            lines.as_slice(),
            "windowed={windowed}: the reused rows must still be the exact frame"
        );
        core.stop();
    }
}

/// A forced full repaint (width change) retires the written baseline — whose
/// rows carry the lazy reset — into the frame pool. The pool must come back
/// stripped, so the very next steady frame copies zero bytes instead of
/// re-copying the whole transcript once.
#[test]
fn full_repaint_hands_back_a_reset_free_scratch_pool() {
    let lines = fixture_lines();
    let mut core = TuiCore::new(counting_terminal());
    core.terminal().set_size(80, 24);
    core.push_content(Box::new(CachedRows(lines)));
    core.render();
    core.render();
    // Width change: full repaint, the old baseline is retired into the pool.
    core.terminal().set_size(90, 24);
    core.render();
    core.render();
    let steady = core.last_frame_rows_work();
    assert_eq!(
        steady.pool_growth, 0,
        "the retired baseline must be recycled by index: {steady:?}"
    );
    assert_eq!(
        steady.row_write_bytes, 0,
        "the recycled baseline must be reset-free (no re-copy after a full repaint): {steady:?}"
    );
    core.stop();
}

/// A one-line change copies exactly that row, never the transcript prefix.
#[test]
fn changed_row_frame_copies_only_the_changed_row() {
    let mut live = fixture_lines();
    let mut pool: Vec<String> = Vec::new();
    {
        let mut rows = Rows::new(&mut pool);
        rows.rewind();
        for line in &live {
            rows.push(line);
        }
        rows.trim();
    }
    let unchanged_bytes: u64 = live
        .iter()
        .enumerate()
        .filter(|(index, _)| *index != 80)
        .map(|(_, line)| line.len() as u64)
        .sum();
    let changed = format!("{} CHANGED", live[80]);
    live[80] = changed;
    let changed_len = live[80].len() as u64;
    {
        let mut rows = Rows::new(&mut pool);
        rows.rewind();
        for line in &live {
            rows.push(line);
        }
        rows.trim();
        let work = rows.work();
        assert_eq!(work.row_writes, live.len() as u64);
        assert_eq!(
            work.row_write_bytes, changed_len,
            "only the changed row may be copied: {work:?}"
        );
        assert_eq!(
            work.row_reuse_bytes, unchanged_bytes,
            "the unchanged prefix must be reused in place: {work:?}"
        );
        assert_eq!(work.rows_reused, live.len() as u64 - 1);
    }
    assert_eq!(pool, live, "the canvas output must stay byte-identical");
}
