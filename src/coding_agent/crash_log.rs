//! Crash log: best-effort persistence of fatal panics to
//! `<agentDir>/crashes.json` (upstream `core/crash-log.ts`).
//!
//! Timestamped records, bounded to [`MAX_CRASH_RECORDS`] entries. Every
//! operation is infallible by design: callers are already crashing, so any
//! I/O or parse error is swallowed and the crash is simply not recorded.

use std::cell::Cell;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::session_manager::types::iso_from_ms;
/// Newest-first retention bound (upstream `MAX_CRASH_RECORDS`).
pub const MAX_CRASH_RECORDS: usize = 5;

/// How long an unnotified crash stays reportable: 7 days (upstream `MAX_AGE`).
pub const MAX_AGE_MS: u64 = 7 * 24 * 60 * 60 * 1000;

/// Why the process died (upstream `CrashRecord.kind`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum CrashKind {
    /// An uncaught panic / unhandled error (upstream `uncaught_exception`).
    #[default]
    #[serde(rename = "uncaught_exception")]
    UncaughtException,
    /// A fatal error caught by a host before the process gave up (upstream
    /// `fatal_error`).
    #[serde(rename = "fatal_error")]
    FatalError,
}

/// One persisted crash record (upstream `CrashRecord`). Deserialization is
/// lenient beyond the two fields upstream validates on read (`timestamp` and
/// `message`): missing/unknown values fall back instead of dropping the
/// record.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CrashRecord {
    pub timestamp: String,
    #[serde(default)]
    pub version: String,
    #[serde(default, deserialize_with = "deserialize_kind")]
    pub kind: CrashKind,
    pub message: String,
    #[serde(default)]
    pub stack: Option<String>,
    #[serde(default)]
    pub session_file: Option<String>,
    #[serde(default)]
    pub cwd: String,
    /// Whether the crash notice was already shown to the user (upstream
    /// `notified`); absent on fresh records.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notified: Option<bool>,
}

/// Upstream keeps any record whose `kind` is not one of the two literals (the
/// value is only ever written by `recordCrash`), so unknown kinds read back as
/// [`CrashKind::UncaughtException`] rather than dropping the record.
fn deserialize_kind<'de, D>(deserializer: D) -> Result<CrashKind, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = Option::<String>::deserialize(deserializer)?;
    Ok(match value.as_deref() {
        Some("fatal_error") => CrashKind::FatalError,
        _ => CrashKind::UncaughtException,
    })
}

/// The crash log path under the agent dir (upstream `crashLogPath`).
pub fn crash_log_path(agent_dir: &Path) -> PathBuf {
    agent_dir.join("crashes.json")
}

/// The crash log a panic records into **now**: the path under the LIVE agent
/// dir ([`crate::coding_agent::context::agent_dir`] — the same single source
/// the crash notice reads with, `tui::terminal::global_agent_dir`).
///
/// The panic hook must not pin the directory it was installed with: the
/// startup/exit notice re-resolves the agent dir (`take_unnotified_crash` on
/// `crash_log_path(&global_agent_dir())`), so a hook that captured the startup
/// dir would record into A while the notice looks in B after an agent-dir
/// change. Upstream resolves both sides at call time (`crash-log.ts:21`'s
/// `crashLogPath(agentDir = getAgentDir())`, used by `recordCrash` and
/// `takeUnnotifiedCrash`), and this is the same resolution.
pub fn live_crash_log_path() -> PathBuf {
    crash_log_path(&crate::coding_agent::context::agent_dir())
}

// ---------------------------------------------------------------------------
// Extension attribution (upstream `core/crash-log.ts`: ExtensionStackMetadata
// + findExtensionStackMatches)
// ---------------------------------------------------------------------------

/// Metadata for one loaded extension, enough to recognize its source files in
/// a panic backtrace (upstream `ExtensionStackMetadata`: `path`,
/// `resolvedPath`, `sourceInfo`).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ExtensionStackMetadata {
    /// Entry file path (the path the loader loaded).
    pub path: String,
    /// Label reported when this extension matches (upstream: the package
    /// source for a package extension, else the extension path).
    pub label: String,
    /// Package root: any frame below it matches (upstream
    /// `sourceInfo.baseDir` for a non-single-file package).
    pub base_dir: Option<String>,
    /// The entry is a directory `index.ts`/`index.js`: frames below its
    /// directory match too (upstream `directoryEntry`).
    pub directory_entry: bool,
}

impl ExtensionStackMetadata {
    pub fn new(path: impl Into<String>, label: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            label: label.into(),
            base_dir: None,
            directory_entry: false,
        }
    }

    /// Entry-file metadata: label defaults to the path, and an `index.*` entry
    /// is marked as a directory entry.
    pub fn from_path(path: impl AsRef<Path>) -> Self {
        let path = path.as_ref();
        let display = path.to_string_lossy().into_owned();
        let directory_entry = path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .is_some_and(|stem| stem.eq_ignore_ascii_case("index"));
        Self {
            path: display.clone(),
            label: display,
            base_dir: None,
            directory_entry,
        }
    }

    #[must_use]
    pub fn with_label(mut self, label: impl Into<String>) -> Self {
        self.label = label.into();
        self
    }

    #[must_use]
    pub fn with_base_dir(mut self, base_dir: impl Into<String>) -> Self {
        self.base_dir = Some(base_dir.into());
        self
    }
}

/// Process-wide set of extensions the host loaded (empty until a host
/// registers them). The TUI registers its discovered entry points at startup;
/// a panic then names the extension whose source appears in the backtrace.
static LOADED_EXTENSIONS: std::sync::Mutex<Vec<ExtensionStackMetadata>> =
    std::sync::Mutex::new(Vec::new());

/// Replace the loaded-extension registry (a reload passes its new set; an
/// empty list clears it).
pub fn register_loaded_extensions(extensions: Vec<ExtensionStackMetadata>) {
    *LOADED_EXTENSIONS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = extensions;
}

/// The registered extensions (detached copy).
pub fn loaded_extensions() -> Vec<ExtensionStackMetadata> {
    LOADED_EXTENSIONS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
}

/// Drop the registry (tests / a host with no extension runtime).
pub fn clear_loaded_extensions() {
    register_loaded_extensions(Vec::new());
}

/// Register the extensions discovered under `paths` (the TUI startup seam).
/// Discovery is the same [`crate::extensions::loader::discover_extensions`]
/// pass the runtime performs, so the registered entry points are exactly the
/// ones that were about to load. Returns the registered metadata.
#[cfg(feature = "extensions")]
pub fn register_extensions_from_paths(paths: &[PathBuf]) -> Vec<ExtensionStackMetadata> {
    let discovery = crate::extensions::loader::discover_extensions(paths);
    let metadata = metadata_from_loaded(&discovery.extensions);
    register_loaded_extensions(metadata.clone());
    metadata
}

/// Metadata for already-loaded extensions
/// ([`crate::extensions::types::LoadedExtension`] carries the entry path).
#[cfg(feature = "extensions")]
pub fn metadata_from_loaded(
    extensions: &[crate::extensions::types::LoadedExtension],
) -> Vec<ExtensionStackMetadata> {
    extensions
        .iter()
        .map(|extension| ExtensionStackMetadata::from_path(&extension.path))
        .collect()
}

/// Normalize a path for stack comparison: forward slashes, no trailing
/// slashes (upstream `normalizeStackPath`).
fn normalize_stack_path(value: &str) -> String {
    value.replace('\\', "/").trim_end_matches('/').to_owned()
}

/// Synthetic module ids (`pir-ext:`, `builtin:`, `node:`) never name an
/// on-disk source file.
fn is_synthetic_path(path: &str) -> bool {
    path.starts_with("pir-ext:")
        || path.starts_with("builtin:")
        || path.starts_with("node:")
        || path.starts_with("virtual:")
}

/// Does `stack` contain `target_path`? `include_descendants` matches frames
/// under the target directory (upstream `stackContainsPath`): Windows drive
/// paths compare case-insensitively, and an exact match must end at a path
/// boundary (`:`, `)`, whitespace, or end of line).
fn stack_contains_path(stack: &str, target_path: &str, include_descendants: bool) -> bool {
    let target = normalize_stack_path(target_path);
    if target.is_empty() || is_synthetic_path(&target) {
        return false;
    }
    let windows_drive = target.len() >= 2
        && target.as_bytes()[0].is_ascii_alphabetic()
        && target.as_bytes()[1] == b':';
    let (haystack, needle) = if windows_drive {
        (stack.to_lowercase(), target.to_lowercase())
    } else {
        (stack.to_owned(), target)
    };
    if include_descendants {
        return haystack.contains(&format!("{needle}/"));
    }
    haystack.match_indices(&needle).any(|(index, _)| {
        haystack
            .as_bytes()
            .get(index + needle.len())
            .is_none_or(|next| matches!(next, b':' | b')') || next.is_ascii_whitespace())
    })
}

/// Best-effort `decodeURI`: `%XX` byte sequences become bytes again, invalid
/// escapes stay literal.
fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut decoded: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            let high = (bytes[index + 1] as char).to_digit(16);
            let low = (bytes[index + 2] as char).to_digit(16);
            if let (Some(high), Some(low)) = (high, low) {
                decoded.push((high * 16 + low) as u8);
                index += 3;
                continue;
            }
        }
        decoded.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&decoded).into_owned()
}

/// Find loaded extensions whose source files appear in a stack trace
/// (upstream `findExtensionStackMatches`). Returns labels in registration
/// order, deduplicated.
///
/// Adaptation: upstream keeps only `    at …` V8 frames; Rust backtraces use
/// `  12: module::fn` / `at path:line` shapes, so every stack line is searched
/// for the path.
pub fn find_extension_stack_matches(
    stack: &str,
    extensions: &[ExtensionStackMetadata],
) -> Vec<String> {
    if stack.is_empty() {
        return Vec::new();
    }
    let normalized_stack = stack
        .lines()
        .map(percent_decode)
        .collect::<Vec<_>>()
        .join("\n")
        .replace('\\', "/");
    let mut matches: Vec<String> = Vec::new();
    let mut seen = std::collections::BTreeSet::new();
    for extension in extensions {
        let resolved = normalize_stack_path(&extension.path);
        let matched = if let Some(base_dir) = &extension.base_dir {
            stack_contains_path(&normalized_stack, base_dir, true)
        } else if extension.directory_entry {
            match resolved.rsplit_once('/') {
                Some((directory, _)) if !directory.is_empty() => {
                    stack_contains_path(&normalized_stack, directory, true)
                }
                _ => false,
            }
        } else {
            stack_contains_path(&normalized_stack, &resolved, false)
        };
        if matched && seen.insert(extension.label.clone()) {
            matches.push(extension.label.clone());
        }
    }
    matches
}

/// Append the matched extensions to a crash stack as a trailing section, so a
/// later crash-notice/bug-report read names the extension that was on the
/// stack. An empty match list returns the stack unchanged.
pub fn attach_extension_attribution(stack: &str, matches: &[String]) -> String {
    if matches.is_empty() {
        return stack.to_owned();
    }
    let mut out = stack.to_owned();
    if !out.ends_with('\n') {
        out.push('\n');
    }
    out.push_str("\nloaded extensions in this stack:\n");
    for label in matches {
        out.push_str("  - ");
        out.push_str(label);
        out.push('\n');
    }
    out
}

/// Current process cwd, empty when the cwd cannot be read (best-effort).
fn current_cwd() -> String {
    std::env::current_dir()
        .map(|cwd| cwd.to_string_lossy().into_owned())
        .unwrap_or_default()
}
pub fn read_crash_log(path: &Path) -> Vec<CrashRecord> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let Ok(values) = serde_json::from_str::<Vec<serde_json::Value>>(&text) else {
        return Vec::new();
    };
    values
        .into_iter()
        .filter(|value| {
            value
                .get("timestamp")
                .is_some_and(serde_json::Value::is_string)
                && value
                    .get("message")
                    .is_some_and(serde_json::Value::is_string)
        })
        .filter_map(|value| serde_json::from_value::<CrashRecord>(value).ok())
        .collect()
}

/// Append one record, keeping at most [`MAX_CRASH_RECORDS`] newest entries
/// (upstream `recordCrash`). Returns the written record; `None` when the
/// write failed (best-effort persistence).
pub fn record_crash(
    kind: CrashKind,
    message: &str,
    stack: Option<&str>,
    session_file: Option<&str>,
    cwd: &str,
    path: &Path,
) -> Option<CrashRecord> {
    let record = CrashRecord {
        timestamp: iso_from_ms(crate::ai::util::now_ms()),
        version: env!("CARGO_PKG_VERSION").to_owned(),
        kind,
        message: message.to_owned(),
        stack: stack.map(str::to_owned),
        session_file: session_file.map(str::to_owned),
        cwd: cwd.to_owned(),
        notified: None,
    };
    let mut records = read_crash_log(path);
    records.push(record.clone());
    if records.len() > MAX_CRASH_RECORDS {
        let drop = records.len() - MAX_CRASH_RECORDS;
        records.drain(..drop);
    }
    let json = serde_json::to_string_pretty(&records).ok()?;
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    std::fs::write(path, format!("{json}\n")).ok()?;
    Some(record)
}

/// Record a panic payload (panic-hook convenience): extracts the message from
/// the payload and captures the backtrace. Deduplicated per thread so a
/// panic-hook chain (TUI cleanup hook + process hook) records one entry.
pub fn record_panic(agent_dir: &Path, info: &std::panic::PanicHookInfo<'_>) {
    thread_local! {
        static PANIC_RECORDED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    }
    let already = PANIC_RECORDED.with(Cell::get);
    if already {
        return;
    }
    PANIC_RECORDED.with(|flag| flag.set(true));

    let message = payload_message(info.payload());
    let location = info
        .location()
        .map(|location| {
            format!(
                "{}:{}:{}",
                location.file(),
                location.line(),
                location.column()
            )
        })
        .unwrap_or_default();
    let backtrace = std::backtrace::Backtrace::force_capture().to_string();
    let stack = if location.is_empty() {
        backtrace
    } else {
        format!("at {location}\n{backtrace}")
    };
    // Extension attribution (§7 #20, upstream `findExtensionStackMatches`):
    // name the loaded extension(s) whose source files are on this stack, so
    // the persisted crash record already points at the culprit. Empty registry
    // or no match leaves the stack byte-identical.
    let matches = find_extension_stack_matches(&stack, &loaded_extensions());
    let stack = attach_extension_attribution(&stack, &matches);
    let session_file = current_session_file();
    let path = crash_log_path(agent_dir);
    let cwd = current_cwd();
    let _ = record_crash(
        CrashKind::UncaughtException,
        &message,
        Some(&stack),
        session_file.as_deref(),
        &cwd,
        &path,
    );
}

/// Extract a human-readable message from a panic payload (upstream
/// `error.message || error.name`; non-string payloads stringify).
fn payload_message(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(text) = payload.downcast_ref::<&'static str>() {
        let message = (*text).to_owned();
        if !message.is_empty() {
            return message;
        }
        return "panic".to_owned();
    }
    if let Some(text) = payload.downcast_ref::<String>() {
        if !text.is_empty() {
            return text.clone();
        }
        return "panic".to_owned();
    }
    "panic".to_owned()
}

/// Best-effort session-file hint from `PIR_SESSION_FILE` (hosts set it when a
/// session is open; upstream reads the live session manager instead).
fn current_session_file() -> Option<String> {
    std::env::var("PIR_SESSION_FILE")
        .ok()
        .filter(|value| !value.is_empty())
}

/// Return the newest unnotified recent crash, marking it notified (upstream
/// `takeUnnotifiedCrash`). `now_ms` is injected for testability.
pub fn take_unnotified_crash(path: &Path, now_ms: u64) -> Option<CrashRecord> {
    let records = read_crash_log(path);
    let mut crash: Option<CrashRecord> = None;
    for record in records.iter().rev() {
        let fresh = crate::session_manager::types::ms_from_iso(&record.timestamp)
            .is_some_and(|recorded| now_ms.saturating_sub(recorded) <= MAX_AGE_MS);
        if record.notified != Some(true) && fresh {
            crash = Some(record.clone());
            break;
        }
    }
    let crash = crash?;
    let updated = records
        .into_iter()
        .map(|mut record| {
            if record.notified != Some(true) {
                record.notified = Some(true);
            }
            record
        })
        .collect::<Vec<_>>();
    if let Ok(json) = serde_json::to_string_pretty(&updated) {
        let _ = std::fs::write(path, format!("{json}\n"));
    }
    Some(crash)
}

/// Delete the crash log (upstream `clearCrashLog`); best-effort.
pub fn clear_crash_log(path: &Path) {
    let _ = std::fs::remove_file(path);
}

/// Record a panic into the **live** agent dir's crash log: the shared entry
/// point for every panic hook pir installs — the process hook from `main`
/// ([`install_hook`]) and the TUI's terminal-restoring hook
/// (`tui::terminal::install_panic_hook`) — each of which must resolve the
/// directory at panic time, never at installation time.
///
/// Whichever hook runs first writes the record (`record_panic` deduplicates
/// per thread), so a hook that pinned its installation-time directory would
/// decide the record's location by itself: after an agent-dir change a panic
/// would be written under A while the startup/exit notice reads B. Resolving
/// here is what keeps every hook and the notice on one file; [`crash_log_path`]
/// of the dir this resolves is [`live_crash_log_path`].
pub fn record_panic_live(info: &std::panic::PanicHookInfo<'_>) {
    record_panic(&crate::coding_agent::context::agent_dir(), info);
}

/// Install the process-level crash panic hook (call once from `main`): every
/// panic appends a bounded record to `<agentDir>/crashes.json` before the
/// previous hook (or the default printer) runs.
///
/// The recorded path is resolved **per panic** through
/// [`live_crash_log_path`], i.e. from the same live agent dir the crash notice
/// reads (`take_unnotified_crash` at startup and exit). An agent-dir change
/// therefore cannot split the two: the record and the notice always name the
/// same file. Upstream behaves identically — `recordCrash` defaults its path
/// to `crashLogPath()` = `join(getAgentDir(), "crashes.json")` at call time
/// (`packages/coding-agent/src/core/crash-log.ts:21,126`), as does
/// `takeUnnotifiedCrash` (`:147`).
pub fn install_hook() {
    let previous_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        record_panic_live(info);
        previous_hook(info);
    }));
}
