use super::inline::{InlineStyleContext, StyleFn};
use super::*;

/// The inline style context for a heading (upstream `headingStyleContext`,
/// markdown.ts:477-490): level 1 is heading+bold+underline, the rest
/// heading+bold; the context's prefix restores the style after nested spans.
fn heading_style_context(theme: &MarkdownTheme, depth: usize) -> (Vec<StyleFn>, String) {
    let styles: Vec<StyleFn> = if depth == 1 {
        vec![theme.heading, theme.bold, theme.underline]
    } else {
        vec![theme.heading, theme.bold]
    };
    let prefix = inline::style_prefix(&styles);
    (styles, prefix)
}

/// Render a heading's inline content with the heading style context.
fn render_heading_inline(
    depth: usize,
    text: &str,
    theme: &MarkdownTheme,
    options: &MarkdownOptions,
) -> String {
    let (styles, prefix) = heading_style_context(theme, depth);
    let ctx = InlineStyleContext::new(&styles, &prefix);
    render_inline_with_context(text, theme, options, false, &ctx)
}

/// The quote style context prefix (upstream `quoteStylePrefix`,
/// markdown.ts:567-575): nested spans inside a blockquote restore it.
fn quote_style_prefix(theme: &MarkdownTheme) -> String {
    inline::style_prefix(&[theme.quote, theme.italic])
}

/// The fenced block's content lines, highlighted through the
/// `MarkdownTheme` hook (upstream `theme.highlightCode`, markdown.ts:533-543).
/// The hook returns `token.text.split("\n")` lines, so empty code still
/// occupies one content line; unknown/absent languages keep `mdCodeBlock`.
fn code_block_content_lines(theme: &MarkdownTheme, lang: &str, text: &str) -> Vec<String> {
    let language = if lang.is_empty() { None } else { Some(lang) };
    (theme.highlight_code)(text, language)
        .into_iter()
        .map(|line| format!("  {line}"))
        .collect()
}

/// Whether `theme` uses the built-in [`crate::tui::syntax::highlight_code`] hook —
/// the only hook that can resume from a line offset without re-styling the
/// skipped lines (the memoized stateful scanner is reachable through
/// [`crate::tui::syntax::highlight_lines_from_with_theme`]).
pub(super) fn uses_default_highlighter(theme: &MarkdownTheme) -> bool {
    std::ptr::fn_addr_eq(
        theme.highlight_code,
        crate::tui::syntax::highlight_code as fn(&str, Option<&str>) -> Vec<String>,
    )
}

/// The fenced block's content lines from `from_line` on, highlighted through
/// the `MarkdownTheme` hook. The default hook answers through the syntax memo
/// with the WHOLE `text` as its key, so the stateful scanner (block comments,
/// triple strings, template literals) resumes at exactly the state the
/// from-scratch render would have — only the requested tail lines are styled
/// and cloned. A custom hook receives the same whole `text` the classic path
/// passes it, then its output is sliced.
pub(super) fn code_block_content_lines_from(
    theme: &MarkdownTheme,
    lang: &str,
    text: &str,
    from_line: usize,
) -> Vec<String> {
    let language = if lang.is_empty() { None } else { Some(lang) };
    let lines = if uses_default_highlighter(theme) {
        with_theme(|active| {
            crate::tui::syntax::highlight_lines_from_with_theme(
                active,
                text,
                language,
                ThemeColor::MdCodeBlock,
                from_line,
            )
        })
    } else {
        let mut lines = (theme.highlight_code)(text, language);
        if from_line >= lines.len() {
            Vec::new()
        } else {
            lines.split_off(from_line)
        }
    };
    lines.into_iter().map(|line| format!("  {line}")).collect()
}

pub(super) fn render_markdown(
    source: &str,
    width: usize,
    theme: &MarkdownTheme,
    options: &MarkdownOptions,
) -> Vec<String> {
    // Documented divergence — `defaultTextStyle` (upstream
    // `applyDefaultStyle` / `getDefaultInlineStyleContext`, markdown.ts:387-448):
    // upstream threads an optional base text style through every render, but
    // its only non-undefined caller is the compaction summary
    // (compaction-summary-message.ts:48). pir renders that path with the
    // default (unstyled) context, which is byte-equivalent to the chat path
    // where upstream passes `undefined` and `applyDefaultStyle` is a no-op.
    if source.trim().is_empty() {
        return Vec::new();
    }
    let normalized = source.replace('\t', "   ");
    let parsed = parse_blocks(&normalized, options);
    let mut lines: Vec<String> = Vec::new();
    render_block_range(
        &mut lines,
        &parsed.blocks,
        0..parsed.blocks.len(),
        width,
        theme,
        options,
    );
    lines
}

/// The block fold: render `blocks[range]` into `lines` — the exact sequence
/// [`render_markdown`] emits. The shared implementation lets the incremental
/// renderer fold a prefix and a tail separately while producing the same
/// lines a from-scratch fold would: the "next block" decisions read
/// `blocks[i + 1]` past the end of `range`, so a range that stops before the
/// final-prefix cut still sees the `Space` block at the cut (the contract
/// [`ParsedBlocks::final_boundary`] establishes).
pub(super) fn render_block_range(
    lines: &mut Vec<String>,
    blocks: &[Block],
    range: std::ops::Range<usize>,
    width: usize,
    theme: &MarkdownTheme,
    options: &MarkdownOptions,
) {
    for index in range {
        let block = &blocks[index];
        let next_is_space = blocks
            .get(index + 1)
            .is_some_and(|b| matches!(b, Block::Space));
        match block {
            Block::Space => lines.push(String::new()),
            Block::Heading { depth, text } => {
                // Level 1: heading+bold+underline; 2: heading+bold; 3+: the
                // `### ` prefix is styled and prepended (upstream rule). The
                // inline content renders under the heading style context, so
                // plain text keeps the heading style and nested spans
                // restore it (markdown.ts:477-494).
                let inline = render_heading_inline(*depth, text, theme, options);
                if *depth >= 3 {
                    let prefix = format!("{} ", "#".repeat(*depth));
                    lines.push(format!(
                        "{}{}",
                        (theme.heading)(&(theme.bold)(&prefix)),
                        inline
                    ));
                } else if *depth == 1 {
                    lines.push((theme.heading)(&(theme.bold)(&(theme.underline)(&inline))));
                } else {
                    lines.push((theme.heading)(&(theme.bold)(&inline)));
                }
                if !next_is_space && index + 1 < blocks.len() {
                    lines.push(String::new());
                }
            }
            Block::Paragraph(text) => {
                lines.push(render_inline_with_context(
                    text,
                    theme,
                    options,
                    false,
                    &InlineStyleContext::default(),
                ));
                // Paragraph spacing (markdown.ts:504-507): none when the next
                // token is a space token or a list (`!== "list"`).
                let next_is_list = blocks
                    .get(index + 1)
                    .is_some_and(|block| matches!(block, Block::List { .. }));
                if !next_is_space && !next_is_list && index + 1 < blocks.len() {
                    lines.push(String::new());
                }
            }
            Block::Code { lang, text } => {
                match mermaid_block(lang, text, width, options) {
                    MermaidDecision::Diagram(diagram) => lines.extend(diagram),
                    decision => {
                        lines.push((theme.code_block_border)(&format!("```{lang}")));
                        lines.extend(code_block_content_lines(theme, lang, text));
                        lines.push((theme.code_block_border)("```"));
                        if let MermaidDecision::RawWithWarning(Some(warning)) = decision {
                            lines.push(format!("  {}", (theme.code_block)(&warning)));
                        }
                    }
                }
                if !next_is_space && index + 1 < blocks.len() {
                    lines.push(String::new());
                }
            }
            Block::List {
                ordered,
                start,
                items,
                markers,
                tasks,
                loose,
            } => {
                render_list(
                    items, markers, tasks, *ordered, *start, *loose, 0, width, theme, options, "",
                    lines,
                );
            }
            Block::Blockquote(text) => {
                let inner_blocks = parse_blocks(text, options).blocks;
                // Nested spans inside the quote restore the quote style
                // (upstream `quoteInlineStyleContext`, markdown.ts:583-586).
                let quote_prefix = quote_style_prefix(theme);
                let mut inner_lines: Vec<String> = Vec::new();
                for inner in &inner_blocks {
                    if matches!(inner, Block::Space) {
                        continue;
                    }
                    inner_lines.extend(render_markdown_block(
                        inner,
                        width.saturating_sub(2).max(1),
                        theme,
                        options,
                        &quote_prefix,
                    ));
                }
                // Trim trailing blanks (upstream pops them before the quote border).
                while inner_lines.last().is_some_and(String::is_empty) {
                    inner_lines.pop();
                }
                for inner_line in inner_lines {
                    for wrapped in wrap_text_with_ansi(&inner_line, width.saturating_sub(2).max(1))
                    {
                        lines.push(format!(
                            "{}{}",
                            (theme.quote_border)("│ "),
                            // Upstream quote body: `theme.quote(theme.italic(text))`
                            // (markdown.ts:567).
                            (theme.quote)(&(theme.italic)(&wrapped))
                        ));
                    }
                }
                if !next_is_space && index + 1 < blocks.len() {
                    lines.push(String::new());
                }
            }
            Block::Table { header, rows, raw } => {
                lines.extend(render_table(header, rows, raw, width, theme, options, ""));
                // Table spacing (markdown.ts:1020-1022): add a blank line after
                // the table unless a space token follows (pushed for both the
                // boxed table and the too-narrow raw fallback, markdown.ts:872-874).
                if !next_is_space && index + 1 < blocks.len() {
                    lines.push(String::new());
                }
            }
            Block::Rule => {
                lines.push((theme.hr)(&"─".repeat(width.min(80))));
                if !next_is_space && index + 1 < blocks.len() {
                    lines.push(String::new());
                }
            }
            Block::Latex { text, pending, raw } => {
                // `latexBlock` (markdown.ts:515-528): pending / failed /
                // disabled math keeps the raw delimiters; otherwise render the
                // math in display mode (stacked fractions, operator limits).
                let rendered = if !pending && options.render_latex {
                    latex::render_latex(text, true).unwrap_or_else(|| raw.trim().to_string())
                } else {
                    raw.trim().to_string()
                };
                for rendered_line in rendered.split('\n') {
                    lines.push(rendered_line.to_owned());
                }
                if !next_is_space && index + 1 < blocks.len() {
                    lines.push(String::new());
                }
            }
        }
    }
}

fn render_markdown_block(
    block: &Block,
    width: usize,
    theme: &MarkdownTheme,
    options: &MarkdownOptions,
    inline_prefix: &str,
) -> Vec<String> {
    render_markdown_block_one(block, width, theme, options, inline_prefix)
}

fn render_markdown_block_one(
    block: &Block,
    width: usize,
    theme: &MarkdownTheme,
    options: &MarkdownOptions,
    inline_prefix: &str,
) -> Vec<String> {
    match block {
        Block::Space => vec![String::new()],
        Block::Heading { depth, text } => {
            // Heading style context (markdown.ts:477-494): nested spans
            // restore the heading style; the incoming quote prefix does not
            // apply (upstream renderToken builds its own context).
            let inline = render_heading_inline(*depth, text, theme, options);
            if *depth >= 3 {
                vec![format!(
                    "{}{}",
                    (theme.heading)(&(theme.bold)(&format!("{} ", "#".repeat(*depth)))),
                    inline
                )]
            } else if *depth == 1 {
                vec![(theme.heading)(&(theme.bold)(&(theme.underline)(&inline)))]
            } else {
                vec![(theme.heading)(&(theme.bold)(&inline))]
            }
        }
        Block::Paragraph(text) => {
            let ctx = InlineStyleContext::new(&[], inline_prefix);
            vec![render_inline_with_context(
                text, theme, options, false, &ctx,
            )]
        }
        Block::Code { lang, text } => {
            match mermaid_block(lang, text, width, options) {
                MermaidDecision::Diagram(diagram) => return diagram,
                MermaidDecision::RawWithWarning(warning) => {
                    let mut lines = vec![(theme.code_block_border)(&format!("```{lang}"))];
                    lines.extend(code_block_content_lines(theme, lang, text));
                    lines.push((theme.code_block_border)("```"));
                    if let Some(warning) = warning {
                        lines.push(format!("  {}", (theme.code_block)(&warning)));
                    }
                    return lines;
                }
                MermaidDecision::Raw => {}
            }
            let mut lines = vec![(theme.code_block_border)(&format!("```{lang}"))];
            lines.extend(code_block_content_lines(theme, lang, text));
            lines.push((theme.code_block_border)("```"));
            lines
        }
        Block::List {
            items,
            markers,
            tasks,
            ordered,
            start,
            loose,
            ..
        } => {
            let mut lines = Vec::new();
            render_list(
                items,
                markers,
                tasks,
                *ordered,
                *start,
                *loose,
                0,
                width,
                theme,
                options,
                inline_prefix,
                &mut lines,
            );
            lines
        }
        Block::Blockquote(text) => {
            let inner = parse_blocks(text, options).blocks;
            // Nested spans inside the quote restore the quote style
            // (upstream `quoteInlineStyleContext`, markdown.ts:583-586).
            let quote_prefix = quote_style_prefix(theme);
            let mut lines = Vec::new();
            for block in &inner {
                if matches!(block, Block::Space) {
                    continue;
                }
                for quote_line in render_markdown_block_one(
                    block,
                    width.saturating_sub(2).max(1),
                    theme,
                    options,
                    &quote_prefix,
                ) {
                    for wrapped in wrap_text_with_ansi(&quote_line, width.saturating_sub(2).max(1))
                    {
                        lines.push(format!(
                            "{}{}",
                            (theme.quote_border)("│ "),
                            // Upstream: `theme.quote(theme.italic(text))`
                            // (markdown.ts:567).
                            (theme.quote)(&(theme.italic)(&wrapped))
                        ));
                    }
                }
            }
            lines
        }
        Block::Table { header, rows, raw } => {
            render_table(header, rows, raw, width, theme, options, inline_prefix)
        }
        Block::Rule => vec![(theme.hr)(&"─".repeat(width.min(80)))],
        Block::Latex { text, pending, raw } => {
            // `latexBlock` (markdown.ts:515-528) — see `render_markdown`.
            let rendered = if !pending && options.render_latex {
                latex::render_latex(text, true).unwrap_or_else(|| raw.trim().to_string())
            } else {
                raw.trim().to_string()
            };
            rendered.split('\n').map(str::to_string).collect()
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn render_list(
    items: &[String],
    markers: &[String],
    tasks: &[Option<bool>],
    ordered: bool,
    start: u64,
    loose: bool,
    depth: usize,
    width: usize,
    theme: &MarkdownTheme,
    options: &MarkdownOptions,
    inline_prefix: &str,
    lines: &mut Vec<String>,
) {
    let indent = "    ".repeat(depth);
    for (index, item) in items.iter().enumerate() {
        // Upstream bullet selection (markdown.ts:775-781): with
        // `preserveOrderedListMarkers` the raw source marker (`1)`, `*`, the
        // literal number) is kept, otherwise it normalizes to `N. ` / `- `.
        let bullet = if ordered {
            if options.preserve_ordered_list_markers {
                markers
                    .get(index)
                    .map(|marker| format!("{marker} "))
                    .unwrap_or_else(|| format!("{}. ", start + index as u64))
            } else {
                format!("{}. ", start + index as u64)
            }
        } else if options.preserve_ordered_list_markers {
            markers
                .get(index)
                .map(|marker| format!("{marker} "))
                .unwrap_or_else(|| "- ".to_string())
        } else {
            "- ".to_string()
        };
        // GFM task-list checkbox folded into the marker (markdown.ts:782-783):
        // `[x] `/`[ ] ` for task items, nothing otherwise.
        let task_marker = match tasks.get(index) {
            Some(Some(true)) => "[x] ",
            Some(Some(false)) => "[ ] ",
            _ => "",
        };
        // The theme styles the combined marker; the continuation prefix is
        // its visible width (markdown.ts:784-785).
        let marker = format!("{bullet}{task_marker}");
        let first_prefix = format!("{indent}{}", (theme.list_bullet)(&marker));
        let continuation_prefix = format!("{indent}{}", " ".repeat(visible_width(&marker)));
        let item_width = width.saturating_sub(visible_width(&first_prefix)).max(1);
        let item_blocks = parse_blocks(item, options).blocks;
        let mut rendered_any = false;
        for item_block in &item_blocks {
            if matches!(item_block, Block::Space) {
                continue;
            }
            if let Block::List {
                items: nested,
                markers: nested_markers,
                tasks: nested_tasks,
                ordered: nested_ordered,
                start: nested_start,
                loose: nested_loose,
                ..
            } = item_block
            {
                render_list(
                    nested,
                    nested_markers,
                    nested_tasks,
                    *nested_ordered,
                    *nested_start,
                    *nested_loose,
                    depth + 1,
                    width,
                    theme,
                    options,
                    inline_prefix,
                    lines,
                );
                rendered_any = true;
                continue;
            }
            for line in
                render_markdown_block_one(item_block, item_width, theme, options, inline_prefix)
            {
                for wrapped in wrap_text_with_ansi(&line, item_width) {
                    if rendered_any {
                        lines.push(format!("{continuation_prefix}{wrapped}"));
                    } else {
                        lines.push(format!("{first_prefix}{wrapped}"));
                    }
                    rendered_any = true;
                }
            }
        }
        if !rendered_any {
            lines.push(first_prefix);
        }
        // Loose lists interleave a blank line after every non-final item
        // (upstream `renderList`, markdown.ts:810-812).
        if loose && index + 1 < items.len() {
            lines.push(String::new());
        }
    }
}

/// Upstream `wrapCellText` (markdown.ts:839-846): wrap a table cell, then on
/// every non-final fragment append the text-style reset
/// `\x1b[22;23;24;25;27;28;29;39m` and re-append the surrounding
/// `stylePrefix`, so a bold/em span that wraps inside the cell closes on the
/// fragment and the outer style is restored before padding and borders (the
/// reset and prefix have zero visible width).
fn wrap_cell_text(text: &str, max_width: usize, style_prefix: &str) -> Vec<String> {
    let lines = wrap_text_with_ansi(text, max_width.max(1));
    let last_index = lines.len().saturating_sub(1);
    lines
        .into_iter()
        .enumerate()
        .map(|(index, line)| {
            let style_reset = if index < last_index {
                "\x1b[22;23;24;25;27;28;29;39m"
            } else {
                ""
            };
            format!("{line}{style_reset}{style_prefix}")
        })
        .collect()
}

/// Upstream table geometry: `┌─…─┬─…─┐` header (bold), `├─┼─┤` separator
/// between rows, `└─┴─┘` bottom; natural widths shrink/grow to fit. When the
/// table cannot fit stably the raw markdown source is width-wrapped instead
/// (markdown.ts:869-876).
fn render_table(
    header: &[String],
    rows: &[Vec<String>],
    raw: &str,
    available_width: usize,
    theme: &MarkdownTheme,
    options: &MarkdownOptions,
    inline_prefix: &str,
) -> Vec<String> {
    let num_cols = header.len();
    if num_cols == 0 {
        return Vec::new();
    }
    let border_overhead = 3 * num_cols + 1;
    let available_for_cells = available_width.saturating_sub(border_overhead);
    if available_for_cells < num_cols {
        // Too narrow to render a stable table. Fall back to the raw markdown
        // (upstream `wrapTextWithAnsi(token.raw, availableWidth)`); the
        // trailing spacing line is added by the caller, exactly as for the
        // boxed path (markdown.ts:872-874, :1020-1022).
        if raw.is_empty() {
            return Vec::new();
        }
        return wrap_text_with_ansi(raw, available_width);
    }

    let ctx = InlineStyleContext::new(&[], inline_prefix);
    let cell_text = |row: &[String], col: usize| -> String {
        row.get(col)
            .map(|c| render_inline_with_context(c, theme, options, false, &ctx))
            .unwrap_or_default()
    };

    let mut natural_widths = vec![0usize; num_cols];
    for (col, width) in natural_widths.iter_mut().enumerate() {
        *width = (*width).max(visible_width(&cell_text(header, col)));
    }
    for row in rows {
        for (col, width) in natural_widths.iter_mut().enumerate() {
            *width = (*width).max(visible_width(&cell_text(row, col)));
        }
    }
    // Minimum = longest word (clamped to 30, upstream maxUnbrokenWordWidth).
    let min_word_widths: Vec<usize> = (0..num_cols)
        .map(|col| {
            let mut longest = 1usize;
            for text in std::iter::once(cell_text(header, col))
                .chain(rows.iter().map(|r| cell_text(r, col)))
            {
                for word in text.split_whitespace() {
                    longest = longest.max(visible_width(word).min(30));
                }
            }
            longest
        })
        .collect();

    let mut column_widths = natural_widths.clone();
    let total_natural = natural_widths.iter().sum::<usize>() + border_overhead;
    if total_natural > available_width {
        let mut min_widths = min_word_widths.clone();
        let min_cells = min_widths.iter().sum::<usize>();
        if min_cells > available_for_cells {
            // Distribute by weight (upstream growth split).
            min_widths = vec![1; num_cols];
            let remaining = available_for_cells.saturating_sub(num_cols);
            if remaining > 0 {
                let total_weight: usize = min_word_widths.iter().map(|w| w.saturating_sub(1)).sum();
                let mut allocated = 0usize;
                for (i, width) in min_word_widths.iter().enumerate() {
                    let weight = width.saturating_sub(1);
                    let grow = (weight * remaining).checked_div(total_weight).unwrap_or(0);
                    min_widths[i] += grow;
                    allocated += grow;
                }
                let mut leftover = remaining.saturating_sub(allocated);
                let mut i = 0;
                while leftover > 0 && num_cols > 0 {
                    min_widths[i % num_cols] += 1;
                    leftover -= 1;
                    i += 1;
                }
            }
        }
        column_widths = min_widths;
        // Grow toward natural widths within the budget.
        let allocated = column_widths.iter().sum::<usize>();
        let mut remaining = available_for_cells.saturating_sub(allocated);
        let mut grew = true;
        while remaining > 0 && grew {
            grew = false;
            for col in 0..num_cols {
                if remaining == 0 {
                    break;
                }
                if column_widths[col] < natural_widths[col] {
                    column_widths[col] += 1;
                    remaining -= 1;
                    grew = true;
                }
            }
        }
    }

    let repeat = |w: usize| "─".repeat(w);
    let join = |cells: &[usize], mid: &str| -> String {
        cells
            .iter()
            .map(|w| repeat(*w))
            .collect::<Vec<_>>()
            .join(mid)
    };
    let mut lines = Vec::new();
    lines.push(format!("┌─{}─┐", join(&column_widths, "─┬─")));

    let render_row = |cells: &[String], bold_header: bool| -> Vec<String> {
        let wrapped: Vec<Vec<String>> = cells
            .iter()
            .enumerate()
            .map(|(col, text)| wrap_cell_text(text, column_widths[col], inline_prefix))
            .collect();
        let height = wrapped.iter().map(Vec::len).max().unwrap_or(1);
        let mut out = Vec::new();
        for line_idx in 0..height {
            let parts: Vec<String> = wrapped
                .iter()
                .enumerate()
                .map(|(col, cell_lines)| {
                    let text = cell_lines.get(line_idx).cloned().unwrap_or_default();
                    pad_to_width(&text, column_widths[col])
                })
                .collect();
            let joined = parts.join(" │ ");
            if bold_header {
                out.push(format!("│ {} │", (theme.bold)(&joined)));
            } else {
                out.push(format!("│ {joined} │"));
            }
        }
        out
    };

    let header_rendered: Vec<String> = header
        .iter()
        .map(|h| render_inline_with_context(h, theme, options, false, &ctx))
        .collect();
    lines.extend(render_row(&header_rendered, true));
    let separator = format!("├─{}─┤", join(&column_widths, "─┼─"));
    lines.push(separator.clone());
    for (row_index, row) in rows.iter().enumerate() {
        let rendered: Vec<String> = row
            .iter()
            .map(|cell| render_inline_with_context(cell, theme, options, false, &ctx))
            .collect();
        lines.extend(render_row(&rendered, false));
        if row_index + 1 < rows.len() {
            lines.push(separator.clone());
        }
    }
    lines.push(format!("└─{}─┘", join(&column_widths, "─┴─")));
    lines
}

// ---------------------------------------------------------------------------
// Inline rendering
// ---------------------------------------------------------------------------
