use super::*;

// ---------------------------------------------------------------------------
// Block parser
// ---------------------------------------------------------------------------

/// A parsed source plus the line geometry the incremental renderer needs
/// ([`ParsedBlocks::final_boundary`]).
pub(super) struct ParsedBlocks {
    /// The block sequence, in source order.
    pub(super) blocks: Vec<Block>,
    /// First source line (index into `source.lines()`) of each block,
    /// parallel to `blocks`.
    pub(super) block_starts: Vec<usize>,
    /// Byte offset of every `source.lines()` line start, plus `source.len()`
    /// as the final entry (`line_starts[lines.len()] == source.len()`).
    pub(super) line_starts: Vec<usize>,
    /// Whether `source` ends with a newline: its last line is terminated, so
    /// an append cannot rewrite it.
    pub(super) ends_with_newline: bool,
}

impl ParsedBlocks {
    /// The offset up to which this parse is FINAL: text appended later can
    /// never change the parse — or the rendered lines — of `source[..offset]`.
    ///
    /// The offset is the start of a blank line that the parse turned into a
    /// `Space` block and whose preceding block cannot grow across the blank:
    ///
    /// - a `List` absorbs a blank line when a list marker follows it
    ///   (`saw_blank_between`, which also flips the whole list loose), so a
    ///   blank that could still see a marker is NOT final. Only the line
    ///   immediately after the blank is consulted, so the blank is final once
    ///   that line exists and cannot change (any terminated line, or another
    ///   blank line);
    /// - every other block is terminated by the blank line for good: a
    ///   paragraph is flushed, the table/quote/list row loops stop, a closed
    ///   fence or latex token has already consumed its own last line, and no
    ///   append can reach back over a terminated block;
    /// - the block after the offset is the `Space` block itself, so the fold
    ///   renders the last final block exactly as the from-scratch fold does
    ///   (its `next` block is — permanently — a space).
    ///
    /// The largest such cut wins; `None` when nothing is final yet (an empty
    /// source, only blank lines, or a single still-open block).
    pub(super) fn final_boundary(&self) -> Option<(usize, usize)> {
        let line_count = self.line_starts.len() - 1;
        for index in (1..self.blocks.len()).rev() {
            if !matches!(self.blocks[index], Block::Space) {
                continue;
            }
            if matches!(self.blocks[index - 1], Block::List { .. }) {
                let next = self.block_starts[index] + 1;
                // The line after the blank must exist and be immutable: every
                // line but a source-final line without a trailing newline is.
                let locked = next < line_count && (next + 1 < line_count || self.ends_with_newline);
                if !locked {
                    continue;
                }
            }
            return Some((index, self.line_starts[self.block_starts[index]]));
        }
        None
    }
}

/// Byte offset of every `source.lines()` line start, plus `source.len()` as
/// the final entry. `str::lines` strips one trailing `\r` per line, so the
/// separator after the content is `\r\n`, `\n`, or nothing for a source-final
/// unterminated line.
fn line_start_offsets(source: &str) -> Vec<usize> {
    let mut starts = Vec::new();
    let mut position = 0usize;
    for line in source.lines() {
        starts.push(position);
        position += line.len();
        if source.as_bytes().get(position) == Some(&b'\r') {
            position += 1;
        }
        if source.as_bytes().get(position) == Some(&b'\n') {
            position += 1;
        }
    }
    starts.push(position);
    starts
}

pub(super) fn parse_blocks(source: &str, options: &MarkdownOptions) -> ParsedBlocks {
    let mut blocks: Vec<Block> = Vec::new();
    let mut block_starts: Vec<usize> = Vec::new();
    let lines: Vec<&str> = source.lines().collect();
    let line_starts = line_start_offsets(source);
    let mut i = 0usize;
    let mut paragraph: Vec<String> = Vec::new();
    // First line of the paragraph being accumulated; only meaningful while
    // `paragraph` is non-empty (the flush below records it).
    let mut paragraph_start = 0usize;

    let flush_paragraph = |paragraph: &mut Vec<String>,
                           blocks: &mut Vec<Block>,
                           block_starts: &mut Vec<usize>,
                           start: usize| {
        if !paragraph.is_empty() {
            block_starts.push(start);
            blocks.push(Block::Paragraph(paragraph.join("\n")));
            paragraph.clear();
        }
    };

    while i < lines.len() {
        let line = lines[i];
        let trimmed = line.trim_start();

        if trimmed.is_empty() {
            flush_paragraph(
                &mut paragraph,
                &mut blocks,
                &mut block_starts,
                paragraph_start,
            );
            block_starts.push(i);
            blocks.push(Block::Space);
            i += 1;
            continue;
        }

        // Block LaTeX (`latexBlock` extension, markdown.ts:101-144). The
        // extension can interrupt a paragraph, so a `$$`/`\[` line (up to
        // three leading spaces, upstream `^ {0,3}`) gets first refusal at the
        // block level; a failed tokenizer falls through to the other rules.
        let spaced = line.trim_start_matches(' ');
        if (spaced.starts_with("$$") || spaced.starts_with("\\["))
            && line.chars().count() - spaced.chars().count() <= 3
        {
            let rest: String = lines[i..].join("\n");
            if let Some(block) = scan_block_latex(&rest) {
                flush_paragraph(
                    &mut paragraph,
                    &mut blocks,
                    &mut block_starts,
                    paragraph_start,
                );
                // `raw` covers the matched source: whole lines (the regex
                // eats the closing newline when present).
                let newlines = block.raw.matches('\n').count();
                let consumed = if block.raw.ends_with('\n') {
                    newlines
                } else {
                    newlines + 1
                };
                block_starts.push(i);
                i += consumed.max(1);
                blocks.push(Block::Latex {
                    text: block.text,
                    pending: block.pending,
                    raw: block.raw,
                });
                continue;
            }
        }

        // Fenced code block (backtick or tilde fence, upstream fence rule).
        if let Some((fence_char, fence_len, info)) = parse_fence_opener(trimmed) {
            flush_paragraph(
                &mut paragraph,
                &mut blocks,
                &mut block_starts,
                paragraph_start,
            );
            let block_line = i;
            let lang = info.trim().to_string();
            let mut code: Vec<&str> = Vec::new();
            i += 1;
            let mut closed = false;
            while i < lines.len() {
                if is_fence_closer(lines[i], fence_char, fence_len) {
                    closed = true;
                    i += 1;
                    break;
                }
                code.push(lines[i]);
                i += 1;
            }
            if !closed && options.trim_partial_closing_fences {
                // `trimPartialClosingFences` (markdown.ts:146-169): while the
                // closing fence streams in, the token's final line holds a
                // partial fence — 1..marker_len-1 repeats of the opening
                // marker character. Drop it (with its newline) so the code
                // block keeps its height when the fence completes. Upstream's
                // marker regex runs on the raw token, so an indented opener
                // is never trimmed (markdown.ts:162).
                let unindented = line.len() == trimmed.len();
                if unindented {
                    if let Some(last) = code.last() {
                        let chars_in_last = last.chars().count();
                        if chars_in_last > 0
                            && chars_in_last < fence_len
                            && last.chars().all(|ch| ch == fence_char)
                        {
                            code.pop();
                        }
                    }
                }
            }
            block_starts.push(block_line);
            blocks.push(Block::Code {
                lang,
                text: code.join("\n"),
            });
            continue;
        }

        // ATX heading.
        if let Some(rest) = trimmed.strip_prefix('#') {
            let depth = rest.chars().take_while(|&c| c == '#').count() + 1;
            if depth <= 6 && rest[depth - 1..].starts_with(' ') {
                flush_paragraph(
                    &mut paragraph,
                    &mut blocks,
                    &mut block_starts,
                    paragraph_start,
                );
                // marked `heading` (Tokenizer.ts): trim the line, then strip a
                // trailing `#` run only when it is preceded by a space/tab or
                // the text is nothing but `#`s — `# C#` keeps its hash.
                let mut text = rest[depth..].trim().to_string();
                if text.ends_with('#') {
                    let trimmed_hashes = text.trim_end_matches('#');
                    if trimmed_hashes.is_empty()
                        || trimmed_hashes.ends_with(' ')
                        || trimmed_hashes.ends_with('\t')
                    {
                        text = trimmed_hashes.trim().to_string();
                    }
                }
                blocks.push(Block::Heading { depth, text });
                block_starts.push(i);
                i += 1;
                continue;
            }
        }

        // Thematic break.
        if is_thematic_break(trimmed) {
            flush_paragraph(
                &mut paragraph,
                &mut blocks,
                &mut block_starts,
                paragraph_start,
            );
            block_starts.push(i);
            blocks.push(Block::Rule);
            i += 1;
            continue;
        }

        // Blockquote.
        if trimmed.starts_with('>') {
            flush_paragraph(
                &mut paragraph,
                &mut blocks,
                &mut block_starts,
                paragraph_start,
            );
            let block_line = i;
            let mut quote: Vec<String> = Vec::new();
            while i < lines.len() && lines[i].trim_start().starts_with('>') {
                let content = lines[i]
                    .trim_start()
                    .strip_prefix('>')
                    .unwrap_or("")
                    .strip_prefix(' ')
                    .unwrap_or_else(|| lines[i].trim_start().strip_prefix('>').unwrap_or(""));
                quote.push(content.to_string());
                i += 1;
            }
            block_starts.push(block_line);
            blocks.push(Block::Blockquote(quote.join("\n")));
            continue;
        }

        // Table: a header row with `|` followed by a delimiter row.
        if trimmed.contains('|') && i + 1 < lines.len() && is_table_delimiter(lines[i + 1]) {
            flush_paragraph(
                &mut paragraph,
                &mut blocks,
                &mut block_starts,
                paragraph_start,
            );
            let table_start = i;
            let header = split_table_row(trimmed);
            let num_cols = header.len();
            let mut rows: Vec<Vec<String>> = Vec::new();
            i += 2;
            while i < lines.len() {
                let row_line = lines[i].trim();
                if row_line.is_empty() || !row_line.contains('|') {
                    break;
                }
                let mut cells = split_table_row(row_line);
                cells.resize(num_cols, String::new());
                rows.push(cells);
                i += 1;
            }
            // Raw source lines for the too-narrow fallback (upstream
            // `token.raw`, markdown.ts:871 — marked keeps the exact source
            // lines, trailing newlines excluded).
            let raw = lines[table_start..i].join("\n");
            block_starts.push(table_start);
            blocks.push(Block::Table { header, rows, raw });
            continue;
        }

        // List item.
        if let Some((ordered, _marker_len, start_num)) = parse_list_marker(trimmed) {
            flush_paragraph(
                &mut paragraph,
                &mut blocks,
                &mut block_starts,
                paragraph_start,
            );
            let block_line = i;
            let mut items: Vec<String> = Vec::new();
            // Raw marker per item (`"1)"`, `"1."`, `"-"`, …) for the
            // `preserve_ordered_list_markers` path (upstream re-reads the raw
            // item prefix, markdown.ts:753-761). The loop below pushes one
            // marker per item, including the first line.
            let mut markers: Vec<String> = Vec::new();
            // GFM task state per item (marked `item.task`/`item.checked`).
            let mut tasks: Vec<Option<bool>> = Vec::new();
            let mut loose = false;
            let mut saw_blank_between = false;
            while i < lines.len() {
                let current = lines[i];
                let current_trimmed = current.trim_start();
                if current_trimmed.is_empty() {
                    // Blank line: possible loose separation or end of list.
                    if i + 1 < lines.len() && parse_list_marker(lines[i + 1].trim_start()).is_some()
                    {
                        saw_blank_between = true;
                        i += 1;
                        continue;
                    }
                    break;
                }
                match parse_list_marker(current_trimmed) {
                    Some((_, marker_len_nested, _)) => {
                        if saw_blank_between {
                            loose = true;
                            saw_blank_between = false;
                        }
                        markers.push(current_trimmed[..marker_len_nested - 1].to_string());
                        let content = current_trimmed[marker_len_nested..].trim().to_string();
                        // GFM task list (marked's listItem rule): an item
                        // starting with `[x]`/`[X]`/`[ ]` plus whitespace is
                        // a task item; the checkbox is removed from the text
                        // (marked `item.task`/`item.checked`).
                        let (task, content) = parse_task_marker(&content);
                        tasks.push(task);
                        items.push(content);
                        i += 1;
                        // Continuation lines (indented under the item).
                        while i < lines.len()
                            && !lines[i].trim_start().is_empty()
                            && parse_list_marker(lines[i].trim_start()).is_none()
                            && (lines[i].starts_with("    ") || lines[i].starts_with("\t"))
                        {
                            if let Some(last) = items.last_mut() {
                                last.push('\n');
                                last.push_str(lines[i].trim());
                            }
                            i += 1;
                        }
                    }
                    None => break,
                }
            }
            block_starts.push(block_line);
            blocks.push(Block::List {
                ordered,
                start: start_num,
                items,
                markers,
                tasks,
                loose,
            });
            continue;
        }

        // Paragraph continuation.
        if paragraph.is_empty() {
            paragraph_start = i;
        }
        paragraph.push(trimmed.to_string());
        i += 1;
    }
    flush_paragraph(
        &mut paragraph,
        &mut blocks,
        &mut block_starts,
        paragraph_start,
    );
    ParsedBlocks {
        blocks,
        block_starts,
        line_starts,
        ends_with_newline: source.ends_with('\n'),
    }
}

fn is_thematic_break(line: &str) -> bool {
    let compact: String = line.chars().filter(|c| !c.is_whitespace()).collect();
    (compact.starts_with("***") && compact.chars().all(|c| c == '*'))
        || (compact.starts_with("---") && compact.chars().all(|c| c == '-'))
        || (compact.starts_with("___") && compact.chars().all(|c| c == '_'))
}

fn is_table_delimiter(line: &str) -> bool {
    let trimmed = line.trim();
    if !trimmed.contains('-') || !trimmed.contains('|') {
        return false;
    }
    trimmed.chars().all(|c| matches!(c, '|' | '-' | ':' | ' '))
}

fn split_table_row(line: &str) -> Vec<String> {
    let trimmed = line.trim();
    let inner = trimmed.strip_prefix('|').unwrap_or(trimmed);
    let inner = inner.strip_suffix('|').unwrap_or(inner);
    inner
        .split('|')
        .map(|cell| cell.trim().to_string())
        .collect()
}

/// GFM task-list marker (marked's listItem rule): `[x]`/`[X]`/`[ ]` followed
/// by whitespace at the item start. Returns `(task, remaining_text)` —
/// `None` when the item is not a task.
fn parse_task_marker(content: &str) -> (Option<bool>, String) {
    let bytes = content.as_bytes();
    if bytes.first() != Some(&b'[') || bytes.get(2) != Some(&b']') {
        return (None, content.to_string());
    }
    let checked = match bytes[1] {
        b'x' | b'X' => true,
        b' ' => false,
        _ => return (None, content.to_string()),
    };
    // A whitespace must follow the checkbox (GFM: `[x]` must be followed by
    // whitespace to form a task item).
    if !content[3..].starts_with(|c: char| c.is_whitespace()) {
        return (None, content.to_string());
    }
    (Some(checked), content[3..].trim_start().to_string())
}

/// Parse a list marker at the start of `line`. Returns
/// `(ordered, marker_len, start_number)`.
fn parse_list_marker(line: &str) -> Option<(bool, usize, u64)> {
    // Unordered: `-`, `*`, `+` followed by space.
    let first = line.chars().next()?;
    if matches!(first, '-' | '*' | '+') && line[1..].starts_with(' ') {
        return Some((false, 2, 1));
    }
    // Ordered: digits + `.` or `)` + space.
    let digits: usize = line.chars().take_while(char::is_ascii_digit).count();
    if digits > 0 {
        let after = &line[digits..];
        if after.starts_with(". ") || after.starts_with(") ") {
            let start = line[..digits].parse::<u64>().unwrap_or(1);
            return Some((true, digits + 2, start));
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Fence helpers (upstream marked fence rule / `trimPartialClosingFences`)
// ---------------------------------------------------------------------------

/// Upstream fence opener `^ {0,3}(`{3,}|~{3,})([^\n]*)` (marked's fence
/// rule). `trimmed` is the line with leading whitespace removed; pir keeps
/// its looser indent rule. Returns `(fence_char, run_len, info_string)`.
fn parse_fence_opener(trimmed: &str) -> Option<(char, usize, String)> {
    let fence_char = trimmed.chars().next()?;
    if fence_char != '`' && fence_char != '~' {
        return None;
    }
    let run = trimmed.chars().take_while(|&c| c == fence_char).count();
    if run < 3 {
        return None;
    }
    let info: String = trimmed.chars().skip(run).collect();
    Some((fence_char, run, info))
}

/// Upstream closing fence ` {0,3}\1[~`]* *(?=\n|$)` (marked's fence rule):
/// the opener's marker run followed only by fence characters and spaces.
fn is_fence_closer(line: &str, fence_char: char, fence_len: usize) -> bool {
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

// ---------------------------------------------------------------------------
// LaTeX scanning (marked extension emulation; the engine lives in `mod latex`)
// ---------------------------------------------------------------------------
