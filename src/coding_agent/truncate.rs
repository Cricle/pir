//! Shared truncation utilities for tool outputs.
//!
//! Rust port of upstream `core/tools/truncate.ts`: two independent limits —
//! whichever is hit first wins (lines, default 2000; bytes, default 50 KB).
//! Head truncation suits file reads; tail truncation suits shell output.
//! Never returns partial lines, except the tail-truncation edge case where
//! the FIRST retained line alone exceeds the byte limit.

pub const DEFAULT_MAX_LINES: usize = 2000;
pub const DEFAULT_MAX_BYTES: usize = 50 * 1024;

/// Which limit fired when content was truncated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TruncatedBy {
    Lines,
    Bytes,
}

/// The result of a truncation pass (mirrors upstream `TruncationResult`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TruncationResult {
    pub content: String,
    pub truncated: bool,
    pub truncated_by: Option<TruncatedBy>,
    pub total_lines: usize,
    pub total_bytes: usize,
    pub output_lines: usize,
    pub output_bytes: usize,
    /// Tail truncation only: the first retained line was cut mid-line.
    pub last_line_partial: bool,
    /// Tail truncation only: byte size of the original final line (footer).
    pub last_line_bytes: usize,
    pub max_lines: usize,
    pub max_bytes: usize,
}

impl TruncationResult {
    /// The tool-details JSON (upstream `TruncationResult` interface,
    /// camelCase keys): `details.truncation` on read/bash/grep/find/ls.
    /// `firstLineExceedsLimit` mirrors upstream's head-truncation edge case
    /// (nothing was kept because the first line alone exceeded the byte
    /// limit); `truncatedBy` is `null` when not truncated.
    ///
    /// SIZE NOTE: `content` makes `details.truncation` a second on-disk copy
    /// of the retained tail (`content` on the message is the first, plus the
    /// `[Showing lines …]` footer). Upstream persists the whole
    /// `TruncationResult` the same way
    /// (`packages/coding-agent/src/core/tools/bash.ts:338-356`), so the
    /// duplicate is pi-session parity and is kept. No TUI path reads it —
    /// [`crate::tui::messages`] reads only the counters (`truncated`,
    /// `truncatedBy`, `outputLines`, `totalLines`, `maxBytes`,
    /// `firstLineExceedsLimit`) — but offline readers of a pi session file
    /// (and upstream's own exports) get the tail from here.
    pub fn to_details_json(&self) -> serde_json::Value {
        serde_json::json!({
            "content": self.content,
            "truncated": self.truncated,
            "truncatedBy": self.truncated_by.map(|by| match by {
                TruncatedBy::Lines => "lines",
                TruncatedBy::Bytes => "bytes",
            }),
            "totalLines": self.total_lines,
            "totalBytes": self.total_bytes,
            "outputLines": self.output_lines,
            "outputBytes": self.output_bytes,
            "lastLinePartial": self.last_line_partial,
            "firstLineExceedsLimit": self.truncated && self.output_lines == 0,
            "maxLines": self.max_lines,
            "maxBytes": self.max_bytes,
        })
    }
}

fn split_lines(content: &str) -> Vec<&str> {
    if content.is_empty() {
        return Vec::new();
    }
    let mut lines: Vec<&str> = content.split('\n').collect();
    if content.ends_with('\n') {
        lines.pop();
    }
    lines
}

/// Human-readable size, mirroring upstream `formatSize`.
pub fn format_size(bytes: usize) -> String {
    if bytes < 1024 {
        format!("{bytes}B")
    } else if bytes < 1024 * 1024 {
        format!("{:.1}KB", bytes as f64 / 1024.0)
    } else {
        format!("{:.1}MB", bytes as f64 / (1024.0 * 1024.0))
    }
}

/// Keep the FIRST max_lines lines within max_bytes.
pub fn truncate_head(content: &str, max_lines: usize, max_bytes: usize) -> TruncationResult {
    let total_bytes = content.len();
    let lines = split_lines(content);
    let total_lines = lines.len();

    if total_lines <= max_lines && total_bytes <= max_bytes {
        return TruncationResult {
            content: content.to_owned(),
            truncated: false,
            truncated_by: None,
            total_lines,
            total_bytes,
            output_lines: total_lines,
            output_bytes: total_bytes,
            last_line_partial: false,
            last_line_bytes: 0,
            max_lines,
            max_bytes,
        };
    }

    // First line alone exceeds the byte limit: upstream returns empty content.
    if lines[0].len() > max_bytes {
        return TruncationResult {
            content: String::new(),
            truncated: true,
            truncated_by: Some(TruncatedBy::Bytes),
            total_lines,
            total_bytes,
            output_lines: 0,
            output_bytes: 0,
            last_line_partial: false,
            last_line_bytes: 0,
            max_lines,
            max_bytes,
        };
    }

    let mut kept: Vec<&str> = Vec::new();
    let mut used = 0usize;
    let mut truncated_by = TruncatedBy::Lines;
    for (index, line) in lines.iter().enumerate() {
        if kept.len() >= max_lines {
            break;
        }
        let line_bytes = line.len() + if index > 0 { 1 } else { 0 };
        if used + line_bytes > max_bytes {
            truncated_by = TruncatedBy::Bytes;
            break;
        }
        used += line_bytes;
        kept.push(line);
    }
    if kept.len() >= max_lines && used <= max_bytes {
        truncated_by = TruncatedBy::Lines;
    }

    let output = kept.join("\n");
    TruncationResult {
        output_bytes: output.len(),
        content: output,
        truncated: true,
        truncated_by: Some(truncated_by),
        total_lines,
        total_bytes,
        output_lines: kept.len(),
        last_line_partial: false,
        last_line_bytes: 0,
        max_lines,
        max_bytes,
    }
}

/// Keep the LAST max_lines lines within max_bytes (shell output: errors and
/// final results live at the end). May return a partial first retained line
/// when that line alone exceeds the byte limit.
pub fn truncate_tail(content: &str, max_lines: usize, max_bytes: usize) -> TruncationResult {
    let total_bytes = content.len();
    let lines = split_lines(content);
    let total_lines = lines.len();

    if total_lines <= max_lines && total_bytes <= max_bytes {
        return TruncationResult {
            content: content.to_owned(),
            truncated: false,
            truncated_by: None,
            total_lines,
            total_bytes,
            output_lines: total_lines,
            output_bytes: total_bytes,
            last_line_partial: false,
            last_line_bytes: 0,
            max_lines,
            max_bytes,
        };
    }

    let mut kept: Vec<&str> = Vec::new();
    let mut used = 0usize;
    let mut truncated_by = TruncatedBy::Lines;
    let mut last_line_partial = false;

    for line in lines.iter().rev() {
        if kept.len() >= max_lines {
            break;
        }
        let line_bytes = line.len() + if kept.is_empty() { 0 } else { 1 };
        if used + line_bytes > max_bytes {
            truncated_by = TruncatedBy::Bytes;
            if kept.is_empty() {
                // Edge case: keep the end of the oversized line — the longest
                // char-boundary suffix within `take` bytes (upstream
                // `truncateStringToBytesFromEnd`; empty when the trailing
                // character alone exceeds `take`).
                let take = max_bytes.saturating_sub(used);
                let start = line
                    .char_indices()
                    .map(|(i, _)| i)
                    .find(|i| line.len() - *i <= take)
                    .unwrap_or(line.len());
                kept.insert(0, &line[start..]);
                last_line_partial = true;
            }
            break;
        }
        used += line_bytes;
        kept.insert(0, line);
    }

    let output = kept.join("\n");
    TruncationResult {
        output_bytes: output.len(),
        content: output,
        truncated: true,
        truncated_by: Some(truncated_by),
        total_lines,
        total_bytes,
        output_lines: kept.len(),
        last_line_partial,
        last_line_bytes: lines.last().map(|l| l.len()).unwrap_or(0),
        max_lines,
        max_bytes,
    }
}

/// Bash result assembly, mirroring upstream `formatOutput`: the tail view plus
/// a footer pointing at the full-output temp file.
pub fn format_shell_output(
    truncation: &TruncationResult,
    full_output_path: Option<&str>,
    empty_text: &str,
) -> String {
    let mut text = if truncation.content.is_empty() {
        empty_text.to_owned()
    } else {
        truncation.content.clone()
    };
    if !truncation.truncated {
        return text;
    }
    let start_line = truncation.total_lines - truncation.output_lines + 1;
    let end_line = truncation.total_lines;
    let full = full_output_path
        .map(|path| format!(" Full output: {path}"))
        .unwrap_or_default();
    if truncation.last_line_partial {
        text.push_str(&format!(
            "\n\n[Showing last {} of line {end_line} (line is {}).{full}]",
            format_size(truncation.output_bytes),
            format_size(truncation.last_line_bytes),
        ));
    } else if truncation.truncated_by == Some(TruncatedBy::Lines) {
        text.push_str(&format!(
            "\n\n[Showing lines {start_line}-{end_line} of {}. {full}]",
            truncation.total_lines,
        ));
    } else {
        text.push_str(&format!(
            "\n\n[Showing lines {start_line}-{end_line} of {} ({} limit).{full}]",
            truncation.total_lines,
            format_size(truncation.max_bytes),
        ));
    }
    text
}
