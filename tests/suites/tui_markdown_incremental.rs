//! Incremental live-stream markdown render: the final-prefix cut rule in
//! `src/tui/markdown/blocks.rs` (`ParsedBlocks::final_boundary`) plus the
//! render state in `src/tui/markdown.rs` (`StreamingRender`).
//!
//! The live entry hands the accumulated assistant text to one `Markdown`
//! component per delta. Before the incremental renderer every frame copied the
//! whole body and re-parsed, re-styled, re-wrapped and re-padded it (O(body)
//! per frame, O(body²) over one reply). The renderer now caches the lines of
//! the text's FINAL PREFIX — the largest blank-line cut that text appended
//! later can never change — and re-renders only the mutable tail.
//!
//! Pinned here:
//! 1. parity: at EVERY intermediate streamed state (character by character and
//!    line by line) the incremental lines are byte-identical to a fresh
//!    component's render of the same text, across widths, paddings, theme
//!    swaps, width changes, replacements and invalidations;
//! 2. cost: the final prefix grows monotonically and the bytes re-parsed per
//!    frame stay bounded by the open tail, not by the whole body;
//! 3. the documented residuals: a list at the very end of the text (or any
//!    still-open block whose lines are not provably append-stable) has no
//!    final prefix and re-parses in full per frame. Plain paragraphs and
//!    unterminated fences DO fold now: `paragraph_candidate` /
//!    `fence_candidate` keep only the open block's last line mutable;
//! 4. the live thinking block renders the settled thinking segment byte for
//!    byte (it drives the same incremental renderer).
//!
//! Every test takes the process-global theme lock first (HANDOFF §5): renders
//! read the live palette and sibling suites swap it.

use std::sync::{Arc, Mutex};

use pir::ai::types::{AssistantMessage, ContentBlock, TextContent, ThinkingContent};
use pir::config::settings::MermaidRenderingMode;
use pir::tui::core::Component;
use pir::tui::markdown::{Markdown, MarkdownOptions, MarkdownTheme};
use pir::tui::messages::{AssistantMessageComponent, StreamingThinking};
use pir::tui::theme::set_theme;

/// Take the theme lock and pin the palette; the guard must outlive the test.
/// The OSC 8 hyperlink gate is pinned too: the documents below carry links,
/// and the parity assertions must not depend on the host terminal's detected
/// capability (the enabled path is covered by the OSC 8 section of
/// `tui_markdown_parity`, which takes this same lock around every pin).
fn pin_theme() -> std::sync::MutexGuard<'static, ()> {
    let guard = super::tui_system_theme::lock();
    set_theme("dark");
    pir::tui::markdown::set_global_hyperlinks(Some(false));
    guard
}

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

/// Documents chosen to hit every cross-block lookahead the cut rule must
/// respect: closed / unterminated / partially closing fences, tables (boxed
/// and too-narrow), block `$$` math (closed and pending), inline math and
/// backslashes, tight and loose lists, task items, blank-joined items,
/// indented markers and indented continuation lines, tabs, autolinks,
/// blockquotes, headings, rules, blank-line runs, trailing whitespace and
/// CRLF, plus wide graphemes.
const DOCS: &[&str] = &[
    "# Head\n\nA paragraph with **bold**, _em_, `code`, ~~gone~~, a [link](https://x.io \"T\") and an autolink <https://example.com>.\n\nSecond paragraph,\nwrapped by a soft newline.\n",
    "```rust\nfn main() {\n    println!(\"hi\");\n}\n```\n\nafter the fence\n",
    "```\nunterminated fence\nthe rest is code\nwith a ~tilde~ line\n",
    "~~~python\nprint('x')\n~~~\n",
    "- a\n- b\n\n- c\n\n1. one\n2. two\n",
    "- [x] done\n- [ ] todo\n- plain\n",
    "- outer\n    - inner one\n    - inner two\n\n- second item\n\n",
    "1) paren\n2) markers\n\n3. dotted\n",
    "| col | other |\n| --- | --- |\n| alpha beta | x |\n| gamma | y |\n",
    "| a | b |\n|---|---|\n| 1 | 2 |\n",
    "$$\nx = \\frac{1}{2}\n$$\n\nInline $a+b$ math and \\[c\\] here.\n\n$$\n\\sum_{i=0}^n i\n$$\n",
    "$$\n\\frac{a}{b\n",
    "    indented continuation\n\ttab indented line\n\ntext\tafter tab\n",
    "> quoted **bold**\n> second line\n\n---\n\n### h3 `code` tail\n",
    "a\n\n\nb\n \n\nc\n",
    "see https://example.com/a?b=1&c=2 and www.x.com and user@example.com\n",
    "日本語のテキスト with 🙂🙂 emoji and **太字**\n\nnext block\n",
    "line one\r\nline two\r\n\r\npara two\r\n",
    "trailing spaces   \nand  \nafter\n",
];

/// A reply-shaped body: blank-line separated paragraphs (with an occasional
/// fenced code block), so the mutable tail is bounded by the biggest block.
/// Returns the document and that block's byte size.
fn reply_document() -> (String, usize) {
    let mut doc = String::new();
    let mut max_block = 0usize;
    for index in 0..24 {
        let block =
            format!("Paragraph {index} with some words to wrap around a narrow viewport width.\n");
        max_block = max_block.max(block.len());
        doc.push_str(&block);
        doc.push('\n');
        if index % 6 == 5 {
            let code = "```\ncode line one\ncode line two\n```\n";
            max_block = max_block.max(code.len());
            doc.push_str(code);
            doc.push('\n');
        }
    }
    (doc, max_block)
}

// ---------------------------------------------------------------------------
// Streaming helpers
// ---------------------------------------------------------------------------

/// Advance one character (never splitting a UTF-8 sequence, like the
/// provider's deltas).
fn advance_character(text: &str, from: usize) -> usize {
    let mut end = (from + 1).min(text.len());
    while end < text.len() && !text.is_char_boundary(end) {
        end += 1;
    }
    end
}

/// Advance to the end of the current line (one line per delta).
fn advance_line(text: &str, from: usize) -> usize {
    match text[from..].find('\n') {
        Some(index) => from + index + 1,
        None => text.len(),
    }
}

/// Stream `text` into one component with `advance`-sized deltas and require
/// every intermediate frame to equal a fresh component's render of the same
/// prefix (full rows, padding included — no trimming).
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
        assert_eq!(
            streamed.get_text(),
            prefix,
            "the growing body must hold exactly the streamed prefix"
        );
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

// ---------------------------------------------------------------------------
// 1. Parity: every streamed state equals the from-scratch render
// ---------------------------------------------------------------------------

#[test]
fn streamed_prefixes_match_fresh_renders_character_by_character() {
    let _guard = pin_theme();
    for doc in DOCS {
        for (width, padding_x, padding_y) in [(400usize, 0usize, 0usize), (24, 1, 0)] {
            assert_every_prefix_matches(doc, width, padding_x, padding_y, advance_character);
        }
    }
}

#[test]
fn streamed_prefixes_match_fresh_renders_line_by_line() {
    let _guard = pin_theme();
    for doc in DOCS {
        for (width, padding_x, padding_y) in [
            (120usize, 1usize, 0usize),
            (30, 1, 1),
            (4000, 0, 0),
            (8, 0, 2),
        ] {
            assert_every_prefix_matches(doc, width, padding_x, padding_y, advance_line);
        }
    }
}

/// The live entry's rows must survive the settle swap: the incremental live
/// component renders the same rows the settled `AssistantMessageComponent`
/// produces for its text segment (the leading spacer row is the only
/// difference). Documents here are settle-clean — no leading blank line and no
/// trailing blank line — because the settled path trims its body while the
/// live path renders the raw accumulator (a pre-existing upstream shape, not a
/// property of the incremental renderer).
const SETTLE_CLEAN: &[&str] = &[
    "# Head\n\nA paragraph with **bold** and `code`.\n\n- one\n- two\n",
    "```rust\nfn main() {}\n```\n\ntail paragraph\n",
    "| a | b |\n|---|---|\n| 1 | 2 |\n\n$$\nx = 1\n$$\n",
    "> quoted\n\n---\n\nlast line",
    "wide 🙂 graphemes 日本語 and a https://example.com/a?b=1 link\n",
];

#[test]
fn a_streamed_entry_settles_byte_identically() {
    let _guard = pin_theme();
    for doc in SETTLE_CLEAN {
        for width in [40usize, 100] {
            let live = Markdown::new("", 1, 0);
            for line_end in SplitLines::new(doc) {
                live.set_text_growing(&doc[..line_end]);
            }
            let live_rows = live.render(width);

            let message = AssistantMessage {
                content: vec![ContentBlock::Text(TextContent {
                    text: doc.to_string(),
                    text_signature: None,
                })],
                ..AssistantMessage::default()
            };
            let settled = AssistantMessageComponent::new_from(Arc::new(message), false, 1);
            let settled_rows = settled.render(width);
            assert_eq!(
                settled_rows.first().map(String::as_str),
                Some(""),
                "the settled component leads with the spacer row"
            );
            assert_eq!(
                live_rows,
                settled_rows[1..].to_vec(),
                "the live body of {:?} @ width {width} must equal the settled segment",
                doc.get(..doc.len().min(48)).unwrap_or(doc),
            );
        }
    }
}

struct SplitLines<'a> {
    text: &'a str,
    end: usize,
}

impl<'a> SplitLines<'a> {
    fn new(text: &'a str) -> Self {
        Self { text, end: 0 }
    }
}

impl<'a> Iterator for SplitLines<'a> {
    type Item = usize;

    fn next(&mut self) -> Option<usize> {
        if self.end >= self.text.len() {
            return None;
        }
        self.end = advance_line(self.text, self.end);
        Some(self.end)
    }
}

// ---------------------------------------------------------------------------
// 2. Cost: the final prefix grows, the re-parse stays bounded
// ---------------------------------------------------------------------------

#[test]
fn the_final_prefix_grows_monotonically_and_the_reparse_stays_bounded() {
    let _guard = pin_theme();
    let (doc, max_block) = reply_document();
    let streamed = Markdown::new("", 0, 0);
    let mut previous_prefix = 0usize;
    let mut max_reparsed = 0usize;
    let mut frames = 0usize;
    for end in SplitLines::new(&doc) {
        frames += 1;
        streamed.set_text_growing(&doc[..end]);
        let _ = streamed.render(48);
        let (prefix, reparsed) = streamed.debug_incremental_metrics();
        assert!(
            prefix >= previous_prefix,
            "the final prefix must never shrink (frame {frames}: {previous_prefix} -> {prefix})"
        );
        // A cut is no longer required to sit on a blank line. The paragraph
        // fold (`paragraph_candidate`) cuts at the open paragraph's last line,
        // the fence fold (`fence_candidate`) cuts at the open fence's last
        // mutable line, and a frame whose newest line is a bare fence opener
        // folds every earlier block plus the opener and leaves only synthetic
        // rows mutable — its cut is the body end (frame 13 of this fixture is
        // exactly that shape). What the old `prefix < end` protected survives
        // as "a cut is a line start or the body end, never mid-line and never
        // past the streamed bytes"; the re-parse bound below is what keeps the
        // work per frame small.
        assert!(
            prefix <= end,
            "the final prefix must never run past the streamed body \
             (frame {frames}: {prefix} > {end})"
        );
        assert!(
            prefix == 0 || prefix == end || doc.as_bytes().get(prefix - 1) == Some(&b'\n'),
            "a cut must be a line start or the body end \
             (frame {frames}: prefix {prefix}, end {end})"
        );
        previous_prefix = prefix;
        max_reparsed = max_reparsed.max(reparsed);
        // Once the first blocks have completed, a frame re-parses the open
        // block (plus its separating blank line) — never the whole body.
        if end > 2 * max_block {
            assert!(
                reparsed <= max_block + 4,
                "frame {frames} re-parsed {reparsed} bytes, above the {max_block}-byte block bound"
            );
        }
    }
    assert!(frames > 30, "the fixture must stream many frames");
    assert!(
        max_reparsed <= max_block + 4,
        "the worst frame re-parsed {max_reparsed} bytes (block bound {max_block})"
    );
    assert!(
        max_reparsed < doc.len() / 4,
        "the worst frame re-parsed {max_reparsed} of {} bytes",
        doc.len()
    );
    assert!(
        previous_prefix + max_block >= doc.len(),
        "the settled prefix {previous_prefix} must cover all but the last block of {} bytes",
        doc.len()
    );
}

/// The open-block fold: a body that is one still-open PLAIN block now folds
/// every completed line into the final prefix (`paragraph_candidate` and
/// `fence_candidate` in `src/tui/markdown.rs`); only the open block's last
/// line — the only line an append can still rewrite — stays mutable.
///
/// Pre-fold this test asserted `prefix == 0` and `reparsed == text.len()` for
/// the paragraph and fence shapes (the whole open block was one tail). The
/// fold flipped those numbers. The list shapes keep the old expectation: a
/// blank line plus a marker still continues (and loosens) the list, so the
/// blank is not locked. A future block parser that can lock a list blank, or
/// prove an inline span closed mid-stream, flips the remaining assertions
/// deliberately.
#[test]
fn an_open_block_folds_all_but_its_last_line() {
    let _guard = pin_theme();

    // A plain paragraph: every complete line but the last folds. The open
    // paragraph's last line is the only line a later append can rewrite, so
    // the cut lands on its start; nothing before it re-parses again.
    let paragraph =
        "one very long paragraph without any blank line\nand a second line\nand a third\n";
    let last_line_start = paragraph
        .trim_end_matches('\n')
        .rfind('\n')
        .map_or(0, |index| index + 1);
    let streamed = Markdown::new("", 0, 0);
    streamed.set_text_growing(paragraph);
    let _ = streamed.render(40);
    let (prefix, reparsed) = streamed.debug_incremental_metrics();
    assert_eq!(
        prefix, last_line_start,
        "the paragraph fold must cut at the open paragraph's last line"
    );
    assert_eq!(
        reparsed,
        paragraph.len() + (paragraph.len() - last_line_start),
        "the first frame parses the body once (nothing is cached yet), then \
         re-parses only the fold's mutable last line"
    );

    // An open fence: every newline-terminated code line is append-stable, so
    // the opener and all code lines fold and NO streamed byte stays mutable;
    // the only mutable row is the synthetic bottom border the classic renderer
    // draws for an unclosed fence. The cut is therefore the body end.
    let fence = "```\ncode line one\ncode line two\n";
    let streamed = Markdown::new("", 0, 0);
    streamed.set_text_growing(fence);
    let _ = streamed.render(40);
    let (prefix, reparsed) = streamed.debug_incremental_metrics();
    assert_eq!(
        prefix,
        fence.len(),
        "an open fence folds every complete code line"
    );
    assert_eq!(
        reparsed,
        fence.len(),
        "the first frame parses the whole body once"
    );

    // The residual: a list at the very end keeps the whole-block tail.
    for text in ["- first\n- second\n\n", "- first\n- second\n"] {
        let streamed = Markdown::new("", 0, 0);
        streamed.set_text_growing(text);
        let _ = streamed.render(40);
        let (prefix, reparsed) = streamed.debug_incremental_metrics();
        assert_eq!(prefix, 0, "no final prefix expected for the list {text:?}");
        assert_eq!(
            reparsed,
            text.len(),
            "the list residual re-parses the whole body for {text:?}"
        );
    }
}

/// The list residual ends the moment its blank line is locked (a non-marker
/// line follows it, or it is followed by another blank): the prefix then
/// carries the list body.
#[test]
fn a_closed_list_becomes_final() {
    let _guard = pin_theme();
    for text in ["- first\n- second\n\ntext\n", "- first\n- second\n\n\nt"] {
        let streamed = Markdown::new("", 0, 0);
        for end in SplitLines::new(text) {
            streamed.set_text_growing(&text[..end]);
            let _ = streamed.render(40);
        }
        let (prefix, _) = streamed.debug_incremental_metrics();
        assert!(
            prefix >= "- first\n- second\n".len(),
            "a locked list blank must be a final cut for {text:?} (prefix {prefix})"
        );
    }
}

// ---------------------------------------------------------------------------
// 3. Invalidation: theme, width, replacement, invalidate
// ---------------------------------------------------------------------------

/// An idle frame — the same body re-fed, e.g. the working-spinner tick — is a
/// pure cache hit: nothing is re-parsed, and the rows are the same bytes.
#[test]
fn an_unchanged_body_is_a_cache_hit() {
    let _guard = pin_theme();
    let (doc, _) = reply_document();
    let streamed = Markdown::new("", 0, 0);
    streamed.set_text_growing(&doc);
    let first = streamed.render(48);
    let (prefix, reparsed) = streamed.debug_incremental_metrics();
    assert!(
        prefix > 0 && reparsed > 0,
        "the first frame parses the body"
    );
    // Re-feed the same accumulated body (what the live path does every frame).
    streamed.set_text_growing(&doc);
    let second = streamed.render(48);
    assert_eq!(first, second, "an idle frame renders the same rows");
    assert_eq!(
        streamed.debug_incremental_metrics(),
        (prefix, 0),
        "an unchanged body must not re-parse anything"
    );
}

#[test]
fn a_theme_swap_drops_the_streamed_prefix() {
    let _guard = pin_theme();
    let streamed = Markdown::new("", 1, 0);
    streamed.set_text_growing("first paragraph\n\nsecond paragraph\n\n");
    let _ = streamed.render(40);
    // The prefix lines carry theme colors, so a swap must re-render them.
    set_theme("light");
    streamed.set_text_growing("first paragraph\n\nsecond paragraph\n\nthird line\n");
    let after = streamed.render(40);
    let want =
        Markdown::new("first paragraph\n\nsecond paragraph\n\nthird line\n", 1, 0).render(40);
    assert_eq!(
        after, want,
        "a theme swap must re-render the whole body under the new palette"
    );
    set_theme("dark");
}

#[test]
fn a_width_change_rebuilds_from_scratch() {
    let _guard = pin_theme();
    let streamed = Markdown::new("", 1, 0);
    let (doc, _) = reply_document();
    streamed.set_text_growing(&doc);
    let _ = streamed.render(60);
    streamed.set_text_growing(&format!("{doc}appended paragraph\n\n"));
    let narrow = streamed.render(24);
    assert_eq!(
        narrow,
        Markdown::new(format!("{doc}appended paragraph\n\n"), 1, 0).render(24),
        "a width change must re-render the prefix at the new width"
    );
}

#[test]
fn a_replaced_body_and_invalidate_drop_the_prefix() {
    let _guard = pin_theme();
    let streamed = Markdown::new("alpha\n\nbeta\n\n", 1, 0);
    let _ = streamed.render(40);
    // `set_text`: a wholesale replacement, not an append.
    streamed.set_text("gamma\n\ndelta\n\n");
    assert_eq!(
        streamed.render(40),
        Markdown::new("gamma\n\ndelta\n\n", 1, 0).render(40)
    );
    // A `set_text_growing` that does not extend the body is a replacement.
    streamed.set_text_growing("omega\n\nsigma\n\n");
    assert_eq!(
        streamed.get_text(),
        "omega\n\nsigma\n\n",
        "a non-extension replaces the body"
    );
    assert_eq!(
        streamed.render(40),
        Markdown::new("omega\n\nsigma\n\n", 1, 0).render(40)
    );
    // `invalidate` forces a from-scratch render of the same body.
    streamed.invalidate();
    assert_eq!(
        streamed.render(40),
        Markdown::new("omega\n\nsigma\n\n", 1, 0).render(40)
    );
    // A shorter snapshot also replaces (never reuses a longer prefix).
    streamed.set_text_growing("omega\n\n");
    assert_eq!(
        streamed.render(40),
        Markdown::new("omega\n\n", 1, 0).render(40)
    );
}

// ---------------------------------------------------------------------------
// 4. The live thinking block (same incremental renderer)
// ---------------------------------------------------------------------------

const THINKING_DOCS: &[&str] = &[
    "first thought\n\nsecond thought with **emphasis**\n\n- a\n- b\n",
    "```\nthinking in a fence\n```\n\nafter\n",
    "日本語の思考 🙂 and a table\n\n| a | b |\n|---|---|\n| 1 | 2 |\n",
];

/// The streamed live thinking rows must equal the settled thinking segment's
/// rows (the component this child is replaced by at `message_done`), across
/// the visible and hidden postures and a live label relabel.
/// Regression: the per-line painter must run exactly once per final row. The
/// live thinking component paints its padded rows (`theme.fg` prepends its
/// escape to the whole row, exactly like the settled arm's map), so applying
/// it to a row that already sits in the cached final prefix would stack a
/// second escape on it (`\x1b[38;5;109m\x1b[38;5;109m…`).
#[test]
fn a_reused_prefix_row_is_painted_exactly_once() {
    let _guard = pin_theme();
    let streamed = StreamingThinking::new("", 1, false, Arc::new(Mutex::new(None)));

    streamed.set_text_growing("first thought\n");
    let first = streamed.render(80);
    assert_eq!(first.len(), 1, "{first:?}");

    // Frame 2 locks the blank line after the paragraph, so the row moves into
    // the cached final prefix — it must be painted where it is produced.
    streamed.set_text_growing("first thought\n\nsecond\n");
    let second = streamed.render(80);
    assert_eq!(second.len(), 3, "{second:?}");
    assert_eq!(second[0], first[0], "first paint: {second:?}");

    // Frame 3 grows the open tail again: the prefix row is *reused*, and a
    // painter that runs over the cached prefix would stack a second escape.
    streamed.set_text_growing("first thought\n\nsecond line\n");
    let third = streamed.render(80);
    assert_eq!(third.len(), 3, "{third:?}");
    assert_eq!(
        third[0], first[0],
        "the reused prefix row must keep its single paint: {third:?}"
    );
}

#[test]
fn live_thinking_rows_match_the_settled_thinking_segment() {
    let _guard = pin_theme();
    let label = Arc::new(Mutex::new(None));
    for doc in THINKING_DOCS {
        for width in [80usize, 24, 400] {
            let streamed = StreamingThinking::new("", 1, false, Arc::clone(&label));
            for end in SplitLines::new(doc) {
                streamed.set_text_growing(&doc[..end]);
                let rows = streamed.render(width);
                let message = AssistantMessage {
                    content: vec![ContentBlock::Thinking(ThinkingContent {
                        thinking: doc[..end].to_string(),
                        thinking_signature: None,
                        redacted: false,
                    })],
                    ..AssistantMessage::default()
                };
                let settled = AssistantMessageComponent::new_from(Arc::new(message), false, 1)
                    .with_hidden_label(Arc::clone(&label));
                let settled_rows = settled.render(width);
                assert_eq!(
                    rows,
                    settled_rows[1..].to_vec(),
                    "live thinking @ {end} bytes, width {width} != settled segment"
                );
            }
        }
    }

    // Hidden posture: one `{pad}{label}` row, live-relabeled through the
    // shared slot — the settled hidden arm renders the same row.
    for label_text in ["Thinking...", "Reasoning..."] {
        *label
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = if label_text == "Thinking..." {
            None
        } else {
            Some(label_text.to_string())
        };
        let streamed = StreamingThinking::new("hidden body", 1, true, Arc::clone(&label));
        let rows = streamed.render(40);
        let message = AssistantMessage {
            content: vec![ContentBlock::Thinking(ThinkingContent {
                thinking: "hidden body".to_string(),
                thinking_signature: None,
                redacted: false,
            })],
            ..AssistantMessage::default()
        };
        let settled = AssistantMessageComponent::new_from(Arc::new(message), true, 1)
            .with_hidden_label(Arc::clone(&label));
        let settled_rows = settled.render(40);
        assert_eq!(rows, settled_rows[1..].to_vec());
        assert_eq!(rows.len(), 1, "the hidden posture is one row: {rows:?}");
        assert!(
            pir::tui::ansi::strip_ansi(&rows[0]).contains(label_text),
            "{rows:?}"
        );
    }
}

// ---------------------------------------------------------------------------
// 5. Mermaid on the live stream (upstream components/mermaid.ts:60-75)
// ---------------------------------------------------------------------------

fn streaming_options(mode: MermaidRenderingMode) -> MarkdownOptions {
    MarkdownOptions {
        mermaid: mode,
        is_streaming: true,
        ..MarkdownOptions::default()
    }
}

/// `markdown.mermaid = streaming` transforms an open/closed mermaid fence
/// while the body is still streaming; `final` keeps the raw fence until the
/// settled entry replaces the live component (upstream's
/// `context.isStreaming && mode !== "streaming"` guard).
#[test]
fn live_stream_mermaid_renders_only_in_streaming_mode() {
    let _guard = pin_theme();
    let complete = "Before\n\n```mermaid\nflowchart LR\n  A[Start] --> B[Done]\n```\nAfter";

    let streaming = Markdown::with_options(
        complete,
        0,
        0,
        MarkdownTheme::default(),
        streaming_options(MermaidRenderingMode::Streaming),
    )
    .render(120);
    assert!(
        streaming.iter().any(|row| row.contains("Start")),
        "streaming mode renders the diagram while streaming: {streaming:?}"
    );
    assert!(
        !streaming.iter().any(|row| row.contains("```mermaid")),
        "the fence is replaced: {streaming:?}"
    );

    let final_mode = Markdown::with_options(
        complete,
        0,
        0,
        MarkdownTheme::default(),
        streaming_options(MermaidRenderingMode::Final),
    )
    .render(120);
    assert!(
        final_mode.iter().any(|row| row.contains("```mermaid")),
        "final mode keeps the raw fence on a live body: {final_mode:?}"
    );
    assert!(
        !final_mode.iter().any(|row| row.contains('─')),
        "no diagram art before the settled render: {final_mode:?}"
    );
}

/// Incremental redraw: the SAME live component shows the raw fence while the
/// diagram source is incomplete and the parsed art once the closing bytes
/// arrive (the open mermaid fence is never folded into the final prefix, so
/// every delta re-evaluates the whole diagram).
#[test]
fn live_stream_mermaid_redraws_when_the_diagram_completes() {
    let _guard = pin_theme();
    let partial = "Before\n\n```mermaid\nflowchart LR\n  A[Start] --> B[";
    let complete = "Before\n\n```mermaid\nflowchart LR\n  A[Start] --> B[Done]\n```";
    let live = Markdown::with_options(
        partial,
        0,
        0,
        MarkdownTheme::default(),
        streaming_options(MermaidRenderingMode::Streaming),
    );
    let before = live.render(120);
    // A partial diagram either keeps the raw fence (the engine cannot parse
    // it yet) or renders a placeholder diagram from the incomplete source;
    // both are legitimate streaming states (upstream re-runs the transformer
    // per delta too).
    let placeholder = before.iter().any(|row| row.contains("```mermaid"))
        || before.iter().any(|row| row.contains("B["));
    assert!(
        placeholder,
        "an incomplete diagram must show a placeholder state: {before:?}"
    );

    live.set_text_growing(complete);
    let after = live.render(120);
    assert!(
        after.iter().any(|row| row.contains("Done")),
        "the completed diagram replaces the placeholder in the live block: {after:?}"
    );
    assert!(
        !after.iter().any(|row| row.contains("```mermaid")),
        "the fence is gone once the diagram renders: {after:?}"
    );
    assert_ne!(
        before, after,
        "the live block re-renders when the diagram source completes"
    );
}
