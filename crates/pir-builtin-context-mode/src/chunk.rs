//! Document chunking for the context-mode index.
//!
//! Markdown is split at ATX headings; fenced code blocks are opaque (a `#`
//! line inside a fence never starts a section, and fences are never split
//! across chunks). Sections larger than ~4 KiB are split at blank lines
//! outside fences, keeping the heading title with a `(part N)` suffix. Other
//! content is split at blank lines around ~2 KiB. Every chunk records its
//! 1-based line range.

use crate::truncate_chars;

/// One parsed chunk (title + content + 1-based line range).
#[derive(Debug, Clone)]
pub struct ParsedChunk {
    pub title: String,
    pub content: String,
    pub line_start: usize,
    pub line_end: usize,
}

/// A document destined for the index: source label, content type, chunks.
#[derive(Debug, Clone)]
pub struct IndexDoc {
    pub source: String,
    pub content_type: String,
    pub chunks: Vec<ParsedChunk>,
    /// Session that produced this content (`ctx_search(scope:"session")`).
    pub session_id: String,
    /// Worktree root that produced it (`ctx_search(scope:"worktree")`).
    pub worktree: String,
}

impl IndexDoc {
    /// Chunk `text` for `source` and wrap it as a document.
    pub fn from_text(source: String, content_type: String, text: &str) -> Self {
        let chunks = chunk_document(&source, &content_type, text);
        Self {
            source,
            content_type,
            chunks,
            session_id: String::new(),
            worktree: String::new(),
        }
    }
}

/// Split `content` according to `content_type` (`markdown` by headings,
/// everything else by blank lines/size).
pub fn chunk_document(source: &str, content_type: &str, content: &str) -> Vec<ParsedChunk> {
    if content_type.eq_ignore_ascii_case("markdown") {
        chunk_markdown(content, source)
    } else {
        chunk_plain(content, source)
    }
}

const PLAIN_CHUNK_TARGET_BYTES: usize = 2 * 1024;
const MARKDOWN_CHUNK_TARGET_BYTES: usize = 4 * 1024;
const TITLE_MAX_CHARS: usize = 80;

/// One open section while streaming lines.
struct Section {
    title: String,
    part: usize,
    start_line: usize,
    last_line: usize,
    content: String,
}

impl Section {
    fn new(title: String) -> Self {
        Self {
            title,
            part: 0,
            start_line: 0,
            last_line: 0,
            content: String::new(),
        }
    }

    fn push_line(&mut self, line: &str, line_no: usize) {
        if self.content.is_empty() {
            self.start_line = line_no;
        }
        self.content.push_str(line);
        self.content.push('\n');
        self.last_line = line_no;
    }

    /// Emit this section (if it has any non-whitespace content) and start the
    /// next part under the same heading.
    fn flush(&mut self, chunks: &mut Vec<ParsedChunk>) {
        if self.content.trim().is_empty() {
            self.content.clear();
            self.start_line = 0;
            self.last_line = 0;
            return;
        }
        let title = if self.part == 0 {
            self.title.clone()
        } else {
            format!("{} (part {})", self.title, self.part + 1)
        };
        chunks.push(ParsedChunk {
            title,
            content: std::mem::take(&mut self.content),
            line_start: self.start_line,
            line_end: self.last_line,
        });
        self.part += 1;
        self.start_line = 0;
        self.last_line = 0;
    }
}

/// ATX heading outside a fence: `(level, title)`.
fn heading(line: &str) -> Option<(usize, &str)> {
    let indent = line.len() - line.trim_start_matches(' ').len();
    if indent > 3 {
        return None;
    }
    let rest = &line[indent..];
    let level = rest.chars().take_while(|ch| *ch == '#').count();
    if !(1..=6).contains(&level) {
        return None;
    }
    let after = &rest[level..];
    if after.is_empty() {
        return Some((level, ""));
    }
    if !after.starts_with(' ') {
        return None;
    }
    Some((level, after.trim().trim_end_matches('#').trim()))
}

/// Fence marker (` ``` ` / `~~~`) at line start, if any.
fn fence_marker(line: &str) -> Option<&'static str> {
    let trimmed = line.trim_start();
    if trimmed.starts_with("```") {
        Some("```")
    } else if trimmed.starts_with("~~~") {
        Some("~~~")
    } else {
        None
    }
}

fn strip_line_ending(raw: &str) -> &str {
    let line = raw.strip_suffix('\n').unwrap_or(raw);
    line.strip_suffix('\r').unwrap_or(line)
}

fn chunk_markdown(content: &str, title_hint: &str) -> Vec<ParsedChunk> {
    let mut chunks = Vec::new();
    let fallback = if title_hint.trim().is_empty() {
        "Introduction".to_owned()
    } else {
        title_hint.to_owned()
    };
    let mut section = Section::new(fallback);
    let mut fence: Option<&'static str> = None;
    let mut line_no = 0usize;

    for raw in content.split_inclusive('\n') {
        line_no += 1;
        let line = strip_line_ending(raw);
        if fence.is_none() {
            if let Some((_level, heading_title)) = heading(line) {
                section.flush(&mut chunks);
                let title = if heading_title.is_empty() {
                    title_hint.to_owned()
                } else {
                    heading_title.to_owned()
                };
                section = Section::new(title);
            }
        }
        section.push_line(line, line_no);
        match (fence, fence_marker(line)) {
            (None, Some(marker)) => fence = Some(marker),
            (Some(marker), Some(_)) if line.trim_start().starts_with(marker) => fence = None,
            _ => {}
        }
        // Split an oversized section at a blank line OUTSIDE a fence: code
        // blocks are never cut in half.
        if fence.is_none()
            && line.trim().is_empty()
            && section.content.len() >= MARKDOWN_CHUNK_TARGET_BYTES
        {
            section.flush(&mut chunks);
        }
    }
    section.flush(&mut chunks);
    chunks
}

fn chunk_plain(content: &str, title_hint: &str) -> Vec<ParsedChunk> {
    let mut chunks = Vec::new();
    let title = content
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(|line| truncate_chars(line, TITLE_MAX_CHARS))
        .filter(|line| !line.is_empty())
        .unwrap_or_else(|| title_hint.to_owned());
    let mut section = Section::new(title);
    let mut fence: Option<&'static str> = None;
    let mut line_no = 0usize;

    for raw in content.split_inclusive('\n') {
        line_no += 1;
        let line = strip_line_ending(raw);
        section.push_line(line, line_no);
        match (fence, fence_marker(line)) {
            (None, Some(marker)) => fence = Some(marker),
            (Some(marker), Some(_)) if line.trim_start().starts_with(marker) => fence = None,
            _ => {}
        }
        if fence.is_none()
            && (line.trim().is_empty() || section.content.len() >= PLAIN_CHUNK_TARGET_BYTES)
            && !section.content.trim().is_empty()
        {
            section.flush(&mut chunks);
        }
    }
    section.flush(&mut chunks);
    chunks
}
