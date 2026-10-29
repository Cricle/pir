//! Shared truncation utilities for tool outputs.
//!
//! Ports upstream `src/truncate.ts`. Truncation is based on two independent
//! limits — whichever is hit first wins:
//!
//! * line limit (default 2000 lines),
//! * byte limit (default 50 KiB).
//!
//! Never returns partial lines: output is a prefix of complete lines. If the
//! first line alone exceeds the byte limit the result is empty and
//! [`TruncationResult::first_line_exceeds_limit`] is set.

/// Default maximum number of lines.
pub const DEFAULT_MAX_LINES: usize = 2000;
/// Default maximum number of bytes (50 KiB).
pub const DEFAULT_MAX_BYTES: usize = 50 * 1024;

/// Which limit stopped the truncation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TruncatedBy {
    /// The line limit.
    Lines,
    /// The byte limit.
    Bytes,
}

/// The outcome of one truncation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TruncationResult {
    /// The truncated content.
    pub content: String,
    /// Whether truncation occurred.
    pub truncated: bool,
    /// Which limit was hit, if any.
    pub truncated_by: Option<TruncatedBy>,
    /// Total number of lines in the original content.
    pub total_lines: usize,
    /// Total number of bytes in the original content.
    pub total_bytes: usize,
    /// Number of complete lines in the truncated output.
    pub output_lines: usize,
    /// Number of bytes in the truncated output.
    pub output_bytes: usize,
    /// Whether the last line was partially truncated. Head truncation never
    /// does this; the field exists for parity with the upstream result type.
    pub last_line_partial: bool,
    /// Whether the first line exceeded the byte limit.
    pub first_line_exceeds_limit: bool,
    /// The applied line limit.
    pub max_lines: usize,
    /// The applied byte limit.
    pub max_bytes: usize,
}

/// Limits for one truncation.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TruncationOptions {
    /// Maximum number of lines (default [`DEFAULT_MAX_LINES`]).
    pub max_lines: Option<usize>,
    /// Maximum number of bytes (default [`DEFAULT_MAX_BYTES`]).
    pub max_bytes: Option<usize>,
}

/// The UTF-8 byte length of a string.
pub fn utf8_byte_length(content: &str) -> usize {
    content.len()
}

/// Split into lines the way the upstream counter does: a trailing newline does
/// not create a final empty line.
fn split_lines_for_counting(content: &str) -> Vec<&str> {
    if content.is_empty() {
        return Vec::new();
    }
    let mut lines: Vec<&str> = content.split('\n').collect();
    if content.ends_with('\n') {
        lines.pop();
    }
    lines
}

/// Format bytes as a human-readable size.
pub fn format_size(bytes: usize) -> String {
    if bytes < 1024 {
        format!("{bytes}B")
    } else if bytes < 1024 * 1024 {
        format!("{:.1}KB", bytes as f64 / 1024.0)
    } else {
        format!("{:.1}MB", bytes as f64 / (1024.0 * 1024.0))
    }
}

/// Truncate content from the head (keep the first N lines/bytes).
///
/// Suitable for file reads where you want to see the beginning. Never returns
/// partial lines.
pub fn truncate_head(content: &str, options: TruncationOptions) -> TruncationResult {
    let totals = Totals {
        lines: split_lines_for_counting(content).len(),
        bytes: utf8_byte_length(content),
    };
    truncate_head_of(content, totals, options)
}

/// Totals of a text, as counted by [`truncate_head`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Totals {
    /// Line count (a trailing newline is ignored).
    pub lines: usize,
    /// UTF-8 byte count.
    pub bytes: usize,
}

/// [`truncate_head`] of a text known by a prefix and its totals.
///
/// The prefix must be the whole text, or longer than `max_bytes + 1` UTF-8
/// bytes, or hold at least `max_lines` newlines; then the result equals
/// [`truncate_head`] of the whole text.
pub fn truncate_head_of(
    prefix: &str,
    totals: Totals,
    options: TruncationOptions,
) -> TruncationResult {
    let max_lines = options.max_lines.unwrap_or(DEFAULT_MAX_LINES);
    let max_bytes = options.max_bytes.unwrap_or(DEFAULT_MAX_BYTES);
    let total_lines = totals.lines;
    let total_bytes = totals.bytes;
    let lines = split_lines_for_counting(prefix);

    if total_lines <= max_lines && total_bytes <= max_bytes {
        return TruncationResult {
            content: prefix.to_owned(),
            truncated: false,
            truncated_by: None,
            total_lines,
            total_bytes,
            output_lines: total_lines,
            output_bytes: total_bytes,
            last_line_partial: false,
            first_line_exceeds_limit: false,
            max_lines,
            max_bytes,
        };
    }

    let first_line_bytes = lines.first().map_or(0, |line| utf8_byte_length(line));
    if first_line_bytes > max_bytes {
        return TruncationResult {
            content: String::new(),
            truncated: true,
            truncated_by: Some(TruncatedBy::Bytes),
            total_lines,
            total_bytes,
            output_lines: 0,
            output_bytes: 0,
            last_line_partial: false,
            first_line_exceeds_limit: true,
            max_lines,
            max_bytes,
        };
    }

    let mut output: Vec<&str> = Vec::new();
    let mut output_bytes = 0usize;
    let mut truncated_by = TruncatedBy::Lines;
    for (index, line) in lines.iter().enumerate().take(max_lines) {
        let line_bytes = utf8_byte_length(line) + usize::from(index > 0);
        if output_bytes + line_bytes > max_bytes {
            truncated_by = TruncatedBy::Bytes;
            break;
        }
        output.push(line);
        output_bytes += line_bytes;
    }
    // Without a byte break, only omitted lines prove the line limit was
    // reached; otherwise a trailing newline exceeded bytes.
    if truncated_by != TruncatedBy::Bytes {
        truncated_by = if output.len() < total_lines {
            TruncatedBy::Lines
        } else {
            TruncatedBy::Bytes
        };
    }

    let content = output.join("\n");
    let final_bytes = utf8_byte_length(&content);
    TruncationResult {
        content,
        truncated: true,
        truncated_by: Some(truncated_by),
        total_lines,
        total_bytes,
        output_lines: output.len(),
        output_bytes: final_bytes,
        last_line_partial: false,
        first_line_exceeds_limit: false,
        max_lines,
        max_bytes,
    }
}
