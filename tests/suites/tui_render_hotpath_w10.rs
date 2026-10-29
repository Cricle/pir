//! W10-C render-hotpath regressions (F4 timer quantization + F3 markdown
//! incremental tail / highlight memo).
//!
//! F4: `ResultRowsCache` used to key on the live `Elapsed` text at
//! millisecond resolution, so every 80 ms spinner tick re-styled and
//! re-wrapped the whole pending shell/codemode result body. The timer key is
//! now quantized to the displayed 0.1 s tick and the timer row is painted
//! outside the cached section: a steady 0.1 s window rebuilds nothing, a
//! boundary crossing rebuilds only the tail text, and output changes still
//! rebuild the body.
//!
//! F3: an open fenced code block used to be one indivisible mutable tail, so
//! every frame re-parsed, re-highlighted and re-wrapped the whole block. The
//! incremental renderer now cuts at the block's last (mutable) line and keeps
//! the full code text in a memo so the stateful highlighter resumes at the
//! exact state a from-scratch render would have. The highlight memo itself is
//! keyed by `(hash of code, language, theme generation)` and extends appended
//! bodies in O(appended).
//!
//! Pinned here:
//! 1. timer: 0 tail/result rebuilds inside one 0.1 s window, exactly 1 tail
//!    rebuild (and 0 result rebuilds) on a boundary crossing, and a body
//!    rebuild when the output changes;
//! 2. open fence: 100 growing frames re-parse O(appended) bytes in total, not
//!    O(n²), while every frame stays byte-identical to a cold render;
//! 3. highlight memo: hit/miss counters for an exact body, an append
//!    extension (only appended bytes are re-highlighted) and a theme swap;
//! 4. byte-identical incremental vs from-scratch rendering for a corpus
//!    (open/closed fences with multi-line scanner state, paragraphs, lists,
//!    tables, CRLF, tabs, a custom highlight hook).
//!
//! Every test takes the process-global theme lock first (HANDOFF §5).

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use pir::tui::core::Component;
use pir::tui::markdown::{set_global_hyperlinks, Markdown, MarkdownOptions, MarkdownTheme};
use pir::tui::messages::{ToolExecutionComponent, ToolMemoWork};
use pir::tui::syntax::{
    highlight_code, highlight_memo_work, reset_highlight_memo_work, HighlightMemoWork,
};
use pir::tui::theme::set_theme;

/// The `g_tui`-binary-wide theme lock: renders read the live palette and a
/// sibling suite may swap it concurrently.
fn theme_lock() -> std::sync::MutexGuard<'static, ()> {
    super::tui_system_theme::lock()
}

fn pin_theme() -> std::sync::MutexGuard<'static, ()> {
    let guard = theme_lock();
    set_theme("dark");
    set_global_hyperlinks(Some(false));
    guard
}

// ---------------------------------------------------------------------------
// F4 — timer quantization
// ---------------------------------------------------------------------------

fn pending_bash_block(started_at: Instant) -> ToolExecutionComponent {
    let block = ToolExecutionComponent::new(
        "bash",
        Arc::new(serde_json::json!({ "command": "sleep 10" })),
        Arc::new(AtomicBool::new(false)),
    );
    block.set_meta(None, Some(started_at), None);
    block
}

#[test]
fn a_clock_tick_repaints_only_the_timer_row() {
    let _guard = pin_theme();
    // Anchor at 5.01 s: the 0.1 s bucket is 50 and the next boundary is 90 ms
    // away, so the two immediate frames below cannot cross it.
    let block = pending_bash_block(Instant::now() - Duration::from_millis(5_010));
    let warm = block.render(80);
    assert!(
        warm.iter().any(|row| row.contains("Elapsed 5.")),
        "the pending shell row must show its live timer: {warm:?}"
    );

    ToolMemoWork::reset();
    let _ = block.render(80);
    assert_eq!(
        ToolMemoWork::snapshot(),
        ToolMemoWork {
            frames: 1,
            ..ToolMemoWork::default()
        },
        "a steady frame inside one 0.1 s window must rebuild no memo \
         (only the frame counter moves)"
    );

    // Cross one display tick by re-anchoring the start clock. The quantized
    // elapsed key moves 5.0 s -> 5.1 s: exactly one tail rebuild, and the
    // cached result section must stay untouched.
    block.set_meta(
        None,
        Some(Instant::now() - Duration::from_millis(5_150)),
        None,
    );
    let crossed = block.render(80);
    let work = ToolMemoWork::snapshot();
    assert_eq!(
        work.result_rebuilds, 0,
        "a clock tick must not re-render the cached result body: {work:?}"
    );
    assert_eq!(
        work.tail_rebuilds, 1,
        "crossing one 0.1 s boundary rebuilds the timer text exactly once: {work:?}"
    );
    assert!(
        crossed.iter().any(|row| row.contains("Elapsed 5.")),
        "the crossed tick must render the new display value: {crossed:?}"
    );

    // A second frame at the same tick is a hit again.
    ToolMemoWork::reset();
    let _ = block.render(80);
    let steady = ToolMemoWork::snapshot();
    assert_eq!(steady.tail_rebuilds, 0, "{steady:?}");
    assert_eq!(steady.result_rebuilds, 0, "{steady:?}");
}

#[test]
fn a_changed_result_body_still_rebuilds_the_cached_section() {
    let _guard = pin_theme();
    let block = ToolExecutionComponent::new(
        "bash",
        Arc::new(serde_json::json!({ "command": "run" })),
        Arc::new(AtomicBool::new(false)),
    );
    block.update_result(
        Arc::from("first output line\nsecond output line"),
        false,
        Some(1_200),
    );
    let _ = block.render(80);

    ToolMemoWork::reset();
    block.update_result(
        Arc::from("changed output line\nsecond output line"),
        false,
        Some(1_200),
    );
    let rows = block.render(80);
    let work = ToolMemoWork::snapshot();
    assert_eq!(
        work.result_rebuilds, 1,
        "an output change must rebuild the result section: {work:?}"
    );
    assert!(
        rows.iter().any(|row| row.contains("changed output line")),
        "the rebuilt section must show the new output: {rows:?}"
    );

    // Expansion and width are inputs too.
    ToolMemoWork::reset();
    let _ = block.render(60);
    let work = ToolMemoWork::snapshot();
    assert_eq!(
        work.result_rebuilds, 1,
        "a width change must rebuild the result section: {work:?}"
    );
}

// ---------------------------------------------------------------------------
// F3 — open-fence incremental rendering
// ---------------------------------------------------------------------------

#[test]
fn one_hundred_open_fence_frames_parse_only_the_appended_bytes() {
    let _guard = pin_theme();
    let width = 80;
    let streamed = Markdown::new("", 0, 0);
    let mut doc = String::from("```rust\n");
    streamed.set_text_growing(&doc);
    let _ = streamed.render(width);

    let mut previous_prefix = 0usize;
    let mut total_reparsed = 0usize;
    for index in 0..100 {
        let line =
            format!("    let value_{index} = compute({index}); // a long enough line to wrap\n");
        doc.push_str(&line);
        streamed.set_text_growing(&doc);
        let got = streamed.render(width);
        let cold = Markdown::new(&doc, 0, 0).render(width);
        assert_eq!(got, cold, "frame {index} diverged from the cold render");

        let (prefix, reparsed) = streamed.debug_incremental_metrics();
        assert!(
            prefix >= previous_prefix,
            "the final prefix must never shrink (frame {index})"
        );
        assert!(
            prefix > 0,
            "the open fence must expose a final prefix at frame {index}"
        );
        previous_prefix = prefix;
        // Each frame re-parses the previous mutable line plus the new line.
        assert!(
            reparsed <= 2 * line.len() + 48,
            "frame {index} re-parsed {reparsed} bytes (line {})",
            line.len()
        );
        total_reparsed += reparsed;
    }
    assert!(
        total_reparsed < doc.len() * 3,
        "100 growing frames re-parsed {total_reparsed} bytes for a {} byte body \
         (must scale with appended bytes, not O(n²))",
        doc.len()
    );

    // When the fence closes, the closing border is folded exactly once and
    // the body settles into the cached prefix.
    doc.push_str("```\n");
    streamed.set_text_growing(&doc);
    let got = streamed.render(width);
    let cold = Markdown::new(&doc, 0, 0).render(width);
    assert_eq!(got, cold, "the closing frame must equal the cold render");
    let (_, reparsed) = streamed.debug_incremental_metrics();
    assert!(
        reparsed <= 16,
        "the closing frame must only parse the closer (got {reparsed})"
    );
}

#[test]
fn a_long_open_paragraph_parses_only_its_last_line() {
    let _guard = pin_theme();
    let width = 40;
    let streamed = Markdown::new("", 0, 0);
    let mut doc = String::new();
    streamed.set_text_growing(&doc);
    let _ = streamed.render(width);

    let mut total_reparsed = 0usize;
    for index in 0..100 {
        let line = format!(
            "This is line {index} of a long plain paragraph with enough words to wrap around.
"
        );
        doc.push_str(&line);
        streamed.set_text_growing(&doc);
        let got = streamed.render(width);
        let cold = Markdown::new(&doc, 0, 0).render(width);
        assert_eq!(
            got, cold,
            "paragraph frame {index} diverged from the cold render"
        );
        let (prefix, reparsed) = streamed.debug_incremental_metrics();
        if index > 0 {
            assert!(
                prefix > 0,
                "a plain open paragraph must expose a final prefix at frame {index}"
            );
        }
        // The paragraph fold (`paragraph_candidate` in
        // `src/tui/markdown.rs`) cuts at the open paragraph's last line, so a
        // frame re-parses the previous mutable line plus the appended one to
        // find the new cut (the discovery pass), then the appended line again
        // to render the tail: three physical lines, i.e. O(appended bytes)
        // whatever the paragraph's total length. Frame 1 of this fixture is
        // exactly 3 x 75 = 225 bytes; pre-fold the tail was the WHOLE
        // paragraph, so this number grew with the paragraph instead.
        assert!(
            reparsed <= 3 * line.len() + 48,
            "paragraph frame {index} re-parsed {reparsed} bytes (line {})",
            line.len()
        );
        total_reparsed += reparsed;
    }
    assert!(
        // Each frame's cost is its discovery pass over the previous mutable
        // line plus the appended one, then the render pass over the appended
        // line — three physical lines — so 100 frames stay under 3x the body
        // (frame 0 is cheaper still). Quadratic re-parsing (the pre-fold
        // whole-paragraph tail) would be ~50x here and blows this bound.
        total_reparsed < doc.len() * 3,
        "100 paragraph frames re-parsed {total_reparsed} bytes for a {} byte body",
        doc.len()
    );
}

// ---------------------------------------------------------------------------
// F3 — highlight memo
// ---------------------------------------------------------------------------

static UNIQUE: AtomicU64 = AtomicU64::new(0);

fn unique_rust_body() -> String {
    let id = UNIQUE.fetch_add(1, Ordering::Relaxed);
    format!("fn unique_{id}() {{\n    let value = {id};\n    println!(\"{{}}\", value);\n}}\n")
}

#[test]
fn highlight_memo_hits_misses_and_appends() {
    let _guard = pin_theme();
    reset_highlight_memo_work();
    let code = unique_rust_body();

    let first = highlight_code(&code, Some("rust"));
    let after_first = highlight_memo_work();
    assert_eq!(
        after_first,
        HighlightMemoWork {
            hits: 0,
            misses: 1,
            lines_highlighted: code.split('\n').count() as u64,
            bytes_highlighted: code.len() as u64,
        },
        "the first body must be highlighted from scratch"
    );

    let second = highlight_code(&code, Some("rust"));
    assert_eq!(first, second, "an exact hit must return identical lines");
    let after_second = highlight_memo_work();
    assert_eq!(
        after_second,
        HighlightMemoWork {
            hits: 1,
            ..after_first
        },
        "an unchanged body must be a memo hit with zero highlight work"
    );

    // Append one line: only that line (the old last line is complete, so just
    // the appended bytes) is re-highlighted.
    let mut grown = code.clone();
    grown.push_str("    let extra = 40 + 2;\n");
    let appended = "    let extra = 40 + 2;\n";
    let before_append = highlight_memo_work();
    let _ = highlight_code(&grown, Some("rust"));
    let after_append = highlight_memo_work();
    assert_eq!(
        after_append.hits,
        before_append.hits + 1,
        "an append must extend a stored body: {after_append:?}"
    );
    assert_eq!(
        after_append.misses, before_append.misses,
        "an append must not rebuild from scratch: {after_append:?}"
    );
    assert_eq!(
        after_append.bytes_highlighted - before_append.bytes_highlighted,
        appended.len() as u64,
        "an append must re-highlight only the appended bytes"
    );

    // A theme generation change drops every stored body.
    set_theme("light");
    let before_swap = highlight_memo_work();
    let light = highlight_code(&code, Some("rust"));
    let after_swap = highlight_memo_work();
    assert_eq!(
        after_swap.misses,
        before_swap.misses + 1,
        "a theme swap must rebuild: {after_swap:?}"
    );
    assert_ne!(
        first, light,
        "the same code under two themes must produce different ANSI"
    );
    set_theme("dark");
}

// ---------------------------------------------------------------------------
// F3 — byte-identical incremental vs from-scratch rendering
// ---------------------------------------------------------------------------

const CORPUS: &[&str] = &[
    // Open fence with scanner state spanning lines (block comment closes on a
    // later line), plus an inline string.
    "```rust\nlet a = 1;\n/* open block comment\nstill comment\n*/ let b = 2;\nlet s = \"x\";\n",
    // Tilde fence with a Python triple-quoted string, still open at the end.
    "~~~python\nx = '''triple\nquoted\nstill quoted'''\nprint(x)\n",
    // Plain open fence with blank lines and wide graphemes.
    "```\nplain code 日本語 🙂\n\nwith a blank line\n",
    // Closed fence followed by prose.
    "```js\nconst x = 1;\n```\n\nafter the fence\n",
    // Inline spans across soft newlines (must not regress).
    "a paragraph\nwith **bold\nacross lines** and `code\nspan` and ~~gone~~\n",
    // Lists, a table, and a quote.
    "- list one\n- list two\n  continuation\n\n| a | b |\n|---|---|\n| 1 | 2 |\n\n> quoted **bold\n> line**\n",
    // CRLF and tabs inside an open fence.
    "```rust\r\nlet x = 1;\r\n\tlet y = 2;\r\n/* comment\r\nstill */\r\n",
    // CRLF prose.
    "line one\r\nline two\r\n\r\npara two\r\n",
];

/// Advance one character (never splitting a UTF-8 sequence).
fn advance_character(text: &str, from: usize) -> usize {
    let mut end = (from + 1).min(text.len());
    while end < text.len() && !text.is_char_boundary(end) {
        end += 1;
    }
    end
}

/// Advance to the end of the current line (one delta per line).
fn advance_line(text: &str, from: usize) -> usize {
    match text[from..].find('\n') {
        Some(index) => from + index + 1,
        None => text.len(),
    }
}

fn assert_every_prefix_matches(
    text: &str,
    width: usize,
    padding_x: usize,
    padding_y: usize,
    advance: fn(&str, usize) -> usize,
) {
    let streamed = Markdown::new("", padding_x, padding_y);
    let mut end = 0usize;
    while end < text.len() {
        end = advance(text, end);
        let prefix = &text[..end];
        streamed.set_text_growing(prefix);
        let got = streamed.render(width);
        let want = Markdown::new(prefix, padding_x, padding_y).render(width);
        assert_eq!(
            got,
            want,
            "streamed prefix {end} of {:?} @ width {width} pad ({padding_x},{padding_y}) \
             differs from a from-scratch render",
            text.get(..text.len().min(48)).unwrap_or(text),
        );
    }
}

#[test]
fn incremental_rendering_is_byte_identical_to_the_classic_path() {
    let _guard = pin_theme();
    for doc in CORPUS {
        for (width, padding_x, padding_y) in [(80usize, 0usize, 0usize), (24, 1, 0)] {
            assert_every_prefix_matches(doc, width, padding_x, padding_y, advance_character);
            assert_every_prefix_matches(doc, width, padding_x, padding_y, advance_line);
        }
    }
}

fn custom_highlight(code: &str, _language: Option<&str>) -> Vec<String> {
    code.split('\n')
        .map(|line| format!("CUSTOM:{line}"))
        .collect()
}

#[test]
fn a_custom_highlight_hook_stays_on_the_classic_path() {
    let _guard = pin_theme();
    let theme = MarkdownTheme {
        highlight_code: custom_highlight,
        ..MarkdownTheme::default()
    };
    let doc = "```rust\nlet a = 1;\nlet b = 2;\nstill open\n";
    let streamed = Markdown::with_options("", 0, 0, theme.clone(), MarkdownOptions::default());
    let mut end = 0usize;
    while end < doc.len() {
        end = advance_character(doc, end);
        let prefix = &doc[..end];
        streamed.set_text_growing(prefix);
        let got = streamed.render(60);
        let want = Markdown::with_options(prefix, 0, 0, theme.clone(), MarkdownOptions::default())
            .render(60);
        assert_eq!(
            got, want,
            "a custom hook must render the same bytes incrementally and cold"
        );
    }
    assert!(
        streamed
            .render(60)
            .iter()
            .any(|row| row.contains("CUSTOM:")),
        "the custom hook must be honored"
    );
}
