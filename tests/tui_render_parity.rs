//! Machine-checked render parity for the TUI optimization wave (S0/S1/S2).
//!
//! The wave landed with a static equivalence argument only: frame-buffer
//! recycling ([`Rows`]), the `Component::render_into` bridge (S1), and entry
//! reuse (S2). These tests turn that argument into execution:
//!
//! 1. `render` ↔ `render_into` byte parity for every publicly constructible
//!    core component at a spread of widths (0/1/10/80/500 — degenerate, tiny,
//!    terminal-sized, and wider than any fixture content), including a
//!    deliberately poisoned `Rows` pool: recycled rows must be cleared, never
//!    appended to, and the pool tail must be trimmed to the frame.
//! 2. `TuiCore` frame idempotence: a second and third `render()` on unchanged
//!    state repaints nothing and leaves both the raw frame
//!    ([`TuiCore::last_frame_lines`]) and the painted baseline
//!    ([`TuiCore::last_screen_lines`]) byte-identical, while
//!    [`TuiCore::frame_version`] keeps advancing.
//! 3. Pool-staleness defence at the frame level: a wide/tall frame followed by
//!    a narrow/short frame must leave no residue (the recycled slot is cleared
//!    by `Rows::claim`, the pool tail dropped by `Rows::trim`), on the
//!    follow-end path, the resized path, and the scrolled (windowed) path.
//! 4. The `messages.rs` canvas migration (section 4): `render` ↔ `render_into`
//!    parity for `UserMessageComponent`, `AssistantMessageComponent` and
//!    `ToolExecutionComponent` in the states the rewrite touched, reference-byte
//!    equality against the PRE-migration row formulas, and the bridge /
//!    pool-reuse round trip.
//!
//! Determinism: every expectation is derived from the same public API
//! (`Component::render`, `viewport_geometry`), never from a golden literal —
//! these are differentials between the two render paths, not frozen layouts.

use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use pir::ai::types::{AssistantMessage, ContentBlock, StopReason, TextContent, ThinkingContent};
use pir::coding_agent::truncate::{truncate_tail, DEFAULT_MAX_BYTES, DEFAULT_MAX_LINES};
use pir::tui::altscreen_search::viewport_geometry;
use pir::tui::ansi::{pad_prefixed_to_width, visible_width, wrap_text_with_ansi, BG_RESET};
use pir::tui::app::{spinner_frame, ToolPreview};
use pir::tui::core::{
    BoxContainer, Component, Container, DynamicBorder, FrameDecorateContext,
    ReservedScrollbarColumn, Rows, Spacer, Terminal, Text, TuiCore,
};
use pir::tui::markdown::{global_mermaid_mode, Markdown, MarkdownOptions, MarkdownTheme};
use pir::tui::messages::{
    edit_preview_input, format_tool_call_line, render_diff_text, shell_truncation_warning,
    strip_shell_footer, AssistantMessageComponent, ToolExecutionComponent, UserMessageComponent,
};
use pir::tui::theme::{set_theme, with_theme, Theme, ThemeColor};

/// The per-line reset `TuiCore` appends to every painted row
/// (`SEGMENT_RESET`, `core.rs`): carried by the painted baseline, absent from
/// the raw frame.
const SEGMENT_RESET: &str = "\x1b[0m\x1b]8;;\x07";

/// Widths every component differential runs at.
const WIDTHS: [usize; 5] = [0, 1, 10, 80, 500];

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// `render_into` through a fresh pool, trimmed exactly like the frame path
/// (`TuiCore::frame_lines` trims after the last component).
fn render_into_lines(component: &dyn Component, width: usize) -> Vec<String> {
    render_into_lines_with_pool(component, width, Vec::new())
}

/// Same, but starting from an existing pool: the state the frame renderer is
/// in on every frame after the first.
fn render_into_lines_with_pool(
    component: &dyn Component,
    width: usize,
    mut pool: Vec<String>,
) -> Vec<String> {
    let mut rows = Rows::new(&mut pool);
    component.render_into(width, &mut rows);
    rows.trim();
    pool
}

/// The stale pool the frame renderer's scratch holds after a wide/tall frame:
/// rows far longer than anything the fixtures produce, so a missing clear in
/// `Rows::claim` shows up as an over-long row.
fn poisoned_pool() -> Vec<String> {
    (0..8)
        .map(|index| format!("STALE-{index:02}-{}", "Z".repeat(600)))
        .collect()
}

/// Byte parity of `render` and `render_into` at every width, on a fresh pool
/// and on a poisoned pool (which must be cleared and trimmed to the frame).
///
/// For the components whose `render` *is* this bridge (`UserMessageComponent`,
/// `AssistantMessageComponent`, `ToolExecutionComponent`) the fresh-pool leg is
/// the same code path on both sides and cannot fail; the load-bearing legs for
/// those are the poisoned pool (a component that appends to a recycled row
/// instead of claiming a cleared one, or one that leaves the pool tail, shows
/// up here) and the reference-byte tests in section 4.2, which compare the very
/// same bytes against the pre-migration formulas. The legacy `render`-only
/// components covered above run the trait's default bridge: their fresh-pool
/// leg compares the bridge against `render()` itself (it can catch a broken
/// row canvas but not a wrong component body), their poisoned leg is the real
/// recycled-row differential, and
/// `default_bridge_copies_foreign_render_lines_verbatim` pins the bridge against
/// literal source rows rather than `render()`'s own output.
fn assert_render_parity(component: &dyn Component, label: &str) {
    for width in WIDTHS {
        let expected = component.render(width);

        let fresh = render_into_lines(component, width);
        assert_eq!(
            fresh, expected,
            "{label}: render_into != render @ width {width}"
        );

        let recycled = render_into_lines_with_pool(component, width, poisoned_pool());
        assert_eq!(
            recycled, expected,
            "{label}: stale pool rows leaked into render_into @ width {width}"
        );
        assert_eq!(
            recycled.len(),
            expected.len(),
            "{label}: pool not trimmed to the frame @ width {width}"
        );
    }
}

/// A transcript component whose text can be swapped between frames. The frame
/// renderer recycles one pool across renders, so swapping the text is what
/// drives a wide/tall frame into the pool and a narrow/short one out of it.
/// Both methods delegate to the real [`Text`], so the core exercises the
/// production `render_into`.
struct SwappableText {
    text: Arc<Text>,
}

impl Component for SwappableText {
    fn render(&self, width: usize) -> Vec<String> {
        self.text.render(width)
    }

    fn render_into(&self, width: usize, out: &mut Rows<'_>) {
        self.text.render_into(width, out);
    }

    fn invalidate(&self) {
        self.text.invalidate();
    }
}

/// A pre-migration component (only `render` implemented): its `render_into`
/// is the trait's default bridge, which must copy the lines verbatim into the
/// pooled rows.
struct LegacyLines {
    lines: Vec<String>,
}

impl Component for LegacyLines {
    fn render(&self, _width: usize) -> Vec<String> {
        self.lines.clone()
    }
}

/// The wide/tall frame the pool-staleness tests retire into the recycle
/// scratch: 12 source lines far wider than the narrow frame that follows,
/// every row carrying a marker no later frame may contain.
fn wide_tall_text() -> String {
    (0..12)
        .map(|index| format!("WIDEMARKER-{index:02} {}", "W".repeat(250)))
        .collect::<Vec<_>>()
        .join("\n")
}

fn numbered_transcript(count: usize) -> String {
    (1..=count)
        .map(|index| format!("line {index}"))
        .collect::<Vec<_>>()
        .join("\n")
}

// ---------------------------------------------------------------------------
// 1. render ↔ render_into parity
// ---------------------------------------------------------------------------

#[test]
fn text_render_matches_render_into_at_every_width() {
    let mut fixtures: Vec<(&str, Text)> = vec![
        ("plain", Text::new("hello render parity")),
        ("empty", Text::new("")),
        ("whitespace-only", Text::new("  \t \n ")),
        (
            "multiline",
            Text::new("first line\nsecond line is quite a bit longer than the first\nthird"),
        ),
        ("tabs", Text::new("col1\tcol2\t\tcol3")),
        (
            "overwide-single-line",
            Text::new(format!("{} tail", "A".repeat(1200))),
        ),
        (
            "wide-graphemes",
            Text::new("日本語テキストの折り返しと結合 🚗 絵文字"),
        ),
        (
            "ansi",
            Text::new("\x1b[31mred\x1b[0m plain \x1b[1;32mbold green\x1b[0m end"),
        ),
        ("trailing-newline", Text::new("body\n")),
        ("padding-0x0", Text::with_padding("no padding", 0, 0)),
        ("padding-3x2", Text::with_padding("padded", 3, 2)),
        ("padding-oversized", Text::with_padding("x", 50, 1)),
    ];
    let painted = Text::new("painted row\nsecond painted row");
    let painter: Arc<dyn Fn(&str) -> String + Send + Sync> =
        Arc::new(|line: &str| format!("\x1b[48;5;236m{line}\x1b[0m"));
    painted.set_bg(Some(painter));
    fixtures.push(("background-painted", painted));

    for (label, fixture) in &fixtures {
        assert_render_parity(fixture, label);
    }

    // A narrow frame does not truncate the painter's replacement: `paint_row`
    // measures the padded row *before* the painter runs, exactly like
    // `apply_background_to_line`.
    let painted_wide = Text::new("painted");
    let painter: Arc<dyn Fn(&str) -> String + Send + Sync> =
        Arc::new(|line: &str| format!("[{line}]"));
    painted_wide.set_bg(Some(painter));
    assert_eq!(
        render_into_lines(&painted_wide, 20),
        painted_wide.render(20)
    );
}

#[test]
fn spacer_border_and_reserved_column_render_parity() {
    assert_render_parity(&Spacer::new(0), "spacer-0");
    assert_render_parity(&Spacer::new(3), "spacer-3");
    assert_render_parity(&DynamicBorder::new(), "dynamic-border");

    // ScrollView's reserved column: the child renders at width - 1 and every
    // line gains one trailing space, except at width <= 1 (upstream guard).
    for reserve in [true, false] {
        let mut child = Container::new();
        child.add_child(Text::new("scrolled row\nsecond scrolled row"));
        child.add_child(Spacer::new(1));
        let label = if reserve {
            "reserved-scrollbar-on"
        } else {
            "reserved-scrollbar-off"
        };
        assert_render_parity(&ReservedScrollbarColumn::new(child, reserve), label);
    }
}

#[test]
fn nested_containers_render_parity() {
    let mut nested = Container::new();
    nested.add_child(Text::new("inner text that wraps when the width is small"));
    nested.add_child(Spacer::new(2));

    let mut boxed = BoxContainer::new(1, 1, |line: &str| format!("\x1b[48;5;236m{line}\x1b[0m"));
    boxed.add_child(Text::with_padding("boxed child", 0, 0));
    boxed.add_child(nested);

    let mut tail = Container::new();
    tail.add_child(Text::with_padding("tail\twith tab", 2, 1));
    tail.add_child(Spacer::new(0));

    let mut outer = Container::new();
    outer.add_child(Text::new("outer head"));
    outer.add_child(boxed);
    outer.add_child(tail);
    assert_render_parity(&outer, "nested-containers");

    // A box whose painter *lengthens* the row (the measured-then-painted
    // order both paths share), padding 0 and a zero-padding box.
    let mut bracketed = BoxContainer::new(2, 0, |line: &str| format!("[{line}]"));
    bracketed.add_child(Text::with_padding("bracketed", 0, 0));
    bracketed.add_child(Spacer::new(1));
    assert_render_parity(&bracketed, "box-lengthening-painter");

    let mut flat = BoxContainer::new(0, 2, |line: &str| format!("\x1b[7m{line}\x1b[0m"));
    flat.add_child(Text::new("flat box"));
    assert_render_parity(&flat, "box-padding-0");
}

#[test]
fn markdown_default_bridge_render_parity() {
    let source = "# Title\n\nsome *body* text long enough to wrap at narrow widths\n\n\
         - a\n- b\n\n```rust\nlet x = 1;\n```\n\n| h | h2 |\n| - | - |\n| 1 | 2 |\n";
    let markdown = Markdown::new(source, 1, 0);
    assert_render_parity(&markdown, "markdown");

    // Its render cache must not change the bytes between two calls.
    assert_eq!(markdown.render(80), markdown.render(80));

    // And it bridges correctly nested inside the migrated containers.
    let mut boxed = BoxContainer::new(1, 0, |line: &str| format!("\x1b[48;5;236m{line}\x1b[0m"));
    boxed.add_child(Markdown::new(source, 0, 0));
    let mut outer = Container::new();
    outer.add_child(Text::new("legend"));
    outer.add_child(boxed);
    assert_render_parity(&outer, "markdown-in-box");
}

#[test]
fn default_bridge_copies_foreign_render_lines_verbatim() {
    // Lines the migrated components would never emit: unpadded short lines,
    // an over-wide line, an unterminated SGR run, wide graphemes, a tab. The
    // bridge copies them verbatim — no repadding, no truncation.
    let expected = vec![
        String::new(),
        "   ".to_string(),
        "short".to_string(),
        "trailing space ".to_string(),
        format!("\x1b[31mopen sgr never closed {}", "x".repeat(700)),
        "wide 日本語 cells".to_string(),
        "\t tab inside".to_string(),
    ];
    let component = LegacyLines {
        lines: expected.clone(),
    };
    assert_render_parity(&component, "legacy-lines");
    // The fixture's literal rows, not `component.render(20)`: the default
    // bridge calls `render`, so comparing the bridge against `render()` only
    // tests its own source lines (the poisoned-pool leg inside
    // `assert_render_parity` is the real check that recycled rows are cleared
    // and trimmed).
    assert_eq!(render_into_lines(&component, 20), expected);
}

#[test]
fn rows_claim_clears_the_recycled_slot_and_trim_drops_the_tail() {
    let mut pool: Vec<String> = vec![
        "STALE-A".to_string(),
        "STALE-B".to_string(),
        "STALE-C".to_string(),
    ];
    // The pool is read once the `Rows` borrow ends, so every assertion below
    // is on the recycled buffers themselves (`line_at` reads the same slot
    // while the canvas is live).
    {
        let mut rows = Rows::new(&mut pool);
        assert!(rows.is_empty());
        assert_eq!(rows.len(), 0);

        // The first push reuses pool row 0 and clears it: never a stale tail.
        rows.push("x");
        assert_eq!(rows.line_at(0).as_str(), "x");
        assert_eq!(rows.len(), 1);

        rows.push_fmt(format_args!("{}-{}", "y", 2));
        assert_eq!(rows.line_at(1).as_str(), "y-2");
        assert_eq!(rows.len(), 2);

        // `line_mut` edits the row the cursor sits on; `line_at` reaches a row
        // already written in this frame in either direction.
        rows.line_mut().push('!');
        assert_eq!(rows.line_at(1).as_str(), "y-2!");
        rows.line_at(0).insert(0, '>');
        assert_eq!(rows.line_at(0).as_str(), ">x");

        // The frame ends at two rows: trim drops the third pooled row.
        rows.trim();
    }
    assert_eq!(pool, vec![">x".to_string(), "y-2!".to_string()]);

    {
        // rewind keeps the buffers; the next push clears the reused slot.
        let mut rows = Rows::new(&mut pool);
        rows.rewind();
        assert!(rows.is_empty());
        rows.push("z");
        assert_eq!(rows.line_at(0).as_str(), "z");
        assert_eq!(rows.len(), 1);
        rows.trim();
    }
    assert_eq!(pool, vec!["z".to_string()]);
}

#[test]
#[should_panic(expected = "is not part of the current frame")]
fn rows_line_at_rejects_rows_outside_the_frame() {
    let mut pool: Vec<String> = vec!["stale".to_string(); 4];
    let mut rows = Rows::new(&mut pool);
    rows.push("live");
    let _ = rows.line_at(1);
}

// ---------------------------------------------------------------------------
// 2. TuiCore frame path + idempotence
// ---------------------------------------------------------------------------

/// Components reachable from the frame path (`TuiCore::frame_lines`): a mixed
/// tree of migrated containers with a non-migrated markdown child, plus a
/// top-level border and spacer. Rebuilt per use so the core and the
/// expectation get independent but identical instances.
fn frame_fixture() -> Vec<Box<dyn Component>> {
    let mut transcript = Container::new();
    transcript.add_child(Text::new(
        "streaming row one\nrow two with\ttab and 日本語 text\nrow three",
    ));
    transcript.add_child(Spacer::new(2));

    let mut nested = Container::new();
    nested.add_child(Text::with_padding("nested", 2, 1));
    nested.add_child(Spacer::new(1));

    let mut boxed = BoxContainer::new(1, 1, |line: &str| format!("\x1b[48;5;236m{line}\x1b[0m"));
    boxed.add_child(Text::with_padding("boxed child", 0, 0));
    boxed.add_child(nested);
    transcript.add_child(boxed);
    transcript.add_child(Markdown::new(
        "## heading\n\nbody text long enough to wrap at narrow widths\n\n- a\n- b",
        1,
        0,
    ));

    vec![
        Box::new(transcript),
        Box::new(DynamicBorder::new()),
        Box::new(Spacer::new(1)),
    ]
}

#[test]
fn core_frame_lines_match_component_render_at_every_width() {
    let terminal = Terminal::new();
    terminal.set_size(80, 24);
    let mut core = TuiCore::new(terminal);
    for component in frame_fixture() {
        core.push_content(component);
    }

    for width in [1usize, 10, 80, 200] {
        core.terminal().set_size(width, 40);
        core.render();

        let expected: Vec<String> = frame_fixture()
            .iter()
            .flat_map(|component| component.render(width))
            .collect();
        assert_eq!(core.last_render_width(), width);
        assert_eq!(core.frame_buffer_length(), expected.len());
        assert_eq!(
            core.last_frame_lines(),
            expected.as_slice(),
            "core frame != concatenated component render @ width {width}"
        );
    }
}

#[test]
fn core_frame_render_is_idempotent_and_frame_version_advances() {
    let terminal = Terminal::new();
    terminal.set_size(60, 12);
    let mut core = TuiCore::new(terminal);
    for component in frame_fixture() {
        core.push_content(component);
    }

    core.render();
    let raw_frame = core.last_frame_lines().to_vec();
    let painted_frame = core.last_screen_lines();
    let version = core.frame_version();
    assert!(!raw_frame.is_empty(), "fixture must produce a frame");
    assert!(painted_frame
        .iter()
        .all(|line| line.ends_with(SEGMENT_RESET)));

    // Two unchanged re-renders: nothing may differ — neither the raw frame,
    // nor the painted baseline the diff compares against (which is what a
    // recycled pool leaking stale rows would show up as).
    for pass in 1u64..=2 {
        core.render();
        assert_eq!(
            core.frame_version(),
            version + pass,
            "frame_version must advance exactly once per render"
        );
        assert_eq!(
            core.last_frame_lines(),
            raw_frame.as_slice(),
            "pass {pass}: raw frame changed with unchanged state"
        );
        assert_eq!(
            core.last_screen_lines(),
            painted_frame,
            "pass {pass}: painted baseline changed with unchanged state"
        );
        assert_eq!(core.frame_buffer_length(), raw_frame.len());
    }
}

// ---------------------------------------------------------------------------
// 3. Pool-staleness defence
// ---------------------------------------------------------------------------

#[test]
fn recycled_pool_leaks_no_wide_frame_rows_into_a_narrow_frame() {
    let terminal = Terminal::new();
    terminal.set_size(200, 40);
    let mut core = TuiCore::new(terminal);
    let text = Arc::new(Text::with_padding(wide_tall_text(), 0, 0));
    core.push_content(Box::new(SwappableText {
        text: Arc::clone(&text),
    }));

    // #1: wide + tall frame (12 source lines wrapped over 200 columns).
    core.render();
    let wide = text.render(200);
    assert!(wide.len() > 10, "fixture must be tall: {} rows", wide.len());
    assert_eq!(core.last_frame_lines(), wide.as_slice());
    assert!(wide.iter().any(|line| line.contains("WIDEMARKER")));

    // #2: the frame shrinks in place. The renderer retires the wide rows into
    // the recycle pool when it swaps `previous_lines` for the new frame.
    text.set_text("short");
    core.render();
    assert_eq!(core.last_frame_lines(), text.render(200).as_slice());

    // #3: narrower AND shorter. The frame must be built entirely from cleared
    // slots: a missing `Rows::claim` clear would surface as a 200-column row
    // inside this 20-column frame, a missing `Rows::trim` as extra rows.
    core.terminal().set_size(20, 40);
    text.set_text("narrow");
    core.render();
    let narrow = text.render(20);
    assert_eq!(narrow.len(), 1);
    assert_eq!(
        core.last_frame_lines(),
        narrow.as_slice(),
        "narrow frame must not carry wide-frame residue"
    );
    assert_eq!(
        core.frame_buffer_length(),
        1,
        "Rows::trim must drop the pool tail"
    );
    for line in core.last_frame_lines() {
        assert_eq!(
            line.chars().count(),
            20,
            "row is exactly the narrow frame width, no wide residue"
        );
        assert!(!line.contains("WIDEMARKER"));
    }
    // The painted baseline shrank with the frame (no leftover rows below).
    assert_eq!(core.last_screen_lines().len(), 1);

    // #4: growing back into more rows than the pool holds (fresh slots are
    // claimed, still without residue), then an unchanged re-render.
    text.set_text(wide_tall_text());
    core.render();
    let tall_again = text.render(20);
    assert!(tall_again.len() > 1);
    assert_eq!(core.last_frame_lines(), tall_again.as_slice());
    core.render();
    assert_eq!(core.last_frame_lines(), tall_again.as_slice());
}

#[test]
fn scrolled_frame_is_idempotent_and_paints_exactly_the_window() {
    let (width, height) = (20usize, 6usize);
    let terminal = Terminal::new();
    terminal.set_size(width, height);
    let mut core = TuiCore::new(terminal);
    let text = Arc::new(Text::with_padding(numbered_transcript(30), 0, 0));
    core.push_content(Box::new(SwappableText {
        text: Arc::clone(&text),
    }));

    // Follow frame first (the baseline the scrolled frame diffs against).
    core.render();
    let frame = text.render(width);
    assert_eq!(frame.len(), 30);
    assert_eq!(core.last_frame_lines(), frame.as_slice());

    // The window the scrolled path must paint for an offset, derived from the
    // same public geometry the renderer uses, plus the per-line reset.
    let geometry = viewport_geometry(frame.len(), 0, height);
    let expected_window = |offset: usize| -> Vec<String> {
        let end = geometry.transcript_end - offset;
        let start = end - geometry.viewport_height;
        frame[start..end]
            .iter()
            .map(|line| format!("{line}{SEGMENT_RESET}"))
            .collect()
    };
    let window_top = |offset: usize| geometry.transcript_end - offset - geometry.viewport_height;

    core.set_scroll_offset(5);
    core.render();
    assert_eq!(core.scroll_offset(), 5);
    assert_eq!(core.visible_top(), window_top(5));
    assert_eq!(core.last_screen_lines(), expected_window(5));
    // The raw frame is still the whole transcript, not the window.
    assert_eq!(core.last_frame_lines(), frame.as_slice());

    // A second scrolled render recycles `scrolled_scratch` and takes the diff
    // branch; the painted window must be byte-identical.
    core.render();
    assert_eq!(core.last_screen_lines(), expected_window(5));

    // A different window overwrites the recycled rows completely: a stale row
    // from the previous window would survive `clone_from` untouched only if a
    // slot were skipped, so pin the exact content.
    core.set_scroll_offset(10);
    core.render();
    assert_eq!(core.scroll_offset(), 10);
    assert_eq!(core.visible_top(), window_top(10));
    let window = expected_window(10);
    assert_eq!(core.last_screen_lines(), window);
    assert_eq!(window.len(), height);
    assert_ne!(expected_window(5), window);
}

#[test]
fn raw_frame_stays_undecorated_while_painted_frame_is_decorated() {
    let terminal = Terminal::new();
    terminal.set_size(40, 10);
    let mut core = TuiCore::new(terminal);
    core.push_content(Box::new(Text::new("decorated transcript")));
    core.set_decorator(Some(Box::new(
        |lines: &mut Vec<String>, _ctx: FrameDecorateContext| {
            for line in lines.iter_mut() {
                line.insert_str(0, "<D>");
            }
        },
    )));

    core.render();
    let raw = core.last_frame_lines().to_vec();
    let painted = core.last_screen_lines();
    assert!(!raw.is_empty());
    assert!(raw.iter().all(|line| !line.starts_with("<D>")));
    assert_eq!(painted.len(), raw.len());
    for (painted_line, raw_line) in painted.iter().zip(raw.iter()) {
        assert_eq!(*painted_line, format!("<D>{raw_line}{SEGMENT_RESET}"));
    }

    // An unchanged re-render must not disturb either buffer.
    core.render();
    assert_eq!(core.last_frame_lines(), raw.as_slice());
    assert_eq!(core.last_screen_lines(), painted);
}

// ---------------------------------------------------------------------------
// 4. Message components (the messages.rs canvas migration)
// ---------------------------------------------------------------------------
//
// The migration rewrote the message components' row construction:
// `paint_padded_line`/`paint_blank_row` returning `String` became
// `push_painted_row`/`push_painted_blank_row` writing into `Rows`,
// `pad_prefixed_to_width` call sites became `push_prefixed_row`,
// `truncate_to_visual_lines` was inlined as `wrap + slice`, `segment_lines`
// became `segment_lines_into`, `render_edit_block`/`render_user_bash` write
// into `out`, and both `render`s became bridges over `render_into`.
//
// The tests below give these components the coverage the core components
// already had (differential parity on a fresh and a poisoned pool), plus a
// reference construction rebuilt from the PRE-migration formulas themselves
// (`.pir-foreman-backup-20261014-1543/lane64-msgcanvas/messages.rs`), so a
// byte difference introduced by the rewrite fails here instead of reaching a
// terminal.

/// Collapsed shell preview length: `BASH_PREVIEW_LINES`, private in
/// `messages.rs` (the collapsed tool-box slice is built from this value).
const BASH_PREVIEW_LINES: usize = 5;
/// Collapsed `!` preview length: `BASH_EXECUTION_PREVIEW_LINES`, private in
/// `messages.rs`.
const BASH_EXECUTION_PREVIEW_LINES: usize = 20;

// --- fixtures --------------------------------------------------------------

fn text_block(text: &str) -> ContentBlock {
    ContentBlock::Text(TextContent {
        text: text.to_string(),
        text_signature: None,
    })
}

fn thinking_block(thinking: &str) -> ContentBlock {
    ContentBlock::Thinking(ThinkingContent {
        thinking: thinking.to_string(),
        thinking_signature: None,
        redacted: false,
    })
}

/// Text AND thinking blocks: the text segments are cached per render position,
/// so the second `render` on this message is the segment-cache hit path the
/// migration rewrote (`segment_lines` clone → `segment_lines_into` pushes).
/// The content carries wide graphemes (7 CJK chars = 14 cells, 2 emoji = 4
/// cells) so the padding is measured, not counted.
fn assistant_text_thinking_message() -> Arc<AssistantMessage> {
    Arc::new(AssistantMessage {
        content: vec![
            text_block("assistant text 日本語テキスト 🙂🙂"),
            thinking_block("thinking run one 日本語\n\nthinking run two 🙂"),
            text_block("tail text after the thinking run, long enough to wrap at narrow widths"),
        ],
        ..AssistantMessage::default()
    })
}

fn assistant_text_thinking_block(
    output_pad: usize,
    hide_thinking: bool,
) -> AssistantMessageComponent {
    AssistantMessageComponent::new_from(
        assistant_text_thinking_message(),
        hide_thinking,
        output_pad,
    )
}

/// `tag line 01` … `tag line NN`: zero-padded, so a "kept tail" assertion
/// cannot collide with the head's numbering.
fn numbered_output(lines: usize, tag: &str) -> String {
    (1..=lines)
        .map(|index| format!("{tag} line {index:02}"))
        .collect::<Vec<_>>()
        .join("\n")
}

fn tool_component(
    tool_name: &str,
    args: serde_json::Value,
    expanded: bool,
) -> ToolExecutionComponent {
    ToolExecutionComponent::new(
        tool_name,
        Arc::new(args),
        Arc::new(AtomicBool::new(expanded)),
    )
}

fn settle(component: &ToolExecutionComponent, output: &str, duration_ms: Option<u64>) {
    component.update_result(Arc::from(output), false, duration_ms);
}

/// Collapsed shell box with hidden lines: 12 output lines leave more than
/// [`BASH_PREVIEW_LINES`] visual lines at every test width, so the block
/// renders the earlier-lines hint and the kept END slice.
fn bash_collapsed_with_hidden_lines() -> ToolExecutionComponent {
    let component = tool_component("bash", serde_json::json!({ "command": "seq 1 12" }), false);
    settle(&component, &numbered_output(12, "out"), None);
    component
}

fn bash_expanded_with_output() -> ToolExecutionComponent {
    let component = tool_component("bash", serde_json::json!({ "command": "seq 1 12" }), true);
    settle(&component, &numbered_output(12, "out"), None);
    component
}

/// Pending shell box: no result rows yet (the loader row belongs to the `!`
/// user-bash block only).
fn bash_pending() -> ToolExecutionComponent {
    tool_component("bash", serde_json::json!({ "command": "sleep 1" }), false)
}

/// Settled shell with a recorded duration: the timer tail (`Took 1.5s`) is the
/// one state the reference construction below cannot rebuild (its text comes
/// from the private `format_duration`), so the parity test covers it instead.
fn bash_settled_with_timer() -> ToolExecutionComponent {
    let component = tool_component("bash", serde_json::json!({ "command": "sleep 1" }), false);
    settle(&component, "done", Some(1500));
    component
}

/// A tool without a builtin renderer: the generic fallback keeps the first
/// `FALLBACK_PREVIEW_LINES` (10) logical lines plus the `... (N more lines,
/// ctrl+o to expand)` hint row.
fn generic_tool_fallback() -> ToolExecutionComponent {
    let component = tool_component(
        "custom_tool",
        serde_json::json!({ "alpha": 1, "beta": "two", "gamma": [1, 2, 3] }),
        false,
    );
    settle(&component, &numbered_output(14, "row"), None);
    component
}

fn user_bash_component(
    args: serde_json::Value,
    output: Option<&str>,
    expanded: bool,
) -> ToolExecutionComponent {
    let component = tool_component("bash", args, expanded);
    if let Some(output) = output {
        settle(&component, output, None);
    }
    component
}

/// Collapsed `!` block settled with output: 25 logical lines exceed the
/// 20-line preview, so the `... 5 more lines (` hint and the `(exit 3)` tail
/// render after the kept-end preview.
fn user_bash_collapsed_with_tail() -> ToolExecutionComponent {
    user_bash_component(
        serde_json::json!({
            "command": "echo 🙂🙂",
            "userBash": true,
            "exitCode": 3,
        }),
        Some(&numbered_output(25, "jb")),
        false,
    )
}

/// Pending `!` block: the Loader row (spinner + muted cancel hint).
fn user_bash_pending() -> ToolExecutionComponent {
    user_bash_component(
        serde_json::json!({ "command": "echo 🙂🙂", "userBash": true }),
        None,
        false,
    )
}

/// Expanded `!` block whose output carries ANSI SGR runs: the padded rows
/// measure the wrapped visual lines with the escapes present.
fn user_bash_expanded() -> ToolExecutionComponent {
    user_bash_component(
        serde_json::json!({ "command": "printf 日本語テキスト", "userBash": true }),
        Some("\x1b[31mred 日本語\x1b[0m plain\n\x1b[1;32mbold green 🙂\x1b[0m end"),
        true,
    )
}

/// `!!` (excludeFromContext) block that was cancelled: dim frame, no exit
/// code, the `(cancelled)` tail.
fn user_bash_cancelled_excluded() -> ToolExecutionComponent {
    user_bash_component(
        serde_json::json!({
            "command": "sleep 5",
            "userBash": true,
            "excludeFromContext": true,
            "cancelled": true,
        }),
        Some("partial output"),
        false,
    )
}

fn edit_args() -> serde_json::Value {
    serde_json::json!({
        "path": "src/lib.rs",
        "edits": [{ "oldText": "old 日本語", "newText": "new 🙂" }],
    })
}

/// The preview the app attaches for the block's args: its `args_key` must
/// match `edit_preview_input` (`preview_is_current`), else the block drops it.
fn matching_preview(args: &serde_json::Value, diff: &str) -> ToolPreview {
    let input = edit_preview_input(args).expect("edit args carry a preview input");
    ToolPreview {
        args_key: input.args_key,
        diff: Some(diff.to_string()),
        first_changed_line: Some(1),
        error: None,
    }
}

/// Edit block with a resolved preview AND a result diff that differs from the
/// preview's: the box renders the preview, the unpainted result section below
/// it renders the result diff.
fn edit_with_preview_and_result() -> ToolExecutionComponent {
    let args = edit_args();
    let component = tool_component("edit", args.clone(), false);
    component.set_meta(
        Some(Arc::new(serde_json::json!({
            "diff": result_diff(),
        }))),
        None,
        Some(matching_preview(&args, edit_preview_diff())),
    );
    settle(&component, "applied 1 edit", None);
    component
}

fn edit_preview_diff() -> &'static str {
    " 1 context\n-1 old 日本語\n+1 new 🙂"
}

fn result_diff() -> &'static str {
    " 1 context\n-1 preview old\n+1 preview new\n-2 second old\n+2 second 🙂"
}

/// Edit block whose preview failed to apply: the error background and the
/// error text inside the box, no result diff.
fn edit_preview_error() -> ToolExecutionComponent {
    let args = edit_args();
    let component = tool_component("edit", args.clone(), false);
    component.set_meta(
        None,
        None,
        Some(ToolPreview {
            args_key: edit_preview_input(&args).expect("input").args_key,
            diff: None,
            first_changed_line: None,
            error: Some("oldText not found".to_string()),
        }),
    );
    settle(&component, "oldText not found", None);
    component
}

/// Pending edit (no preview, no result): the pending background and the call
/// line only.
fn edit_pending() -> ToolExecutionComponent {
    tool_component("edit", edit_args(), false)
}

/// `update_result` with `is_error`: [`settle`] pins the success phase, so the
/// error phase (`toolErrorBg`) needs its own setter.
fn fail(component: &ToolExecutionComponent, output: &str, duration_ms: Option<u64>) {
    component.update_result(Arc::from(output), true, duration_ms);
}

/// Settled shell tool in the ERROR phase: the box flips to `toolErrorBg` (the
/// `bg_edges` color no other fixture reaches) and the failure text renders in
/// place of the output.
fn bash_error() -> ToolExecutionComponent {
    let component = tool_component("bash", serde_json::json!({ "command": "false" }), false);
    fail(&component, &numbered_output(12, "err"), None);
    component
}

/// Expanded `!` block whose output exceeds the 20-line preview: the hidden
/// lines render the `(` + ctrl+o + `)` COLLAPSE hint — the expanded
/// counterpart of the collapsed `... N more lines (` hint, a separate branch
/// of the status tail the migration rewrote.
fn user_bash_expanded_with_hidden() -> ToolExecutionComponent {
    user_bash_component(
        serde_json::json!({ "command": "seq 1 25", "userBash": true }),
        Some(&numbered_output(25, "jb")),
        true,
    )
}

/// ANSI-styled output long enough to trip the collapsed `!` preview: every
/// line carries SGR runs, so the kept END of the styled wrap is the case where
/// the inlined `wrap + slice` and the pre-migration `truncate_to_visual_lines`
/// re-wrap could disagree.
fn ansi_numbered_output(lines: usize) -> String {
    (1..=lines)
        .map(|index| format!("\x1b[31mred\x1b[0m jb line {index:02} 🙂"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Collapsed `!` block over [`ansi_numbered_output`]: the kept-end preview
/// plus the `... 5 more lines (` hint.
fn user_bash_collapsed_ansi_over_preview() -> ToolExecutionComponent {
    user_bash_component(
        serde_json::json!({ "command": "printf 🙂🙂", "userBash": true }),
        Some(&ansi_numbered_output(25)),
        false,
    )
}

/// Single-line replacement sharing no token: `render_intra_line_diff`'s
/// inverse-video fallback marks the whole token pair (`\x1b[7m…\x1b[27m`), so
/// the legacy edit body is a closed-form two-line diff the reference can
/// rebuild without the private formatter.
fn legacy_edit_args() -> serde_json::Value {
    serde_json::json!({
        "path": "src/lib.rs",
        "edits": [{ "oldText": "old", "newText": "new" }],
    })
}

/// Settled edit with NO meta attached: the block takes the pre-details replay
/// branch inside the box (`edit_result_body`'s args-rebuilt intra-line diff)
/// and renders no result section.
fn edit_legacy_settled() -> ToolExecutionComponent {
    let component = tool_component("edit", legacy_edit_args(), false);
    settle(&component, "applied 1 edit", None);
    component
}

/// Edit whose run FAILED while a preview error is current: the box renders the
/// preview error and the unpainted result section below renders the failure
/// text (`edit_result_section`'s `is_error` arm). `details` must be present
/// (empty) for that section to render at all.
fn edit_error_result() -> ToolExecutionComponent {
    let args = edit_args();
    let component = tool_component("edit", args.clone(), false);
    component.set_meta(
        Some(Arc::new(serde_json::json!({}))),
        None,
        Some(ToolPreview {
            args_key: edit_preview_input(&args).expect("input").args_key,
            diff: None,
            first_changed_line: None,
            error: Some("oldText not found".to_string()),
        }),
    );
    fail(&component, "write failed: permission denied", None);
    component
}

// --- pre-migration reference formulas --------------------------------------
//
// Copied from `.pir-foreman-backup-20261014-1543/lane64-msgcanvas/messages.rs`
// (the pre-migration `src/tui/messages.rs`), whose functions the canvas
// migration replaced with pooled builders. Each reference returns exactly the
// bytes the old function returned, so a rewrite that changed a row's bytes
// (padding measured on the wrong span, a dropped background close, the preview
// keeping the wrong end) makes the component's `render` differ from them.
//
// `visible_width` is public (`pir::tui::ansi`), so the width measurement is the
// production one, not a reimplementation.

/// `bg_edges` (`messages.rs`, private): `Theme::bg` renders
/// `{open}{text}\x1b[49m`, so painting an empty string splits the pair out.
fn reference_bg_edges(color: ThemeColor) -> (String, String) {
    let probe = with_theme(|theme| theme.bg(color, ""));
    match probe.strip_suffix(BG_RESET) {
        Some(open) => (open.to_string(), BG_RESET.to_string()),
        None => (String::new(), String::new()),
    }
}

/// Pre-migration `paint_padded_line`: `open` + `prefix` + `text`, padded to
/// `width` with the pad measured on the prefixed text alone, then `close`.
fn reference_paint_padded_line(
    open: &str,
    close: &str,
    prefix: &str,
    text: &str,
    width: usize,
) -> String {
    let mut out = String::new();
    out.push_str(open);
    let body = out.len();
    out.push_str(prefix);
    out.push_str(text);
    let pad = width.saturating_sub(visible_width(&out[body..]));
    for _ in 0..pad {
        out.push(' ');
    }
    out.push_str(close);
    out
}

/// Pre-migration `paint_blank_row`: the background pair around `width` spaces.
fn reference_paint_blank_row(open: &str, close: &str, width: usize) -> String {
    let mut out = String::new();
    out.push_str(open);
    for _ in 0..width {
        out.push(' ');
    }
    out.push_str(close);
    out
}

/// `pad_prefixed_to_width`'s row shape (`ansi.rs`, still public): `prefix` +
/// `text` padded to `width`, measuring the concatenation. The migration's
/// `push_prefixed_row` is this formula written into a pooled row.
fn reference_pad_prefixed_row(prefix: &str, text: &str, width: usize) -> String {
    let mut out = String::new();
    out.push_str(prefix);
    out.push_str(text);
    let pad = width.saturating_sub(visible_width(&out));
    for _ in 0..pad {
        out.push(' ');
    }
    out
}

/// Pre-migration `truncate_to_visual_lines` (`messages.rs`, removed by the
/// migration): wrap to the width FIRST, then keep `max_visual_lines` of the
/// resulting visual lines, `keep_end` selecting the tail. The user-bash
/// collapsed preview inlined this as `wrap + slice`; the reference pins that
/// equivalence.
fn reference_truncate_to_visual_lines(
    text: &str,
    max_visual_lines: usize,
    width: usize,
    keep_end: bool,
) -> (Vec<String>, usize) {
    if text.is_empty() {
        return (Vec::new(), 0);
    }
    let visual = wrap_text_with_ansi(text, width);
    if visual.len() <= max_visual_lines {
        return (visual, 0);
    }
    let skipped = visual.len() - max_visual_lines;
    let kept = if keep_end {
        visual[visual.len() - max_visual_lines..].to_vec()
    } else {
        visual[..max_visual_lines].to_vec()
    };
    (kept, skipped)
}

/// `expand_key_hint` (`messages.rs`, private). Takes the borrowed theme
/// because `with_theme` is not reentrant — the production hint closures call
/// it the same way.
fn reference_expand_key_hint(theme: &Theme, description: &str) -> String {
    format!(
        "{}{}",
        theme.fg(ThemeColor::Dim, "ctrl+o"),
        theme.fg(ThemeColor::Muted, &format!(" {description}"))
    )
}

/// `earlier_lines_hint` (`messages.rs`, private).
fn reference_earlier_lines_hint(theme: &Theme, skipped: usize) -> String {
    format!(
        "{} {}{}",
        theme.fg(ThemeColor::Muted, &format!("... ({skipped} earlier lines,")),
        reference_expand_key_hint(theme, "to expand"),
        theme.fg(ThemeColor::Muted, ")")
    )
}

/// The rows every wrapped visual line produces at the
/// `pad_prefixed_to_width(" ", visual, width)` call sites (the user-bash block
/// and the edit result section).
fn reference_push_prefixed_rows(rows: &mut Vec<String>, visual: &[String], width: usize) {
    for line in visual {
        rows.push(reference_pad_prefixed_row(" ", line, width));
    }
}

/// `render_user_bash` (pre-migration) for one state: args, output, pending and
/// expansion are its only inputs.
fn reference_user_bash_rows(
    args: &serde_json::Value,
    output: &str,
    pending: bool,
    expanded: bool,
    width: usize,
) -> Vec<String> {
    let exclude = args
        .get("excludeFromContext")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    let color = if exclude {
        ThemeColor::Dim
    } else {
        ThemeColor::BashMode
    };
    let command = args
        .get("command")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("...");
    let exit_code = args.get("exitCode").and_then(serde_json::Value::as_i64);
    let cancelled = args
        .get("cancelled")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    let content_width = width.saturating_sub(2).max(1);
    let mut rows: Vec<String> = Vec::new();
    // Spacer(1) + top DynamicBorder.
    rows.push(String::new());
    rows.push(with_theme(|theme| {
        theme.fg(color, &"─".repeat(width.max(1)))
    }));
    // Header.
    let header = with_theme(|theme| theme.fg(color, &theme.bold(&format!("$ {command}"))));
    reference_push_prefixed_rows(
        &mut rows,
        &wrap_text_with_ansi(&header, content_width),
        width,
    );
    // Context truncation, then the preview slice.
    let truncated = truncate_tail(output, DEFAULT_MAX_LINES, DEFAULT_MAX_BYTES);
    let available: Vec<&str> = if truncated.content.is_empty() {
        Vec::new()
    } else {
        truncated.content.split('\n').collect()
    };
    let preview = &available[available.len().saturating_sub(BASH_EXECUTION_PREVIEW_LINES)..];
    let hidden = available.len() - preview.len();
    if !available.is_empty() {
        if expanded {
            let display = available
                .iter()
                .map(|line| with_theme(|theme| theme.fg(ThemeColor::Muted, line)))
                .collect::<Vec<_>>()
                .join("\n");
            reference_push_prefixed_rows(
                &mut rows,
                &wrap_text_with_ansi(&format!("\n{display}"), content_width),
                width,
            );
        } else {
            let styled = preview
                .iter()
                .map(|line| with_theme(|theme| theme.fg(ThemeColor::Muted, line)))
                .collect::<Vec<_>>()
                .join("\n");
            let (kept, _) = reference_truncate_to_visual_lines(
                &format!("\n{styled}"),
                BASH_EXECUTION_PREVIEW_LINES,
                content_width,
                true,
            );
            reference_push_prefixed_rows(&mut rows, &kept, width);
        }
    }
    // Loader / status.
    if pending {
        let loader = format!(
            "{} {}",
            with_theme(|theme| theme.fg(color, spinner_frame(0))),
            with_theme(|theme| theme.fg(ThemeColor::Muted, "Running... (escape/ctrl+c to cancel)"))
        );
        rows.push(String::new());
        reference_push_prefixed_rows(
            &mut rows,
            &wrap_text_with_ansi(&loader, content_width),
            width,
        );
    } else {
        let mut parts: Vec<String> = Vec::new();
        if hidden > 0 {
            if expanded {
                parts.push(with_theme(|theme| {
                    format!(
                        "{}{}{}",
                        theme.fg(ThemeColor::Muted, "("),
                        reference_expand_key_hint(theme, "to collapse"),
                        theme.fg(ThemeColor::Muted, ")")
                    )
                }));
            } else {
                parts.push(with_theme(|theme| {
                    format!(
                        "{} {} {}",
                        theme.fg(ThemeColor::Muted, &format!("... {hidden} more lines (")),
                        reference_expand_key_hint(theme, "to expand"),
                        theme.fg(ThemeColor::Muted, ")")
                    )
                }));
            }
        }
        if cancelled {
            parts.push(with_theme(|theme| {
                theme.fg(ThemeColor::Warning, "(cancelled)")
            }));
        } else if let Some(code) = exit_code.filter(|code| *code != 0) {
            parts.push(with_theme(|theme| {
                theme.fg(ThemeColor::Error, &format!("(exit {code})"))
            }));
        }
        if !parts.is_empty() {
            rows.push(String::new());
            for part in parts {
                reference_push_prefixed_rows(
                    &mut rows,
                    &wrap_text_with_ansi(&part, content_width),
                    width,
                );
            }
        }
    }
    // Bottom DynamicBorder.
    rows.push(with_theme(|theme| {
        theme.fg(color, &"─".repeat(width.max(1)))
    }));
    rows
}

/// `ToolExecutionComponent`'s default box (a settled shell tool): the
/// pre-migration `render` body for that state. `details` is `None` in most
/// fixtures — `strip_shell_footer` is then the identity and there are no
/// warning rows — and the truncation fixture passes the block's details.
fn reference_tool_box_bash_rows(
    args: &serde_json::Value,
    output: &str,
    details: Option<&serde_json::Value>,
    expanded: bool,
    width: usize,
) -> Vec<String> {
    reference_tool_box_rows_with_bg(
        ThemeColor::ToolSuccessBg,
        args,
        output,
        details,
        expanded,
        width,
    )
}

/// [`reference_tool_box_bash_rows`] for an explicit phase color: a shell
/// tool's box body is phase-independent (only `bg_edges` differs), so the
/// pending (`toolPendingBg`) box and the error (`toolErrorBg`) box rebuild
/// from the same formula with a different background pair.
fn reference_tool_box_rows_with_bg(
    bg_color: ThemeColor,
    args: &serde_json::Value,
    output: &str,
    details: Option<&serde_json::Value>,
    expanded: bool,
    width: usize,
) -> Vec<String> {
    let (bg_open, bg_close) = reference_bg_edges(bg_color);
    let content_width = width.saturating_sub(2).max(1);
    let mut rows: Vec<String> = vec![
        String::new(),
        reference_paint_blank_row(&bg_open, &bg_close, width),
    ];
    let call_line = format_tool_call_line("bash", args, expanded);
    for visual in wrap_text_with_ansi(&call_line, content_width) {
        rows.push(reference_paint_padded_line(
            &bg_open, &bg_close, " ", &visual, width,
        ));
    }
    let output = strip_shell_footer(output.trim(), details);
    if !output.is_empty() {
        let mut styled = String::with_capacity(output.len());
        for (index, line) in output.lines().enumerate() {
            if index > 0 {
                styled.push('\n');
            }
            styled.push_str(&with_theme(|theme| theme.fg(ThemeColor::ToolOutput, line)));
        }
        let visual = wrap_text_with_ansi(&styled, content_width);
        if expanded {
            rows.push(reference_paint_blank_row(&bg_open, &bg_close, width));
            for line in &visual {
                rows.push(reference_paint_padded_line(
                    &bg_open, &bg_close, " ", line, width,
                ));
            }
        } else {
            rows.push(reference_paint_blank_row(&bg_open, &bg_close, width));
            let (kept, skipped): (&[String], usize) = if visual.len() <= BASH_PREVIEW_LINES {
                (&visual, 0)
            } else {
                let skipped = visual.len() - BASH_PREVIEW_LINES;
                (&visual[skipped..], skipped)
            };
            if skipped > 0 {
                let hint = with_theme(|theme| reference_earlier_lines_hint(theme, skipped));
                for line in wrap_text_with_ansi(&hint, content_width) {
                    rows.push(reference_paint_padded_line(
                        &bg_open, &bg_close, " ", &line, width,
                    ));
                }
            }
            for line in kept {
                rows.push(reference_paint_padded_line(
                    &bg_open, &bg_close, " ", line, width,
                ));
            }
        }
    }
    if let Some(warning) = shell_truncation_warning(details) {
        rows.push(reference_paint_blank_row(&bg_open, &bg_close, width));
        for line in wrap_text_with_ansi(&warning, content_width) {
            rows.push(reference_paint_padded_line(
                &bg_open, &bg_close, " ", &line, width,
            ));
        }
    }
    // `tool_timer_tail` is `None` for these fixtures (`durationMs` and
    // `startedAt` absent). Its row text comes from the private
    // `format_duration`, so the timer state is not reference-covered here and
    // the parity test pins it instead.
    rows.push(reference_paint_blank_row(&bg_open, &bg_close, width));
    rows
}

/// `render_edit_block` (pre-migration) for a block with `is_error == false`:
/// the box around the call line and the preview body, then the unpainted
/// result section when the result diff differs from the preview's.
///
/// The call-line formatter ignores `expanded` for `edit`, so the fixtures are
/// all collapsed. The `is_error` branch of `edit_result_section` (which
/// compares the trimmed output against the preview error) is not rebuilt here;
/// the parity test covers an error result differentially.
fn reference_edit_rows(
    args: &serde_json::Value,
    preview: Option<&ToolPreview>,
    result_diff: Option<&str>,
    pending: bool,
    width: usize,
) -> Vec<String> {
    let preview_diff = preview.and_then(|preview| preview.diff.as_deref());
    let bg_color = match preview {
        Some(preview) if preview.error.is_some() => ThemeColor::ToolErrorBg,
        Some(_) => ThemeColor::ToolSuccessBg,
        None if pending => ThemeColor::ToolPendingBg,
        None => ThemeColor::ToolSuccessBg,
    };
    let (bg_open, bg_close) = reference_bg_edges(bg_color);
    let content_width = width.saturating_sub(2).max(1);
    let mut rows: Vec<String> = Vec::new();
    // Outer Container: Spacer(1) + the call Box(1, 1, bg).
    rows.push(String::new());
    rows.push(reference_paint_blank_row(&bg_open, &bg_close, width));
    let call_line = format_tool_call_line("edit", args, false);
    for visual in wrap_text_with_ansi(&call_line, content_width) {
        rows.push(reference_paint_padded_line(
            &bg_open, &bg_close, " ", &visual, width,
        ));
    }
    if let Some(preview) = preview {
        // Spacer(1) + the preview body inside the box.
        rows.push(reference_paint_blank_row(&bg_open, &bg_close, width));
        let body = match preview.error.as_deref() {
            Some(error) => with_theme(|theme| theme.fg(ThemeColor::Error, error)),
            None => render_diff_text(preview.diff.as_deref().unwrap_or_default()),
        };
        for visual in wrap_text_with_ansi(&body, content_width) {
            rows.push(reference_paint_padded_line(
                &bg_open, &bg_close, " ", &visual, width,
            ));
        }
    }
    rows.push(reference_paint_blank_row(&bg_open, &bg_close, width));
    // Result section below the box (unpainted): `details["diff"]` renders only
    // when it differs from the preview diff.
    if let Some(result_diff) = result_diff {
        if Some(result_diff) != preview_diff {
            rows.push(String::new());
            for visual in wrap_text_with_ansi(&render_diff_text(result_diff), content_width) {
                rows.push(reference_pad_prefixed_row(" ", &visual, width));
            }
        }
    }
    rows
}

/// `render_edit_block`'s pre-details replay branch (settled, no `details`, no
/// preview): the same box around the call line, with the args-rebuilt body
/// `edit_result_body` returns for a single-line pair — the intra-line
/// inverse-video lines (SGR 7/27, `render_intra_line_diff`'s fallback when the
/// two lines share no token) — and no result section.
fn reference_edit_legacy_rows(args: &serde_json::Value, width: usize) -> Vec<String> {
    let (bg_open, bg_close) = reference_bg_edges(ThemeColor::ToolSuccessBg);
    let content_width = width.saturating_sub(2).max(1);
    let mut rows: Vec<String> = Vec::new();
    rows.push(String::new());
    rows.push(reference_paint_blank_row(&bg_open, &bg_close, width));
    let call_line = format_tool_call_line("edit", args, false);
    for visual in wrap_text_with_ansi(&call_line, content_width) {
        rows.push(reference_paint_padded_line(
            &bg_open, &bg_close, " ", &visual, width,
        ));
    }
    // `edit_result_body` wraps the two inverse-video lines in `\n…`, which the
    // renderer trims before wrapping them at the box content width.
    let body = format!(
        "{}\n{}",
        with_theme(|theme| theme.fg(ThemeColor::ToolDiffRemoved, "-\x1b[7mold\x1b[27m")),
        with_theme(|theme| theme.fg(ThemeColor::ToolDiffAdded, "+\x1b[7mnew\x1b[27m")),
    );
    for visual in wrap_text_with_ansi(&body, content_width) {
        rows.push(reference_paint_padded_line(
            &bg_open, &bg_close, " ", &visual, width,
        ));
    }
    rows.push(reference_paint_blank_row(&bg_open, &bg_close, width));
    rows
}

/// `UserMessageComponent::new`'s pre-migration body (`self.boxed.render`):
/// the padded background box around the message markdown, built here from the
/// same public `BoxContainer` + `Markdown` pieces and the user-message markdown
/// options. The migrated `render_into` only delegates into the box, so a
/// difference means the box construction (padding, painter color, markdown
/// options) or the delegation changed.
fn reference_user_message_rows(text: &str, output_pad: usize, width: usize) -> Vec<String> {
    let mut boxed = BoxContainer::new(output_pad, 1, |line: &str| {
        with_theme(|theme| theme.bg(ThemeColor::UserMessageBg, line))
    });
    boxed.add_child(Markdown::with_options(
        text,
        0,
        0,
        MarkdownTheme::default(),
        MarkdownOptions {
            preserve_ordered_list_markers: true,
            preserve_backslash_escapes: true,
            ..MarkdownOptions::default()
        },
    ));
    boxed.render(width)
}

/// Whether a content block renders a body segment (`has_visible_content` /
/// the `has_after` separator check).
fn block_is_visible(block: &ContentBlock) -> bool {
    match block {
        ContentBlock::Text(text) => !text.text.trim().is_empty(),
        ContentBlock::Thinking(thinking) => !thinking.thinking.trim().is_empty(),
        _ => false,
    }
}

/// The assistant component's body (the segment walk in `render`, whose
/// per-segment `render_segment_lines` the migration folded into
/// `segment_lines_into`), rebuilt from the public [`Markdown`] component —
/// the same arguments `render_segment_lines` passed to the private
/// `render_padded_lines`.
fn reference_assistant_rows(
    message: &AssistantMessage,
    output_pad: usize,
    hide_thinking: bool,
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
                if hide_thinking {
                    // The single `{pad}{label}` line, italic in ThinkingText.
                    let pad = " ".repeat(output_pad);
                    rows.push(with_theme(|theme| {
                        format!(
                            "{pad}{}",
                            theme.italic(&theme.fg(ThemeColor::ThinkingText, "Thinking..."))
                        )
                    }));
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

// --- 4.1 render ↔ render_into parity ---------------------------------------

/// Byte parity of `render` and `render_into` for every message-component state
/// the migration touched, at every width in [`WIDTHS`], on a fresh pool and on
/// a poisoned one.
///
/// The helper calls `render` first, then goes through `render_into` twice; for
/// the assistant fixtures the first call is the segment-cache miss and both
/// `render_into` calls are the cache HIT path (`segment_lines_into` pushing
/// the cached lines into pooled rows), so a failure on those rows means the
/// cache-copy path changed bytes — a leaked/over-long row from the pool, a
/// padding regression, or the pool tail not trimmed.
#[test]
fn message_components_match_render_into_on_fresh_and_poisoned_pools() {
    // Reinstalls the built-in palette the process starts on (identical bytes,
    // so it cannot disturb a concurrently running test), pinning the escapes
    // the painted rows carry.
    set_theme("dark");

    let fixtures: Vec<(&str, Box<dyn Component>)> = vec![
        (
            "assistant-text-thinking",
            Box::new(assistant_text_thinking_block(1, false)),
        ),
        (
            "assistant-thinking-hidden",
            Box::new(assistant_text_thinking_block(1, true)),
        ),
        (
            "assistant-pad-0",
            Box::new(assistant_text_thinking_block(0, false)),
        ),
        (
            "user-message-padded",
            Box::new(UserMessageComponent::new(
                "user asks 日本語テキスト 🙂\n\nsecond line",
                1,
            )),
        ),
        (
            "user-message-pad-0",
            Box::new(UserMessageComponent::new("no pad user message", 0)),
        ),
        (
            "tool-bash-collapsed-hidden",
            Box::new(bash_collapsed_with_hidden_lines()),
        ),
        ("tool-bash-expanded", Box::new(bash_expanded_with_output())),
        ("tool-bash-pending", Box::new(bash_pending())),
        ("tool-bash-error", Box::new(bash_error())),
        (
            "tool-bash-settled-timer",
            Box::new(bash_settled_with_timer()),
        ),
        ("tool-fallback-collapsed", Box::new(generic_tool_fallback())),
        (
            "user-bash-collapsed-tail",
            Box::new(user_bash_collapsed_with_tail()),
        ),
        (
            "user-bash-collapsed-ansi-keep-end",
            Box::new(user_bash_collapsed_ansi_over_preview()),
        ),
        ("user-bash-pending-loader", Box::new(user_bash_pending())),
        ("user-bash-expanded-ansi", Box::new(user_bash_expanded())),
        (
            "user-bash-expanded-collapse-hint",
            Box::new(user_bash_expanded_with_hidden()),
        ),
        (
            "user-bash-cancelled-excluded",
            Box::new(user_bash_cancelled_excluded()),
        ),
        (
            "edit-preview-and-result",
            Box::new(edit_with_preview_and_result()),
        ),
        ("edit-preview-error", Box::new(edit_preview_error())),
        ("edit-legacy-settled", Box::new(edit_legacy_settled())),
        ("edit-error-result", Box::new(edit_error_result())),
        ("edit-pending", Box::new(edit_pending())),
    ];

    for (label, component) in &fixtures {
        assert_render_parity(component.as_ref(), label);
    }
}

// --- 4.2 reference-byte equality -------------------------------------------

/// Assert the component's public bytes for `what` equal the reference
/// construction: a failure means the migrated row formula and the
/// pre-migration formula disagree.
fn assert_reference_bytes(
    what: &str,
    component: &dyn Component,
    width: usize,
    expected: &[String],
) {
    assert_eq!(
        component.render(width),
        expected,
        "{what}: component bytes differ from the pre-migration formula @ width {width}"
    );
}

/// The migrated tool-component row formulas, rebuilt from the pre-migration
/// code above, must reproduce the component's public `render` bytes.
///
/// Kills, per case: a pad measured on `text` instead of `prefix + text`, a
/// pad counted in `chars()` instead of measured (CJK/emoji/ANSI), a dropped
/// background close on blank/painted rows, the collapsed preview keeping the
/// wrong end of the wrap, and the `!` hint/loader/tail rows losing their
/// prefix.
#[test]
fn pre_migration_row_formulas_reproduce_tool_component_bytes() {
    set_theme("dark");

    // The reimplementation must agree with the production
    // `pad_prefixed_to_width` (`ansi.rs`) on the width-ambiguous classes, so a
    // mismatch below is the component's formula, not the reference's.
    for text in [
        "日本語テキスト",
        "🙂🙂",
        "\x1b[31mred\x1b[0m plain",
        "plain ascii",
    ] {
        assert_eq!(
            reference_pad_prefixed_row(" ", text, 20),
            pad_prefixed_to_width(" ", text, 20),
            "the reference row shape diverges from pad_prefixed_to_width for {text:?}"
        );
    }
    let (bg_open, close) = reference_bg_edges(ThemeColor::ToolSuccessBg);
    assert!(
        !bg_open.is_empty(),
        "the pinned palette must carry a toolSuccessBg background, else the painted rows are untested"
    );
    assert_eq!(close, BG_RESET);

    for width in [1usize, 20, 80] {
        // user-bash, collapsed, CJK command and output: the header and preview
        // rows pad by `visible_width`, so a `chars().count()` revert
        // over-pads 日本語テキスト by 7 cells.
        let args = serde_json::json!({
            "command": "echo 日本語テキスト",
            "userBash": true,
            "exitCode": 0,
        });
        let output = "日本語テキスト\nsecond 日本語テキスト line";
        let component = user_bash_component(args.clone(), Some(output), false);
        assert_reference_bytes(
            "user-bash-cjk",
            &component,
            width,
            &reference_user_bash_rows(&args, output, false, false, width),
        );

        // user-bash, collapsed, MORE visual lines than the 20-line preview:
        // the kept END of the wrap (`truncate_to_visual_lines` keep-end, now
        // inlined) plus the `... 5 more lines (` hint. A keep-start revert
        // renders `jb line 01` instead of `jb line 25`.
        let args = serde_json::json!({ "command": "printf 🙂🙂", "userBash": true });
        let output = numbered_output(25, "jb");
        let component = user_bash_component(args.clone(), Some(&output), false);
        if width >= 20 {
            let rendered = component.render(width).join("\n");
            assert!(
                rendered.contains("jb line 25"),
                "user-bash preview must keep the END of the output @ width {width}"
            );
            assert!(
                !rendered.contains("jb line 01"),
                "user-bash preview must skip the head of the output @ width {width}"
            );
        }
        assert_reference_bytes(
            "user-bash-emoji-preview",
            &component,
            width,
            &reference_user_bash_rows(&args, &output, false, false, width),
        );

        // user-bash, expanded, ANSI-styled output: the pad is measured with
        // the SGR runs present but not counted.
        let args = serde_json::json!({ "command": "printf 日本語テキスト", "userBash": true });
        let output = "\x1b[31mred 日本語\x1b[0m plain\n\x1b[1;32mbold green 🙂\x1b[0m end";
        let component = user_bash_component(args.clone(), Some(output), true);
        assert_reference_bytes(
            "user-bash-ansi-expanded",
            &component,
            width,
            &reference_user_bash_rows(&args, output, false, true, width),
        );

        // Pending `!` block: the Loader row (spinner + muted cancel hint) with
        // the ` ` prefix of every `pad_prefixed_to_width` call site.
        let args = serde_json::json!({ "command": "sleep 5", "userBash": true });
        let component = user_bash_component(args.clone(), None, false);
        assert_reference_bytes(
            "user-bash-loader",
            &component,
            width,
            &reference_user_bash_rows(&args, "", true, false, width),
        );

        // Cancelled `!!` block: dim frame (`excludeFromContext`) and the
        // `(cancelled)` tail after the blank separator.
        let args = serde_json::json!({
            "command": "sleep 5",
            "userBash": true,
            "excludeFromContext": true,
            "cancelled": true,
        });
        let component = user_bash_component(args.clone(), Some("partial output"), false);
        assert_reference_bytes(
            "user-bash-cancelled",
            &component,
            width,
            &reference_user_bash_rows(&args, "partial output", false, false, width),
        );

        // Default tool box, collapsed: 12 output lines exceed
        // BASH_PREVIEW_LINES, so the earlier-lines hint and the kept END slice
        // render (a keep-start revert shows `out line 01`, not `out line 12`).
        let args = serde_json::json!({ "command": "seq 1 12" });
        let output = numbered_output(12, "out");
        let component = tool_component("bash", args.clone(), false);
        settle(&component, &output, None);
        if width >= 20 {
            let rendered = component.render(width).join("\n");
            assert!(
                rendered.contains("out line 12"),
                "collapsed shell preview must keep the END of the output @ width {width}"
            );
            assert!(
                !rendered.contains("out line 01"),
                "collapsed shell preview must skip the head of the output @ width {width}"
            );
        }
        assert_reference_bytes(
            "tool-bash-collapsed",
            &component,
            width,
            &reference_tool_box_bash_rows(&args, &output, None, false, width),
        );

        // Expanded shell box: every output line, no hint.
        let component = tool_component("bash", args.clone(), true);
        settle(&component, &output, None);
        assert_reference_bytes(
            "tool-bash-expanded",
            &component,
            width,
            &reference_tool_box_bash_rows(&args, &output, None, true, width),
        );

        // Settled shell carrying truncation details: the warning row block
        // after the preview (another blank + painted row pair).
        let details = serde_json::json!({
            "truncation": { "truncated": true, "truncatedBy": "lines", "outputLines": 2, "totalLines": 8 },
            "fullOutputPath": "/tmp/full-output.txt",
        });
        let args = serde_json::json!({ "command": "seq 1 8" });
        let component = tool_component("bash", args.clone(), false);
        component.set_meta(Some(Arc::new(details.clone())), None, None);
        settle(&component, &output, None);
        assert_reference_bytes(
            "tool-bash-truncation-warning",
            &component,
            width,
            &reference_tool_box_bash_rows(&args, &output, Some(&details), false, width),
        );

        // Edit block with a preview AND a result diff: the box rows plus the
        // unpainted result section (`push_prefixed_row`).
        let args = edit_args();
        let preview = matching_preview(&args, edit_preview_diff());
        let component = tool_component("edit", args.clone(), false);
        component.set_meta(
            Some(Arc::new(serde_json::json!({ "diff": result_diff() }))),
            None,
            Some(preview.clone()),
        );
        settle(&component, "applied 1 edit", None);
        assert_reference_bytes(
            "edit-preview-and-result",
            &component,
            width,
            &reference_edit_rows(&args, Some(&preview), Some(result_diff()), false, width),
        );

        // Edit block whose preview failed: the error background and the error
        // text inside the box.
        let args = edit_args();
        let preview_error = ToolPreview {
            args_key: edit_preview_input(&args).expect("input").args_key,
            diff: None,
            first_changed_line: None,
            error: Some("oldText not found".to_string()),
        };
        let component = tool_component("edit", args.clone(), false);
        component.set_meta(None, None, Some(preview_error.clone()));
        settle(&component, "oldText not found", None);
        assert_reference_bytes(
            "edit-preview-error",
            &component,
            width,
            &reference_edit_rows(&args, Some(&preview_error), None, false, width),
        );

        // Pending edit: the pending background and the call line only.
        let args = edit_args();
        let component = tool_component("edit", args.clone(), false);
        assert_reference_bytes(
            "edit-pending",
            &component,
            width,
            &reference_edit_rows(&args, None, None, true, width),
        );
    }
}

/// The pre-migration formulas for the states the first reference test does not
/// rebuild: the PENDING shell box (`toolPendingBg`), the ERROR shell box
/// (`toolErrorBg`, expanded and collapsed), the expanded `!` block whose hidden
/// lines render the COLLAPSE hint, the collapsed `!` preview over ANSI-styled
/// output longer than the preview, the legacy edit replay body, and the edit
/// error-result section.
///
/// Kills, per case: a phase that paints the wrong background pair (`bg_edges`
/// fed the success color for a pending/error block), a dropped background close
/// on the spacer rows, the expanded `!` tail losing the hint branch, the
/// inlined `wrap + slice` keeping the wrong end of a STYLED wrap, the legacy
/// edit body dropping out of the box, and the error result text losing its
/// `push_prefixed_row` prefix.
#[test]
fn pre_migration_row_formulas_reproduce_the_remaining_tool_states() {
    set_theme("dark");

    for width in [1usize, 20, 80] {
        // Pending shell box: the pending background, the call line, and no
        // result rows (no output yet, and no timer without `startedAt`).
        let args = serde_json::json!({ "command": "sleep 1" });
        let component = tool_component("bash", args.clone(), false);
        assert_reference_bytes(
            "tool-bash-pending-box",
            &component,
            width,
            &reference_tool_box_rows_with_bg(
                ThemeColor::ToolPendingBg,
                &args,
                "",
                None,
                false,
                width,
            ),
        );

        // Error shell box: the same box rows on the error background, over the
        // kept end of the failure output — collapsed and expanded.
        let args = serde_json::json!({ "command": "false" });
        let output = numbered_output(12, "err");
        let component = tool_component("bash", args.clone(), false);
        fail(&component, &output, None);
        assert_reference_bytes(
            "tool-bash-error-collapsed",
            &component,
            width,
            &reference_tool_box_rows_with_bg(
                ThemeColor::ToolErrorBg,
                &args,
                &output,
                None,
                false,
                width,
            ),
        );
        let component = tool_component("bash", args.clone(), true);
        fail(&component, &output, None);
        assert_reference_bytes(
            "tool-bash-error-expanded",
            &component,
            width,
            &reference_tool_box_rows_with_bg(
                ThemeColor::ToolErrorBg,
                &args,
                &output,
                None,
                true,
                width,
            ),
        );

        // Expanded `!` block with hidden lines: the `to collapse` hint after
        // the whole output, not the collapsed `N more lines` one.
        let args = serde_json::json!({ "command": "seq 1 25", "userBash": true });
        let output = numbered_output(25, "jb");
        let component = user_bash_component(args.clone(), Some(&output), true);
        assert_reference_bytes(
            "user-bash-expanded-collapse-hint",
            &component,
            width,
            &reference_user_bash_rows(&args, &output, false, true, width),
        );

        // Collapsed `!` block over ANSI-styled output longer than the preview:
        // the migrated branch wraps the styled lines ONCE and slices the tail,
        // while the reference re-wraps through `truncate_to_visual_lines`; a
        // keep-start revert, or a slice taken off a differently-wrapped pass,
        // makes the two disagree.
        let args = serde_json::json!({ "command": "printf 🙂🙂", "userBash": true });
        let output = ansi_numbered_output(25);
        let component = user_bash_component(args.clone(), Some(&output), false);
        assert_reference_bytes(
            "user-bash-ansi-keep-end",
            &component,
            width,
            &reference_user_bash_rows(&args, &output, false, false, width),
        );

        // Legacy edit replay (settled, no `details`): the args-rebuilt
        // intra-line diff inside the box, no result section.
        let args = legacy_edit_args();
        let component = tool_component("edit", args.clone(), false);
        settle(&component, "applied 1 edit", None);
        assert_reference_bytes(
            "edit-legacy-settled",
            &component,
            width,
            &reference_edit_legacy_rows(&args, width),
        );

        // Edit error result: the failure text below the box, prefixed and
        // padded at `width` (not painted).
        let args = edit_args();
        let preview_error = ToolPreview {
            args_key: edit_preview_input(&args).expect("input").args_key,
            diff: None,
            first_changed_line: None,
            error: Some("oldText not found".to_string()),
        };
        let component = tool_component("edit", args.clone(), false);
        component.set_meta(
            Some(Arc::new(serde_json::json!({}))),
            None,
            Some(preview_error.clone()),
        );
        fail(&component, "write failed: permission denied", None);
        let mut expected = reference_edit_rows(&args, Some(&preview_error), None, false, width);
        expected.push(String::new());
        for visual in wrap_text_with_ansi(
            &with_theme(|theme| theme.fg(ThemeColor::Error, "write failed: permission denied")),
            width.saturating_sub(2).max(1),
        ) {
            expected.push(reference_pad_prefixed_row(" ", &visual, width));
        }
        assert_reference_bytes("edit-error-result", &component, width, &expected);
    }
}

/// The assistant component's segments — including the cache-hit bytes
/// `segment_lines_into` copies into the canvas — must equal a fresh public
/// [`Markdown`] construction built with the pre-migration
/// `render_segment_lines` arguments. A failure means the segment cache or the
/// pooled copy no longer emits what a cold render would.
#[test]
fn assistant_segment_cache_matches_fresh_public_markdown() {
    set_theme("dark");
    let message = assistant_text_thinking_message();
    for hide_thinking in [false, true] {
        let component = AssistantMessageComponent::new_from(Arc::clone(&message), hide_thinking, 1);
        for width in [1usize, 20, 80] {
            let expected = reference_assistant_rows(&message, 1, hide_thinking, width);
            // First render: segment-cache miss (renders through the public
            // markdown pipeline).
            assert_eq!(
                component.render(width),
                expected,
                "assistant cache-miss render (hide_thinking {hide_thinking}) != public markdown \
                 construction @ width {width}"
            );
            // Second render: the cache HIT the migration rewrote.
            assert_eq!(
                component.render(width),
                expected,
                "assistant cache-hit render (hide_thinking {hide_thinking}) != public markdown \
                 construction @ width {width}"
            );
        }
    }

    // The error tail: `out.push_fmt(format_args!("{pad}{}", fg(Error, …)))` —
    // the tail row is deliberately NOT padded to the width.
    let message = Arc::new(AssistantMessage {
        content: vec![text_block("body")],
        stop_reason: StopReason::Error,
        error_message: Some("boom".to_string()),
        ..AssistantMessage::default()
    });
    let component = AssistantMessageComponent::new_from(Arc::clone(&message), false, 1);
    for width in [1usize, 20] {
        let mut expected = reference_assistant_rows(&message, 1, false, width);
        expected.push(String::new());
        expected.push(format!(
            " {}",
            with_theme(|theme| theme.fg(ThemeColor::Error, "Error: boom"))
        ));
        assert_eq!(
            component.render(width),
            expected,
            "assistant error tail != the unpadded `{{pad}}{{fg(Error)}}` row @ width {width}"
        );
    }
}

/// The `UserMessageComponent`'s migrated `render_into` (the background box
/// written straight into the canvas) must stay byte-identical to the box
/// `new` has always built — rebuilt here from the public `BoxContainer` +
/// `Markdown` pieces with the user-message markdown options — at every width,
/// on a fresh pool and on a poisoned one.
///
/// Kills: a box painter switched to another background color, `paddingX`
/// swapped with `paddingY`, the markdown options (ordered-list markers,
/// backslash escapes) dropped, and a `render_into` that delegates to something
/// other than the box.
#[test]
fn user_message_component_matches_pre_migration_box() {
    set_theme("dark");

    let cases: [(&str, &str, usize); 5] = [
        ("user-message-plain", "hello user message", 0),
        ("user-message-padded", "hello user message", 2),
        (
            "user-message-cjk-emoji-ansi",
            "\x1b[31mred\x1b[0m 日本語テキスト 🙂\n\nsecond 日本語 line",
            1,
        ),
        (
            "user-message-ordered-list-and-escape",
            "1) first item\n2) second \\* item",
            1,
        ),
        ("user-message-empty", "", 1),
    ];

    for (label, text, output_pad) in cases {
        let component = UserMessageComponent::new(text, output_pad);
        assert_render_parity(&component, label);
        for width in WIDTHS {
            assert_reference_bytes(
                label,
                &component,
                width,
                &reference_user_message_rows(text, output_pad, width),
            );
        }
    }
}

/// The hidden-thinking label rides the segment cache key: a relabel through the
/// app's shared slot (`with_hidden_label` ← `ctx.ui.setHiddenThinkingLabel`)
/// must MISS the warm cache and repaint with the new label, and restoring the
/// default must miss back to `Thinking...`. With thinking visible the label is
/// keyed but never painted, so the same mutation must leave the bytes alone.
///
/// Kills: a `SegmentKey` that drops the label (the warm `Thinking...` line
/// would be served for the relabelled component), a hit path that copies the
/// wrong slot, and an `invalidate` that no longer clears the cache.
#[test]
fn assistant_hidden_label_relabel_misses_the_segment_cache() {
    set_theme("dark");
    let message = Arc::new(AssistantMessage {
        content: vec![thinking_block("thinking body 日本語 🙂")],
        ..AssistantMessage::default()
    });
    let label_row = |text: &str| {
        format!(
            " {}",
            with_theme(|theme| theme.italic(&theme.fg(ThemeColor::ThinkingText, text)))
        )
    };
    let slot: Arc<std::sync::Mutex<Option<String>>> = Arc::new(std::sync::Mutex::new(None));

    let hidden = AssistantMessageComponent::new_from(Arc::clone(&message), true, 1)
        .with_hidden_label(Arc::clone(&slot));
    for width in [1usize, 20, 80] {
        assert_eq!(
            hidden.render(width),
            vec![String::new(), label_row("Thinking...")],
            "default hidden label @ width {width}"
        );
    }
    *slot.lock().unwrap() = Some("Ruminating...".to_string());
    for width in [1usize, 20, 80] {
        assert_eq!(
            hidden.render(width),
            vec![String::new(), label_row("Ruminating...")],
            "relabelled hidden thinking must not serve the cached default @ width {width}"
        );
    }
    *slot.lock().unwrap() = None;
    for width in [1usize, 20, 80] {
        assert_eq!(
            hidden.render(width),
            vec![String::new(), label_row("Thinking...")],
            "restored default label @ width {width}"
        );
    }

    // Visible thinking: the label is part of the key but not of the bytes.
    *slot.lock().unwrap() = Some("Ruminating...".to_string());
    let visible = AssistantMessageComponent::new_from(Arc::clone(&message), false, 1)
        .with_hidden_label(Arc::clone(&slot));
    for width in [1usize, 20, 80] {
        assert_eq!(
            visible.render(width),
            reference_assistant_rows(&message, 1, false, width),
            "visible thinking must ignore the hidden label @ width {width}"
        );
    }
    visible.invalidate();
    for width in [1usize, 20, 80] {
        assert_eq!(
            visible.render(width),
            reference_assistant_rows(&message, 1, false, width),
            "visible thinking after invalidate must re-render identically @ width {width}"
        );
    }
}

// --- 4.3 bridge + pool stability -------------------------------------------

/// Message components for a whole frame, rebuilt per use (like
/// [`frame_fixture`]): one instance of each render branch the migration
/// rewrote, so every component claims its rows from the SAME recycled pool.
fn message_frame_fixture() -> Vec<Box<dyn Component>> {
    vec![
        Box::new(UserMessageComponent::new(
            "user asks 日本語テキスト 🙂\n\nsecond line",
            1,
        )),
        Box::new(assistant_text_thinking_block(1, false)),
        Box::new(bash_collapsed_with_hidden_lines()),
        Box::new(bash_error()),
        Box::new(user_bash_collapsed_with_tail()),
        Box::new(edit_with_preview_and_result()),
        Box::new(edit_legacy_settled()),
        Box::new(generic_tool_fallback()),
    ]
}

fn message_frame_render(width: usize) -> Vec<String> {
    message_frame_fixture()
        .iter()
        .flat_map(|component| component.render(width))
        .collect()
}

/// The frame renderer's canvas path for the message components: a `TuiCore`
/// frame holding them must equal the concatenation of their public `render`
/// (the `render_into` bridge), including when the shared `frame_scratch` pool
/// is recycled from a wider/taller frame.
///
/// A failure means a component wrote into the wrong pooled row — appending to
/// the previous component's row instead of claiming a fresh one — or left a
/// long row's tail in the frame.
#[test]
fn core_frame_of_message_components_matches_their_render() {
    set_theme("dark");
    let terminal = Terminal::new();
    terminal.set_size(80, 24);
    let mut core = TuiCore::new(terminal);
    for component in message_frame_fixture() {
        core.push_content(component);
    }

    // Wide first, so `frame_scratch` holds rows longer than every frame below.
    core.terminal().set_size(200, 60);
    core.render();
    assert_eq!(
        core.last_frame_lines(),
        message_frame_render(200).as_slice(),
        "wide core frame != concatenated message component render"
    );

    for width in [1usize, 10, 80, 200] {
        core.terminal().set_size(width, 60);
        core.render();
        assert_eq!(core.last_render_width(), width);
        assert_eq!(
            core.last_frame_lines(),
            message_frame_render(width).as_slice(),
            "core frame != concatenated message component render @ width {width}"
        );
    }
}

/// Bridge + pool stability for the migrated components: two consecutive
/// `render(width)` calls must be byte-identical (cache stability — for the
/// assistant the second is the segment-cache hit), and a `render_into` into a
/// pool carrying a LONGER previous frame must produce exactly the shorter
/// frame.
///
/// A failure means either a render cache that serves different bytes on the
/// hit path, or a recycled pool row that was appended to / not cleared
/// (`Rows::claim`) or a pool tail that was not dropped (`Rows::trim`).
#[test]
fn message_component_render_is_stable_and_pool_reuse_leaves_no_residue() {
    set_theme("dark");

    let fixtures: Vec<(&str, Box<dyn Component>)> = vec![
        (
            "user-message-padded",
            Box::new(UserMessageComponent::new(
                "user asks 日本語テキスト 🙂\n\nsecond line",
                1,
            )),
        ),
        (
            "assistant-text-thinking",
            Box::new(assistant_text_thinking_block(1, false)),
        ),
        (
            "tool-bash-collapsed-hidden",
            Box::new(bash_collapsed_with_hidden_lines()),
        ),
        ("tool-bash-error", Box::new(bash_error())),
        ("tool-fallback-collapsed", Box::new(generic_tool_fallback())),
        (
            "user-bash-collapsed-tail",
            Box::new(user_bash_collapsed_with_tail()),
        ),
        (
            "user-bash-expanded-collapse-hint",
            Box::new(user_bash_expanded_with_hidden()),
        ),
        (
            "edit-preview-and-result",
            Box::new(edit_with_preview_and_result()),
        ),
        ("edit-legacy-settled", Box::new(edit_legacy_settled())),
        ("edit-error-result", Box::new(edit_error_result())),
    ];

    for (label, component) in &fixtures {
        for width in WIDTHS {
            let first = component.render(width);
            assert_eq!(
                component.render(width),
                first,
                "{label}: two consecutive renders differ @ width {width}"
            );

            // The realistic recycle: the pool holds this component's own
            // 500-column frame (longer rows than any shorter frame emits).
            let prefilled = render_into_lines(component.as_ref(), 500);
            let recycled =
                render_into_lines_with_pool(component.as_ref(), width, prefilled.clone());
            assert_eq!(
                recycled, first,
                "{label}: pool residue from the 500-column frame @ width {width}"
            );
            assert_eq!(
                recycled.len(),
                first.len(),
                "{label}: pool tail not trimmed to the frame @ width {width}"
            );
            if width < 500 && !first.is_empty() {
                let prefilled_max = prefilled.iter().map(String::len).max().unwrap_or(0);
                let short_max = first.iter().map(String::len).max().unwrap_or(0);
                assert!(
                    prefilled_max > short_max,
                    "{label}: the 500-column frame must leave longer rows than the \
                     {width}-column frame"
                );
            }
        }

        // A cache keyed on something other than the width (or a pooled row
        // reused across widths) would serve the wide frame's rows here.
        let narrow = component.render(10);
        let wide = component.render(80);
        assert_eq!(
            component.render(10),
            narrow,
            "{label}: width-10 rows changed after an 80-wide render"
        );
        assert_eq!(
            component.render(80),
            wide,
            "{label}: width-80 rows changed after a re-render"
        );
    }
}
