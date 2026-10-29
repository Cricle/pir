//! Chat message components — port of upstream `user-message.ts`,
//! `assistant-message.ts`, and `tool-execution.ts` rendering.
//!
//! Contracts pinned from the TypeScript:
//! - user messages: markdown in a `userMessageBg` block, 1-cell side
//!   padding, **no "You" label**;
//! - assistant messages: spacer + trimmed markdown per text block; thinking
//!   blocks render dim-italic (or the `Thinking...` label when hidden);
//!   error/abort/length tails render in the error color;
//! - tool executions: a `toolPendingBg` block while running that flips to
//!   `toolSuccessBg`/`toolErrorBg` when done; per-tool call formatters
//!   match upstream (`$ cmd`, `read path`, `edit path`, `grep /pat/`, …).
//!
//! # Row caches and their budget
//!
//! Every block type here memoizes the rows it last built (`SegmentCache`,
//! `ResultRowsCache`/`CallRowsCache`/`UserBashRowsCache`/`Edit*RowsCache`,
//! `TextRowsCache`, the markdown caches), each keyed by the render inputs it
//! depends on — the per-frame allocation ceilings in
//! `tests/tui_frame_reuse.rs` are what those memos buy. The caches are
//! owned by the COMPONENT, so their memory lifetime is the component's: a
//! block that leaves the transcript window is dropped by
//! `ChatContainer::set_entries` (the windowed slice) and takes its rows with
//! it. The block-level cap therefore lives with the window that decides
//! which components exist — [`crate::tui::terminal::TranscriptWindow`] and
//! its `TRANSCRIPT_TAIL_BYTES` / `TRANSCRIPT_WINDOW_MAX_BYTES` /
//! `TRANSCRIPT_WINDOW_MAX_ENTRIES` budgets — rather than in a per-component
//! LRU here: a long session's resident rows are bounded by the window, and
//! the components inside it keep the memo guarantees above.

// Pre-existing baseline lints (tui is frozen for the parity batch).
#![allow(clippy::field_reassign_with_default)]
#![allow(clippy::collapsible_match)]

use std::borrow::Cow;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, RwLock};

/// One relaxed increment on a [`ToolMemoWork`] counter.
macro_rules! bump_memo_work {
    ($field:ident) => {
        TOOL_MEMO_WORK.$field.fetch_add(1, Ordering::Relaxed)
    };
}

use super::ansi::{visible_width, wrap_text_with_ansi, BG_RESET};
use super::core::{Component, Rows, Text};
use super::markdown::{
    render_padded_lines, Markdown, MarkdownOptions, MarkdownTheme, StreamingRender,
};
use super::syntax;
use super::theme::{with_theme, Theme, ThemeColor};
use crate::ai::types::{AssistantMessage, ContentBlock, ImageContent};
use crate::coding_agent::truncate::{format_size, DEFAULT_MAX_BYTES, DEFAULT_MAX_LINES};
use crate::config::settings::MermaidRenderingMode;

/// User message component (upstream `UserMessageComponent`): markdown
/// inside a padded background box. Deliberately renders **no "You:"**
/// prefix — the background block is the only affordance.
pub struct UserMessageComponent {
    boxed: BoxContainer,
    /// Memo of this message's final painted rows (see
    /// [`UserMessageRowsCache`]).
    cache: Mutex<Option<UserMessageRowsCache>>,
}

/// Memo of one user message's painted rows.
///
/// The message text is fixed at construction (`UserMessageComponent::new`;
/// there is no setter), so the box's body and its background painter are a
/// pure function of the render width and the active palette. Without the memo
/// the box repaints every row on every frame (its painter is a closure, so
/// `BoxContainer` itself cannot key it) — the last non-zero steady-frame
/// component of a user turn, i.e. one `String` allocation per row per frame
/// for as long as the message is on screen.
struct UserMessageRowsCache {
    width: usize,
    theme_generation: u64,
    rows: Vec<String>,
}

use super::core::BoxContainer;

impl UserMessageComponent {
    pub fn new(text: &str, output_pad: usize) -> Self {
        let theme = MarkdownTheme::default();
        let options = MarkdownOptions {
            preserve_ordered_list_markers: true,
            preserve_backslash_escapes: true,
            ..MarkdownOptions::default()
        };
        let mut boxed = BoxContainer::new(output_pad, 1, move |s: &str| {
            with_theme(|theme| theme.bg(ThemeColor::UserMessageBg, s))
        });
        boxed.add_child(Markdown::with_options(text, 0, 0, theme, options));
        Self {
            boxed,
            cache: Mutex::new(None),
        }
    }
}

impl Component for UserMessageComponent {
    fn render(&self, width: usize) -> Vec<String> {
        let mut lines = Vec::new();
        {
            let mut rows = Rows::new(&mut lines);
            self.render_into(width, &mut rows);
            rows.trim();
        }
        lines
    }

    /// Direct-write path: a steady frame re-pushes the memoized painted rows
    /// into the frame's recycled row buffers — no re-wrap, no repaint, no
    /// per-row allocation. A width or palette move rebuilds them through the
    /// box (byte-identical to the pre-memo rows).
    fn render_into(&self, width: usize, out: &mut Rows<'_>) {
        let generation = Theme::generation();
        let mut cache = self
            .cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let hit = cache
            .as_ref()
            .is_some_and(|entry| entry.width == width && entry.theme_generation == generation);
        if !hit {
            // The rebuilt frame pool reuses the previous cache rows' buffers
            // (the miss is rare: width / palette only).
            let mut rows = match cache.take() {
                Some(entry) => entry.rows,
                None => Vec::new(),
            };
            {
                let mut canvas = Rows::new(&mut rows);
                canvas.rewind();
                self.boxed.render_into(width, &mut canvas);
                canvas.trim();
            }
            *cache = Some(UserMessageRowsCache {
                width,
                theme_generation: generation,
                rows,
            });
        }
        if let Some(entry) = &*cache {
            for row in &entry.rows {
                out.push(row);
            }
        }
    }

    fn invalidate(&self) {
        *self
            .cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
        self.boxed.invalidate();
    }
}

/// A parsed `<skill …>` user-message block (upstream `ParsedSkillBlock`,
/// agent-session.ts:166-186).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedSkillBlock {
    pub name: String,
    pub location: String,
    /// The skill file body between the opening and closing tags (upstream
    /// `match[3]`, untrimmed).
    pub content: String,
    /// The user text after the block (`\n\n`-separated and trimmed, upstream
    /// `match[4]?.trim() || undefined`); `None` when the block is the whole
    /// message.
    pub user_message: Option<String>,
}

/// Parse a skill block from message text (upstream `parseSkillBlock`,
/// agent-session.ts:177-186).
///
/// The block must span the WHOLE message: the opening tag on the first line,
/// the closing tag on its own line, and an optional `\n\n`-separated user
/// message after it. Anything else stays a normal user message.
pub fn parse_skill_block(text: &str) -> Option<ParsedSkillBlock> {
    static PATTERN: OnceLock<Option<regex::Regex>> = OnceLock::new();
    let pattern = PATTERN.get_or_init(|| {
        // `^<skill name="…" location="…">\n<body>\n</skill>(?:\n\n<user>)?$`
        // — the exact upstream pattern, `[\s\S]` included so the body may
        // contain newlines. A pattern the engine refuses degrades to "no
        // block" (the message renders as plain user text) instead of
        // panicking the render path.
        regex::Regex::new(
            r#"^<skill name="([^"]+)" location="([^"]+)">\n([\s\S]*?)\n</skill>(?:\n\n([\s\S]+))?$"#,
        )
        .ok()
    });
    let captures = pattern.as_ref()?.captures(text)?;
    let user_message = captures
        .get(4)
        .map(|matched| matched.as_str().trim())
        .filter(|message| !message.is_empty())
        .map(str::to_owned);
    Some(ParsedSkillBlock {
        name: captures.get(1)?.as_str().to_owned(),
        location: captures.get(2)?.as_str().to_owned(),
        content: captures.get(3)?.as_str().to_owned(),
        user_message,
    })
}

/// The `[skill]` invocation card (upstream
/// `SkillInvocationMessageComponent`, components/skill-invocation-message.ts):
/// a `customMessageBg` box that collapses to one
/// `[skill] <name> (ctrl+o to expand)` line and expands with Ctrl+O
/// (`app.tools.expand`, shared with the tool blocks) to the `[skill]` label
/// plus the skill body as markdown.
pub struct SkillInvocationComponent {
    tools_expanded: Arc<AtomicBool>,
    collapsed: BoxContainer,
    expanded: BoxContainer,
}

impl SkillInvocationComponent {
    pub fn new(
        block: &ParsedSkillBlock,
        output_pad: usize,
        tools_expanded: Arc<AtomicBool>,
    ) -> Self {
        let background =
            |text: &str| with_theme(|theme| theme.bg(ThemeColor::CustomMessageBg, text));
        // Collapsed: the single `[skill] name (ctrl+o to expand)` line.
        let mut collapsed = BoxContainer::new(output_pad, 1, background);
        collapsed.add_child(Text::with_padding(
            with_theme(|theme| {
                format!(
                    "{}{}{}",
                    theme.fg(
                        ThemeColor::CustomMessageLabel,
                        "\u{1b}[1m[skill]\u{1b}[22m "
                    ),
                    theme.fg(ThemeColor::CustomMessageText, &block.name),
                    theme.fg(ThemeColor::Dim, " (ctrl+o to expand)"),
                )
            }),
            0,
            0,
        ));
        // Expanded: the label, then `**name**` + the body as markdown.
        let mut expanded = BoxContainer::new(output_pad, 1, background);
        expanded.add_child(Text::with_padding(
            with_theme(|theme| {
                theme.fg(ThemeColor::CustomMessageLabel, "\u{1b}[1m[skill]\u{1b}[22m")
            }),
            0,
            0,
        ));
        expanded.add_child(Markdown::with_options(
            format!("**{}**\n\n{}", block.name, block.content),
            0,
            0,
            MarkdownTheme::default(),
            MarkdownOptions::default(),
        ));
        Self {
            tools_expanded,
            collapsed,
            expanded,
        }
    }
}

impl Component for SkillInvocationComponent {
    fn render(&self, width: usize) -> Vec<String> {
        let mut lines = Vec::new();
        {
            let mut rows = Rows::new(&mut lines);
            self.render_into(width, &mut rows);
            rows.trim();
        }
        lines
    }

    /// The expansion posture is read live from the shared `app.tools.expand`
    /// cell, exactly like [`ToolExecutionComponent`], so a Ctrl+O toggle
    /// restyles the card on the current frame without a transcript rebuild.
    fn render_into(&self, width: usize, out: &mut Rows<'_>) {
        if self.tools_expanded.load(Ordering::SeqCst) {
            self.expanded.render_into(width, out);
        } else {
            self.collapsed.render_into(width, out);
        }
    }

    fn invalidate(&self) {
        self.collapsed.invalidate();
        self.expanded.invalidate();
    }
}

/// Assistant message component (upstream `AssistantMessageComponent`).
pub struct AssistantMessageComponent {
    state: Mutex<AssistantState>,
    /// Persistent body cache, one slot per rendered segment in render order.
    /// Before the cache the component built a throwaway [`Markdown`] per
    /// segment on every frame, so a settled message re-parsed, re-wrapped and
    /// re-padded its whole body per frame against an always-cold cache.
    cache: Mutex<Vec<Option<SegmentCache>>>,
    /// Per-block memo of rendered inline-image rows, indexed by the block's
    /// position in the message (see [`AssistantImageRows`]). The message is
    /// immutable for the component's life, so the position identifies the
    /// block and only the width and the terminal capability generation can
    /// move the rows.
    image_rows: Mutex<Vec<Option<AssistantImageRows>>>,
    output_pad: usize,
    /// `" ".repeat(output_pad)`, built ONCE per component (see
    /// [`AssistantMessageComponent::new`]). `output_pad` is fixed at
    /// construction — the transcript rebuilds the component instead of
    /// mutating it — so the pad is an invariant; building it inside
    /// `render_into` allocated one `String` per settled assistant message per
    /// frame only to drop it again.
    pad: String,
}

struct AssistantState {
    message: Arc<AssistantMessage>,
    hide_thinking: bool,
    /// Shared live hidden-thinking label (`ctx.ui.setHiddenThinkingLabel`,
    /// upstream `hiddenThinkingLabel`): `None` = the default "Thinking...".
    /// Every assistant component shares the app's slot, so a relabel reaches
    /// already-rendered entries without rebuilding the transcript.
    hidden_label: Arc<Mutex<Option<String>>>,
}

/// One body segment the assistant render materializes: a non-empty text
/// block, or a run of non-empty thinking blocks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SegmentKind {
    Text,
    Thinking,
}

/// Cache key of one rendered body segment: every *mutable* input the render
/// path reads for that segment — the viewport width, `hideThinkingBlock`
/// plus the live hidden-thinking label (thinking runs), the
/// `markdown.mermaid` snapshot (text blocks), and the active theme's
/// generation. The rendered lines carry theme colors, so a swap must miss
/// ([`Theme::generation`] bumps on every install).
///
/// The source text is deliberately NOT part of the key: the component holds
/// an `Arc<AssistantMessage>` and never mutates it (the transcript rebuilds
/// the component — `new_from` — instead of editing the message), so the
/// segment at a given render position cannot change for the component's
/// life. Storing the text here cost one retained copy per segment plus a
/// full `==` memcmp per frame; the cache slot index identifies the segment
/// (the kind is kept as a cheap defensive check).
///
/// `tools_expanded` is deliberately not a factor: it drives
/// `ToolExecutionComponent` only and cannot change these lines.
#[derive(Debug)]
struct SegmentKey {
    kind: SegmentKind,
    width: usize,
    hidden: bool,
    label: String,
    mermaid: MermaidRenderingMode,
    theme_generation: u64,
}

/// One cached body segment: the key it was rendered for, plus the final
/// lines the frame extends (markdown lines for a text block, painted lines
/// for a visible thinking run, the label line for a hidden run).
struct SegmentCache {
    key: SegmentKey,
    lines: Vec<String>,
}

/// Memo of one inline image block's rendered rows (the
/// [`AssistantMessageComponent`] image arm): the protocol escape lines (or the
/// `[Image: …]` fallback) are a pure function of the immutable image payload,
/// the render width and the resolved terminal capabilities. Without the memo
/// every frame — every spinner tick — re-decoded the base64 prefix and
/// re-encoded the whole protocol payload, an O(image) cost the tool-result
/// image path already avoids through [`ImageRowsCache`].
struct AssistantImageRows {
    width: usize,
    capabilities_generation: u64,
    rows: Vec<String>,
}

/// The borrowed form of a [`SegmentKey`]: what the render knows about a
/// segment before it knows whether the cache hits. Comparing borrows keeps
/// the hit path free of per-frame key allocations (only the reused lines are
/// copied out).
struct SegmentQuery<'a> {
    kind: SegmentKind,
    text: &'a str,
    width: usize,
    hidden: bool,
    label: &'a str,
    mermaid: MermaidRenderingMode,
    theme_generation: u64,
}

impl SegmentKey {
    /// Whether this cached entry answers `query` (same segment source, width,
    /// visibility posture, hidden label, mermaid snapshot and theme
    /// generation).
    fn answers(&self, query: &SegmentQuery<'_>) -> bool {
        self.kind == query.kind
            && self.width == query.width
            && self.hidden == query.hidden
            && self.label == query.label
            && self.mermaid == query.mermaid
            && self.theme_generation == query.theme_generation
    }
}

impl SegmentQuery<'_> {
    /// The owned key stored next to the rendered lines.
    fn to_key(&self) -> SegmentKey {
        SegmentKey {
            kind: self.kind,
            width: self.width,
            hidden: self.hidden,
            label: self.label.to_string(),
            mermaid: self.mermaid,
            theme_generation: self.theme_generation,
        }
    }
}

impl AssistantMessageComponent {
    pub fn new(output_pad: usize) -> Self {
        Self {
            state: Mutex::new(AssistantState {
                message: Arc::new(AssistantMessage::default()),
                hide_thinking: false,
                hidden_label: Arc::new(Mutex::new(None)),
            }),
            cache: Mutex::new(Vec::new()),
            image_rows: Mutex::new(Vec::new()),
            output_pad,
            pad: " ".repeat(output_pad),
        }
    }

    pub fn new_from(
        message: Arc<AssistantMessage>,
        hide_thinking: bool,
        output_pad: usize,
    ) -> Self {
        Self {
            state: Mutex::new(AssistantState {
                message,
                hide_thinking,
                hidden_label: Arc::new(Mutex::new(None)),
            }),
            cache: Mutex::new(Vec::new()),
            image_rows: Mutex::new(Vec::new()),
            output_pad,
            pad: " ".repeat(output_pad),
        }
    }

    /// Share the app's live hidden-thinking label slot (`ChatContainer`'s),
    /// so `ctx.ui.setHiddenThinkingLabel` relabels this component in place.
    pub fn with_hidden_label(self, label: Arc<Mutex<Option<String>>>) -> Self {
        {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            state.hidden_label = label;
        }
        self
    }

    /// Append body segment `index`'s lines to the canvas: the cached entry's
    /// lines when its key matches, else a fresh render stored for the
    /// following frames. Indexed by render position — the message is
    /// immutable, so a given position always renders the same source
    /// segment; the key guards the width, the visibility posture, the
    /// label, the mermaid snapshot and the theme generation, and the source
    /// text is passed straight to the miss render (never retained).
    fn segment_lines_into(
        &self,
        index: usize,
        query: &SegmentQuery<'_>,
        pad: &str,
        out: &mut Rows<'_>,
    ) {
        if let Some(cached) = self
            .cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(index)
            .and_then(Option::as_ref)
        {
            if cached.key.answers(query) {
                // A hit copies the cached lines into the pooled rows; the
                // cache keeps its own copy reusable for the next frame (the
                // old `Vec`-returning form deep-cloned every line instead).
                // On a steady frame the pool already holds these exact bytes,
                // so `Rows::push` reuses each pooled row without copying.
                for line in &cached.lines {
                    out.push(line);
                }
                return;
            }
        }

        // Miss: render with the cache lock released (the markdown render
        // reads the global theme; the cache mutex is never held across it).
        let key = query.to_key();
        let lines = render_segment_lines(&key, query.text, self.output_pad, pad);
        for line in &lines {
            out.push(line);
        }
        let mut cache = self
            .cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if cache.len() <= index {
            cache.resize_with(index + 1, || None);
        }
        cache[index] = Some(SegmentCache { key, lines });
    }
}

impl Component for AssistantMessageComponent {
    /// The trait's owned-line form: the same rows, built through
    /// [`Component::render_into`] in a fresh pool. The frame renderer drives
    /// the canvas path; this bridge keeps `render` correct for any caller
    /// that asks for lines (tests, alternate drivers).
    fn render(&self, width: usize) -> Vec<String> {
        let mut lines = Vec::new();
        {
            let mut rows = Rows::new(&mut lines);
            self.render_into(width, &mut rows);
            rows.trim();
        }
        lines
    }

    /// Direct-write path: every row is written into the frame's recycled row
    /// buffers, so a cached body segment is copied line by line into pooled
    /// rows instead of being deep-cloned into a per-frame line `Vec` — row
    /// for row byte-identical to [`Component::render`].
    fn render_into(&self, width: usize, out: &mut Rows<'_>) {
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // The per-segment pad is the component's construction-time invariant
        // (see the `pad` field); the notice branches below read it too.
        let pad = self.pad.as_str();
        // One generation for the whole frame: every segment rendered below is
        // keyed on the palette this frame paints with.
        let generation = Theme::generation();
        // Cache-slot cursor: body segments in render order.
        let mut segment = 0usize;

        let has_visible_content = state.message.content.iter().any(|block| match block {
            ContentBlock::Text(text) => !text.text.trim().is_empty(),
            ContentBlock::Thinking(thinking) => !thinking.thinking.trim().is_empty(),
            // An image block always occupies at least the fallback line.
            ContentBlock::Image(_) => true,
            _ => false,
        });
        if has_visible_content {
            out.push("");
        }

        let mut thinking_run = 0usize;
        let mut index = 0usize;
        while index < state.message.content.len() {
            match &state.message.content[index] {
                ContentBlock::Text(text) => {
                    if !text.text.trim().is_empty() {
                        // `outputPad` rides the markdown's own paddingX
                        // (upstream `new Markdown(content.text.trim(),
                        // this.outputPad, 0, …)`, assistant-message.ts:114):
                        // every line comes back padded to the full width, so
                        // it must NOT be prefixed again (a width+1 line wraps
                        // in the terminal and desyncs the diff renderer).
                        //
                        // `markdown.mermaid` rides the process-wide snapshot
                        // the TUI host installs at startup (upstream registers
                        // the mermaid transformer on the assistant path); it
                        // is part of the key, so a late mode change re-renders.
                        let query = SegmentQuery {
                            kind: SegmentKind::Text,
                            text: text.text.trim(),
                            width,
                            hidden: false,
                            label: "",
                            mermaid: super::markdown::global_mermaid_mode(),
                            theme_generation: generation,
                        };
                        self.segment_lines_into(segment, &query, pad, out);
                        segment += 1;
                    }
                    index += 1;
                }
                ContentBlock::Thinking(_) => {
                    // Collect the run of thinking blocks into the
                    // `\n\n`-joined body the markdown below consumes. The
                    // common run is a SINGLE non-empty block — the trimmed
                    // block itself answers the cache key, so a hit frame only
                    // borrows it (the pre-fix form rebuilt the joined copy on
                    // every frame, hit or miss, i.e. re-copied every thinking
                    // body in the transcript per frame); only a multi-block
                    // run materializes the join, exactly the bytes the join
                    // used to produce.
                    let mut joined: Option<String> = None;
                    let mut first_block: Option<&str> = None;
                    while index < state.message.content.len() {
                        if let ContentBlock::Thinking(thinking) = &state.message.content[index] {
                            let trimmed = thinking.thinking.trim();
                            if !trimmed.is_empty() {
                                match (&mut joined, first_block) {
                                    (Some(body), _) => {
                                        body.push_str("\n\n");
                                        body.push_str(trimmed);
                                    }
                                    (None, None) => first_block = Some(trimmed),
                                    (None, Some(first)) => {
                                        let mut body =
                                            String::with_capacity(first.len() + trimmed.len() + 2);
                                        body.push_str(first);
                                        body.push_str("\n\n");
                                        body.push_str(trimmed);
                                        joined = Some(body);
                                    }
                                }
                            }
                            index += 1;
                        } else {
                            break;
                        }
                    }
                    let run = thinking_run;
                    thinking_run += 1;
                    // No non-empty thinking block in the run: nothing to
                    // render (the classic path skipped the run entirely).
                    let Some(blocks) = joined.as_deref().or(first_block) else {
                        let _ = run;
                        continue;
                    };
                    let hidden = state.hide_thinking;
                    // The live label (`setHiddenThinkingLabel`) is BORROWED out
                    // of the shared slot for the key compare: the pre-fix form
                    // cloned it (one `String` per thinking run per frame, plus
                    // a fresh `"Thinking..."` when the slot was empty). The
                    // default is upstream `defaultHiddenThinkingLabel`.
                    let label_guard = state
                        .hidden_label
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    let label = label_guard.as_deref().unwrap_or("Thinking...");
                    // Thinking text: outputPad as paddingX like the text
                    // blocks (upstream `new Markdown(thinkingBlocks…,
                    // this.outputPad, 0, …)`); lines are already full-width
                    // padded, so no second prefix. The default options keep
                    // `markdown.mermaid` off on this path (upstream registers
                    // the transformer for the assistant text path only).
                    let query = SegmentQuery {
                        kind: SegmentKind::Thinking,
                        text: blocks,
                        width,
                        hidden,
                        label,
                        mermaid: MermaidRenderingMode::Off,
                        theme_generation: generation,
                    };
                    self.segment_lines_into(segment, &query, pad, out);
                    segment += 1;
                    let has_after =
                        state.message.content[index..]
                            .iter()
                            .any(|block| match block {
                                ContentBlock::Text(text) => !text.text.trim().is_empty(),
                                ContentBlock::Thinking(thinking) => {
                                    !thinking.thinking.trim().is_empty()
                                }
                                _ => false,
                            });
                    if has_after {
                        out.push("");
                    }
                    let _ = run;
                }
                ContentBlock::Image(image) => {
                    // Inline image block (upstream `components/image.ts`):
                    // Kitty/iTerm2 protocol output when the terminal supports
                    // it, else the upstream `[Image: <mime> <WxH>]` text
                    // line. The reservation width is
                    // `min(width - 2, imageWidthCells-or-60)` like upstream.
                    // The rows are memoized per block ([`AssistantImageRows`]):
                    // an unchanged frame re-pushes them instead of decoding
                    // the payload and re-encoding the protocol lines.
                    let max_width = width.saturating_sub(2).clamp(1, 60);
                    let max_height = max_width.div_ceil(2).max(1);
                    let capabilities_generation = super::terminal_image::capabilities_generation();
                    let mut image_rows = self
                        .image_rows
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    if image_rows.len() <= index {
                        image_rows.resize_with(index + 1, || None);
                    }
                    let hit = image_rows[index].as_ref().is_some_and(|entry| {
                        entry.width == width
                            && entry.capabilities_generation == capabilities_generation
                    });
                    if !hit {
                        image_rows[index] = Some(AssistantImageRows {
                            width,
                            capabilities_generation,
                            rows: super::terminal_image::image_content_lines(
                                &image.data,
                                &image.mime_type,
                                max_width,
                                Some(max_height),
                            ),
                        });
                    }
                    if let Some(entry) = &image_rows[index] {
                        out.push("");
                        for line in &entry.rows {
                            out.push_fmt(format_args!("{pad}{line}"));
                        }
                    }
                    index += 1;
                }
                ContentBlock::ToolCall(_) => {
                    index += 1;
                }
            }
        }

        let has_tool_calls = state
            .message
            .content
            .iter()
            .any(|block| matches!(block, ContentBlock::ToolCall(_)));
        match state.message.stop_reason {
            crate::ai::types::StopReason::Length => {
                out.push("");
                out.push_fmt(format_args!(
                    "{pad}{}",
                    with_theme(|theme| theme.fg(
                        ThemeColor::Error,
                        "Response was truncated before completion."
                    ))
                ));
            }
            crate::ai::types::StopReason::Aborted if !has_tool_calls => {
                let message = state
                    .message
                    .error_message
                    .as_deref()
                    .filter(|m| *m != "Request was aborted")
                    .unwrap_or("Operation aborted");
                out.push("");
                out.push_fmt(format_args!(
                    "{pad}{}",
                    with_theme(|theme| theme.fg(ThemeColor::Error, message))
                ));
            }
            crate::ai::types::StopReason::Error if !has_tool_calls => {
                let message = state
                    .message
                    .error_message
                    .as_deref()
                    .unwrap_or("Unknown error");
                out.push("");
                out.push_fmt(format_args!(
                    "{pad}{}",
                    with_theme(|theme| theme.fg(ThemeColor::Error, &format!("Error: {message}")))
                ));
            }
            _ => {}
        }
    }

    fn invalidate(&self) {
        // The body is immutable once built and the cache key carries every
        // render input, so this is a memory/hygiene reset for the
        // container-driven invalidations (`set_hide_thinking`, label changes,
        // transcript rebuilds) — correctness never depends on it.
        self.cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clear();
    }
}

/// Live thinking block of the in-flight assistant entry: the streaming
/// counterpart of [`AssistantMessageComponent`]'s thinking arm. Upstream
/// creates the assistant component on `message_start` and re-renders it on
/// every `message_update`, so the thinking block is visible — dim
/// `thinkingText` markdown, or the hidden-thinking label — while the provider
/// streams it; pir used to render thinking only once the turn settled (the
/// `ChatEntry::Streaming` accumulator carried text alone), which left the
/// whole thinking phase with no visible content at all.
///
/// The lines are exactly what [`render_segment_lines`] materializes for a
/// visible/hidden thinking run at the same width and padding, so the settled
/// [`AssistantMessageComponent`] that replaces this child at `message_done`
/// renders the identical block. The body drives the same incremental renderer
/// as the live markdown body ([`StreamingRender`]): a delta extends the
/// accumulated text in place and only the mutable tail is re-parsed and
/// re-painted, where the previous memo re-rendered the whole run per delta.
pub struct StreamingThinking {
    /// The accumulated thinking body, trimmed — the settled arm renders the
    /// trimmed run — plus the incremental render state of its final lines.
    body: Mutex<StreamingRender>,
    output_pad: usize,
    /// `settings.hideThinkingBlock`: collapse to the label line.
    hidden: bool,
    /// The app's shared live hidden-thinking label slot.
    label: Arc<Mutex<Option<String>>>,
    /// Memo of the hidden posture's single row (see [`HiddenThinkingRow`]).
    hidden_row: Mutex<Option<HiddenThinkingRow>>,
}

/// Memo of [`StreamingThinking`]'s hidden-posture row: `{pad}{label}` painted
/// italic in `ThinkingText`.
///
/// The key is the row's complete input set. The label text is the live value
/// read out of the shared slot (`ctx.ui.setHiddenThinkingLabel` rewrites it
/// under the same mutex this render reads it through); the padding is
/// `StreamingThinking::output_pad`; and the palette participates through
/// [`Theme::generation`] — `fg`/`italic` are pure reads of the theme installed
/// at that generation. The render width is deliberately NOT part of the key:
/// the hidden row is padded to `output_pad` only, no part of it reads the
/// width. Without the memo the row clones the label and re-styles it
/// (`fg` + `italic` + the painted copy) on every frame.
struct HiddenThinkingRow {
    row: String,
    label: String,
    output_pad: usize,
    theme_generation: u64,
}

impl HiddenThinkingRow {
    fn matches(&self, output_pad: usize, theme_generation: u64, label: &str) -> bool {
        self.output_pad == output_pad
            && self.theme_generation == theme_generation
            && self.label == label
    }
}

impl StreamingThinking {
    /// `text` is the raw accumulated thinking body (the live accumulator's
    /// contents); the trimmed body is what renders.
    pub fn new(
        text: &str,
        output_pad: usize,
        hidden: bool,
        label: Arc<Mutex<Option<String>>>,
    ) -> Self {
        Self {
            body: Mutex::new(StreamingRender::new(text.trim().to_string())),
            output_pad,
            hidden,
            label,
            hidden_row: Mutex::new(None),
        }
    }

    /// Build from an already-shared buffer (the settled-path fixtures): the
    /// trimmed body is copied once into the growing buffer.
    pub fn new_shared(
        text: Arc<str>,
        output_pad: usize,
        hidden: bool,
        label: Arc<Mutex<Option<String>>>,
    ) -> Self {
        Self::new(&text, output_pad, hidden, label)
    }

    /// Replace the accumulated thinking text (a non-append update: the cached
    /// final prefix is dropped).
    pub fn set_text_shared(&self, text: Arc<str>) {
        self.body
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .replace(text.trim().to_string());
    }

    /// Live-stream update (the per-delta path): when `text` extends the
    /// accumulated body only the appended bytes are copied, and the next
    /// render re-parses just the mutable tail.
    pub fn set_text_growing(&self, text: &str) {
        self.body
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .grow(text.trim());
    }
}

impl Component for StreamingThinking {
    /// The trait's owned-line form: the same rows, built through
    /// [`Component::render_into`] in a fresh pool. The frame renderer drives
    /// the canvas path; this bridge keeps `render` correct for callers that
    /// ask for lines (tests, alternate drivers).
    fn render(&self, width: usize) -> Vec<String> {
        let mut lines = Vec::new();
        {
            let mut rows = Rows::new(&mut lines);
            self.render_into(width, &mut rows);
            rows.trim();
        }
        lines
    }

    /// Direct-write path: the visible body goes through the shared incremental
    /// renderer's canvas form (its cached prefix/tail rows are pushed into the
    /// frame's recycled buffers, so an unchanged body allocates no row), and
    /// the hidden posture re-pushes its memoized `{pad}{label}` row. Row for
    /// row byte-identical to [`Component::render`].
    fn render_into(&self, width: usize, out: &mut Rows<'_>) {
        if self.hidden {
            // `settings.hideThinkingBlock`: the single `{pad}{label}` line,
            // italic in `ThinkingText` — the exact line the settled component
            // renders for a hidden run. The row is memoized (see
            // [`HiddenThinkingRow`]): it only moves when the label slot, the
            // output padding or the palette does, so a steady-state frame
            // re-pushes the stored row — copied into the frame's recycled
            // buffer, never moved out of the memo.
            let mut memo = self
                .hidden_row
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let label = self
                .label
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            // The live label (upstream `hiddenThinkingLabel`), the default
            // ("Thinking...", upstream `defaultHiddenThinkingLabel`) otherwise.
            let label_text = label.as_deref().unwrap_or("Thinking...");
            // One palette generation per render, like the tool-result memo: a
            // generation that moved mid-render misses on the next frame.
            let generation = Theme::generation();
            let fresh = !memo
                .as_ref()
                .is_some_and(|entry| entry.matches(self.output_pad, generation, label_text));
            if fresh {
                let painted = with_theme(|theme| {
                    theme.italic(&theme.fg(ThemeColor::ThinkingText, label_text))
                });
                let mut row = String::with_capacity(self.output_pad + painted.len());
                for _ in 0..self.output_pad {
                    row.push(' ');
                }
                row.push_str(&painted);
                *memo = Some(HiddenThinkingRow {
                    row,
                    label: label_text.to_string(),
                    output_pad: self.output_pad,
                    theme_generation: generation,
                });
            }
            if let Some(entry) = memo.as_ref() {
                out.push(&entry.row);
            }
            return;
        }
        // Visible thinking: the trimmed body through the assistant thinking
        // markdown (defaults), every line painted in `ThinkingText` — the same
        // arguments `render_segment_lines` passes for a visible thinking run.
        let painter = |line: &str| with_theme(|theme| theme.fg(ThemeColor::ThinkingText, line));
        let painter: &dyn Fn(&str) -> String = &painter;
        self.body
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .lines_into(
                width,
                self.output_pad,
                0,
                &MarkdownTheme::default(),
                &MarkdownOptions::default(),
                Some(painter),
                out,
            );
    }

    fn invalidate(&self) {
        self.body
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .invalidate();
        *self
            .hidden_row
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
    }
}

/// Materialize one body segment's final lines — the cache-miss path, kept
/// byte-identical to what the component emitted before it cached: text
/// blocks render through the assistant markdown options; thinking runs
/// through the defaults, painted in `ThinkingText`; a hidden run is the
/// single `{pad}{label}` line.
fn render_segment_lines(key: &SegmentKey, text: &str, output_pad: usize, pad: &str) -> Vec<String> {
    match key.kind {
        SegmentKind::Text => render_padded_lines(
            text,
            key.width,
            output_pad,
            0,
            &MarkdownTheme::default(),
            &MarkdownOptions {
                mermaid: key.mermaid,
                ..MarkdownOptions::default()
            },
        ),
        SegmentKind::Thinking if key.hidden => vec![with_theme(|theme| {
            format!(
                "{pad}{}",
                theme.italic(&theme.fg(ThemeColor::ThinkingText, &key.label))
            )
        })],
        SegmentKind::Thinking => render_padded_lines(
            text,
            key.width,
            output_pad,
            0,
            &MarkdownTheme::default(),
            &MarkdownOptions::default(),
        )
        .into_iter()
        .map(|line| with_theme(|theme| theme.fg(ThemeColor::ThinkingText, &line)))
        .collect(),
    }
}

/// Tool-execution phase (upstream `ToolExecutionComponent` background
/// selection: `isPartial` → pending; else `result.isError`, tool-execution.ts:271-275).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolPhase {
    Pending,
    Success,
    Error,
}

// ---------------------------------------------------------------------------
// Call-line formatting — port of upstream `core/tools/render-utils.ts` and
// `core/tools/renderers/{bash,read,edit,write,grep,find,ls}.ts`.
// ---------------------------------------------------------------------------

/// Outcome of upstream `str(value)` (render-utils.ts:25-29): strings pass
/// through, null/undefined become "" and any other JSON type is `null`.
enum ArgStr<'a> {
    Str(&'a str),
    Empty,
    Invalid,
}

/// `args?.a ?? args?.b` (nullish coalescing: null/undefined fall through to
/// the next key).
fn first_str_arg<'a>(args: &'a serde_json::Value, keys: &[&str]) -> ArgStr<'a> {
    for key in keys {
        match args.get(*key) {
            None | Some(serde_json::Value::Null) => continue,
            Some(serde_json::Value::String(value)) => return ArgStr::Str(value),
            Some(_) => return ArgStr::Invalid,
        }
    }
    ArgStr::Empty
}

fn str_arg<'a>(args: &'a serde_json::Value, key: &str) -> ArgStr<'a> {
    first_str_arg(args, &[key])
}

/// The process-level environment the call-line formatters read: the cwd
/// (`getCompactReadClassification` resolves relative read paths against it),
/// the `HOME`/`USERPROFILE` home prefix (`shortenPath`) and `PI_PACKAGE_DIR`
/// (the package-docs tier of the classification).
///
/// Captured once ([`with_call_line_env`]): the process never moves its cwd —
/// `std::env::set_current_dir` appears nowhere in `src/`, the JS runtime's
/// `process.chdir` throws, and the Node shim deliberately omits a `chdir` —
/// and the TUI does not mutate these variables after startup, so the previous
/// per-call `current_dir()`/`var` reads were pure syscall overhead. The
/// generation is part of the [`CallRowsCache`] key and is bumped by
/// [`refresh_call_line_env`] (the hook a future chdir/env mutation must call).
#[derive(Debug)]
struct CallLineEnv {
    /// `current_dir()` at capture time; `"."` when unavailable (the old
    /// per-call fallback).
    cwd: std::path::PathBuf,
    /// `HOME`, then `USERPROFILE` — exactly the old `shorten_path` fallback
    /// (`var`, not `var_os`, so a non-UTF-8 value still falls through).
    home: Option<String>,
    /// `PI_PACKAGE_DIR` at capture time (`config::env::package_dir`).
    package_dir: Option<std::path::PathBuf>,
}

impl CallLineEnv {
    fn capture() -> Self {
        Self {
            cwd: std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from(".")),
            home: std::env::var("HOME")
                .or_else(|_| std::env::var("USERPROFILE"))
                .ok(),
            package_dir: crate::config::env::package_dir(),
        }
    }
}

/// The once-captured environment cell. The `OnceLock` keeps the steady path
/// free of repeated captures; the `RwLock` exists only so
/// [`refresh_call_line_env`] can re-capture in place (call-line formatting is
/// memoized, so the uncontended read costs nothing per frame).
static CALL_LINE_ENV: OnceLock<RwLock<CallLineEnv>> = OnceLock::new();

/// Bumped on every re-capture; every block's call-line cache keys on it.
static CALL_LINE_ENV_GENERATION: AtomicU64 = AtomicU64::new(0);

fn call_line_env_cell() -> &'static RwLock<CallLineEnv> {
    CALL_LINE_ENV.get_or_init(|| RwLock::new(CallLineEnv::capture()))
}

/// Run `f` against the captured environment. `f` receives `&Path`/`&str`
/// borrows, so callers never allocate a `PathBuf`/`String` just to read the
/// cwd or the home prefix.
fn with_call_line_env<T>(f: impl FnOnce(&CallLineEnv) -> T) -> T {
    let env = call_line_env_cell()
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    f(&env)
}

/// The generation of the current environment snapshot (see
/// [`CallRowsCache`]). A steady frame reads one atomic; it never calls into
/// [`with_call_line_env`].
#[doc(hidden)]
pub fn call_line_env_generation() -> u64 {
    CALL_LINE_ENV_GENERATION.load(Ordering::Relaxed)
}

/// Re-capture the environment snapshot (cwd, `HOME`/`USERPROFILE`,
/// `PI_PACKAGE_DIR`) and return the new generation.
///
/// Normal operation never needs this (the process cwd and the relevant
/// variables are fixed at startup); it is the hook a future explicit
/// chdir/env mutation must call — the bumped generation makes every block's
/// cached call text miss on the next frame. Exposed as a test seam so the
/// invalidation path is exercised.
#[doc(hidden)]
pub fn refresh_call_line_env() -> u64 {
    let cell = call_line_env_cell();
    {
        let mut env = cell
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *env = CallLineEnv::capture();
    }
    CALL_LINE_ENV_GENERATION.fetch_add(1, Ordering::Relaxed) + 1
}

/// Replace the home prefix with `~` (render-utils.ts:10-17 `shortenPath`),
/// against the captured [`CallLineEnv`]. Borrows the input when there is no
/// home prefix to rewrite, so a steady-state call line pays neither an
/// allocation nor a `var`/`current_dir` syscall.
fn shorten_path(path: &str) -> Cow<'_, str> {
    with_call_line_env(|env| match env.home.as_deref() {
        Some(home) => match path.strip_prefix(home) {
            Some(rest) => Cow::Owned(format!("~{rest}")),
            None => Cow::Borrowed(path),
        },
        None => Cow::Borrowed(path),
    })
}

/// JS template interpolation of a JSON value (`${value}`): strings raw,
/// numbers/bools bare, `null` literal, everything else compact JSON.
fn js_value_text(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(text) => text.clone(),
        serde_json::Value::Number(number) => number.to_string(),
        serde_json::Value::Bool(flag) => flag.to_string(),
        serde_json::Value::Null => "null".to_string(),
        other => serde_json::to_string(other).unwrap_or_default(),
    }
}

/// `renderToolPath` (render-utils.ts:102-112): invalid → `[invalid arg]`;
/// empty → the `emptyFallback` (or `...` in toolOutput); else accent + `~`.
/// The styling core of [`render_tool_path`], for callers ALREADY holding the
/// theme lock — re-entering `with_theme` from inside a `with_theme` closure
/// self-deadlocks the non-reentrant mutex (the write/edit/read/ls tool-line
/// freeze).
fn render_tool_path_with(theme: &Theme, raw: ArgStr<'_>, empty_fallback: Option<&str>) -> String {
    match raw {
        ArgStr::Invalid => theme.fg(ThemeColor::Error, "[invalid arg]"),
        ArgStr::Empty | ArgStr::Str("") => match empty_fallback {
            Some(fallback) => theme.fg(ThemeColor::Accent, &shorten_path(fallback)),
            None => theme.fg(ThemeColor::ToolOutput, "..."),
        },
        ArgStr::Str(path) => theme.fg(ThemeColor::Accent, &shorten_path(path)),
    }
}

/// `parseLineNumber` (render-utils.ts:32-35): numeric strings count, blank
/// strings and non-finite numbers do not.
fn parse_line_number(value: &serde_json::Value) -> Option<f64> {
    let number = match value {
        serde_json::Value::Number(number) => number.as_f64()?,
        serde_json::Value::String(text) => {
            let text = text.trim();
            if text.is_empty() {
                return None;
            }
            // `Number()` rejects Rust's `inf`/`nan` spellings too; the finite
            // check below drops them (and `Infinity`) exactly like upstream.
            text.parse::<f64>().ok()?
        }
        _ => return None,
    };
    number.is_finite().then_some(number)
}

/// The unstyled `formatReadLineRange` text (render-utils.ts:38-49): shown
/// whenever either `offset` or `limit` is non-null; a lone `limit` starts at
/// line 1, and a malformed range is omitted rather than guessed at.
///
/// Deliberately theme-free: every caller already holds the theme lock, so the
/// styling step is separated out as [`paint_read_line_range`] — the same
/// "`*_with(theme, …)`" shape [`render_tool_path_with`] uses.
fn read_line_range_text(args: &serde_json::Value) -> String {
    let offset = args.get("offset").filter(|value| !value.is_null());
    let limit = args.get("limit").filter(|value| !value.is_null());
    if offset.is_none() && limit.is_none() {
        return String::new();
    }
    let start = match offset {
        Some(offset) => match parse_line_number(offset) {
            Some(start) => start,
            None => return String::new(),
        },
        None => 1.0,
    };
    match limit {
        Some(limit) => {
            let Some(count) = parse_line_number(limit) else {
                return String::new();
            };
            let end = start + count - 1.0;
            // `endLine ? `-${endLine}` : ""` — 0 is falsy in JS.
            if end != 0.0 {
                format!(":{start}-{end}")
            } else {
                format!(":{start}")
            }
        }
        None => format!(":{start}"),
    }
}

/// [`read_line_range_text`] in warning color. Takes the theme by reference:
/// the read call lines format from inside `format_tool_call_line` /
/// `format_compact_read_call`, which already hold the non-reentrant theme
/// mutex — an inner `with_theme` there self-deadlocks the UI render thread
/// (the `read` + `offset`/`limit` transcript freeze this split fixes).
fn paint_read_line_range(theme: &Theme, args: &serde_json::Value) -> String {
    // `return range ? theme.fg("warning", range) : ""` (read.ts:28-31): an
    // absent/malformed range must contribute nothing at all. `Theme::fg` wraps
    // even "" in its SGR pair, so painting unconditionally would splice two
    // no-op escapes into every plain `read` header.
    let range = read_line_range_text(args);
    if range.is_empty() {
        String::new()
    } else {
        theme.fg(ThemeColor::Warning, &range)
    }
}

/// `formatReadLineRange` for callers that do NOT already hold the theme lock
/// (the `tests/suites` seam; the render path uses [`paint_read_line_range`]).
#[doc(hidden)]
pub fn format_read_line_range(args: &serde_json::Value) -> String {
    with_theme(|theme| paint_read_line_range(theme, args))
}

fn replace_tabs(text: &str) -> String {
    text.replace('\t', "   ")
}

/// [`replace_tabs`] without the copy when the text holds no tab — the
/// overwhelming majority of result and diff lines: `String::replace` allocates
/// and copies even when it replaces nothing, so every tab-free output/line paid
/// a full copy on every frame.
fn replace_tabs_cow(text: &str) -> Cow<'_, str> {
    if text.contains('\t') {
        Cow::Owned(replace_tabs(text))
    } else {
        Cow::Borrowed(text)
    }
}

/// `text.replace('\t', "   ").replace('\r', "").split('\n').join("\n    ")`
/// appended to `out`: a tab becomes three spaces, a carriage return vanishes,
/// a newline becomes a newline plus the four-space value indent. The chain
/// built four intermediate `String`s per entry (and the `split`/`join` pair a
/// `Vec` on top) every frame.
fn push_indented(out: &mut String, text: &str) {
    for ch in text.chars() {
        match ch {
            '\t' => out.push_str("   "),
            '\r' => {}
            '\n' => out.push_str("\n    "),
            other => out.push(other),
        }
    }
}

/// Generic tool call header (render-utils.ts:78-96 `formatToolCallWithArgs`):
/// collapsed `key=value` pairs cut to 100 chars; expanded `key: value` lines
/// under the title.
fn format_tool_call_with_args(
    tool_name: &str,
    args: &serde_json::Value,
    expanded: bool,
    theme: &super::theme::Theme,
) -> String {
    let header = theme.fg(ThemeColor::ToolTitle, &theme.bold(tool_name));
    if args.is_null() {
        return header;
    }
    // Borrowed keys (and a single element for the non-object shape): every
    // reader below formats the key straight into the output, so the per-key
    // `String` clone the old `Vec<(String, …)>` carried was a frame-rate copy
    // of the whole argument name set.
    let entries: Vec<(&str, &serde_json::Value)> = match args {
        serde_json::Value::Object(map) => map
            .iter()
            .map(|(key, value)| (key.as_str(), value))
            .collect(),
        other => vec![("args", other)],
    };
    if entries.is_empty() {
        return header;
    }
    if expanded {
        // One buffer for the whole `key: value` block (the previous shape
        // cloned each string value, tab/CR-normalized it into two more
        // `String`s, split/joined into two more, per entry, per frame).
        let mut body = String::new();
        for (index, (key, value)) in entries.iter().enumerate() {
            if index > 0 {
                body.push('\n');
            }
            body.push_str("  ");
            body.push_str(key);
            body.push_str(": ");
            match value {
                serde_json::Value::String(text) => push_indented(&mut body, text),
                other => push_indented(
                    &mut body,
                    &serde_json::to_string_pretty(other).unwrap_or_default(),
                ),
            }
        }
        format!("{header}\n{}", theme.fg(ThemeColor::Muted, &body))
    } else {
        // `key=value` pairs into one buffer: the old shape collected a
        // `Vec<String>` of formatted pairs and `join`ed a second full copy of
        // them, and the 100-char cut then re-copied the head through
        // `format!`.
        let mut pairs = String::with_capacity(128);
        for (index, (key, value)) in entries.iter().enumerate() {
            if index > 0 {
                pairs.push(' ');
            }
            pairs.push_str(key);
            pairs.push('=');
            pairs.push_str(&serde_json::to_string(value).unwrap_or_default());
        }
        let preview = if pairs.chars().count() > 100 {
            let mut head = String::with_capacity(100);
            head.extend(pairs.chars().take(97));
            head.push_str("...");
            head
        } else {
            pairs
        };
        format!("{header} {}", theme.fg(ThemeColor::Muted, &preview))
    }
}

/// Per-tool call-line formatter (upstream `core/tools/renderers/*`). `expanded`
/// mirrors `ToolRenderContext.expanded` (read cards and generic headers
/// change shape in expanded mode).
pub fn format_tool_call_line(tool_name: &str, args: &serde_json::Value, expanded: bool) -> String {
    with_theme(|theme| {
        let title = |name: &str| theme.fg(ThemeColor::ToolTitle, &theme.bold(name));
        let accent = |text: &str| theme.fg(ThemeColor::Accent, text);
        let output = |text: &str| theme.fg(ThemeColor::ToolOutput, text);
        let invalid = || theme.fg(ThemeColor::Error, "[invalid arg]");
        match tool_name {
            // `formatShellCall` (renderers/bash.ts:31-37): `$ cmd` in bold
            // toolTitle, an empty command shows `...`, a non-string shows
            // `[invalid arg]`; a truthy timeout appends ` (timeout Ns)`.
            "bash" | "powershell" => {
                let prompt = if tool_name == "bash" { "$" } else { "PS>" };
                let command = match str_arg(args, "command") {
                    ArgStr::Invalid => invalid(),
                    ArgStr::Empty | ArgStr::Str("") => output("..."),
                    ArgStr::Str(command) => command.to_string(),
                };
                let mut text = theme.fg(
                    ThemeColor::ToolTitle,
                    &theme.bold(&format!("{prompt} {command}")),
                );
                if let Some(timeout) = args.get("timeout").and_then(serde_json::Value::as_f64) {
                    if timeout != 0.0 {
                        text.push_str(
                            &theme.fg(ThemeColor::Muted, &format!(" (timeout {timeout}s)")),
                        );
                    }
                }
                text
            }
            // `formatReadCall` (renderers/read.ts:35-38).
            "read" => format!(
                "{} {}{}",
                title("read"),
                render_tool_path_with(theme, first_str_arg(args, &["file_path", "path"]), None),
                paint_read_line_range(theme, args)
            ),
            // `formatEditCall` (renderers/edit.ts:83-86).
            "edit" => format!(
                "{} {}",
                title("edit"),
                render_tool_path_with(theme, first_str_arg(args, &["file_path", "path"]), None)
            ),
            // Write header (renderers/write.ts:106).
            "write" => format!(
                "{} {}",
                title("write"),
                render_tool_path_with(theme, first_str_arg(args, &["file_path", "path"]), None)
            ),
            // `formatGrepCall` (renderers/grep.ts:17-35).
            "grep" => {
                let pattern = match str_arg(args, "pattern") {
                    ArgStr::Invalid => invalid(),
                    ArgStr::Empty => accent("//"),
                    ArgStr::Str(pattern) => accent(&format!("/{pattern}/")),
                };
                let path = match str_arg(args, "path") {
                    ArgStr::Invalid => Cow::Owned(invalid()),
                    ArgStr::Empty | ArgStr::Str("") => shorten_path("."),
                    ArgStr::Str(path) => shorten_path(path),
                };
                let mut text = format!(
                    "{} {} {}",
                    title("grep"),
                    pattern,
                    output(&format!(" in {path}"))
                );
                if let ArgStr::Str(glob) = str_arg(args, "glob") {
                    if !glob.is_empty() {
                        text.push_str(&output(&format!(" ({glob})")));
                    }
                }
                if let Some(limit) = args.get("limit") {
                    text.push_str(&output(&format!(" limit {}", js_value_text(limit))));
                }
                text
            }
            // `formatFindCall` (renderers/find.ts:17-32).
            "find" => {
                let pattern = match str_arg(args, "pattern") {
                    ArgStr::Invalid => invalid(),
                    ArgStr::Empty | ArgStr::Str("") => accent(""),
                    ArgStr::Str(pattern) => accent(pattern),
                };
                let path = match str_arg(args, "path") {
                    ArgStr::Invalid => Cow::Owned(invalid()),
                    ArgStr::Empty | ArgStr::Str("") => shorten_path("."),
                    ArgStr::Str(path) => shorten_path(path),
                };
                let mut text = format!(
                    "{} {} {}",
                    title("find"),
                    pattern,
                    output(&format!(" in {path}"))
                );
                if let Some(limit) = args.get("limit") {
                    text.push_str(&output(&format!(" (limit {})", js_value_text(limit))));
                }
                text
            }
            // `formatLsCall` (renderers/ls.ts:17-25) — empty path falls back to ".".
            "ls" => {
                let mut text = format!(
                    "{} {}",
                    title("ls"),
                    render_tool_path_with(theme, str_arg(args, "path"), Some("."))
                );
                if let Some(limit) = args.get("limit") {
                    text.push_str(&output(&format!(" (limit {})", js_value_text(limit))));
                }
                text
            }
            // `codemode` (extensions/codemode/renderer.ts:71-96): title row,
            // then the script as JavaScript (`highlightCode(..., "javascript")`,
            // tabs replaced, CRs removed, trailing whitespace trimmed),
            // collapsed to 10 visual lines with the expand hint.
            "codemode" => {
                let mut text = title("codemode");
                match str_arg(args, "code") {
                    ArgStr::Invalid => {
                        text.push(' ');
                        text.push_str(&invalid());
                    }
                    ArgStr::Empty | ArgStr::Str("") => {}
                    ArgStr::Str(code) => {
                        let stripped = code.replace('\r', "");
                        let normalized = replace_tabs(stripped.trim_end());
                        let highlighted = syntax::highlight_lines_with_theme(
                            theme,
                            &normalized,
                            Some("javascript"),
                            ThemeColor::ToolOutput,
                        );
                        let max = if expanded {
                            highlighted.len()
                        } else {
                            CODEMODE_PREVIEW_LINES
                        };
                        let kept = &highlighted[..max.min(highlighted.len())];
                        if !kept.is_empty() {
                            text.push('\n');
                            text.push_str(&kept.join("\n"));
                            let remaining = highlighted.len() - kept.len();
                            if remaining > 0 {
                                text.push('\n');
                                text.push_str(&more_lines_hint(theme, remaining));
                            }
                        }
                    }
                }
                text
            }
            // Tools without a builtin renderer: generic header formatter.
            other => format_tool_call_with_args(other, args, expanded, theme),
        }
    })
}

// ---------------------------------------------------------------------------
// Per-tool result renderers — port of upstream `core/tools/renderers/*.ts`.
//
// The TUI entry carries the call args, the flattened result TEXT and the
// structured `details` (`ChatEntry::ToolExecution.details` mirrors upstream
// `result.details`), so the details-derived presentation — truncation
// warnings, `fullOutputPath`, the edit diff and its live preview — renders
// like upstream. The read result's expanded body and the write call preview
// additionally go through the hand-written `syntax` highlighter
// (`highlightCode`/`getLanguageFromPath`); unknown paths keep `toolOutput`,
// error output and bash output stay plain, and edit diffs keep their own
// added/removed/context colors.
// ---------------------------------------------------------------------------

/// Collapsed read preview (renderers/read.ts:130).
const READ_PREVIEW_LINES: usize = 10;
/// Collapsed write content preview (renderers/write.ts:117).
const WRITE_PREVIEW_LINES: usize = 10;
/// Collapsed grep preview (renderers/grep.ts:49).
const GREP_PREVIEW_LINES: usize = 15;
/// Collapsed find preview (renderers/find.ts:46).
const FIND_PREVIEW_LINES: usize = 20;
/// Collapsed ls preview (renderers/ls.ts:39).
const LS_PREVIEW_LINES: usize = 20;
/// Collapsed user-bash preview (components/bash-execution.ts:19).
const BASH_EXECUTION_PREVIEW_LINES: usize = 20;
/// Collapsed codemode script preview (extensions/codemode/renderer.ts:22).
const CODEMODE_PREVIEW_LINES: usize = 10;

/// `normalizeDisplayText` (render-utils.ts:35): strip carriage returns.
fn normalize_display_text(text: &str) -> String {
    text.replace('\r', "")
}

/// `\n` + the kept lines (highlighted when a language resolves, else in
/// `toolOutput`) + the muted `... (N more lines, ctrl+o to expand)` tail
/// (`formatReadResult` read.ts:131-136; grep.ts:50-55; find.ts:47-52;
/// ls.ts:40-45).
fn preview_text(
    lines: &[&str],
    max_lines: usize,
    expanded: bool,
    leading_newline: bool,
    language: Option<&str>,
) -> String {
    let max = if expanded { lines.len() } else { max_lines };
    let kept = &lines[..max.min(lines.len())];
    let remaining = lines.len() - kept.len();
    let mut body = String::new();
    if leading_newline {
        body.push('\n');
    }
    if language.is_some() {
        // Only the displayed lines are highlighted; the scanner runs over
        // exactly that slice so state (block comments, triple strings) starts
        // at the first visible line.
        let mut source = String::new();
        for (index, line) in kept.iter().enumerate() {
            if index > 0 {
                source.push('\n');
            }
            source.push_str(line);
        }
        let highlighted = with_theme(|theme| {
            syntax::highlight_lines_with_theme(theme, &source, language, ThemeColor::ToolOutput)
        });
        body.push_str(&highlighted.join("\n"));
    } else {
        // Styled straight into the body: the old `map(...).collect::<Vec<_>>()`
        // built a `Vec<String>` of styled lines and `join`ed a second full copy
        // of them, on every frame of every read/grep/find/ls result.
        for (index, line) in kept.iter().enumerate() {
            if index > 0 {
                body.push('\n');
            }
            body.push_str(&with_theme(|theme| theme.fg(ThemeColor::ToolOutput, line)));
        }
    }
    if remaining > 0 {
        body.push_str(&with_theme(|theme| {
            format!(
                "{} {} {}",
                theme.fg(
                    ThemeColor::Muted,
                    &format!("\n... ({remaining} more lines,")
                ),
                expand_key_hint(theme, "to expand"),
                theme.fg(ThemeColor::Muted, ")")
            )
        }));
    }
    body
}

/// The read result's highlight language (read.ts:124): only a non-error
/// result with a known path extension resolves one.
fn read_result_language(args: &serde_json::Value) -> Option<&'static str> {
    match first_str_arg(args, &["file_path", "path"]) {
        ArgStr::Str(path) => syntax::language_from_path(path),
        _ => None,
    }
}

/// `formatReadResult` (renderers/read.ts:112-148): collapsed success renders
/// NOTHING (only the call line); errors preview up to 10 lines; tabs replaced;
/// trailing blank lines trimmed. A truncated result appends the warning line
/// from `details.truncation` (read.ts:138-147). `language` is the resolved
/// `getLanguageFromPath` id of the read file — upstream highlights only a
/// non-error result and only when the path is known (read.ts:124-125).
fn read_result_body(
    output: &str,
    expanded: bool,
    is_error: bool,
    details: Option<&serde_json::Value>,
    language: Option<&str>,
) -> Option<String> {
    if !expanded && !is_error {
        return None;
    }
    let replaced = replace_tabs_cow(output);
    let mut lines: Vec<&str> = replaced.split('\n').collect();
    // `trimTrailingEmptyLines` (read.ts:39-45).
    while lines.last().is_some_and(|line| line.is_empty()) {
        lines.pop();
    }
    if lines.is_empty() {
        return None;
    }
    let mut body = preview_text(&lines, READ_PREVIEW_LINES, expanded, true, language);
    if let Some(warning) = read_truncation_warning(details) {
        body.push('\n');
        body.push_str(&warning);
    }
    Some(body)
}

/// `formatGrepResult` (renderers/grep.ts:36-69) / `formatFindResult`
/// (find.ts:33-64) / `formatLsResult` (ls.ts:26-57): trimmed output, plain
/// `toolOutput` lines, tool-specific collapsed caps (15 / 20 / 20), and the
/// `[Truncated: …]` warning line from the limited-result details.
fn trimmed_result_body(
    output: &str,
    max_lines: usize,
    expanded: bool,
    kind: LimitedResultKind,
    details: Option<&serde_json::Value>,
) -> Option<String> {
    let trimmed = output.trim();
    let mut body = if trimmed.is_empty() {
        String::new()
    } else {
        let replaced = replace_tabs_cow(trimmed);
        let lines: Vec<&str> = replaced.split('\n').collect();
        preview_text(&lines, max_lines, expanded, true, None)
    };
    if let Some(warning) = limited_result_warning(kind, details) {
        body.push('\n');
        body.push_str(&warning);
    }
    if body.is_empty() {
        return None;
    }
    Some(body)
}

/// `formatWriteResult` (renderers/write.ts:128-143): the FILE CONTENT is
/// rendered by the call block (`formatWriteCall`, write.ts:96-127, see
/// [`write_call_text`]); the result only shows error text in the error color,
/// nothing on success.
fn write_result_body(output: &str, is_error: bool) -> Option<String> {
    if !is_error || output.is_empty() {
        return None;
    }
    Some(format!(
        "\n{}",
        with_theme(|theme| theme.fg(ThemeColor::Error, output))
    ))
}

/// Memoized scan of a `write` block's content ([`write_call_text`]), keyed by
/// the args `Arc` (the `Value` behind it is never mutated in place, so pointer
/// identity covers the whole content).
///
/// The collapsed preview only needs the first [`WRITE_PREVIEW_LINES`] lines
/// plus the trimmed total, yet the old shape normalized and split the WHOLE
/// file content on every render. The memo retains the CR/tab decision and the
/// head, so a re-render of unchanged args (a palette swap or an expansion
/// toggle) no longer scans the full content; a steady frame does not call this
/// path at all (the call rows are cached).
#[derive(Debug)]
struct WritePreviewCache {
    args: Arc<serde_json::Value>,
    /// `content.contains('\r') || content.contains('\t')` at capture time.
    needs_normalization: bool,
    /// The first `head_lines` normalized lines, trailing blank lines already
    /// excluded through `total`, joined with `'\n'`.
    head: String,
    /// Number of lines in `head` (`head.split('\n').take(head_lines)`).
    head_lines: usize,
    /// Total normalized lines after trimming trailing blank lines.
    total: usize,
}

impl WritePreviewCache {
    fn capture(args: &Arc<serde_json::Value>) -> Self {
        let (needs_normalization, head, head_lines, total) = match str_arg(args, "content") {
            ArgStr::Str(content) if !content.is_empty() => {
                let needs_normalization = content.contains('\r') || content.contains('\t');
                let normalized: Cow<'_, str> = if needs_normalization {
                    Cow::Owned(replace_tabs(&normalize_display_text(content)))
                } else {
                    Cow::Borrowed(content)
                };
                // `split('\n')` semantics (a trailing newline yields a final
                // empty element) with the trailing empties trimmed exactly
                // like the uncached formatter.
                let mut first: Vec<&str> = Vec::with_capacity(WRITE_PREVIEW_LINES);
                let mut count = 0usize;
                let mut last_non_empty = 0usize;
                for line in normalized.split('\n') {
                    count += 1;
                    if !line.is_empty() {
                        last_non_empty = count;
                    }
                    if first.len() < WRITE_PREVIEW_LINES {
                        first.push(line);
                    }
                }
                let head_lines = first.len().min(last_non_empty);
                let mut head = String::new();
                for (index, line) in first[..head_lines].iter().enumerate() {
                    if index > 0 {
                        head.push('\n');
                    }
                    head.push_str(line);
                }
                (needs_normalization, head, head_lines, last_non_empty)
            }
            _ => (false, String::new(), 0, 0),
        };
        Self {
            args: Arc::clone(args),
            needs_normalization,
            head,
            head_lines,
            total,
        }
    }
}

/// `formatWriteCall` (renderers/write.ts:96-127): header + `\n\n` + the file
/// content (collapsed cap 10 lines, `... (N more lines, T total, ctrl+o to
/// expand)`), tabs replaced, `\r` removed, trailing blank lines trimmed; an
/// invalid non-string content arg shows the error note. A recognized file
/// path highlights the displayed lines (write.ts:111-123); an unknown path
/// keeps `toolOutput`.
///
/// The content scan is memoized per args `Arc` ([`WritePreviewCache`]): the
/// collapsed preview styles the cached head instead of normalizing and
/// splitting the whole content again. Expanded mode shows every line by
/// definition, so it re-normalizes (the memo deliberately does not retain a
/// second copy of the file).
fn write_call_text(
    args: &Arc<serde_json::Value>,
    expanded: bool,
    preview_cache: &mut Option<WritePreviewCache>,
) -> String {
    let mut text = format_tool_call_line("write", args, expanded);
    // `rawPath ? getLanguageFromPath(rawPath) : undefined` (write.ts:111).
    let language = match first_str_arg(args, &["file_path", "path"]) {
        ArgStr::Str(path) => syntax::language_from_path(path),
        _ => None,
    };
    match str_arg(args, "content") {
        ArgStr::Invalid => {
            text.push_str(&format!(
                "\n\n{}",
                with_theme(
                    |theme| theme.fg(ThemeColor::Error, "[invalid content arg - expected string]")
                )
            ));
        }
        ArgStr::Str(content) if !content.is_empty() => {
            if !preview_cache
                .as_ref()
                .is_some_and(|cache| Arc::ptr_eq(&cache.args, args))
            {
                *preview_cache = Some(WritePreviewCache::capture(args));
            }
            // The branch above always fills the cache for a non-empty string
            // content; the early return keeps this fallible shape explicit
            // instead of asserting on it.
            let Some(preview) = preview_cache.as_ref() else {
                return text;
            };
            let mut styled = String::new();
            match language {
                Some(language) if expanded => {
                    let normalized: Cow<'_, str> = if preview.needs_normalization {
                        Cow::Owned(replace_tabs(&normalize_display_text(content)))
                    } else {
                        Cow::Borrowed(content)
                    };
                    // `lines[..total]` — every kept line, same as the
                    // uncached path (`max == total` when expanded).
                    let mut source = String::new();
                    if preview.total > 0 {
                        for (index, line) in normalized.split('\n').take(preview.total).enumerate()
                        {
                            if index > 0 {
                                source.push('\n');
                            }
                            source.push_str(line);
                        }
                        let highlighted = with_theme(|theme| {
                            syntax::highlight_lines_with_theme(
                                theme,
                                &source,
                                Some(language),
                                ThemeColor::ToolOutput,
                            )
                        });
                        styled.push_str(&highlighted.join("\n"));
                    }
                }
                Some(language) => {
                    // Only the displayed head is highlighted (the scanner
                    // starts at line 1, so block state is still correct).
                    if preview.head_lines > 0 {
                        let highlighted = with_theme(|theme| {
                            syntax::highlight_lines_with_theme(
                                theme,
                                &preview.head,
                                Some(language),
                                ThemeColor::ToolOutput,
                            )
                        });
                        styled.push_str(&highlighted.join("\n"));
                    }
                }
                None => {
                    // One pass into a single buffer (the old `collect` + `join`
                    // pair allocated a `Vec<String>` and a second copy of the
                    // kept lines on every frame).
                    if expanded {
                        let normalized: Cow<'_, str> = if preview.needs_normalization {
                            Cow::Owned(replace_tabs(&normalize_display_text(content)))
                        } else {
                            Cow::Borrowed(content)
                        };
                        for (index, line) in normalized.split('\n').take(preview.total).enumerate()
                        {
                            if index > 0 {
                                styled.push('\n');
                            }
                            styled.push_str(&with_theme(|theme| {
                                theme.fg(ThemeColor::ToolOutput, line)
                            }));
                        }
                    } else {
                        for (index, line) in preview
                            .head
                            .split('\n')
                            .take(preview.head_lines)
                            .enumerate()
                        {
                            if index > 0 {
                                styled.push('\n');
                            }
                            styled.push_str(&with_theme(|theme| {
                                theme.fg(ThemeColor::ToolOutput, line)
                            }));
                        }
                    }
                }
            }
            text.push_str(&format!("\n\n{styled}"));
            let remaining = if expanded {
                0
            } else {
                preview.total - preview.head_lines
            };
            if remaining > 0 {
                let total = preview.total;
                text.push_str(&with_theme(|theme| {
                    format!(
                        "{} {} {}",
                        theme.fg(
                            ThemeColor::Muted,
                            &format!("\n... ({remaining} more lines, {total} total,")
                        ),
                        expand_key_hint(theme, "to expand"),
                        theme.fg(ThemeColor::Muted, ")")
                    )
                }));
            }
        }
        _ => {}
    }
    text
}

/// `getRenderablePreviewInput` (renderers/edit.ts:59-82): `edits[]` string
/// pairs win; the legacy single `oldText`/`newText` pair is tolerated.
/// Upstream `getRenderablePreviewInput` (renderers/edit.ts:64-85) plus the
/// `argsKey` (`JSON.stringify({path, edits})`): the edit preview input the
/// scheduler computes and the renderer validates against.
#[derive(Debug, Clone, PartialEq)]
pub struct EditPreviewInput {
    pub path: String,
    /// `(oldText, newText, replaceAll)` in arg order.
    pub edits: Vec<(String, String, bool)>,
    /// Identity of the parsed input — a preview whose key no longer matches
    /// the block's current args is stale (upstream `previewArgsKey`).
    pub args_key: String,
}

/// `typeof args.path === "string" ? args.path : typeof args.file_path ===/// "string" ? args.file_path : null`, then a truthy check (upstream).
pub fn edit_preview_input(args: &serde_json::Value) -> Option<EditPreviewInput> {
    let path = match args.get("path") {
        Some(serde_json::Value::String(path)) => path.as_str(),
        _ => match args.get("file_path") {
            Some(serde_json::Value::String(path)) => path.as_str(),
            _ => return None,
        },
    };
    if path.is_empty() {
        return None;
    }
    let mut edits: Vec<(String, String, bool)> = Vec::new();
    if let Some(array) = args.get("edits").and_then(serde_json::Value::as_array) {
        if !array.is_empty() {
            let valid = array.iter().all(|entry| {
                matches!(entry.get("oldText"), Some(serde_json::Value::String(_)))
                    && matches!(entry.get("newText"), Some(serde_json::Value::String(_)))
            });
            if valid {
                for entry in array {
                    let old = entry.get("oldText").and_then(serde_json::Value::as_str);
                    let new = entry.get("newText").and_then(serde_json::Value::as_str);
                    let replace_all = entry
                        .get("replaceAll")
                        .and_then(serde_json::Value::as_bool)
                        .unwrap_or(false);
                    if let (Some(old), Some(new)) = (old, new) {
                        edits.push((old.to_string(), new.to_string(), replace_all));
                    }
                }
            }
        }
    }
    if edits.is_empty() {
        match (
            args.get("oldText").and_then(serde_json::Value::as_str),
            args.get("newText").and_then(serde_json::Value::as_str),
        ) {
            (Some(old), Some(new)) => edits.push((old.to_string(), new.to_string(), false)),
            _ => return None,
        }
    }
    let args_key = serde_json::json!({
        "path": path,
        "edits": edits
            .iter()
            .map(|(old, new, replace_all)| serde_json::json!({
                "oldText": old,
                "newText": new,
                "replaceAll": replace_all,
            }))
            .collect::<Vec<_>>(),
    })
    .to_string();
    Some(EditPreviewInput {
        path: path.to_string(),
        edits,
        args_key,
    })
}

fn renderable_edit_pairs(args: &serde_json::Value) -> Option<Vec<(String, String)>> {
    // Upstream requires a truthy `path` (preferring `path` over `file_path`).
    let has_path = matches!(str_arg(args, "path"), ArgStr::Str(value) if !value.is_empty())
        || matches!(str_arg(args, "file_path"), ArgStr::Str(value) if !value.is_empty());
    if !has_path {
        return None;
    }
    if let Some(edits) = args.get("edits").and_then(serde_json::Value::as_array) {
        if !edits.is_empty() {
            let pairs: Option<Vec<(String, String)>> = edits
                .iter()
                .map(|edit| {
                    let old = edit.get("oldText")?.as_str()?;
                    let new = edit.get("newText")?.as_str()?;
                    Some((old.to_string(), new.to_string()))
                })
                .collect();
            if let Some(pairs) = pairs {
                return Some(pairs);
            }
        }
    }
    match (
        args.get("oldText").and_then(serde_json::Value::as_str),
        args.get("newText").and_then(serde_json::Value::as_str),
    ) {
        (Some(old), Some(new)) => Some(vec![(old.to_string(), new.to_string())]),
        _ => None,
    }
}

/// Upper bound on the intra-line LCS table (`old_tokens * new_tokens`). The
/// diff runs in the render path over model/tool-controlled `oldText`/`newText`,
/// so an unbounded table would let one long single-line edit allocate hundreds
/// of MB (or abort the TUI) per edit pair. At 4 bytes per cell (`u32`) the
/// table peaks at ~1.3 MB; real code lines sit far below this.
const INTRA_LINE_DIFF_MAX_CELLS: usize = 250_000;

/// Upper bound on `old_tokens + new_tokens`. This also bounds the token span
/// vectors (~16 bytes each) and covers the `n == 0 || m == 0` case, where
/// `n * m == 0` would otherwise leave the token count unbounded. 65_536
/// tokens cap the vectors at ~1 MB.
const INTRA_LINE_DIFF_MAX_TOKENS: usize = 65_536;

/// Word-level inverse highlighting (`renderIntraLineDiff`, components/diff.ts:26-66):
/// a whitespace/non-whitespace token LCS approximating `Diff.diffWords`;
/// changed runs render inverse-video (chalk `inverse` = SGR 7/27), with the
/// leading whitespace of the first changed run left plain.
///
/// Above [`INTRA_LINE_DIFF_MAX_CELLS`] / [`INTRA_LINE_DIFF_MAX_TOKENS`] the
/// pair falls back to the plain whole-line rendering (the same shape the
/// multi-line branch emits): no LCS and no inverse runs, just the replaced
/// text for each side. Small/medium inputs keep the exact previous output.
fn render_intra_line_diff(old_content: &str, new_content: &str) -> (String, String) {
    fn token_count(text: &str) -> usize {
        let mut count = 0usize;
        let mut current_ws: Option<bool> = None;
        for ch in text.chars() {
            let ws = ch.is_whitespace();
            match current_ws {
                Some(flag) if flag == ws => {}
                Some(_) => {
                    count += 1;
                    current_ws = Some(ws);
                }
                None => current_ws = Some(ws),
            }
        }
        if current_ws.is_some() {
            count += 1;
        }
        count
    }
    /// Token spans borrow the (tab-expanded) input; unlike the old
    /// `Vec<String>` this costs one allocation and no per-token copies.
    fn tokens(text: &str, count: usize) -> Vec<&str> {
        let mut out: Vec<&str> = Vec::with_capacity(count);
        let mut start = 0usize;
        let mut current_ws: Option<bool> = None;
        for (idx, ch) in text.char_indices() {
            let ws = ch.is_whitespace();
            match current_ws {
                Some(flag) if flag == ws => {}
                Some(_) => {
                    out.push(&text[start..idx]);
                    start = idx;
                    current_ws = Some(ws);
                }
                None => current_ws = Some(ws),
            }
        }
        if current_ws.is_some() {
            out.push(&text[start..]);
        }
        out
    }
    fn push_inverse(line: &mut String, value: &str) {
        line.push_str("\u{1b}[7m");
        line.push_str(value);
        line.push_str("\u{1b}[27m");
    }
    fn push_changed(line: &mut String, value: &str, first: &mut bool) {
        if *first {
            let lead_len = value.len() - value.trim_start().len();
            line.push_str(&value[..lead_len]);
            let rest = &value[lead_len..];
            if !rest.is_empty() {
                push_inverse(line, rest);
            }
            *first = false;
        } else if !value.is_empty() {
            push_inverse(line, value);
        }
    }

    let old_content = replace_tabs_cow(old_content);
    let new_content = replace_tabs_cow(new_content);
    if old_content == new_content {
        return (old_content.into_owned(), new_content.into_owned());
    }
    let n = token_count(&old_content);
    let m = token_count(&new_content);
    if n.saturating_mul(m) > INTRA_LINE_DIFF_MAX_CELLS
        || n.saturating_add(m) > INTRA_LINE_DIFF_MAX_TOKENS
    {
        return (old_content.into_owned(), new_content.into_owned());
    }
    let old_tokens = tokens(&old_content, n);
    let new_tokens = tokens(&new_content, m);
    // One flat `u32` table instead of `n + 1` `usize` rows: an LCS length is
    // bounded by `min(n, m) <= INTRA_LINE_DIFF_MAX_TOKENS` and the cell budget
    // above keeps this allocation at ~1.3 MB.
    let width = m + 1;
    let mut dp = vec![0u32; (n + 1) * width];
    for i in (0..n).rev() {
        let row = i * width;
        let next_row = row + width;
        for j in (0..m).rev() {
            dp[row + j] = if old_tokens[i] == new_tokens[j] {
                dp[next_row + j + 1] + 1
            } else {
                dp[next_row + j].max(dp[row + j + 1])
            };
        }
    }
    let mut removed_line = String::new();
    let mut added_line = String::new();
    let mut first_removed = true;
    let mut first_added = true;
    let (mut i, mut j) = (0usize, 0usize);
    while i < n || j < m {
        if i < n && j < m && old_tokens[i] == new_tokens[j] {
            removed_line.push_str(old_tokens[i]);
            added_line.push_str(old_tokens[i]);
            i += 1;
            j += 1;
        } else if j >= m || (i < n && dp[(i + 1) * width + j] >= dp[i * width + j + 1]) {
            push_changed(&mut removed_line, old_tokens[i], &mut first_removed);
            i += 1;
        } else {
            push_changed(&mut added_line, new_tokens[j], &mut first_added);
            j += 1;
        }
    }
    (removed_line, added_line)
}

/// `formatEditResult` (renderers/edit.ts:87-114) + `renderDiff`
/// (components/diff.ts:79-147): errors render the raw error text; success
/// renders a DIFF view of the change in `toolDiffRemoved`/`toolDiffAdded`.
/// Upstream's diff comes from `details.diff` (a file-accurate, line-numbered
/// diff); the TUI entry carries no `details`, so the diff view is rebuilt
/// from the call's own `edits[]`: removed block (`-`), then added block
/// (`+`), with intra-line inverse on single-line replacements
/// (diff.ts:113-134). Line-number columns and context lines are omitted
/// (gap); a large `edits[]` may therefore show more text than upstream's
/// context-clamped diff.
fn edit_result_body(args: &serde_json::Value, output: &str, is_error: bool) -> Option<String> {
    if is_error {
        if output.is_empty() {
            return None;
        }
        return Some(format!(
            "\n{}",
            with_theme(|theme| theme.fg(ThemeColor::Error, output))
        ));
    }
    let edits = renderable_edit_pairs(args)?;
    let mut lines: Vec<String> = Vec::new();
    for (old_text, new_text) in edits {
        let old_lines: Vec<&str> = old_text.split('\n').collect();
        let new_lines: Vec<&str> = new_text.split('\n').collect();
        // diff.ts:115-134: intra-line diffing only for exactly one removed +
        // one added line; otherwise all removed lines first, then all added.
        if old_lines.len() == 1 && new_lines.len() == 1 {
            let (removed, added) = render_intra_line_diff(old_lines[0], new_lines[0]);
            lines.push(with_theme(|theme| {
                theme.fg(ThemeColor::ToolDiffRemoved, &format!("-{removed}"))
            }));
            lines.push(with_theme(|theme| {
                theme.fg(ThemeColor::ToolDiffAdded, &format!("+{added}"))
            }));
        } else {
            for line in &old_lines {
                lines.push(with_theme(|theme| {
                    theme.fg(
                        ThemeColor::ToolDiffRemoved,
                        &format!("-{}", replace_tabs_cow(line)),
                    )
                }));
            }
            for line in &new_lines {
                lines.push(with_theme(|theme| {
                    theme.fg(
                        ThemeColor::ToolDiffAdded,
                        &format!("+{}", replace_tabs_cow(line)),
                    )
                }));
            }
        }
    }
    if lines.is_empty() {
        return None;
    }
    Some(format!("\n{}", lines.join("\n")))
}

// --- Diff rendering (components/diff.ts) -----------------------------------

/// A parsed display-diff line. `#[doc(hidden)] pub` (with public fields):
/// `tests/suites/tui_messages_units.rs` asserts the parsed shape directly,
/// and there is no other surface that keeps those assertions unweakened.
#[doc(hidden)]
pub struct ParsedDiffLine {
    pub prefix: char,
    pub line_num: String,
    pub content: String,
}

/// `parseDiffLine` (components/diff.ts:6-13), regex `^([+-\s])(\s*\d*)\s(.*)$`
/// with regex backtracking mirrored: the `...` skip markers parse with an
/// empty line number. `#[doc(hidden)] pub` for the tests/ suite (see
/// [`ParsedDiffLine`]).
#[doc(hidden)]
pub fn parse_diff_line(line: &str) -> Option<ParsedDiffLine> {
    let mut chars = line.chars();
    let prefix = chars.next()?;
    if !matches!(prefix, '+' | '-' | ' ') {
        return None;
    }
    let rest = &line[prefix.len_utf8()..];
    let ws = rest.len() - rest.trim_start_matches(' ').len();
    let digits = rest[ws..]
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .count();
    let candidate = ws + digits;
    if let Some(sep) = rest[candidate..].chars().next() {
        if sep.is_whitespace() {
            return Some(ParsedDiffLine {
                prefix,
                line_num: rest[..candidate].to_string(),
                content: rest[candidate + sep.len_utf8()..].to_string(),
            });
        }
    }
    // Backtrack to an empty `(\s*\d*)`: the separator follows immediately.
    let sep = rest.chars().next()?;
    if !sep.is_whitespace() {
        return None;
    }
    Some(ParsedDiffLine {
        prefix,
        line_num: String::new(),
        content: rest[sep.len_utf8()..].to_string(),
    })
}

/// `renderDiff` (components/diff.ts:79-147): re-render a display diff
/// (` [n] line` / `-[n] line` / `+[n] line`) with `toolDiffContext` for
/// context lines, `toolDiffRemoved`/`toolDiffAdded` for changes, and the
/// intra-line inverse highlight for a single removed + added pair. An
/// unparseable line keeps the context color verbatim.
pub fn render_diff_text(diff_text: &str) -> String {
    let lines: Vec<&str> = diff_text.split('\n').collect();
    let mut out: Vec<String> = Vec::new();
    let mut i = 0usize;
    while i < lines.len() {
        let Some(parsed) = parse_diff_line(lines[i]) else {
            out.push(with_theme(|theme| {
                theme.fg(ThemeColor::ToolDiffContext, lines[i])
            }));
            i += 1;
            continue;
        };
        if parsed.prefix == '-' {
            // Collect consecutive removed lines, then consecutive added ones.
            let mut removed: Vec<(String, String)> = Vec::new();
            while i < lines.len() {
                match parse_diff_line(lines[i]) {
                    Some(p) if p.prefix == '-' => {
                        removed.push((p.line_num, p.content));
                        i += 1;
                    }
                    _ => break,
                }
            }
            let mut added: Vec<(String, String)> = Vec::new();
            while i < lines.len() {
                match parse_diff_line(lines[i]) {
                    Some(p) if p.prefix == '+' => {
                        added.push((p.line_num, p.content));
                        i += 1;
                    }
                    _ => break,
                }
            }
            if removed.len() == 1 && added.len() == 1 {
                let (removed_line, added_line) = render_intra_line_diff(
                    &replace_tabs_cow(&removed[0].1),
                    &replace_tabs_cow(&added[0].1),
                );
                out.push(with_theme(|theme| {
                    theme.fg(
                        ThemeColor::ToolDiffRemoved,
                        &format!("-{} {removed_line}", removed[0].0),
                    )
                }));
                out.push(with_theme(|theme| {
                    theme.fg(
                        ThemeColor::ToolDiffAdded,
                        &format!("+{} {added_line}", added[0].0),
                    )
                }));
            } else {
                for (num, content) in &removed {
                    out.push(with_theme(|theme| {
                        theme.fg(
                            ThemeColor::ToolDiffRemoved,
                            &format!("-{num} {}", replace_tabs_cow(content)),
                        )
                    }));
                }
                for (num, content) in &added {
                    out.push(with_theme(|theme| {
                        theme.fg(
                            ThemeColor::ToolDiffAdded,
                            &format!("+{num} {}", replace_tabs_cow(content)),
                        )
                    }));
                }
            }
        } else if parsed.prefix == '+' {
            out.push(with_theme(|theme| {
                theme.fg(
                    ThemeColor::ToolDiffAdded,
                    &format!("+{} {}", parsed.line_num, replace_tabs_cow(&parsed.content)),
                )
            }));
            i += 1;
        } else {
            out.push(with_theme(|theme| {
                theme.fg(
                    ThemeColor::ToolDiffContext,
                    &format!(" {} {}", parsed.line_num, replace_tabs_cow(&parsed.content)),
                )
            }));
            i += 1;
        }
    }
    out.join("\n")
}

// --- Truncation warnings (upstream renderers) ------------------------------

/// The shell result text with its truncation footer stripped when the final
/// details prove the footer is duplicated (upstream bash.ts:52-60): a
/// truncated result whose text ends with `]` and whose last `\n\n[` block
/// carries the full output path drops that block — the warning line below
/// renders it instead. `#[doc(hidden)] pub` for
/// `tests/suites/tui_messages_units.rs`.
#[doc(hidden)]
pub fn strip_shell_footer<'a>(output: &'a str, details: Option<&serde_json::Value>) -> &'a str {
    let Some(details) = details else {
        return output;
    };
    let truncated = details
        .get("truncation")
        .and_then(|truncation| truncation.get("truncated"))
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    let Some(full_path) = details
        .get("fullOutputPath")
        .and_then(serde_json::Value::as_str)
    else {
        return output;
    };
    if !truncated || !output.ends_with(']') {
        return output;
    }
    let Some(footer_start) = output.rfind("\n\n[") else {
        return output;
    };
    if output[footer_start..].contains(full_path) {
        output[..footer_start].trim_end()
    } else {
        output
    }
}

/// The shell `[Full output: …. Truncated: …]` warning line (upstream
/// bash.ts:85-100), warning-colored. `#[doc(hidden)] pub` for
/// `tests/suites/tui_messages_units.rs`.
#[doc(hidden)]
pub fn shell_truncation_warning(details: Option<&serde_json::Value>) -> Option<String> {
    let details = details?;
    let truncation = details.get("truncation");
    let truncated = truncation
        .and_then(|truncation| truncation.get("truncated"))
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    let full_path = details
        .get("fullOutputPath")
        .and_then(serde_json::Value::as_str);
    if !truncated && full_path.is_none() {
        return None;
    }
    let mut warnings: Vec<String> = Vec::new();
    if let Some(path) = full_path {
        warnings.push(format!("Full output: {path}"));
    }
    if truncated {
        let truncation = truncation?;
        let output_lines = truncation
            .get("outputLines")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0);
        if truncation
            .get("truncatedBy")
            .and_then(serde_json::Value::as_str)
            == Some("lines")
        {
            let total_lines = truncation
                .get("totalLines")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(0);
            warnings.push(format!(
                "Truncated: showing {output_lines} of {total_lines} lines"
            ));
        } else {
            let max_bytes = truncation
                .get("maxBytes")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(DEFAULT_MAX_BYTES as u64);
            warnings.push(format!(
                "Truncated: {output_lines} lines shown ({} limit)",
                format_size(max_bytes as usize)
            ));
        }
    }
    Some(with_theme(|theme| {
        theme.fg(ThemeColor::Warning, &format!("[{}]", warnings.join(". ")))
    }))
}

/// The read `[Truncated: …]`/`[First line exceeds …]` warning line (upstream
/// read.ts:138-147), warning-colored. `#[doc(hidden)] pub` for
/// `tests/suites/tui_messages_units.rs`.
#[doc(hidden)]
pub fn read_truncation_warning(details: Option<&serde_json::Value>) -> Option<String> {
    let truncation = details?.get("truncation")?;
    if !truncation
        .get("truncated")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false)
    {
        return None;
    }
    let output_lines = truncation
        .get("outputLines")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0);
    let text = if truncation
        .get("firstLineExceedsLimit")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false)
    {
        let max_bytes = truncation
            .get("maxBytes")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(DEFAULT_MAX_BYTES as u64);
        format!(
            "[First line exceeds {} limit]",
            format_size(max_bytes as usize)
        )
    } else if truncation
        .get("truncatedBy")
        .and_then(serde_json::Value::as_str)
        == Some("lines")
    {
        let total_lines = truncation
            .get("totalLines")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0);
        let max_lines = truncation
            .get("maxLines")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(DEFAULT_MAX_LINES as u64);
        format!(
            "[Truncated: showing {output_lines} of {total_lines} lines ({max_lines} line limit)]"
        )
    } else {
        let max_bytes = truncation
            .get("maxBytes")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(DEFAULT_MAX_BYTES as u64);
        format!(
            "[Truncated: {output_lines} lines shown ({} limit)]",
            format_size(max_bytes as usize)
        )
    };
    Some(with_theme(|theme| theme.fg(ThemeColor::Warning, &text)))
}

/// Which limited-result warning shape a trimmed body uses (grep/find/ls
/// renderers). `#[doc(hidden)] pub` for `tests/suites/tui_messages_units.rs`.
#[doc(hidden)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LimitedResultKind {
    Grep,
    Find,
    Ls,
}

/// The grep/find/ls `[Truncated: …]` warning line (upstream grep.ts:59-67,
/// find.ts:56-63, ls.ts:52-59), warning-colored. `#[doc(hidden)] pub` for
/// `tests/suites/tui_messages_units.rs`.
#[doc(hidden)]
pub fn limited_result_warning(
    kind: LimitedResultKind,
    details: Option<&serde_json::Value>,
) -> Option<String> {
    let details = details?;
    let (limit_key, limit_unit) = match kind {
        LimitedResultKind::Grep => ("matchLimitReached", "matches limit"),
        LimitedResultKind::Find => ("resultLimitReached", "results limit"),
        LimitedResultKind::Ls => ("entryLimitReached", "entries limit"),
    };
    let limit = details.get(limit_key).and_then(serde_json::Value::as_u64);
    let truncation = details.get("truncation");
    let truncated = truncation
        .and_then(|truncation| truncation.get("truncated"))
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    let lines_truncated = details
        .get("linesTruncated")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    if limit.is_none() && !truncated && !lines_truncated {
        return None;
    }
    let mut warnings: Vec<String> = Vec::new();
    if let Some(limit) = limit {
        warnings.push(format!("{limit} {limit_unit}"));
    }
    if truncated {
        let max_bytes = truncation
            .and_then(|truncation| truncation.get("maxBytes"))
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(DEFAULT_MAX_BYTES as u64);
        warnings.push(format!("{} limit", format_size(max_bytes as usize)));
    }
    if lines_truncated {
        warnings.push("some lines truncated".to_string());
    }
    Some(with_theme(|theme| {
        theme.fg(
            ThemeColor::Warning,
            &format!("[Truncated: {}]", warnings.join(", ")),
        )
    }))
}

// --- Edit block rendering (renderers/edit.ts) ------------------------------

/// Whether the block's edit preview is valid — its `argsKey` still matches the
/// block's current args (upstream `previewArgsKey` guard). The args→`argsKey`
/// parse is memoized on the args `Arc` ([`PreviewKeyCache`]), so the guard
/// costs a pointer compare per frame instead of re-copying and re-serializing
/// every edit in the call.
fn preview_is_current(state: &mut ToolState) -> bool {
    if state.preview.is_none() {
        return false;
    }
    let stale_memo = !matches!(
        state.preview_key.as_ref(),
        Some(cache) if Arc::ptr_eq(&cache.args, &state.args)
    );
    if stale_memo {
        let args_key = edit_preview_input(&state.args).map(|input| input.args_key);
        state.preview_key = Some(PreviewKeyCache {
            args: Arc::clone(&state.args),
            args_key,
        });
    }
    state
        .preview_key
        .as_ref()
        .and_then(|cache| cache.args_key.as_deref())
        == state
            .preview
            .as_ref()
            .map(|preview| preview.args_key.as_str())
}

/// `formatEditResult` (renderers/edit.ts:87-114) for the section BELOW the
/// edit box (upstream result component, unpainted): the error text (unless it
/// merely repeats the preview error), or the result diff (unless it equals
/// the preview diff).
fn edit_result_section(
    state: &ToolState,
    preview_diff: Option<&str>,
    preview_error: Option<&str>,
) -> Option<String> {
    if state.is_error {
        let error_text = state.output.trim_end();
        if error_text.is_empty() || Some(error_text) == preview_error {
            return None;
        }
        return Some(with_theme(|theme| theme.fg(ThemeColor::Error, error_text)));
    }
    let result_diff = state
        .details
        .as_deref()
        .and_then(|details| details.get("diff"))
        .and_then(serde_json::Value::as_str);
    let result_diff = result_diff?;
    if Some(result_diff) == preview_diff {
        return None;
    }
    Some(render_diff_text(result_diff))
}

/// The memoized [`render_diff_text`] body for `diff`, stored in the block's
/// own `Mutex<ToolState>` slot ([`PreviewDiffCache`], no extra lock). The
/// generation is read BEFORE the render: an install that landed mid-render
/// leaves the entry tagged with the older generation, so the next frame
/// misses and re-renders instead of serving mixed-palette lines (generations
/// are never reused; the same ordering the other generation-keyed caches in
/// this module rely on).
fn preview_diff_text(slot: &mut Option<PreviewDiffCache>, diff: &str) -> String {
    let generation = Theme::generation();
    let hit = slot
        .as_ref()
        .filter(|cache| cache.theme_generation == generation && cache.diff == diff);
    if let Some(cache) = hit {
        return cache.text.clone();
    }
    let text = render_diff_text(diff);
    *slot = Some(PreviewDiffCache {
        diff: diff.to_string(),
        theme_generation: generation,
        text: text.clone(),
    });
    text
}

/// The edit tool's `renderShell: "self"` block (renderers/edit.ts): the box
/// IS the call component — it owns its background (`getEditHeaderBg`:
/// success/error as soon as the preview resolves) and padding — while the
/// result section renders below the box, unpainted. Every row is written
/// into the canvas (padded/painted in place), so the block allocates no
/// per-frame line `Vec` and no per-row `String`.
fn render_edit_block(state: &mut ToolState, width: usize, expanded: bool, out: &mut Rows<'_>) {
    // The background pair is resolved from copies of the phase inputs BEFORE
    // the `preview` borrow below (the block's background memo needs `&mut
    // state`, and the preview borrow lives until the end of the render). Same
    // decision table as the direct `match preview { … }`: a current preview
    // paints success (error when it failed), otherwise the pending/settled
    // phase.
    let preview_current = preview_is_current(state);
    let preview_failed =
        preview_current && state.preview.as_ref().is_some_and(|p| p.error.is_some());
    let bg_color = if preview_current {
        if preview_failed {
            ThemeColor::ToolErrorBg
        } else {
            ThemeColor::ToolSuccessBg
        }
    } else if state.pending {
        ThemeColor::ToolPendingBg
    } else if state.is_error {
        ThemeColor::ToolErrorBg
    } else {
        ThemeColor::ToolSuccessBg
    };
    // The frame's palette generation keys the block's background memo (see
    // [`tool_bg_edges`]); read once so every painted row of this block agrees.
    let generation = Theme::generation();
    let bg = tool_bg_edges(state, bg_color, generation);
    let content_width = width.saturating_sub(2).max(1);

    // Outer Container: Spacer(1) (unpainted) + the call Box(1, 1, bg). The
    // call line goes through the same per-block rows memo as the shell path
    // ([`ensure_call_rows`], which needs `&mut state` and so runs BEFORE the
    // `preview` borrow below): the edit formatter is env-dependent, so the
    // text is rendered fresh and the wrapped rows are reused byte-identically
    // while it is unchanged.
    out.push("");
    push_painted_blank_row(out, &bg.open, &bg.close, width);
    ensure_call_rows(state, expanded, content_width);
    if let Some(cache) = &state.call_rows {
        for visual in &cache.rows {
            push_painted_row(out, &bg.open, &bg.close, " ", visual, width);
        }
    }
    let preview = if preview_current {
        state.preview.as_ref()
    } else {
        None
    };
    let preview_error = preview.and_then(|preview| preview.error.as_deref());
    let preview_diff = preview.and_then(|preview| preview.diff.as_deref());
    if let Some(preview) = preview {
        // Spacer(1) + the preview body inside the box. The wrapped, painted
        // rows are memoized ([`EditPreviewRowsCache`]): a steady frame pushes
        // the stored rows, so neither the body nor its wrap is rebuilt.
        push_painted_blank_row(out, &bg.open, &bg.close, width);
        let key_hit = state.edit_preview_rows.as_ref().is_some_and(|cache| {
            cache.bg_color == bg_color
                && cache.width == width
                && cache.theme_generation == generation
                && cache.error.as_deref() == preview.error.as_deref()
                && cache.diff.as_deref() == preview.diff.as_deref()
        });
        if !key_hit {
            bump_memo_work!(preview_rebuilds);
            let body = match preview.error.as_deref() {
                Some(error) => with_theme(|theme| theme.fg(ThemeColor::Error, error)),
                None => preview_diff_text(
                    &mut state.preview_diff_text,
                    preview.diff.as_deref().unwrap_or_default(),
                ),
            };
            let mut rows: Vec<String> = Vec::new();
            {
                let mut canvas = Rows::new(&mut rows);
                for visual in wrap_text_with_ansi(&body, content_width) {
                    push_painted_row(&mut canvas, &bg.open, &bg.close, " ", &visual, width);
                }
                canvas.trim();
            }
            state.edit_preview_rows = Some(EditPreviewRowsCache {
                error: preview.error.clone(),
                diff: preview.diff.clone(),
                bg_color,
                width,
                theme_generation: generation,
                rows,
            });
        }
        if let Some(cache) = &state.edit_preview_rows {
            for row in &cache.rows {
                out.push(row);
            }
        }
    } else if !state.pending && state.details.is_none() {
        // Legacy replays without stored details/preview keep the
        // args-rebuilt body inside the box (the pre-details behaviour). The
        // body re-parses and re-styles the call's `edits[]` and then wraps it,
        // so both are memoized ([`EditLegacyRowsCache`]) on the builder's
        // inputs.
        let key_hit = state.edit_legacy_rows.as_ref().is_some_and(|cache| {
            cache.is_error == state.is_error
                && cache.bg_color == bg_color
                && cache.width == width
                && cache.theme_generation == generation
                && Arc::ptr_eq(&cache.args, &state.args)
                && Arc::ptr_eq(&cache.output, &state.output)
        });
        if !key_hit {
            bump_memo_work!(legacy_rebuilds);
            let body = edit_result_body(&state.args, &state.output, state.is_error);
            let mut rows: Vec<String> = Vec::new();
            if let Some(body) = body {
                let mut canvas = Rows::new(&mut rows);
                for visual in wrap_text_with_ansi(body.trim_start_matches('\n'), content_width) {
                    push_painted_row(&mut canvas, &bg.open, &bg.close, " ", &visual, width);
                }
                canvas.trim();
            }
            state.edit_legacy_rows = Some(EditLegacyRowsCache {
                args: Arc::clone(&state.args),
                output: Arc::clone(&state.output),
                is_error: state.is_error,
                bg_color,
                width,
                theme_generation: generation,
                rows,
            });
        }
        if let Some(cache) = &state.edit_legacy_rows {
            for row in &cache.rows {
                out.push(row);
            }
        }
    }
    push_painted_blank_row(out, &bg.open, &bg.close, width);

    // Result section below the box (unpainted; upstream result component:
    // Spacer(1) + Text(output, outputPad, 0)). Rebuilding the body renders
    // the whole result diff per frame, so the rows are memoized
    // ([`EditResultRowsCache`]) on the same inputs `edit_result_section`
    // reads.
    if state.details.is_some() {
        let key_hit = state.edit_result_rows.as_ref().is_some_and(|cache| {
            cache.is_error == state.is_error
                && cache.width == width
                && cache.theme_generation == generation
                && Arc::ptr_eq(&cache.output, &state.output)
                && match (&cache.details, &state.details) {
                    (Some(mine), Some(theirs)) => Arc::ptr_eq(mine, theirs),
                    (None, None) => true,
                    _ => false,
                }
                && cache.preview_error.as_deref() == preview_error
                && cache.preview_diff.as_deref() == preview_diff
        });
        if !key_hit {
            bump_memo_work!(edit_result_rebuilds);
            let body = edit_result_section(state, preview_diff, preview_error);
            let mut rows: Vec<String> = Vec::new();
            if let Some(body) = body {
                let mut canvas = Rows::new(&mut rows);
                canvas.push("");
                for visual in wrap_text_with_ansi(&body, content_width) {
                    push_prefixed_row(&mut canvas, " ", &visual, width);
                }
                canvas.trim();
            }
            state.edit_result_rows = Some(EditResultRowsCache {
                output: Arc::clone(&state.output),
                details: state.details.clone(),
                is_error: state.is_error,
                preview_error: preview_error.map(str::to_string),
                preview_diff: preview_diff.map(str::to_string),
                width,
                theme_generation: generation,
                rows,
            });
        }
        if let Some(cache) = &state.edit_result_rows {
            for row in &cache.rows {
                out.push(row);
            }
        }
    }
}

// --- Compact read header (renderers/read.ts:22-111) ------------------------

/// `COMPACT_RESOURCE_FILE_NAMES` (read.ts:26).
const COMPACT_RESOURCE_FILE_NAMES: [&str; 5] = [
    "AGENTS.override.md",
    "AGENTS.md",
    "AGENTS.MD",
    "CLAUDE.md",
    "CLAUDE.MD",
];

#[derive(PartialEq, Eq)]
enum CompactReadKind {
    Docs,
    Resource,
    Skill,
}

struct CompactReadClassification {
    kind: CompactReadKind,
    label: String,
}

/// `toPosixPath` (read.ts:46-48).
fn to_posix(path: &std::path::Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

/// Lexical `path.resolve` approximation (upstream `resolvePath`).
fn lexical_resolve(cwd: &std::path::Path, raw: &str) -> std::path::PathBuf {
    let raw_path = std::path::Path::new(raw);
    let joined = if raw_path.is_absolute() {
        raw_path.to_path_buf()
    } else {
        cwd.join(raw_path)
    };
    let mut normalized = std::path::PathBuf::new();
    for component in joined.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                if !normalized.pop() {
                    normalized.push("..");
                }
            }
            other => normalized.push(other.as_os_str()),
        }
    }
    normalized
}

/// `formatPathRelativeToCwdOrAbsolute` / `getCwdRelativePath`
/// (utils/paths.ts:109-123): cwd-relative when inside, absolute otherwise.
fn cwd_relative_or_absolute(path: &std::path::Path, cwd: &std::path::Path) -> String {
    match path.strip_prefix(cwd) {
        Ok(relative) if relative.as_os_str().is_empty() => ".".to_string(),
        Ok(relative) => to_posix(relative),
        Err(_) => to_posix(path),
    }
}

/// `getCompactReadClassification` (read.ts:67-88): `SKILL.md` →
/// `[skill] <parent dir>`, package-docs paths (`README.md`, `docs/…`,
/// `examples/…` under the package root) → `read docs <rel>`, agent resource
/// files → `read resource <path>`. The docs classification needs the package
/// root (`getPackageDir`); pir mirrors `PI_PACKAGE_DIR`
/// (`config::env::package_dir`) and skips the docs tier when it is unset
/// (upstream falls back to its real package directory — gap).
fn compact_read_classification(args: &serde_json::Value) -> Option<CompactReadClassification> {
    let raw_path = match first_str_arg(args, &["file_path", "path"]) {
        ArgStr::Str(path) if !path.is_empty() => path,
        _ => return None,
    };
    // The cwd and `PI_PACKAGE_DIR` come from the captured [`CallLineEnv`]:
    // the old per-call `current_dir()`/`package_dir()` reads allocated on
    // every frame and could only change if the process moved or re-set its
    // environment (see [`refresh_call_line_env`]).
    with_call_line_env(|env| {
        compact_read_classification_with(raw_path, &env.cwd, env.package_dir.as_deref())
    })
}

/// [`compact_read_classification`] against an explicit environment snapshot —
/// the borrowed `cwd`/`package_dir` keep the read path free of per-call
/// `PathBuf` allocations.
fn compact_read_classification_with(
    raw_path: &str,
    cwd: &std::path::Path,
    package_dir: Option<&std::path::Path>,
) -> Option<CompactReadClassification> {
    // `resolveToCwd` (path-utils.ts:48-50) strips a leading `@`
    // (`stripAtPrefix`); unicode-space normalization is not mirrored.
    let raw = raw_path.strip_prefix('@').unwrap_or(raw_path);
    let absolute = lexical_resolve(cwd, raw);
    let file_name = absolute
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    if file_name == "SKILL.md" {
        // `basename(dirname(absolutePath)) || fileName` (read.ts:76-78).
        let label = absolute
            .parent()
            .and_then(std::path::Path::file_name)
            .map(|name| name.to_string_lossy().into_owned())
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| file_name.clone());
        return Some(CompactReadClassification {
            kind: CompactReadKind::Skill,
            label,
        });
    }
    if let Some(root) = package_dir {
        // `getPiDocsClassification` (read.ts:49-66): excluded when the path is
        // the root itself or outside it.
        if let Ok(relative) = absolute.strip_prefix(root) {
            if !relative.as_os_str().is_empty() {
                let label = to_posix(relative);
                if label == "README.md"
                    || label.starts_with("docs/")
                    || label.starts_with("examples/")
                {
                    return Some(CompactReadClassification {
                        kind: CompactReadKind::Docs,
                        label,
                    });
                }
            }
        }
    }
    if COMPACT_RESOURCE_FILE_NAMES.contains(&file_name.as_str()) {
        return Some(CompactReadClassification {
            kind: CompactReadKind::Resource,
            label: cwd_relative_or_absolute(&absolute, cwd),
        });
    }
    None
}

/// `formatCompactReadCall` (read.ts:89-111): compact header +
/// ` (ctrl+o to expand)` hint, both while collapsed.
fn format_compact_read_call(
    classification: &CompactReadClassification,
    args: &serde_json::Value,
) -> String {
    with_theme(|theme| {
        let range = paint_read_line_range(theme, args);
        let expand_hint = theme.fg(ThemeColor::Dim, " (ctrl+o to expand)");
        match classification.kind {
            CompactReadKind::Skill => format!(
                "{}{}{range}{expand_hint}",
                theme.fg(
                    ThemeColor::CustomMessageLabel,
                    "\u{1b}[1m[skill]\u{1b}[22m "
                ),
                theme.fg(ThemeColor::CustomMessageText, &classification.label)
            ),
            CompactReadKind::Docs | CompactReadKind::Resource => {
                let kind = if classification.kind == CompactReadKind::Docs {
                    "docs"
                } else {
                    "resource"
                };
                format!(
                    "{} {}{range}{expand_hint}",
                    theme.fg(ThemeColor::ToolTitle, &theme.bold(&format!("read {kind}"))),
                    theme.fg(ThemeColor::Accent, &classification.label)
                )
            }
        }
    })
}

/// The block's call text (upstream `renderCall` composition): the write
/// content preview (write.ts:145-169), the compact read header while
/// collapsed (read.ts:151-160), else the standard per-tool call line.
fn format_block_call_text(
    tool_name: &str,
    args: &Arc<serde_json::Value>,
    expanded: bool,
    write_preview: &mut Option<WritePreviewCache>,
) -> String {
    match tool_name {
        "write" => write_call_text(args, expanded, write_preview),
        "read" if !expanded => match compact_read_classification(args) {
            Some(classification) => format_compact_read_call(&classification, args),
            None => format_tool_call_line("read", args, expanded),
        },
        _ => format_tool_call_line(tool_name, args, expanded),
    }
}

/// Ensure [`CallRowsCache`] holds this frame's wrapped call-line rows: one
/// entry per block, rebuilt only when the call line, the content width, the
/// palette or the environment snapshot moves. A steady frame re-pushes the
/// stored rows, where the previous per-frame shape re-formatted the line
/// (several `format!`/`fg`/`bold` allocations) and re-wrapped it (a `Vec`
/// plus one `String` per visual line) on every frame of every block.
///
/// The file-tool formatters read the process environment (`HOME`/cwd —
/// `shortenPath`, `getCompactReadClassification`), so their text is a pure
/// function of the args, the render inputs and the once-captured
/// [`CallLineEnv`] — the generation is part of the key, exactly like the
/// palette generation: an explicit [`refresh_call_line_env`] rebuilds the
/// text, a steady frame never formats it at all.
fn ensure_call_rows(state: &mut ToolState, expanded: bool, content_width: usize) {
    let theme_generation = Theme::generation();
    let env_generation = call_line_env_generation();
    // The args `Arc` identity plus the render inputs are the whole key (the
    // `Value` behind the `Arc` is never mutated in place — the entry holds it
    // shared, `app.rs::TextTag` documents the same invariant). The stored
    // text answers for a width-only move.
    let key_hit = state.call_rows.as_ref().is_some_and(|cache| {
        cache.expanded == expanded
            && cache.theme_generation == theme_generation
            && cache.env_generation == env_generation
            && Arc::ptr_eq(&cache.args, &state.args)
    });
    if key_hit {
        if let Some(cache) = state.call_rows.as_mut() {
            if cache.width != content_width {
                bump_memo_work!(call_rows_rewraps);
                cache.rows = wrap_text_with_ansi(&cache.text, content_width);
                cache.width = content_width;
            }
            return;
        }
    }
    bump_memo_work!(call_text_formats);
    let text = format_block_call_text(
        &state.tool_name,
        &state.args,
        expanded,
        &mut state.write_preview,
    );
    bump_memo_work!(call_rows_rewraps);
    let rows = wrap_text_with_ansi(&text, content_width);
    bump_memo_work!(call_rows_rebuilds);
    state.call_rows = Some(CallRowsCache {
        args: Arc::clone(&state.args),
        expanded,
        theme_generation,
        env_generation,
        width: content_width,
        text,
        rows,
    });
}

/// The block's whole row set behind [`render_user_bash`], served from the
/// per-block memo ([`UserBashRowsCache`]): the rows are rebuilt only when the
/// args/output `Arc`s, the pending/expanded flags, the width or the palette
/// generation move; a steady frame re-pushes the stored rows.
fn render_user_bash(state: &mut ToolState, width: usize, expanded: bool, out: &mut Rows<'_>) {
    let theme_generation = Theme::generation();
    let key_hit = state.user_bash_rows.as_ref().is_some_and(|cache| {
        cache.pending == state.pending
            && cache.expanded == expanded
            && cache.width == width
            && cache.theme_generation == theme_generation
            && Arc::ptr_eq(&cache.args, &state.args)
            && Arc::ptr_eq(&cache.output, &state.output)
    });
    if !key_hit {
        bump_memo_work!(user_bash_rebuilds);
        // Same shape as [`ResultRowsCache`]: build into a fresh pool the memo
        // keeps, then push the stored rows into the frame canvas.
        let mut rows: Vec<String> = Vec::new();
        {
            let mut canvas = Rows::new(&mut rows);
            render_user_bash_rows(state, width, expanded, &mut canvas);
            canvas.trim();
        }
        state.user_bash_rows = Some(UserBashRowsCache {
            args: Arc::clone(&state.args),
            output: Arc::clone(&state.output),
            pending: state.pending,
            expanded,
            width,
            theme_generation,
            rows,
        });
    }
    if let Some(cache) = &state.user_bash_rows {
        for row in &cache.rows {
            out.push(row);
        }
    }
}

/// `!`/`!!` user-bash blocks (upstream `BashExecutionComponent`), detected by
/// the `userBash` display marker the TUI puts into the args (terminal.rs
/// `handle_bash` / `WorkerCommand::Bash`).
fn is_user_bash(tool_name: &str, args: &serde_json::Value) -> bool {
    tool_name == "bash" && args.get("userBash").and_then(serde_json::Value::as_bool) == Some(true)
}

/// Port of `BashExecutionComponent` (components/bash-execution.ts:21-223):
/// Spacer + bashMode(`dim` for `!!`)-framed block; bold `$ <command>` header;
/// collapsed preview of the LAST 20 logical lines, visually truncated to 20
/// lines; a Loader line while running; the `(cancelled)` / `(exit N)` status
/// tail and the hidden-lines expand/collapse hints.
/// Gaps: pir receives no streaming output chunks (the block only shows the
/// Loader, then the final text), and no execution path currently produces a
/// `cancelled` flag; the truncation `Full output: <path>` tail needs
/// `fullOutputPath` (absent → omitted, matching upstream's condition).
/// Every row is written into the canvas (padded in place), so the block
/// allocates no per-frame line `Vec` and no per-row `String`; the uncached
/// builder behind [`render_user_bash`].
fn render_user_bash_rows(state: &ToolState, width: usize, expanded: bool, out: &mut Rows<'_>) {
    let exclude = state
        .args
        .get("excludeFromContext")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    // bash-execution.ts:38 `colorKey` — `dim` for `!!`.
    let color = if exclude {
        ThemeColor::Dim
    } else {
        ThemeColor::BashMode
    };
    let command = state
        .args
        .get("command")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("...");
    let exit_code = state
        .args
        .get("exitCode")
        .and_then(serde_json::Value::as_i64);
    let cancelled = state
        .args
        .get("cancelled")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    let content_width = width.saturating_sub(2).max(1);
    // Spacer(1) + top DynamicBorder (bash-execution.ts:42-46).
    out.push("");
    out.push(&with_theme(|theme| {
        theme.fg(color, &"─".repeat(width.max(1)))
    }));
    // Header (bash-execution.ts:141).
    let header = with_theme(|theme| theme.fg(color, &theme.bold(&format!("$ {command}"))));
    for visual in wrap_text_with_ansi(&header, content_width) {
        push_prefixed_row(out, " ", &visual, width);
    }
    // Context truncation then the preview slice (bash-execution.ts:124-135).
    let truncated = crate::coding_agent::truncate::truncate_tail(
        state.output.as_ref(),
        crate::coding_agent::truncate::DEFAULT_MAX_LINES,
        crate::coding_agent::truncate::DEFAULT_MAX_BYTES,
    );
    let available: Vec<&str> = if truncated.content.is_empty() {
        Vec::new()
    } else {
        truncated.content.split('\n').collect()
    };
    let preview = &available[available.len().saturating_sub(BASH_EXECUTION_PREVIEW_LINES)..];
    let hidden = available.len() - preview.len();
    if !available.is_empty() {
        if expanded {
            // `new Text(`\n${displayText}`)` (bash-execution.ts:146-149) —
            // the styled lines go into one buffer (the old
            // `map(...).collect::<Vec<_>>().join("\n")` built a `Vec<String>`
            // of them plus a second full copy for the join, per frame).
            let mut display = String::new();
            for (index, line) in available.iter().enumerate() {
                if index > 0 {
                    display.push('\n');
                }
                display.push_str(&with_theme(|theme| theme.fg(ThemeColor::Muted, line)));
            }
            for visual in wrap_text_with_ansi(&format!("\n{display}"), content_width) {
                push_prefixed_row(out, " ", &visual, width);
            }
        } else {
            // `truncateToVisualLines(styledInput, PREVIEW_LINES, width,
            // outputPad)` keep end (bash-execution.ts:150-170); the hidden
            // logical lines are reported by the status tail below, not a hint
            // line inside the preview. Same one-buffer styling as above, and
            // the truncation runs against the wrap itself (the helper used to
            // re-wrap the same text and copy the kept tail into a fresh
            // `Vec`, which the canvas path drops).
            let mut styled = String::new();
            for (index, line) in preview.iter().enumerate() {
                if index > 0 {
                    styled.push('\n');
                }
                styled.push_str(&with_theme(|theme| theme.fg(ThemeColor::Muted, line)));
            }
            let visual = wrap_text_with_ansi(&format!("\n{styled}"), content_width);
            let start = visual.len().saturating_sub(BASH_EXECUTION_PREVIEW_LINES);
            for line in &visual[start..] {
                push_prefixed_row(out, " ", line, width);
            }
        }
    }
    // Loader / status (bash-execution.ts:173-207).
    if state.pending {
        // `Running... (${keyText("tui.select.cancel")} to cancel)`
        // (bash-execution.ts:52-57) — spinner in the frame color, message
        // muted. pir has no per-component animation timer, so the first
        // spinner frame renders (no animation).
        let mut loader = with_theme(|theme| theme.fg(color, super::app::spinner_frame(0)));
        loader.push(' ');
        loader.push_str(&with_theme(|theme| {
            theme.fg(ThemeColor::Muted, "Running... (escape/ctrl+c to cancel)")
        }));
        out.push("");
        for visual in wrap_text_with_ansi(&loader, content_width) {
            push_prefixed_row(out, " ", &visual, width);
        }
    } else {
        // The blank separator row exists only when a tail renders, so the
        // tail conditions are read before it is claimed (the old code
        // collected the styled tails into a `Vec` per frame to test that).
        let exit_code = exit_code.filter(|code| *code != 0);
        if hidden > 0 || cancelled || exit_code.is_some() {
            out.push("");
            if hidden > 0 {
                let hint = if expanded {
                    with_theme(|theme| {
                        format!(
                            "{}{}{}",
                            theme.fg(ThemeColor::Muted, "("),
                            expand_key_hint(theme, "to collapse"),
                            theme.fg(ThemeColor::Muted, ")")
                        )
                    })
                } else {
                    with_theme(|theme| {
                        format!(
                            "{} {} {}",
                            theme.fg(ThemeColor::Muted, &format!("... {hidden} more lines (")),
                            expand_key_hint(theme, "to expand"),
                            theme.fg(ThemeColor::Muted, ")")
                        )
                    })
                };
                for visual in wrap_text_with_ansi(&hint, content_width) {
                    push_prefixed_row(out, " ", &visual, width);
                }
            }
            if cancelled {
                let tail = with_theme(|theme| theme.fg(ThemeColor::Warning, "(cancelled)"));
                for visual in wrap_text_with_ansi(&tail, content_width) {
                    push_prefixed_row(out, " ", &visual, width);
                }
            } else if let Some(code) = exit_code {
                let tail =
                    with_theme(|theme| theme.fg(ThemeColor::Error, &format!("(exit {code})")));
                for visual in wrap_text_with_ansi(&tail, content_width) {
                    push_prefixed_row(out, " ", &visual, width);
                }
            }
        }
    }
    // Bottom DynamicBorder (bash-execution.ts:60).
    out.push(&with_theme(|theme| {
        theme.fg(color, &"─".repeat(width.max(1)))
    }));
}

// ---------------------------------------------------------------------------
// Tool execution component — port of `ToolExecutionComponent` (default
// shell) + `truncateToVisualLines` preview behavior.
// ---------------------------------------------------------------------------

/// Collapsed generic preview length (tool-execution.ts:23
/// `FALLBACK_PREVIEW_LINES`).
const FALLBACK_PREVIEW_LINES: usize = 10;
/// Collapsed shell output preview length (renderers/bash.ts:18
/// `BASH_PREVIEW_LINES`).
const BASH_PREVIEW_LINES: usize = 5;

/// The `Theme::bg` opener/closer escapes for `color` — `Theme::bg` renders
/// exactly `{open}{text}\x1b[49m`, so painting an empty string splits the pair
/// out for the row builders below. Both are empty when the palette carries no
/// background for `color` (`Theme::bg` then returns its text untouched).
struct BgEdges {
    open: String,
    close: String,
}

/// One block's memoized background escape pair ([`tool_bg_edges`]).
struct BgEdgesCache {
    generation: u64,
    color: ThemeColor,
    edges: Arc<BgEdges>,
}

/// The block's background pair for `color` under the frame's palette
/// `generation`, taken from the block's own memo and shared out behind an
/// `Arc` (handing it out costs a refcount bump, never an allocation).
///
/// Invariant: the pair is a pure function of exactly two inputs — the palette
/// generation and the color (`Theme::bg` reads nothing else) — so a hit is
/// byte-exact. The pre-memo form built the probe `String`, the opener and the
/// closer on every call, i.e. three allocations per painted tool block per
/// frame plus one more probe allocation, for bytes that only change when the
/// palette does. A cache miss (first frame, theme install, or a phase flip
/// between pending/success/error) rebuilds once; a theme installed mid-render
/// repaints on the next frame through the generation check, this file's
/// existing one-palette-per-frame posture.
fn tool_bg_edges(state: &mut ToolState, color: ThemeColor, generation: u64) -> Arc<BgEdges> {
    if let Some(cache) = &state.bg_edges {
        if cache.generation == generation && cache.color == color {
            return Arc::clone(&cache.edges);
        }
    }
    bump_memo_work!(bg_rebuilds);
    let edges = Arc::new(bg_edges_uncached(color));
    state.bg_edges = Some(BgEdgesCache {
        generation,
        color,
        edges: Arc::clone(&edges),
    });
    edges
}

/// The uncached pair builder behind [`tool_bg_edges`].
fn bg_edges_uncached(color: ThemeColor) -> BgEdges {
    let probe = with_theme(|theme| theme.bg(color, ""));
    match probe.strip_suffix(BG_RESET) {
        Some(open) => BgEdges {
            open: open.to_string(),
            close: BG_RESET.to_string(),
        },
        None => BgEdges {
            open: String::new(),
            close: String::new(),
        },
    }
}

/// One prefixed, padded, background-painted box row, written into a pooled
/// canvas row: the exact bytes
/// `theme.bg(color, &pad_prefixed_to_width(prefix, text, width))` produces.
/// The previous shape built that padded `String` (two full `width`-sized
/// allocations plus one extra copy inside `Theme::bg`) per painted visual
/// line of every block, every frame, and the frame path then copied it into a
/// canvas row and dropped it.
fn push_painted_row(
    out: &mut Rows<'_>,
    open: &str,
    close: &str,
    prefix: &str,
    text: &str,
    width: usize,
) {
    out.push("");
    let row = out.line_mut();
    row.push_str(open);
    // Width of the prefixed text alone, exactly what `pad_prefixed_to_width`
    // measures before it pads (the paint opener is not measured).
    let body = row.len();
    row.push_str(prefix);
    row.push_str(text);
    let pad = width.saturating_sub(visible_width(&row[body..]));
    row.reserve(pad);
    for _ in 0..pad {
        row.push(' ');
    }
    row.push_str(close);
}

/// The blank box row (`theme.bg(color, &" ".repeat(width))`) written into a
/// pooled canvas row — no throwaway `" ".repeat(width)` buffer and no
/// per-frame row `String`.
fn push_painted_blank_row(out: &mut Rows<'_>, open: &str, close: &str, width: usize) {
    out.push("");
    let row = out.line_mut();
    row.push_str(open);
    row.reserve(width);
    for _ in 0..width {
        row.push(' ');
    }
    row.push_str(close);
}

/// [`super::ansi::pad_prefixed_to_width`] written into a pooled canvas row: the
/// prefix and text are concatenated into the row and the padding is measured on
/// those identical bytes.
fn push_prefixed_row(out: &mut Rows<'_>, prefix: &str, text: &str, width: usize) {
    out.push("");
    let row = out.line_mut();
    row.push_str(prefix);
    row.push_str(text);
    let pad = width.saturating_sub(visible_width(row.as_str()));
    row.reserve(pad);
    for _ in 0..pad {
        row.push(' ');
    }
}

/// `keyHint("app.tools.expand", description)` (keybinding-hints.ts:40-42):
/// dim key text followed by the muted description.
fn expand_key_hint(theme: &super::theme::Theme, description: &str) -> String {
    format!(
        "{}{}",
        theme.fg(ThemeColor::Dim, "ctrl+o"),
        theme.fg(ThemeColor::Muted, &format!(" {description}"))
    )
}

/// `... (N more lines, ctrl+o to expand)` (tool-execution.ts:157).
fn more_lines_hint(theme: &super::theme::Theme, skipped: usize) -> String {
    format!(
        "{} {}{}",
        theme.fg(ThemeColor::Muted, &format!("... ({skipped} more lines,")),
        expand_key_hint(theme, "to expand"),
        theme.fg(ThemeColor::Muted, ")")
    )
}

/// `... (N earlier lines, ctrl+o to expand)` (renderers/bash.ts:77-79).
fn earlier_lines_hint(theme: &super::theme::Theme, skipped: usize) -> String {
    format!(
        "{} {}{}",
        theme.fg(ThemeColor::Muted, &format!("... ({skipped} earlier lines,")),
        expand_key_hint(theme, "to expand"),
        theme.fg(ThemeColor::Muted, ")")
    )
}

/// `formatDuration` (renderers/bash.ts:20-30): `3.2s` → `1m 5s` → `1h 2m 5s`.
fn format_duration(ms: u64) -> String {
    let seconds = ms as f64 / 1000.0;
    if seconds < 60.0 {
        return format!("{seconds:.1}s");
    }
    let total_seconds = seconds.floor() as u64;
    let minutes = total_seconds / 60;
    let remainder = total_seconds % 60;
    if minutes < 60 {
        return format!("{minutes}m {remainder}s");
    }
    format!("{}h {}m {remainder}s", minutes / 60, minutes % 60)
}

/// The `Elapsed X` / `Took X` footer of a timed tool row (upstream
/// `toolTimerText`, render-utils.ts:125-140). While the row is partial the
/// live clock from `startedAt` drives it; once settled the recorded
/// `durationMs` wins (monotonic, survives reloads) and the clock is the
/// fallback. `elapsed_ms` is the resolved `started_at` tick — read once by
/// [`cached_timer_tail`], so the memo's key and this text come from the same
/// instant.
fn tool_timer_tail(
    pending: bool,
    duration_ms: Option<u64>,
    elapsed_ms: Option<u64>,
) -> Option<String> {
    if pending {
        elapsed_ms.map(|ms| format!("Elapsed {}", format_duration(ms)))
    } else {
        duration_ms
            .or(elapsed_ms)
            .map(|ms| format!("Took {}", format_duration(ms)))
    }
}

/// [`tool_timer_tail`] behind the block's memo (see [`TimerTailCache`]): a
/// settled block answers with its stored `Arc<str>` — the result-rows key no
/// longer includes the tail, so a clock tick only repaints the single tail
/// row. Only a moved input re-renders the text.
fn cached_timer_tail(state: &mut ToolState) -> Option<Arc<str>> {
    let pending = state.pending;
    let duration_ms = state.duration_ms;
    // The clock is an input only while the tail ticks: a pending row, or a
    // settled one whose duration was never recorded (the `started_at`
    // fallback). `format_duration` renders sub-minute durations at 0.1 s
    // precision, so the memo keys on the same quantum: every spinner tick
    // inside one 0.1 s window is a hit, and crossing a boundary rebuilds the
    // text exactly once.
    let elapsed_ms = if pending || duration_ms.is_none() {
        state
            .started_at
            .map(|started| timer_display_tick(started.elapsed().as_millis() as u64))
    } else {
        None
    };
    let hit = state.timer_tail.as_ref().is_some_and(|cache| {
        cache.pending == pending
            && cache.duration_ms == duration_ms
            && cache.elapsed_ms == elapsed_ms
    });
    if !hit {
        bump_memo_work!(tail_rebuilds);
        let text = tool_timer_tail(pending, duration_ms, elapsed_ms);
        state.timer_tail = Some(TimerTailCache {
            pending,
            duration_ms,
            elapsed_ms,
            text: text.map(Arc::<str>::from),
        });
    }
    state
        .timer_tail
        .as_ref()
        .and_then(|cache| cache.text.clone())
}

/// Quantize a live elapsed time to the precision `format_duration` displays:
/// tenths of a second below a minute, whole seconds above it. The quantized
/// value drives both the memo key and the rendered text, so a cached tail is
/// byte-identical to what a fresh render at that tick would produce.
fn timer_display_tick(elapsed_ms: u64) -> u64 {
    if elapsed_ms < 60_000 {
        elapsed_ms / 100 * 100
    } else {
        elapsed_ms / 1_000 * 1_000
    }
}

/// Tool execution component (upstream `ToolExecutionComponent`, default
/// shell): an unpainted spacer row, then the `toolPendingBg`/
/// `toolSuccessBg`/`toolErrorBg` box (1-cell x/y padding) holding the call
/// line and, once a result exists, the output preview.
pub struct ToolExecutionComponent {
    state: Mutex<ToolState>,
    /// Upstream `ToolExecutionComponent.setExpanded(app.tools.expand)`
    /// (interactive-mode.ts:4557-4575): the app-level Ctrl+O toggle is shared
    /// with every block and read live, so expansion applies on the next frame
    /// without rebuilding the transcript.
    tools_expanded: Arc<AtomicBool>,
}

struct ToolState {
    tool_name: String,
    args: Arc<serde_json::Value>,
    output: Arc<str>,
    is_error: bool,
    /// Monotonic `durationMs` from `tool_execution_end`.
    duration_ms: Option<u64>,
    /// Still running (upstream `isPartial`): pending background, call only.
    pending: bool,
    /// Final result `details` (upstream renderers read `result.details`).
    details: Option<Arc<serde_json::Value>>,
    /// Wall clock at pending-block creation (upstream shell `startedAt`).
    started_at: Option<std::time::Instant>,
    /// Edit live preview (upstream `EditCallRenderComponent.preview`).
    preview: Option<super::app::ToolPreview>,
    /// Memoized call line and its wrapped rows for this block (see
    /// [`CallRowsCache`]).
    call_rows: Option<CallRowsCache>,
    /// Memoized content scan of a `write` block (see [`WritePreviewCache`]).
    write_preview: Option<WritePreviewCache>,
    /// Memoized `argsKey` of this block's args (see [`PreviewKeyCache`]).
    preview_key: Option<PreviewKeyCache>,
    /// Memoized rendered preview diff text of this block (see
    /// [`PreviewDiffCache`]).
    preview_diff_text: Option<PreviewDiffCache>,
    /// Memoized rendered result section of this block (see
    /// [`ResultRowsCache`]).
    result_rows: Option<ResultRowsCache>,
    /// Memoized background escape pair of this block (see [`BgEdgesCache`]).
    bg_edges: Option<BgEdgesCache>,
    /// Memoized rendered timer tail of this block (see
    /// [`TimerTailCache`]).
    timer_tail: Option<TimerTailCache>,
    /// Memoized rendered rows of a `!`/`!!` bash block (see
    /// [`UserBashRowsCache`]).
    user_bash_rows: Option<UserBashRowsCache>,
    /// Memoized rows of the edit box's preview body (see
    /// [`EditPreviewRowsCache`]).
    edit_preview_rows: Option<EditPreviewRowsCache>,
    /// Memoized rows of the edit box's args-rebuilt legacy body (see
    /// [`EditLegacyRowsCache`]).
    edit_legacy_rows: Option<EditLegacyRowsCache>,
    /// Memoized rows of the edit result section below the box (see
    /// [`EditResultRowsCache`]).
    edit_result_rows: Option<EditResultRowsCache>,
    /// Tool-result image blocks (upstream `ToolExecutionComponent`'s
    /// `imageComponents`): rendered after the tool box through the same
    /// Kitty/iTerm2 path assistant images and `/image` use.
    images: Arc<Vec<ImageContent>>,
    /// Memoized rendered image rows (see [`ImageRowsCache`]).
    image_rows: Option<ImageRowsCache>,
}

/// Memo of one block's rendered result-image rows. The source images are
/// `Arc`-shared with the chat entry (immutable for the block's lifetime), so
/// pointer identity plus the render width and capability generation answers
/// "did anything move?" without hashing the base64 payload per frame.
struct ImageRowsCache {
    images: Arc<Vec<ImageContent>>,
    width: usize,
    capabilities_generation: u64,
    rows: Vec<String>,
}

/// Memo of one block's rendered result section ([`render_result_section`]):
/// every input its rows were built from, cheap to compare. Both payload
/// `Arc`s are held — not just their addresses — so pointer identity stays
/// sound: the buffer behind them cannot be freed and its address recycled by
/// a different value while the cache holds it. The tool name is fixed per
/// component and the box width/phase feed the key through `width` and the
/// `pending`/`is_error` pair. The timer tail is deliberately NOT part of the
/// key: a clock tick repaints only that one row (see [`cached_timer_tail`]),
/// so a pending shell block with a 50 KiB output no longer re-styles and
/// re-wraps the whole body every 0.1 s.
struct ResultRowsKey {
    output: Arc<str>,
    details: Option<Arc<serde_json::Value>>,
    expanded: bool,
    width: usize,
    theme_generation: u64,
    pending: bool,
    is_error: bool,
    duration_ms: Option<u64>,
}

impl ResultRowsKey {
    fn matches(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.output, &other.output)
            && match (&self.details, &other.details) {
                (Some(mine), Some(theirs)) => Arc::ptr_eq(mine, theirs),
                (None, None) => true,
                _ => false,
            }
            && self.expanded == other.expanded
            && self.width == other.width
            && self.theme_generation == other.theme_generation
            && self.pending == other.pending
            && self.is_error == other.is_error
            && self.duration_ms == other.duration_ms
    }
}

/// Memo of one block's rendered result section: the final padded/painted rows
/// (background escapes included) exactly as the section renderer produced
/// them last time, so a frame whose block did not change re-pushes these rows
/// instead of re-rendering the body.
struct ResultRowsCache {
    key: ResultRowsKey,
    rows: Vec<String>,
}

/// Memo of the block's rendered call line AND its wrapped rows (see
/// [`ensure_call_rows`]). The call text is a pure function of the (immutable)
/// tool name, the args `Arc`, `expanded`, the active theme and — for the
/// file-tool formatters, which resolve `HOME`/cwd (`shortenPath`,
/// `getCompactReadClassification`) — the [`CallLineEnv`] snapshot generation,
/// so every input is in the key and a steady frame re-pushes the stored rows.
/// The rows are that text wrapped to the box content width, so `width` is the
/// CONTENT width the wrap ran at — a box-width change re-wraps.
struct CallRowsCache {
    args: Arc<serde_json::Value>,
    expanded: bool,
    theme_generation: u64,
    /// [`call_line_env_generation`] the text was formatted under.
    env_generation: u64,
    /// Box content width the rows were wrapped to.
    width: usize,
    /// The call text the rows were wrapped from.
    text: String,
    /// The wrapped call line (`wrap_text_with_ansi(&text, width)`).
    rows: Vec<String>,
}

/// Memo of one block's rendered timer tail (see [`cached_timer_tail`]).
///
/// The settled tail (`Took X`) is a pure function of the recorded
/// `duration_ms` — the state every finished block sits in for the rest of the
/// process — so it is rendered once and re-served; that is what keeps a long
/// transcript's steady frame free of the per-block `format!` pair. The live
/// postures (pending, or settled without a recorded duration) tick with the
/// wall clock quantized to [`timer_display_tick`] — the precision
/// `format_duration` displays — so spinner frames inside one 0.1 s window all
/// hit and the boundary rebuilds the text exactly once.
struct TimerTailCache {
    pending: bool,
    duration_ms: Option<u64>,
    /// The `started_at`-derived display tick in ms; `None` when the clock is
    /// not an input (settled with a recorded duration).
    elapsed_ms: Option<u64>,
    /// The rendered tail; `None` when the block shows no timer at all.
    text: Option<Arc<str>>,
}

/// Memo of [`edit_preview_input`]'s `argsKey` for the block's args — the parse
/// reads nothing else. The per-frame preview validation otherwise re-parses
/// and JSON-serializes the whole `edits[]` array, re-copying every
/// `oldText`/`newText` of the call on every frame.
struct PreviewKeyCache {
    args: Arc<serde_json::Value>,
    args_key: Option<String>,
}

/// Memo of [`render_diff_text`]'s body for the block's live preview diff. The
/// body is a pure function of exactly two inputs — the diff text and the
/// active theme (every `with_theme` in the pipeline reads the process
/// palette, and [`Theme::generation`] keys it) — so both participate in the
/// key. The width-dependent wrapping and padding run on the RETURNED string,
/// in `render_edit_block`, so neither `width` nor `expanded` is a factor here.
///
/// The diff is keyed by CONTENT: `preview.diff` is a plain `String` whose
/// buffer address the allocator may recycle for a later, different diff, so
/// pointer identity could alias a stale hit while a content compare cannot. A
/// hit pays one `str` compare plus the single clone of the cached body that
/// replaces the per-line `parse_diff_line`/`replace_tabs_cow`/`format!`/
/// `theme.fg` chain and the final `join`.
struct PreviewDiffCache {
    diff: String,
    theme_generation: u64,
    text: String,
}

/// Per-frame work counters for the tool block's caches — a measurement seam in
/// the spirit of `TuiCore`'s [`FrameWork`](crate::tui::core::FrameWork): one
/// relaxed add per rebuild, no allocation, no lock.
///
/// A steady frame (unchanged inputs) must leave every counter at zero; that is
/// the invariant `tests/tui_frame_reuse.rs` pins. When a frame still allocates,
/// this attributes the work to the exact memo that rebuilt.
#[doc(hidden)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ToolMemoWork {
    /// Frames the shell/user-bash/edit paths rendered.
    pub frames: u64,
    /// `format_block_call_text` runs (only on a memo key miss).
    pub call_text_formats: u64,
    /// `wrap_text_with_ansi` runs for the call line (key miss or width move).
    pub call_rows_rewraps: u64,
    /// `CallRowsCache` slot stores.
    pub call_rows_rebuilds: u64,
    /// `BgEdgesCache` rebuilds (palette generation or phase color moved).
    pub bg_rebuilds: u64,
    /// `TimerTailCache` rebuilds (the tail text is re-rendered).
    pub tail_rebuilds: u64,
    /// `ResultRowsCache` (shell/read/... result section) rebuilds.
    pub result_rebuilds: u64,
    /// `EditPreviewRowsCache` (edit box preview body) rebuilds.
    pub preview_rebuilds: u64,
    /// `EditLegacyRowsCache` (edit box args-rebuilt body) rebuilds.
    pub legacy_rebuilds: u64,
    /// `EditResultRowsCache` (edit result section) rebuilds.
    pub edit_result_rebuilds: u64,
    /// `UserBashRowsCache` (whole `!`/`!!` block) rebuilds.
    pub user_bash_rebuilds: u64,
}

impl ToolMemoWork {
    /// The process-wide counters as of now (tests / probes).
    #[doc(hidden)]
    pub fn snapshot() -> Self {
        Self {
            frames: TOOL_MEMO_WORK.frames.load(Ordering::Relaxed),
            call_text_formats: TOOL_MEMO_WORK.call_text_formats.load(Ordering::Relaxed),
            call_rows_rewraps: TOOL_MEMO_WORK.call_rows_rewraps.load(Ordering::Relaxed),
            call_rows_rebuilds: TOOL_MEMO_WORK.call_rows_rebuilds.load(Ordering::Relaxed),
            bg_rebuilds: TOOL_MEMO_WORK.bg_rebuilds.load(Ordering::Relaxed),
            tail_rebuilds: TOOL_MEMO_WORK.tail_rebuilds.load(Ordering::Relaxed),
            result_rebuilds: TOOL_MEMO_WORK.result_rebuilds.load(Ordering::Relaxed),
            preview_rebuilds: TOOL_MEMO_WORK.preview_rebuilds.load(Ordering::Relaxed),
            legacy_rebuilds: TOOL_MEMO_WORK.legacy_rebuilds.load(Ordering::Relaxed),
            edit_result_rebuilds: TOOL_MEMO_WORK.edit_result_rebuilds.load(Ordering::Relaxed),
            user_bash_rebuilds: TOOL_MEMO_WORK.user_bash_rebuilds.load(Ordering::Relaxed),
        }
    }

    /// Zero every counter (tests read a per-frame delta around a render).
    #[doc(hidden)]
    pub fn reset() {
        for counter in [
            &TOOL_MEMO_WORK.frames,
            &TOOL_MEMO_WORK.call_text_formats,
            &TOOL_MEMO_WORK.call_rows_rewraps,
            &TOOL_MEMO_WORK.call_rows_rebuilds,
            &TOOL_MEMO_WORK.bg_rebuilds,
            &TOOL_MEMO_WORK.tail_rebuilds,
            &TOOL_MEMO_WORK.result_rebuilds,
            &TOOL_MEMO_WORK.preview_rebuilds,
            &TOOL_MEMO_WORK.legacy_rebuilds,
            &TOOL_MEMO_WORK.edit_result_rebuilds,
            &TOOL_MEMO_WORK.user_bash_rebuilds,
        ] {
            counter.store(0, Ordering::Relaxed);
        }
    }
}

/// The atomics behind [`ToolMemoWork`] (relaxed: counters, not sync).
struct ToolMemoWorkCounters {
    frames: AtomicU64,
    call_text_formats: AtomicU64,
    call_rows_rewraps: AtomicU64,
    call_rows_rebuilds: AtomicU64,
    bg_rebuilds: AtomicU64,
    tail_rebuilds: AtomicU64,
    result_rebuilds: AtomicU64,
    preview_rebuilds: AtomicU64,
    legacy_rebuilds: AtomicU64,
    edit_result_rebuilds: AtomicU64,
    user_bash_rebuilds: AtomicU64,
}

static TOOL_MEMO_WORK: ToolMemoWorkCounters = ToolMemoWorkCounters {
    frames: AtomicU64::new(0),
    call_text_formats: AtomicU64::new(0),
    call_rows_rewraps: AtomicU64::new(0),
    call_rows_rebuilds: AtomicU64::new(0),
    bg_rebuilds: AtomicU64::new(0),
    tail_rebuilds: AtomicU64::new(0),
    result_rebuilds: AtomicU64::new(0),
    preview_rebuilds: AtomicU64::new(0),
    legacy_rebuilds: AtomicU64::new(0),
    edit_result_rebuilds: AtomicU64::new(0),
    user_bash_rebuilds: AtomicU64::new(0),
};

/// Memo of one `!`/`!!` bash block's rendered rows ([`render_user_bash`]).
///
/// The block's rows are a pure function of the args `Arc` (the `userBash`
/// marker, `command`, `exitCode`, `cancelled`, `excludeFromContext`; the
/// `Value` behind the `Arc` is never mutated in place), the output `Arc`, the
/// pending/expanded flags, the frame width and the palette generation — all
/// cheap to key, pointer identity included. A settled block is a guaranteed
/// hit, so the steady frame re-pushes the stored rows instead of re-styling
/// and re-wrapping the header, the whole preview body and every status tail
/// (the pre-memo shape rebuilt all of them, with one `String` per visual
/// line, on every frame).
struct UserBashRowsCache {
    args: Arc<serde_json::Value>,
    output: Arc<str>,
    pending: bool,
    expanded: bool,
    width: usize,
    theme_generation: u64,
    rows: Vec<String>,
}

/// Memo of the edit box's preview body rows ([`render_edit_block`]): the
/// wrapped, `box`-painted rows of the preview diff (or its error text).
///
/// The body is keyed by its INPUTS, not by the rendered text: the preview
/// strings are plain `String`s (CONTENT key, like [`PreviewDiffCache`] — their
/// buffers can be recycled) and the body is a pure function of the diff/error
/// text, the palette generation and the box width, so a steady frame serves
/// the stored rows without even cloning the body out of [`PreviewDiffCache`].
struct EditPreviewRowsCache {
    error: Option<String>,
    diff: Option<String>,
    bg_color: ThemeColor,
    width: usize,
    theme_generation: u64,
    rows: Vec<String>,
}

/// Memo of the edit box's args-rebuilt legacy body rows
/// ([`edit_result_body`], the posture with no preview and no `details`): the
/// body re-parses and re-styles the call's `edits[]` on every frame, so the
/// painted rows are keyed on the body's inputs — the args `Arc`, the output
/// `Arc`, `is_error`, the box width and the palette generation.
struct EditLegacyRowsCache {
    args: Arc<serde_json::Value>,
    output: Arc<str>,
    is_error: bool,
    bg_color: ThemeColor,
    width: usize,
    theme_generation: u64,
    rows: Vec<String>,
}

/// Memo of the edit result section's rows (the unpainted part below the box,
/// [`edit_result_section`]): the section re-renders the whole result diff
/// (`render_diff_text`) or the error text every frame, so its rows are keyed
/// on the same inputs the body builder reads — the output/details `Arc`s (the
/// [`ResultRowsKey`] posture), `is_error`, the preview's error/diff text (the
/// section is skipped when either repeats the preview) and the palette
/// generation. An empty `rows` means the section renders nothing.
struct EditResultRowsCache {
    output: Arc<str>,
    details: Option<Arc<serde_json::Value>>,
    is_error: bool,
    preview_error: Option<String>,
    preview_diff: Option<String>,
    width: usize,
    theme_generation: u64,
    rows: Vec<String>,
}

/// The transcript rows of a tool result's image blocks, in the upstream
/// `ToolExecutionComponent` shape: one unpainted spacer, then the block's
/// protocol lines (or its `[Image: …]` fallback) at the shared transcript
/// image width (`min(width - 2, imageWidthCells-or-60)`, like the assistant
/// image arm and upstream `components/image.ts`).
pub fn tool_image_lines(images: &[ImageContent], width: usize) -> Vec<String> {
    if images.is_empty() {
        return Vec::new();
    }
    let max_width = width.saturating_sub(2).clamp(1, 60);
    let max_height = max_width.div_ceil(2).max(1);
    let mut rows = Vec::new();
    for image in images {
        rows.push(String::new());
        rows.extend(super::terminal_image::image_content_lines(
            &image.data,
            &image.mime_type,
            max_width,
            Some(max_height),
        ));
    }
    rows
}

impl ToolExecutionComponent {
    pub fn new(
        tool_name: &str,
        args: Arc<serde_json::Value>,
        tools_expanded: Arc<AtomicBool>,
    ) -> Self {
        Self {
            state: Mutex::new(ToolState {
                tool_name: tool_name.to_string(),
                args,
                output: Arc::from(""),
                is_error: false,
                duration_ms: None,
                pending: true,
                details: None,
                started_at: None,
                preview: None,
                call_rows: None,
                write_preview: None,
                preview_key: None,
                preview_diff_text: None,
                result_rows: None,
                bg_edges: None,
                timer_tail: None,
                user_bash_rows: None,
                edit_preview_rows: None,
                edit_legacy_rows: None,
                edit_result_rows: None,
                images: Arc::new(Vec::new()),
                image_rows: None,
            }),
            tools_expanded,
        }
    }

    /// Attach the result metadata the entry carries (details / start time /
    /// edit preview); called once per rebuild from the chat entry.
    pub fn set_meta(
        &self,
        details: Option<Arc<serde_json::Value>>,
        started_at: Option<std::time::Instant>,
        preview: Option<super::app::ToolPreview>,
    ) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.details = details;
        state.started_at = started_at;
        state.preview = preview;
    }

    /// Upstream `updateResult`: the block leaves the pending phase; the
    /// background flips to success/error and the duration is recorded.
    pub fn update_result(&self, output: Arc<str>, is_error: bool, duration_ms: Option<u64>) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.output = output;
        state.is_error = is_error;
        state.duration_ms = duration_ms;
        state.pending = false;
    }

    /// Attach the result's image content blocks (upstream
    /// `ToolExecutionComponent.updateDisplay`'s `imageBlocks`). Called by the
    /// entry rebuild/patch paths; an empty vector is a no-op that clears the
    /// cached rows.
    pub fn set_images(&self, images: Arc<Vec<ImageContent>>) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let unchanged = Arc::ptr_eq(&state.images, &images);
        if !unchanged {
            state.images = images;
            state.image_rows = None;
        }
    }

    fn phase(&self, state: &ToolState) -> ToolPhase {
        if state.pending {
            ToolPhase::Pending
        } else if state.is_error {
            ToolPhase::Error
        } else {
            ToolPhase::Success
        }
    }

    fn bg_color(phase: ToolPhase) -> ThemeColor {
        match phase {
            ToolPhase::Pending => ThemeColor::ToolPendingBg,
            ToolPhase::Success => ThemeColor::ToolSuccessBg,
            ToolPhase::Error => ThemeColor::ToolErrorBg,
        }
    }
}

impl Component for ToolExecutionComponent {
    /// The trait's owned-line form: the same rows, built through
    /// [`ToolExecutionComponent::render_into`] in a fresh pool. The frame
    /// renderer drives the canvas path; this bridge keeps `render` correct
    /// for any caller that asks for lines (tests, alternate drivers).
    fn render(&self, width: usize) -> Vec<String> {
        let mut lines = Vec::new();
        {
            let mut rows = Rows::new(&mut lines);
            self.render_into(width, &mut rows);
            rows.trim();
        }
        lines
    }

    /// Canvas-direct render: every box/spacer row is written into the frame's
    /// recycled row buffers and padded/painted IN PLACE. The old `render`
    /// path allocated, per frame, the block's line `Vec` plus one padded
    /// `String` per row, and the frame renderer then copied each of those
    /// into a canvas row and dropped it. Rows stay byte-identical (same
    /// order, same padding, same background escapes).
    fn render_into(&self, width: usize, out: &mut Rows<'_>) {
        let expanded = self.tools_expanded.load(Ordering::SeqCst);
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // `!`/`!!` bash renders as its dedicated block (a separate upstream
        // component; never the tool-box shell).
        bump_memo_work!(frames);
        if is_user_bash(&state.tool_name, &state.args) {
            render_user_bash(&mut state, width, expanded, out);
            return;
        }
        // The edit tool renders its own shell (`renderShell: "self"`).
        if state.tool_name == "edit" {
            render_edit_block(&mut state, width, expanded, out);
            // Upstream appends the result's image components after the
            // component's own content (`ToolExecutionComponent.updateDisplay`
            // adds them outside the content box).
            render_tool_images(&mut state, width, out);
            return;
        }
        let bg_color = Self::bg_color(self.phase(&state));
        // One palette generation for the whole block: the background pair and
        // the memoized result rows are keyed on the theme this frame paints
        // with, so a mid-render swap repaints on the next frame instead of
        // reusing a mixed-palette entry.
        let generation = Theme::generation();
        let bg = tool_bg_edges(&mut state, bg_color, generation);
        // Box content width (Box.render → Math.max(1, width - paddingX * 2),
        // box.ts:105) — children wrap to this; each rendered line is then
        // re-padded to the full width and background-painted.
        let content_width = width.saturating_sub(2).max(1);

        // Upstream Container: Spacer(1) (unpainted) + Box(1, 1, bg)
        // (tool-execution.ts:85-99).
        out.push("");
        // Box top padding: applyBg("", width).
        push_painted_blank_row(out, &bg.open, &bg.close, width);
        // Call line (upstream `renderCall`/`createCallFallback`): wrapped by
        // the Text component to the box content width. The rows are memoized
        // per block ([`ensure_call_rows`]): an unchanged frame re-pushes them
        // instead of re-formatting and re-wrapping the line.
        ensure_call_rows(&mut state, expanded, content_width);
        if let Some(cache) = &state.call_rows {
            for visual in &cache.rows {
                push_painted_row(out, &bg.open, &bg.close, " ", visual, width);
            }
        }

        let is_shell = state.tool_name == "bash" || state.tool_name == "powershell";
        // The codemode row shows the elapsed/total time like bash rows
        // (upstream `renderers/codemode.ts` + `toolTimerText`/`startToolTimer`
        // in render-utils.ts, #10708).
        let is_timed = is_shell || state.tool_name == "codemode";
        // The result section renders when the run settled; shell tools render
        // live partial output snapshots while pending (upstream
        // `tool_execution_update` with `isPartial: true`), and a codemode row
        // shows its live `Elapsed` tail while the script runs.
        //
        // The whole section is memoized (see [`ResultRowsCache`]): it is the
        // frame's most expensive piece by far — the shell branch styles AND
        // wraps the WHOLE result body every time it runs (a per-line
        // `format!` plus grapheme-width measurement of the wrapped copy), and
        // the read/grep/find/ls/fallback branches re-scan the whole output —
        // so a frame whose block did not change now pushes the stored rows
        // instead of re-rendering them. A settled block is a guaranteed hit
        // until the theme/width/expansion changes; a pending shell block
        // misses while its output grows, exactly when re-rendering is
        // required anyway. The timer tail is not part of the key: it is a
        // single row painted after the cached section, so the 0.1 s display
        // tick never re-renders the body.
        let key = ResultRowsKey {
            output: Arc::clone(&state.output),
            details: state.details.clone(),
            expanded,
            width,
            theme_generation: generation,
            pending: state.pending,
            is_error: state.is_error,
            duration_ms: state.duration_ms,
        };
        if !state
            .result_rows
            .as_ref()
            .is_some_and(|cache| cache.key.matches(&key))
        {
            bump_memo_work!(result_rebuilds);
            let mut rows: Vec<String> = Vec::new();
            {
                let mut canvas = Rows::new(&mut rows);
                render_result_section(
                    &state,
                    is_shell,
                    is_timed,
                    expanded,
                    width,
                    &bg.open,
                    &bg.close,
                    &mut canvas,
                );
                canvas.trim();
            }
            state.result_rows = Some(ResultRowsCache { key, rows });
        }
        if let Some(cache) = &state.result_rows {
            for row in &cache.rows {
                out.push(row);
            }
        }

        // The timer row (renderers/bash.ts:104-110; renderer.ts:147-151 for
        // codemode): painted from the memoized text every frame. This is the
        // only work a 0.1 s clock tick causes — a blank spacer, one styled
        // string and its wrap — instead of the whole cached body above.
        if is_shell || state.tool_name == "codemode" {
            if let Some(tail) = cached_timer_tail(&mut state) {
                let tail = with_theme(|theme| theme.fg(ThemeColor::Muted, tail.as_ref()));
                push_painted_blank_row(out, &bg.open, &bg.close, width);
                for line in wrap_text_with_ansi(&tail, content_width) {
                    push_painted_row(out, &bg.open, &bg.close, " ", &line, width);
                }
            }
        }

        // Box bottom padding.
        push_painted_blank_row(out, &bg.open, &bg.close, width);
        render_tool_images(&mut state, width, out);
    }

    fn invalidate(&self) {}
}
/// Write the block's result-image rows into the canvas (the upstream
/// `imageSpacers`/`Image` children appended after the content box). Rows are
/// memoized on the image `Arc` identity, the render width and the terminal
/// capability generation, so a steady frame re-pushes them instead of
/// re-encoding (and possibly re-transcoding) every image payload.
fn render_tool_images(state: &mut ToolState, width: usize, out: &mut Rows<'_>) {
    if state.images.is_empty() {
        state.image_rows = None;
        return;
    }
    let capabilities_generation = super::terminal_image::capabilities_generation();
    let hit = state.image_rows.as_ref().is_some_and(|cache| {
        Arc::ptr_eq(&cache.images, &state.images)
            && cache.width == width
            && cache.capabilities_generation == capabilities_generation
    });
    if !hit {
        let rows = tool_image_lines(&state.images, width);
        state.image_rows = Some(ImageRowsCache {
            images: Arc::clone(&state.images),
            width,
            capabilities_generation,
            rows,
        });
    }
    if let Some(cache) = &state.image_rows {
        for row in &cache.rows {
            out.push(row);
        }
    }
}

/// The result section of one tool block: the rows between the call line and
/// the box bottom padding (the output body and truncation warnings), written
/// into the canvas. Split out of
/// [`ToolExecutionComponent::render_into`] so the whole section can be memoized
/// (see [`ResultRowsCache`]) — it is the frame's most expensive piece, and a
/// block whose inputs did not change re-pushes the stored rows instead of
/// re-rendering the body.
///
/// `is_shell`/`is_timed` are derived from the (fixed) tool name by the caller.
/// The `Elapsed X` / `Took X` timer row is deliberately NOT rendered here: the
/// caller appends it after the cached rows, so a clock tick rebuilds only that
/// one row (see [`cached_timer_tail`]).
// Every parameter is part of the memo key the caller derives; bundling them
// into a struct would just move the same list one line up.
#[allow(clippy::too_many_arguments)]
fn render_result_section(
    state: &ToolState,
    is_shell: bool,
    is_timed: bool,
    expanded: bool,
    width: usize,
    bg_open: &str,
    bg_close: &str,
    out: &mut Rows<'_>,
) {
    // Box content width (Box.render → Math.max(1, width - paddingX * 2),
    // box.ts:105) — the body wraps to this before the painted rows are padded
    // back to the full width.
    let content_width = width.saturating_sub(2).max(1);
    // Upstream bash.ts:104-110 appends the Elapsed/Took footer unconditionally
    // on the shell path, so a pending shell block still renders its section.
    if !state.pending || is_timed {
        if is_shell {
            // `rebuildBashResultRenderComponent` (renderers/bash.ts:38-110)
            // — output trimmed; a finalized truncated result's duplicated
            // footer is stripped (the warning line carries it instead);
            // collapsed keeps the last BASH_PREVIEW_LINES visual lines
            // (`keep: "end"`).
            let output = strip_shell_footer(state.output.trim(), state.details.as_deref());
            if !output.is_empty() {
                // Styled in one pass into a single buffer: the old
                // `map(...).collect::<Vec<_>>().join("\n")` built a
                // `Vec<String>` of styled lines plus a second full copy of
                // the output for the join, on every frame.
                let mut styled = String::with_capacity(output.len());
                for (index, line) in output.lines().enumerate() {
                    if index > 0 {
                        styled.push('\n');
                    }
                    styled.push_str(&with_theme(|theme| theme.fg(ThemeColor::ToolOutput, line)));
                }
                let visual = wrap_text_with_ansi(&styled, content_width);
                if expanded {
                    // `new Text(`\n${styledOutput}`)` — blank line + all output.
                    // Box spacer rows are padded to the full width before the
                    // background applies (upstream Box.render → applyBg("", width)).
                    push_painted_blank_row(out, bg_open, bg_close, width);
                    for line in &visual {
                        push_painted_row(out, bg_open, bg_close, " ", line, width);
                    }
                } else {
                    // Spacer(1) + VisualLinePreview { keep: "end" } — the
                    // hint precedes the kept lines. The truncation runs
                    // against the wrap above (`truncate_to_visual_lines`
                    // re-wrapped `styled` and copied the kept tail into a
                    // fresh `Vec`): the collapsed branch used to wrap the
                    // whole shell output twice per frame and drop the first
                    // result.
                    push_painted_blank_row(out, bg_open, bg_close, width);
                    let (kept, skipped): (&[String], usize) = if visual.len() <= BASH_PREVIEW_LINES
                    {
                        (&visual, 0)
                    } else {
                        let skipped = visual.len() - BASH_PREVIEW_LINES;
                        (&visual[skipped..], skipped)
                    };
                    if skipped > 0 {
                        let hint = with_theme(|theme| earlier_lines_hint(theme, skipped));
                        for line in wrap_text_with_ansi(&hint, content_width) {
                            push_painted_row(out, bg_open, bg_close, " ", &line, width);
                        }
                    }
                    for line in kept {
                        push_painted_row(out, bg_open, bg_close, " ", line, width);
                    }
                }
            }
            // Truncation warnings (renderers/bash.ts:85-100): the
            // `[Full output: …. Truncated: …]` line, warning-colored.
            if let Some(warning) = shell_truncation_warning(state.details.as_deref()) {
                push_painted_blank_row(out, bg_open, bg_close, width);
                for line in wrap_text_with_ansi(&warning, content_width) {
                    push_painted_row(out, bg_open, bg_close, " ", &line, width);
                }
            }
        } else {
            // Per-tool result renderer (`renderResult` of
            // renderers/{read,write,grep,find,ls}.ts) — each returns
            // its own body (with the upstream leading `\n` where the
            // renderer emits one); tools without a builtin renderer keep
            // `createResultFallback` (tool-execution.ts:146-160).
            let specialized = matches!(
                state.tool_name.as_str(),
                "read" | "write" | "grep" | "find" | "ls"
            );
            let body = if specialized {
                match state.tool_name.as_str() {
                    "read" => read_result_body(
                        &state.output,
                        expanded,
                        state.is_error,
                        state.details.as_deref(),
                        if state.is_error {
                            None
                        } else {
                            read_result_language(&state.args)
                        },
                    ),
                    "write" => write_result_body(&state.output, state.is_error),
                    "grep" => trimmed_result_body(
                        &state.output,
                        GREP_PREVIEW_LINES,
                        expanded,
                        LimitedResultKind::Grep,
                        state.details.as_deref(),
                    ),
                    "find" => trimmed_result_body(
                        &state.output,
                        FIND_PREVIEW_LINES,
                        expanded,
                        LimitedResultKind::Find,
                        state.details.as_deref(),
                    ),
                    "ls" => trimmed_result_body(
                        &state.output,
                        LS_PREVIEW_LINES,
                        expanded,
                        LimitedResultKind::Ls,
                        state.details.as_deref(),
                    ),
                    _ => None,
                }
            } else {
                None
            };
            match body {
                Some(body) => {
                    for line in wrap_text_with_ansi(&body, content_width) {
                        push_painted_row(out, bg_open, bg_close, " ", &line, width);
                    }
                }
                None if !specialized => {
                    // `createResultFallback` (tool-execution.ts:146-160):
                    // output lines in toolOutput, collapsed keeps the first
                    // FALLBACK_PREVIEW_LINES LOGICAL lines and appends the
                    // "... (N more lines, ctrl+o to expand)" hint.
                    let output = state.output.as_ref();
                    if !output.is_empty() {
                        // Line counts and the first `kept` lines off the
                        // iterator: the old `collect::<Vec<&str>>()` built
                        // a pointer array for the whole output — a result
                        // can be thousands of lines — on every frame.
                        let total = output.lines().count();
                        let max = if expanded {
                            total
                        } else {
                            FALLBACK_PREVIEW_LINES
                        };
                        let kept_len = max.min(total);
                        let remaining = total - kept_len;
                        // Same one-pass styling as the shell branch: no
                        // `Vec<String>` + `join` copy of the kept lines
                        // per frame.
                        let mut styled = String::with_capacity(
                            output
                                .lines()
                                .take(kept_len)
                                .map(|line| line.len() + 1)
                                .sum(),
                        );
                        for (index, line) in output.lines().take(kept_len).enumerate() {
                            if index > 0 {
                                styled.push('\n');
                            }
                            styled.push_str(&with_theme(|theme| {
                                theme.fg(ThemeColor::ToolOutput, line)
                            }));
                        }
                        if remaining > 0 {
                            styled.push('\n');
                            styled.push_str(&with_theme(|theme| more_lines_hint(theme, remaining)));
                        }
                        for line in wrap_text_with_ansi(&styled, content_width) {
                            push_painted_row(out, bg_open, bg_close, " ", &line, width);
                        }
                    }
                }
                None => {}
            }
        }
    }
}
