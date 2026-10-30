//! The `context-mode` builtin plugin: "think in code" execution plus a
//! persistent FTS5 knowledge base.
//!
//! The paradigm (ported natively from the `context-mode` npm plugin): the
//! model writes a Python script that analyzes data inside the embedded
//! RustPython sandbox, and only the script's stdout enters the transcript.
//! Raw file bytes, fetched pages and indexed sections never enter context —
//! they stay in the sandbox/DB and are recalled by topic via
//! `ctx_search`.
//!
//! Tools: [`CTX_EXECUTE_TOOL_NAME`], [`CTX_EXECUTE_FILE_TOOL_NAME`],
//! [`CTX_BATCH_EXECUTE_TOOL_NAME`], [`CTX_INDEX_TOOL_NAME`],
//! [`CTX_SEARCH_TOOL_NAME`], [`CTX_FETCH_AND_INDEX_TOOL_NAME`],
//! [`CTX_STATS_TOOL_NAME`], [`CTX_PURGE_TOOL_NAME`],
//! [`CTX_DOCTOR_TOOL_NAME`].
//!
//! ## Hooks (native PreToolUse / PostToolUse parity)
//!
//! The plugin declares `tool_call`, `tool_result` and `session_start` in
//! [`AgentPlugin::events`] and implements the same two enforcement points the
//! upstream npm plugin installs as hooks, natively (no QuickJS):
//!
//! - **`tool_call` routing** (PreToolUse): a shell command whose executable
//!   is `curl`/`wget` (plus PowerShell `Invoke-WebRequest`/`iwr`) is blocked
//!   with guidance to use `ctx_fetch_and_index` or `ctx_execute`; a simple
//!   `cat`/`head`/`tail`/`less` invocation, or a direct `read`/`read_file`
//!   call, whose single target file is larger than
//!   [`ROUTING_FILE_THRESHOLD_BYTES`] is blocked with guidance to use
//!   `ctx_execute_file`. Only unambiguous single-file commands are inspected
//!   (no pipelines, redirections or globs); everything else — and every
//!   `ctx_*` tool call — is left untouched. The block outcome is the standard
//!   native `{block: true, reason}` merge, so the model sees an error result
//!   and the tool never runs.
//! - **`tool_result` auto-index** (PostToolUse): a finalized result whose
//!   text exceeds [`TOOL_RESULT_INDEX_THRESHOLD_BYTES`] is indexed into the
//!   same FTS5 database under `session:<session id>` (fallback
//!   `tool:<tool name>`), bounded to [`TOOL_RESULT_INDEX_MAX_BYTES`]. The
//!   event payload is never mutated: this is a recall-by-topic side effect,
//!   not a rewrite.
//!
//! Both are on by default (upstream behavior) and independently disableable:
//! `PIR_CONTEXT_MODE_ROUTING=0` turns off blocking and
//! `PIR_CONTEXT_MODE_INDEX_RESULTS=0` turns off auto-indexing (`false`, `off`
//! and `no` are accepted too). `session_start` only records the session id
//! and cwd for the source label; it never opens the database.
//!
//! ## Upstream tools that are intentionally absent
//!
//! Upstream `context-mode` also ships `ctx_upgrade` and `ctx_insight`. Both are
//! npm/cloud-specific: `ctx_upgrade` upgrades the npm package installation and
//! `ctx_insight` queries the vendor's hosted analytics. Neither has a local
//! meaning in a compiled Rust builtin that is versioned with the binary, so
//! they are intentionally not ported. [`CTX_DOCTOR_TOOL_NAME`] is the local
//! diagnostic equivalent: it reports the interpreter, database, FTS5 schema
//! and hook state without contacting any service. The other deliberate
//! deviation is the sandbox: upstream runs the user's native `python3` in a
//! child process, while this port runs scripts on the embedded RustPython 3
//! interpreter (pure-Python stdlib only, no C extensions); the tool contract
//! — write code, only stdout enters context — is unchanged.
//!
//! ## Laziness (the builtin is default-enabled)
//!
//! Constructing [`ContextModePlugin`] allocates the tool objects and the
//! shared state only: no RustPython worker thread/VM is spawned and the
//! SQLite index is not opened. The VM starts on the first execute call (one
//! worker per plugin instance, reused; each job gets a fresh Python
//! namespace), and the DB opens on the first index/search/stats/purge/
//! fetch-cache call.

pub(crate) mod exec;
pub(crate) mod fetch;
pub(crate) mod index;

use std::borrow::Cow;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use serde_json::Value as JsonValue;

use crate::extensions::SessionContextSnapshot;
use crate::plugins::builtin::python_vm::{PythonSession, MAX_OUTPUT_CHARS};
use crate::plugins::plugin::{
    AgentPlugin, EventInterest, PluginEvent, PluginEventOutcome, PluginFuture, RegistrationContext,
};
use crate::plugins::source::PluginInfo;

use index::{IndexDoc, IndexStore};

// ---------------------------------------------------------------------------
// Tool names
// ---------------------------------------------------------------------------

/// Run Python in the embedded sandbox; only stdout enters context.
pub const CTX_EXECUTE_TOOL_NAME: &str = "ctx_execute";
/// Run Python over one confined file (`FILE_CONTENT`); only stdout enters.
pub const CTX_EXECUTE_FILE_TOOL_NAME: &str = "ctx_execute_file";
/// Run several Python analyses in one call; outputs are auto-indexed and the
/// optional queries return matched sections in the same round trip.
pub const CTX_BATCH_EXECUTE_TOOL_NAME: &str = "ctx_batch_execute";
/// Index inline content or a file/directory into the FTS5 knowledge base.
pub const CTX_INDEX_TOOL_NAME: &str = "ctx_index";
/// BM25 search over the knowledge base (multiple queries per call).
pub const CTX_SEARCH_TOOL_NAME: &str = "ctx_search";
/// Fetch a URL, convert it to text, and index it (raw page never echoed).
pub const CTX_FETCH_AND_INDEX_TOOL_NAME: &str = "ctx_fetch_and_index";
/// Process counters + knowledge-base statistics.
pub const CTX_STATS_TOOL_NAME: &str = "ctx_stats";
/// Delete indexed chunks (requires `confirm: true`).
pub const CTX_PURGE_TOOL_NAME: &str = "ctx_purge";
/// Local, read-only diagnostics (the port of upstream `ctx_doctor`; see the
/// module docs for the absent `ctx_upgrade`/`ctx_insight`).
pub const CTX_DOCTOR_TOOL_NAME: &str = "ctx_doctor";

// ---------------------------------------------------------------------------
// Hook thresholds and switches
// ---------------------------------------------------------------------------

/// `tool_call` routing blocks a `cat`/`head`/`tail`/`less` or `read` of a
/// file larger than this (upstream PreToolUse size gate).
pub const ROUTING_FILE_THRESHOLD_BYTES: u64 = 64 * 1024;

/// A finalized `tool_result` whose text exceeds this is auto-indexed
/// (upstream PostToolUse gate).
pub const TOOL_RESULT_INDEX_THRESHOLD_BYTES: usize = 20 * 1024;

/// Auto-index upper bound, matching the sandbox stdout cap: a huge result is
/// truncated on a UTF-8 boundary before chunking, so one tool call cannot
/// insert unbounded text into the FTS database.
pub const TOOL_RESULT_INDEX_MAX_BYTES: usize = MAX_OUTPUT_CHARS;

/// `0`/`false`/`off`/`no` disables `tool_call` routing (default on).
pub const ROUTING_ENV: &str = "PIR_CONTEXT_MODE_ROUTING";
/// `0`/`false`/`off`/`no` disables `tool_result` auto-indexing (default on).
pub const INDEX_RESULTS_ENV: &str = "PIR_CONTEXT_MODE_INDEX_RESULTS";

/// Whether the `tool_call` routing hook is enabled (upstream default: on).
pub(crate) fn routing_enabled() -> bool {
    env_flag(ROUTING_ENV)
}

/// Whether the `tool_result` auto-index hook is enabled (upstream default:
/// on).
pub(crate) fn result_indexing_enabled() -> bool {
    env_flag(INDEX_RESULTS_ENV)
}

fn env_flag(name: &str) -> bool {
    // `var_os` + a borrowed `&str` keeps the per-event switch read
    // allocation-free; a non-Unicode value cannot spell a disable switch, so
    // it keeps the default-on behavior `var` (which would error) produced.
    let Some(value) = std::env::var_os(name) else {
        return true;
    };
    let value = value.to_str().map(str::trim).unwrap_or_default();
    !["0", "false", "off", "no"]
        .iter()
        .any(|disabled| value.eq_ignore_ascii_case(disabled))
}

/// Default `ctx_execute` / per-script `ctx_batch_execute` deadline (seconds).
pub(crate) const DEFAULT_TIMEOUT_SECS: f64 = 30.0;
/// Hard ceiling for `ctx_execute`, `ctx_execute_file` and per-script batch
/// execution (seconds).
pub(crate) const MAX_TIMEOUT_SECS: f64 = 120.0;
/// Stdout above this size is indexed when `intent` is set (5 KiB).
pub(crate) const AUTO_INDEX_THRESHOLD_BYTES: usize = 5 * 1024;
/// Number of sections returned by the intent-driven auto-index path.
pub(crate) const AUTO_INDEX_RESULTS: usize = 5;

// ---------------------------------------------------------------------------
// Shared state
// ---------------------------------------------------------------------------

/// This process's execution counters (the knowledge-base counters live in
/// SQLite and are read by `ctx_stats`).
#[derive(Default)]
pub(crate) struct ExecStats {
    executions: AtomicU64,
    file_executions: AtomicU64,
    bytes_in: AtomicU64,
    bytes_out: AtomicU64,
    auto_indexes: AtomicU64,
}

impl ExecStats {
    fn record_execution(&self, bytes_in: usize, bytes_out: usize) {
        self.executions.fetch_add(1, Ordering::Relaxed);
        self.bytes_in.fetch_add(bytes_in as u64, Ordering::Relaxed);
        self.bytes_out
            .fetch_add(bytes_out as u64, Ordering::Relaxed);
    }

    fn record_file_execution(&self, bytes_in: usize, bytes_out: usize) {
        self.file_executions.fetch_add(1, Ordering::Relaxed);
        self.record_execution(bytes_in, bytes_out);
    }

    fn record_auto_index(&self) {
        self.auto_indexes.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn snapshot(&self) -> ExecStatsSnapshot {
        ExecStatsSnapshot {
            executions: self.executions.load(Ordering::Relaxed),
            file_executions: self.file_executions.load(Ordering::Relaxed),
            bytes_in: self.bytes_in.load(Ordering::Relaxed),
            bytes_out: self.bytes_out.load(Ordering::Relaxed),
            auto_indexes: self.auto_indexes.load(Ordering::Relaxed),
        }
    }
}

pub(crate) struct ExecStatsSnapshot {
    pub executions: u64,
    pub file_executions: u64,
    pub bytes_in: u64,
    pub bytes_out: u64,
    pub auto_indexes: u64,
}

/// Session identity and hook counters. The session id/cwd feed the
/// `tool_result` source label (`session:<id>`); the host pushes them through
/// `on_session_context` and a `session_start` payload can provide them too.
#[derive(Default)]
pub(crate) struct HookState {
    session_id: Mutex<Option<String>>,
    cwd: Mutex<Option<PathBuf>>,
    routing_blocks: AtomicU64,
    indexed_results: AtomicU64,
    hook_errors: AtomicU64,
    last_error: Mutex<Option<String>>,
}

impl HookState {
    fn set_session_id(&self, session_id: Option<String>) {
        let session_id = session_id
            .map(|id| id.trim().to_owned())
            .filter(|id| !id.is_empty());
        if session_id.is_some() {
            *self.lock_session_id() = session_id;
        }
    }

    fn set_cwd(&self, cwd: Option<PathBuf>) {
        if let Some(cwd) = cwd {
            *self
                .cwd
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(cwd);
        }
    }

    fn lock_session_id(&self) -> std::sync::MutexGuard<'_, Option<String>> {
        self.session_id
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    pub(crate) fn session_id(&self) -> Option<String> {
        self.lock_session_id().clone()
    }

    pub(crate) fn cwd(&self) -> Option<PathBuf> {
        self.cwd
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Record the identity fields a `session_start` payload may carry. The
    /// event fires before any DB use; this must stay allocation-only so the
    /// laziness contract holds.
    fn capture_payload(&self, payload: &JsonValue) {
        let string = |key: &str| {
            payload
                .get(key)
                .and_then(JsonValue::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_owned)
        };
        self.set_session_id(string("sessionId").or_else(|| string("session_id")));
        self.set_cwd(string("cwd").map(PathBuf::from));
    }

    fn record_routing_block(&self) {
        self.routing_blocks.fetch_add(1, Ordering::Relaxed);
    }

    fn record_indexed_result(&self) {
        self.indexed_results.fetch_add(1, Ordering::Relaxed);
    }

    fn record_error(&self, error: String) {
        self.hook_errors.fetch_add(1, Ordering::Relaxed);
        *self
            .last_error
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(error);
    }

    fn snapshot(&self) -> HookSnapshot {
        HookSnapshot {
            routing_blocks: self.routing_blocks.load(Ordering::Relaxed),
            indexed_results: self.indexed_results.load(Ordering::Relaxed),
            hook_errors: self.hook_errors.load(Ordering::Relaxed),
            last_error: self
                .last_error
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone(),
            session_id: self.session_id(),
        }
    }
}

pub(crate) struct HookSnapshot {
    pub routing_blocks: u64,
    pub indexed_results: u64,
    pub hook_errors: u64,
    pub last_error: Option<String>,
    pub session_id: Option<String>,
}

/// State shared by all nine tools: the lazy Python session, the lazy index,
/// the process counters and the hook state.
pub(crate) struct ContextModeState {
    pub python: PythonSession,
    pub index: Arc<IndexStore>,
    pub stats: ExecStats,
    pub hooks: HookState,
}

impl ContextModeState {
    fn new(db_path: Option<PathBuf>, db_base_dir: Option<PathBuf>) -> Self {
        Self {
            // Fresh namespace per job; the worker/VM is shared across calls.
            python: PythonSession::fresh(),
            index: Arc::new(match (db_path, db_base_dir) {
                (Some(path), _) => IndexStore::with_path(path),
                (None, Some(base)) => IndexStore::with_base_dir(base),
                (None, None) => IndexStore::project_default(),
            }),
            stats: ExecStats::default(),
            hooks: HookState::default(),
        }
    }
}

// ---------------------------------------------------------------------------
// `tool_call` routing (upstream PreToolUse)
// ---------------------------------------------------------------------------

impl ContextModePlugin {
    /// Enforce the context-mode routing policy on one `tool_call` payload.
    /// Returns the standard native block outcome only for an unambiguous
    /// violation; every other call is untouched (`None`).
    fn route_tool_call(&self, payload: &JsonValue) -> PluginEventOutcome {
        let Some(tool) = payload.get("toolName").and_then(JsonValue::as_str) else {
            return PluginEventOutcome::None;
        };
        // Never police the context-mode tools themselves.
        if tool.starts_with("ctx_") {
            return PluginEventOutcome::None;
        }
        // Only bash-like and read-like calls can be routed. Checking this
        // before the environment switch keeps every other tool call (edit,
        // grep, write, ...) free of an environment lookup.
        let routable = matches!(
            tool,
            "bash" | "shell_command" | "powershell" | "read" | "read_file"
        );
        if !routable || !routing_enabled() {
            return PluginEventOutcome::None;
        }
        let input = payload.get("input").unwrap_or(&JsonValue::Null);
        let reason = match tool {
            "bash" | "shell_command" | "powershell" => input
                .get("command")
                .and_then(JsonValue::as_str)
                .and_then(|command| {
                    let cwd = self.state.hooks.cwd();
                    shell_block_reason(tool, command, cwd.as_deref())
                }),
            "read" | "read_file" => read_tool_path(input).and_then(|path| {
                let cwd = self.state.hooks.cwd();
                read_block_reason(tool, path, cwd.as_deref())
            }),
            _ => None,
        };
        match reason {
            Some(reason) => {
                self.state.hooks.record_routing_block();
                PluginEventOutcome::ToolCall {
                    block: true,
                    reason: Some(reason),
                    terminate: false,
                }
            }
            None => PluginEventOutcome::None,
        }
    }

    /// Auto-index one finalized `tool_result` (upstream PostToolUse). The
    /// payload is only read; the result the model sees is never modified.
    async fn index_tool_result(&self, payload: &JsonValue) {
        let Some(tool) = payload.get("toolName").and_then(JsonValue::as_str) else {
            return;
        };
        // The ctx_* tools already index what they are asked to index; folding
        // their output back in would duplicate every ctx_execute/ctx_index
        // result and let a search result index itself.
        if tool.starts_with("ctx_") {
            return;
        }
        // Apply the size gate BEFORE the environment switch: the common case
        // is a small result, and that path must not pay for an environment
        // lookup or a text-block copy.
        let Some(text_len) = tool_result_text_len(payload) else {
            return;
        };
        if text_len <= TOOL_RESULT_INDEX_THRESHOLD_BYTES {
            return;
        }
        if !result_indexing_enabled() {
            return;
        }
        // Only a result that will actually be indexed pays for joining its
        // text blocks; a single text block stays borrowed from the payload.
        let Some(text) = tool_result_text(payload) else {
            return;
        };
        let bounded = truncate_utf8(text.as_ref(), TOOL_RESULT_INDEX_MAX_BYTES);
        let source = self
            .state
            .hooks
            .session_id()
            .map(|id| format!("session:{id}"))
            .unwrap_or_else(|| format!("tool:{tool}"));
        let title = tool_result_title(tool, payload.get("input"));
        let mut doc = IndexDoc::from_text(source, "text".to_owned(), bounded);
        for (index, chunk) in doc.chunks.iter_mut().enumerate() {
            chunk.title = if index == 0 {
                title.clone()
            } else {
                format!("{title} (part {})", index + 1)
            };
        }
        match self.state.index.index_documents(vec![doc]).await {
            Ok(report) if report.chunks > 0 => self.state.hooks.record_indexed_result(),
            Ok(_) => {}
            Err(error) => self
                .state
                .hooks
                .record_error(format!("tool_result auto-index failed: {error}")),
        }
    }
}

/// `(reader, path)` when `command` is a simple single-file `cat`/`head`/
/// `tail`/`less` invocation. Pipelines, redirections, globs and multiple
/// operands deliberately return `None`: the routing hook only acts on
/// unambiguous cases.
///
/// Takes the already-tokenized command so one `tool_call` never pays for
/// tokenizing the same shell line twice (downloader probe + file-read probe).
fn simple_single_file_read(command: &str, segments: Vec<Vec<String>>) -> Option<(String, String)> {
    if command.chars().any(|ch| {
        matches!(
            ch,
            '|' | ';' | '&' | '<' | '>' | '\n' | '\r' | '`' | '(' | ')'
        )
    }) || command.contains("$(")
    {
        return None;
    }
    let mut segments = segments;
    segments.retain(|segment| !segment.is_empty());
    if segments.len() != 1 {
        return None;
    }
    let words = segments.pop()?;
    let executable_index = executable_index(&words)?;
    let reader = executable_basename(&words[executable_index]);
    if !matches!(reader.as_str(), "cat" | "head" | "tail" | "less") {
        return None;
    }
    let mut operands = Vec::new();
    let mut index = executable_index + 1;
    // `head`/`tail` take a value for -n/-c; `cat -n` and `less -N` are plain
    // flags, so only those two commands get the option/value skip.
    let value_options = matches!(reader.as_str(), "head" | "tail");
    while index < words.len() {
        let token = words[index].as_str();
        if value_options && matches!(token, "-n" | "-c" | "--lines" | "--bytes") {
            index += 2;
            continue;
        }
        if token.starts_with('-') || token.starts_with('+') {
            index += 1;
            continue;
        }
        operands.push(token.to_owned());
        index += 1;
    }
    if operands.len() != 1 {
        return None;
    }
    Some((reader, operands.pop()?))
}

/// Cheap pre-filter over the first word only: the executable actually matched
/// by [`blocked_downloader`]/[`simple_single_file_read`] sits at the start of
/// a segment (after optional wrappers/env assignments). Scanning the whole
/// command here would cost more than one tokenization on long command lines.
fn may_be_routing_target(command: &str, powershell: bool) -> bool {
    let mut words = command.split_whitespace();
    let Some(first) = words.next() else {
        return false;
    };
    // Skip leading `VAR=value` assignments and wrappers with a bounded scan;
    // the full tokenizer still makes the final decision.
    let mut candidate = first;
    for _ in 0..4 {
        if !(candidate.contains('=') || is_wrapper(candidate)) {
            break;
        }
        let Some(next) = words.next() else {
            return false;
        };
        candidate = next;
    }
    // More than four leading assignments/wrappers: fall back to the full
    // tokenizer rather than risk missing the executable (conservative).
    if candidate.contains('=') || is_wrapper(candidate) {
        return true;
    }
    const ALWAYS: [&str; 6] = ["curl", "wget", "cat", "head", "tail", "less"];
    if ALWAYS
        .iter()
        .any(|needle| contains_ignore_ascii_case(candidate, needle))
    {
        return true;
    }
    powershell
        && ["iwr", "invoke-webrequest", "invoke-restmethod"]
            .iter()
            .any(|needle| contains_ignore_ascii_case(candidate, needle))
}

fn contains_ignore_ascii_case(haystack: &str, needle: &str) -> bool {
    haystack.len() >= needle.len()
        && haystack
            .as_bytes()
            .windows(needle.len())
            .any(|window| window.eq_ignore_ascii_case(needle.as_bytes()))
}

fn shell_block_reason(tool: &str, command: &str, cwd: Option<&Path>) -> Option<String> {
    let powershell = tool == "powershell";
    // A command with no separator can only have its first word as the
    // executable, so a cheap first-word check can skip tokenization for
    // `git status`, `cargo test`, etc. Commands containing separators must
    // still be tokenized: `cd x && curl ...` has a routing target later on.
    let single_segment = !command.chars().any(|ch| {
        matches!(
            ch,
            '|' | ';' | '&' | '\n' | '\r' | '`' | '(' | ')' | '{' | '}'
        )
    });
    if single_segment && !may_be_routing_target(command, powershell) {
        return None;
    }
    let segments = shell_segments(command);
    if let Some(executable) = blocked_downloader(&segments, powershell) {
        return Some(format!(
            "context-mode routing: `{executable}` would put the raw downloaded content into the \
             transcript. Use `ctx_fetch_and_index` (fetch + index; the raw page never enters \
             context) or `ctx_execute` with Python (e.g. `urllib.request`) so only the analysis \
             output reaches context."
        ));
    }
    let (reader, path) = simple_single_file_read(command, segments)?;
    let size = file_size_if_over(&path, cwd, ROUTING_FILE_THRESHOLD_BYTES)?;
    Some(format!(
        "context-mode routing: `{reader}` would read {size} bytes from `{path}` into context \
         (limit {limit} bytes). Use `ctx_execute_file` to analyze the file in the sandbox: only \
         the script's stdout enters context.",
        limit = ROUTING_FILE_THRESHOLD_BYTES
    ))
}

/// The first `curl`/`wget` (or PowerShell `iwr`/`Invoke-WebRequest`) command
/// in a tokenized command line, as the canonical name to report.
fn blocked_downloader(segments: &[Vec<String>], powershell: bool) -> Option<String> {
    for segment in segments {
        let Some(index) = executable_index(segment) else {
            continue;
        };
        let name = executable_basename(&segment[index]);
        match name.as_str() {
            "curl" => return Some("curl".to_owned()),
            "wget" => return Some("wget".to_owned()),
            "iwr" | "invoke-webrequest" if powershell => {
                return Some(if name == "iwr" {
                    "iwr".to_owned()
                } else {
                    "Invoke-WebRequest".to_owned()
                })
            }
            "invoke-restmethod" if powershell => return Some("Invoke-RestMethod".to_owned()),
            _ => {}
        }
    }
    None
}

fn read_tool_path(input: &JsonValue) -> Option<&str> {
    let mut found: Option<&str> = None;
    for key in ["path", "file_path", "filePath"] {
        let Some(value) = input
            .get(key)
            .and_then(JsonValue::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
        else {
            continue;
        };
        match found {
            None => found = Some(value),
            // Two disagreeing path spellings are not unambiguous.
            Some(existing) if existing == value => {}
            Some(_) => return None,
        }
    }
    found
}

fn read_block_reason(tool: &str, path: &str, cwd: Option<&Path>) -> Option<String> {
    let size = file_size_if_over(path, cwd, ROUTING_FILE_THRESHOLD_BYTES)?;
    Some(format!(
        "context-mode routing: `{tool}` would load {size} bytes from `{path}` into context \
         (limit {limit} bytes). Use `ctx_execute_file` to analyze the file in the sandbox: only \
         the script's stdout enters context.",
        limit = ROUTING_FILE_THRESHOLD_BYTES
    ))
}

/// Metadata-only size probe (nothing is read). `None` when the target does
/// not exist, is not a regular file, or is at/below `limit`.
fn file_size_if_over(path: &str, cwd: Option<&Path>, limit: u64) -> Option<u64> {
    let target = Path::new(path);
    let resolved = if target.is_absolute() {
        target.to_path_buf()
    } else {
        let base = cwd
            .map(Path::to_path_buf)
            .or_else(|| std::env::current_dir().ok())?;
        base.join(target)
    };
    let metadata = std::fs::metadata(&resolved).ok()?;
    if metadata.is_file() && metadata.len() > limit {
        Some(metadata.len())
    } else {
        None
    }
}

// ---------------------------------------------------------------------------
// Shell tokenization (detection only; no evaluation)
// ---------------------------------------------------------------------------

fn is_shell_separator(ch: char) -> bool {
    matches!(
        ch,
        ';' | '|' | '&' | '\n' | '\r' | '(' | ')' | '{' | '}' | '`'
    )
}

/// Split a command line into command segments at unquoted separators,
/// dropping quotes and backslash escapes. Only used to find executable words.
fn shell_segments(command: &str) -> Vec<Vec<String>> {
    let mut segments = vec![Vec::new()];
    let mut word = String::new();
    let mut quote: Option<char> = None;
    let mut escaped = false;
    for ch in command.chars() {
        if escaped {
            word.push(ch);
            escaped = false;
            continue;
        }
        if ch == '\\' && quote != Some('\'') {
            escaped = true;
            continue;
        }
        match quote {
            Some(open) => {
                if ch == open {
                    quote = None;
                } else {
                    word.push(ch);
                }
            }
            None => {
                if ch == '\'' || ch == '"' {
                    quote = Some(ch);
                } else if is_shell_separator(ch) {
                    push_shell_word(&mut segments, &mut word);
                    segments.push(Vec::new());
                } else if ch.is_whitespace() {
                    push_shell_word(&mut segments, &mut word);
                } else {
                    word.push(ch);
                }
            }
        }
    }
    if escaped {
        word.push('\\');
    }
    push_shell_word(&mut segments, &mut word);
    segments
}

fn push_shell_word(segments: &mut [Vec<String>], word: &mut String) {
    if word.is_empty() {
        return;
    }
    if let Some(segment) = segments.last_mut() {
        segment.push(std::mem::take(word));
    }
}

/// Index of the executable word in one command segment (leading
/// `VAR=value` assignments and command wrappers skipped).
fn executable_index(segment: &[String]) -> Option<usize> {
    segment
        .iter()
        .position(|token| !is_env_assignment(token) && !is_wrapper(token))
}

fn is_env_assignment(token: &str) -> bool {
    if let Some(rest) = token.strip_prefix("$env:") {
        return rest.contains('=');
    }
    let Some((name, _)) = token.split_once('=') else {
        return false;
    };
    !name.is_empty()
        && name
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
}

fn is_wrapper(token: &str) -> bool {
    const WRAPPERS: [&str; 10] = [
        "sudo", "env", "command", "time", "nohup", "nice", "exec", "builtin", "call", "start",
    ];
    WRAPPERS
        .iter()
        .any(|wrapper| token.eq_ignore_ascii_case(wrapper))
}

fn executable_basename(token: &str) -> String {
    let name = token
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(token)
        .to_ascii_lowercase();
    match name.strip_suffix(".exe") {
        Some(stem) => stem.to_owned(),
        None => name,
    }
}

// ---------------------------------------------------------------------------
// `tool_result` payload helpers
// ---------------------------------------------------------------------------

/// Byte length of the joined text [`tool_result_text`] would produce, without
/// building it. Used to apply the indexing threshold before any copy/allocation
/// (most tool results are small and must stay free of payload-sized work).
fn tool_result_text_len(payload: &JsonValue) -> Option<usize> {
    match payload.get("content")? {
        JsonValue::String(text) => Some(text.len()),
        JsonValue::Array(blocks) => {
            let mut total = 0usize;
            let mut has_content = false;
            for block in blocks {
                if block.get("type").and_then(JsonValue::as_str) != Some("text") {
                    continue;
                }
                let Some(piece) = block.get("text").and_then(JsonValue::as_str) else {
                    continue;
                };
                if has_content {
                    total += 1; // the '\n' separator
                }
                total += piece.len();
                if !piece.is_empty() {
                    has_content = true;
                }
            }
            has_content.then_some(total)
        }
        _ => None,
    }
}

/// Join the text blocks of a `tool_result` payload (the host's wire shape is
/// `content: [{type: "text", text}]`; a bare string is accepted too).
///
/// A single text block is returned borrowed: the auto-index threshold check
/// and the chunker can read it directly from the event payload. Only the
/// (rare) multi-block case above the threshold pays for the joined string.
fn tool_result_text(payload: &JsonValue) -> Option<Cow<'_, str>> {
    match payload.get("content")? {
        JsonValue::String(text) => Some(Cow::Borrowed(text)),
        JsonValue::Array(blocks) => {
            let mut text_blocks = blocks.iter().filter_map(|block| {
                if block.get("type").and_then(JsonValue::as_str) != Some("text") {
                    return None;
                }
                block.get("text").and_then(JsonValue::as_str)
            });
            let first = text_blocks.next()?;
            let Some(second) = text_blocks.next() else {
                return (!first.is_empty()).then_some(Cow::Borrowed(first));
            };
            let mut text = String::new();
            for piece in std::iter::once(first)
                .chain(std::iter::once(second))
                .chain(text_blocks)
            {
                if !text.is_empty() {
                    text.push('\n');
                }
                text.push_str(piece);
            }
            (!text.is_empty()).then_some(Cow::Owned(text))
        }
        _ => None,
    }
}

/// Compact title: tool name plus the first path/command token of the call.
fn tool_result_title(tool: &str, input: Option<&JsonValue>) -> String {
    let detail = input.and_then(|input| {
        if let Some(path) = input
            .get("path")
            .or_else(|| input.get("file_path"))
            .and_then(JsonValue::as_str)
        {
            return Some(truncate_chars(path.trim(), 80));
        }
        input
            .get("command")
            .and_then(JsonValue::as_str)
            .map(|command| truncate_chars(command.split_whitespace().next().unwrap_or(""), 80))
    });
    match detail.filter(|detail| !detail.is_empty()) {
        Some(detail) => format!("{tool}: {detail}"),
        None => tool.to_owned(),
    }
}

fn truncate_chars(text: &str, max: usize) -> String {
    match text.char_indices().nth(max) {
        Some((byte_index, _)) => text[..byte_index].trim_end().to_owned(),
        None => text.to_owned(),
    }
}

/// Truncate on a UTF-8 boundary (the index stores valid text).
fn truncate_utf8(text: &str, max: usize) -> &str {
    if text.len() <= max {
        return text;
    }
    let mut end = max;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

// ---------------------------------------------------------------------------
// The builtin plugin
// ---------------------------------------------------------------------------

/// The `builtin:context-mode` plugin. Replaceable like the other builtins: a
/// user extension registering any of the same tool names omits it.
pub struct ContextModePlugin {
    info: PluginInfo,
    state: Arc<ContextModeState>,
}

impl ContextModePlugin {
    /// The default plugin (project DB under the agent dir, opened lazily).
    pub fn new() -> Self {
        Self::build(None, None)
    }

    /// A plugin whose knowledge base lives at `path` — hermetic tests and
    /// embedders that do not want to touch `~/.pi`.
    pub fn with_db_path(path: impl Into<PathBuf>) -> Self {
        Self::build(Some(path.into()), None)
    }

    /// A plugin whose *default* database path is rooted at `base` instead of
    /// the shared agent dir. The per-project file name still derives from the
    /// session cwd, so callers can test cwd isolation without writing
    /// `~/.pi` (hermetic tests).
    #[doc(hidden)]
    pub fn with_db_base_dir(base: impl Into<PathBuf>) -> Self {
        Self::build(None, Some(base.into()))
    }

    fn build(db_path: Option<PathBuf>, db_base_dir: Option<PathBuf>) -> Self {
        let mut info = PluginInfo::builtin("context-mode");
        info.replaceable = crate::plugins::builtin::builtin_extension_def("context-mode")
            .is_some_and(|def| def.replaceable);
        Self {
            info,
            state: Arc::new(ContextModeState::new(db_path, db_base_dir)),
        }
    }

    /// The resolved knowledge-base path (without opening the database).
    /// Hidden test seam for the per-session-cwd path choice.
    #[doc(hidden)]
    pub fn resolved_db_path(&self) -> PathBuf {
        self.state.index.path()
    }

    /// Run the directory walk and report `(files, directories listed, entries
    /// read)`. Hidden test seam: proves the walk stops at the file cap instead
    /// of traversing/sorting a whole tree.
    #[doc(hidden)]
    pub fn walk_directory_probe(
        root: &std::path::Path,
        max_files: usize,
    ) -> Result<(usize, usize, usize), String> {
        index::walk_directory_probe(root, max_files)
    }

    /// How long the process-wide Python execution lock may be held before
    /// waiting jobs fail fast (the starvation safety valve documented in
    /// `python_vm`). Hidden test seam for that policy.
    #[doc(hidden)]
    pub fn python_exec_lock_wedged_after() -> std::time::Duration {
        crate::plugins::builtin::python_vm::exec_lock_wedged_after()
    }

    /// Whether the Python worker thread/VM currently exists. Hidden test
    /// seam for the laziness contract: `false` after construction and after
    /// pure index/search calls; `true` only after an execute call.
    pub fn has_python_worker(&self) -> bool {
        self.state.python.has_worker()
    }

    /// How many interpreter workers this instance has spawned.
    pub fn python_spawn_count(&self) -> u64 {
        self.state.python.spawn_count()
    }

    /// Whether the SQLite index connection is currently open (hidden test
    /// seam for the shutdown contract: `false` before first use and after
    /// [`AgentPlugin::on_shutdown`]).
    pub fn index_is_open(&self) -> bool {
        self.state.index.is_open()
    }

    /// Factory for the builtin registration list.
    pub fn new_plugin() -> Arc<dyn AgentPlugin> {
        Arc::new(Self::new())
    }
}

impl Default for ContextModePlugin {
    fn default() -> Self {
        Self::new()
    }
}

impl AgentPlugin for ContextModePlugin {
    fn info(&self) -> &PluginInfo {
        &self.info
    }

    fn register(&self, ctx: &mut RegistrationContext<'_>) -> anyhow::Result<()> {
        ctx.register_tool(Arc::new(exec::CtxExecuteTool::new(Arc::clone(&self.state))));
        ctx.register_tool(Arc::new(exec::CtxExecuteFileTool::new(Arc::clone(
            &self.state,
        ))));
        ctx.register_tool(Arc::new(exec::CtxBatchExecuteTool::new(Arc::clone(
            &self.state,
        ))));
        ctx.register_tool(Arc::new(index::CtxIndexTool::new(Arc::clone(&self.state))));
        ctx.register_tool(Arc::new(index::CtxSearchTool::new(Arc::clone(&self.state))));
        ctx.register_tool(Arc::new(index::CtxStatsTool::new(Arc::clone(&self.state))));
        ctx.register_tool(Arc::new(index::CtxPurgeTool::new(Arc::clone(&self.state))));
        ctx.register_tool(Arc::new(index::CtxDoctorTool::new(Arc::clone(&self.state))));
        ctx.register_tool(Arc::new(fetch::CtxFetchAndIndexTool::new(Arc::clone(
            &self.state,
        ))));
        Ok(())
    }

    /// The hook surface: `tool_call` (routing enforcement), `tool_result`
    /// (auto-index) and `session_start` (identity capture). Declaring them
    /// here is what turns the agent loop's hosted fan-out on
    /// (`PluginHost::has_native_handlers`).
    fn events(&self) -> EventInterest {
        EventInterest::List(vec![
            "tool_call".to_owned(),
            "tool_result".to_owned(),
            "session_start".to_owned(),
        ])
    }

    fn on_event<'a>(&'a self, event: PluginEvent<'a>) -> PluginFuture<'a, PluginEventOutcome> {
        Box::pin(async move {
            match event.name.as_ref() {
                "session_start" => {
                    // Identity only: never open the DB before a hook needs it.
                    self.state.hooks.capture_payload(event.payload);
                    PluginEventOutcome::None
                }
                "tool_call" => self.route_tool_call(event.payload),
                "tool_result" => {
                    self.index_tool_result(event.payload).await;
                    PluginEventOutcome::None
                }
                _ => PluginEventOutcome::None,
            }
        })
    }

    fn on_session_context(&self, snapshot: &SessionContextSnapshot) {
        // Prefer the live session cwd over the process cwd when resolving the
        // default database path, so two projects in one process do not share
        // (and purge) each other's index. The store resolves once, on first
        // DB use; a host that never pushes a snapshot keeps the process-cwd
        // fallback (documented on `IndexStore`).
        if let Some(cwd) = snapshot
            .cwd
            .as_deref()
            .map(str::trim)
            .filter(|cwd| !cwd.is_empty())
        {
            let cwd = PathBuf::from(cwd);
            self.state.index.set_session_cwd(cwd.clone());
            self.state.hooks.set_cwd(Some(cwd));
        }
        self.state.hooks.set_session_id(snapshot.session_id.clone());
    }

    fn on_shutdown(&self) -> PluginFuture<'_, anyhow::Result<()>> {
        Box::pin(async {
            // Interrupt a running ctx_execute job and stop the worker instead
            // of leaving it to the 120s ceiling; close the index connection
            // so the database is released cleanly. Both are idempotent and
            // no-ops when the worker/DB was never started.
            self.state.python.shutdown().await;
            self.state.index.close();
            Ok(())
        })
    }
}
