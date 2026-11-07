//! Deterministic evidence block for compaction summaries.
//!
//! The LLM-written summary can paraphrase away exact facts (paths, commands,
//! exit codes). This module scans the summarized span's tool calls/results and
//! appends a machine-derived block, so those facts survive verbatim:
//!
//! ```text
//! ## Evidence (deterministic)
//! ### Modified files
//! - path
//! ### Read files
//! - path
//! ### Commands & results
//! - `<cmd>` -> exit N: <last meaningful line>
//! ### Failed checks
//! - <cmd>: exit N
//! ```
//!
//! The block is bounded to [`MAX_EVIDENCE_BYTES`]; an oversized block is cut
//! at a UTF-8 boundary and annotated.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::Value as JsonValue;

use crate::ai::types::{ContentBlock, Message};

/// Header line that opens the block (and is used to find/strip it).
pub const EVIDENCE_HEADER: &str = "## Evidence (deterministic)";

/// Hard cap on the formatted block, in bytes.
pub const MAX_EVIDENCE_BYTES: usize = 8 * 1024;

/// Truncation note appended when the block exceeds [`MAX_EVIDENCE_BYTES`].
const TRUNCATION_NOTE: &str = "\n[Evidence truncated: exceeded 8 KiB limit]";

/// One shell command observed in the span.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandEvidence {
    /// The command line (`args.command`).
    pub command: String,
    /// Parsed exit code; `None` when the result carried none.
    pub exit_code: Option<i32>,
    /// Last meaningful output line (status lines / placeholders removed).
    pub last_line: String,
    /// Whether the result represents a failure (non-zero exit, `is_error`,
    /// abort/timeout/spawn-failure text).
    pub failed: bool,
}

/// Deterministic facts extracted from one compaction span.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct EvidenceBlock {
    modified_files: Vec<String>,
    read_files: Vec<String>,
    commands: Vec<CommandEvidence>,
    failures: Vec<String>,
}

impl EvidenceBlock {
    /// True when there is nothing to render.
    pub fn is_empty(&self) -> bool {
        self.modified_files.is_empty()
            && self.read_files.is_empty()
            && self.commands.is_empty()
            && self.failures.is_empty()
    }

    /// Merge another block into this one (carry-forward chain): files and
    /// failures deduplicate, commands deduplicate on their full evidence.
    pub fn merge(&mut self, other: EvidenceBlock) {
        merge_unique(&mut self.modified_files, other.modified_files);
        merge_unique(&mut self.read_files, other.read_files);
        merge_unique(&mut self.failures, other.failures);
        for command in other.commands {
            if !self.commands.contains(&command) {
                self.commands.push(command);
            }
        }
    }
}

fn merge_unique(target: &mut Vec<String>, incoming: Vec<String>) {
    let mut seen: BTreeSet<String> = target.iter().cloned().collect();
    for value in incoming {
        if seen.insert(value.clone()) {
            target.push(value);
        }
    }
}

/// Collect evidence from owned messages (public entry point).
pub fn collect_evidence(messages: &[Message]) -> EvidenceBlock {
    let refs: Vec<&Message> = messages.iter().collect();
    collect_evidence_refs(&refs)
}

/// [`collect_evidence`] over borrowed messages (used by the compaction path,
/// which holds `&[&Message]` spans).
pub(crate) fn collect_evidence_refs(messages: &[&Message]) -> EvidenceBlock {
    // File operations: reuse the exact parser/rule used by the summary file
    // lists (`read`/`write`/`edit` tool calls plus codemode nested calls).
    let mut ops = super::FileOperations::default();
    for message in messages {
        super::extract_file_ops_from_message(message, &mut ops);
    }
    let (read_files, modified_files) = super::compute_file_lists(&ops);

    // Tool-call metadata, keyed by call id, for matching results.
    let mut calls: BTreeMap<String, (String, Option<String>)> = BTreeMap::new();
    for message in messages {
        if let Message::Assistant(assistant) = message {
            for block in &assistant.content {
                if let ContentBlock::ToolCall(call) = block {
                    let command = call
                        .arguments
                        .get("command")
                        .and_then(JsonValue::as_str)
                        .map(str::to_owned);
                    calls.insert(call.id.clone(), (call.name.clone(), command));
                }
            }
        }
    }

    let mut block = EvidenceBlock {
        modified_files,
        read_files,
        ..EvidenceBlock::default()
    };

    for message in messages {
        let Message::ToolResult(result) = message else {
            continue;
        };
        let (call_name, call_command) =
            calls.get(&result.tool_call_id).cloned().unwrap_or_default();
        let tool_name = if call_name.is_empty() {
            result.tool_name.clone()
        } else {
            call_name
        };
        if !is_shell_tool(&tool_name) {
            continue;
        }
        let text = content_text(&result.content);
        let Some(command) = call_command.or_else(|| {
            result
                .details
                .as_ref()
                .and_then(|details| details.get("command"))
                .and_then(JsonValue::as_str)
                .map(str::to_owned)
        }) else {
            // No call metadata in this span (e.g. a cut boundary): a result
            // without the command line cannot be reported truthfully.
            continue;
        };
        let exit_code = parse_exit_code(&text).or_else(|| {
            result
                .details
                .as_ref()
                .and_then(|details| details.get("exitCode"))
                .and_then(JsonValue::as_i64)
                .and_then(|code| i32::try_from(code).ok())
        });
        let failed =
            result.is_error || exit_code.is_some_and(|code| code != 0) || looks_failed(&text);
        let last_line = last_meaningful_line(&text);
        let command_evidence = CommandEvidence {
            command,
            exit_code,
            last_line,
            failed,
        };
        if failed {
            let exit = command_evidence
                .exit_code
                .map(|code| code.to_string())
                .unwrap_or_else(|| "?".to_owned());
            block
                .failures
                .push(format!("{}: exit {exit}", command_evidence.command));
        }
        if !block.commands.contains(&command_evidence) {
            block.commands.push(command_evidence);
        }
    }

    block
}

fn is_shell_tool(name: &str) -> bool {
    matches!(name, "bash" | "shell" | "powershell" | "sh")
}

fn content_text(content: &[ContentBlock]) -> String {
    content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text(text) => Some(text.text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Parse the status lines pir's bash tool appends (`Command exited with code
/// N`, `Exit code: N`, `exit code N`) plus generic tool-result variants
/// (`"Exit code:"`), accepting 0. No status line / no number => `None`.
fn parse_exit_code(text: &str) -> Option<i32> {
    const MARKERS: [&str; 4] = [
        "command exited with code ",
        "exit code: ",
        "exit code ",
        "exit_code: ",
    ];
    let lower = text.to_ascii_lowercase();
    for marker in MARKERS {
        let mut search_from = 0;
        while let Some(offset) = lower[search_from..].find(marker) {
            let start = search_from + offset + marker.len();
            let rest = lower[start..].trim_start();
            let digits: String = rest
                .chars()
                .take_while(|ch| ch.is_ascii_digit() || *ch == '-')
                .collect();
            if let Ok(code) = digits.parse::<i32>() {
                return Some(code);
            }
            search_from = start;
        }
    }
    None
}

fn looks_failed(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    lower.contains("command failed")
        || lower.contains("command aborted")
        || lower.contains("command timed out")
        || lower.contains("command terminated without an exit code")
        || lower.contains("command exited with code")
            && lower
                .split_whitespace()
                .last()
                .and_then(|word| word.parse::<i32>().ok())
                .is_some_and(|code| code != 0)
}

/// Last non-empty output line, excluding shell status lines and the
/// `(no output)` placeholder. Truncated to 200 chars.
fn last_meaningful_line(text: &str) -> String {
    let line = text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .filter(|line| !line.eq_ignore_ascii_case("(no output)"))
        .rfind(|line| !is_status_line(line))
        .unwrap_or("");
    truncate_chars(line, 200)
}

fn is_status_line(line: &str) -> bool {
    let lower = line.to_ascii_lowercase();
    lower.starts_with("command exited with code ")
        || lower.starts_with("command aborted")
        || lower.starts_with("command timed out")
        || lower.starts_with("command terminated without an exit code")
        || lower.starts_with("exit code")
        || lower.starts_with("exit_code")
}

fn truncate_chars(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_owned();
    }
    let mut out: String = text.chars().take(max_chars).collect();
    out.push('…');
    out
}

/// Render the block appended to the summary. Empty block => empty string.
/// A leading `\n\n` separates the block from the LLM summary (same
/// convention as [`super::format_file_operations`]).
pub fn format_evidence_block(block: &EvidenceBlock) -> String {
    if block.is_empty() {
        return String::new();
    }

    let mut out = String::new();
    out.push_str("\n\n");
    out.push_str(EVIDENCE_HEADER);
    out.push('\n');

    if !block.modified_files.is_empty() {
        out.push_str("### Modified files\n");
        push_list(&mut out, &block.modified_files);
    }
    if !block.read_files.is_empty() {
        out.push_str("### Read files\n");
        push_list(&mut out, &block.read_files);
    }
    if !block.commands.is_empty() {
        out.push_str("### Commands & results\n");
        for command in &block.commands {
            let exit = command
                .exit_code
                .map(|code| code.to_string())
                .unwrap_or_else(|| "?".to_owned());
            if command.last_line.is_empty() {
                out.push_str(&format!("- `{}` -> exit {exit}\n", command.command));
            } else {
                out.push_str(&format!(
                    "- `{}` -> exit {exit}: {}\n",
                    command.command, command.last_line
                ));
            }
        }
    }
    if !block.failures.is_empty() {
        out.push_str("### Failed checks\n");
        push_list(&mut out, &block.failures);
    }

    truncate_block(out)
}

fn push_list(out: &mut String, items: &[String]) {
    for item in items {
        out.push_str("- ");
        out.push_str(item);
        out.push('\n');
    }
}

fn truncate_block(mut out: String) -> String {
    if out.len() <= MAX_EVIDENCE_BYTES {
        return out;
    }
    let budget = MAX_EVIDENCE_BYTES - TRUNCATION_NOTE.len();
    let cut = floor_char_boundary(&out, budget);
    out.truncate(cut);
    out.push_str(TRUNCATION_NOTE);
    out
}

fn floor_char_boundary(text: &str, mut index: usize) -> usize {
    while index > 0 && !text.is_char_boundary(index) {
        index -= 1;
    }
    index
}

/// Parse a previously rendered evidence block out of a summary (carry the
/// facts forward across compactions).
pub(crate) fn parse_evidence_block(summary: &str) -> Option<EvidenceBlock> {
    let start = summary.find(EVIDENCE_HEADER)?;
    let block_text = &summary[start..];
    let end = section_end(block_text).unwrap_or(block_text.len());
    let block_text = &block_text[..end];

    let mut block = EvidenceBlock::default();
    let mut section = "";
    for line in block_text.lines() {
        let line = line.trim();
        if let Some(name) = line.strip_prefix("### ") {
            section = name;
            continue;
        }
        let Some(item) = line.strip_prefix("- ") else {
            continue;
        };
        match section {
            "Modified files" => block.modified_files.push(item.to_owned()),
            "Read files" => block.read_files.push(item.to_owned()),
            "Commands & results" => {
                if let Some(command) = parse_command_line(item) {
                    block.commands.push(command);
                }
            }
            "Failed checks" => block.failures.push(item.to_owned()),
            _ => {}
        }
    }
    if block.is_empty() {
        None
    } else {
        Some(block)
    }
}

fn parse_command_line(item: &str) -> Option<CommandEvidence> {
    let rest = item.strip_prefix('`')?;
    let (command, rest) = rest.split_once('`')?;
    let rest = rest.strip_prefix(" -> exit ")?;
    let (exit, last_line) = rest.split_once(':').unwrap_or((rest, ""));
    let exit_code = if exit.trim() == "?" {
        None
    } else {
        exit.trim().parse::<i32>().ok()
    };
    Some(CommandEvidence {
        command: command.to_owned(),
        exit_code,
        last_line: last_line.trim().to_owned(),
        failed: exit_code.is_some_and(|code| code != 0),
    })
}

/// Remove the deterministic block from a previous summary before handing it
/// to the model (the block is rebuilt from the messages, never paraphrased).
pub(crate) fn strip_evidence_block(summary: &str) -> String {
    let Some(start) = summary.find(EVIDENCE_HEADER) else {
        return summary.to_owned();
    };
    let rest = &summary[start..];
    let end = section_end(rest)
        .map(|offset| start + offset)
        .unwrap_or(summary.len());
    let mut stripped = String::with_capacity(summary.len());
    stripped.push_str(summary[..start].trim_end());
    let tail = summary[end..].trim_start_matches('\n');
    if !tail.is_empty() {
        stripped.push_str("\n\n");
        stripped.push_str(tail);
    }
    stripped
}

/// Offset where the evidence section ends: the next markdown `## ` heading or
/// the upstream `<read-files>` / `<modified-files>` tags appended after it.
fn section_end(text: &str) -> Option<usize> {
    ["\n## ", "\n<read-files>", "\n<modified-files>"]
        .iter()
        .filter_map(|marker| text.find(marker))
        .min()
}
