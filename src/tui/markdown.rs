//! Markdown component — port of `pi-tui`'s `components/markdown.ts`
//! rendering rules (the marked-based token pipeline reduced to a
//! self-contained block/inline parser that produces the same styled lines
//! for the constructs pi renders: headings, paragraphs, code fences,
//! lists, blockquotes, tables, rules, links, emphasis, inline code).
//!
//! Styling delegates to the theme (`mdHeading`, `mdCode`, `mdQuote`, …);
//! wrapping uses the shared ANSI-aware wrapper; tables render the exact
//! upstream box-drawing shape (`┌─┬─┐ / ├─┼─┤ / └─┴─┘` with bold header).

// Pre-existing baseline lints (tui is frozen for the parity batch).
#![allow(clippy::enum_variant_names)]
#![allow(clippy::needless_range_loop)]
#![allow(clippy::manual_checked_ops)]
#![allow(clippy::unused_enumerate_index)]

use super::ansi::{pad_to_width, visible_width, wrap_text_with_ansi};
use super::core::{Component, Rows};
use super::mermaid;
use super::theme::{with_theme, Theme, ThemeColor};
use crate::config::settings::MermaidRenderingMode;
use std::borrow::Cow;

/// Markdown render options mirroring the upstream `MarkdownOptions` subset
/// the chat surface uses.
#[derive(Debug, Clone)]
pub struct MarkdownOptions {
    /// Preserve `1)`-style ordered markers verbatim (upstream
    /// `preserveOrderedListMarkers`, used by the user-message component).
    pub preserve_ordered_list_markers: bool,
    /// Keep backslash escapes visible (user messages).
    pub preserve_backslash_escapes: bool,
    /// Trim a streamed partial closing fence so code blocks don't shrink
    /// mid-stream. Upstream trims unconditionally: `trimPartialClosingFences`
    /// runs on every parse (markdown.ts:309).
    pub trim_partial_closing_fences: bool,
    /// Render supported LaTeX math as Unicode text (upstream `renderLatex`:
    /// on unless explicitly `false`, markdown.ts:518 and markdown.ts:658).
    pub render_latex: bool,
    /// Mermaid code-block rendering (upstream `markdown.mermaid` via
    /// `createMermaidMarkdownTransformer`): `Off` keeps the raw fence,
    /// `Streaming`/`Final` replace supported diagrams with Unicode box
    /// drawings. `Final` only applies to settled bodies; the live streaming
    /// body sets [`MarkdownOptions::is_streaming`], which keeps the raw fence
    /// unless the mode is `Streaming` (upstream
    /// `components/mermaid.ts:60-75`).
    pub mermaid: MermaidRenderingMode,
    /// The body is still streaming (upstream `MarkdownTransformContext.
    /// isStreaming`): with `mermaid: Final` a mermaid fence stays raw, with
    /// `mermaid: Streaming` it is re-rendered per delta (the partial source
    /// falls back to the raw fence until the engine can parse it).
    pub is_streaming: bool,
}

impl Default for MarkdownOptions {
    fn default() -> Self {
        Self {
            preserve_ordered_list_markers: false,
            preserve_backslash_escapes: false,
            // Defaults reproduce upstream for every caller: partial closing
            // fences are always trimmed while streaming (markdown.ts:146-169)
            // and LaTeX math renders as Unicode (markdown.ts:518, :658).
            trim_partial_closing_fences: true,
            render_latex: true,
            // Off keeps every existing caller byte-identical; the chat host
            // opts in from `markdown.mermaid` via
            // [`set_global_mermaid_mode`] + [`global_mermaid_mode`].
            mermaid: MermaidRenderingMode::Off,
            is_streaming: false,
        }
    }
}

/// Process-wide `markdown.mermaid` snapshot for components built without a
/// settings handle (the assistant message path). Set once at TUI startup
/// from the merged settings; `Off` until then.
static MERMAID_MODE: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(0);

/// Install the effective `markdown.mermaid` mode for the process (upstream
/// reads it live through the settings manager; pir snapshots it at startup,
/// matching the once-set theme semantics).
pub fn set_global_mermaid_mode(mode: MermaidRenderingMode) {
    let encoded = match mode {
        MermaidRenderingMode::Off => 0,
        MermaidRenderingMode::Final => 1,
        MermaidRenderingMode::Streaming => 2,
    };
    MERMAID_MODE.store(encoded, std::sync::atomic::Ordering::SeqCst);
}

/// The installed [`MermaidRenderingMode`] (upstream
/// `settingsManager.getMermaidRenderingMode()`; default `streaming` when the
/// setting is unset or unrecognized).
pub fn global_mermaid_mode() -> MermaidRenderingMode {
    match MERMAID_MODE.load(std::sync::atomic::Ordering::SeqCst) {
        1 => MermaidRenderingMode::Final,
        2 => MermaidRenderingMode::Streaming,
        _ => MermaidRenderingMode::Off,
    }
}

/// Process-wide OSC 8 hyperlink capability — upstream
/// `getCapabilities().hyperlinks`, the flag `components/markdown.ts:702`
/// gates the link case on. `HYPERLINK_AUTO` leaves the answer to the
/// environment detection ([`super::terminal_image::detect_capabilities`],
/// upstream `detectCapabilities`); [`set_global_hyperlinks`] pins it the way
/// upstream's capability overrides do (`setCapabilityOverrides`,
/// `terminal-image.ts:187-197`).
///
/// Upstream reads the cached capability at render time and keeps it out of the
/// component's own render cache (`Markdown.cachedLines` is keyed on
/// text+width only), so an override is expected to land before the first
/// render.
static HYPERLINK_CAPABILITY: std::sync::atomic::AtomicU8 =
    std::sync::atomic::AtomicU8::new(HYPERLINK_AUTO);
/// [`HYPERLINK_CAPABILITY`] states.
const HYPERLINK_AUTO: u8 = 0;
const HYPERLINK_ENABLED: u8 = 1;
const HYPERLINK_DISABLED: u8 = 2;
/// The environment detection, cached exactly once like upstream's
/// `cachedCapabilities` (`terminal-image.ts:35,167-176`).
static DETECTED_HYPERLINKS: std::sync::OnceLock<bool> = std::sync::OnceLock::new();

/// Pin the OSC 8 hyperlink gate (upstream `setCapabilityOverrides({
/// hyperlinks })`): `Some(true)`/`Some(false)` force the flag, `None` clears
/// the override and returns to the detected environment default. The detected
/// answer itself stays cached after its first read — clearing the override
/// does not re-probe, matching upstream's cache (only
/// `resetCapabilitiesCache` re-probes, and the environment does not change
/// mid-process).
pub fn set_global_hyperlinks(enabled: Option<bool>) {
    HYPERLINK_CAPABILITY.store(
        match enabled {
            Some(true) => HYPERLINK_ENABLED,
            Some(false) => HYPERLINK_DISABLED,
            None => HYPERLINK_AUTO,
        },
        std::sync::atomic::Ordering::SeqCst,
    );
}

/// Whether the renderer emits OSC 8 hyperlink runs: the pinned override when
/// one is installed, else the detected terminal capability (upstream
/// `getCapabilities().hyperlinks`, read at render time by `markdown.ts:702`).
pub fn global_hyperlinks() -> bool {
    match HYPERLINK_CAPABILITY.load(std::sync::atomic::Ordering::SeqCst) {
        HYPERLINK_ENABLED => true,
        HYPERLINK_DISABLED => false,
        _ => *DETECTED_HYPERLINKS
            .get_or_init(|| super::terminal_image::detect_capabilities().hyperlinks),
    }
}

/// The mermaid code-block decision of the markdown pipeline (upstream
/// `createMermaidMarkdownTransformer`, mermaid.ts:60-88).
enum MermaidDecision {
    /// Replace the fence with the diagram lines (themed styling is a
    /// documented deviation: pir renders the plain rows).
    Diagram(Vec<String>),
    /// Keep the raw fence; `Some(text)` appends one advisory line after it
    /// (upstream attaches the first dropped statement, summarizing the rest).
    RawWithWarning(Option<String>),
    /// Keep the raw fence untouched.
    Raw,
}

fn mermaid_block(
    lang: &str,
    text: &str,
    width: usize,
    options: &MarkdownOptions,
) -> MermaidDecision {
    if options.mermaid == MermaidRenderingMode::Off {
        return MermaidDecision::Raw;
    }
    // Upstream `createMermaidMarkdownTransformer` (mermaid.ts:60-75): a
    // streaming body is transformed only in `streaming` mode; `final` mode
    // waits for the settled message (the transformer sees `isStreaming`).
    if options.is_streaming && options.mermaid != MermaidRenderingMode::Streaming {
        return MermaidDecision::Raw;
    }
    let is_mermaid = lang
        .split_whitespace()
        .next()
        .is_some_and(|first| first.eq_ignore_ascii_case("mermaid"));
    if !is_mermaid {
        return MermaidDecision::Raw;
    }
    let art = match mermaid::render(text) {
        mermaid::MermaidOutcome::Diagram(art) => art,
        mermaid::MermaidOutcome::NotRendered(advisory) => {
            // No art to show: the raw block wins. A settled render with an
            // engine diagnostic reports it under the fence (mermaid.ts:77-82);
            // streaming (and silent unsupported/blank input) keeps the fence
            // without an advisory line.
            return match (advisory, options.mermaid) {
                (Some(message), MermaidRenderingMode::Final) => MermaidDecision::RawWithWarning(
                    Some(format!("Mermaid diagram not rendered: {message}")),
                ),
                _ => MermaidDecision::Raw,
            };
        }
    };
    if art.width > width {
        return MermaidDecision::Raw;
    }
    MermaidDecision::Diagram(art.plain)
}

/// Theme hooks (upstream `MarkdownTheme`) with pir defaults wired to the
/// global theme.
#[derive(Clone)]
pub struct MarkdownTheme {
    pub heading: fn(&str) -> String,
    pub link: fn(&str) -> String,
    pub link_url: fn(&str) -> String,
    pub code: fn(&str) -> String,
    pub code_block: fn(&str) -> String,
    pub code_block_border: fn(&str) -> String,
    pub quote: fn(&str) -> String,
    pub quote_border: fn(&str) -> String,
    pub hr: fn(&str) -> String,
    pub list_bullet: fn(&str) -> String,
    pub bold: fn(&str) -> String,
    pub italic: fn(&str) -> String,
    pub underline: fn(&str) -> String,
    pub strikethrough: fn(&str) -> String,
    /// `highlightCode` (upstream `MarkdownTheme.highlightCode`, markdown.ts:
    /// 533-543): the styled lines of a fenced code block. `None` (fence with
    /// no info string) and unknown languages keep the `mdCodeBlock` color.
    pub highlight_code: fn(&str, Option<&str>) -> Vec<String>,
}

impl Default for MarkdownTheme {
    fn default() -> Self {
        Self {
            heading: |t| with_theme(|th| th.fg(ThemeColor::MdHeading, t)),
            link: |t| with_theme(|th| th.fg(ThemeColor::MdLink, t)),
            link_url: |t| with_theme(|th| th.fg(ThemeColor::MdLinkUrl, t)),
            code: |t| with_theme(|th| th.fg(ThemeColor::MdCode, t)),
            code_block: |t| with_theme(|th| th.fg(ThemeColor::MdCodeBlock, t)),
            code_block_border: |t| with_theme(|th| th.fg(ThemeColor::MdCodeBlockBorder, t)),
            quote: |t| with_theme(|th| th.fg(ThemeColor::MdQuote, t)),
            quote_border: |t| with_theme(|th| th.fg(ThemeColor::MdQuoteBorder, t)),
            hr: |t| with_theme(|th| th.fg(ThemeColor::MdHr, t)),
            list_bullet: |t| with_theme(|th| th.fg(ThemeColor::MdListBullet, t)),
            bold: |t| with_theme(|th| th.bold(t)),
            italic: |t| with_theme(|th| th.italic(t)),
            underline: |t| with_theme(|th| th.underline(t)),
            strikethrough: |t| with_theme(|th| th.strikethrough(t)),
            highlight_code: super::syntax::highlight_code,
        }
    }
}

/// The Markdown component (upstream `Markdown`): cached render to styled
/// ANSI lines with padding.
///
/// The body and its render state live behind one lock (`StreamingRender`):
/// the live-stream path hands the accumulated text over per delta and the
/// render reuses the lines of the text's FINAL PREFIX (the incremental
/// renderer's cut rule) instead of re-parsing, re-styling, re-wrapping and
/// re-padding the whole body every frame. A body that only grew therefore
/// costs O(mutable tail) per frame instead of O(body) — O(block) summed over a
/// reply instead of O(body) per frame.
pub struct Markdown {
    body: std::sync::Mutex<StreamingRender>,
    padding_x: usize,
    padding_y: usize,
    theme: MarkdownTheme,
    options: MarkdownOptions,
}

impl Markdown {
    pub fn new(text: impl Into<String>, padding_x: usize, padding_y: usize) -> Self {
        Self {
            body: std::sync::Mutex::new(StreamingRender::new(text.into())),
            padding_x,
            padding_y,
            theme: MarkdownTheme::default(),
            options: MarkdownOptions::default(),
        }
    }

    /// Build from an already-shared buffer: the body is copied once into the
    /// component's own growing buffer, which the live path then extends in
    /// place through [`Markdown::set_text_growing`].
    pub fn new_shared(text: std::sync::Arc<str>, padding_x: usize, padding_y: usize) -> Self {
        Self::new(text.to_string(), padding_x, padding_y)
    }

    pub fn with_options(
        text: impl Into<String>,
        padding_x: usize,
        padding_y: usize,
        theme: MarkdownTheme,
        options: MarkdownOptions,
    ) -> Self {
        Self {
            body: std::sync::Mutex::new(StreamingRender::new(text.into())),
            padding_x,
            padding_y,
            theme,
            options,
        }
    }

    /// Replace the text wholesale; the cached final prefix is dropped.
    pub fn set_text(&self, text: impl Into<String>) {
        self.body
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .replace(text.into());
    }

    /// Replace the text with an already-shared buffer (the settled path's
    /// one-shot commit): the bytes are copied once into the growing buffer.
    pub fn set_text_shared(&self, text: std::sync::Arc<str>) {
        self.set_text(text.to_string());
    }

    /// Live-stream update: `text` is the new accumulated body. When it extends
    /// the current body — the append-only contract of the live entry — only
    /// the appended bytes are copied, and the next render re-parses just the
    /// mutable tail. Any other text replaces the body (and drops the prefix).
    pub fn set_text_growing(&self, text: &str) {
        self.body
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .grow(text);
    }

    /// Live-stream fast path: append only the bytes the accumulator gained.
    /// `ChatContainer` derives `delta` from the entry's allocation tag, so the
    /// markdown body is never compared against the full snapshot. An empty
    /// delta is a no-op (an idle frame keeps the cached lines).
    #[doc(hidden)]
    pub fn set_text_growing_delta(&self, delta: &str) {
        self.body
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .grow_delta(delta);
    }

    pub fn get_text(&self) -> String {
        self.body
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .text()
            .to_string()
    }

    /// `(final-prefix bytes, bytes re-parsed by the last render)` — the
    /// incremental-render proof hook (`tests/suites/tui_markdown_incremental.rs`).
    #[doc(hidden)]
    pub fn debug_incremental_metrics(&self) -> (usize, usize) {
        self.body
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .debug_metrics()
    }
}

impl Component for Markdown {
    /// The trait's owned-line form: the same rows, built through
    /// [`Component::render_into`] in a fresh pool (the [`StreamingRender::lines`]
    /// bridge). The frame renderer drives the canvas path; this form keeps
    /// `render` correct for callers that ask for lines (tests, alternate
    /// drivers).
    fn render(&self, width: usize) -> Vec<String> {
        // One lock covers the body and the lines it renders into: a frame
        // either answers from the state or extends it, and the two can never
        // disagree (the component is only touched by the TUI thread).
        self.body
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .lines(
                width,
                self.padding_x,
                self.padding_y,
                &self.theme,
                &self.options,
                None,
            )
    }

    /// Direct-write path: the body's cached final lines (the final prefix and
    /// the mutable tail) are pushed into the frame's recycled row buffers
    /// instead of being cloned into a per-frame line `Vec`, so a body that did
    /// not change costs zero row allocations per frame. Row for row
    /// byte-identical to [`Component::render`] (both funnel through
    /// [`StreamingRender::lines_into`]).
    fn render_into(&self, width: usize, out: &mut Rows<'_>) {
        self.body
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .lines_into(
                width,
                self.padding_x,
                self.padding_y,
                &self.theme,
                &self.options,
                None,
                out,
            )
    }

    fn invalidate(&self) {
        self.body
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .invalidate();
    }
}

/// One body's rendered lines, keyed by every input they were rendered under.
struct RenderedLines {
    /// Body generation answered — an exact hit.
    generation: u64,
    /// Replacement epoch: the final prefix may only be reused while the body
    /// was extended, never replaced.
    replace_epoch: u64,
    width: usize,
    padding_x: usize,
    padding_y: usize,
    theme_generation: u64,
    mermaid: MermaidRenderingMode,
    /// Streaming posture the lines were rendered under (the mermaid
    /// transformer's `isStreaming` decision).
    is_streaming: bool,
    painted: bool,
    /// Raw byte length of the final prefix (`body[..final_len]`); the block
    /// parse of those bytes is final (appends can never change it).
    final_len: usize,
    /// Final, fully padded lines of the prefix.
    prefix_lines: Vec<String>,
    /// Final, fully padded lines of the mutable tail.
    tail_lines: Vec<String>,
    /// The open fenced code block straddling `final_len`: its opener context
    /// and full code text, so the next render parses only the bytes from the
    /// mutable line on instead of the whole block (see [`FenceTail`]).
    fence_tail: Option<FenceTail>,
}

/// Incremental tail context of a still-open fenced code block.
///
/// An open fence used to be one indivisible mutable block: every frame
/// re-parsed its whole body, re-highlighted every line (the stateful scanner
/// runs over the whole text) and re-wrapped/re-padded every row. The block's
/// *complete lines* are append-stable, though: a later line can never change
/// an earlier code line's highlighting (the scanner state only flows
/// forward), so `final_len` is moved to the start of the block's last line and
/// the rows before it are folded into `prefix_lines`. `code_text` keeps the
/// normalized code text through that last line so the next frame can resume
/// the stateful highlighter exactly where the from-scratch render would.
struct FenceTail {
    /// Fence info string (`Block::Code.lang`).
    lang: String,
    /// Original fence run, so a `~~~` closer is recognized as a closer.
    fence_char: char,
    fence_len: usize,
    /// Whether the real opener line carried no leading whitespace. The
    /// parser's partial-closing-fence trim is gated on an unindented opener
    /// (`blocks.rs`'s `trimPartialClosingFences` branch), so the synthetic
    /// opener must reproduce that shape for the resume parse to trim exactly
    /// when a from-scratch parse would.
    opener_unindented: bool,
    /// Normalized code text of every parsed code line (joined by `\n`).
    code_text: String,
    /// Number of code lines `code_text` holds (an empty code block is zero;
    /// the renderer still paints one empty row for it).
    code_lines: usize,
    /// Content lines already folded into `prefix_lines` — the code-line index
    /// the mutable tail starts at.
    stable_lines: usize,
}

impl RenderedLines {
    #[allow(clippy::too_many_arguments)]
    fn answers(
        &self,
        generation: u64,
        width: usize,
        padding_x: usize,
        padding_y: usize,
        theme_generation: u64,
        mermaid: MermaidRenderingMode,
        is_streaming: bool,
        painted: bool,
    ) -> bool {
        self.generation == generation
            && self.width == width
            && self.padding_x == padding_x
            && self.padding_y == padding_y
            && self.theme_generation == theme_generation
            && self.mermaid == mermaid
            && self.is_streaming == is_streaming
            && self.painted == painted
    }

    /// The frame's rows into the canvas: padding, final prefix, mutable tail,
    /// padding — the exact order the pre-migration `assemble` built its `Vec`
    /// in. The cached prefix and tail rows are COPIED into the pooled row
    /// buffers (the cache keeps its own copy for the next frame, and a cached
    /// row must never be moved out of it) and the padding rows are written in
    /// place, so a frame whose body did not change allocates no row buffer.
    fn assemble_into(&self, width: usize, padding_y: usize, out: &mut Rows<'_>) {
        push_padding_rows(out, width, padding_y);
        for line in &self.prefix_lines {
            out.push(line);
        }
        for line in &self.tail_lines {
            out.push(line);
        }
        push_padding_rows(out, width, padding_y);
    }
}

/// The render state of one append-only body (the live-stream path): the
/// accumulated text plus the rendered lines of its final prefix and of its
/// mutable tail.
///
/// Split out of [`Markdown`] so the live thinking block (`messages.rs`) drives
/// the same incremental render: both hand it the accumulated text per delta,
/// and both get the final lines for a width back.
pub(crate) struct StreamingRender {
    text: String,
    /// Bumped on every body change; the render cache answers on equality.
    generation: u64,
    /// Bumped when the body was NOT extended but replaced: the final prefix
    /// is only reused across an unchanged epoch.
    replace_epoch: u64,
    rendered: Option<RenderedLines>,
    /// Raw bytes parsed+rendered by the last [`StreamingRender::lines`] call
    /// (0 on an exact hit) — see [`StreamingRender::debug_metrics`].
    last_reparsed: usize,
}

/// How many leading and trailing bytes [`extends_body`] probes.
const GROW_PROBE_BYTES: usize = 64;

/// Whether `new` is `old` plus appended bytes, probed in O(1) instead of the
/// whole-prefix memcmp `str::starts_with` performs per delta.
///
/// The live callers are append-only (the stream accumulators only ever grow),
/// but [`StreamingRender::grow`] is also a public-ish snapshot setter that
/// tests feed wholesale replacements. A replacement shorter than the body, or
/// one whose first/last [`GROW_PROBE_BYTES`] bytes differ, is still detected
/// as a replacement; only a non-shrinking replacement that happens to share
/// both probes would be misread as an extension (never produced by the live
/// path).
fn extends_body(old: &str, new: &str) -> bool {
    let old_len = old.len();
    // An extension can only be longer, and must split the new text on a char
    // boundary at the old length (a replacement may not).
    if new.len() < old_len || !new.is_char_boundary(old_len) {
        return false;
    }
    let probe = old_len.min(GROW_PROBE_BYTES);
    let tail_from = old_len - probe;
    let old_bytes = old.as_bytes();
    let new_bytes = new.as_bytes();
    new_bytes[..probe] == old_bytes[..probe]
        && new_bytes[tail_from..old_len] == old_bytes[tail_from..old_len]
}

impl StreamingRender {
    pub(crate) fn new(text: String) -> Self {
        Self {
            text,
            generation: 0,
            replace_epoch: 0,
            rendered: None,
            last_reparsed: 0,
        }
    }

    pub(crate) fn text(&self) -> &str {
        &self.text
    }

    /// Replace the body wholesale; the final prefix is dropped.
    pub(crate) fn replace(&mut self, text: String) {
        if text == self.text {
            // Same bytes (a rebuild re-feeding the entry): the cached lines
            // still answer, nothing to drop.
            return;
        }
        self.text = text;
        self.generation += 1;
        self.replace_epoch += 1;
        self.rendered = None;
        self.last_reparsed = 0;
    }

    /// Set the body from a full snapshot: an extension of the current body
    /// copies only the appended bytes (the append-only stream contract); any
    /// other text is a replacement.
    ///
    /// The append fast path never compares the accumulated body: a snapshot
    /// whose length differs can only be an extension or a replacement, and
    /// [`extends_body`] probes the old prefix/suffix in O(1). The full
    /// equality compare runs only when the lengths match (the idle re-feed
    /// case), so a growing reply costs O(appended bytes) per delta instead of
    /// O(accumulated bytes).
    pub(crate) fn grow(&mut self, text: &str) {
        if text.len() == self.text.len() {
            if text == self.text {
                // The frame re-fed the same body (an idle/spinner frame): the
                // generation stays put so the cached lines answer it.
                return;
            }
        } else if extends_body(&self.text, text) {
            let appended = &text[self.text.len()..];
            self.text.push_str(appended);
            self.generation += 1;
            self.last_reparsed = 0;
            return;
        }
        self.text.clear();
        self.text.push_str(text);
        self.replace_epoch += 1;
        self.rendered = None;
        self.generation += 1;
        self.last_reparsed = 0;
    }

    /// Append one live delta to the body without a snapshot compare.
    ///
    /// The caller (the live-entry patch path) establishes that `delta` is
    /// exactly the bytes the accumulator gained since the previous feed, so
    /// the whole accumulated body never has to be compared or copied.
    pub(crate) fn grow_delta(&mut self, delta: &str) {
        if delta.is_empty() {
            return;
        }
        self.text.push_str(delta);
        self.generation += 1;
        self.last_reparsed = 0;
    }

    pub(crate) fn invalidate(&mut self) {
        self.rendered = None;
        self.last_reparsed = 0;
    }

    /// The trait's owned-line form: the same rows, built through
    /// [`StreamingRender::lines_into`] in a fresh pool. The frame renderer
    /// drives the canvas path; this bridge keeps `render` correct for callers
    /// that ask for lines (tests, alternate drivers).
    pub(crate) fn lines(
        &mut self,
        width: usize,
        padding_x: usize,
        padding_y: usize,
        theme: &MarkdownTheme,
        options: &MarkdownOptions,
        paint: Option<&dyn Fn(&str) -> String>,
    ) -> Vec<String> {
        let mut lines = Vec::new();
        {
            let mut rows = Rows::new(&mut lines);
            self.lines_into(
                width, padding_x, padding_y, theme, options, paint, &mut rows,
            );
            rows.trim();
        }
        lines
    }

    /// The final padded lines for the current body at `width`, reusing the
    /// cached final prefix when the body only grew and every other input is
    /// unchanged. `paint`, when set, wraps every line (the active theme's
    /// generation is part of the cache key, so painted lines are stable).
    ///
    /// Direct-write form: the padding rows, the cached final prefix and the
    /// mutable tail are pushed straight into the frame's recycled row buffers.
    /// On an exact cache hit (the steady-state frame) not one row buffer is
    /// allocated; on a miss the prefix/tail buffers are still built for the
    /// cache, but the frame no longer pays the assembled `Vec` and its
    /// per-row clones on top. Row for row byte-identical to
    /// [`StreamingRender::lines`], which bridges through this method.
    // Every parameter is an independent rendering input (padding axes, theme,
    // options, the optional paint wrapper, the output canvas); bundling them
    // into a struct would only move the same list to the call sites.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn lines_into(
        &mut self,
        width: usize,
        padding_x: usize,
        padding_y: usize,
        theme: &MarkdownTheme,
        options: &MarkdownOptions,
        paint: Option<&dyn Fn(&str) -> String>,
        out: &mut Rows<'_>,
    ) {
        // Read the generation *before* rendering: a theme swap landing
        // mid-render leaves the lines tagged with the older generation, so the
        // next frame re-renders instead of reusing a mixed-palette entry.
        let theme_generation = Theme::generation();
        if self.text.trim().is_empty() {
            // The classic path renders no blocks for an empty body; there is
            // nothing to reuse either.
            self.rendered = None;
            self.last_reparsed = 0;
            push_padding_rows(out, width, padding_y);
            return;
        }
        if let Some(rendered) = self.rendered.as_ref() {
            if rendered.answers(
                self.generation,
                width,
                padding_x,
                padding_y,
                theme_generation,
                options.mermaid,
                options.is_streaming,
                paint.is_some(),
            ) {
                self.last_reparsed = 0;
                rendered.assemble_into(width, padding_y, out);
                return;
            }
        }
        // Reuse the final prefix only when the body only grew (same epoch) and
        // every other render input is unchanged; otherwise re-parse from the
        // top exactly like the classic path.
        let previous = self.rendered.take().filter(|rendered| {
            rendered.replace_epoch == self.replace_epoch
                && rendered.width == width
                && rendered.padding_x == padding_x
                && rendered.padding_y == padding_y
                && rendered.theme_generation == theme_generation
                && rendered.mermaid == options.mermaid
                && rendered.is_streaming == options.is_streaming
                && rendered.painted == paint.is_some()
                && rendered.final_len <= self.text.len()
        });
        let (mut final_len, mut prefix_lines, mut fence_tail) = match previous {
            Some(rendered) => (
                rendered.final_len,
                rendered.prefix_lines,
                rendered.fence_tail,
            ),
            None => (0, Vec::new(), None),
        };

        let content_width = width.saturating_sub(padding_x * 2).max(1);
        let left = " ".repeat(padding_x);
        let right = " ".repeat(padding_x);
        let prefix_was = prefix_lines.len();
        let mut tail_lines: Vec<String> = Vec::new();
        let reparsed = render_tail_into(
            &self.text,
            &mut final_len,
            &mut prefix_lines,
            &mut fence_tail,
            &mut tail_lines,
            content_width,
            &left,
            &right,
            width,
            theme,
            options,
        );
        if let Some(paint) = paint {
            // Paint exactly the lines this call produced: the cached prefix
            // lines were painted when they became final, and the painter
            // prepends its escape to the whole row (`theme.fg`), so painting a
            // reused row again would stack a second escape on it. The mutable
            // tail is rebuilt every call, so all of its lines are new.
            for line in prefix_lines[prefix_was..]
                .iter_mut()
                .chain(tail_lines.iter_mut())
            {
                *line = paint(line);
            }
        }

        let rendered = RenderedLines {
            generation: self.generation,
            replace_epoch: self.replace_epoch,
            width,
            padding_x,
            padding_y,
            theme_generation,
            mermaid: options.mermaid,
            is_streaming: options.is_streaming,
            painted: paint.is_some(),
            final_len,
            prefix_lines,
            tail_lines,
            fence_tail,
        };
        // The frame's rows: padding, final prefix, mutable tail, padding. The
        // cache keeps the prefix/tail buffers (they answer the next frame);
        // the canvas rows are copies of them.
        rendered.assemble_into(width, padding_y, out);
        self.last_reparsed = reparsed;
        self.rendered = Some(rendered);
    }

    /// `(final-prefix raw bytes, raw bytes re-parsed by the last render)`.
    pub(crate) fn debug_metrics(&self) -> (usize, usize) {
        (
            self.rendered
                .as_ref()
                .map_or(0, |rendered| rendered.final_len),
            self.last_reparsed,
        )
    }
}

/// The open-paragraph tail candidate: the trailing paragraph's complete
/// lines when they are provably append-stable (see [`paragraph_candidate`]).
struct ParagraphCandidate {
    /// Index of the trailing `Block::Paragraph` in the parsed tail.
    block_index: usize,
    /// The paragraph's text through the last complete line (no trailing
    /// newline).
    stable_text: String,
    /// Number of complete lines in `stable_text`.
    stable_lines: usize,
    /// Byte offset (in the normalized tail) of the mutable last line.
    mutable_offset: usize,
}

/// The trailing block of `parsed` when it is a paragraph whose complete lines
/// can be folded into the prefix.
///
/// A paragraph line's wrapping is per physical line, but inline markup can
/// carry across lines (`**bold\nmore**`), and a later line can reinterpret an
/// earlier one (a table delimiter after a `|` header, a closing `$$`). The
/// cut is therefore only taken when the complete-line prefix contains no
/// character that can start an inline span or a block reinterpretation — the
/// plain-prose streaming case, where a line's rendered row is a pure function
/// of that line. Everything else keeps the existing whole-block tail.
fn paragraph_candidate(
    parsed: &blocks::ParsedBlocks,
    normalized: &str,
) -> Option<ParagraphCandidate> {
    let block_index = parsed.blocks.len().checked_sub(1)?;
    let Block::Paragraph(text) = &parsed.blocks[block_index] else {
        return None;
    };
    let stable = stable_line_count(text);
    if stable == 0 {
        return None;
    }
    let last_newline = text.rfind('\n')?;
    let stable_text = &text[..last_newline];
    if !inline_inert(stable_text) {
        return None;
    }
    let start_line = *parsed.block_starts.get(block_index)?;
    let mutable_offset = parsed
        .line_starts
        .get(start_line + stable)
        .copied()
        .unwrap_or(normalized.len());
    Some(ParagraphCandidate {
        block_index,
        stable_text: stable_text.to_string(),
        stable_lines: stable,
        mutable_offset,
    })
}

/// Whether `text` contains nothing the inline renderer or the block parser
/// could reinterpret across a line boundary: emphasis/strike/code/math
/// delimiters, links/autolinks/entities, escapes, table pipes, and GFM
/// extended-autolink triggers.
fn inline_inert(text: &str) -> bool {
    !text.bytes().any(|byte| {
        matches!(
            byte,
            b'*' | b'_'
                | b'~'
                | b'`'
                | b'$'
                | b'['
                | b']'
                | b'<'
                | b'>'
                | b'&'
                | b'\\'
                | b'|'
                | b'@'
        )
    }) && !text.contains("://")
        && !text.contains("www.")
}

/// The open-fence tail candidate of one parsed mutable tail: everything the
/// first fence frame needs to move the cut from the block boundary to the
/// block's last line.
struct FenceCandidate {
    /// Index of the trailing `Block::Code` in the parsed tail.
    block_index: usize,
    lang: String,
    fence_char: char,
    fence_len: usize,
    /// Whether the opener line was unindented (see [`FenceTail`]).
    opener_unindented: bool,
    /// The block's full normalized code text.
    code_text: String,
    /// Number of code lines in `code_text`.
    code_lines: usize,
    /// Content lines before the first still-mutable line.
    stable: usize,
    /// Byte offset (in the normalized tail) of that line.
    mutable_offset: usize,
}

/// One pass (repeating past a fence closer) of the incremental tail render.
///
/// Parses the bytes from `*final_len` on, folds every block that is final into
/// `prefix_lines`, renders the mutable remainder into `tail_lines`, and
/// advances `*final_len` to the last line that can still change. When the tail
/// ends inside an open fence, `*fence_tail` is left describing it; the next
/// frame then parses only from the fence's mutable line, with a synthetic
/// opener recreating the block context. Returns the parsed byte count (the
/// `debug_incremental_metrics` proof counter).
#[allow(clippy::too_many_arguments)]
fn render_tail_into(
    text: &str,
    final_len: &mut usize,
    prefix_lines: &mut Vec<String>,
    fence_tail: &mut Option<FenceTail>,
    tail_lines: &mut Vec<String>,
    content_width: usize,
    left: &str,
    right: &str,
    width: usize,
    theme: &MarkdownTheme,
    options: &MarkdownOptions,
) -> usize {
    let mut reparsed = 0usize;
    loop {
        let raw_tail = &text[*final_len..];
        // A tail that begins with a newline immediately after a non-newline
        // byte resumes at the END of a line that is already folded: the
        // unterminated-fence-closer frame advances to the end of the closer
        // text and the next appended byte is that line's terminator. Parsing
        // it in isolation would invent an empty line the document does not
        // have (the closer is not followed by a blank row), so consume it as
        // what it is — the previous line's terminator.
        if *final_len > 0
            && raw_tail.starts_with('\n')
            && text.as_bytes().get(*final_len - 1) != Some(&b'\n')
        {
            *final_len += 1;
            continue;
        }
        reparsed = reparsed.saturating_add(raw_tail.len());
        // Tab expansion is a per-character substitution and the cut is at a
        // line start, so normalizing the tail alone is byte-identical to the
        // normalized full text's tail.
        let normalized: Cow<'_, str> = if raw_tail.contains('\t') {
            Cow::Owned(raw_tail.replace('\t', "   "))
        } else {
            Cow::Borrowed(raw_tail)
        };

        // Resume inside an open fence: recreate the opener so the parser sees
        // a code block, then parse only the mutable line and the appended
        // bytes after it. The immutable borrow ends here — the block below
        // mutates the fence state.
        let resuming = fence_tail.as_ref().map(|fence| {
            let mut opener = String::new();
            if !fence.opener_unindented {
                // One leading space is enough to reproduce the parser's
                // `unindented` gate (see `FenceTail::opener_unindented`).
                opener.push(' ');
            }
            opener.push_str(&fence.fence_char.to_string().repeat(fence.fence_len));
            opener.push_str(&fence.lang);
            let source = format!("{opener}\n{normalized}");
            (opener, source)
        });
        if let Some((opener, source)) = resuming {
            let synthetic_len = opener.len() + 1;
            let parsed = parse_blocks(&source, options);
            let Some(Block::Code {
                text: tail_code, ..
            }) = parsed.blocks.first()
            else {
                // Defensive: the synthetic opener always yields a code block.
                // Drop the state rather than loop on a malformed tail.
                *fence_tail = None;
                *final_len = text.len();
                break;
            };
            let tail_code = tail_code.clone();
            // The `resuming` snapshot above came from this same `Some`, so the
            // slot cannot have been cleared in between; a `None` here is
            // unreachable but tolerable — dropping to the full-render path
            // below is still correct, so no panic is warranted.
            let Some(fence) = fence_tail.as_mut() else {
                *final_len = text.len();
                break;
            };
            // The synthetic source is one opener line followed by this tail.
            // `closer_line`/`code_source` describe the code lines the parser
            // could have consumed: everything after the opener, up to a closer
            // (or the end of the tail).
            let source_lines: Vec<&str> = source.lines().collect();
            let closer_line = find_fence_closer(&source, fence.fence_char, fence.fence_len);
            let code_end = closer_line.unwrap_or(source_lines.len());
            let code_source: &[&str] = if code_end > 1 {
                &source_lines[1..code_end]
            } else {
                &[]
            };
            // `tail_code` is the join of the code lines the parser KEPT; an
            // empty join is ambiguous (no code line vs. one empty line), so
            // the first source line disambiguates. `trimmed_tail` marks the
            // trailing partial-closer line `trimPartialClosingFences` dropped.
            let mut tail_code_lines = code_line_count(&tail_code);
            if tail_code_lines == 0 && code_source.first().is_some_and(|line| line.is_empty()) {
                tail_code_lines = 1;
            }
            let trimmed_tail = closer_line.is_none() && tail_code_lines < code_source.len();
            // The full code text through the current last line: keep the
            // already-folded lines and append this tail's kept code lines —
            // exactly the join a from-scratch parse produces. (The mutable
            // suffix the tail re-parsed is replaced by the same lines.)
            truncate_to_lines(&mut fence.code_text, fence.stable_lines, fence.code_lines);
            if tail_code_lines > 0 {
                // A separator is needed after every already-folded line, even
                // when `code_text` is empty: `stable_lines == 1` with an empty
                // folded line is represented by "" but is still one line.
                if fence.stable_lines > 0 {
                    fence.code_text.push('\n');
                }
                fence.code_text.push_str(&tail_code);
            }
            let total_lines = fence.stable_lines + tail_code_lines;
            fence.code_lines = total_lines;
            // Only a still-unterminated final code line can change when more
            // bytes arrive; every newline-terminated line is append-stable.
            let last_line_mutable = tail_code_lines > 0
                && closer_line.is_none()
                && !trimmed_tail
                && !normalized.ends_with('\n');
            let stable_new = if last_line_mutable {
                total_lines.saturating_sub(1)
            } else {
                total_lines
            };
            let rows = render::code_block_content_lines_from(
                theme,
                &fence.lang,
                &fence.code_text,
                fence.stable_lines,
            );
            debug_assert_eq!(
                rows.len(),
                total_lines.max(1) - fence.stable_lines,
                "one highlighted row per code line (plus the empty-block row)"
            );
            if let Some(close_line) = closer_line {
                // The fence closed in this tail: every content line is final.
                push_wrapped_padded(prefix_lines, rows, content_width, left, right, width);
                let mut closing: Vec<String> = vec![(theme.code_block_border)("```")];
                let next_is_space = parsed
                    .blocks
                    .get(1)
                    .is_some_and(|block| matches!(block, Block::Space));
                if parsed.blocks.len() > 1 && !next_is_space {
                    closing.push(String::new());
                }
                push_wrapped_padded(prefix_lines, closing, content_width, left, right, width);
                let next_offset = parsed
                    .line_starts
                    .get(close_line + 1)
                    .copied()
                    .unwrap_or(source.len());
                let normalized_offset = next_offset.saturating_sub(synthetic_len);
                *final_len = final_len.saturating_add(raw_len_for_normalized(
                    raw_tail,
                    normalized_offset.min(normalized.len()),
                ));
                *fence_tail = None;
                if *final_len >= text.len() {
                    break;
                }
                continue;
            }
            let split = stable_new
                .saturating_sub(fence.stable_lines)
                .min(rows.len());
            let (stable_rows, mutable_rows) = rows.split_at(split);
            push_wrapped_padded(
                prefix_lines,
                stable_rows.to_vec(),
                content_width,
                left,
                right,
                width,
            );
            push_wrapped_padded(
                tail_lines,
                mutable_rows.to_vec(),
                content_width,
                left,
                right,
                width,
            );
            // The classic block renderer always draws a bottom border, even
            // while the source fence's own closer has not arrived yet (that is
            // what `trimPartialClosingFences` keeps stable mid-stream). The
            // live tail must draw it too, or the streamed rows diverge from a
            // from-scratch render of the same prefix (and from the settled
            // message component).
            push_wrapped_padded(
                tail_lines,
                vec![(theme.code_block_border)("```")],
                content_width,
                left,
                right,
                width,
            );
            // The mutable line is code line `stable_new` of the WHOLE code
            // text, but `parsed` is a parse of the synthetic source
            // (`opener` + this tail), whose code lines start at content line
            // `fence.stable_lines`. Indexing `line_starts` with `1 +
            // stable_new` therefore points `fence.stable_lines` lines too far:
            // the cut lands past lines this frame never folded into the prefix
            // (they are dropped for good) — the `value_2` loss the byte-identity
            // tests catch. Convert to the synthetic line index first, and then
            // the final prefix advances past exactly the rows `split` folded.
            // When every code line is stable (`stable_new == total_lines`) the
            // index is the first line after the last code line (a closer, a
            // dropped partial closer, or EOF) — still the correct cut.
            let synthetic_line = 1 + stable_new.saturating_sub(fence.stable_lines);
            let cut = parsed
                .line_starts
                .get(synthetic_line)
                .copied()
                .unwrap_or(source.len());
            fence.stable_lines = stable_new;
            *final_len = final_len.saturating_add(raw_len_for_normalized(
                raw_tail,
                cut.saturating_sub(synthetic_len).min(normalized.len()),
            ));
            break;
        }

        let parsed = parse_blocks(&normalized, options);
        if let Some(candidate) = fence_candidate(&parsed, &normalized, options, theme) {
            // Everything before the open fence is final: fold it, the opener
            // row and the stable code lines into the prefix, and keep only the
            // fence's last line mutable.
            let mut newly_final: Vec<String> = Vec::new();
            render_block_range(
                &mut newly_final,
                &parsed.blocks,
                0..candidate.block_index,
                content_width,
                theme,
                options,
            );
            push_wrapped_padded(prefix_lines, newly_final, content_width, left, right, width);
            let opener = (theme.code_block_border)(&format!("```{}", candidate.lang));
            push_wrapped_padded(
                prefix_lines,
                vec![opener],
                content_width,
                left,
                right,
                width,
            );
            let rows = render::code_block_content_lines_from(
                theme,
                &candidate.lang,
                &candidate.code_text,
                0,
            );
            let split = candidate.stable.min(rows.len());
            let (stable_rows, mutable_rows) = rows.split_at(split);
            push_wrapped_padded(
                prefix_lines,
                stable_rows.to_vec(),
                content_width,
                left,
                right,
                width,
            );
            push_wrapped_padded(
                tail_lines,
                mutable_rows.to_vec(),
                content_width,
                left,
                right,
                width,
            );
            // Same synthetic bottom border as the resume path: the classic
            // block renderer closes an open fence, so the live tail must too.
            push_wrapped_padded(
                tail_lines,
                vec![(theme.code_block_border)("```")],
                content_width,
                left,
                right,
                width,
            );
            *final_len = final_len.saturating_add(raw_len_for_normalized(
                raw_tail,
                candidate.mutable_offset.min(normalized.len()),
            ));
            *fence_tail = Some(FenceTail {
                lang: candidate.lang,
                fence_char: candidate.fence_char,
                fence_len: candidate.fence_len,
                opener_unindented: candidate.opener_unindented,
                code_text: candidate.code_text,
                code_lines: candidate.code_lines,
                stable_lines: candidate.stable,
            });
            break;
        }

        if let Some(candidate) = paragraph_candidate(&parsed, &normalized) {
            // The paragraph's complete lines are inert prose: fold them (and
            // every earlier block) into the prefix and re-parse only the last
            // line on the next pass.
            let advance =
                raw_len_for_normalized(raw_tail, candidate.mutable_offset.min(normalized.len()));
            if advance > 0 {
                let mut newly_final: Vec<String> = Vec::new();
                render_block_range(
                    &mut newly_final,
                    &parsed.blocks,
                    0..candidate.block_index,
                    content_width,
                    theme,
                    options,
                );
                push_wrapped_padded(prefix_lines, newly_final, content_width, left, right, width);
                let stable_rows: Vec<String> = candidate
                    .stable_text
                    .split('\n')
                    .map(str::to_string)
                    .collect();
                debug_assert_eq!(stable_rows.len(), candidate.stable_lines);
                push_wrapped_padded(prefix_lines, stable_rows, content_width, left, right, width);
                *final_len = final_len.saturating_add(advance);
                continue;
            }
        }

        let (cut_block, cut_offset) = parsed.final_boundary().unwrap_or((0, 0));
        // Newly final blocks: fold and cache them in the prefix. Their last
        // block's `next` is `parsed.blocks[cut_block]` — the `Space` block at
        // the cut — so the fold matches the from-scratch one exactly.
        let mut newly_final: Vec<String> = Vec::new();
        render_block_range(
            &mut newly_final,
            &parsed.blocks,
            0..cut_block,
            content_width,
            theme,
            options,
        );
        push_wrapped_padded(prefix_lines, newly_final, content_width, left, right, width);
        // The mutable tail (from the cut to the end of the text).
        let mut logical: Vec<String> = Vec::new();
        render_block_range(
            &mut logical,
            &parsed.blocks,
            cut_block..parsed.blocks.len(),
            content_width,
            theme,
            options,
        );
        push_wrapped_padded(tail_lines, logical, content_width, left, right, width);
        *final_len = final_len.saturating_add(raw_len_for_normalized(raw_tail, cut_offset));
        break;
    }
    reparsed
}

/// The trailing block of `parsed` when it is a still-open fenced code block
/// whose last line can be made the incremental cut.
///
/// The opener line must be terminated (a partial info string is still
/// mutable) and no closer may follow; mermaid diagrams are excluded because
/// their output depends on the whole body, not per line.
fn fence_candidate(
    parsed: &blocks::ParsedBlocks,
    normalized: &str,
    options: &MarkdownOptions,
    theme: &MarkdownTheme,
) -> Option<FenceCandidate> {
    // A custom highlight hook may return any shape; the line cut assumes the
    // built-in one-line-per-input-line contract.
    if !render::uses_default_highlighter(theme) {
        return None;
    }
    let block_index = parsed.blocks.len().checked_sub(1)?;
    let Block::Code { lang, text } = &parsed.blocks[block_index] else {
        return None;
    };
    let start_line = *parsed.block_starts.get(block_index)?;
    let line_count = parsed.line_starts.len().checked_sub(1)?;
    let opener = normalized.lines().nth(start_line)?;
    let (fence_char, fence_len, _info) = parse_fence_opener_line(opener.trim_start())?;
    // A source-final opener without a newline can still grow its info string.
    if start_line + 1 >= line_count && !parsed.ends_with_newline {
        return None;
    }
    if normalized
        .lines()
        .skip(start_line + 1)
        .any(|line| is_fence_closer_line(line, fence_char, fence_len))
    {
        return None;
    }
    if options.mermaid != MermaidRenderingMode::Off
        && lang
            .split_whitespace()
            .next()
            .is_some_and(|first| first.eq_ignore_ascii_case("mermaid"))
    {
        return None;
    }
    let lines = code_line_count(text);
    let opener_unindented = opener.len() == opener.trim_start().len();
    // With no closer (checked above), every line after the opener is a code
    // line except a trailing partial closing-fence line that
    // `trimPartialClosingFences` dropped. That trim only runs for an
    // unindented opener.
    let trimmed_tail = opener_unindented
        && options.trim_partial_closing_fences
        && normalized.lines().count() > start_line + 1 + lines;
    // Complete code lines can be folded into the prefix: a newline-terminated
    // line is append-stable, and a dropped partial closer always follows a
    // terminated line. Only a source-final, unterminated line stays mutable.
    let stable = if normalized.ends_with('\n') || trimmed_tail {
        lines
    } else {
        lines.saturating_sub(1)
    };
    let mutable_line = start_line + 1 + stable;
    let mutable_offset = parsed
        .line_starts
        .get(mutable_line)
        .copied()
        .unwrap_or(normalized.len());
    Some(FenceCandidate {
        block_index,
        lang: lang.clone(),
        fence_char,
        fence_len,
        opener_unindented,
        code_text: text.clone(),
        code_lines: lines,
        stable,
        mutable_offset,
    })
}

/// Number of `\n`-separated lines before the (possibly partial) last line of
/// a block text: the count of code lines that can be folded into the prefix.
fn stable_line_count(text: &str) -> usize {
    match text.rfind('\n') {
        Some(index) => text[..index].bytes().filter(|&byte| byte == b'\n').count() + 1,
        None => 0,
    }
}

/// Number of `\n`-separated lines in a code text. The empty string is zero
/// lines — but an open fence with no content lines still renders one
/// (phantom) empty row, which callers keep in the mutable tail.
fn code_line_count(text: &str) -> usize {
    if text.is_empty() {
        0
    } else {
        text.split('\n').count()
    }
}

/// Truncate a code text that currently holds `current` lines to its first
/// `lines` lines (separators included). Scans backward from the end, so the
/// cost is the removed suffix (usually the one mutable line), not the whole
/// text.
fn truncate_to_lines(text: &mut String, lines: usize, current: usize) {
    if lines >= current {
        return;
    }
    if lines == 0 {
        text.clear();
        return;
    }
    let remove = current - lines;
    let mut seen = 0usize;
    for (index, byte) in text.bytes().enumerate().rev() {
        if byte == b'\n' {
            seen += 1;
            if seen == remove {
                text.truncate(index);
                return;
            }
        }
    }
    text.clear();
}

/// Upstream fence opener `^ {0,3}(`{3,}|~{3,})([^\n]*)` (the subset the block
/// parser accepts). Kept local so the incremental cut does not depend on the
/// parser's private helpers.
fn parse_fence_opener_line(trimmed: &str) -> Option<(char, usize, String)> {
    let fence_char = trimmed.chars().next()?;
    if fence_char != '`' && fence_char != '~' {
        return None;
    }
    let run = trimmed.chars().take_while(|&c| c == fence_char).count();
    if run < 3 {
        return None;
    }
    let info: String = trimmed.chars().skip(run).collect();
    Some((fence_char, run, info.trim().to_string()))
}

/// The block parser's closing-fence rule (` {0,3}\1[~`]* *(?=\n|$)`).
fn is_fence_closer_line(line: &str, fence_char: char, fence_len: usize) -> bool {
    let mut chars = line.trim_start_matches(' ').chars().peekable();
    let mut run = 0usize;
    while chars.peek() == Some(&fence_char) {
        chars.next();
        run += 1;
    }
    if run < fence_len {
        return false;
    }
    let mut past_fence_chars = false;
    for c in chars {
        match c {
            '`' | '~' if !past_fence_chars => {}
            ' ' => past_fence_chars = true,
            _ => return false,
        }
    }
    true
}

/// First closer line in `source` after its synthetic opener line.
fn find_fence_closer(source: &str, fence_char: char, fence_len: usize) -> Option<usize> {
    source
        .lines()
        .enumerate()
        .skip(1)
        .find(|(_, line)| is_fence_closer_line(line, fence_char, fence_len))
        .map(|(index, _)| index)
}

/// `padding_y` blank rows for the top and for the bottom of a body, written
/// into the canvas: each row is exactly `" ".repeat(width)` (the pre-migration
/// `vec![" ".repeat(width); padding_y * 2]`), built in a pooled row buffer.
fn push_padding_rows(out: &mut Rows<'_>, width: usize, padding_y: usize) {
    for _ in 0..padding_y * 2 {
        out.push("");
        let row = out.line_mut();
        row.reserve(width);
        for _ in 0..width {
            row.push(' ');
        }
    }
}

/// The raw byte length whose tab-expanded form is `normalized_offset` bytes
/// long. The cut is at a line start, so it never lands inside an expanded tab.
fn raw_len_for_normalized(raw: &str, normalized_offset: usize) -> usize {
    let mut normalized = 0usize;
    for (index, ch) in raw.char_indices() {
        if normalized >= normalized_offset {
            return index;
        }
        normalized += if ch == '\t' { 3 } else { ch.len_utf8() };
    }
    raw.len()
}

/// Wrap + side margins + `pad_to_width` tail (upstream render tail), appended
/// to `out` — the per-logical-line steps [`render_padded_lines`] runs.
fn push_wrapped_padded(
    out: &mut Vec<String>,
    logical: Vec<String>,
    content_width: usize,
    left: &str,
    right: &str,
    width: usize,
) {
    for line in logical {
        for wrapped in wrap_text_with_ansi(&line, content_width) {
            out.push(pad_to_width(&format!("{left}{wrapped}{right}"), width));
        }
    }
}

/// Parse `text` and produce the final full-width lines [`Markdown`] renders:
/// markdown body at the padding-inset content width, then the wrap + side
/// margins + `pad_to_width` tail (upstream render tail). Shared so a caller
/// holding its own persistent cache (the assistant message's per-segment
/// cache) goes through exactly one implementation of the layout rules.
pub(crate) fn render_padded_lines(
    text: &str,
    width: usize,
    padding_x: usize,
    padding_y: usize,
    theme: &MarkdownTheme,
    options: &MarkdownOptions,
) -> Vec<String> {
    let content_width = width.saturating_sub(padding_x * 2).max(1);
    let rendered = render_markdown(text, content_width, theme, options);
    // Wrap + margins + pad (upstream render tail).
    let left = " ".repeat(padding_x);
    let right = " ".repeat(padding_x);
    let mut lines = Vec::new();
    for _ in 0..padding_y {
        lines.push(" ".repeat(width));
    }
    push_wrapped_padded(&mut lines, rendered, content_width, &left, &right, width);
    for _ in 0..padding_y {
        lines.push(" ".repeat(width));
    }
    lines
}

// ---------------------------------------------------------------------------
// Block parsing & rendering
// ---------------------------------------------------------------------------

/// One parsed markdown block.
#[derive(Debug, Clone, PartialEq)]
enum Block {
    Heading {
        depth: usize,
        text: String,
    },
    Paragraph(String),
    Code {
        lang: String,
        text: String,
    },
    List {
        ordered: bool,
        start: u64,
        items: Vec<String>,
        /// Raw marker per item (`"1)"`, `"3."`, `"-"`, `"*"`), used only by
        /// `preserve_ordered_list_markers` (upstream `getOrderedListMarker` /
        /// `getUnorderedListMarker`, markdown.ts:753-761).
        markers: Vec<String>,
        /// GFM task state per item (upstream `item.task`/`item.checked`):
        /// `None` = not a task, `Some(checked)` for `- [x]`/`- [ ]` items.
        tasks: Vec<Option<bool>>,
        loose: bool,
    },
    Blockquote(String),
    Table {
        header: Vec<String>,
        rows: Vec<Vec<String>>,
        /// Raw source lines (header + delimiter + rows joined by `\n`),
        /// used by the too-narrow fallback (upstream `token.raw`,
        /// markdown.ts:871).
        raw: String,
    },
    Rule,
    /// Block LaTeX (`latexBlock` token, markdown.ts:101-121 / :515-528).
    Latex {
        /// Math source between the delimiters (trimmed when closed).
        text: String,
        /// Streamed partial block: render the raw source verbatim.
        pending: bool,
        /// Full token raw including the `$$`/`\[` delimiters.
        raw: String,
    },
    Space,
}
mod blocks;
mod inline;
mod latex;
mod latex_scan;
mod render;

// Internal cross-submodule helpers: re-exported (privately) so every child
// module reaches them through `use super::*`.
use blocks::parse_blocks;
use inline::render_inline_with_context;
use latex_scan::{emit_inline_latex, scan_block_latex, tokenize_inline_latex};
use render::{render_block_range, render_markdown};
