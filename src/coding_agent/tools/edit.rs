//! Edit tool: exact text replacement with fuzzy-match fallback, BOM and CRLF
//! preservation, and unified-diff output.
//!
//! Compiled under `any(feature = "tool-edit", feature = "tui")`: a
//! `tool-edit`-off TUI build keeps `compute_edits_diff` for the live edit
//! preview. In that build the tool registration is absent, so the tool-only
//! helpers are unreachable.
#![cfg_attr(not(feature = "tool-edit"), allow(dead_code))]

use std::path::Path;

use crate::agent::tool_batch::ToolResult;
use crate::agent::tool_execution::BoxFuture;
#[cfg(feature = "tool-edit")]
use crate::agent::tool_execution::{AgentTool, ToolContext};
use crate::ai::types::AbortSignal;

#[cfg(feature = "tool-edit")]
use super::confine::read_confined;
use super::confine::ConfinedTarget;
#[cfg(feature = "tool-edit")]
use super::util::single_path_arg;
use super::util::{
    max_source_bytes, source_limit_error, workspace_path_parts, workspace_path_remote_checked,
    write_remote_file_atomically,
};

// ---------------------------------------------------------------------------
// Edit tool
// ---------------------------------------------------------------------------

/// One requested replacement (upstream `edit.ts` `edits[]` entry).
struct EditRequest {
    old_text: String,
    new_text: String,
    replace_all: bool,
}

/// A matched replacement region inside the (possibly fuzzy-normalized) base
/// content (upstream `edit-diff.ts` `MatchedEdit` / `TextReplacement`).
#[derive(Clone)]
struct TextReplacement {
    edit_index: usize,
    match_index: usize,
    match_length: usize,
    new_text: String,
}

const BOM: char = '\u{FEFF}';

/// Local reads and writes are chunked so a large payload observes cancellation
/// between chunks; the size mirrors the remote layer's `WRITE_CHUNK`.
const LOCAL_IO_CHUNK_BYTES: usize = 512 * 1024;

/// Upstream `splitBom`: separate a leading U+FEFF so it never participates in
/// matching; it is restored after writing.
fn split_bom(content: &str) -> (bool, &str) {
    match content.strip_prefix(BOM) {
        Some(rest) => (true, rest),
        None => (false, content),
    }
}

/// Upstream `normalizeToLF`: CRLF and lone CR both become LF.
fn normalize_to_lf(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\r' {
            if chars.peek() == Some(&'\n') {
                chars.next();
            }
            out.push('\n');
        } else {
            out.push(c);
        }
    }
    out
}

/// Upstream `detectLineEnding`: CRLF wins when the first CRLF precedes the
/// first lone LF.
fn detect_line_ending(content: &str) -> &'static str {
    let lf = content.find('\n');
    let crlf = content.find("\r\n");
    match (lf, crlf) {
        (Some(lf_index), Some(crlf_index)) if crlf_index < lf_index => "\r\n",
        _ => "\n",
    }
}

fn restore_line_endings(text: String, ending: &str) -> String {
    if ending == "\r\n" {
        text.replace('\n', "\r\n")
    } else {
        text
    }
}

/// Upstream `normalizeForFuzzyMatch`: NFKC, per-line trailing-whitespace strip,
/// smart quotes/dashes/special spaces mapped to ASCII equivalents.
///
/// The four upstream `.replace` passes are folded into one char map: each
/// replacement is ASCII and outside every later pattern, so the passes cannot
/// interact. Lines are trimmed and appended directly, avoiding the intermediate
/// `Vec<&str>`, `join`, and four replacement strings.
fn normalize_for_fuzzy_match(text: &str) -> String {
    use unicode_normalization::UnicodeNormalization;
    let nfkc: String = text.nfkc().collect();
    let mut out = String::with_capacity(nfkc.len());
    for (index, line) in nfkc.split('\n').enumerate() {
        if index > 0 {
            out.push('\n');
        }
        for character in line.trim_end().chars() {
            out.push(match character {
                '\u{2018}' | '\u{2019}' | '\u{201A}' | '\u{201B}' => '\'',
                '\u{201C}' | '\u{201D}' | '\u{201E}' | '\u{201F}' => '"',
                '\u{2010}' | '\u{2011}' | '\u{2012}' | '\u{2013}' | '\u{2014}' | '\u{2015}'
                | '\u{2212}' => '-',
                '\u{00A0}' | '\u{2002}' | '\u{2003}' | '\u{2004}' | '\u{2005}' | '\u{2006}'
                | '\u{2007}' | '\u{2008}' | '\u{2009}' | '\u{200A}' | '\u{202F}' | '\u{205F}'
                | '\u{3000}' => ' ',
                other => other,
            });
        }
    }
    out
}

/// Split into lines keeping their trailing `\n` (upstream
/// `splitLinesWithEndings`).
fn split_lines_with_endings(content: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut rest = content;
    while let Some(index) = rest.find('\n') {
        out.push(&rest[..=index]);
        rest = &rest[index + 1..];
    }
    if !rest.is_empty() {
        out.push(rest);
    }
    out
}

/// Byte spans of each line-with-ending (upstream `getLineSpans`).
fn line_spans(content: &str) -> Vec<(usize, usize)> {
    let mut offset = 0usize;
    split_lines_with_endings(content)
        .into_iter()
        .map(|line| {
            let span = (offset, offset + line.len());
            offset = span.1;
            span
        })
        .collect()
}

/// Byte range of lines touched by one replacement (upstream
/// `getReplacementLineRange`).
fn replacement_line_range(
    spans: &[(usize, usize)],
    replacement: &TextReplacement,
) -> Result<(usize, usize), String> {
    let start = replacement.match_index;
    let end = replacement.match_index + replacement.match_length;
    let start_line = spans
        .iter()
        .position(|(line_start, line_end)| start >= *line_start && start < *line_end)
        .ok_or("Replacement range is outside the base content.")?;
    let mut end_line = start_line;
    while end_line < spans.len() && spans[end_line].1 < end {
        end_line += 1;
    }
    if end_line >= spans.len() {
        return Err("Replacement range is outside the base content.".to_owned());
    }
    Ok((start_line, end_line + 1))
}

/// Apply replacements (sorted ascending by match index) right-to-left so
/// earlier offsets stay stable (upstream `applyReplacements`).
fn apply_replacements(content: &str, replacements: &[TextReplacement], offset: usize) -> String {
    let mut result = content.to_owned();
    for replacement in replacements.iter().rev() {
        let index = replacement.match_index - offset;
        result.replace_range(
            index..index + replacement.match_length,
            &replacement.new_text,
        );
    }
    result
}

/// Overlay replacements made in fuzzy-normalized space back onto the original
/// content, rewriting only the touched line ranges (upstream
/// `applyReplacementsPreservingUnchangedLines`).
fn apply_replacements_preserving_unchanged_lines(
    original_content: &str,
    base_content: &str,
    replacements: &[TextReplacement],
) -> Result<String, String> {
    let original_lines = split_lines_with_endings(original_content);
    let base_spans = line_spans(base_content);
    if original_lines.len() != base_spans.len() {
        return Err(
            "Cannot preserve unchanged lines because the base content has a different line count."
                .to_owned(),
        );
    }

    let mut sorted: Vec<TextReplacement> = replacements.to_vec();
    sorted.sort_by_key(|replacement| replacement.match_index);

    let mut groups: Vec<(usize, usize, Vec<TextReplacement>)> = Vec::new();
    for replacement in sorted {
        let (start, end) = replacement_line_range(&base_spans, &replacement)?;
        match groups.last_mut() {
            Some((_, group_end, reps)) if start < *group_end => {
                *group_end = (*group_end).max(end);
                reps.push(replacement);
            }
            _ => groups.push((start, end, vec![replacement])),
        }
    }

    let mut original_line_index = 0usize;
    let mut result = String::new();
    for (start_line, end_line, reps) in &groups {
        result.push_str(&original_lines[original_line_index..*start_line].concat());
        let group_start = base_spans[*start_line].0;
        let group_end = base_spans[end_line - 1].1;
        result.push_str(&apply_replacements(
            &base_content[group_start..group_end],
            reps,
            group_start,
        ));
        original_line_index = *end_line;
    }
    result.push_str(&original_lines[original_line_index..].concat());
    Ok(result)
}

/// Upstream `fuzzyFindText` in counting form: occurrences of the already
/// fuzzy-normalized `old_text` in the already fuzzy-normalized `content`.
/// `normalize_for_fuzzy_match` is idempotent (its output contains no characters
/// from the replacement sets and no trailing line whitespace), so callers must
/// normalize both inputs once and reuse them across edits.
fn count_fuzzy_occurrences(fuzzy_content: &str, fuzzy_old_text: &str) -> usize {
    fuzzy_content.match_indices(fuzzy_old_text).count()
}

/// Collect `edits[]` from tool arguments, tolerating the legacy single
/// `oldText`/`newText` pair, a JSON-encoded `edits` string, and a single edit
/// object instead of a one-element array (upstream `prepareEditArguments`).
fn parse_edit_requests(args: &serde_json::Value) -> Result<Vec<EditRequest>, String> {
    let mut edits: Vec<EditRequest> = Vec::new();
    if let Some(edits_value) = args.get("edits") {
        let entries: Vec<serde_json::Value> = match edits_value {
            serde_json::Value::Array(items) => items.clone(),
            serde_json::Value::String(raw) => {
                match serde_json::from_str::<serde_json::Value>(raw) {
                    Ok(serde_json::Value::Array(items)) => items,
                    Ok(single @ serde_json::Value::Object(_)) => vec![single],
                    _ => return Err(
                        "Edit tool input is invalid. edits must contain at least one replacement."
                            .to_owned(),
                    ),
                }
            }
            single @ serde_json::Value::Object(_) => vec![single.clone()],
            _ => {
                return Err(
                    "Edit tool input is invalid. edits must contain at least one replacement."
                        .to_owned(),
                )
            }
        };
        for (index, entry) in entries.iter().enumerate() {
            let old_text = entry
                .get("oldText")
                .and_then(|value| value.as_str())
                .ok_or_else(|| format!("edits[{index}].oldText must be a string"))?;
            let new_text = entry
                .get("newText")
                .and_then(|value| value.as_str())
                .ok_or_else(|| format!("edits[{index}].newText must be a string"))?;
            let replace_all = entry
                .get("replaceAll")
                .and_then(|value| value.as_bool())
                .unwrap_or(false);
            edits.push(EditRequest {
                old_text: old_text.to_owned(),
                new_text: new_text.to_owned(),
                replace_all,
            });
        }
    }
    // Legacy single-edit arguments are appended when present.
    if let (Some(old_text), Some(new_text)) = (
        args.get("oldText").and_then(|value| value.as_str()),
        args.get("newText").and_then(|value| value.as_str()),
    ) {
        edits.push(EditRequest {
            old_text: old_text.to_owned(),
            new_text: new_text.to_owned(),
            replace_all: false,
        });
    }
    if edits.is_empty() {
        return Err(
            "Edit tool input is invalid. edits must contain at least one replacement.".to_owned(),
        );
    }
    Ok(edits)
}

fn edit_empty_old_text_error(path: &str, edit_index: usize, total: usize) -> String {
    if total == 1 {
        format!("oldText must not be empty in {path}.")
    } else {
        format!("edits[{edit_index}].oldText must not be empty in {path}.")
    }
}

fn edit_not_found_error(path: &str, edit_index: usize, total: usize) -> String {
    if total == 1 {
        format!(
            "Could not find the exact text in {path}. The old text must match exactly including all whitespace and newlines."
        )
    } else {
        format!(
            "Could not find edits[{edit_index}] in {path}. The oldText must match exactly including all whitespace and newlines."
        )
    }
}

fn edit_duplicate_error(path: &str, edit_index: usize, total: usize, occurrences: usize) -> String {
    if total == 1 {
        format!(
            "Found {occurrences} occurrences of the text in {path}. The text must be unique. Please provide more context to make it unique."
        )
    } else {
        format!(
            "Found {occurrences} occurrences of edits[{edit_index}] in {path}. Each oldText must be unique. Please provide more context to make it unique."
        )
    }
}

fn edit_no_change_error(path: &str, total: usize) -> String {
    if total == 1 {
        format!(
            "No changes made to {path}. The replacement produced identical content. This might indicate an issue with special characters or the text not existing as expected."
        )
    } else {
        format!("No changes made to {path}. The replacements produced identical content.")
    }
}

/// Upstream `applyEditsToNormalizedContent`: match every edit against the same
/// original content, validate uniqueness/non-overlap, and produce the new
/// content. Returns `(base_content, new_content)` in LF-normalized space.
fn apply_edits_to_normalized_content(
    normalized_content: &str,
    edits: &[EditRequest],
    path: &str,
) -> Result<(String, String), String> {
    let total = edits.len();
    let normalized_edits: Vec<(String, String, bool)> = edits
        .iter()
        .map(|edit| {
            (
                normalize_to_lf(&edit.old_text),
                normalize_to_lf(&edit.new_text),
                edit.replace_all,
            )
        })
        .collect();

    for (index, (old_text, _, _)) in normalized_edits.iter().enumerate() {
        if old_text.is_empty() {
            return Err(edit_empty_old_text_error(path, index, total));
        }
    }

    // Normalize the base and every oldText exactly once per call; the per-edit
    // counting/matching below reuses these strings instead of re-normalizing
    // the whole file for each edit (E × file-size work).
    let fuzzy_content = normalize_for_fuzzy_match(normalized_content);
    let fuzzy_old_texts: Vec<String> = normalized_edits
        .iter()
        .map(|(old_text, _, _)| normalize_for_fuzzy_match(old_text))
        .collect();
    // A whitespace-only oldText normalizes to the empty string. Fuzzy
    // `contains("")` is always true, and `match_indices("")` yields every
    // character boundary, so reject it before any matching/`replaceAll` work.
    for (index, fuzzy_old_text) in fuzzy_old_texts.iter().enumerate() {
        if fuzzy_old_text.is_empty() {
            return Err(edit_empty_old_text_error(path, index, total));
        }
    }

    // Decide whether any edit needs fuzzy matching; then the whole operation
    // runs in fuzzy-normalized space (upstream semantics).
    let used_fuzzy_match =
        normalized_edits
            .iter()
            .zip(&fuzzy_old_texts)
            .any(|((old_text, _, _), fuzzy_old_text)| {
                normalized_content.find(old_text).is_none()
                    && fuzzy_content.contains(fuzzy_old_text)
            });
    let replacement_base_content = if used_fuzzy_match {
        fuzzy_content.as_str()
    } else {
        normalized_content
    };

    let mut matched_edits: Vec<TextReplacement> = Vec::new();
    for (index, (old_text, new_text, replace_all)) in normalized_edits.iter().enumerate() {
        // Upstream counts occurrences in fuzzy-normalized space (even when the
        // exact path is used) and matches the fuzzy-normalized oldText inside
        // the (possibly fuzzy-normalized) base.
        let match_old = if used_fuzzy_match {
            fuzzy_old_texts[index].as_str()
        } else {
            old_text.as_str()
        };
        let fuzzy_occurrences = count_fuzzy_occurrences(&fuzzy_content, &fuzzy_old_texts[index]);
        if fuzzy_occurrences == 0 && !replacement_base_content.contains(match_old) {
            return Err(edit_not_found_error(path, index, total));
        }
        if fuzzy_occurrences > 1 && !replace_all {
            return Err(edit_duplicate_error(path, index, total, fuzzy_occurrences));
        }
        let match_indices: Vec<usize> = if *replace_all {
            replacement_base_content
                .match_indices(match_old)
                .map(|(i, _)| i)
                .collect()
        } else {
            replacement_base_content
                .find(match_old)
                .into_iter()
                .collect()
        };
        for match_index in match_indices {
            matched_edits.push(TextReplacement {
                edit_index: index,
                match_index,
                match_length: match_old.len(),
                new_text: new_text.clone(),
            });
        }
    }

    matched_edits.sort_by_key(|replacement| replacement.match_index);
    for window in matched_edits.windows(2) {
        let previous = &window[0];
        let current = &window[1];
        if previous.match_index + previous.match_length > current.match_index {
            return Err(format!(
                "edits[{}] and edits[{}] overlap in {path}. Merge them into one edit or target disjoint regions.",
                previous.edit_index, current.edit_index
            ));
        }
    }

    let base_content = normalized_content.to_owned();
    let new_content = if used_fuzzy_match {
        apply_replacements_preserving_unchanged_lines(
            &base_content,
            replacement_base_content,
            &matched_edits,
        )?
    } else {
        apply_replacements(replacement_base_content, &matched_edits, 0)
    };

    if base_content == new_content {
        return Err(edit_no_change_error(path, total));
    }
    Ok((base_content, new_content))
}

/// Upstream `generateUnifiedPatch`: a unified patch with 4 context lines using
/// the `similar` crate.
fn generate_unified_patch(path: &str, old_content: &str, new_content: &str) -> String {
    let diff = similar::TextDiff::from_lines(old_content, new_content);
    let mut unified = diff.unified_diff();
    unified.context_radius(4).header(path, path);
    unified.to_string()
}

/// Upstream `generateDiffString`: a display diff with line numbers and 4
/// lines of context around each change. Returns the rendered diff and the
/// first changed line number in the new file.
fn generate_diff_string(old_content: &str, new_content: &str) -> (String, Option<usize>) {
    const CONTEXT_LINES: usize = 4;
    let diff = similar::TextDiff::from_lines(old_content, new_content);
    // Group consecutive same-tag changes into parts (upstream
    // `Diff.diffLines` merges runs of same-kind lines into one part).
    let mut parts: Vec<(similar::ChangeTag, String)> = Vec::new();
    for change in diff.iter_all_changes() {
        let tag = change.tag();
        match parts.last_mut() {
            Some((existing, text)) if *existing == tag => text.push_str(change.value()),
            _ => parts.push((tag, change.value().to_owned())),
        }
    }

    let old_total = old_content.split('\n').count();
    let new_total = new_content.split('\n').count();
    let line_num_width = old_total.max(new_total).to_string().len();

    let mut output: Vec<String> = Vec::new();
    let mut old_line_num = 1usize;
    let mut new_line_num = 1usize;
    let mut last_was_change = false;
    let mut first_changed_line: Option<usize> = None;

    for (index, (tag, text)) in parts.iter().enumerate() {
        let lines = diff_part_lines(text);
        match tag {
            similar::ChangeTag::Equal => {
                let has_leading_change = last_was_change;
                let has_trailing_change = parts
                    .get(index + 1)
                    .is_some_and(|(tag, _)| *tag != similar::ChangeTag::Equal);
                if has_leading_change && has_trailing_change {
                    if lines.len() <= CONTEXT_LINES * 2 {
                        for line in &lines {
                            output.push(format!(
                                " {:>width$} {line}",
                                old_line_num,
                                width = line_num_width
                            ));
                            old_line_num += 1;
                            new_line_num += 1;
                        }
                    } else {
                        let skipped = lines.len() - CONTEXT_LINES * 2;
                        for line in &lines[..CONTEXT_LINES] {
                            output.push(format!(
                                " {:>width$} {line}",
                                old_line_num,
                                width = line_num_width
                            ));
                            old_line_num += 1;
                            new_line_num += 1;
                        }
                        output.push(format!(" {:>width$} ...", "", width = line_num_width));
                        old_line_num += skipped;
                        new_line_num += skipped;
                        for line in &lines[lines.len() - CONTEXT_LINES..] {
                            output.push(format!(
                                " {:>width$} {line}",
                                old_line_num,
                                width = line_num_width
                            ));
                            old_line_num += 1;
                            new_line_num += 1;
                        }
                    }
                } else if has_leading_change {
                    let skipped = lines.len().saturating_sub(CONTEXT_LINES);
                    for line in &lines[..CONTEXT_LINES.min(lines.len())] {
                        output.push(format!(
                            " {:>width$} {line}",
                            old_line_num,
                            width = line_num_width
                        ));
                        old_line_num += 1;
                        new_line_num += 1;
                    }
                    if skipped > 0 {
                        output.push(format!(" {:>width$} ...", "", width = line_num_width));
                        old_line_num += skipped;
                        new_line_num += skipped;
                    }
                } else if has_trailing_change {
                    let skipped = lines.len().saturating_sub(CONTEXT_LINES);
                    if skipped > 0 {
                        output.push(format!(" {:>width$} ...", "", width = line_num_width));
                        old_line_num += skipped;
                        new_line_num += skipped;
                    }
                    for line in &lines[skipped..] {
                        output.push(format!(
                            " {:>width$} {line}",
                            old_line_num,
                            width = line_num_width
                        ));
                        old_line_num += 1;
                        new_line_num += 1;
                    }
                } else {
                    // Skip these context lines entirely.
                    old_line_num += lines.len();
                    new_line_num += lines.len();
                }
                last_was_change = false;
            }
            similar::ChangeTag::Insert => {
                if first_changed_line.is_none() {
                    first_changed_line = Some(new_line_num);
                }
                for line in &lines {
                    output.push(format!(
                        "+{:>width$} {line}",
                        new_line_num,
                        width = line_num_width
                    ));
                    new_line_num += 1;
                }
                last_was_change = true;
            }
            similar::ChangeTag::Delete => {
                if first_changed_line.is_none() {
                    first_changed_line = Some(new_line_num);
                }
                for line in &lines {
                    output.push(format!(
                        "-{:>width$} {line}",
                        old_line_num,
                        width = line_num_width
                    ));
                    old_line_num += 1;
                }
                last_was_change = true;
            }
        }
    }
    (output.join("\n"), first_changed_line)
}

/// Split one diff part's concatenated value into display lines (upstream
/// `part.value.split("\n")` with the trailing empty piece popped).
fn diff_part_lines(value: &str) -> Vec<String> {
    let mut lines: Vec<String> = value.split('\n').map(str::to_owned).collect();
    if lines.last().is_some_and(String::is_empty) {
        lines.pop();
    }
    lines
}

/// Pluggable filesystem operations for the edit tool (upstream
/// `EditOperations`, edit.ts:83-101). Path normalization and workspace
/// confinement still run in the tool before the backend is called.
pub trait EditOperations: Send + Sync {
    /// Read the file contents.
    fn read_file<'a>(&'a self, absolute_path: &'a Path) -> BoxFuture<'a, std::io::Result<Vec<u8>>>;
    /// Write the new contents.
    fn write_file<'a>(
        &'a self,
        absolute_path: &'a Path,
        content: &'a str,
    ) -> BoxFuture<'a, std::io::Result<()>>;
    /// Check the file is readable and writable (an error means it is not).
    fn access<'a>(&'a self, absolute_path: &'a Path) -> BoxFuture<'a, std::io::Result<()>>;
}

pub struct EditTool {
    operations: Option<std::sync::Arc<dyn EditOperations>>,
}

impl EditTool {
    /// The built-in tool (local filesystem, or the `env_client` remote arm
    /// when configured).
    pub fn new() -> Self {
        Self { operations: None }
    }

    /// The tool with an explicit edit backend (upstream
    /// `createEditTool(cwd, { operations })`). An injected backend wins over
    /// the process-wide `env_client` selection.
    pub fn with_operations(operations: std::sync::Arc<dyn EditOperations>) -> Self {
        Self {
            operations: Some(operations),
        }
    }
}

impl Default for EditTool {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(feature = "tool-edit")]
impl AgentTool for EditTool {
    fn name(&self) -> &'static str {
        "edit"
    }
    fn description(&self) -> &'static str {
        "Edit a single file using exact text replacement. Every edits[].oldText must match a unique, non-overlapping region of the original file. If two changes affect the same block or nearby lines, merge them into one edit instead of emitting overlapping edits. Do not include large unchanged regions just to connect distant changes."
    }
    fn is_mutation_tool(&self) -> bool {
        true
    }
    fn mutation_paths(&self, args: &serde_json::Value, cwd: &Path) -> Vec<std::path::PathBuf> {
        single_path_arg(args, cwd).1
    }
    fn prompt_snippet(&self) -> Option<String> {
        Some(
            "Make precise file edits with exact text replacement, including multiple disjoint \
             edits in one call"
                .to_owned(),
        )
    }
    fn prompt_guidelines(&self) -> Vec<String> {
        vec![
            "Use edit for precise changes (edits[].oldText must match exactly)".to_owned(),
            "When changing multiple separate locations in one file, use one edit call with \
             multiple entries in edits[] instead of multiple edit calls"
                .to_owned(),
            "Each edits[].oldText is matched against the original file, not after earlier edits \
             are applied. Do not emit overlapping or nested edits. Merge nearby changes into one \
             edit."
                .to_owned(),
            "Keep edits[].oldText as small as possible while still being unique in the file. Do \
             not pad with large unchanged regions."
                .to_owned(),
        ]
    }
    fn parameters(&self) -> Option<serde_json::Value> {
        // Upstream's public schema declares `path` + `edits[]` with
        // `oldText`/`newText` and REQUIRES both (`edit.ts:32-41`). The legacy
        // single `oldText`/`newText` pair is still tolerated by
        // `parse_edit_requests` for direct callers, and the per-edit
        // `replaceAll` flag stays hidden but honored (upstream
        // `prepareEditArguments`/edit-diff). Validation mirrors upstream's
        // `validateEditInput` error text.
        Some(serde_json::json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Path to the file to edit (relative or absolute)" },
                "edits": {
                    "type": "array",
                    "description": "One or more targeted replacements. Each edit is matched against the original file, not incrementally. Do not include overlapping or nested edits. If two changes touch the same block or nearby lines, merge them into one edit instead.",
                    "items": {
                        "type": "object",
                        "properties": {
                            "oldText": { "type": "string", "description": "Exact text for one targeted replacement. It must be unique in the original file and must not overlap with any other edits[].oldText in the same call." },
                            "newText": { "type": "string", "description": "Replacement text for this targeted edit." }
                        },
                        "required": ["oldText", "newText"]
                    }
                }
            },
            "required": ["path", "edits"]
        }))
    }
    /// Upstream `edit.ts:156`: the edit tool prefers strict JSON-schema
    /// constrained sampling.
    fn constrained_sampling(&self) -> Option<crate::ai::types::ConstrainedSamplingConfig> {
        Some(crate::ai::types::ConstrainedSamplingConfig::JsonSchema {
            strict: crate::ai::types::StrictPreference::Prefer,
        })
    }
    fn run(&self, args: &serde_json::Value, ctx: &ToolContext) -> BoxFuture<'static, ToolResult> {
        let (path, _) = single_path_arg(args, &ctx.cwd);
        let edits = match parse_edit_requests(args) {
            Ok(edits) => edits,
            Err(error) => return Box::pin(async move { ToolResult::error(error) }),
        };
        let cwd = ctx.cwd.clone();
        let aborted = ctx.is_aborted();
        let signal = ctx.signal.clone();
        // The env indirection layer (`env_client::selection`): `None` — the
        // default — leaves the local path below byte-identical.
        let remote = crate::env_client::configured_remote_env();
        let operations = self.operations.clone();
        Box::pin(async move {
            if aborted {
                return ToolResult::error("Operation aborted");
            }
            if let Some(operations) = operations {
                return edit_with_operations(operations, &cwd, &path, &edits, signal).await;
            }
            if let Some(environment) = remote {
                return edit_remote(environment, &path, &edits, signal).await;
            }
            // Name-based confinement first (model-facing errors), then the
            // read and the replacement through a handle anchored at the
            // verified parent directory: a component swapped after the check
            // cannot redirect either side (no resolve-then-open window).
            let parts = match workspace_path_parts(&cwd, &path) {
                Ok(parts) => parts,
                Err(error) => return ToolResult::error(error),
            };
            let target = match ConfinedTarget::open(&parts.root, &parts.relative, false) {
                Ok(target) => target,
                Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => {
                    return ToolResult::error(super::confine::confinement_error(&path, &error));
                }
                Err(error) => {
                    return ToolResult::error(format!(
                        "Could not edit file: {path}. {}.",
                        access_error_message(&error)
                    ))
                }
            };
            // Read as bytes so an invalid-UTF-8 (binary) file is reported
            // cleanly instead of surfacing a lossy conversion error. The
            // source-size cap bounds the whole-file buffer this path needs.
            let file_bytes = match read_confined(
                &target,
                signal.as_ref(),
                max_source_bytes(),
                LOCAL_IO_CHUNK_BYTES,
                source_limit_error,
            )
            .await
            {
                Ok(Some(bytes)) => bytes,
                Ok(None) => return ToolResult::error("Operation aborted"),
                Err(e) if e.kind() == std::io::ErrorKind::InvalidData => {
                    return ToolResult::error(format!("Cannot edit '{}': {}", path, e))
                }
                Err(e) => {
                    // Upstream access failure shape:
                    // `Could not edit file: <p>. Error code: <code>.`
                    return ToolResult::error(format!(
                        "Could not edit file: {path}. {}.",
                        access_error_message(&e)
                    ));
                }
            };
            let raw_content = match String::from_utf8(file_bytes) {
                Ok(content) => content,
                Err(_) => return ToolResult::error(format!("Cannot edit '{}': binary file", path)),
            };
            let transformed = match transform_edit_content(raw_content, &edits, &path) {
                Ok(transformed) => transformed,
                Err(error) => return ToolResult::error(error),
            };
            match target
                .write_atomically(
                    transformed.final_content.as_bytes(),
                    signal.as_ref(),
                    LOCAL_IO_CHUNK_BYTES,
                )
                .await
            {
                Ok(true) => edit_success_result(&path, &edits, &transformed),
                Ok(false) => ToolResult::error("Operation aborted"),
                Err(e) => ToolResult::error(format!("Cannot write '{}': {}", path, e)),
            }
        })
    }
}

/// Wait until the signal aborts; never resolves when no signal is installed.
async fn wait_for_abort(signal: Option<&AbortSignal>) {
    match signal {
        Some(signal) => signal.cancelled().await,
        None => std::future::pending::<()>().await,
    }
}

/// Race one local filesystem await against the abort signal. `None` means the
/// signal fired first; otherwise the await's own result is returned unchanged.
async fn cancellable<T>(
    signal: Option<&AbortSignal>,
    future: impl std::future::Future<Output = T>,
) -> Option<T> {
    tokio::select! {
        value = future => Some(value),
        _ = wait_for_abort(signal) => None,
    }
}

/// The pure middle of an edit: the file's text in, the content to write out
/// plus the diff inputs. Shared by the local arm and the remote arm
/// ([`edit_remote`]), so one edit applies identically on either machine.
struct TransformedEdit {
    final_content: String,
    base_content: String,
    new_content: String,
}

fn transform_edit_content(
    raw_content: String,
    edits: &[EditRequest],
    path: &str,
) -> Result<TransformedEdit, String> {
    // Strip BOM before matching (the model never includes it in oldText); it is
    // restored after writing. CRLF files are matched in LF space and line
    // endings are restored afterwards.
    let (has_bom, content) = split_bom(&raw_content);
    let original_ending = detect_line_ending(content);
    let normalized_content = normalize_to_lf(content);
    let (base_content, new_content) =
        apply_edits_to_normalized_content(&normalized_content, edits, path)?;
    let mut final_content = String::new();
    if has_bom {
        final_content.push(BOM);
    }
    final_content.push_str(&restore_line_endings(new_content.clone(), original_ending));
    Ok(TransformedEdit {
        final_content,
        base_content,
        new_content,
    })
}

/// The model-facing refusal for a whole-file read that exceeded the source
/// cap. Shared by the two arms that materialize the file through a backend
/// (`edit_with_operations`, `edit_remote`); the local arm's `read_confined`
/// enforces the same cap while streaming and maps the check through
/// `source_limit_error` itself.
///
/// The check must run on the bytes actually read, not only on a prior `stat`:
/// a file that grows between the stat and the read would otherwise be
/// materialized without a bound (`edit_remote` used to trust `file_info`).
/// `None` means the payload is within the cap.
fn source_limit_refusal(path: &str, bytes: u64) -> Option<ToolResult> {
    (bytes > max_source_bytes()).then(|| {
        ToolResult::error(format!(
            "Cannot edit '{}': {}",
            path,
            source_limit_error(bytes)
        ))
    })
}

/// The success result of an applied edit: the model-visible text is ONLY the
/// success line; the display diff and unified patch are UI-only
/// (`details.{diff, patch, firstChangedLine}`).
fn edit_success_result(
    path: &str,
    edits: &[EditRequest],
    transformed: &TransformedEdit,
) -> ToolResult {
    let patch = generate_unified_patch(path, &transformed.base_content, &transformed.new_content);
    let (diff, first_changed_line) =
        generate_diff_string(&transformed.base_content, &transformed.new_content);
    let mut result = ToolResult::ok(format!(
        "Successfully replaced {} block(s) in {path}.",
        edits.len()
    ));
    let mut details = serde_json::Map::new();
    details.insert("diff".to_owned(), serde_json::json!(diff));
    details.insert("patch".to_owned(), serde_json::json!(patch));
    if let Some(line) = first_changed_line {
        details.insert("firstChangedLine".to_owned(), serde_json::json!(line));
    }
    result.details = Some(serde_json::Value::Object(details));
    result
}

/// The pluggable-backend arm of the edit tool: the same normalized,
/// workspace-confined target the local arm resolves, then access → read →
/// transform → write through the backend. Messages match the local arm.
async fn edit_with_operations(
    operations: std::sync::Arc<dyn EditOperations>,
    cwd: &Path,
    path: &str,
    edits: &[EditRequest],
    signal: Option<AbortSignal>,
) -> ToolResult {
    let parts = match workspace_path_parts(cwd, path) {
        Ok(parts) => parts,
        Err(error) => return ToolResult::error(error),
    };
    let target = parts.candidate;
    if let Err(error) = operations.access(&target).await {
        return ToolResult::error(format!(
            "Could not edit file: {path}. {}.",
            access_error_message(&error)
        ));
    }
    if signal.as_ref().is_some_and(AbortSignal::is_aborted) {
        return ToolResult::error("Operation aborted");
    }
    let file_bytes = match operations.read_file(&target).await {
        Ok(bytes) => bytes,
        Err(error) => {
            return ToolResult::error(format!(
                "Could not edit file: {path}. {}.",
                access_error_message(&error)
            ))
        }
    };
    if signal.as_ref().is_some_and(AbortSignal::is_aborted) {
        return ToolResult::error("Operation aborted");
    }
    if let Some(refusal) = source_limit_refusal(path, file_bytes.len() as u64) {
        return refusal;
    }
    let raw_content = match String::from_utf8(file_bytes) {
        Ok(content) => content,
        Err(_) => return ToolResult::error(format!("Cannot edit '{}': binary file", path)),
    };
    let transformed = match transform_edit_content(raw_content, edits, path) {
        Ok(transformed) => transformed,
        Err(error) => return ToolResult::error(error),
    };
    match operations
        .write_file(&target, &transformed.final_content)
        .await
    {
        Ok(()) if signal.as_ref().is_some_and(AbortSignal::is_aborted) => {
            ToolResult::error("Operation aborted")
        }
        Ok(()) => edit_success_result(path, edits, &transformed),
        Err(error) => ToolResult::error(format!("Cannot write '{}': {}", path, error)),
    }
}

/// The remote arm of the edit tool (the env indirection layer's `Some`
/// branch): read, transform and write through `RemoteExecutionEnv`, with the
/// same messages as the local arm ([`access_error_message`]'s remote form maps
/// the daemon's Node-style codes back to the codes the local arm prints).
async fn edit_remote(
    environment: std::sync::Arc<crate::env_client::RemoteExecutionEnv>,
    path: &str,
    edits: &[EditRequest],
    signal: Option<AbortSignal>,
) -> ToolResult {
    let context = crate::env_client::Context::new(signal.clone());
    let file_path = match cancellable(
        signal.as_ref(),
        workspace_path_remote_checked(&environment, path),
    )
    .await
    {
        Some(Ok(path)) => path,
        Some(Err(error)) => return ToolResult::error(error),
        None => return ToolResult::error("Operation aborted"),
    };
    let file_size = match environment.file_info(&file_path, &context).await {
        Ok(info) => info.size,
        Err(error) if error.code == crate::env_client::FileErrorCode::Aborted => {
            return ToolResult::error("Operation aborted")
        }
        Err(error) => {
            return ToolResult::error(format!(
                "Could not edit file: {path}. {}.",
                access_error_message_remote(&error)
            ));
        }
    };
    if file_size > max_source_bytes() {
        return ToolResult::error(format!(
            "Cannot edit '{}': {}",
            path,
            source_limit_error(file_size)
        ));
    }
    let file_bytes = match environment.read_binary_file(&file_path, &context).await {
        Ok(bytes) => bytes,
        Err(error) if error.code == crate::env_client::FileErrorCode::Aborted => {
            return ToolResult::error("Operation aborted")
        }
        Err(error) => {
            return ToolResult::error(format!(
                "Could not edit file: {path}. {}.",
                access_error_message_remote(&error)
            ));
        }
    };
    if context.aborted() {
        return ToolResult::error("Operation aborted");
    }
    // The `file_info` size above is only a fast pre-check: the bytes the
    // daemon actually returned are what this arm materializes, so the cap is
    // re-applied to them (the other two arms do the same). A file that grew
    // between the stat and the read can therefore not bypass the bound.
    if let Some(refusal) = source_limit_refusal(path, file_bytes.len() as u64) {
        return refusal;
    }
    let raw_content = match String::from_utf8(file_bytes) {
        Ok(content) => content,
        Err(_) => return ToolResult::error(format!("Cannot edit '{}': binary file", path)),
    };
    let transformed = match transform_edit_content(raw_content, edits, path) {
        Ok(transformed) => transformed,
        Err(error) => return ToolResult::error(error),
    };
    match write_remote_file_atomically(
        &environment,
        &file_path,
        transformed.final_content.as_bytes(),
        &context,
    )
    .await
    {
        Ok(()) => edit_success_result(path, edits, &transformed),
        Err(error) if error.code == crate::env_client::FileErrorCode::Aborted => {
            ToolResult::error("Operation aborted")
        }
        Err(error) => ToolResult::error(format!("Cannot write '{}': {}", path, error.message)),
    }
}

/// The remote analogue of [`access_error_message`]: the daemon reports Node's
/// error code, which maps back to the code the local arm would print for the
/// same failure.
fn access_error_message_remote(error: &crate::env_client::FileError) -> String {
    use crate::env_client::FileErrorCode;
    match error.code {
        FileErrorCode::NotFound => "Error code: ENOENT".to_owned(),
        FileErrorCode::PermissionDenied => "Error code: EACCES".to_owned(),
        FileErrorCode::NotDirectory => "Error code: ENOTDIR".to_owned(),
        FileErrorCode::IsDirectory => "Error code: EISDIR".to_owned(),
        FileErrorCode::Invalid => "Error code: EINVAL".to_owned(),
        FileErrorCode::Aborted => "Error code: ABORT_ERR".to_owned(),
        FileErrorCode::NotSupported | FileErrorCode::Unknown => {
            format!("Error code: {}", error.message)
        }
    }
}

/// Upstream access-error message: `Error code: <code>` for a coded OS error,
/// the error text otherwise.
/// Upstream `computeEditsDiff` (edit-diff.ts:514-543): compute the display
/// diff for one or more edits WITHOUT applying them. Backs the TUI's live
/// edit preview (the box shows the resolved diff before execution finishes).
/// Returns the line-numbered display diff plus the first changed line.
#[cfg(feature = "tui")]
pub(crate) fn compute_edits_diff(
    path: &str,
    edits: &[(String, String, bool)],
    cwd: &Path,
) -> Result<(String, Option<usize>), String> {
    use std::io::Read;

    // The preview must obey the same confinement as the edit itself: an
    // absolute path outside the workspace is not readable here either, and the
    // read goes through the same handle-relative no-follow open as execution
    // (a symlink swapped in after the name check cannot make the preview
    // render outside content).
    let parts = workspace_path_parts(cwd, path)?;
    let target = ConfinedTarget::open(&parts.root, &parts.relative, false).map_err(|error| {
        format!(
            "Could not edit file: {path}. {}.",
            access_error_message(&error)
        )
    })?;
    let file = target.open_read().map_err(|error| {
        format!(
            "Could not edit file: {path}. {}.",
            access_error_message(&error)
        )
    })?;
    match file.metadata() {
        Ok(metadata) if metadata.len() > max_source_bytes() => {
            return Err(format!(
                "Cannot edit '{path}': {}",
                source_limit_error(metadata.len())
            ));
        }
        Ok(_) => {}
        Err(error) => {
            // Upstream `computeEditsDiff` access-check failure text.
            return Err(format!(
                "Could not edit file: {path}. {}.",
                access_error_message(&error)
            ));
        }
    }
    let mut file_bytes = Vec::new();
    if let Err(error) = file
        .take(max_source_bytes().saturating_add(1))
        .read_to_end(&mut file_bytes)
    {
        // Upstream `computeEditsDiff` access-check failure text.
        return Err(format!(
            "Could not edit file: {path}. {}.",
            access_error_message(&error)
        ));
    }
    if file_bytes.len() as u64 > max_source_bytes() {
        return Err(format!(
            "Cannot edit '{path}': {}",
            source_limit_error(file_bytes.len() as u64)
        ));
    }
    let raw_content = match String::from_utf8(file_bytes) {
        Ok(content) => content,
        Err(_) => return Err(format!("Cannot edit '{path}': binary file")),
    };
    // Strip BOM before matching (the model never includes it in oldText) and
    // match in LF space, mirroring the execution path.
    let (_, content) = split_bom(&raw_content);
    let normalized_content = normalize_to_lf(content);
    let requests: Vec<EditRequest> = edits
        .iter()
        .map(|(old_text, new_text, replace_all)| EditRequest {
            old_text: old_text.clone(),
            new_text: new_text.clone(),
            replace_all: *replace_all,
        })
        .collect();
    let (base_content, new_content) =
        apply_edits_to_normalized_content(&normalized_content, &requests, path)?;
    Ok(generate_diff_string(&base_content, &new_content))
}

fn access_error_message(error: &std::io::Error) -> String {
    let code = match error.kind() {
        std::io::ErrorKind::NotFound => Some("ENOENT"),
        std::io::ErrorKind::PermissionDenied => Some("EACCES"),
        std::io::ErrorKind::AlreadyExists => Some("EEXIST"),
        _ => None,
    };
    match code {
        Some(code) => format!("Error code: {code}"),
        None => format!("Error code: {}", error),
    }
}
