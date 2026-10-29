//! The built-in `sd` tool: search & replace over MCP, hosted by `pir mcp-sd`.
//!
//! [`sd`](https://github.com/chmln/sd) is the sed alternative that trades the
//! sed/awk DSL for a plain regex plus capture references. This module is that
//! tool as an MCP **server** surface: [`SdServer`] exposes exactly one tool,
//! [`TOOL_NAME`], over stdio, and [`run`] is the engine behind it (also driven
//! directly by the unit tests, which need no process).
//!
//! ## Tool contract
//!
//! - `pattern` / `replacement` are required. The replacement uses sd's capture
//!   references: `$1`, `${name}`, and `$$` for a literal `$` (the `regex`
//!   crate's `Captures::expand`, which is what sd itself calls).
//! - `literal` escapes the pattern (`sd -F`), `whole_word` wraps it as
//!   `\b(?:…)\b` (`sd -w`), `case_insensitive` (`sd -i`) and `multiline`
//!   (`sd -m`) apply the matching regex flags; without `multiline` the
//!   replacement runs line by line (sd's default: a pattern cannot match
//!   across a line boundary), with CRLF endings preserved and the `\r` kept
//!   out of the matched line so `$` anchors where users expect.
//! - `paths` defaults to the server's `--root`. Entries are files or
//!   directories, relative to the root or absolute; a directory is walked with
//!   the `ignore` crate's filters (`.gitignore`/`.ignore` honored even outside
//!   a git repository, hidden entries skipped, symlinks never followed), and
//!   an explicitly named file is honored as named.
//! - `dry_run` defaults to **true**: the call returns the preview and writes
//!   nothing. `dry_run: false` rewrites the matched files in place; files with
//!   no match are never opened for writing.
//! - `max_matches` / `max_files` bound one call. Both are optional; when one
//!   is hit the work stops there and the summary reports `truncated: true`
//!   instead of silently finishing.
//!
//! ## Guards
//!
//! - Every requested path is canonicalized and must resolve **inside** the
//!   canonical `--root`; a path that resolves outside is refused with an error
//!   (never followed), and directory walks never follow symlinks.
//! - Binary files (a NUL byte in the first [`BINARY_SNIFF_BYTES`] bytes) and
//!   files that are not valid UTF-8 are skipped and counted, never rewritten.
//! - vcs internals are never walked (a `.git` component is skipped even when a
//!   path entry names it).
//!
//! ## Result
//!
//! `tools/call` answers with both a human-readable text block and structured
//! content: `{dry_run, files_scanned, files_changed, replacements, truncated,
//! binary_skipped, errors, files: [{path, replacements, preview}]}` — see
//! [`SdSummary::to_json`]. The text block is preview-bounded
//! ([`PREVIEW_TOTAL_BYTES`]) and says so when it clips; the structured summary
//! is complete apart from the per-file previews ([`PREVIEW_MATCHES_PER_FILE`]).

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use regex::{Regex, RegexBuilder};
use rmcp::model::{
    CallToolRequestMethod, CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock,
    ErrorData, Implementation, ListToolsResult, PaginatedRequestParams, ServerCapabilities,
    ServerConfig, Tool,
};
use rmcp::service::RequestContext;
use rmcp::{RoleServer, ServerHandler, ServiceExt};
use serde_json::{json, Map as JsonMap, Value as JsonValue};

/// The one tool this server exposes.
pub const TOOL_NAME: &str = "sd";

/// The server identity reported by `initialize`.
pub const SERVER_NAME: &str = "pir-mcp-sd";

/// Matches shown per file in a preview (the rest are counted, not printed).
const PREVIEW_MATCHES_PER_FILE: usize = 3;

/// Cap of one matched/replaced snippet inside a preview line.
const PREVIEW_SNIPPET_CHARS: usize = 120;

/// Cap of the aggregate human-readable preview in the text content block.
const PREVIEW_TOTAL_BYTES: usize = 16 * 1024;

/// Bytes sniffed for a NUL byte when classifying a file as binary.
const BINARY_SNIFF_BYTES: usize = 8192;

/// Instructions sent with `initialize` (what a model sees about this server).
const SERVER_INSTRUCTIONS: &str =
    "Built-in sd (search & replace) server hosted by the pir executable. \
The `sd` tool replaces a regex `pattern` across files under the server root and returns a preview \
(dry_run defaults to true). Set dry_run to false to write the changes in place.";

/// Tool description (what a model sees about the tool).
const TOOL_DESCRIPTION: &str = "Search and replace across files with sd semantics: a plain regex \
`pattern` (or a literal string with `literal`), and a `replacement` with capture references \
`$1`, `${name}`, and `$$` for a literal `$`. Walks `paths` (default: the server root) honoring \
.gitignore and skipping hidden, binary and non-UTF-8 files. `dry_run` defaults to true: the call \
returns a preview and writes nothing; pass `dry_run: false` to rewrite the matched files in place. \
`max_matches` / `max_files` bound one call and are reported as `truncated`.";

// ---------------------------------------------------------------------------
// Request
// ---------------------------------------------------------------------------

/// One parsed `sd` tool call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SdRequest {
    /// Regex (or literal) pattern to match.
    pub pattern: String,
    /// Replacement text with `$1` / `${name}` / `$$` references.
    pub replacement: String,
    /// Files or directories, relative to the root or absolute. Empty = root.
    pub paths: Vec<String>,
    /// Treat `pattern` as a literal string (`sd -F`).
    pub literal: bool,
    /// Case-insensitive matching (`sd -i`).
    pub case_insensitive: bool,
    /// Match only whole words (`sd -w`).
    pub whole_word: bool,
    /// Let the pattern span lines (`sd -m`); without it the replacement runs
    /// line by line.
    pub multiline: bool,
    /// Preview only (default true) vs write in place.
    pub dry_run: bool,
    /// Cap on replacements in this call (optional).
    pub max_matches: Option<usize>,
    /// Cap on files scanned in this call (optional).
    pub max_files: Option<usize>,
}

impl SdRequest {
    /// Parse and validate one `tools/call` argument object.
    ///
    /// Every failure is a caller-visible message (the handler turns it into an
    /// `invalid_params` error): a missing required field, an empty `pattern`
    /// or `path`, a negative limit, or an unknown key — a typo like `dryrun`
    /// must not silently mean "preview only".
    pub fn from_arguments(arguments: &JsonMap<String, JsonValue>) -> Result<Self, String> {
        const KEYS: &[&str] = &[
            "pattern",
            "replacement",
            "paths",
            "literal",
            "case_insensitive",
            "whole_word",
            "multiline",
            "dry_run",
            "max_matches",
            "max_files",
        ];
        for key in arguments.keys() {
            if !KEYS.contains(&key.as_str()) {
                return Err(format!(
                    "unknown argument {key:?}; accepted arguments: {}",
                    KEYS.join(", ")
                ));
            }
        }
        let pattern = required_string(arguments, "pattern")?;
        if pattern.is_empty() {
            return Err("pattern must not be empty".to_owned());
        }
        Ok(Self {
            pattern,
            replacement: required_string(arguments, "replacement")?,
            paths: optional_paths(arguments)?,
            literal: optional_bool(arguments, "literal")?,
            case_insensitive: optional_bool(arguments, "case_insensitive")?,
            whole_word: optional_bool(arguments, "whole_word")?,
            multiline: optional_bool(arguments, "multiline")?,
            dry_run: optional_bool_with_default(arguments, "dry_run", true)?,
            max_matches: optional_limit(arguments, "max_matches")?,
            max_files: optional_limit(arguments, "max_files")?,
        })
    }
}

fn required_string(arguments: &JsonMap<String, JsonValue>, key: &str) -> Result<String, String> {
    match arguments.get(key) {
        Some(JsonValue::String(value)) => Ok(value.clone()),
        Some(_) => Err(format!("{key} must be a string")),
        None => Err(format!("missing required argument: {key}")),
    }
}

fn optional_bool(arguments: &JsonMap<String, JsonValue>, key: &str) -> Result<bool, String> {
    optional_bool_with_default(arguments, key, false)
}

fn optional_bool_with_default(
    arguments: &JsonMap<String, JsonValue>,
    key: &str,
    default: bool,
) -> Result<bool, String> {
    match arguments.get(key) {
        Some(JsonValue::Bool(value)) => Ok(*value),
        Some(_) => Err(format!("{key} must be a boolean")),
        None => Ok(default),
    }
}

fn optional_limit(
    arguments: &JsonMap<String, JsonValue>,
    key: &str,
) -> Result<Option<usize>, String> {
    match arguments.get(key) {
        Some(JsonValue::Number(number)) => match number.as_u64().and_then(|value| {
            usize::try_from(value)
                .ok()
                .filter(|converted| *converted > 0)
        }) {
            Some(value) => Ok(Some(value)),
            None => Err(format!("{key} must be a positive integer")),
        },
        Some(_) => Err(format!("{key} must be a positive integer")),
        None => Ok(None),
    }
}

fn optional_paths(arguments: &JsonMap<String, JsonValue>) -> Result<Vec<String>, String> {
    match arguments.get("paths") {
        Some(JsonValue::Array(entries)) => {
            let mut paths = Vec::with_capacity(entries.len());
            for entry in entries {
                match entry {
                    JsonValue::String(value) if !value.is_empty() => paths.push(value.clone()),
                    JsonValue::String(_) => {
                        return Err("paths entries must not be empty".to_owned());
                    }
                    _ => return Err("paths must be an array of strings".to_owned()),
                }
            }
            Ok(paths)
        }
        Some(_) => Err("paths must be an array of strings".to_owned()),
        None => Ok(Vec::new()),
    }
}

// ---------------------------------------------------------------------------
// Summary
// ---------------------------------------------------------------------------

/// One changed (or, in a dry run, would-change) file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileChange {
    /// Path relative to the server root, `/`-separated for MCP readership.
    pub path: String,
    /// Replacements in this file.
    pub replacements: usize,
    /// Bounded per-match preview (`line N: \`match\` -> \`replacement\``).
    pub preview: String,
}

/// The `sd` tool's result: what was scanned, what changed, and why the result
/// may be partial.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SdSummary {
    /// Whether the call wrote anything (false = preview only).
    pub dry_run: bool,
    /// Files read and inspected.
    pub files_scanned: usize,
    /// Files with at least one replacement (in a dry run: would change).
    pub files_changed: usize,
    /// Total replacements performed (in a dry run: previewed).
    pub replacements: usize,
    /// True when a limit clipped the work or the preview text: matches, files
    /// or previews are missing from this summary.
    pub truncated: bool,
    /// Files skipped as binary or non-UTF-8.
    pub binary_skipped: usize,
    /// Per-path failures (read/write/walk), each `path: message`.
    pub errors: Vec<String>,
    /// One entry per changed file, in walk order.
    pub files: Vec<FileChange>,
}

impl SdSummary {
    /// The structured `tools/call` content.
    pub fn to_json(&self) -> JsonValue {
        json!({
            "dry_run": self.dry_run,
            "files_scanned": self.files_scanned,
            "files_changed": self.files_changed,
            "replacements": self.replacements,
            "truncated": self.truncated,
            "binary_skipped": self.binary_skipped,
            "errors": self.errors,
            "files": self
                .files
                .iter()
                .map(|file| json!({
                    "path": file.path,
                    "replacements": file.replacements,
                    "preview": file.preview,
                }))
                .collect::<Vec<JsonValue>>(),
        })
    }

    /// The human-readable content block, and whether building it hit
    /// [`PREVIEW_TOTAL_BYTES`] (the caller folds that into `truncated`).
    pub fn to_text(&self) -> (String, bool) {
        let mut text = String::new();
        if self.dry_run {
            text.push_str(&format!(
                "sd (dry run): {} file(s) scanned, {} file(s) would change, {} replacement(s)\n",
                self.files_scanned, self.files_changed, self.replacements
            ));
        } else {
            text.push_str(&format!(
                "sd: {} file(s) scanned, {} file(s) changed, {} replacement(s)\n",
                self.files_scanned, self.files_changed, self.replacements
            ));
        }
        let mut clipped = false;
        let mut shown = 0usize;
        for file in &self.files {
            let block = format!(
                "  {}: {} replacement(s)\n{}\n",
                file.path,
                file.replacements,
                indent(&file.preview, 4)
            );
            if text.len() + block.len() > PREVIEW_TOTAL_BYTES {
                clipped = true;
                break;
            }
            text.push_str(&block);
            shown += 1;
        }
        if clipped {
            text.push_str(&format!(
                "  … {} more changed file(s) omitted (preview truncated)\n",
                self.files.len() - shown
            ));
        }
        if self.truncated || clipped {
            text.push_str("truncated: the results above are partial\n");
        }
        if self.binary_skipped > 0 {
            text.push_str(&format!(
                "skipped {} binary or non-UTF-8 file(s)\n",
                self.binary_skipped
            ));
        }
        const MAX_ERRORS_SHOWN: usize = 5;
        if !self.errors.is_empty() {
            text.push_str(&format!("errors ({}):\n", self.errors.len()));
            for error in self.errors.iter().take(MAX_ERRORS_SHOWN) {
                text.push_str(&format!("  {error}\n"));
            }
            if self.errors.len() > MAX_ERRORS_SHOWN {
                text.push_str(&format!(
                    "  … {} more error(s)\n",
                    self.errors.len() - MAX_ERRORS_SHOWN
                ));
            }
        }
        (text, clipped)
    }
}

/// Indent every line of `text` by `spaces` (empty text stays empty).
fn indent(text: &str, spaces: usize) -> String {
    if text.is_empty() {
        return String::new();
    }
    let padding = " ".repeat(spaces);
    text.lines()
        .map(|line| format!("{padding}{line}"))
        .collect::<Vec<String>>()
        .join("\n")
}

// ---------------------------------------------------------------------------
// The replace engine
// ---------------------------------------------------------------------------

/// Build the matcher for one request: escape (literal), word-wrap (whole
/// word), then the flag set. `multi_line(true)` gives sd's line-oriented
/// anchors (`^`/`$` at line boundaries) in both modes; `dot_matches_new_line`
/// is what `multiline` switches.
pub fn build_regex(request: &SdRequest) -> Result<Regex, String> {
    let mut pattern = if request.literal {
        regex::escape(&request.pattern)
    } else {
        request.pattern.clone()
    };
    if request.whole_word {
        pattern = format!(r"\b(?:{pattern})\b");
    }
    RegexBuilder::new(&pattern)
        .case_insensitive(request.case_insensitive)
        .multi_line(true)
        .dot_matches_new_line(request.multiline)
        .build()
        .map_err(|error| format!("invalid pattern: {error}"))
}

/// Replace inside one string, exactly as a file's content would be replaced.
///
/// Public because this is the tool's matching contract: the unit tests pin the
/// capture/literal/flag semantics here without touching the filesystem.
pub fn replace_text(content: &str, request: &SdRequest) -> Result<String, String> {
    Ok(replace_content(
        content,
        &build_regex(request)?,
        &request.replacement,
        request.multiline,
        request.max_matches,
    )
    .content)
}

/// One match worth previewing.
struct MatchSite {
    line: usize,
    matched: String,
    replaced: String,
}

/// One replaced string (or line) plus its preview material.
struct SpanOutcome {
    output: String,
    replacements: usize,
    limit_hit: bool,
    sites: Vec<MatchSite>,
}

/// One file's replacement outcome.
struct ReplaceOutcome {
    content: String,
    replacements: usize,
    limit_hit: bool,
    preview: String,
}

/// Replace `regex` in one file's content: line by line by default (sd's
/// default mode), or across the whole content in multiline mode. `limit` is
/// the remaining global replacement budget.
fn replace_content(
    content: &str,
    regex: &Regex,
    replacement: &str,
    multiline: bool,
    limit: Option<usize>,
) -> ReplaceOutcome {
    let span = if multiline {
        replace_span(content, regex, replacement, 1, limit)
    } else {
        replace_lines(content, regex, replacement, limit)
    };
    let preview = render_preview(&span.sites, span.replacements);
    ReplaceOutcome {
        content: span.output,
        replacements: span.replacements,
        limit_hit: span.limit_hit,
        preview,
    }
}

/// Line-by-line replacement (sd's default): each line is its own haystack, so
/// a pattern cannot match across a line boundary and `^`/`$` anchor at the
/// line. A trailing `\r` stays out of the matched line (so `$` works on CRLF
/// files) and is re-emitted after the replacement.
fn replace_lines(
    content: &str,
    regex: &Regex,
    replacement: &str,
    limit: Option<usize>,
) -> SpanOutcome {
    let mut output = String::with_capacity(content.len());
    let mut replacements = 0usize;
    let mut limit_hit = false;
    let mut sites: Vec<MatchSite> = Vec::new();
    let mut line_number = 1usize;
    let mut first = true;
    for line in content.split('\n') {
        if !first {
            output.push('\n');
        }
        first = false;
        if limit_hit {
            output.push_str(line);
            line_number += 1;
            continue;
        }
        let (body, carriage_return) = match line.strip_suffix('\r') {
            Some(body) => (body, true),
            None => (line, false),
        };
        let remaining = limit.map(|max| max.saturating_sub(replacements));
        let span = replace_span(body, regex, replacement, line_number, remaining);
        output.push_str(&span.output);
        if carriage_return {
            output.push('\r');
        }
        replacements += span.replacements;
        limit_hit = span.limit_hit;
        sites.extend(span.sites);
        line_number += 1;
    }
    SpanOutcome {
        output,
        replacements,
        limit_hit,
        sites,
    }
}

/// One linear replacement pass over `content` (a line, or a whole file in
/// multiline mode), stopping at `limit` matches. Capture references go
/// through `Captures::expand`, i.e. sd's `$1` / `${name}` / `$$`.
fn replace_span(
    content: &str,
    regex: &Regex,
    replacement: &str,
    first_line: usize,
    limit: Option<usize>,
) -> SpanOutcome {
    let mut output = String::with_capacity(content.len());
    let mut replacements = 0usize;
    let mut limit_hit = false;
    let mut sites: Vec<MatchSite> = Vec::new();
    let mut last_end = 0usize;
    let mut counted_to = 0usize;
    let mut line = first_line;
    let mut expanded = String::new();
    for captures in regex.captures_iter(content) {
        if limit.is_some_and(|max| replacements >= max) {
            limit_hit = true;
            break;
        }
        let Some(matched) = captures.get(0) else {
            continue;
        };
        line += content[counted_to..matched.start()].matches('\n').count();
        counted_to = matched.start();
        output.push_str(&content[last_end..matched.start()]);
        expanded.clear();
        captures.expand(replacement, &mut expanded);
        output.push_str(&expanded);
        last_end = matched.end();
        replacements += 1;
        if sites.len() < PREVIEW_MATCHES_PER_FILE {
            sites.push(MatchSite {
                line,
                matched: matched.as_str().to_owned(),
                replaced: expanded.clone(),
            });
        }
    }
    output.push_str(&content[last_end..]);
    SpanOutcome {
        output,
        replacements,
        limit_hit,
        sites,
    }
}

/// Render the per-file preview from the collected sites plus the total match
/// count (so the tail can say honestly how much it is not showing).
fn render_preview(sites: &[MatchSite], total: usize) -> String {
    let mut preview = String::new();
    for site in sites {
        preview.push_str(&format!(
            "line {}: `{}` -> `{}`\n",
            site.line,
            snippet(&site.matched),
            snippet(&site.replaced)
        ));
    }
    if total > sites.len() {
        preview.push_str(&format!("… {} more match(es)\n", total - sites.len()));
    }
    preview.trim_end().to_owned()
}

/// One preview snippet: control characters escaped, capped at
/// [`PREVIEW_SNIPPET_CHARS`] characters.
fn snippet(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len());
    for character in text.chars() {
        match character {
            '\n' => escaped.push_str("\\n"),
            '\r' => escaped.push_str("\\r"),
            '\t' => escaped.push_str("\\t"),
            other => escaped.push(other),
        }
    }
    let mut out: String = escaped.chars().take(PREVIEW_SNIPPET_CHARS).collect();
    if escaped.chars().count() > PREVIEW_SNIPPET_CHARS {
        out.push('…');
    }
    out
}

// ---------------------------------------------------------------------------
// Walking
// ---------------------------------------------------------------------------

/// A file is binary when a NUL byte appears in its first
/// [`BINARY_SNIFF_BYTES`] bytes (the same sniff `grep`-family tools use).
fn looks_binary(bytes: &[u8]) -> bool {
    bytes.iter().take(BINARY_SNIFF_BYTES).any(|byte| *byte == 0)
}

/// Resolve `request.paths` (default: the root itself) to files inside `root`.
///
/// Returns the files in request order, the walk failures that did not stop
/// the walk, and whether `max_files` stopped it. A requested path that does
/// not exist, or that resolves outside `root`, is a hard error for the whole
/// call: a search & replace tool must never quietly touch something else.
fn collect_files(
    root: &Path,
    request: &SdRequest,
) -> Result<(Vec<PathBuf>, Vec<String>, bool), String> {
    let requested: Vec<String> = if request.paths.is_empty() {
        vec![".".to_owned()]
    } else {
        request.paths.clone()
    };
    let mut files: Vec<PathBuf> = Vec::new();
    let mut errors: Vec<String> = Vec::new();
    let mut seen: HashSet<PathBuf> = HashSet::new();
    let mut truncated = false;
    'requested: for entry in &requested {
        let joined = if Path::new(entry).is_absolute() {
            PathBuf::from(entry)
        } else {
            root.join(entry)
        };
        let canonical =
            std::fs::canonicalize(&joined).map_err(|error| format!("path {entry:?}: {error}"))?;
        if !canonical.starts_with(root) {
            return Err(format!(
                "path {entry:?} resolves to {} — outside --root {}; refusing to touch it",
                display_path(&canonical),
                display_path(root)
            ));
        }
        if canonical.is_file() {
            // An explicitly named file is honored as named (sd semantics: the
            // user asked for exactly this path); directories are filtered.
            if seen.insert(canonical.clone()) {
                files.push(canonical);
            }
            if let Some(max) = request.max_files {
                if files.len() >= max {
                    truncated = true;
                    break 'requested;
                }
            }
            continue;
        }
        if !canonical.is_dir() {
            return Err(format!("path {entry:?} is neither a file nor a directory"));
        }
        let mut builder = ignore::WalkBuilder::new(&canonical);
        // Honor .gitignore/.ignore even outside a git repository (ripgrep's
        // practical behavior, shared with the grep/find tools).
        builder.require_git(false);
        // Walker defaults are kept on purpose: hidden files and directories
        // are skipped (`hidden = true`), which also keeps `.git` internals out
        // of a whole-root rewrite, and symlinks are never followed
        // (`follow_links = false`), so a directory walk cannot leave the root.
        for walked in builder.build() {
            let walked = match walked {
                Ok(walked) => walked,
                Err(error) => {
                    errors.push(error.to_string());
                    continue;
                }
            };
            if walked
                .path()
                .components()
                .any(|component| component.as_os_str() == ".git")
            {
                continue;
            }
            if !walked
                .file_type()
                .is_some_and(|file_type| file_type.is_file())
            {
                continue;
            }
            let path = walked.path().to_path_buf();
            if seen.insert(path.clone()) {
                files.push(path);
            }
            if let Some(max) = request.max_files {
                if files.len() >= max {
                    truncated = true;
                    break 'requested;
                }
            }
        }
    }
    Ok((files, errors, truncated))
}

// ---------------------------------------------------------------------------
// The engine entry point
// ---------------------------------------------------------------------------

/// Run one `sd` request against `root`.
///
/// The root is canonicalized here (so containment is decided on resolved
/// paths even when a caller passes a relative or symlinked root; `--root`
/// hands over an already-canonical one). Errors are caller-visible strings
/// (bad regex, a path outside the root, a missing path); per-file failures
/// are collected in the summary instead so one unreadable file does not sink
/// a whole-tree replacement.
pub fn run(root: &Path, request: &SdRequest) -> Result<SdSummary, String> {
    let root = std::fs::canonicalize(root)
        .map_err(|error| format!("root {}: {error}", display_path(root)))?;
    let root = root.as_path();
    let regex = build_regex(request)?;
    let (files, errors, truncated) = collect_files(root, request)?;
    let mut summary = SdSummary {
        dry_run: request.dry_run,
        truncated,
        errors,
        ..SdSummary::default()
    };
    let mut remaining = request.max_matches;
    for file in &files {
        if remaining == Some(0) {
            // The replacement budget is spent and files remain: the call is
            // honestly partial (never a silent no-op).
            summary.truncated = true;
            break;
        }
        summary.files_scanned += 1;
        let bytes = match std::fs::read(file) {
            Ok(bytes) => bytes,
            Err(error) => {
                summary
                    .errors
                    .push(format!("{}: {error}", display_path(file)));
                continue;
            }
        };
        if looks_binary(&bytes) {
            summary.binary_skipped += 1;
            continue;
        }
        let Ok(content) = String::from_utf8(bytes) else {
            // Invalid UTF-8 is as untouchable as a binary blob here: a
            // lossy rewrite would corrupt bytes the caller cannot see.
            summary.binary_skipped += 1;
            continue;
        };
        let outcome = replace_content(
            &content,
            &regex,
            &request.replacement,
            request.multiline,
            remaining,
        );
        if outcome.replacements == 0 {
            continue; // No match: the file is never opened for writing.
        }
        if !request.dry_run {
            if let Err(error) = std::fs::write(file, outcome.content.as_bytes()) {
                summary
                    .errors
                    .push(format!("{}: {error}", display_path(file)));
                continue;
            }
        }
        summary.files_changed += 1;
        summary.replacements += outcome.replacements;
        summary.truncated |= outcome.limit_hit;
        if let Some(max) = remaining {
            remaining = Some(max.saturating_sub(outcome.replacements));
        }
        summary.files.push(FileChange {
            path: relative_path(root, file),
            replacements: outcome.replacements,
            preview: outcome.preview,
        });
    }
    Ok(summary)
}

/// A path as reported in a summary: relative to the root, `/`-separated.
fn relative_path(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

/// A path for diagnostics: absolute, without Windows' canonical `\\?\` prefix.
fn display_path(path: &Path) -> String {
    let text = path.to_string_lossy();
    text.strip_prefix(r"\\?\")
        .unwrap_or(text.as_ref())
        .to_owned()
}

// ---------------------------------------------------------------------------
// The MCP server
// ---------------------------------------------------------------------------

/// The `sd` MCP server: one root, one tool.
#[derive(Debug, Clone)]
pub struct SdServer {
    root: PathBuf,
}

impl SdServer {
    /// A server bound to `root` (already canonicalized by
    /// [`super::run_main`]).
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    /// The `sd` tool declaration: a hand-built JSON Schema (rmcp's `macros`
    /// and `schemars` features are off in this lock) plus the structured
    /// output schema.
    pub fn tool() -> Tool {
        Tool::new(TOOL_NAME, TOOL_DESCRIPTION, input_schema())
            .with_raw_output_schema(Arc::new(output_schema()))
    }
}

impl ServerHandler for SdServer {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new(SERVER_NAME, env!("CARGO_PKG_VERSION")))
            .with_instructions(SERVER_INSTRUCTIONS)
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        Ok(ListToolsResult::with_all_items(vec![Self::tool()]))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        if request.name.as_ref() != TOOL_NAME {
            return Err(ErrorData::method_not_found::<CallToolRequestMethod>());
        }
        let arguments = request.arguments.unwrap_or_default();
        let parsed = SdRequest::from_arguments(&arguments)
            .map_err(|message| ErrorData::invalid_params(message, None))?;
        let root = self.root.clone();
        // The walk + rewrite is blocking work; keep the transport's task free.
        let outcome = tokio::task::spawn_blocking(move || run(&root, &parsed)).await;
        match outcome {
            Ok(Ok(summary)) => {
                let (text, clipped) = summary.to_text();
                let mut summary = summary;
                summary.truncated |= clipped;
                let mut result = CallToolResult::structured(summary.to_json());
                result.content = vec![ContentBlock::text(text)];
                Ok(CallToolResponse::Complete(result))
            }
            // A request-level failure (bad regex, path outside the root) is a
            // tool-level error: the caller sees the message.
            Ok(Err(message)) => Ok(CallToolResponse::Complete(CallToolResult::error(vec![
                ContentBlock::text(message),
            ]))),
            Err(join_error) => Err(ErrorData::internal_error(
                format!("sd worker failed: {join_error}"),
                None,
            )),
        }
    }
}

/// Serve the `sd` tool on stdio until the client closes stdin (EOF) or the
/// transport drops. Nothing but MCP frames is written to stdout; the startup
/// line goes to stderr.
pub async fn serve_stdio(root: PathBuf) -> Result<(), String> {
    let display = display_path(&root);
    let handler = SdServer::new(root);
    let service = handler
        .serve(rmcp::transport::io::stdio())
        .await
        .map_err(|error| format!("initialize failed: {error}"))?;
    eprintln!("{SERVER_NAME}: serving {display}");
    service
        .waiting()
        .await
        .map_err(|error| format!("serve task failed: {error}"))?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Schemas
// ---------------------------------------------------------------------------

/// A JSON object map from `(key, value)` pairs (no `unwrap` on `json!`).
fn object(entries: Vec<(&str, JsonValue)>) -> JsonMap<String, JsonValue> {
    entries
        .into_iter()
        .map(|(key, value)| (key.to_owned(), value))
        .collect()
}

/// The tool's input schema.
fn input_schema() -> JsonMap<String, JsonValue> {
    object(vec![
        ("type", json!("object")),
        (
            "properties",
            JsonValue::Object(object(vec![
                (
                    "pattern",
                    json!({
                        "type": "string",
                        "minLength": 1,
                        "description": "Regex to match (a literal string when `literal` is true).",
                    }),
                ),
                (
                    "replacement",
                    json!({
                        "type": "string",
                        "description": "Replacement text; capture references `$1`, `${name}`, and `$$` for a literal `$`.",
                    }),
                ),
                (
                    "paths",
                    json!({
                        "type": "array",
                        "items": {"type": "string"},
                        "description": "Files or directories to process, relative to the server root or absolute. Default: the whole root.",
                    }),
                ),
                (
                    "literal",
                    json!({
                        "type": "boolean",
                        "default": false,
                        "description": "Treat `pattern` as a literal string instead of a regex.",
                    }),
                ),
                (
                    "case_insensitive",
                    json!({
                        "type": "boolean",
                        "default": false,
                        "description": "Case-insensitive matching.",
                    }),
                ),
                (
                    "whole_word",
                    json!({
                        "type": "boolean",
                        "default": false,
                        "description": "Match whole words only (word boundaries around the pattern).",
                    }),
                ),
                (
                    "multiline",
                    json!({
                        "type": "boolean",
                        "default": false,
                        "description": "Let the pattern span lines (`.` matches newline). Default: line-by-line matching.",
                    }),
                ),
                (
                    "dry_run",
                    json!({
                        "type": "boolean",
                        "default": true,
                        "description": "Preview only (default true). Set to false to write changes in place.",
                    }),
                ),
                (
                    "max_matches",
                    json!({
                        "type": "integer",
                        "minimum": 1,
                        "description": "Stop after this many replacements and report `truncated`.",
                    }),
                ),
                (
                    "max_files",
                    json!({
                        "type": "integer",
                        "minimum": 1,
                        "description": "Stop after scanning this many files and report `truncated`.",
                    }),
                ),
            ])),
        ),
        ("required", json!(["pattern", "replacement"])),
        ("additionalProperties", json!(false)),
    ])
}

/// The tool's structured output schema (matches [`SdSummary::to_json`]).
fn output_schema() -> JsonMap<String, JsonValue> {
    object(vec![
        ("type", json!("object")),
        (
            "properties",
            JsonValue::Object(object(vec![
                ("dry_run", json!({"type": "boolean"})),
                ("files_scanned", json!({"type": "integer"})),
                ("files_changed", json!({"type": "integer"})),
                ("replacements", json!({"type": "integer"})),
                ("truncated", json!({"type": "boolean"})),
                ("binary_skipped", json!({"type": "integer"})),
                (
                    "errors",
                    json!({"type": "array", "items": {"type": "string"}}),
                ),
                (
                    "files",
                    json!({
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "path": {"type": "string"},
                                "replacements": {"type": "integer"},
                                "preview": {"type": "string"},
                            },
                            "required": ["path", "replacements", "preview"],
                        },
                    }),
                ),
            ])),
        ),
        (
            "required",
            json!([
                "dry_run",
                "files_scanned",
                "files_changed",
                "replacements",
                "truncated",
                "binary_skipped",
                "errors",
                "files"
            ]),
        ),
    ])
}
