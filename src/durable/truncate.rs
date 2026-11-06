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

pub use crate::text::{format_size, TruncatedBy, DEFAULT_MAX_BYTES, DEFAULT_MAX_LINES};

/// The UTF-8 byte length of a string.
pub fn utf8_byte_length(content: &str) -> usize {
    content.len()
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

/// Truncate content from the head (keep the first N lines/bytes).
///
/// Suitable for file reads where you want to see the beginning. Never returns
/// partial lines.
pub fn truncate_head(content: &str, options: TruncationOptions) -> TruncationResult {
    let totals = Totals {
        lines: crate::text::split_truncate_lines(content).len(),
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
    let core =
        crate::text::truncate_head_core(prefix, totals.lines, totals.bytes, max_lines, max_bytes);
    TruncationResult {
        content: core.content,
        truncated: core.truncated,
        truncated_by: core.truncated_by,
        total_lines: core.total_lines,
        total_bytes: core.total_bytes,
        output_lines: core.output_lines,
        output_bytes: core.output_bytes,
        last_line_partial: false,
        first_line_exceeds_limit: core.first_line_exceeds_limit,
        max_lines: core.max_lines,
        max_bytes: core.max_bytes,
    }
}
