//! Session management, byte-interoperable with upstream pi
//! (`packages/coding-agent/src/core/session-manager.ts`).
//!
//! A session is one append-only JSONL file stored under
//! `<agent-dir>/sessions/<encoded-cwd>/<ISO-timestamp>_<session-id>.jsonl`
//! (the agent dir defaults to `~/.pi/agent`, overridable via
//! `PIR_CODING_AGENT_DIR`; the whole directory is overridable via
//! `PIR_SESSION_DIR`). Line 1 is the session header
//! (`{"type":"session","version":3,...}`); every following line is one entry
//! tagged by `type`, forming a tree through `id`/`parentId` with a moving
//! `leaf`. The file is created lazily on the first user or assistant message
//! so a prompt survives a failed first turn.
//!
//! This module replaces pir's former JSONL v4 transactional format (ported
//! from upstream's deleted `packages/agent` harness): sessions are no longer
//! byte-compatible with that lineage, only with upstream coding-agent.
//!
//! # Feature gate (`session`)
//!
//! The session *data plane* — [`types`], [`projection`], [`json_depth`], the
//! in-memory entry tree, and [`SessionManager`]'s append/branch/projection
//! API — is core: `AgentSession`, `compact` and the `pi_format` interop are
//! built on it and none of it needs a file.
//!
//! The *persistence plane* rides the `session` feature: the JSONL reader/
//! writer, session discovery, and [`tail_page`] (the `memmap2` reader). With
//! the feature off every file-backed constructor degrades cleanly:
//!
//! - [`SessionManager::create`] / [`SessionManager::create_with_id`] /
//!   [`SessionManager::in_memory`] still return a working manager, but it is
//!   **in-memory only** (`persist == false`, no session file) — the run and its
//!   transcript are fully functional and simply not written to disk;
//! - [`SessionManager::open`], [`open_with`](SessionManager::open_with),
//!   [`open_paged`](SessionManager::open_paged) and
//!   [`fork_from_source`](SessionManager::fork_from_source) return the typed
//!   [`SESSION_PERSISTENCE_DISABLED`] error (there is no file to read);
//! - [`SessionManager::continue_recent`] starts a fresh in-memory session;
//! - [`SessionManager::list_sessions`] / [`find_session_by_id`](SessionManager::find_session_by_id)
//!   report "no saved sessions" (and log the disabled notice).
//!
//! Nothing silently claims success: a caller that asked for a file gets either
//! a fake-free in-memory manager with `session_file() == None` or the typed
//! error, never a path that is never written.
//!
//! The residual JSONL parser helpers stay compiled when the feature is off so
//! this file keeps its single-source layout; they are unreachable then, hence
//! the conditional `dead_code` allowance below.

#![cfg_attr(not(feature = "session"), allow(dead_code))]

pub mod json_depth;
pub mod projection;
// Session-selector search (upstream
// `modes/interactive/components/session-selector-search.ts`): token/phrase/
// regex parsing plus tiered exact/fuzzy scoring over id/name/cwd and every
// user/assistant message.
pub mod search;
// The tail-first memory-mapped reader (`memmap2`) is the only persistence
// piece that needs an external crate; it rides the `session` feature.
#[cfg(feature = "session")]
pub mod tail_page;
pub mod types;

pub use projection::{
    build_context_entries, build_projection_entry_meta, build_session_projection,
    projected_message_count, session_entry_to_context_messages, ProjectedSessionEntry,
    SessionProjection,
};
#[cfg(feature = "session")]
pub use tail_page::{
    SessionTailPage, TailPageStats, DEFAULT_TAIL_PAGE_BYTES, DEFAULT_TAIL_PAGE_ENTRIES,
};

// `session`-off twins for the two page caps. `SessionManager::open_paged` keeps
// its signature in the no-feature build (it reports the typed
// [`SESSION_PERSISTENCE_DISABLED`] error instead of returning a page), and its
// callers still pass these caps as arguments: `open_session_tail_first` in
// `coding_agent::session_resolve` and the SDK's `next_older_page`. A
// "disabled" value here would be a behaviour lie — the constants are call
// arguments, not behaviour — so the stub keeps the real defaults, identical to
// `tail_page.rs` (1 MiB / 1024 entries).
#[cfg(not(feature = "session"))]
pub const DEFAULT_TAIL_PAGE_BYTES: usize = 1024 * 1024;

#[cfg(not(feature = "session"))]
pub const DEFAULT_TAIL_PAGE_ENTRIES: usize = 1024;

/// `session`-feature-off stand-in for [`tail_page::SessionTailPage`]: the type
/// the static signatures (`read_older_page`, the TUI's load-more fallback) name,
/// with the same shape but no mapping and no parsing. Every constructor returns
/// `None`, so a caller's `let Some(page) = ... else { return }` takes its
/// existing "no more pages" branch.
#[cfg(not(feature = "session"))]
#[derive(Debug)]
pub struct SessionTailPage;

#[cfg(not(feature = "session"))]
impl SessionTailPage {
    /// Always `None`: tail pages need the `memmap2` reader this build omits.
    pub fn open(
        _path: &Path,
        _max_bytes: usize,
        _max_entries: usize,
    ) -> crate::Result<Option<Self>> {
        Ok(None)
    }

    /// Always `None` (see [`Self::open`]).
    pub fn open_window(
        _path: &Path,
        _end: Option<u64>,
        _max_bytes: usize,
        _max_entries: usize,
    ) -> crate::Result<Option<Self>> {
        Ok(None)
    }

    /// Always `None`: a stub page always started at the file start.
    pub fn extend(&self, _max_bytes: usize, _max_entries: usize) -> crate::Result<Option<Self>> {
        Ok(None)
    }

    /// No entries: nothing was parsed.
    pub fn entries(&self) -> &[Arc<FileEntry>] {
        &[]
    }

    /// No unparsed lines: nothing was parsed.
    pub fn unparsed_lines(&self) -> &[String] {
        &[]
    }

    /// No header.
    pub fn header(&self) -> Option<&SessionHeader> {
        None
    }

    /// No header.
    pub fn file_header(&self) -> Option<SessionHeader> {
        None
    }

    /// No session id.
    pub fn session_id(&self) -> Option<&str> {
        None
    }

    /// No name.
    pub fn session_name(&self) -> Option<String> {
        None
    }

    /// Zero-length page.
    pub fn page_start(&self) -> u64 {
        0
    }

    /// Zero-length file.
    pub fn file_len(&self) -> u64 {
        0
    }

    /// Zero-length window.
    pub fn window_end(&self) -> u64 {
        0
    }

    /// No entries.
    pub fn oldest_entry(&self) -> Option<&Arc<FileEntry>> {
        None
    }

    /// No leaf.
    pub fn leaf_id(&self) -> Option<&str> {
        None
    }

    /// No compaction entry.
    pub fn contains_compaction_entry(&self) -> bool {
        false
    }
}
pub use types::{
    assert_valid_session_id, iso_from_ms, ms_from_iso, new_entry_id, new_session_id,
    BranchSummaryEntry, CompactionEntry, ContextEditEntry, ContextEditReplacement, CustomEntry,
    CustomMessageEntry, FileEntry, LabelEntry, MessageEntry, ModelChangeEntry, SessionHeader,
    SessionInfoEntry, StoredBashExecutionMessage, StoredMessage, StoredSystemMessage,
    ThinkingLevelChangeEntry, UsageEntry, CURRENT_SESSION_VERSION,
};

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::SystemTime;

use crate::ai::types::{Message, Usage};
use crate::ai::util::now_ms;
use serde_json::Value as JsonValue;

/// Trait import for line writes.
use std::io::Write as _;

/// The error every file-backed constructor returns when the `session` feature
/// is off: the in-memory data plane still works, but there is no JSONL reader/
/// writer in this build, so asking to open, resume or fork a session FILE can
/// only be answered honestly.
pub const SESSION_PERSISTENCE_DISABLED: &str =
    "session persistence is disabled in this build (the `session` cargo feature is off); \
     new sessions run in memory only";

/// Upper bound for the header line read during discovery (mirrors upstream's
/// `MAX_SESSION_HEADER_SCAN_BYTES`).
const SESSION_HEADER_SCAN_BYTES: u64 = 1024 * 1024;

/// Session files opened by [`SessionManager::list_sessions`] since the last
/// reset, a `#[doc(hidden)]` test seam pinning "one read per file": the
/// pre-fix listing opened every file twice (header scan + peek scan).
static SESSION_LIST_FILE_READS: AtomicU64 = AtomicU64::new(0);

/// Number of session files [`SessionManager::list_sessions`] has opened
/// (test/profiling probe).
#[doc(hidden)]
pub fn session_list_file_reads() -> u64 {
    SESSION_LIST_FILE_READS.load(Ordering::Relaxed)
}

/// Reset [`session_list_file_reads`] (test probe).
#[doc(hidden)]
pub fn reset_session_list_file_reads() {
    SESSION_LIST_FILE_READS.store(0, Ordering::Relaxed);
}

/// Session files opened by [`SessionManager::list_session_headers`] since the
/// last reset. Kept SEPARATE from [`session_list_file_reads`] so a consumer
/// (the server registry) can prove it took the header-only path instead of
/// the full message-peek listing.
static SESSION_HEADER_LIST_FILE_READS: AtomicU64 = AtomicU64::new(0);

/// Number of session files [`SessionManager::list_session_headers`] has
/// opened (test/profiling probe).
#[doc(hidden)]
pub fn session_header_list_file_reads() -> u64 {
    SESSION_HEADER_LIST_FILE_READS.load(Ordering::Relaxed)
}

/// Reset [`session_header_list_file_reads`] (test probe).
#[doc(hidden)]
pub fn reset_session_header_list_file_reads() {
    SESSION_HEADER_LIST_FILE_READS.store(0, Ordering::Relaxed);
}

/// Injected stall for the NEXT header-only listing of one directory
/// (test/profiling seam; always `None` in production). Matching on the
/// directory keeps concurrently running tests from consuming each other's
/// stall.
static SESSION_HEADER_LIST_STALL: std::sync::Mutex<Option<(PathBuf, std::time::Duration)>> =
    std::sync::Mutex::new(None);

/// Stall the next header-only listing of `session_dir` (test seam; see
/// [`SessionManager::list_session_headers`]). The sleep runs on whatever
/// thread performs the scan, so callers can prove the scan does not hold an
/// async registry lock or a runtime worker.
#[doc(hidden)]
pub fn stall_next_session_header_listing_for_test(
    session_dir: impl AsRef<Path>,
    stall: std::time::Duration,
) {
    let mut slot = SESSION_HEADER_LIST_STALL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    *slot = Some((session_dir.as_ref().to_path_buf(), stall));
}

fn take_session_header_listing_stall(session_dir: &Path) -> Option<std::time::Duration> {
    let mut slot = SESSION_HEADER_LIST_STALL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    match slot.as_ref() {
        Some((path, _)) if path == session_dir => slot.take().map(|(_, stall)| stall),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Directory layout (upstream `config.ts` + `session-manager.ts`)
// ---------------------------------------------------------------------------

/// The agent config directory (upstream `getAgentDir`): delegates to the
/// single source of truth — `crate::ai::providers::agent_dir()`, honoring
/// `$PIR_AGENT_DIR` → `$PIR_CODING_AGENT_DIR` → `$PI_CODING_AGENT_DIR`, else
/// `~/.pi/agent`. Keeping this a thin alias preserves the module's public API
/// (discovery and the SDK resolve session dirs through it).
pub fn agent_dir() -> PathBuf {
    crate::ai::providers::agent_dir()
}

/// The home directory (`USERPROFILE` on Windows, `HOME` elsewhere).
pub fn home_dir() -> Option<PathBuf> {
    std::env::var_os("USERPROFILE")
        .filter(|value| !value.is_empty())
        .or_else(|| std::env::var_os("HOME").filter(|value| !value.is_empty()))
        .map(PathBuf::from)
}

/// `<agent-dir>/sessions` (upstream `getSessionsDir`).
pub fn sessions_dir() -> PathBuf {
    sessions_dir_in(&agent_dir())
}

/// `<agent-dir>/sessions` for an EXPLICIT agent directory (D1): a live session
/// scopes its session-root listings to the directory it was built with instead
/// of re-resolving the process environment on every read. `agent_dir` is the
/// caller's already-resolved value; this function never reads the environment.
pub fn sessions_dir_in(agent_dir: &Path) -> PathBuf {
    agent_dir.join("sessions")
}

/// Resolve a path to an absolute, lexically normalized string against an
/// EXPLICIT `home` and `cwd` — the pure core of [`resolve_path_str`]
/// (upstream `resolvePath`: tilde expansion + `path.resolve`; symlinks are
/// not followed so the encoding is stable without touching the filesystem).
/// `home = None` leaves a leading `~` verbatim instead of reading
/// `$HOME`/`$USERPROFILE` (the `expand_tilde_in_pattern` precedent), and
/// `cwd` is the base for relative paths instead of the process cwd. UNC paths
/// (`\\server\share\...`) keep their `\\server\share` prefix, mirroring
/// Node's win32 `path.resolve`.
pub fn resolve_path_str_with(path: &str, home: Option<&Path>, cwd: &Path) -> String {
    let expanded = {
        let trimmed = path.trim();
        if trimmed == "~" {
            home.map(|home| home.to_string_lossy().into_owned())
                .unwrap_or_else(|| trimmed.to_owned())
        } else if let Some(rest) = trimmed
            .strip_prefix("~/")
            .or_else(|| trimmed.strip_prefix("~\\"))
        {
            match home {
                Some(home) => home.join(rest).to_string_lossy().into_owned(),
                None => trimmed.to_owned(),
            }
        } else {
            trimmed.to_owned()
        }
    };
    let absolute = if Path::new(&expanded).is_absolute() {
        expanded
    } else {
        cwd.join(&expanded).to_string_lossy().into_owned()
    };
    // Lexically normalize separators and `.`/`..` components. UNC prefixes
    // (`\\server\share`) are preserved as the root, like Node's win32
    // `path.resolve`; drive and POSIX roots keep the pre-existing handling.
    let is_separator = |byte: u8| byte == b'/' || byte == b'\\';
    let bytes = absolute.as_bytes();
    if bytes.len() >= 2 && is_separator(bytes[0]) && is_separator(bytes[1]) {
        let mut parts = absolute[2..]
            .split(['/', '\\'])
            .filter(|part| !part.is_empty());
        let server = parts.next().unwrap_or_default();
        let share = parts.next().unwrap_or_default();
        let mut tail: Vec<String> = Vec::new();
        for part in parts {
            match part {
                "." => {}
                ".." => {
                    tail.pop();
                }
                other => tail.push(other.to_owned()),
            }
        }
        if share.is_empty() {
            return format!(r"\\{server}");
        }
        if tail.is_empty() {
            return format!(r"\\{server}\{share}");
        }
        return format!(r"\\{server}\{share}\{}", tail.join("\\"));
    }
    let mut components: Vec<String> = Vec::new();
    let mut prefix = String::new();
    for (index, part) in absolute.split(['/', '\\']).enumerate() {
        if index == 0 && part.len() == 2 && part.as_bytes()[1] == b':' {
            prefix = format!("{part}\\");
            continue;
        }
        if index == 0 && part.is_empty() {
            prefix = String::from("/");
            continue;
        }
        match part {
            "" | "." => {}
            ".." => {
                components.pop();
            }
            other => components.push(other.to_owned()),
        }
    }
    if prefix.is_empty() && cfg!(windows) && absolute.len() >= 2 && absolute.as_bytes()[1] == b':' {
        prefix = format!("{}\\", &absolute[..2]);
    }
    if prefix == "/" {
        format!("/{}", components.join("/"))
    } else {
        format!("{prefix}{}", components.join("\\"))
    }
}

/// [`resolve_path_str_with`] against the ambient process state: [`home_dir`]
/// and `std::env::current_dir()` (falling back to `.`). The ambient reads
/// live ONLY here (upstream `resolvePath` resolves against `process.cwd()`);
/// this is the documented boundary wrapper kept for external callers, while
/// [`resolve_path_str_with`] lets tests and callers with explicit values
/// resolve without touching the process environment.
pub fn resolve_path_str(path: &str) -> String {
    let home = home_dir();
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    resolve_path_str_with(path, home.as_deref(), &cwd)
}

/// Encode a resolved cwd into a safe directory name (upstream
/// `getDefaultSessionDirPath`): strip exactly ONE leading separator, map the
/// remaining `/`, `\`, `:` to `-`, and wrap in `--...--`.
pub fn encode_cwd_dir_name(resolved_cwd: &str) -> String {
    let stripped = resolved_cwd
        .strip_prefix(['/', '\\'])
        .unwrap_or(resolved_cwd);
    let safe: String = stripped
        .chars()
        .map(|c| match c {
            '/' | '\\' | ':' => '-',
            other => other,
        })
        .collect();
    format!("--{safe}--")
}

/// The default session directory for a cwd (upstream
/// `getDefaultSessionDirPath`): `<agent-dir>/sessions/<encoded-cwd>`.
pub fn default_session_dir_path(cwd: &str) -> PathBuf {
    sessions_dir().join(encode_cwd_dir_name(&resolve_path_str(cwd)))
}

/// Compute and create the default session directory for a cwd (upstream
/// `getDefaultSessionDir`).
pub fn default_session_dir(cwd: &str) -> PathBuf {
    let dir = default_session_dir_path(cwd);
    if !dir.exists() {
        let _ = std::fs::create_dir_all(&dir);
    }
    dir
}

// ---------------------------------------------------------------------------
// Session summary (listing)
// ---------------------------------------------------------------------------

/// A session discovered in a session directory (the pir counterpart of the
/// upstream `SessionInfo` discovery record, session-manager.ts:229-243).
/// Field population mirrors upstream `buildSessionInfo` (799-882): `name`
/// from the latest `session_info` entry, `first_message` the first user
/// message text, `message_count` every `message` entry,
/// `parent_session_path` the header's `parentSession`, and
/// `all_messages_text` every user/assistant message's text joined with a space
/// (the selector-search corpus, exactly like upstream `buildSessionInfo`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionSummary {
    pub path: PathBuf,
    pub id: String,
    pub cwd: String,
    /// User-defined display name from the latest `session_info` entry
    /// (`None` when the session carries none).
    pub modified: SystemTime,
    /// User-defined display name from the latest `session_info` entry
    /// (upstream `SessionInfo.name`).
    pub name: Option<String>,
    /// Text of the first user message in the file, or `"(no messages)"` like
    /// upstream when the session carries none.
    pub first_message: String,
    /// Every user/assistant message's text, in file order, joined with a
    /// single space (upstream `SessionInfo.allMessagesText`). This is the
    /// corpus the session selector searches, so a query can hit a message
    /// from any point in the conversation.
    pub all_messages_text: String,
    /// Number of `message` entries in the file (upstream `messageCount`).
    pub message_count: usize,
    /// Parent session path from the header (upstream `parentSessionPath`),
    /// for forked/branched sessions.
    pub parent_session_path: Option<String>,
}

/// A header-only listing row ([`SessionManager::list_session_headers`]): the
/// discovery data a session registry needs to merge on-disk sessions into its
/// metadata list. Unlike [`SessionSummary`] it carries NO first-message text,
/// message count, or search corpus — the scan never deserializes message
/// lines, so listing a directory of large sessions stays cheap.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionHeaderSummary {
    pub path: PathBuf,
    pub id: String,
    pub cwd: String,
    pub modified: SystemTime,
    /// Latest `session_info` display name (`None` when none was recorded).
    pub name: Option<String>,
}

/// Disk usage of one session directory (see
/// [`SessionManager::disk_usage`]): the number of `.jsonl` session files and
/// their combined size. Computed from directory metadata only — no file
/// content is read.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SessionDiskUsage {
    /// Number of `*.jsonl` regular files in the directory.
    pub sessions: usize,
    /// Combined length of those files in bytes.
    pub bytes: u64,
}

/// A progress callback for [`SessionManager::list_sessions_cancellable`]
/// (upstream `SessionListProgress`, session-manager.ts:884-890): number of
/// files processed, the candidate total, and — on the periodic publishes
/// (the first file, every [`SESSION_LIST_PUBLISH_INTERVAL`], and the last) —
/// the sessions loaded so far, sorted by activity.
pub type SessionListProgress<'a> = dyn FnMut(usize, usize, Option<&[SessionSummary]>) + 'a;

/// How many loaded files pass between periodic partial-listing publishes
/// (upstream `CURRENT_SESSION_LIST_PUBLISH_INTERVAL`).
pub const SESSION_LIST_PUBLISH_INTERVAL: usize = 10;

/// Result of a cancellable session listing
/// ([`SessionManager::list_sessions_cancellable`]).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SessionListOutcome {
    /// Sessions discovered so far, most recently modified first. A cancelled
    /// scan returns the partial list (never a misleading empty one).
    pub sessions: Vec<SessionSummary>,
    /// Whether the scan stopped early because the cancel signal asked for it
    /// (upstream `AbortSignal` `throwIfAborted`).
    pub cancelled: bool,
}

/// Parse one JSONL line into a file entry; malformed lines are skipped
/// (upstream `parseSessionEntryLine`, which repairs `"content": null` and a
/// string message content so the entry survives instead of being dropped from
/// the projection).
fn parse_entry_line(line: &str) -> Option<FileEntry> {
    if line.trim().is_empty() {
        return None;
    }
    // Fast path: a well-formed line deserializes STRAIGHT into the entry.
    // The former shape always materialized the whole line as a
    // `serde_json::Value` and then converted that tree into `FileEntry` — the
    // JSON tree (one `String` per key, one map per object) existed only to be
    // immediately walked again. Only lines the repairs actually rewrite (a
    // `null`/string `content`, which the typed shape rejects) take the slow
    // path below, so the parsed result is identical.
    if let Ok(entry) = serde_json::from_str::<FileEntry>(line) {
        return Some(entry);
    }
    let mut value: serde_json::Value = serde_json::from_str(line).ok()?;
    repair_null_message_content(&mut value);
    normalize_string_message_content(&mut value);
    serde_json::from_value(value).ok()
}

/// Wrap upstream's `content: string` message shape as one text block.
///
/// Upstream types a user/system message's `content` as
/// `string | (TextContent | ImageContent)[]`, so a real pi session line may
/// carry plain text as a JSON string where pir's `Vec<ContentBlock>` expects
/// a block array. Without this repair the WHOLE line fails
/// [`StoredMessage`] deserialization: it lands in `unparsed_raw_lines`, and
/// the migration then refuses its rewrite (the preserve-unknown-lines rule) —
/// a v2 session whose messages use the string form opens with the header
/// ONLY, reading as an empty transcript. The text becomes a single text
/// block, the same lossy reduction [`StoredSystemMessage`] already applies to
/// its string content; a later rewrite publishes the block form, which pi
/// reads back identically. Any other content shape is left untouched so it
/// still fails loudly into `unparsed_raw` instead of being coerced.
fn normalize_string_message_content(value: &mut serde_json::Value) {
    let Some(message) = value.get_mut("message") else {
        return;
    };
    if !matches!(
        message.get("role").and_then(JsonValue::as_str),
        Some("user" | "developer" | "assistant" | "toolResult")
    ) {
        return;
    }
    let Some(JsonValue::String(text)) = message.get("content") else {
        return;
    };
    let text = text.clone();
    message["content"] = JsonValue::Array(vec![serde_json::json!({
        "type": "text",
        "text": text,
    })]);
}

/// Repair a `"content": null` message payload (upstream
/// `parseSessionEntryLine`, session-manager.ts:439-451): a system message
/// with null content becomes `""`; a user/assistant/toolResult message with
/// null content becomes `[]`. The entry is repaired and kept rather than
/// failing [`StoredMessage`] deserialization and disappearing from the
/// projection.
fn repair_null_message_content(value: &mut serde_json::Value) {
    let Some(message) = value.get_mut("message") else {
        return;
    };
    if message
        .get("content")
        .is_none_or(|content| !content.is_null())
    {
        return;
    }
    match message.get("role").and_then(serde_json::Value::as_str) {
        Some("system") => {
            message["content"] = serde_json::Value::String(String::new());
        }
        Some("user" | "assistant" | "toolResult") => {
            message["content"] = serde_json::Value::Array(Vec::new());
        }
        _ => {}
    }
}

/// Parse one JSONL line into a file entry, applying the v2→v3 `hookMessage` →
/// `custom` role rename for files whose header version is below 3 (upstream
/// `migrateV2ToV3` moves the rename into the load path so migrated messages
/// can be deserialized at all).
pub(crate) fn parse_entry_line_with_version(
    line: &str,
    header_version: Option<u32>,
) -> Option<FileEntry> {
    if line.trim().is_empty() {
        return None;
    }
    // Fast path (see `parse_entry_line`): the line deserializes directly,
    // without the throwaway `serde_json::Value` tree. A v2 `hookMessage`
    // line can never succeed here — no stored-message variant accepts that
    // role — so it still takes the rename/repair path below unchanged.
    if let Ok(entry) = serde_json::from_str::<FileEntry>(line) {
        return Some(entry);
    }
    let mut value: serde_json::Value = serde_json::from_str(line).ok()?;
    repair_null_message_content(&mut value);
    normalize_string_message_content(&mut value);
    if header_version.is_some_and(|version| version < 3)
        && value.get("type").and_then(serde_json::Value::as_str) == Some("message")
        && value
            .get("message")
            .and_then(|message| message.get("role"))
            .and_then(serde_json::Value::as_str)
            == Some("hookMessage")
    {
        if let Some(message) = value.get_mut("message") {
            message["role"] = serde_json::Value::String("custom".to_owned());
        }
    }
    serde_json::from_value(value).ok()
}

/// One session file's streaming read result (upstream `loadEntriesFromFile`):
/// every raw line aligned with its parse outcome, so undeserializable lines
/// are carried verbatim instead of being silently erased.
struct LoadedSessionFile {
    /// Raw text of lines that FAILED to parse, aligned with [`Self::parsed`]
    /// (`None` where the line parsed or was blank). The loader no longer keeps
    /// every line: a 100 MB session used to hold a second full copy of the file
    /// in a `Vec<String>` while parsing.
    unparsed_raw: Vec<Option<String>>,
    /// Parse outcome per line (`None` for blank and unparseable lines).
    parsed: Vec<Option<FileEntry>>,
    /// Raw text of non-blank lines that failed to parse.
    unparsed_lines: Vec<String>,
    /// True when the final line lacked a `\n` (upstream's non-empty `pending`).
    torn_tail: bool,
    /// True when the first parsed entry is a session header (upstream's
    /// `loadEntriesFromFile` validation).
    header_valid: bool,
}

impl LoadedSessionFile {
    /// Parsed entries in file order, MOVED out of the parse result.
    ///
    /// INVARIANT (move, not clone): the loader is consumed here, so the parsed
    /// entries are handed over instead of being deep-cloned into a second
    /// buffer that the temporary `LoadedSessionFile` then dropped — the clone
    /// copied the whole session (every stored tool result) on every open. A
    /// caller that still needs `lines`/`unparsed_lines` takes those fields
    /// first (`std::mem::take`); this method consumes the rest.
    fn into_entries(self) -> Vec<FileEntry> {
        self.parsed.into_iter().flatten().collect()
    }
}

/// Upper bound for a single session entry line; a longer line fails the load
/// instead of growing memory without limit (the former whole-file 256 MiB cap
/// silently truncated mid-line and then corrupted the file with a newline
/// repair — the cap now errors, per line, with no truncation).
const MAX_SESSION_LINE_BYTES: usize = 256 * 1024 * 1024;

/// Streaming read buffer size (upstream `SESSION_READ_BUFFER_SIZE`).
pub(crate) const SESSION_READ_BUFFER_SIZE: usize = 1024 * 1024;

/// Decode one session line, borrowing from `raw` when it is valid UTF-8 and
/// falling back to the lossy decode (upstream `StringDecoder`) — counting the
/// lossy lines — when it is not. One validation pass per line: the former
/// shape probed `from_utf8(..).is_err()` and then decoded again.
fn decode_session_line<'a>(raw: &'a [u8], lossy_lines: &mut usize) -> std::borrow::Cow<'a, str> {
    match std::str::from_utf8(raw) {
        Ok(text) => std::borrow::Cow::Borrowed(text),
        Err(_) => {
            *lossy_lines += 1;
            String::from_utf8_lossy(raw)
        }
    }
}

/// Upper bound for ONE line while streaming the skipped prefix of a paged
/// session ([`SessionManager::for_each_prefix_entry`] and the paged
/// compaction streamer). Prefix scans tolerate unknown/unparseable lines by
/// design, so without a cap a single corrupt line would grow the pending
/// buffer without limit. Deliberately smaller than
/// [`MAX_SESSION_LINE_BYTES`] (the full loader's contract): this is bulk
/// streaming of an already-unknown file, and a 32 MiB single entry line is
/// far beyond any realistic message.
pub const MAX_PREFIX_ENTRY_LINE_BYTES: usize = 32 * 1024 * 1024;

/// Append the missing trailing newline to a session file that was left torn by
/// a crash between an entry's line and its closing newline.
///
/// This is the same repair [`load_entries_from_file`] performs on the full-open
/// path (upstream `appendFileSync "\n"`); a paged open skips that loader, so it
/// must repair the tail itself before the manager starts appending — appending
/// raw JSON onto an unterminated line joins the two records and makes the whole
/// line unparseable on every later load.
#[cfg(feature = "session")]
fn repair_missing_trailing_newline(path: &Path) -> crate::Result<()> {
    use std::io::{Read, Seek, SeekFrom, Write};
    let mut file = std::fs::File::open(path)?;
    let length = file.metadata()?.len();
    if length == 0 {
        return Ok(());
    }
    file.seek(SeekFrom::End(-1))?;
    let mut last = [0u8; 1];
    file.read_exact(&mut last)?;
    if last[0] == b'\n' {
        return Ok(());
    }
    let mut append = std::fs::OpenOptions::new().append(true).open(path)?;
    append.write_all(b"\n")?;
    append.sync_all()?;
    Ok(())
}

/// Read all entries from a session file (upstream `loadEntriesFromFile`):
/// streamed line-by-line with a bounded read buffer and no whole-file cap, so
/// a read never stops mid-line. Invalid UTF-8 is tolerated lossily (upstream
/// `StringDecoder`) with a warning note. The header is validated before any
/// repair, and a missing trailing newline is repaired only after a COMPLETE
/// read (upstream `appendFileSync "\n"` on a non-empty final line).
fn load_entries_from_file(path: &Path) -> crate::Result<LoadedSessionFile> {
    use std::io::Read;
    if !path.exists() {
        return Ok(LoadedSessionFile {
            parsed: Vec::new(),
            unparsed_raw: Vec::new(),
            unparsed_lines: Vec::new(),
            torn_tail: false,
            header_valid: false,
        });
    }
    let mut file = std::fs::File::open(path)
        .map_err(|error| anyhow::anyhow!("cannot read session {}: {error}", path.display()))?;
    // Reserve from the file size (an entry averages a few hundred bytes), so a
    // large session does not pay repeated Vec growth on top of its parse.
    let estimated_entries = std::fs::metadata(path)
        .map(|meta| ((meta.len() / 256) as usize).min(16_384))
        .unwrap_or(0);
    let mut buffer = vec![0u8; SESSION_READ_BUFFER_SIZE];
    // Each line is parsed AS IT COMPLETES and its raw text is dropped right
    // there. The previous shape first collected every line into a
    // `Vec<String>` and parsed afterwards, which materialized a second full
    // copy of the file during the load (a 100 MB session was measured peaking
    // at ~1.8 GB on load + one turn). Only lines that FAIL to parse stay
    // owned, in `unparsed_raw` (aligned with `parsed` so the fork can carry
    // them verbatim); `unparsed_lines` keeps the manager's append-only list.
    let mut parsed: Vec<Option<FileEntry>> = Vec::with_capacity(estimated_entries);
    let mut unparsed_raw: Vec<Option<String>> = Vec::with_capacity(estimated_entries);
    let mut unparsed_lines: Vec<String> = Vec::new();
    let mut pending: Vec<u8> = Vec::new();
    let mut lossy_lines = 0usize;
    let mut header_version: Option<u32> = None;
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|error| anyhow::anyhow!("cannot read session {}: {error}", path.display()))?;
        if read == 0 {
            break;
        }
        pending.extend_from_slice(&buffer[..read]);
        let mut line_start = 0usize;
        while let Some(relative) = pending[line_start..].iter().position(|byte| *byte == b'\n') {
            let end = line_start + relative;
            let raw = &pending[line_start..end];
            // ONE UTF-8 validation per line: decoding the line and observing
            // whether it was lossy is the same check, so the former separate
            // `from_utf8(raw).is_err()` probe (a second full validation of the
            // whole file) folds into the decode.
            let text = decode_session_line(raw, &mut lossy_lines);
            let entry = parse_entry_line_with_version(&text, header_version);
            if header_version.is_none() {
                if let Some(header) = entry.as_ref().and_then(FileEntry::as_header) {
                    header_version = Some(header.version.unwrap_or(1));
                }
            }
            if entry.is_none() && !text.trim().is_empty() {
                let owned = text.into_owned();
                unparsed_raw.push(Some(owned.clone()));
                unparsed_lines.push(owned);
            } else {
                unparsed_raw.push(None);
            }
            parsed.push(entry);
            line_start = end + 1;
        }
        pending.drain(..line_start);
        if pending.len() > MAX_SESSION_LINE_BYTES {
            anyhow::bail!(
                "Session entry line exceeds {MAX_SESSION_LINE_BYTES}-byte limit: {}",
                path.display()
            );
        }
    }
    let torn_tail = !pending.is_empty();
    if torn_tail {
        let text = decode_session_line(&pending, &mut lossy_lines);
        // The version a header on this last line carries is never consumed:
        // nothing is parsed after it (upstream reads the version off the
        // first entry, which the loop above already resolved).
        let entry = parse_entry_line_with_version(&text, header_version);
        if entry.is_none() && !text.trim().is_empty() {
            let owned = text.into_owned();
            unparsed_raw.push(Some(owned.clone()));
            unparsed_lines.push(owned);
        } else {
            unparsed_raw.push(None);
        }
        parsed.push(entry);
    }
    if lossy_lines > 0 {
        tracing::warn!(
            "session {} contains {lossy_lines} line(s) with invalid UTF-8; decoded lossily",
            path.display()
        );
    }

    let mut loaded = LoadedSessionFile {
        parsed,
        unparsed_raw,
        unparsed_lines,
        torn_tail,
        header_valid: false,
    };

    // Validate the session header before repairing the file (upstream
    // `loadEntriesFromFile`): the first parsed line must be a session header;
    // otherwise the file is not a session (no entries, no repair).
    let first_entry = loaded.parsed.iter().flatten().next();
    loaded.header_valid = first_entry.is_some_and(FileEntry::is_header);
    if first_entry.is_some() && !loaded.header_valid {
        loaded.parsed.clear();
        loaded.unparsed_raw.clear();
        loaded.unparsed_lines.clear();
        return Ok(loaded);
    }

    if loaded.header_valid && loaded.torn_tail {
        // Repair a missing trailing newline (upstream `appendFileSync "\n"`).
        let mut append = std::fs::OpenOptions::new().append(true).open(path)?;
        append.write_all(b"\n")?;
    }
    Ok(loaded)
}

/// Attempts, and the pause between them, for a session read that observed a
/// ZERO-BYTE file (see [`load_entries_retrying_transient_empties`]).
const EMPTY_SESSION_READ_ATTEMPTS: usize = 10;
const EMPTY_SESSION_READ_DELAY: std::time::Duration = std::time::Duration::from_millis(5);

/// [`load_entries_from_file`] with a bounded retry while the file is observed
/// as ZERO BYTES.
///
/// INVARIANT: an empty published session file is not a state pir's writer can
/// produce — a rewrite publishes by renaming a fully flushed temp file, and the
/// first-write path takes `create_new` and writes the header immediately. A
/// reader can still catch a zero-length name transiently: (a) a competing host
/// on the `create_new`-then-write path, and (b) a filesystem/antivirus that
/// exposes the rename target before its bytes. `set_session_file` treats "no
/// entries" as a NEW session and WRITES a header into the path, so believing
/// such a transient would replace the other writer's transcript. The retry is
/// bounded, so a genuinely empty file (the documented `open`/`--session`
/// empty-file case) simply pays the delay once and is initialized exactly as
/// before.
///
/// A NON-EMPTY file that yields no parseable entry is NOT retried: that is
/// invalid data, and the caller must report it without rewriting the file.
fn load_entries_retrying_transient_empties(path: &Path) -> crate::Result<LoadedSessionFile> {
    let mut loaded = load_entries_from_file(path)?;
    for attempt in 1..=EMPTY_SESSION_READ_ATTEMPTS {
        if loaded.parsed.iter().any(|entry| entry.is_some())
            || attempt == EMPTY_SESSION_READ_ATTEMPTS
        {
            return Ok(loaded);
        }
        // Bytes AFTER an empty read mean the read raced a writer: re-read
        // instead of judging the file from the stale empty parse (the caller's
        // "non-empty but unparseable is invalid data" branch must see a
        // CURRENT read).
        if std::fs::metadata(path).is_ok_and(|meta| meta.len() > 0) {
            return load_entries_from_file(path);
        }
        std::thread::sleep(EMPTY_SESSION_READ_DELAY);
        loaded = load_entries_from_file(path)?;
    }
    Ok(loaded)
}

/// Read and validate only the first parseable line of a session file
/// (bounded, upstream `readSessionHeader`). Returns `Ok(None)` when the file
/// has no session header; I/O errors and the scan limit surface as `Err`.
fn read_session_header_bounded(path: &Path) -> crate::Result<Option<SessionHeader>> {
    let mut reader = std::io::BufReader::new(std::fs::File::open(path)?);
    read_session_header_from_reader(&mut reader)
}

/// The bounded header scan shared by [`read_session_header_bounded`] and the
/// one-pass listing read: consume lines until the first *parseable* entry,
/// which must be the session header. The reader is left positioned right
/// after that line, so a caller can continue streaming the same file without
/// a second open.
fn read_session_header_from_reader(
    reader: &mut impl std::io::BufRead,
) -> crate::Result<Option<SessionHeader>> {
    use std::io::{BufRead as _, Read as _};

    let mut scanned = 0u64;
    let mut line = Vec::new();
    loop {
        line.clear();
        let remaining = SESSION_HEADER_SCAN_BYTES.saturating_sub(scanned);
        if remaining == 0 {
            // The scan budget is exhausted; a byte beyond it means the header
            // is not bounded by the limit.
            if !reader.fill_buf()?.is_empty() {
                anyhow::bail!("Session header exceeds {SESSION_HEADER_SCAN_BYTES}-byte scan limit");
            }
            return Ok(None);
        }
        let read = reader
            .by_ref()
            .take(remaining)
            .read_until(b'\n', &mut line)?;
        if read == 0 {
            return Ok(None);
        }
        scanned += read as u64;
        // The limit was reached in the middle of a line and the file goes on:
        // the header scan is unbounded.
        if line.last() != Some(&b'\n')
            && scanned == SESSION_HEADER_SCAN_BYTES
            && !reader.fill_buf()?.is_empty()
        {
            anyhow::bail!("Session header exceeds {SESSION_HEADER_SCAN_BYTES}-byte scan limit");
        }
        let Ok(text) = std::str::from_utf8(&line) else {
            continue;
        };
        let Some(entry) = parse_entry_line(text) else {
            continue;
        };
        return Ok(match entry {
            FileEntry::Session(header) => Some(header),
            _ => None,
        });
    }
}

/// Best-effort header read for discovery: unreadable or oversized files are
/// not sessions and must not prevent other sessions from being found
/// (upstream `readSessionHeaderForDiscovery`).
fn read_session_header_for_discovery(path: &Path) -> Option<SessionHeader> {
    read_session_header_bounded(path).ok().flatten()
}

/// The header+peek discovery payload of [`SessionManager::list_sessions`]
/// (upstream `buildSessionInfo`): the latest `session_info` name, the first
/// user message's text, the message count, and the joined text of every
/// user/assistant message (the selector-search corpus).
struct SessionInfoPeek {
    name: Option<String>,
    first_message: String,
    all_messages_text: String,
    message_count: usize,
}

/// Whether a serialized message line carries `"role":"<role>"` at the JSON
/// structure level. The listing peeks every user/assistant message to build
/// `allMessagesText`, but a tool-result line can be megabytes; a byte scan for
/// the role marker lets the peek skip deserializing those lines entirely.
/// Whitespace between the tokens is tolerated: hand-written fixtures and any
/// valid pretty-printed JSONL must list the same as pir's compact writer.
/// Text content containing the literal is JSON-escaped (`\"`), so it cannot
/// produce a false positive.
///
/// One scan reports both roles: the peek asks "assistant?" and "user?" for
/// every message line, and the old shape scanned (and allocated a quoted-role
/// `String`) once per question.
fn line_message_roles(line: &[u8]) -> (bool, bool) {
    let key = b"\"role\"";
    let mut is_assistant = false;
    let mut is_user = false;
    let mut offset = 0;
    while let Some(found) = line[offset..]
        .windows(key.len())
        .position(|window| window == key)
    {
        let mut rest = &line[offset + found + key.len()..];
        while rest.first().is_some_and(|byte| byte.is_ascii_whitespace()) {
            rest = &rest[1..];
        }
        if rest.first() == Some(&b':') {
            rest = &rest[1..];
            while rest.first().is_some_and(|byte| byte.is_ascii_whitespace()) {
                rest = &rest[1..];
            }
            if rest.starts_with(b"\"assistant\"") {
                is_assistant = true;
            } else if rest.starts_with(b"\"user\"") {
                is_user = true;
            }
        }
        offset += found + key.len();
    }
    (is_assistant, is_user)
}

/// The searchable text of one message's content blocks (upstream
/// `extractTextContent`): TEXT blocks joined with a single space, non-text
/// blocks skipped.
fn searchable_content_text(content: &[crate::ai::types::ContentBlock]) -> String {
    content
        .iter()
        .filter_map(|block| match block {
            crate::ai::types::ContentBlock::Text(text) => Some(text.text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Whether a serialized entry line carries `{"type":"<expected>"` at the top
/// level (whitespace between the tokens is tolerated). The listing's message
/// count uses this byte scan instead of deserializing every line; pir (and
/// upstream `JSON.stringify`) write the tag first, so the check is exact for
/// the format both sides produce.
fn line_has_entry_type(line: &[u8], expected: &str) -> bool {
    let line = line.trim_ascii_start();
    let Some(rest) = line.strip_prefix(b"{") else {
        return false;
    };
    let Some(rest) = rest.trim_ascii_start().strip_prefix(b"\"type\"") else {
        return false;
    };
    let Some(rest) = rest.trim_ascii_start().strip_prefix(b":") else {
        return false;
    };
    let Some(rest) = rest.trim_ascii_start().strip_prefix(b"\"") else {
        return false;
    };
    let Some(rest) = rest.strip_prefix(expected.as_bytes()) else {
        return false;
    };
    rest.first() == Some(&b'"')
}

/// Stream the remainder of a session file once and extract the
/// [`SessionInfoPeek`] fields. Every line that is a `message` entry is counted
/// with a cheap byte scan; user/assistant lines are deserialized to collect
/// their text, while tool-result lines (often the largest) are skipped by the
/// role scan. `session_info` lines are always deserialized.
fn peek_session_info(reader: &mut impl std::io::BufRead) -> SessionInfoPeek {
    let mut peek = SessionInfoPeek {
        name: None,
        first_message: String::new(),
        all_messages_text: String::new(),
        message_count: 0,
    };
    let mut line = Vec::new();
    loop {
        line.clear();
        let Ok(read) = reader.read_until(b'\n', &mut line) else {
            break;
        };
        if read == 0 {
            break;
        }
        if line_has_entry_type(&line, "message") {
            peek.message_count += 1;
            // Upstream keeps only user/assistant text in `allMessagesText`
            // (and only a user's text is the first message). One byte scan
            // answers both role questions.
            let (is_assistant, is_user) = line_message_roles(&line);
            if !is_assistant && !is_user {
                continue;
            }
            let Ok(text) = std::str::from_utf8(&line) else {
                continue;
            };
            let Some(FileEntry::Message(message_entry)) = parse_entry_line(text) else {
                continue;
            };
            let text = match &message_entry.message {
                StoredMessage::User(user) if user.role == crate::ai::types::UserRole::User => {
                    searchable_content_text(&user.content)
                }
                StoredMessage::Assistant(assistant) => searchable_content_text(&assistant.content),
                _ => continue,
            };
            if text.is_empty() {
                continue;
            }
            if peek.first_message.is_empty() && is_user && !is_assistant {
                peek.first_message = text.clone();
            }
            if !peek.all_messages_text.is_empty() {
                peek.all_messages_text.push(' ');
            }
            peek.all_messages_text.push_str(&text);
        } else if line_has_entry_type(&line, "session_info") {
            let Ok(text) = std::str::from_utf8(&line) else {
                continue;
            };
            let Some(FileEntry::SessionInfo(info)) = parse_entry_line(text) else {
                continue;
            };
            // Latest session_info wins; empty names clear (upstream).
            peek.name = info
                .name
                .as_ref()
                .map(|name| name.trim().to_owned())
                .filter(|name| !name.is_empty());
        }
    }
    if peek.first_message.is_empty() {
        peek.first_message = "(no messages)".to_owned();
    }
    peek
}

/// Open one session file ONCE: bounded header scan, `cwd` filter, then the
/// discovery peek over the same reader. `None` when the file is not a session
/// (no header) or its header cwd does not match `resolved_cwd` (the caller
/// then skips it without a peek pass).
fn read_session_listing(
    path: &Path,
    resolved_cwd: Option<&str>,
) -> Option<(SessionHeader, SessionInfoPeek)> {
    SESSION_LIST_FILE_READS.fetch_add(1, Ordering::Relaxed);
    let mut reader = std::io::BufReader::new(std::fs::File::open(path).ok()?);
    let header = read_session_header_from_reader(&mut reader)
        .ok()
        .flatten()?;
    if let Some(reference) = resolved_cwd {
        if !session_cwd_matches(Some(header.cwd.as_str()), reference) {
            return None;
        }
    }
    let peek = peek_session_info(&mut reader);
    Some((header, peek))
}

/// Scan a session file for the latest `session_info` name ONLY (the
/// header-only listing): message lines are skipped with the same byte-level
/// `type` scan the full peek uses and never deserialized, so no message text
/// (and no `all_messages_text` corpus) is materialized.
fn peek_session_name(reader: &mut impl std::io::BufRead) -> Option<String> {
    let mut name = None;
    let mut line = Vec::new();
    loop {
        line.clear();
        let Ok(read) = reader.read_until(b'\n', &mut line) else {
            break;
        };
        if read == 0 {
            break;
        }
        if !line_has_entry_type(&line, "session_info") {
            continue;
        }
        let Ok(text) = std::str::from_utf8(&line) else {
            continue;
        };
        let Some(FileEntry::SessionInfo(info)) = parse_entry_line(text) else {
            continue;
        };
        // Latest session_info wins; empty names clear (upstream).
        name = info
            .name
            .as_ref()
            .map(|name| name.trim().to_owned())
            .filter(|name| !name.is_empty());
    }
    name
}

/// One open for the header-only listing: bounded header scan, cwd filter,
/// then the session_info-only tail scan.
fn read_session_header_listing(
    path: &Path,
    resolved_cwd: Option<&str>,
) -> Option<(SessionHeader, Option<String>)> {
    SESSION_HEADER_LIST_FILE_READS.fetch_add(1, Ordering::Relaxed);
    let mut reader = std::io::BufReader::new(std::fs::File::open(path).ok()?);
    let header = read_session_header_from_reader(&mut reader)
        .ok()
        .flatten()?;
    if let Some(reference) = resolved_cwd {
        if !session_cwd_matches(Some(header.cwd.as_str()), reference) {
            return None;
        }
    }
    let name = peek_session_name(&mut reader);
    Some((header, name))
}

/// Whether a header cwd matches the reference cwd (upstream
/// `sessionCwdMatches`: resolved-path comparison). Takes the header cwd
/// borrowed: the listings used to build a `Some(header.cwd.clone())` just to
/// call this, one `String` copy per candidate file.
fn session_cwd_matches(cwd: Option<&str>, resolved_cwd: &str) -> bool {
    cwd.filter(|value| !value.is_empty())
        .is_some_and(|value| resolve_path_str(value) == resolved_cwd)
}

/// The process working directory as a string (upstream `process.cwd()`).
fn current_cwd_string() -> String {
    std::env::current_dir()
        .map(|dir| dir.to_string_lossy().into_owned())
        .unwrap_or_else(|_| String::from("."))
}

/// Path of the most recent session file for `cwd`, if any (upstream
/// `findMostRecentSession`).
fn find_most_recent_session(session_dir: &Path, cwd: Option<&str>) -> Option<PathBuf> {
    let resolved_cwd = cwd.map(resolve_path_str);
    let mut files: Vec<(PathBuf, SystemTime)> = std::fs::read_dir(session_dir)
        .ok()?
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().and_then(|value| value.to_str()) == Some("jsonl"))
        .filter_map(|path| {
            let mtime = std::fs::metadata(&path).ok()?.modified().ok()?;
            Some((path, mtime))
        })
        .collect();
    files.sort_by(|(path_a, time_a), (path_b, time_b)| {
        time_b.cmp(time_a).then_with(|| path_b.cmp(path_a))
    });
    files.into_iter().map(|(path, _)| path).find(|path| {
        read_session_header_for_discovery(path).is_some_and(|header| {
            resolved_cwd
                .as_deref()
                .is_none_or(|reference| session_cwd_matches(Some(header.cwd.as_str()), reference))
        })
    })
}

// ---------------------------------------------------------------------------
// SessionManager
// ---------------------------------------------------------------------------

/// Whether one entry makes the session a conversation (the file is created
/// only once a user or assistant message exists, upstream `_hasConversation`).
fn entry_is_conversation(entry: &FileEntry) -> bool {
    match entry {
        FileEntry::Message(inner) => matches!(inner.message.role(), "user" | "assistant"),
        _ => false,
    }
}

/// A deterministic append failure injected by a test (the production writer
/// has no failure seam: real errors come from the OS).
#[doc(hidden)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppendFailure {
    /// Fail before writing any bytes of the new entry.
    BeforeWrite,
    /// Write `bytes` bytes of the new entry's line, then fail (a short
    /// `write_all`, the case that used to leave a partial JSONL line).
    PartialWrite(usize),
}

/// Manages one conversation session as an append-only entry tree stored in a
/// JSONL file (the pir facade over upstream `SessionManager` semantics).
#[derive(Debug)]
pub struct SessionManager {
    session_id: String,
    /// Planned/actual session file path (`None` when not persisting).
    session_file: Option<PathBuf>,
    session_dir: PathBuf,
    cwd: String,
    persist: bool,
    /// Whether the file exists and future appends go through the fast path.
    flushed: bool,
    /// Reused append handle for `session_file` (opened lazily once the file
    /// exists). Every append still writes the line through to the OS, so a
    /// fresh open sees each completed line immediately; only the `sync_all`
    /// durability barrier is deferred (see [`SessionManager::sync_now`] and
    /// the `Drop` impl). Cleared whenever the path or the file contents are
    /// replaced ([`SessionManager::rewrite_file`],
    /// [`SessionManager::set_session_file`], a new/branched session) so a
    /// stale descriptor can never append to an unlinked or superseded file.
    append_file: Option<std::fs::File>,
    /// Length of `session_file` as written through [`Self::append_file`].
    /// A path whose length no longer matches was deleted, truncated or
    /// replaced behind the handle; the handle is then dropped and reopened
    /// by path (a deleted path fails exactly like the old per-append open),
    /// so an append can never vanish into an unlinked inode.
    append_len: u64,
    /// Every parsed line, header first (upstream `fileEntries`). Entries are
    /// `Arc`-shared with the `by_id` index so each parsed entry is held once
    /// in memory instead of twice (the index clone used to deep-copy every
    /// message, including full tool-result file contents, on every load and
    /// append).
    file_entries: Vec<Arc<FileEntry>>,
    /// Raw text of lines that failed to deserialize on load. Their presence
    /// refuses migration rewrites so the file is never erased mid-entry.
    unparsed_raw_lines: Vec<String>,
    by_id: BTreeMap<String, Arc<FileEntry>>,
    /// Interned `toolsAdded` payloads shared by every compaction checkpoint.
    /// The loadout is identical across hundreds of compactions; storing one
    /// `Arc` per distinct schema instead of a deep `serde_json::Value` per
    /// checkpoint keeps long compaction-heavy sessions bounded.
    tool_schema_intern: HashMap<String, Arc<JsonValue>>,
    /// Latest label per target id, indexed by `build_index` from the
    /// resident `file_entries` only: on a paged manager a label change in the
    /// skipped prefix is absent here (see [`SessionManager::get_label`]).
    labels_by_id: BTreeMap<String, String>,
    label_timestamps_by_id: BTreeMap<String, String>,
    leaf_id: Option<String>,
    /// Injected failure for the NEXT append (test seam; always `None` in
    /// production).
    append_failure: Option<AppendFailure>,
    /// Injected stall for the NEXT append (test/profiling seam; always `None`
    /// in production). Executed where the file write happens, so an off-thread
    /// append proves the async runtime is not blocked.
    append_stall: Option<std::time::Duration>,
    /// Injected stall for the NEXT diagnostic compaction-trigger WIDEN (test
    /// seam; always `None` in production). Executed where the blocking widen
    /// runs, so callers can prove the async runtime is not blocked.
    widen_stall: Option<std::time::Duration>,
    /// Number of full projections this manager has built (test/profiling
    /// probe for "one projection per prompt": a prompt must not build the
    /// transcript twice when no compaction runs).
    projection_builds: AtomicU64,
    /// PAGED mode (pir extension): the entries before the tail window stayed on
    /// disk and are NOT resident. See [`SessionManager::open_paged`].
    paged: Option<PagedWindow>,
}

/// The paged-open state of a [`SessionManager`]: the byte window that is NOT
/// resident. Every API that would need the prefix either streams it
/// ([`SessionManager::for_each_prefix_entry`]) or refuses the operation
/// ([`SessionManager::rewrite_file`]), so a tail window can never be written
/// back as if it were the whole session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PagedWindow {
    /// Byte offset where the resident tail window starts.
    pub page_start: u64,
    /// File length at the time of the paged open.
    pub file_bytes: u64,
    /// Entries before the resident window (from the page's parse counters).
    pub parsed_entries: usize,
}

/// Options for [`SessionManager::new_session`] (upstream `NewSessionOptions`).
#[derive(Debug, Clone, Default)]
pub struct NewSessionOptions {
    pub id: Option<String>,
    pub parent_session: Option<String>,
}

/// A persisted session whose header `cwd` no longer exists (upstream
/// `SessionCwdIssue`, session-cwd.ts:3-7).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MissingSessionCwd {
    pub session_file: PathBuf,
    pub session_cwd: String,
}

impl MissingSessionCwd {
    /// Upstream `formatMissingSessionCwdError` (session-cwd.ts:35-38).
    pub fn error_message(&self, fallback_cwd: &str) -> String {
        format!(
            "Stored session working directory does not exist: {}\nSession file: {}\nCurrent working directory: {}",
            self.session_cwd,
            self.session_file.display(),
            fallback_cwd
        )
    }
}

impl SessionManager {
    fn new(
        cwd: &str,
        session_dir: &Path,
        session_file: Option<PathBuf>,
        persist: bool,
        new_session_options: Option<NewSessionOptions>,
        preloaded_file_entries: Option<Vec<FileEntry>>,
    ) -> crate::Result<Self> {
        // `session` feature OFF: this build has no JSONL reader/writer. Every
        // file-backed constructor funnels through here, so this is the ONE
        // degradation seam: asking for a file fails with the typed notice, and
        // asking for a session (persist == false, or `create`, which the
        // feature-off twins below route here) gets a fully working in-memory
        // manager whose `session_file()` is `None`.
        #[cfg(not(feature = "session"))]
        {
            // No JSONL reader exists in this build, so a preloaded entry list
            // has no consumer; keep the parameter for the shared constructor
            // signature without a dead-code/unused-variable warning.
            let _ = preloaded_file_entries;
            if session_file.is_some() || persist {
                if session_file.is_some() {
                    return Err(anyhow::anyhow!(SESSION_PERSISTENCE_DISABLED));
                }
                tracing::warn!("{SESSION_PERSISTENCE_DISABLED}");
            }
            Self::new_in_memory_only(cwd, session_dir, new_session_options)
        }
        #[cfg(feature = "session")]
        {
            Self::new_with_persistence(
                cwd,
                session_dir,
                session_file,
                persist,
                new_session_options,
                preloaded_file_entries,
            )
        }
    }

    /// Build a working, never-persisted manager (the `session`-feature-off
    /// shape of every constructor). Kept separate so the enabling path above is
    /// byte-identical to the pre-gate code.
    #[cfg(not(feature = "session"))]
    fn new_in_memory_only(
        cwd: &str,
        session_dir: &Path,
        new_session_options: Option<NewSessionOptions>,
    ) -> crate::Result<Self> {
        let mut manager = Self {
            session_id: String::new(),
            session_file: None,
            session_dir: session_dir.to_path_buf(),
            cwd: resolve_path_str(cwd),
            persist: false,
            flushed: false,
            append_file: None,
            append_len: 0,
            file_entries: Vec::new(),
            unparsed_raw_lines: Vec::new(),
            by_id: BTreeMap::new(),
            tool_schema_intern: HashMap::new(),
            labels_by_id: BTreeMap::new(),
            label_timestamps_by_id: BTreeMap::new(),
            leaf_id: None,
            append_failure: None,
            append_stall: None,
            widen_stall: None,
            projection_builds: AtomicU64::new(0),
            paged: None,
        };
        manager.new_session(new_session_options)?;
        Ok(manager)
    }

    #[cfg(feature = "session")]
    fn new_with_persistence(
        cwd: &str,
        session_dir: &Path,
        session_file: Option<PathBuf>,
        persist: bool,
        new_session_options: Option<NewSessionOptions>,
        preloaded_file_entries: Option<Vec<FileEntry>>,
    ) -> crate::Result<Self> {
        let cwd = resolve_path_str(cwd);
        if persist && !session_dir.as_os_str().is_empty() && !session_dir.exists() {
            std::fs::create_dir_all(session_dir)?;
        }
        let mut manager = Self {
            session_id: String::new(),
            session_file: None,
            session_dir: session_dir.to_path_buf(),
            cwd,
            persist,
            flushed: false,
            append_file: None,
            append_len: 0,
            file_entries: Vec::new(),
            unparsed_raw_lines: Vec::new(),
            by_id: BTreeMap::new(),
            tool_schema_intern: HashMap::new(),
            labels_by_id: BTreeMap::new(),
            label_timestamps_by_id: BTreeMap::new(),
            leaf_id: None,
            append_failure: None,
            append_stall: None,
            widen_stall: None,
            projection_builds: AtomicU64::new(0),
            paged: None,
        };
        match session_file {
            Some(path) => manager.set_session_file(&path, preloaded_file_entries)?,
            None => match preloaded_file_entries {
                Some(entries) if !entries.is_empty() => {
                    manager.load_entries(entries, new_session_options)?
                }
                _ => {
                    manager.new_session(new_session_options)?;
                }
            },
        }
        Ok(manager)
    }

    /// Switch to a different session file (used for resume and branching).
    pub fn set_session_file(
        &mut self,
        path: &Path,
        preloaded: Option<Vec<FileEntry>>,
    ) -> crate::Result<()> {
        // Any cached append handle belongs to the previous file.
        self.append_file = None;
        self.append_len = 0;
        self.session_file = Some(path.to_path_buf());
        if path.exists() {
            let entries = match preloaded {
                Some(entries) => {
                    // Preloaded entries carry no unparsed raw lines of their own.
                    self.unparsed_raw_lines.clear();
                    entries
                }
                None => {
                    let mut loaded = load_entries_retrying_transient_empties(path)?;
                    // The raw undeserializable lines move out (no clone); the
                    // parse result is consumed right below.
                    self.unparsed_raw_lines = std::mem::take(&mut loaded.unparsed_lines);
                    if !self.unparsed_raw_lines.is_empty() {
                        tracing::warn!(
                            "session {} contains {} undeserializable line(s); \
                             migration rewrites are refused to preserve them",
                            path.display(),
                            self.unparsed_raw_lines.len()
                        );
                    }
                    loaded.into_entries()
                }
            };
            if entries.is_empty() {
                // Empty file: initialize with a valid header. A non-empty file
                // that did not parse must fail without modifying it.
                if std::fs::metadata(path)?.len() > 0 {
                    anyhow::bail!("Session file is not a valid pi session: {}", path.display());
                }
                self.unparsed_raw_lines.clear();
                self.new_session(None)?;
                self.session_file = Some(path.to_path_buf());
                self.rewrite_file()?;
                self.flushed = true;
                return Ok(());
            }
            self.load_entries(entries, None)?;
            self.flushed = true;
        } else {
            self.unparsed_raw_lines.clear();
            self.new_session(None)?;
            // Preserve the explicit path (the `--session` flag).
            self.session_file = Some(path.to_path_buf());
        }
        Ok(())
    }

    /// Reset to a fresh session (upstream `newSession`): new header, empty
    /// tree, planned file name; nothing is written yet.
    pub fn new_session(
        &mut self,
        options: Option<NewSessionOptions>,
    ) -> crate::Result<Option<PathBuf>> {
        let options = options.unwrap_or_default();
        if let Some(id) = &options.id {
            assert_valid_session_id(id)?;
        }
        self.session_id = options.id.clone().unwrap_or_else(new_session_id);
        let timestamp = iso_from_ms(now_ms());
        let header =
            SessionHeader::new(self.session_id.clone(), self.cwd.clone(), timestamp.clone())
                .with_parent(options.parent_session.clone());
        self.file_entries = vec![Arc::new(FileEntry::Session(header))];
        self.unparsed_raw_lines.clear();
        self.by_id.clear();
        self.tool_schema_intern.clear();
        self.labels_by_id.clear();
        self.label_timestamps_by_id.clear();
        self.leaf_id = None;
        self.flushed = false;
        // The new session has no file yet (and may reuse the old path).
        self.append_file = None;
        self.append_len = 0;

        if self.persist {
            let file_timestamp = timestamp.replace([':', '.'], "-");
            let path = self
                .session_dir
                .join(format!("{file_timestamp}_{}.jsonl", self.session_id));
            self.session_file = Some(path);
        }
        Ok(self.session_file.clone())
    }

    fn load_entries(
        &mut self,
        entries: Vec<FileEntry>,
        options: Option<NewSessionOptions>,
    ) -> crate::Result<()> {
        let entries: Vec<Arc<FileEntry>> = entries.into_iter().map(Arc::new).collect();
        self.load_shared_entries(entries, options)
    }

    /// [`Self::load_entries`] over ALREADY SHARED entries: the paged open
    /// hands over the tail page's own `Arc<FileEntry>` handles, so loading a
    /// resident window never deep-clones every stored tool result just to
    /// wrap the copies in new `Arc`s. Only compaction entries need mutable
    /// access (for `toolsAdded` interning); `Arc::make_mut` clones at most
    /// that one entry, never the transcript.
    fn load_shared_entries(
        &mut self,
        mut entries: Vec<Arc<FileEntry>>,
        options: Option<NewSessionOptions>,
    ) -> crate::Result<()> {
        // Upstream `_loadEntries` searches the WHOLE list for the session
        // header (`entries.find((e) => e.type === "session")`), not just the
        // first entry.
        let header = entries.iter().find_map(|entry| entry.as_header()).cloned();
        match header {
            Some(header) => {
                self.session_id = header.id.clone();
                // Upstream `open` adopts the session header cwd.
                self.cwd = resolve_path_str(&header.cwd);
                self.tool_schema_intern.clear();
                for entry in &mut entries {
                    if matches!(entry.as_ref(), FileEntry::Compaction(_)) {
                        self.intern_compaction_tools(Arc::make_mut(entry));
                    }
                }
                self.file_entries = entries;
                if self.migrate_to_current_version() {
                    // Never erase undeserializable lines: refuse the rewrite
                    // when the file carried any (upstream keeps unknown data;
                    // a rewrite from the parsed subset would drop it).
                    if self.unparsed_raw_lines.is_empty() {
                        self.rewrite_file()?;
                    } else {
                        tracing::warn!(
                            "session {} holds undeserializable lines; migration \
                             applied in memory but the file was NOT rewritten",
                            self.session_file
                                .as_deref()
                                .map(Path::display)
                                .unwrap_or_else(|| Path::new("?").display())
                        );
                    }
                }
            }
            None => {
                self.new_session(options)?;
                for entry in &mut entries {
                    if matches!(entry.as_ref(), FileEntry::Compaction(_)) {
                        self.intern_compaction_tools(Arc::make_mut(entry));
                    }
                }
                self.file_entries.extend(entries);
            }
        }
        self.build_index();
        Ok(())
    }

    /// Migrate v1/v2 sessions in place (upstream `migrateToCurrentVersion`).
    /// v1 → v2 adds the id/parentId tree (ids and linear parent chaining are
    /// assigned ONLY where missing, upstream `migrateV1ToV2`) and converts
    /// compaction `firstKeptEntryIndex` to `firstKeptEntryId`; v2 → v3 ONLY
    /// bumps the header version and renames the `hookMessage` message role to
    /// `custom` (the rename itself happens at parse time in
    /// `load_entries_from_file` so the migrated messages deserialize; upstream
    /// `migrateV2ToV3`) — a v2 file's id/parentId branch structure is
    /// preserved untouched.
    fn migrate_to_current_version(&mut self) -> bool {
        let version = self
            .file_entries
            .iter()
            .find(|entry| entry.is_header())
            .and_then(|entry| entry.as_header())
            .and_then(|header| header.version)
            .unwrap_or(1);
        if version >= CURRENT_SESSION_VERSION {
            return false;
        }
        if version < 2 {
            // v1 → v2 (upstream `migrateV1ToV2`): entries carry no ids, so
            // assign them and chain the linear parent tree; the chaining is
            // skipped for any entry that already has an id so an existing
            // structure is never re-linked.
            let mut used_ids: std::collections::HashSet<String> = self
                .file_entries
                .iter()
                .filter_map(|entry| entry.id())
                .filter(|id| !id.is_empty())
                .map(str::to_owned)
                .collect();
            let mut previous_id: Option<String> = None;
            for entry in &mut self.file_entries {
                // Only the migration path mutates stored entries; `make_mut`
                // clones (at most) the single entry being migrated.
                match Arc::make_mut(entry) {
                    FileEntry::Session(header) => header.version = Some(CURRENT_SESSION_VERSION),
                    FileEntry::Message(MessageEntry { id, parent_id, .. })
                    | FileEntry::ThinkingLevelChange(ThinkingLevelChangeEntry {
                        id,
                        parent_id,
                        ..
                    })
                    | FileEntry::ModelChange(ModelChangeEntry { id, parent_id, .. })
                    | FileEntry::Usage(UsageEntry { id, parent_id, .. })
                    | FileEntry::Compaction(CompactionEntry { id, parent_id, .. })
                    | FileEntry::BranchSummary(BranchSummaryEntry { id, parent_id, .. })
                    | FileEntry::Custom(CustomEntry { id, parent_id, .. })
                    | FileEntry::CustomMessage(CustomMessageEntry { id, parent_id, .. })
                    | FileEntry::ContextEdit(ContextEditEntry { id, parent_id, .. })
                    | FileEntry::Label(LabelEntry { id, parent_id, .. })
                    | FileEntry::SessionInfo(SessionInfoEntry { id, parent_id, .. }) => {
                        if id.is_empty() {
                            let new_id = loop {
                                let candidate = new_entry_id();
                                if !used_ids.contains(&candidate) {
                                    break candidate;
                                }
                            };
                            used_ids.insert(new_id.clone());
                            *id = new_id;
                            *parent_id = previous_id.clone();
                        }
                        previous_id = Some(id.clone());
                    }
                }
            }
            // Convert compaction firstKeptEntryIndex → firstKeptEntryId.
            let entry_ids: Vec<Option<String>> = self
                .file_entries
                .iter()
                .map(|entry| entry.id().map(str::to_owned))
                .collect();
            for entry in &mut self.file_entries {
                if let FileEntry::Compaction(compaction) = Arc::make_mut(entry) {
                    if let Some(first_kept_index) = compaction.first_kept_entry_index.take() {
                        if let Some(Some(target)) = entry_ids.get(first_kept_index as usize) {
                            compaction.first_kept_entry_id = target.clone();
                        }
                    }
                }
            }
        }
        // v2 → v3 (and the v1 → v3 header bump): only the version changes.
        // The `hookMessage` → `custom` role rename happens at parse time; no
        // id/parentId mutation, so a v2 branch tree survives the migration.
        for entry in &mut self.file_entries {
            if let FileEntry::Session(header) = Arc::make_mut(entry) {
                header.version = Some(CURRENT_SESSION_VERSION);
            }
        }
        true
    }

    fn build_index(&mut self) {
        self.by_id.clear();
        self.labels_by_id.clear();
        self.label_timestamps_by_id.clear();
        self.leaf_id = None;
        for entry in &self.file_entries {
            if entry.is_header() {
                continue;
            }
            let id = entry.entry_id().to_owned();
            self.by_id.insert(id.clone(), entry.clone());
            self.leaf_id = Some(id);
            if let FileEntry::Label(label) = entry.as_ref() {
                let target = label.target_id.clone();
                // An empty label clears like a missing one (upstream `if (label)`).
                match label.label.as_deref().filter(|text| !text.is_empty()) {
                    Some(text) => {
                        self.labels_by_id.insert(target.clone(), text.to_owned());
                        self.label_timestamps_by_id
                            .insert(target, label.timestamp.clone());
                    }
                    None => {
                        self.labels_by_id.remove(&target);
                        self.label_timestamps_by_id.remove(&target);
                    }
                }
            }
        }
    }

    /// Refuse a whole-file rewrite on a paged manager BEFORE the caller
    /// mutates anything.
    ///
    /// Never write a tail window back as if it were the whole session: a
    /// rewrite (migration, branch-to-new-file) would silently drop the unread
    /// prefix. Callers that need a rewrite widen first (`widen_to_full`).
    /// APPEND paths are unaffected. This is shared by [`Self::rewrite_file`]
    /// and [`Self::create_branched_session`] so both refuse with the same
    /// honest error — the branch path must bail at its TOP, because by the
    /// time its final `rewrite_file` runs it has already replaced the
    /// entries/id/path with a truncated transcript.
    fn reject_paged_rewrite(&self) -> crate::Result<()> {
        if self.paged.is_some() {
            anyhow::bail!(
                "session {} is open as a tail page; reopen it fully before rewriting",
                self.session_file
                    .as_deref()
                    .map(Path::display)
                    .unwrap_or_else(|| Path::new("?").display())
            );
        }
        Ok(())
    }

    fn rewrite_file(&mut self) -> crate::Result<()> {
        // The rewrite replaces the file, so any cached append handle would
        // point at the superseded file and silently drop later appends: drop
        // it first. The next append reopens lazily.
        self.append_file = None;
        self.append_len = 0;
        self.reject_paged_rewrite()?;
        if !self.persist {
            return Ok(());
        }
        let Some(path) = self.session_file.clone() else {
            return Ok(());
        };
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut content = String::new();
        for entry in &self.file_entries {
            content.push_str(&serde_json::to_string(entry)?);
            content.push('\n');
        }
        // Atomic rewrite: write the full content to a same-directory temp
        // file, flush + fsync it, then rename it over the session file. A
        // crash mid-write can no longer corrupt the live session file — the
        // worst case is a leftover temp file.
        //
        // The temp name is UNIQUE per rewrite (pid + a process-local counter):
        // two writers rewriting the same session path must never share one
        // temp file, or their contents would interleave before the rename and
        // the last rename would publish a mixture of both transcripts.
        //
        // ORDERING / DURABILITY INVARIANT: the published path only ever names
        // a file whose bytes are complete and flushed. (1) the whole
        // transcript is written to the temp file and `sync_all`ed BEFORE the
        // rename, so no reader and no crash can observe a short transcript
        // under the live name; (2) the rename is one atomic
        // `MoveFileExW(MOVEFILE_REPLACE_EXISTING)` / `rename(2)`, so the live
        // path never passes through a missing or zero-length state (the old
        // `remove_file` + rename fallback did); (3) the parent directory is
        // synced afterwards on Unix, where a rename is only durable once the
        // directory entry is. The live path is never truncated or removed
        // first, and `create_new` on the temp means a recycled pid can never
        // inherit a stale crashed writer's bytes.
        let (mut file, tmp_path) = create_new_rewrite_temp(&path)?;
        let write_result: crate::Result<()> = (|| {
            file.write_all(content.as_bytes())?;
            file.flush()?;
            file.sync_all()?;
            Ok(())
        })();
        // The handle must be closed before the rename: Windows cannot rename a
        // file whose handle is still open without delete sharing.
        drop(file);
        let write_result = write_result.and_then(|()| {
            // `std::fs::rename` replaces an existing destination atomically on
            // Unix and on Windows (the standard library maps it to
            // `MoveFileExW(MOVEFILE_REPLACE_EXISTING)`), so ONE call is both
            // atomic and sufficient. The previous `remove_file` fallback was
            // destructive: when the rename failed because another process held
            // the session open, removing the destination first could leave NO
            // session file at all. A failed rename now leaves the live file
            // intact and only the fully-written temp behind.
            std::fs::rename(&tmp_path, &path)?;
            sync_parent_dir(&path);
            Ok(())
        });
        if write_result.is_err() {
            let _ = std::fs::remove_file(&tmp_path);
        }
        write_result
    }

    /// Whether the session contains a user or assistant message (upstream
    /// `_hasConversation`): the file is created only once it does, so a
    /// prompt survives a failed first turn.
    fn has_conversation(&self) -> bool {
        self.file_entries
            .iter()
            .any(|entry| entry_is_conversation(entry.as_ref()))
    }

    /// Persist one new entry BEFORE the caller commits it to memory.
    ///
    /// The write is the fallible half of an append; the in-memory index
    /// (`by_id`, `file_entries`, `leaf_id`, the label maps) is committed only
    /// after it succeeds. A failed append therefore cannot leave a phantom
    /// leaf a later append parents against (the reopened file would truncate
    /// history at the missing parent), and a retry cannot duplicate the entry.
    ///
    /// Two modes, matching the file lifecycle:
    ///
    /// - **Creating** (`!flushed`): the whole transcript so far plus `entry`
    ///   is written through a `create_new` handle. A failure removes the
    ///   partially written file (nothing existed under the path before), so
    ///   no malformed line survives and the next conversation append can
    ///   create it afresh.
    /// - **Appending** (`flushed`): only `entry` is written through the
    ///   reusable handle. A failure truncates the file back to its previous
    ///   length, so a short `write_all` never leaves a partial JSONL line.
    fn persist_entry(&mut self, entry: &Arc<FileEntry>) -> crate::Result<()> {
        if !self.persist {
            return Ok(());
        }
        let Some(path) = self.session_file.clone() else {
            return Ok(());
        };
        // Consumed even on the file-creation path so a seam armed before the
        // first conversation append cannot fire on a later unrelated append.
        let failure = self.append_failure.take();
        // Test/profiling seam: sleep where the blocking write happens. The
        // async entry point runs this on the blocking pool, so the runtime
        // stays responsive; a synchronous caller would block its own thread.
        if let Some(stall) = self.append_stall.take() {
            std::thread::sleep(stall);
        }
        if !self.flushed {
            if !self.has_conversation() && !entry_is_conversation(entry.as_ref()) {
                return Ok(());
            }
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            // `wx` semantics: fail if the file appeared meanwhile.
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
                .map_err(|error| {
                    anyhow::anyhow!("cannot create session {}: {error}", path.display())
                })?;
            use std::io::Write;
            let mut written = 0u64;
            let write_result: crate::Result<()> = (|| {
                for existing in &self.file_entries {
                    let line = serde_json::to_string(existing)?;
                    file.write_all(line.as_bytes())?;
                    file.write_all(b"\n")?;
                    written = written.saturating_add(line.len() as u64 + 1);
                }
                let line = serde_json::to_string(entry)?;
                file.write_all(line.as_bytes())?;
                file.write_all(b"\n")?;
                written = written.saturating_add(line.len() as u64 + 1);
                file.flush()?;
                // The creation barrier stays: this is the one write that may
                // not wait for an explicit sync boundary before the file
                // exists at all (same durability as before).
                file.sync_all()?;
                Ok(())
            })();
            if let Err(error) = write_result {
                // Restore the pre-call state: no file existed (the open used
                // `create_new`), so removing it also discards any partial
                // line a short write left behind.
                drop(file);
                let _ = std::fs::remove_file(&path);
                return Err(error);
            }
            // Reuse the created descriptor for every later append.
            self.append_file = Some(file);
            self.append_len = written;
            self.flushed = true;
            return Ok(());
        }
        let line = serde_json::to_string(entry)?;
        // A cached handle outlives an external delete/truncate/replace: when
        // the path no longer names a file of the length written through it,
        // drop the handle and reopen by path. A missing path then fails
        // exactly like the old per-append open did, instead of silently
        // appending to the unlinked inode and losing the entry.
        if self.append_file.is_some()
            && !std::fs::metadata(&path).is_ok_and(|metadata| metadata.len() == self.append_len)
        {
            self.append_file = None;
            self.append_len = 0;
        }
        if self.append_file.is_none() {
            let opened = std::fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .map_err(|error| {
                    anyhow::anyhow!("cannot append to session {}: {error}", path.display())
                })?;
            self.append_len = opened.metadata()?.len();
            self.append_file = Some(opened);
        }
        // This is the length the file must return to when the write fails.
        let previous_len = self.append_len;
        let write_result: crate::Result<()> = {
            let Some(file) = self.append_file.as_mut() else {
                return Err(anyhow::anyhow!(
                    "session append handle for {} was not initialized",
                    path.display()
                ));
            };
            use std::io::Write;
            (|| {
                if matches!(failure, Some(AppendFailure::BeforeWrite)) {
                    anyhow::bail!("injected session append failure");
                }
                file.write_all(line.as_bytes())?;
                if let Some(AppendFailure::PartialWrite(bytes)) = failure {
                    file.write_all(&line.as_bytes()[..bytes.min(line.len())])?;
                    anyhow::bail!("injected session append failure");
                }
                file.write_all(b"\n")?;
                // Visibility barrier, not a durability one: `File` has no
                // user-space buffer, so the line is already readable by a
                // fresh open after this (a no-op) flush. Only `sync_all` is
                // deferred to [`Self::sync_now`] / session close, where
                // upstream's `appendFileSync` never fsynced per append at all.
                file.flush()?;
                Ok(())
            })()
        };
        if let Err(error) = write_result {
            // Truncate back to the previous transcript length. The handle is
            // tried first; a handle that cannot truncate falls back to a fresh
            // path handle, so the common failure (a short write) leaves no
            // partial JSONL line behind.
            let truncated = self
                .append_file
                .as_mut()
                .is_some_and(|file| file.set_len(previous_len).is_ok());
            if !truncated {
                let _ = std::fs::OpenOptions::new()
                    .write(true)
                    .open(&path)
                    .and_then(|file| file.set_len(previous_len));
            }
            // Drop the cached handle in every case: a failed `write_all` may
            // have advanced its cursor past the truncation (Windows writes at
            // the handle position), and reopening on the next append also
            // re-reads the real length when truncation itself failed.
            self.append_file = None;
            self.append_len = previous_len;
            return Err(error);
        }
        self.append_len = previous_len.saturating_add(line.len() as u64 + 1);
        Ok(())
    }

    /// Inject a deterministic failure into the next append (test/profiling
    /// seam). The append path persists before committing memory and truncates
    /// a partial line, so both variants must leave the transcript intact.
    #[doc(hidden)]
    pub fn fail_next_append_for_test(&mut self, failure: AppendFailure) {
        self.append_failure = Some(failure);
    }

    /// Inject a deterministic stall into the next append (test/profiling
    /// seam). The sleep runs on whatever thread performs the file write, so
    /// [`Self::append_message_off_thread`] proves the async runtime is free
    /// while the blocking pool is busy.
    #[doc(hidden)]
    pub fn stall_next_append_for_test(&mut self, stall: std::time::Duration) {
        self.append_stall = Some(stall);
    }

    /// Inject a deterministic stall into the next [`Self::widen_to_full`]
    /// (test/profiling seam). The sleep runs on whatever thread performs the
    /// widen, so the paged run-start path can prove the widen runs on the
    /// blocking pool and not on a runtime worker.
    #[doc(hidden)]
    pub fn stall_next_widen_for_test(&mut self, stall: std::time::Duration) {
        self.widen_stall = Some(stall);
    }

    /// Share one `toolsAdded` allocation across all compaction checkpoints
    /// with the same loadout. The JSON stays byte-identical on disk; only the
    /// in-memory representation is interned.
    fn intern_compaction_tools(&mut self, entry: &mut FileEntry) {
        let FileEntry::Compaction(compaction) = entry else {
            return;
        };
        let Some(system) = compaction.system_message.as_mut() else {
            return;
        };
        let Some(tools) = system.tools_added.take() else {
            return;
        };
        let Ok(key) = serde_json::to_string(tools.as_ref()) else {
            system.tools_added = Some(tools);
            return;
        };
        if let Some(existing) = self.tool_schema_intern.get(&key) {
            system.tools_added = Some(Arc::clone(existing));
        } else {
            self.tool_schema_intern.insert(key, Arc::clone(&tools));
            system.tools_added = Some(tools);
        }
    }

    fn append_entry(&mut self, mut entry: FileEntry) -> crate::Result<String> {
        self.intern_compaction_tools(&mut entry);
        let id = entry.entry_id().to_owned();
        let entry = Arc::new(entry);
        // Persist before committing ANY memory state (by_id, file_entries,
        // leaf_id, label maps): on error the transcript keeps its previous
        // leaf and the next append parents correctly against it.
        self.persist_entry(&entry)?;
        if let FileEntry::Label(label) = entry.as_ref() {
            let target = label.target_id.clone();
            // An empty label clears like a missing one (upstream `if (label)`).
            match label.label.as_deref().filter(|text| !text.is_empty()) {
                Some(text) => {
                    self.labels_by_id.insert(target.clone(), text.to_owned());
                    self.label_timestamps_by_id
                        .insert(target, label.timestamp.clone());
                }
                None => {
                    self.labels_by_id.remove(&target);
                    self.label_timestamps_by_id.remove(&target);
                }
            }
        }
        // One allocation per append: the same `Arc` is shared between the
        // index and the entry list instead of deep-copying the whole message
        // (tool results carry full file contents) into `by_id`.
        self.by_id.insert(id.clone(), Arc::clone(&entry));
        self.file_entries.push(entry);
        self.leaf_id = Some(id.clone());
        Ok(id)
    }

    /// Flush the session's reusable append handle and wait for the
    /// durability barrier (`sync_all`).
    ///
    /// Every appended line is already complete and visible to a fresh open
    /// (the file handle has no user-space buffer); this only defers the
    /// fsync, which upstream's `appendFileSync` never paid per append
    /// either. Hosts call this at explicit boundaries — turn end / session
    /// close — instead of paying one fsync per entry. [`Drop`] syncs
    /// best-effort as the session-close boundary.
    pub fn sync_now(&self) -> crate::Result<()> {
        if let Some(file) = &self.append_file {
            file.sync_all()?;
        }
        Ok(())
    }

    // ------------------------------------------------------------------
    // Construction
    // ------------------------------------------------------------------

    /// Create a new session under `session_dir`. The file itself is created
    /// lazily on the first user or assistant message.
    pub fn create(cwd: &str, session_dir: impl AsRef<Path>) -> crate::Result<Self> {
        Self::new(cwd, session_dir.as_ref(), None, true, None, None)
    }

    /// Create a new session file whose id (and file name) is `id` (upstream
    /// `SessionManager.create(cwd, dir, { id })`).
    pub fn create_with_id(cwd: &str, session_dir: &Path, id: &str) -> crate::Result<Self> {
        Self::new(
            cwd,
            session_dir,
            None,
            true,
            Some(NewSessionOptions {
                id: Some(id.to_owned()),
                parent_session: None,
            }),
            None,
        )
    }

    /// Open a specific session file (replays and repairs a missing trailing
    /// newline). The constructor cwd comes from the session header, or from
    /// `process.cwd()` when the file has no header (upstream `open`: an empty
    /// file being initialized must not adopt the file's directory as cwd).
    /// Open a session TAIL-FIRST (pir extension, the large-session path): only
    /// a byte- and entry-capped tail window is parsed and resident; the skipped
    /// prefix stays on disk and is reachable through
    /// [`Self::for_each_prefix_entry`] (streaming, one entry at a time) or
    /// [`Self::widen_to_full`].
    ///
    /// Falls back to [`Self::open`] (the full streaming load) when the file is
    /// empty, cannot be mapped, or fits entirely inside the caps — so a small
    /// session is byte-for-byte the normal open.
    ///
    /// Correctness rules for a paged manager:
    ///
    /// - the projection/context APIs see the RESIDENT WINDOW only; a caller
    ///   that needs the whole transcript must call [`Self::widen_to_full`]
    ///   (which materializes the prefix, the cost the paged mode exists to
    ///   avoid) — nothing silently reports a truncated transcript as complete:
    ///   [`Self::is_paged`] is the flag to check;
    /// - writes APPEND (the file is append-only); any REWRITE (migration,
    ///   branch-to-new-file) is refused with an error instead of dropping the
    ///   prefix;
    /// - [`Self::unparsed_raw_lines`] carries the window's undeserializable
    ///   lines only; the prefix is untouched and never rewritten.
    #[cfg(feature = "session")]
    pub fn open_paged(
        path: impl AsRef<Path>,
        max_bytes: usize,
        max_entries: usize,
    ) -> crate::Result<Self> {
        let path = path.as_ref().to_path_buf();
        let page = tail_page::SessionTailPage::open(&path, max_bytes, max_entries)?;
        let Some(page) = page else {
            return Self::open(&path);
        };
        let stats = page.stats();
        if stats.whole_file {
            // The page covers the file: the normal open is identical.
            return Self::open(&path);
        }
        // The PAGED open deliberately skips the full loader, which is also
        // where a torn trailing line is repaired (upstream appends the missing
        // newline). Without this repair the manager's next append would
        // concatenate its JSON onto the unterminated last line, and EVERY
        // later load would lose both entries (the joined line no longer
        // parses). A crash between the line write and its newline is exactly
        // the state this recovers.
        repair_missing_trailing_newline(&path)?;
        let cwd = std::env::current_dir()
            .map(|dir| dir.to_string_lossy().into_owned())
            .unwrap_or_else(|_| ".".to_owned());
        let mut manager = Self::new(
            &cwd,
            path.parent().unwrap_or(Path::new(".")),
            None,
            true,
            None,
            None,
        )?;
        // The header comes from the FILE'S first line (a bounded probe), not
        // from the page: a tail page of a huge session does not contain it, and
        // loading a header-less entry list would make `load_entries` fall back
        // to `new_session` — inventing a fresh id/cwd for an existing session.
        // Prepending the real header keeps `header()`, `id()`, `cwd()`, the
        // migration check and the print host's JSON header line correct.
        //
        // INVARIANT (Arc hand-over): the page's entries are adopted by
        // reference-count bump. The old shape deep-cloned every resident entry
        // (`entry.as_ref().clone()`) and then re-wrapped the copies in fresh
        // `Arc`s, so a paged open of a tool-result-heavy window paid a full
        // copy of the transcript it had just parsed.
        let mut entries: Vec<Arc<FileEntry>> = Vec::with_capacity(page.entries().len() + 1);
        match page.file_header() {
            Some(header) => entries.push(Arc::new(FileEntry::Session(header))),
            None => {
                anyhow::bail!(
                    "Cannot open paged: session file has no header: {}",
                    path.display()
                );
            }
        }
        entries.extend(page.entries().iter().cloned());
        // ARM the paged state and the file path BEFORE loading: the load runs
        // the migration check, and a v1/v2 session wants to REWRITE the file
        // (which would drop the unread prefix). Armed first, that rewrite hits
        // the guard and the open fails loudly; armed later it would silently
        // migrate in memory while the file kept its old version.
        manager.session_file = Some(path.clone());
        manager.paged = Some(PagedWindow {
            page_start: page.page_start(),
            // The repair above may have appended one newline byte; the cursor
            // cap is the CURRENT file length (nothing else reads it).
            file_bytes: std::fs::metadata(&path)
                .map(|metadata| metadata.len())
                .unwrap_or_else(|_| page.file_len()),
            parsed_entries: stats.parsed_entries,
        });
        manager.unparsed_raw_lines = page.unparsed_lines().to_vec();
        manager.load_shared_entries(entries, None)?;
        manager.flushed = true;
        // One INFO line per paged open (a rare, meaningful event): the
        // counters that prove only the tail window was parsed —
        // `page_bytes` vs `file_bytes` (the skipped prefix is
        // `skipped_bytes`), and the resident entry count. `RUST_LOG` /
        // `--verbose` surfaces it for the big-session probe.
        tracing::info!(
            path = %path.display(),
            page_bytes = stats.page_bytes,
            skipped_bytes = stats.skipped_bytes,
            file_bytes = stats.file_bytes,
            parsed_entries = stats.parsed_entries,
            parsed_lines = stats.parsed_lines,
            "session opened paged (tail window resident; the prefix stays on disk)"
        );
        Ok(manager)
    }

    /// Whether this manager holds only a tail window (see [`Self::open_paged`]).
    pub fn is_paged(&self) -> bool {
        self.paged.is_some()
    }

    /// The paged-open window, when any.
    pub fn paged_window(&self) -> Option<PagedWindow> {
        self.paged
    }

    /// The entries of the resident tail window, oldest first (only the window
    /// of a paged manager; the full list otherwise).
    pub fn resident_entries(&self) -> &[Arc<FileEntry>] {
        &self.file_entries
    }

    /// Stream the SKIPPED prefix, one entry at a time (bounded memory: a 1 MiB
    /// read buffer plus one parsed line). Entries that fail to parse are
    /// skipped, like the loader skips them into `unparsed_raw`. Returns the
    /// number of entries visited.
    ///
    /// `callback` receives each entry by value so the caller can materialize
    /// its messages, summarize them and drop them; the entry is not retained
    /// here.
    #[cfg(feature = "session")]
    pub fn for_each_prefix_entry<F>(&self, mut callback: F) -> crate::Result<usize>
    where
        F: FnMut(FileEntry),
    {
        let Some(paged) = self.paged.as_ref() else {
            return Ok(0);
        };
        let Some(path) = self.session_file.as_ref() else {
            return Ok(0);
        };
        use std::io::Read;
        let mut file = std::fs::File::open(path)?;
        let mut buffer = vec![0u8; SESSION_READ_BUFFER_SIZE];
        let mut pending: Vec<u8> = Vec::new();
        let mut header_version: Option<u32> = None;
        let mut visited = 0usize;
        let mut consumed = 0u64;
        while consumed < paged.page_start {
            let read = file.read(&mut buffer)?;
            if read == 0 {
                break;
            }
            let limit = ((paged.page_start - consumed) as usize).min(read);
            pending.extend_from_slice(&buffer[..limit]);
            consumed += limit as u64;
            let mut line_start = 0usize;
            while let Some(relative) = pending[line_start..].iter().position(|byte| *byte == b'\n')
            {
                let end = line_start + relative;
                // A TERMINATED over-long line must fail like an unterminated
                // one (the check after the drain): the cap bounds a single
                // line's allocation, and a 32 MiB+ line closed by a newline
                // would otherwise be parsed and dropped without tripping it.
                if end - line_start > MAX_PREFIX_ENTRY_LINE_BYTES {
                    anyhow::bail!(
                        "session {} holds an over-long entry line (>{MAX_PREFIX_ENTRY_LINE_BYTES} bytes) before the resident window",
                        path.display()
                    );
                }
                let text = String::from_utf8_lossy(&pending[line_start..end]);
                if let Some(entry) = parse_entry_line_with_version(&text, header_version) {
                    if header_version.is_none() {
                        if let Some(header) = entry.as_header() {
                            header_version = Some(header.version.unwrap_or(1));
                        }
                    }
                    visited += 1;
                    callback(entry);
                }
                line_start = end + 1;
            }
            pending.drain(..line_start);
            if pending.len() > MAX_PREFIX_ENTRY_LINE_BYTES {
                anyhow::bail!(
                    "session {} holds an over-long entry line (>{MAX_PREFIX_ENTRY_LINE_BYTES} bytes) before the resident window",
                    path.display()
                );
            }
        }
        Ok(visited)
    }

    /// Byte cursor of the oldest RESIDENT entry (the paged window's start):
    /// the point a "load more" step walks back from. `None` when the session is
    /// not paged (nothing older on disk) or holds no prefix.
    pub fn prefix_cursor(&self) -> Option<u64> {
        self.paged
            .as_ref()
            .map(|paged| paged.page_start)
            .filter(|cursor| *cursor > 0)
    }

    /// Parse the NEXT OLDER chunk of the skipped prefix, newest-first chunking:
    /// the newest `max_bytes` / `max_entries` of `[0, cursor)`. Bounded by the
    /// same caps as [`SessionTailPage::open`] and — like it — never a
    /// whole-prefix scan; the returned page's
    /// [`SessionTailPage::page_start`] is the cursor for the next step.
    ///
    /// `None` for a non-paged session or when the cursor is already the file
    /// start.
    #[cfg(feature = "session")]
    pub fn read_older_page(
        &self,
        cursor: u64,
        max_bytes: usize,
        max_entries: usize,
    ) -> crate::Result<Option<tail_page::SessionTailPage>> {
        let Some(paged) = self.paged.as_ref() else {
            return Ok(None);
        };
        if cursor == 0 {
            return Ok(None);
        }
        let Some(path) = self.session_file.as_ref() else {
            return Ok(None);
        };
        let limit = cursor.min(paged.file_bytes);
        tail_page::SessionTailPage::open_window(path, Some(limit), max_bytes, max_entries)
    }

    /// Materialize the skipped prefix into the resident entry list (the FULL
    /// view) and clear the paged flag. This is the documented escape hatch for
    /// a consumer that genuinely needs the whole transcript in memory; it costs
    /// the whole prefix (the reason paged mode exists).
    #[cfg(feature = "session")]
    pub fn widen_to_full(&mut self) -> crate::Result<()> {
        if let Some(stall) = self.widen_stall.take() {
            std::thread::sleep(stall);
        }
        if self.paged.is_none() {
            return Ok(());
        }
        let Some(path) = self.session_file.clone() else {
            self.paged = None;
            return Ok(());
        };
        let mut loaded = load_entries_from_file(&path)?;
        self.unparsed_raw_lines = std::mem::take(&mut loaded.unparsed_lines);
        let entries = loaded.into_entries();
        self.load_entries(entries, None)?;
        self.flushed = true;
        self.paged = None;
        Ok(())
    }

    #[cfg(feature = "session")]
    pub fn open(path: impl AsRef<Path>) -> crate::Result<Self> {
        Self::open_with(path, None, None)
    }

    /// Open a specific session file with the upstream
    /// `SessionManager.open(path, sessionDir, cwdOverride)` overrides:
    /// `session_dir` replaces the branch/fork target directory (default: the
    /// file's parent), `cwd_override` replaces the header cwd (default: the
    /// header cwd, else the process cwd). The SDK extension bridge
    /// (`agent_session::ext_session_ops`) exposes this face.
    #[cfg(feature = "session")]
    pub fn open_with(
        path: impl AsRef<Path>,
        session_dir: Option<&Path>,
        cwd_override: Option<&str>,
    ) -> crate::Result<Self> {
        let path = path.as_ref();
        let dir = session_dir
            .map(Path::to_path_buf)
            .or_else(|| path.parent().map(Path::to_path_buf))
            .unwrap_or_else(|| PathBuf::from("."));
        let cwd = match cwd_override {
            Some(cwd) => resolve_path_str(cwd),
            None => read_session_header_bounded(path)
                .ok()
                .flatten()
                .map(|header| header.cwd)
                .unwrap_or_else(current_cwd_string),
        };
        let mut manager = Self::new(&cwd, &dir, Some(path.to_path_buf()), true, None, None)?;
        // `load_entries` adopts the header cwd during construction; an explicit
        // override must win for the rest of this manager's lifetime (the TUI
        // "continue in current cwd" path relies on it).
        if let Some(cwd) = cwd_override {
            manager.cwd = resolve_path_str(cwd);
        }
        Ok(manager)
    }

    /// Open the most recent session in `session_dir` whose header `cwd`
    /// matches; create a new one when there is none (upstream
    /// `continueRecent`).
    #[cfg(feature = "session")]
    pub fn continue_recent(session_dir: impl AsRef<Path>, cwd: &str) -> crate::Result<Self> {
        let session_dir = session_dir.as_ref();
        // Upstream: a custom session dir filters by header cwd (it may hold
        // sessions for several projects); the default dir is cwd-encoded, so
        // no filter is needed.
        let is_default = resolve_path_str(&session_dir.to_string_lossy())
            == resolve_path_str(&default_session_dir_path(cwd).to_string_lossy());
        let recent = if is_default {
            find_most_recent_session(session_dir, None)
        } else {
            find_most_recent_session(session_dir, Some(cwd))
        };
        match recent {
            Some(path) => Self::open(path),
            None => Self::create(cwd, session_dir),
        }
    }

    /// Fork a source session file into `session_dir` under the *target* cwd
    /// (upstream `SessionManager.forkFrom`: new id, `parentSession` pointing
    /// at the source path, full non-header history copied verbatim).
    #[cfg(feature = "session")]
    pub fn fork_from_source(
        source_path: &Path,
        target_cwd: &str,
        session_dir: &Path,
        new_id: Option<&str>,
    ) -> crate::Result<Self> {
        if let Some(id) = new_id {
            assert_valid_session_id(id)?;
        }
        let loaded = load_entries_from_file(source_path)?;
        // Emptiness probe over the parsed lines: no entry is cloned (the fork
        // still needs `loaded.lines`/`parsed` aligned below).
        if loaded.parsed.iter().flatten().next().is_none() {
            anyhow::bail!(
                "Cannot fork: source session file is empty or invalid: {}",
                source_path.display()
            );
        }
        if !loaded.header_valid {
            anyhow::bail!(
                "Cannot fork: source session has no header: {}",
                source_path.display()
            );
        }
        std::fs::create_dir_all(session_dir)?;
        let id = new_id.map(str::to_owned).unwrap_or_else(new_session_id);
        let timestamp = iso_from_ms(now_ms());
        let file_timestamp = timestamp.replace([':', '.'], "-");
        let destination = session_dir.join(format!("{file_timestamp}_{id}.jsonl"));
        let mut header = SessionHeader::new(id.clone(), resolve_path_str(target_cwd), timestamp);
        header.parent_session = Some(source_path.to_string_lossy().into_owned());
        let mut content = serde_json::to_string(&FileEntry::Session(header))?;
        content.push('\n');
        for (raw, parsed) in loaded.unparsed_raw.iter().zip(&loaded.parsed) {
            match parsed {
                Some(entry) if !entry.is_header() => {
                    content.push_str(&serde_json::to_string(entry)?);
                    content.push('\n');
                }
                // Undeserializable lines are carried verbatim so the fork
                // never loses source data (upstream drops them with the
                // parsed-entries copy; pir preserves the raw text). `raw` is
                // Some exactly for those lines (blank lines stay None).
                None => {
                    if let Some(line) = raw {
                        content.push_str(line);
                        content.push('\n');
                    }
                }
                _ => {}
            }
        }
        // Atomic fork write: a crash mid-copy must never leave a truncated
        // destination that later looks like a real session (and would be
        // listed/opened as one). The temp is same-directory and exclusive;
        // fsync precedes the rename, and the parent directory is synced after
        // it on Unix. A failed write/rename leaves the previous destination
        // (if any) intact and only removes this attempt's temp.
        let (mut file, tmp_path) = create_new_rewrite_temp(&destination)?;
        let write_result: crate::Result<()> = (|| {
            file.write_all(content.as_bytes())?;
            file.flush()?;
            file.sync_all()?;
            Ok(())
        })();
        drop(file);
        let write_result = write_result.and_then(|()| {
            std::fs::rename(&tmp_path, &destination)?;
            sync_parent_dir(&destination);
            Ok(())
        });
        if write_result.is_err() {
            let _ = std::fs::remove_file(&tmp_path);
        }
        write_result?;
        Self::open(destination)
    }

    /// Create an in-memory session (no file persistence), optionally from
    /// entries (upstream `inMemory`).
    pub fn in_memory(cwd: &str, options: Option<NewSessionOptions>) -> crate::Result<Self> {
        Self::new(cwd, Path::new(""), None, false, options, None)
    }

    // ------------------------------------------------------------------
    // Discovery
    // ------------------------------------------------------------------

    /// List the sessions in `session_dir`, most recently modified first.
    ///
    /// When `cwd` is `Some`, only sessions whose header cwd resolves to the
    /// same path are returned. Unreadable or non-upstream-format files are
    /// skipped (best-effort, like upstream `SessionManager.list`). Beyond the
    /// header, a bounded peek populates `name`, `first_message`,
    /// `all_messages_text`, `message_count`, and `parent_session_path`
    /// (upstream `buildSessionInfo`).
    #[cfg(feature = "session")]
    pub fn list_sessions(session_dir: &Path, cwd: Option<&str>) -> Vec<SessionSummary> {
        Self::list_sessions_cancellable(session_dir, cwd, None, None).sessions
    }

    /// [`Self::list_sessions`] with progress reporting and a cancel signal
    /// (upstream `listSessions({ onProgress, signal })`,
    /// session-manager.ts:912-973).
    ///
    /// `progress` is invoked once per candidate file with
    /// `(loaded, total, partial)`; `partial` is `Some` on the upstream
    /// publishes — the first file, every [`SESSION_LIST_PUBLISH_INTERVAL`],
    /// and the last — and holds the sessions loaded so far sorted by
    /// activity. `cancel` is polled before each file; when it returns `true`
    /// the scan stops and [`SessionListOutcome::cancelled`] is set, with the
    /// partial list (sorted) still returned. The old two-argument
    /// [`Self::list_sessions`] stays the no-progress, non-cancellable form.
    ///
    /// (pir's listing is synchronous and reads files in sequence; the
    /// upstream concurrency bounds have no Rust counterpart here, so
    /// `loaded`/`total` are exact file counts rather than in-flight tasks.)
    #[cfg(feature = "session")]
    pub fn list_sessions_cancellable(
        session_dir: &Path,
        cwd: Option<&str>,
        mut progress: Option<&mut SessionListProgress<'_>>,
        cancel: Option<&dyn Fn() -> bool>,
    ) -> SessionListOutcome {
        let is_cancelled = || cancel.is_some_and(|cancel| cancel());
        let resolved_cwd = cwd.map(resolve_path_str);
        let Ok(entries) = std::fs::read_dir(session_dir) else {
            return SessionListOutcome::default();
        };
        // Candidates first so `total` is known up front (upstream readdir +
        // `.jsonl` filter before the info loads start). Name-descending order
        // matches upstream's pre-sort; the final order is by modified time.
        let mut candidates: Vec<(PathBuf, SystemTime)> = entries
            .flatten()
            .filter(|entry| {
                entry.path().extension().and_then(|value| value.to_str()) == Some("jsonl")
            })
            .map(|entry| {
                let modified = entry
                    .metadata()
                    .and_then(|meta| meta.modified())
                    .unwrap_or(SystemTime::UNIX_EPOCH);
                (entry.path(), modified)
            })
            .collect();
        candidates.sort_by(|a, b| b.0.cmp(&a.0));
        let total = candidates.len();
        let mut sessions: Vec<SessionSummary> = Vec::new();
        let mut cancelled = false;
        for (index, (path, modified)) in candidates.into_iter().enumerate() {
            if is_cancelled() {
                cancelled = true;
                break;
            }
            // One open per file: header, cwd filter, and peek share a reader.
            if let Some((header, peek)) = read_session_listing(&path, resolved_cwd.as_deref()) {
                sessions.push(SessionSummary {
                    path,
                    id: header.id,
                    cwd: header.cwd,
                    modified,
                    name: peek.name,
                    first_message: peek.first_message,
                    all_messages_text: peek.all_messages_text,
                    message_count: peek.message_count,
                    parent_session_path: header.parent_session,
                });
            }
            let loaded = index + 1;
            // Every file is reported; the partial list only on the upstream
            // publish points (and only when there is something to publish).
            let publish_partial =
                loaded == 1 || loaded % SESSION_LIST_PUBLISH_INTERVAL == 0 || loaded == total;
            if let Some(progress) = progress.as_deref_mut() {
                if publish_partial {
                    // Sort the live list in place instead of deep-cloning every
                    // session summary so far (`path` + `all_messages_text`
                    // strings) for each periodic publish: a directory of N
                    // sessions paid O(N^2/interval) summary copies during one
                    // listing. The final order below is the same sort.
                    sessions.sort_by_key(|session| std::cmp::Reverse(session.modified));
                    progress(loaded, total, Some(&sessions));
                } else {
                    progress(loaded, total, None);
                }
            }
        }
        sessions.sort_by_key(|session| std::cmp::Reverse(session.modified));
        SessionListOutcome {
            sessions,
            cancelled,
        }
    }

    /// Header-only listing of the sessions in `session_dir`, most recently
    /// modified first.
    ///
    /// This is the discovery seam for consumers that need only identity and
    /// display metadata (the server's session registry merging on-disk
    /// sessions): it reads the bounded header line plus the latest
    /// `session_info` name and the file's mtime, and never peeks message
    /// lines. [`Self::list_sessions`] keeps its full `SessionSummary` shape
    /// (first message, message count, selector-search corpus) unchanged.
    ///
    /// When `cwd` is `Some`, only sessions whose header cwd resolves to the
    /// same path are returned. Unreadable or non-upstream-format files are
    /// skipped (best-effort, like upstream `SessionManager.list`).
    #[cfg(feature = "session")]
    pub fn list_session_headers(
        session_dir: &Path,
        cwd: Option<&str>,
    ) -> Vec<SessionHeaderSummary> {
        if let Some(stall) = take_session_header_listing_stall(session_dir) {
            std::thread::sleep(stall);
        }
        let resolved_cwd = cwd.map(resolve_path_str);
        let Ok(entries) = std::fs::read_dir(session_dir) else {
            return Vec::new();
        };
        let mut candidates: Vec<(PathBuf, SystemTime)> = entries
            .flatten()
            .filter(|entry| {
                entry.path().extension().and_then(|value| value.to_str()) == Some("jsonl")
            })
            .map(|entry| {
                let modified = entry
                    .metadata()
                    .and_then(|meta| meta.modified())
                    .unwrap_or(SystemTime::UNIX_EPOCH);
                (entry.path(), modified)
            })
            .collect();
        candidates.sort_by(|a, b| b.0.cmp(&a.0));
        let mut sessions: Vec<SessionHeaderSummary> = Vec::new();
        for (path, modified) in candidates {
            if let Some((header, name)) =
                read_session_header_listing(&path, resolved_cwd.as_deref())
            {
                sessions.push(SessionHeaderSummary {
                    path,
                    id: header.id,
                    cwd: header.cwd,
                    modified,
                    name,
                });
            }
        }
        sessions.sort_by_key(|session| std::cmp::Reverse(session.modified));
        sessions
    }

    /// Disk usage of one session directory: the number of session files and
    /// their total size in bytes. Uses `read_dir` + `metadata` only — file
    /// CONTENTS are never opened, so this is safe on a directory of
    /// transcript-sized sessions (a usage reporter must not pay a full scan).
    pub fn disk_usage(session_dir: &Path) -> SessionDiskUsage {
        let mut usage = SessionDiskUsage::default();
        let Ok(entries) = std::fs::read_dir(session_dir) else {
            return usage;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|value| value.to_str()) != Some("jsonl") {
                continue;
            }
            // `DirEntry::metadata` does not follow symlinks on all platforms;
            // `fs::metadata` measures the linked file, which is what a usage
            // figure should report. Either way no file content is read.
            let Ok(metadata) = std::fs::metadata(&path) else {
                continue;
            };
            if !metadata.is_file() {
                continue;
            }
            usage.sessions += 1;
            usage.bytes = usage.bytes.saturating_add(metadata.len());
        }
        usage
    }

    /// Filter an already-computed listing by header cwd, without re-reading
    /// any file. The `/resume` flow used to call [`Self::list_sessions`]
    /// twice (once filtered, once unfiltered); one unfiltered scan followed by
    /// this in-memory filter is the single-pass equivalent.
    pub fn filter_sessions_by_cwd(sessions: &[SessionSummary], cwd: &str) -> Vec<SessionSummary> {
        let resolved_cwd = resolve_path_str(cwd);
        sessions
            .iter()
            .filter(|session| {
                !session.cwd.is_empty() && resolve_path_str(&session.cwd) == resolved_cwd
            })
            .cloned()
            .collect()
    }

    /// Find a session file by exact header id (upstream
    /// `SessionManager.findById`; best-effort like [`Self::list_sessions`]).
    #[cfg(feature = "session")]
    pub fn find_session_by_id(session_dir: &Path, id: &str) -> Option<PathBuf> {
        if !session_dir.exists() {
            return None;
        }
        let entries = std::fs::read_dir(session_dir).ok()?;
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|value| value.to_str()) != Some("jsonl") {
                continue;
            }
            if read_session_header_for_discovery(&path).is_some_and(|header| header.id == id) {
                return Some(path);
            }
        }
        None
    }

    // ------------------------------------------------------------------
    // `session`-feature-off twins
    //
    // Same names and signatures as the persistence versions above, with the
    // documented in-memory degradation: every constructor still yields a
    // USABLE manager (so a caller can build an agent and run it), but nothing
    // is written to disk and no session file is ever claimed.
    // ------------------------------------------------------------------

    /// `session` off: paged reads need the `memmap2` tail reader, so this
    /// reports the typed disabled error instead of pretending to have a page.
    #[cfg(not(feature = "session"))]
    pub fn open_paged(
        path: impl AsRef<Path>,
        max_bytes: usize,
        max_entries: usize,
    ) -> crate::Result<Self> {
        let _ = (path, max_bytes, max_entries);
        Err(anyhow::anyhow!(SESSION_PERSISTENCE_DISABLED))
    }

    /// `session` off: nothing was paged, so there is no prefix to stream.
    #[cfg(not(feature = "session"))]
    pub fn for_each_prefix_entry<F>(&self, mut callback: F) -> crate::Result<usize>
    where
        F: FnMut(FileEntry),
    {
        let _ = &mut callback;
        Ok(0)
    }

    /// `session` off: no paged window, hence no older page on disk. The
    /// returned type is the [`SessionTailPage`] stub so the signature matches
    /// the persistence build exactly; the value is always `None`.
    #[cfg(not(feature = "session"))]
    pub fn read_older_page(
        &self,
        cursor: u64,
        max_bytes: usize,
        max_entries: usize,
    ) -> crate::Result<Option<SessionTailPage>> {
        let _ = (cursor, max_bytes, max_entries);
        Ok(None)
    }

    /// `session` off: a non-paged session is already "full", so widening is a
    /// no-op that succeeds (never an error: nothing was truncated).
    #[cfg(not(feature = "session"))]
    pub fn widen_to_full(&mut self) -> crate::Result<()> {
        // Keep the test-seam field "read" in this build (no paged window can
        // exist without the JSONL reader).
        let _ = self.widen_stall.take();
        Ok(())
    }

    /// `session` off: opening a session FILE requires the JSONL reader.
    #[cfg(not(feature = "session"))]
    pub fn open(path: impl AsRef<Path>) -> crate::Result<Self> {
        let _ = path;
        Err(anyhow::anyhow!(SESSION_PERSISTENCE_DISABLED))
    }

    /// `session` off: see [`Self::open`].
    #[cfg(not(feature = "session"))]
    pub fn open_with(
        path: impl AsRef<Path>,
        session_dir: Option<&Path>,
        cwd_override: Option<&str>,
    ) -> crate::Result<Self> {
        let _ = (path, session_dir, cwd_override);
        Err(anyhow::anyhow!(SESSION_PERSISTENCE_DISABLED))
    }

    /// `session` off: there is nothing on disk to continue, so this starts a
    /// fresh in-memory session (the same result upstream's `continueRecent`
    /// gives on an empty session directory).
    #[cfg(not(feature = "session"))]
    pub fn continue_recent(session_dir: impl AsRef<Path>, cwd: &str) -> crate::Result<Self> {
        tracing::warn!("{SESSION_PERSISTENCE_DISABLED}");
        Self::new(cwd, session_dir.as_ref(), None, true, None, None)
    }

    /// `session` off: forking reads and rewrites session files, so it needs the
    /// JSONL reader/writer.
    #[cfg(not(feature = "session"))]
    pub fn fork_from_source(
        source_path: &Path,
        target_cwd: &str,
        session_dir: &Path,
        new_id: Option<&str>,
    ) -> crate::Result<Self> {
        let _ = (source_path, target_cwd, session_dir, new_id);
        Err(anyhow::anyhow!(SESSION_PERSISTENCE_DISABLED))
    }

    /// `session` off: no listed sessions (one warning explains why).
    #[cfg(not(feature = "session"))]
    pub fn list_sessions(session_dir: &Path, cwd: Option<&str>) -> Vec<SessionSummary> {
        let _ = session_dir;
        let _ = cwd;
        tracing::warn!("{SESSION_PERSISTENCE_DISABLED}");
        Vec::new()
    }

    /// `session` off: there is no JSONL reader, so the header-only listing is
    /// empty (same degradation as [`Self::list_sessions`]).
    #[cfg(not(feature = "session"))]
    pub fn list_session_headers(
        session_dir: &Path,
        cwd: Option<&str>,
    ) -> Vec<SessionHeaderSummary> {
        let _ = session_dir;
        let _ = cwd;
        tracing::warn!("{SESSION_PERSISTENCE_DISABLED}");
        Vec::new()
    }

    /// `session` off: the cancellable twin of [`Self::list_sessions`] reports
    /// the same disabled listing (no sessions, not cancelled).
    #[cfg(not(feature = "session"))]
    pub fn list_sessions_cancellable(
        session_dir: &Path,
        cwd: Option<&str>,
        progress: Option<&mut SessionListProgress<'_>>,
        cancel: Option<&dyn Fn() -> bool>,
    ) -> SessionListOutcome {
        let _ = (progress, cancel);
        SessionListOutcome {
            sessions: Self::list_sessions(session_dir, cwd),
            cancelled: false,
        }
    }

    /// `session` off: no session file can be found.
    #[cfg(not(feature = "session"))]
    pub fn find_session_by_id(session_dir: &Path, id: &str) -> Option<PathBuf> {
        let _ = (session_dir, id);
        tracing::warn!("{SESSION_PERSISTENCE_DISABLED}");
        None
    }

    // ------------------------------------------------------------------
    // Accessors
    // ------------------------------------------------------------------

    /// The planned/actual session file path. The file may not exist yet (the
    /// creation rule defers it to the first user/assistant message).
    pub fn path(&self) -> &Path {
        self.session_file.as_deref().unwrap_or(Path::new(""))
    }

    /// The session file path when persisting (`None` for in-memory sessions).
    pub fn session_file(&self) -> Option<&Path> {
        self.session_file.as_deref()
    }

    pub fn header(&self) -> Option<&SessionHeader> {
        self.file_entries
            .iter()
            .find(|entry| entry.is_header())
            .and_then(|entry| entry.as_header())
    }

    pub fn id(&self) -> &str {
        &self.session_id
    }

    pub fn cwd(&self) -> &str {
        &self.cwd
    }

    /// Upstream `getMissingSessionCwdIssue` (session-cwd.ts:14-33): the stored
    /// session's working directory no longer exists. `None` for in-memory
    /// sessions, a missing header cwd, or an existing directory.
    pub fn missing_session_cwd(&self) -> Option<MissingSessionCwd> {
        let file = self.session_file.as_deref()?;
        let cwd = self.cwd();
        if cwd.is_empty() || Path::new(cwd).exists() {
            return None;
        }
        Some(MissingSessionCwd {
            session_file: file.to_path_buf(),
            session_cwd: cwd.to_owned(),
        })
    }

    pub fn session_dir(&self) -> &Path {
        &self.session_dir
    }

    pub fn parent_session(&self) -> Option<&str> {
        self.header()?.parent_session.as_deref()
    }

    pub fn leaf_id(&self) -> Option<&str> {
        self.leaf_id.as_deref()
    }

    pub fn entry(&self, id: &str) -> Option<&FileEntry> {
        self.by_id.get(id).map(Arc::as_ref)
    }

    /// All non-header entries in file order (upstream `getEntries`, which
    /// filters `type !== "session"` wherever the header sits).
    ///
    /// PAGED managers ([`Self::open_paged`]): this is the RESIDENT TAIL WINDOW
    /// only (header filtered out), not the whole session — check
    /// [`Self::is_paged`]. A whole-session caller must [`Self::widen_to_full`]
    /// first; a streaming caller can walk the skipped prefix with
    /// [`Self::for_each_prefix_entry`], and tree consumers have the
    /// whole-session [`Self::for_each_tree_row`]. The extension
    /// `ctx.sessionManager` snapshot (`agent_session/session_ctx.rs`) and the
    /// SDK session snapshot (`agent_session/ext_session_ops.rs`, used by
    /// `agent_session/session_store.rs`) read through this method (and
    /// [`Self::branch_entries`] / [`Self::tree`] /
    /// [`Self::build_session_projection`]), so on a paged manager those
    /// snapshots carry the window.
    pub fn entries(&self) -> Vec<FileEntry> {
        self.file_entries
            .iter()
            .filter(|entry| !entry.is_header())
            .map(|entry| FileEntry::clone(entry))
            .collect()
    }

    /// Walk from entry to root, returning all entries in path order (upstream
    /// `getBranch`).
    ///
    /// PAGED managers ([`Self::open_paged`]): the walk starts from the
    /// resident window's leaf and stops where the parent falls outside the
    /// window, so the result is the resident SEGMENT of the branch only — its
    /// oldest entry may still carry a `parent_id` naming a skipped-prefix
    /// entry that is absent from the list. Whole-session callers must
    /// [`Self::widen_to_full`] first ([`Self::for_each_prefix_entry`] streams
    /// entries but cannot rebuild the parent links).
    ///
    /// INVARIANT (no whole-session copy): the walk borrows the live
    /// `Arc<FileEntry>` list and only the path members are cloned — the old
    /// shape deep-cloned EVERY session entry into a temporary list (the whole
    /// transcript, tool results included) just to index it and then clone the
    /// path back out. Per-turn cost drops from two full-session copies to one.
    pub fn branch_entries(&self, from_id: Option<&str>) -> crate::Result<Vec<FileEntry>> {
        let path =
            projection::build_session_path(&self.file_entries, from_id.or(self.leaf_id.as_deref()));
        Ok(path.into_iter().cloned().collect())
    }

    // ------------------------------------------------------------------
    // Appends
    // ------------------------------------------------------------------

    /// Find the id of the newest branch entry whose stored message matches
    /// `matches` (upstream `_findPersistedMessageEntryId`).
    ///
    /// Walks the branch path by REFERENCE: the turn-end boundary calls this
    /// once per produced message, so cloning the whole transcript path (as
    /// [`Self::branch_entries`] does) would be a per-turn deep copy of every
    /// message.
    fn find_message_entry_id_where(
        &self,
        matches: impl Fn(&StoredMessage) -> bool,
    ) -> Option<String> {
        let path = projection::build_session_path(&self.file_entries, self.leaf_id.as_deref());
        Self::find_message_entry_id_in(&path, matches)
    }

    /// [`Self::find_message_entry_id_where`] over an ALREADY-built branch path.
    ///
    /// The turn-end boundary resolves one assistant message and every tool
    /// result of the turn; rebuilding the path index (one hash map over the
    /// whole session) per message was O(messages x session) work while holding
    /// the session lock.
    fn find_message_entry_id_in(
        path: &[&FileEntry],
        matches: impl Fn(&StoredMessage) -> bool,
    ) -> Option<String> {
        for entry in path.iter().rev() {
            let FileEntry::Message(message_entry) = *entry else {
                continue;
            };
            if matches(&message_entry.message) && !message_entry.id.is_empty() {
                return Some(message_entry.id.clone());
            }
        }
        None
    }

    /// The entry id of a persisted ASSISTANT message (upstream
    /// `_findPersistedMessageEntryId`, used by the extension `turn_end`
    /// payload's `messageEntryId`).
    ///
    /// Rust values carry no object identity, so the match uses the fields that
    /// identify the produced turn: content, timestamp, and stop reason. The
    /// per-message sink may stamp provider/model for virtual routing, so those
    /// are deliberately excluded. The newest matching branch entry wins, which
    /// is the entry just persisted for the current turn even when an identical
    /// older message exists.
    pub fn find_persisted_assistant_entry_id(
        &self,
        message: &crate::ai::types::AssistantMessage,
    ) -> Option<String> {
        self.find_message_entry_id_where(|stored| match stored {
            StoredMessage::Assistant(stored) => {
                stored.content == message.content
                    && stored.timestamp == message.timestamp
                    && stored.stop_reason == message.stop_reason
            }
            _ => false,
        })
    }

    /// The entry id of a persisted TOOL RESULT message (upstream
    /// `_findPersistedMessageEntryId`, used by the extension `turn_end`
    /// payload's `toolResultEntryIds`). `toolCallId` is unique per call;
    /// timestamp and tool name disambiguate a replay of the same id.
    pub fn find_persisted_tool_result_entry_id(
        &self,
        message: &crate::ai::types::ToolResultMessage,
    ) -> Option<String> {
        self.find_message_entry_id_where(|stored| match stored {
            StoredMessage::ToolResult(stored) => {
                stored.tool_call_id == message.tool_call_id
                    && stored.tool_name == message.tool_name
                    && stored.timestamp == message.timestamp
            }
            _ => false,
        })
    }

    /// The persisted entry ids of a completed turn, resolved over ONE
    /// branch-path walk (the extension `turn_end` payload's `messageEntryId`
    /// and `toolResultEntryIds`).
    ///
    /// Same match rules as [`Self::find_persisted_assistant_entry_id`] /
    /// [`Self::find_persisted_tool_result_entry_id`], but the path index is
    /// built once: the per-message lookups each rebuilt a hash map over the
    /// whole session, so a turn with N tool results paid N+1 full-session
    /// index builds while the caller held the session lock.
    pub(crate) fn find_persisted_turn_entry_ids(
        &self,
        message: &crate::ai::types::AssistantMessage,
        tool_results: &[crate::ai::types::ToolResultMessage],
    ) -> (Option<String>, Vec<String>) {
        let path = projection::build_session_path(&self.file_entries, self.leaf_id.as_deref());
        let message_entry_id = Self::find_message_entry_id_in(&path, |stored| match stored {
            StoredMessage::Assistant(stored) => {
                stored.content == message.content
                    && stored.timestamp == message.timestamp
                    && stored.stop_reason == message.stop_reason
            }
            _ => false,
        });
        let tool_result_entry_ids = tool_results
            .iter()
            .filter_map(|result| {
                Self::find_message_entry_id_in(&path, |stored| match stored {
                    StoredMessage::ToolResult(stored) => {
                        stored.tool_call_id == result.tool_call_id
                            && stored.tool_name == result.tool_name
                            && stored.timestamp == result.timestamp
                    }
                    _ => false,
                })
            })
            .collect();
        (message_entry_id, tool_result_entry_ids)
    }

    /// Append a message as child of the current leaf, then advance the leaf.
    /// Returns the entry id.
    ///
    /// Like upstream `appendMessage` (session-manager.ts:1198-1214),
    /// compaction-summary and branch-summary message payloads are REFUSED as
    /// message entries: those live as top-level `compaction` / `branch_summary`
    /// entries appended via [`Self::append_compaction`] /
    /// [`Self::branch_with_summary`] so they stay easy to find. pir's message
    /// union cannot express those roles, so the guard is a defensive no-op
    /// today, kept for parity with the upstream contract.
    pub fn append_message(&mut self, message: Message) -> crate::Result<String> {
        let stored = StoredMessage::from(message);
        if matches!(stored.role(), "compactionSummary" | "branchSummary") {
            anyhow::bail!(
                "compaction-summary/branch-summary messages must be appended via \
                 append_compaction/branch_with_summary, not as message entries"
            );
        }
        let entry = FileEntry::Message(MessageEntry {
            id: self.generate_id(),
            parent_id: self.leaf_id.clone(),
            timestamp: iso_from_ms(now_ms()),
            message: stored,
        });
        self.append_entry(entry)
    }

    /// Async append whose JSONL write runs on the blocking pool.
    ///
    /// [`Self::append_message`] is synchronous (the coding-agent and extension
    /// hosts call it under their own locks), so this is the async-facing seam
    /// for calls made from a Tokio task. The async mutex is acquired INSIDE
    /// the blocking closure (`blocking_lock`): the async caller never holds the
    /// storage lock while the OS write runs, and a slow disk cannot stall a
    /// runtime worker. Every append still goes through the same mutex, so file
    /// order, leaf order, error propagation, and the per-append rollback
    /// (`persist_entry`) are unchanged.
    pub async fn append_message_off_thread(
        session: &Arc<tokio::sync::Mutex<Self>>,
        message: Message,
    ) -> crate::Result<String> {
        let session = Arc::clone(session);
        tokio::task::spawn_blocking(move || -> crate::Result<String> {
            let mut session = session.blocking_lock();
            session.append_message(message)
        })
        .await
        .map_err(|error| anyhow::anyhow!("session append blocking task failed: {error}"))?
    }

    /// Batch form of [`Self::append_message_off_thread`]: every message is
    /// appended under ONE lock acquisition, in the order given, so a batch
    /// cannot interleave with another task's appends.
    pub async fn append_messages_off_thread(
        session: &Arc<tokio::sync::Mutex<Self>>,
        messages: Vec<Message>,
    ) -> crate::Result<Vec<String>> {
        if messages.is_empty() {
            return Ok(Vec::new());
        }
        let session = Arc::clone(session);
        tokio::task::spawn_blocking(move || -> crate::Result<Vec<String>> {
            let mut session = session.blocking_lock();
            let mut ids = Vec::with_capacity(messages.len());
            for message in messages {
                ids.push(session.append_message(message)?);
            }
            Ok(ids)
        })
        .await
        .map_err(|error| anyhow::anyhow!("session append blocking task failed: {error}"))?
    }

    /// Async form of [`Self::append_compaction_with_usage`].
    ///
    /// The method body builds the current system message from the WHOLE
    /// session projection and the first write of a new file fsyncs
    /// (`persist_entry` → `sync_all`); both are blocking work over a
    /// transcript-sized file. The async mutex is acquired INSIDE the blocking
    /// closure (`blocking_lock`), so the compaction append runs on the
    /// blocking pool and never parks a runtime worker while holding the
    /// session lock.
    #[allow(clippy::too_many_arguments)]
    pub async fn append_compaction_off_thread(
        session: &Arc<tokio::sync::Mutex<Self>>,
        summary: impl Into<String> + Send + 'static,
        first_kept_entry_id: Option<String>,
        tokens_before: u64,
        details: Option<JsonValue>,
        usage: Option<Usage>,
        from_hook: bool,
    ) -> crate::Result<String> {
        let session = Arc::clone(session);
        tokio::task::spawn_blocking(move || -> crate::Result<String> {
            let mut session = session.blocking_lock();
            session.append_compaction_with_usage(
                summary,
                first_kept_entry_id.as_deref(),
                tokens_before,
                details,
                usage,
                from_hook,
            )
        })
        .await
        .map_err(|error| {
            anyhow::anyhow!("session compaction append blocking task failed: {error}")
        })?
    }

    /// Append a `!` command record as an upstream `bashExecution` message
    /// entry (upstream `recordBashResult` → `sessionManager.appendMessage`).
    ///
    /// Unlike [`Self::append_message`] this writes the coding-agent-only
    /// `bashExecution` role directly, so a pi session reads it back verbatim
    /// instead of losing it to `unparsed_raw_lines`.
    #[allow(clippy::too_many_arguments)]
    pub fn append_bash_execution(
        &mut self,
        command: &str,
        output: &str,
        exit_code: Option<i64>,
        cancelled: bool,
        truncated: bool,
        full_output_path: Option<&str>,
        exclude_from_context: bool,
        timestamp: u64,
    ) -> crate::Result<String> {
        let entry = FileEntry::Message(MessageEntry {
            id: self.generate_id(),
            parent_id: self.leaf_id.clone(),
            timestamp: iso_from_ms(timestamp),
            message: StoredMessage::BashExecution(StoredBashExecutionMessage {
                role: crate::session_manager::types::BashExecutionRole::BashExecution,
                command: command.to_owned(),
                output: output.to_owned(),
                exit_code,
                cancelled,
                truncated,
                full_output_path: full_output_path.map(str::to_owned),
                timestamp,
                exclude_from_context: Some(exclude_from_context).filter(|excluded| *excluded),
            }),
        });
        self.append_entry(entry)
    }

    /// Append a thinking level change; returns the entry id.
    pub fn append_thinking_level_change(&mut self, thinking_level: &str) -> crate::Result<String> {
        let entry = FileEntry::ThinkingLevelChange(ThinkingLevelChangeEntry {
            id: self.generate_id(),
            parent_id: self.leaf_id.clone(),
            timestamp: iso_from_ms(now_ms()),
            thinking_level: thinking_level.to_owned(),
        });
        self.append_entry(entry)
    }

    /// Append a model change; returns the entry id.
    pub fn append_model_change(&mut self, provider: &str, model_id: &str) -> crate::Result<String> {
        let entry = FileEntry::ModelChange(ModelChangeEntry {
            id: self.generate_id(),
            parent_id: self.leaf_id.clone(),
            timestamp: iso_from_ms(now_ms()),
            provider: provider.to_owned(),
            model_id: model_id.to_owned(),
        });
        self.append_entry(entry)
    }

    /// Append model-attributed usage that does not participate in LLM context
    /// (upstream `appendUsage`; `kind` includes `"cache_warm"`).
    pub fn append_usage(
        &mut self,
        kind: &str,
        provider: &str,
        model: &str,
        usage: Usage,
        note: Option<&str>,
    ) -> crate::Result<String> {
        let entry = FileEntry::Usage(UsageEntry {
            id: self.generate_id(),
            parent_id: self.leaf_id.clone(),
            timestamp: iso_from_ms(now_ms()),
            kind: kind.to_owned(),
            provider: provider.to_owned(),
            model: model.to_owned(),
            usage,
            note: note.map(str::to_owned),
        });
        self.append_entry(entry)
    }

    /// Append a compaction summary as child of the current leaf, then advance
    /// the leaf (upstream `appendCompaction(summary, firstKeptEntryId,
    /// tokensBefore, details)`); `usage`/`fromHook` default to `None`/`false`.
    /// `first_kept_entry_id: None` means SELF-RETAINING — the compaction
    /// entry's own id is stored, so the retained range is empty and everything
    /// after the entry replays. The current system message of the leaf-path
    /// entries (if any) is stored as the entry's `systemMessage`. Returns the
    /// entry id.
    #[allow(clippy::too_many_arguments)]
    pub fn append_compaction(
        &mut self,
        summary: impl Into<String>,
        first_kept_entry_id: Option<&str>,
        tokens_before: u64,
        details: Option<serde_json::Value>,
    ) -> crate::Result<String> {
        self.append_compaction_with_usage(
            summary,
            first_kept_entry_id,
            tokens_before,
            details,
            None,
            false,
        )
    }

    /// [`Self::append_compaction`] with the upstream `fromHook` and `usage`
    /// parameters (upstream `appendCompaction(..., fromHook, usage)`):
    /// `usage` carries the summarizer call's usage, `from_hook` marks
    /// extension-generated compactions (it gates file-op carry-over in the
    /// next compaction's preparation). A `false` `from_hook` serializes as
    /// the omitted field (pi-generated, upstream wire compatibility).
    pub fn append_compaction_with_usage(
        &mut self,
        summary: impl Into<String>,
        first_kept_entry_id: Option<&str>,
        tokens_before: u64,
        details: Option<serde_json::Value>,
        usage: Option<Usage>,
        from_hook: bool,
    ) -> crate::Result<String> {
        let timestamp = iso_from_ms(now_ms());
        let id = self.generate_id();
        // Walk only the leaf path's SYSTEM contributions: the compaction entry
        // stores the replayed prompt/sections/tool loadout, and the projection
        // materialization that used to provide them cloned the whole
        // conversation at every compaction (see
        // `projection::current_system_message_from_entries`).
        let system_message = projection::current_system_message_from_entries(
            &self.file_entries,
            self.leaf_id.as_deref(),
        )
        .map(|system| StoredSystemMessage {
            timestamp: ms_from_iso(&timestamp).unwrap_or(now_ms()),
            ..system
        });
        let entry = FileEntry::Compaction(CompactionEntry {
            id: id.clone(),
            parent_id: self.leaf_id.clone(),
            timestamp,
            summary: summary.into(),
            first_kept_entry_id: first_kept_entry_id
                .map(str::to_owned)
                .unwrap_or_else(|| id.clone()),
            tokens_before,
            details,
            first_kept_entry_index: None,
            usage,
            from_hook: if from_hook { Some(true) } else { None },
            system_message,
        });
        self.append_entry(entry)
    }

    /// Start a new branch from an earlier entry AND append a branch_summary
    /// entry capturing the abandoned path's context (upstream
    /// `branchWithSummary`). `entry_id: None` resets the leaf like
    /// [`Self::reset_leaf`]. Returns the branch-summary entry id.
    pub fn branch_with_summary(
        &mut self,
        entry_id: Option<&str>,
        summary: impl Into<String>,
        details: Option<serde_json::Value>,
        from_hook: Option<bool>,
        usage: Option<Usage>,
    ) -> crate::Result<String> {
        if let Some(entry_id) = entry_id {
            if !self.by_id.contains_key(entry_id) {
                anyhow::bail!("Entry {entry_id} not found");
            }
        }
        let from_id = self.leaf_id.clone().unwrap_or_else(|| "root".to_owned());
        // Append first: `append_entry` advances the leaf only after the write
        // succeeds, so a failed persistence leaves the prior leaf intact.
        let entry = FileEntry::BranchSummary(BranchSummaryEntry {
            id: self.generate_id(),
            parent_id: entry_id.map(str::to_owned),
            timestamp: iso_from_ms(now_ms()),
            from_id,
            summary: summary.into(),
            details,
            usage,
            from_hook,
        });
        self.append_entry(entry)
    }

    /// Append a custom entry (for extensions); returns the entry id.
    pub fn append_custom_entry(
        &mut self,
        custom_type: &str,
        data: Option<serde_json::Value>,
    ) -> crate::Result<String> {
        let entry = FileEntry::Custom(CustomEntry {
            custom_type: custom_type.to_owned(),
            data,
            id: self.generate_id(),
            parent_id: self.leaf_id.clone(),
            timestamp: iso_from_ms(now_ms()),
        });
        self.append_entry(entry)
    }

    /// Append a session info entry (display name); returns the entry id.
    pub fn append_session_info(&mut self, name: &str) -> crate::Result<String> {
        // Upstream `name.replace(/[\r\n]+/g, " ").trim()` (session-manager.ts:
        // 1305): each RUN of carriage returns / newlines collapses to ONE
        // space (the previous per-character mapping turned an N-character run
        // into N spaces).
        let mut collapsed = String::with_capacity(name.len());
        let mut in_newline_run = false;
        for c in name.chars() {
            if c == '\r' || c == '\n' {
                if !in_newline_run {
                    collapsed.push(' ');
                }
                in_newline_run = true;
            } else {
                collapsed.push(c);
                in_newline_run = false;
            }
        }
        let sanitized = collapsed.trim().to_owned();
        let entry = FileEntry::SessionInfo(SessionInfoEntry {
            id: self.generate_id(),
            parent_id: self.leaf_id.clone(),
            timestamp: iso_from_ms(now_ms()),
            name: Some(sanitized),
        });
        self.append_entry(entry)
    }

    /// Append a custom message entry that participates in LLM context
    /// (upstream `appendCustomMessageEntry`); returns the entry id.
    pub fn append_custom_message_entry(
        &mut self,
        custom_type: &str,
        content: serde_json::Value,
        display: bool,
        details: Option<serde_json::Value>,
    ) -> crate::Result<String> {
        let entry = FileEntry::CustomMessage(CustomMessageEntry {
            custom_type: custom_type.to_owned(),
            content,
            display,
            details,
            id: self.generate_id(),
            parent_id: self.leaf_id.clone(),
            timestamp: iso_from_ms(now_ms()),
        });
        self.append_entry(entry)
    }

    /// Append a branch-local edit to an earlier model-visible entry (upstream
    /// `appendContextEdit`): `replacement: None` omits the target from
    /// context, `{content}` replaces only its content. String replacements on
    /// assistant/toolResult targets are normalized to a text block. Returns
    /// the entry id.
    pub fn append_context_edit(
        &mut self,
        target_id: &str,
        replacement: Option<serde_json::Value>,
    ) -> crate::Result<String> {
        let target = self
            .by_id
            .get(target_id)
            .map(|entry| FileEntry::clone(entry))
            .ok_or_else(|| anyhow::anyhow!("Entry {target_id} not found"))?;
        let branch = self.branch_entries(None)?;
        if !branch.iter().any(|entry| entry.id() == Some(target_id)) {
            anyhow::bail!("Entry {target_id} is not on the active branch");
        }
        let editable_role = match &target {
            FileEntry::Message(inner) => match inner.message.role() {
                "user" | "assistant" | "toolResult" => Some(inner.message.role().to_owned()),
                _ => None,
            },
            FileEntry::CustomMessage(_) => Some("custom".to_owned()),
            _ => None,
        };
        let Some(target_role) = editable_role else {
            anyhow::bail!("Entry {target_id} does not contribute editable model content");
        };
        let content_is_string = replacement.as_ref().is_some_and(|value| value.is_string());
        if replacement.is_some()
            && replacement
                .as_ref()
                .is_some_and(|value| !value.is_string() && !value.is_array())
        {
            anyhow::bail!("Context edit replacement must be null or contain string/array content");
        }
        // Upstream normalizes string replacements to [{type:"text",...}] ONLY
        // for assistant/toolResult targets; user and custom_message targets
        // keep the raw string content ({content: "str"}).
        let normalized = match (&replacement, target_role.as_str()) {
            (Some(value), role)
                if content_is_string && (role == "assistant" || role == "toolResult") =>
            {
                Some(serde_json::json!({
                    "content": [{
                        "type": "text",
                        "text": value.as_str().unwrap_or_default(),
                    }]
                }))
            }
            (Some(value), _) => Some(serde_json::json!({ "content": value })),
            (None, _) => None,
        };
        let replacement = normalized
            .map(serde_json::from_value::<ContextEditReplacement>)
            .transpose()
            .map_err(|error| anyhow::anyhow!("invalid context edit replacement: {error}"))?;
        let entry = FileEntry::ContextEdit(ContextEditEntry {
            id: self.generate_id(),
            parent_id: self.leaf_id.clone(),
            timestamp: iso_from_ms(now_ms()),
            target_id: target_id.to_owned(),
            replacement,
        });
        self.append_entry(entry)
    }

    /// Append a label change on an entry (upstream `appendLabelChange`);
    /// `None` clears the label. An empty string is serialized as
    /// `label: ""` (upstream writes it) and clears the label like any falsy
    /// value. Returns the entry id.
    pub fn set_label(&mut self, target_id: &str, label: Option<&str>) -> crate::Result<String> {
        if !self.by_id.contains_key(target_id) {
            anyhow::bail!("Entry {target_id} not found");
        }
        let entry = FileEntry::Label(LabelEntry {
            id: self.generate_id(),
            parent_id: self.leaf_id.clone(),
            timestamp: iso_from_ms(now_ms()),
            target_id: target_id.to_owned(),
            label: label.map(str::to_owned),
        });
        self.append_entry(entry)
    }

    /// Get the label for an entry, if any.
    ///
    /// PAGED managers ([`Self::open_paged`]): `labels_by_id` is rebuilt by
    /// `build_index` from the resident `file_entries` only, so a label
    /// change stored in the skipped prefix is invisible here (a RESIDENT label
    /// entry can still label a prefix entry, and is returned). Whole-session
    /// label consumers must [`Self::widen_to_full`] first;
    /// [`Self::for_each_tree_row`] resolves prefix-entry labels from its
    /// streamed metadata without widening.
    pub fn get_label(&self, entry_id: &str) -> Option<String> {
        self.labels_by_id.get(entry_id).cloned()
    }

    /// Append a session name change (upstream `appendSessionInfo`) and return
    /// the entry id.
    pub fn set_session_name(&mut self, name: Option<&str>) -> crate::Result<String> {
        match name {
            Some(name) => self.append_session_info(name),
            None => {
                // Explicit clear: a session_info entry with an empty name
                // (upstream `appendSessionInfo("")` serializes `name: ""`).
                let entry = FileEntry::SessionInfo(SessionInfoEntry {
                    id: self.generate_id(),
                    parent_id: self.leaf_id.clone(),
                    timestamp: iso_from_ms(now_ms()),
                    name: Some(String::new()),
                });
                self.append_entry(entry)
            }
        }
    }

    /// The current session name from the latest `session_info` entry, if any
    /// (upstream `getSessionName`; empty names clear the title).
    pub fn session_name(&self) -> Option<String> {
        for entry in self.file_entries.iter().rev() {
            if let FileEntry::SessionInfo(info) = entry.as_ref() {
                return info
                    .name
                    .as_ref()
                    .map(|name| name.trim().to_owned())
                    .filter(|name| !name.is_empty());
            }
        }
        None
    }

    /// Generate a unique 8-hex entry id (upstream `generateId`).
    fn generate_id(&self) -> String {
        for _ in 0..100 {
            let id = new_entry_id();
            if !self.by_id.contains_key(&id) {
                return id;
            }
        }
        // Fallback to a full UUID on (impossible) collisions.
        uuid::Uuid::new_v4().to_string()
    }

    // ------------------------------------------------------------------
    // Branching
    // ------------------------------------------------------------------

    /// Start a new branch from an earlier entry: moves the leaf pointer; the
    /// next append becomes a child of that entry (upstream `branch`).
    pub fn branch(&mut self, branch_from_id: &str) -> crate::Result<()> {
        if !self.by_id.contains_key(branch_from_id) {
            anyhow::bail!("Entry {branch_from_id} not found");
        }
        self.leaf_id = Some(branch_from_id.to_owned());
        Ok(())
    }

    /// Reset the leaf pointer to `None`; the next append creates a new root
    /// (upstream `resetLeaf`).
    pub fn reset_leaf(&mut self) {
        self.leaf_id = None;
    }

    /// Create a new session file containing only the path from root to the
    /// specified leaf (upstream `createBranchedSession`): labels are removed
    /// from the path and re-appended at the end, compaction cut points are
    /// remapped (self-retaining compactions keep their own id), and the new
    /// header points at the previous session via `parentSession`.
    pub fn create_branched_session(&mut self, leaf_id: &str) -> crate::Result<Option<PathBuf>> {
        // A paged manager's `branch_entries` walks the RESIDENT WINDOW only
        // (the window's oldest parent is not in the index), so the "path" it
        // would copy is a truncated transcript, not the session. Refuse before
        // touching `file_entries`/`session_id`/`session_file`: the old bail at
        // the final `rewrite_file` fired AFTER those were replaced (the
        // manager was left pointing at a never-written file), and a branch
        // whose truncated path held no user/assistant message never reached
        // the guard at all and returned `Ok` on the truncated history.
        self.reject_paged_rewrite()?;
        let previous_session_file = self.session_file.clone();
        let path = self.branch_entries(Some(leaf_id))?;
        if path.is_empty() {
            anyhow::bail!("Entry {leaf_id} not found");
        }

        // Filter out label entries; re-chain the retained path and remember
        // the replacements for cut-point remapping.
        let mut path_without_labels: Vec<FileEntry> = Vec::new();
        let mut replacement_by_label_id: BTreeMap<String, String> = BTreeMap::new();
        let mut pending_label_ids: Vec<String> = Vec::new();
        let mut path_parent_id: Option<String> = None;
        for entry in path {
            if let FileEntry::Label(label) = &entry {
                pending_label_ids.push(label.id.clone());
                continue;
            }
            let entry_id = entry.entry_id().to_owned();
            for label_id in pending_label_ids.drain(..) {
                replacement_by_label_id.insert(label_id, entry_id.clone());
            }
            let mut entry = entry;
            if let FileEntry::Compaction(compaction) = &mut entry {
                compaction.parent_id = path_parent_id.clone();
                // Preserve a self-retaining compaction instead of remapping.
                if compaction.first_kept_entry_id != compaction.id {
                    if let Some(replacement) =
                        replacement_by_label_id.get(&compaction.first_kept_entry_id)
                    {
                        compaction.first_kept_entry_id = replacement.clone();
                    }
                }
            } else {
                set_parent_id(&mut entry, path_parent_id.clone());
            }
            path_parent_id = Some(entry_id);
            path_without_labels.push(entry);
        }

        let new_session_id = new_session_id();
        let timestamp = iso_from_ms(now_ms());
        let file_timestamp = timestamp.replace([':', '.'], "-");
        let new_session_file = self
            .session_dir
            .join(format!("{file_timestamp}_{new_session_id}.jsonl"));
        let mut header = SessionHeader::new(new_session_id.clone(), self.cwd.clone(), timestamp);
        header.parent_session = if self.persist {
            previous_session_file.map(|path| path.to_string_lossy().into_owned())
        } else {
            None
        };

        // Collect labels for entries in the path.
        let mut path_entry_ids: std::collections::HashSet<String> = path_without_labels
            .iter()
            .map(|entry| entry.entry_id().to_owned())
            .collect();
        let mut labels_to_write: Vec<(String, String, String)> = Vec::new();
        for (target_id, label) in &self.labels_by_id {
            if path_entry_ids.contains(target_id) {
                let timestamp = self
                    .label_timestamps_by_id
                    .get(target_id)
                    .cloned()
                    .unwrap_or_else(|| iso_from_ms(now_ms()));
                labels_to_write.push((target_id.clone(), label.clone(), timestamp));
            }
        }

        // Label entries re-appended as children of the retained path.
        let mut parent_id = path_without_labels
            .last()
            .map(|entry| entry.entry_id().to_owned());
        let mut label_entries = Vec::new();
        for (target_id, label, label_timestamp) in labels_to_write {
            let id = loop {
                let candidate = new_entry_id();
                if !path_entry_ids.contains(&candidate) {
                    break candidate;
                }
            };
            path_entry_ids.insert(id.clone());
            let entry = FileEntry::Label(LabelEntry {
                id,
                parent_id: parent_id.clone(),
                timestamp: label_timestamp,
                target_id,
                label: Some(label),
            });
            parent_id = Some(entry.entry_id().to_owned());
            label_entries.push(entry);
        }

        let old_session_id = self.session_id.clone();
        let old_session_file = self.session_file.clone();
        let old_file_entries = std::mem::take(&mut self.file_entries);
        let old_append_file = self.append_file.take();
        let old_append_len = self.append_len;
        let old_flushed = self.flushed;
        let old_leaf = self.leaf_id.clone();

        self.file_entries = Vec::with_capacity(1 + path_without_labels.len() + label_entries.len());
        self.file_entries.push(Arc::new(FileEntry::Session(header)));
        self.file_entries
            .extend(path_without_labels.drain(..).map(Arc::new));
        self.file_entries
            .extend(label_entries.drain(..).map(Arc::new));
        self.session_id = new_session_id;
        self.session_file = if self.persist {
            Some(new_session_file.clone())
        } else {
            None
        };
        // The branched file is a different file: never write through the
        // source session's cached handle.
        self.append_file = None;
        self.append_len = 0;
        self.build_index();

        // A branch is immediately addressable by its caller (the TUI opens the
        // returned path), so persist it even when the retained path has no
        // conversation. On any write failure restore every live field before
        // returning: callers must never observe a half-switched manager.
        if self.persist {
            if let Err(error) = self.rewrite_file() {
                self.session_id = old_session_id;
                self.session_file = old_session_file;
                self.file_entries = old_file_entries;
                self.append_file = old_append_file;
                self.append_len = old_append_len;
                self.flushed = old_flushed;
                self.build_index();
                self.leaf_id = old_leaf;
                return Err(error);
            }
            self.flushed = true;
        } else {
            self.flushed = false;
        }
        Ok(if self.persist {
            Some(new_session_file)
        } else {
            None
        })
    }

    // ------------------------------------------------------------------
    // Context
    // ------------------------------------------------------------------

    /// Build the session context (what gets sent to the LLM) from the
    /// canonical projection.
    ///
    /// INVARIANT (borrowed projection): the projection walks the live
    /// `Arc<FileEntry>` list directly. [`Self::entries`] is for callers that
    /// need an owned snapshot; using it here deep-cloned the whole session
    /// (every stored tool result) into a temporary that the projection then
    /// copied again.
    pub fn build_context_entries(&self) -> crate::Result<Vec<Message>> {
        Ok(
            projection::build_session_projection(&self.file_entries, self.leaf_id.as_deref())
                .messages,
        )
    }

    /// [`Self::build_context_entries`] aligned with the session entry ids
    /// each message came from (`Some(id)` for message entries, `None` for
    /// summaries, system replays, and custom messages).
    pub fn build_context_entries_with_ids(&self) -> crate::Result<Vec<(Option<String>, Message)>> {
        let projection = self.build_session_projection()?;
        Ok(projection
            .entries
            .iter()
            .flat_map(|entry| {
                let id = match entry.source_entry.as_ref() {
                    FileEntry::Message(inner) => Some(inner.id.clone()),
                    _ => None,
                };
                entry
                    .messages_in(&projection.messages)
                    .iter()
                    .map(move |message| (id.clone(), message.clone()))
            })
            .collect())
    }

    /// Per-entry projection metadata WITHOUT the messages
    /// ([`projection::build_projection_entry_meta`]): the compaction hooks run
    /// their trigger check and cut-point selection off this plus the live
    /// context's own message slices, so a transcript-sized session never pays
    /// a second copy of the conversation to decide whether to compact.
    pub fn projection_entry_meta(&self) -> Vec<crate::compaction::ProjectionEntryMeta> {
        projection::build_projection_entry_meta(&self.file_entries, self.leaf_id.as_deref())
    }

    /// The text of the last assistant message on the live context path,
    /// computed from the retained entries WITHOUT materializing the
    /// conversation (see [`projection::last_assistant_text_from_entries`]).
    /// The RPC `get_last_assistant_text` / state handlers call this on every
    /// poll; a transcript-sized projection per poll would be pure duplication.
    pub fn last_assistant_text(&self) -> Option<String> {
        projection::last_assistant_text_from_entries(&self.file_entries, self.leaf_id.as_deref())
    }

    /// Borrowed metadata of the leaf path ([`algorithm::PathEntryMeta`]): the
    /// ids and the two entry kinds the projected-context estimate and the
    /// "newest entry is a compaction" short-circuit read. The full
    /// [`Self::branch_entries`] view (a deep copy of every entry on the path)
    /// is only needed when a caller hands the branch to an extension.
    pub fn branch_path_meta(&self, from_id: Option<&str>) -> Vec<crate::compaction::PathEntryMeta> {
        projection::build_session_path(&self.file_entries, from_id.or(self.leaf_id.as_deref()))
            .into_iter()
            .map(|entry| crate::compaction::PathEntryMeta {
                id: entry.id().map(str::to_owned),
                is_compaction: matches!(entry, FileEntry::Compaction(_)),
                is_context_edit: matches!(entry, FileEntry::ContextEdit(_)),
            })
            .collect()
    }

    /// Build the full projection (upstream `buildSessionProjection`).
    ///
    /// "Full" describes the projection over its INPUT, the resident
    /// `file_entries` list. PAGED managers ([`Self::open_paged`]): that list is
    /// the RESIDENT TAIL WINDOW, so the projected messages (and the context /
    /// compaction views built from them) stop at the page start. Check
    /// [`Self::is_paged`]; a whole-session caller must [`Self::widen_to_full`]
    /// first ([`Self::for_each_prefix_entry`] streams entries without parent
    /// links, so it cannot rebuild this projection). The same resident list
    /// backs [`Self::build_context_entries`] /
    /// [`Self::build_context_entries_with_ids`] and the extension
    /// `ctx.sessionManager` snapshot's projection
    /// (`agent_session/session_ctx.rs`).
    ///
    /// Always exactly one `source_entry` clone per retained entry plus the
    /// `messages` aggregate: this used to start from [`Self::entries`]'s
    /// whole-session deep clone and paid two further copies of the retained
    /// range inside the projection.
    pub fn build_session_projection(&self) -> crate::Result<SessionProjection> {
        self.projection_builds.fetch_add(1, Ordering::Relaxed);
        Ok(projection::build_session_projection(
            &self.file_entries,
            self.leaf_id.as_deref(),
        ))
    }

    /// How many full projections this manager has built (test/profiling
    /// probe). A prompt that runs without compaction builds exactly one:
    /// the pre-compaction projection is reused as the run context instead of
    /// being built a second time.
    #[doc(hidden)]
    pub fn projection_build_count(&self) -> u64 {
        self.projection_builds.load(Ordering::Relaxed)
    }

    // ------------------------------------------------------------------
    // Tree
    // ------------------------------------------------------------------

    /// Build the session entry forest (upstream `getTree`): each node carries
    /// its own entry (a defensive copy — the same JSON shape `getEntries`
    /// serializes), the recursively nested child nodes, and the resolved
    /// label plus label-change timestamp. Orphaned entries are returned as
    /// roots; children are ordered oldest-first by entry timestamp.
    ///
    /// PAGED managers ([`Self::open_paged`]): the forest is built from
    /// [`Self::entries`], i.e. the RESIDENT TAIL WINDOW only — a node whose
    /// parent lies in the skipped prefix becomes a ROOT here, because parent
    /// resolution cannot see the prefix. Whole-session tree consumers must
    /// [`Self::widen_to_full`] first, or use [`Self::for_each_tree_row`],
    /// which streams the prefix into rows without widening. The extension
    /// `ctx.sessionManager` snapshot (`agent_session/session_ctx.rs`) and the
    /// SDK session snapshot (`agent_session/ext_session_ops.rs`, used by
    /// `agent_session/session_store.rs`) build their forest here, so on a
    /// paged manager they carry the window.
    ///
    /// DEEP SESSIONS: a linear session's forest is a chain nested one node per
    /// entry, so the RECURSIVE operations over it (`serde_json::to_value` /
    /// `to_vec` of the forest, `serde_json::Value` equality, and the plain
    /// container drop glue) grow with the session length — a ~1400-entry
    /// session overran a 2 MiB thread stack. The forest itself is built
    /// iteratively, its teardown is iterative ([`TreeChildren`]), and every
    /// in-tree consumer serializes through [`TreeNode::to_value_iterative`] /
    /// [`TreeNode::forest_to_json_iterative`]; a consumer that only needs the
    /// entries should prefer [`Self::for_each_tree_row`], which never builds
    /// the nested chain at all.
    pub fn tree(&self) -> Vec<TreeNode> {
        // Build straight off the live shared list: the old shape first called
        // `entries()` (one deep clone of every entry) and then cloned each
        // entry AGAIN into its `TreeNode`, so one `/tree` paid two full
        // transcript copies. A node must own its entry, so exactly one deep
        // clone per node remains (the `Arc` clone that could avoid it would
        // change the public `TreeNode` shape).
        let mut nodes: BTreeMap<String, TreeNode> = BTreeMap::new();
        let mut file_order: Vec<String> = Vec::with_capacity(self.file_entries.len());
        for entry in self.file_entries.iter().filter(|entry| !entry.is_header()) {
            let id = entry.entry_id().to_owned();
            nodes.insert(
                id.clone(),
                TreeNode {
                    id: id.clone(),
                    entry: entry.as_ref().clone(),
                    label: self.labels_by_id.get(entry.entry_id()).cloned(),
                    label_timestamp: self.label_timestamps_by_id.get(entry.entry_id()).cloned(),
                    children: TreeChildren::default(),
                },
            );
            file_order.push(id);
        }
        // Parent resolution (upstream): `parentId === null` or
        // `parentId === id` is a root; a parent that is not a known entry
        // (orphan) is also a root.
        let mut parent_of: BTreeMap<String, Option<String>> = BTreeMap::new();
        let mut children_of: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for entry in self.file_entries.iter().filter(|entry| !entry.is_header()) {
            let id = entry.entry_id().to_owned();
            let parent = entry
                .parent_id()
                .map(str::to_owned)
                .filter(|parent| *parent != id && nodes.contains_key(parent));
            if let Some(parent) = &parent {
                children_of
                    .entry(parent.clone())
                    .or_default()
                    .push(id.clone());
            }
            parent_of.insert(id, parent);
        }
        // Depth per node via a BFS from the roots. Parent cycles (unreachable
        // from any root) keep no depth and are dropped below, exactly like
        // the previous root-collection behavior.
        let mut depth: BTreeMap<String, usize> = BTreeMap::new();
        let mut frontier: Vec<String> = Vec::new();
        for id in &file_order {
            if parent_of[id.as_str()].is_none() {
                frontier.push(id.clone());
            }
        }
        let mut level = 0_usize;
        while !frontier.is_empty() {
            let mut next: Vec<String> = Vec::new();
            for id in &frontier {
                depth.insert(id.clone(), level);
                if let Some(children) = children_of.get(id) {
                    next.extend(children.iter().cloned());
                }
            }
            frontier = next;
            level += 1;
        }
        let timestamp_of = |id: &str| -> u64 {
            self.by_id
                .get(id)
                .map(|entry| ms_from_iso(entry.timestamp()).unwrap_or(0))
                .unwrap_or(0)
        };
        // Deepest-first extraction builds the nesting iteratively (no
        // recursion; upstream's stack-based sort exists for the same reason):
        // when a node is extracted, every descendant has already pushed
        // itself into it, so its child list is complete and can be ordered
        // (oldest first, newest at bottom — a stable sort keeps file order
        // for equal timestamps).
        let mut ids: Vec<String> = file_order;
        ids.sort_by_key(|id| usize::MAX - depth.get(id).copied().unwrap_or(0));
        let mut roots: Vec<TreeNode> = Vec::new();
        for id in ids {
            let Some(mut node) = nodes.remove(&id) else {
                continue;
            };
            node.children.sort_by_key(|child| timestamp_of(&child.id));
            match parent_of.get(&id).and_then(|parent| parent.clone()) {
                Some(parent) => {
                    // The parent is still in the map for every reachable node
                    // (its depth is smaller, so it extracts later); an already
                    // extracted parent means a cycle no root can reach — the
                    // node is dropped with it.
                    if let Some(parent_node) = nodes.get_mut(&parent) {
                        parent_node.children.push(node);
                    }
                }
                None => roots.push(node),
            }
        }
        roots
    }

    /// Visit every entry of the session tree in the DFS pre-order
    /// [`Self::tree`] nests it, WITHOUT building the nested forest and WITHOUT
    /// materializing a paged session's skipped prefix.
    ///
    /// COMPLETENESS (the contract): the scan covers the WHOLE session for both
    /// managers, so a caller can never mistake a paged window for the tree.
    ///
    /// - A non-paged manager ([`Self::open`]) walks its entries.
    /// - A PAGED manager ([`Self::open_paged`]) holds only its tail window in
    ///   `file_entries`, so the scan first STREAMS the skipped prefix from
    ///   disk one entry at a time ([`Self::for_each_prefix_entry`]) and keeps
    ///   only row metadata per entry — id and its parent occurrence, timestamp,
    ///   kind, display text, tool name, and the label changes in file order. No
    ///   transcript entry is retained, the manager stays paged, and the same
    ///   DFS then runs over prefix plus resident window. The rows and the
    ///   [`TreeScanStats`] are therefore identical to the same session opened
    ///   fully, while [`Self::entries`] / [`Self::resident_entries`] keep
    ///   holding the window: [`TreeScanStats::entries`] counts the streamed
    ///   prefix too, so `stats.entries > self.entries().len()` whenever the
    ///   skipped prefix holds any non-header entry — the counter showing the
    ///   prefix was streamed, not materialized.
    ///
    /// Rows come out exactly as an iterative DFS over `tree()` would visit its
    /// nodes: roots in file order, and each node's children ordered
    /// oldest-first by entry timestamp (stable — equal timestamps keep file
    /// order). `parent_id` is the RESOLVED parent (`None` for a root: no
    /// parent, a self-parent, or a parent id that names no entry), matching
    /// `tree()`; entries unreachable from any root (a parent cycle) are never
    /// visited, again like `tree()`. An id that occurs more than once (invalid
    /// data) is visited once, and its row facts come from the LAST occurrence,
    /// like `by_id` / `tree()`.
    ///
    /// This is the deep-session-safe read face: it holds no node chain and
    /// recurses nowhere, so a 100 MB session's tree can be listed without the
    /// per-entry nesting `tree()` materializes. Callers that need the nested
    /// wire shape use [`TreeNode::to_value_iterative`] /
    /// [`TreeNode::forest_to_json_iterative`] instead.
    pub fn for_each_tree_row<F>(&self, mut visit: F) -> crate::Result<TreeScanStats>
    where
        F: FnMut(&TreeRowMeta),
    {
        if self.paged.is_some() {
            return self.for_each_tree_row_paged(&mut visit);
        }
        let occurrences: Vec<(&str, Option<&str>)> = self
            .file_entries
            .iter()
            .filter(|entry| !entry.is_header())
            .map(|entry| (entry.entry_id(), entry.parent_id()))
            .collect();
        let stats = walk_tree_rows(
            &occurrences,
            |id| {
                self.by_id
                    .get(id)
                    .map(|entry| ms_from_iso(entry.timestamp()).unwrap_or(0))
                    .unwrap_or(0)
            },
            |id| {
                self.by_id.get(id).map(|entry| TreeRowFacts {
                    timestamp_ms: ms_from_iso(entry.timestamp()).unwrap_or(0),
                    label: self.labels_by_id.get(id).cloned(),
                    label_timestamp: self.label_timestamps_by_id.get(id).cloned(),
                    kind_hint: tree_row_kind(entry),
                    preview: tree_row_preview(entry),
                    tool_name: tree_row_tool_name(entry),
                })
            },
            &mut visit,
        );
        Ok(stats)
    }

    /// [`Self::for_each_tree_row`] over a PAGED manager (see that method's docs
    /// for the completeness contract and [`TreeScanStats::entries`] for the
    /// counter proving the prefix was streamed): collect row metadata from the
    /// skipped prefix and the resident window in FILE order, then run the
    /// shared DFS — the same walk a non-paged manager runs, over the whole
    /// entry set.
    fn for_each_tree_row_paged<F>(&self, visit: &mut F) -> crate::Result<TreeScanStats>
    where
        F: FnMut(&TreeRowMeta),
    {
        let mut occurrences: Vec<(String, Option<String>)> = Vec::new();
        // Row facts of the LAST occurrence per id win, exactly like `by_id`
        // (and `tree()`'s id-keyed node map).
        let mut facts: HashMap<String, TreeRowFacts> = HashMap::new();
        // Latest label change per target, applied in FILE order exactly like
        // `build_index` (an empty label clears).
        let mut labels: HashMap<String, (String, String)> = HashMap::new();
        {
            let mut collect = |entry: &FileEntry| {
                if entry.is_header() {
                    return;
                }
                if let FileEntry::Label(label) = entry {
                    let target = label.target_id.clone();
                    match label.label.as_deref().filter(|text| !text.is_empty()) {
                        Some(text) => {
                            labels.insert(target, (text.to_owned(), label.timestamp.clone()));
                        }
                        None => {
                            labels.remove(&target);
                        }
                    }
                }
                let id = entry.entry_id().to_owned();
                facts.insert(
                    id.clone(),
                    TreeRowFacts {
                        timestamp_ms: ms_from_iso(entry.timestamp()).unwrap_or(0),
                        // Labels are resolved below, once the whole scan's
                        // label changes are known.
                        label: None,
                        label_timestamp: None,
                        kind_hint: tree_row_kind(entry),
                        preview: tree_row_preview(entry),
                        tool_name: tree_row_tool_name(entry),
                    },
                );
                occurrences.push((id, entry.parent_id().map(str::to_owned)));
            };
            self.for_each_prefix_entry(|entry| collect(&entry))?;
            for entry in &self.file_entries {
                collect(entry.as_ref());
            }
        }
        for (id, row_facts) in facts.iter_mut() {
            if let Some((text, timestamp)) = labels.get(id) {
                row_facts.label = Some(text.clone());
                row_facts.label_timestamp = Some(timestamp.clone());
            }
        }
        Ok(walk_tree_rows(
            &occurrences,
            |id| facts.get(id).map_or(0, |facts| facts.timestamp_ms),
            |id| facts.get(id).cloned(),
            visit,
        ))
    }
}

/// Per-process rewrite sequence, so two rewrites of the same session path
/// never share a temp file (see [`unique_rewrite_path`]).
static REWRITE_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// A same-directory, process-unique temp path for one atomic rewrite. The
/// session path plus pid and a process-local counter keeps concurrent
/// rewriters from interleaving their temp contents; the rename stays atomic
/// because the temp lives on the same volume.
fn unique_rewrite_path(path: &Path) -> PathBuf {
    let sequence = REWRITE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let mut tmp_name = path.as_os_str().to_os_string();
    tmp_name.push(format!(".tmp.{}.{sequence}", std::process::id()));
    PathBuf::from(tmp_name)
}

/// The temp-file sequence the next rewrite will draw (test probe: a test can
/// place a leftover temp on an upcoming name and pin that the rewrite SKIPS it
/// instead of truncating it).
#[doc(hidden)]
pub fn next_rewrite_temp_sequence() -> u64 {
    REWRITE_SEQUENCE.load(Ordering::Relaxed)
}

/// Temp-file names to try before giving up on a rewrite.
///
/// The name is already unique per process, so a collision means a LEFTOVER
/// temp from a crashed (or still-running) writer that happened to use the same
/// pid (a recycled pid), or the previous pid holder crashed leaving debris.
/// A small bound is enough: every retry takes a fresh process-global sequence
/// number, so the names stay unique.
const REWRITE_TEMP_ATTEMPTS: usize = 8;

/// Create this rewrite's temp file EXCLUSIVELY (`create_new`) next to
/// `session_path`, walking to the next unique name while an existing temp
/// blocks it.
///
/// `create_new` is the invariant, not a detail: truncating an existing temp
/// would erase bytes another writer is still filling, and their contents would
/// then be published as one mixed transcript. A blocked name is therefore
/// skipped, never reused; a rewrite that cannot find a free name fails and
/// leaves the live session file intact. Returns the opened file and the name
/// it was created under (the caller renames exactly that name).
///
/// On Windows a temp another process holds open without delete sharing fails
/// with a sharing violation (raw error 32) rather than `AlreadyExists`, so
/// that is treated as "name taken" too; a genuinely unwritable directory
/// surfaces the same error after the attempts are spent.
fn create_new_rewrite_temp(session_path: &Path) -> std::io::Result<(std::fs::File, PathBuf)> {
    let mut last_error = None;
    for _ in 0..REWRITE_TEMP_ATTEMPTS {
        let candidate = unique_rewrite_path(session_path);
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&candidate)
        {
            Ok(file) => return Ok((file, candidate)),
            Err(error) if is_taken_rewrite_temp(&error) => last_error = Some(error),
            Err(error) => return Err(error),
        }
    }
    Err(last_error.unwrap_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            format!(
                "no free rewrite temp name next to {} after {REWRITE_TEMP_ATTEMPTS} attempts",
                session_path.display()
            ),
        )
    }))
}

/// Whether a `create_new` failure means the temp NAME is taken (retry with the
/// next one) rather than the directory being unwritable.
fn is_taken_rewrite_temp(error: &std::io::Error) -> bool {
    if error.kind() == std::io::ErrorKind::AlreadyExists {
        return true;
    }
    // ERROR_SHARING_VIOLATION: the name exists and its owner holds it open.
    cfg!(windows) && error.raw_os_error() == Some(32)
}

/// Make a completed rename durable by syncing the containing directory.
///
/// On Unix the file's own `sync_all` names only its inode; the rename is
/// durable once the DIRECTORY entry is synced. Windows has no directory
/// handle to sync through `std::fs` and `MoveFileExW` is already durable in
/// the metadata journal, so this is a no-op there. Best-effort: the rename
/// already published a complete file, and a failure here only costs
/// crash durability, which no caller can repair.
fn sync_parent_dir(path: &Path) {
    #[cfg(unix)]
    {
        if let Some(dir) = path
            .parent()
            .and_then(|parent| std::fs::File::open(parent).ok())
        {
            let _ = dir.sync_all();
        }
    }
    #[cfg(not(unix))]
    let _ = path;
}

impl Drop for SessionManager {
    fn drop(&mut self) {
        // Session close is the deferred durability boundary: appends were
        // already written through (a fresh open sees them), so this only
        // makes the tail power-loss durable. Best-effort — Drop has no caller
        // to report to, and the fallback is exactly the pre-deferral state.
        if let Some(file) = &self.append_file {
            let _ = file.sync_all();
        }
    }
}

/// One row's source-independent facts: everything [`TreeRowMeta`] carries
/// except the identity and position fields the DFS fills in. Collected while
/// streaming a paged manager's skipped prefix (metadata only — the entry
/// itself is never retained) or derived from a resident entry on demand.
#[derive(Clone)]
struct TreeRowFacts {
    timestamp_ms: u64,
    label: Option<String>,
    label_timestamp: Option<String>,
    kind_hint: &'static str,
    preview: String,
    tool_name: Option<String>,
}

/// The one DFS behind [`SessionManager::for_each_tree_row`], over `(id, raw
/// parent id)` occurrences in FILE order — the shared walk both managers' scans
/// run, so their rows and stats cannot drift apart.
///
/// Parent resolution, sibling order, the duplicate-id rule and the cycle-
/// skipping visited set mirror [`SessionManager::tree`]; `facts_of` supplies
/// the last occurrence's row facts for an id, `timestamp_of` the sibling sort
/// key.
fn walk_tree_rows<I, FT, FF, FV>(
    occurrences: &[(I, Option<I>)],
    timestamp_of: FT,
    mut facts_of: FF,
    mut visit: FV,
) -> TreeScanStats
where
    I: AsRef<str> + std::ops::Deref<Target = str>,
    FT: Fn(&str) -> u64,
    FF: FnMut(&str) -> Option<TreeRowFacts>,
    FV: FnMut(&TreeRowMeta),
{
    let mut stats = TreeScanStats {
        entries: occurrences.len(),
        ..TreeScanStats::default()
    };
    let mut known: std::collections::BTreeSet<&str> = std::collections::BTreeSet::new();
    for (id, _) in occurrences {
        known.insert(id.as_ref());
    }
    // Parent/children resolution mirrors `tree()`: a parent that is the id
    // itself or names no known entry makes the entry a root. Each occurrence
    // contributes its OWN raw parent, so a duplicate id can hang under two
    // parents (the first visit position wins the row, the last wins the facts).
    let mut children_of: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    let mut roots: Vec<&str> = Vec::new();
    for (id, parent) in occurrences {
        let id = id.as_ref();
        let parent = parent
            .as_deref()
            .filter(|parent| *parent != id && known.contains(*parent));
        match parent {
            Some(parent) => children_of.entry(parent).or_default().push(id),
            None => roots.push(id),
        }
    }
    // Children oldest-first (a stable sort keeps file order for equal
    // timestamps), the `tree()` ordering.
    for children in children_of.values_mut() {
        children.sort_by_key(|id| timestamp_of(id));
    }
    // DFS pre-order with an explicit worklist; `depth` rides the frame.
    // `known` doubles as the visited set after resolution: a DUPLICATE entry
    // id (invalid data) would otherwise be reported once per entry, while
    // `tree()`'s id-keyed node map visits it once.
    let mut stack: Vec<(&str, usize, Option<&str>)> =
        roots.iter().rev().map(|id| (*id, 0_usize, None)).collect();
    while let Some((id, depth, parent)) = stack.pop() {
        if !known.remove(id) {
            continue;
        }
        let Some(row_facts) = facts_of(id) else {
            continue;
        };
        let row = TreeRowMeta {
            id: id.to_owned(),
            parent_id: parent.map(str::to_owned),
            depth,
            timestamp_ms: row_facts.timestamp_ms,
            label: row_facts.label,
            label_timestamp: row_facts.label_timestamp,
            kind_hint: row_facts.kind_hint,
            preview: row_facts.preview,
            tool_name: row_facts.tool_name,
        };
        visit(&row);
        stats.rows += 1;
        stats.max_depth = stats.max_depth.max(depth);
        if depth == 0 {
            stats.roots += 1;
        }
        if let Some(children) = children_of.get(id) {
            for child in children.iter().rev() {
                stack.push((child, depth + 1, Some(id)));
            }
        }
    }
    stats
}

/// A node in the session entry forest (upstream `SessionTreeNode`,
/// session-manager.ts:199-207): the entry itself, the child nodes, and the
/// resolved label for the entry plus the timestamp of its latest label
/// change. `entry` serializes exactly like one `get_entries` entry (the same
/// `FileEntry` wire shape); `id` and `label` are kept from the previous flat
/// shape for compatibility.
///
/// The DERIVED `Serialize` recurses one level per nested entry (a long linear
/// session is a chain one node per entry), so in-tree callers serialize
/// through [`TreeNode::to_value_iterative`] /
/// [`TreeNode::forest_to_json_iterative`], which emit the identical wire
/// shape without the recursion.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TreeNode {
    /// The entry at this node (its `id` is the node id).
    pub entry: FileEntry,
    /// Child nodes, ordered oldest-first by entry timestamp.
    pub children: TreeChildren,
    /// Resolved label for this entry, if any.
    pub label: Option<String>,
    /// Timestamp of the latest label change for this entry, if any
    /// (`None` is omitted on the wire, upstream `labelTimestamp?: string`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label_timestamp: Option<String>,
    /// The entry id (compatibility alias of `entry.id`).
    pub id: String,
}

/// The children of one [`TreeNode`], torn down ITERATIVELY.
///
/// A bare `Vec<TreeNode>` gets the compiler's recursive drop glue: dropping a
/// linear session's forest then walks one stack frame per entry, which
/// overran a 2 MiB thread at ~1400 entries. Dismantling here — every node's
/// children are moved onto an explicit worklist before the node itself is
/// dropped — keeps the teardown at constant stack depth no matter how long
/// the session is.
///
/// It derefs to `Vec<TreeNode>` (and iterates like one), so the tree builders
/// and consumers keep their `Vec` spelling; `#[serde(transparent)]` keeps the
/// `TreeNode` JSON byte-identical to the former `Vec` field.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize)]
#[serde(transparent)]
pub struct TreeChildren(Vec<TreeNode>);

impl TreeChildren {
    /// The children as a slice.
    pub fn as_slice(&self) -> &[TreeNode] {
        &self.0
    }

    /// Number of children.
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Whether this node has no children.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl From<Vec<TreeNode>> for TreeChildren {
    fn from(children: Vec<TreeNode>) -> Self {
        Self(children)
    }
}

impl std::ops::Deref for TreeChildren {
    type Target = Vec<TreeNode>;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl std::ops::DerefMut for TreeChildren {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl IntoIterator for TreeChildren {
    type Item = TreeNode;
    type IntoIter = std::vec::IntoIter<TreeNode>;

    fn into_iter(mut self) -> Self::IntoIter {
        // The inner vector moves out; `self` then drops with an empty list
        // (see the `Drop` impl below for why the teardown is explicit).
        std::mem::take(&mut self.0).into_iter()
    }
}

impl<'a> IntoIterator for &'a TreeChildren {
    type Item = &'a TreeNode;
    type IntoIter = std::slice::Iter<'a, TreeNode>;

    fn into_iter(self) -> Self::IntoIter {
        self.0.iter()
    }
}

impl Drop for TreeChildren {
    fn drop(&mut self) {
        // See the type docs: an explicit worklist replaces the recursive drop
        // glue. Each `node` is dropped with EMPTY children, so the drop that
        // follows never descends another level.
        let mut pending: Vec<TreeNode> = std::mem::take(&mut self.0);
        while let Some(mut node) = pending.pop() {
            pending.extend(std::mem::take(&mut node.children.0));
        }
    }
}

impl TreeNode {
    /// Serialize a whole forest to a [`serde_json::Value`], iteratively.
    ///
    /// The result is exactly `serde_json::to_value(forest)`: same keys, same
    /// insertion order (`entry`, `children`, `label` — always present, `null`
    /// when unset — `labelTimestamp` only when set, then `id`), same child
    /// order. The forest is dismantled as it converts, so neither the
    /// conversion nor the teardown of the consumed forest recurses over the
    /// nesting.
    ///
    /// TEARDOWN CONTRACT: the returned value is a chain nested one level per
    /// entry, and `serde_json::Value` has no iterative drop. Whoever ends up
    /// owning it must move it (never `clone()` / `==` / `to_value` /
    /// `to_string` it, all of which recurse per level) and tear it down with
    /// [`crate::session_manager::json_depth::drop_value_iteratively`] rather
    /// than letting the drop glue walk the chain. Callers that only need the
    /// JSON text should use [`Self::forest_to_json_iterative`] and never build
    /// the nested value at all.
    pub fn to_value_iterative(forest: Vec<TreeNode>) -> JsonValue {
        // `Finish` carries the node's own value with its `children` key still
        // holding the empty placeholder; it is replaced in place (retaining
        // its position) once the children are collected.
        enum Work {
            Node(Box<TreeNode>),
            Finish { node: JsonValue, children: usize },
        }
        let mut work: Vec<Work> = forest
            .into_iter()
            .rev()
            .map(|node| Work::Node(Box::new(node)))
            .collect();
        let mut done: Vec<JsonValue> = Vec::new();
        while let Some(item) = work.pop() {
            match item {
                Work::Node(node) => {
                    let mut node = *node;
                    let children = std::mem::take(&mut node.children);
                    let value = node_value_without_children(&node);
                    if children.is_empty() {
                        done.push(value);
                    } else {
                        work.push(Work::Finish {
                            node: value,
                            children: children.len(),
                        });
                        // Reversed so the first child is finished first.
                        for child in children.into_iter().rev() {
                            work.push(Work::Node(Box::new(child)));
                        }
                    }
                }
                Work::Finish { mut node, children } => {
                    let split = done.len().saturating_sub(children);
                    let kids = done.split_off(split);
                    if let Some(object) = node.as_object_mut() {
                        object.insert(String::from("children"), JsonValue::Array(kids));
                    }
                    done.push(node);
                }
            }
        }
        JsonValue::Array(done)
    }

    /// The JSON TEXT of a whole forest, produced iteratively.
    ///
    /// Exactly the bytes `serde_json::to_string(&serde_json::to_value(forest))`
    /// produces (compact form, the same key order). Used by hosts that must
    /// return the tree inside a JSON string (the SDK session bridge): the
    /// `Value` route recurses one serializer frame per nesting level, so a
    /// long linear session overruns the calling thread's stack.
    pub fn forest_to_json_iterative(forest: Vec<TreeNode>) -> String {
        enum Item {
            Node(TreeNode),
            /// The node's `entry` and opening of `children` are written; close
            /// the array, write the remaining fields, close the object.
            Tail(TreeNode),
        }
        // One "have we written a value into this array yet" flag per OPEN
        // array, so commas never need lookahead.
        let mut open_arrays: Vec<bool> = vec![true];
        let mut stack: Vec<Item> = forest.into_iter().rev().map(Item::Node).collect();
        let mut out = String::new();
        out.push('[');
        while let Some(item) = stack.pop() {
            match item {
                Item::Node(mut node) => {
                    if let Some(first) = open_arrays.last_mut() {
                        if *first {
                            *first = false;
                        } else {
                            out.push(',');
                        }
                    }
                    out.push_str("{\"entry\":");
                    out.push_str(&json_text_of(&node.entry));
                    out.push_str(",\"children\":[");
                    open_arrays.push(true);
                    let children = std::mem::take(&mut node.children);
                    stack.push(Item::Tail(node));
                    for child in children.into_iter().rev() {
                        stack.push(Item::Node(child));
                    }
                }
                Item::Tail(node) => {
                    open_arrays.pop();
                    out.push(']');
                    out.push_str(",\"label\":");
                    out.push_str(&json_text_of(&node.label));
                    if let Some(timestamp) = &node.label_timestamp {
                        out.push_str(",\"labelTimestamp\":");
                        out.push_str(&json_text_of(timestamp));
                    }
                    out.push_str(",\"id\":");
                    out.push_str(&json_text_of(&node.id));
                    out.push('}');
                }
            }
        }
        out.push(']');
        out
    }
}

/// One node's value WITHOUT its children: the keys are inserted in the
/// derived serialization order (`entry`, `children` placeholder, `label`,
/// optional `labelTimestamp`, `id`) so replacing the placeholder keeps the
/// insertion order — and therefore the bytes — of `serde_json::to_value`.
fn node_value_without_children(node: &TreeNode) -> JsonValue {
    let mut object = serde_json::Map::new();
    object.insert(String::from("entry"), json_value_of(&node.entry));
    object.insert(String::from("children"), JsonValue::Array(Vec::new()));
    object.insert(String::from("label"), json_value_of(&node.label));
    if let Some(timestamp) = &node.label_timestamp {
        object.insert(String::from("labelTimestamp"), json_value_of(timestamp));
    }
    object.insert(String::from("id"), json_value_of(&node.id));
    JsonValue::Object(object)
}

/// `serde_json` value of one leaf, `null` when it cannot be encoded (the same
/// fallback the tree call sites apply to a failed `to_value`).
fn json_value_of<T: serde::Serialize + ?Sized>(value: &T) -> JsonValue {
    match serde_json::to_value(value) {
        Ok(value) => value,
        Err(_) => JsonValue::Null,
    }
}

/// `serde_json` text of one leaf, `null` when it cannot be encoded (the same
/// fallback [`json_value_of`] applies).
fn json_text_of<T: serde::Serialize + ?Sized>(value: &T) -> String {
    match serde_json::to_string(value) {
        Ok(text) => text,
        Err(_) => String::from("null"),
    }
}

/// One flat row of [`SessionManager::for_each_tree_row`]: the entry identity
/// and the two facts a listing needs (where it sits, how deep), plus the
/// resolved label and the display facts (a bounded text preview, and the tool
/// name of a result).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeRowMeta {
    /// The entry id.
    pub id: String,
    /// The RESOLVED parent (`None` for a root: no parent, a self-parent, or a
    /// parent id that names no entry) — the same resolution `tree()` nests.
    pub parent_id: Option<String>,
    /// Depth from the root (0 for a root).
    pub depth: usize,
    /// Entry timestamp in epoch milliseconds (`0` when unparsable).
    pub timestamp_ms: u64,
    /// Resolved label for this entry, if any.
    pub label: Option<String>,
    /// Timestamp of the latest label change for this entry, if any.
    pub label_timestamp: Option<String>,
    /// The entry kind, refined for messages by their role: `user_message`,
    /// `assistant_message`, `tool_result`, `system_message`,
    /// `developer_message`, `custom`, `custom_message`, `compaction`,
    /// `branch_summary`, `usage`, `label`, `thinking_level_change`,
    /// `model_change`, `session_info`, or `context_edit`.
    pub kind_hint: &'static str,
    /// Flattened text preview: the entry's text blocks CONCATENATED in order
    /// (the flattening the `/tree` picker shows for a message row),
    /// whitespace-normalized and capped at [`TREE_ROW_PREVIEW_CHARS`]
    /// characters; empty when the entry carries no previewable text.
    ///
    /// This is the display text for the text kinds; a `tool_result` row
    /// displays as `[tool_name]` and a compaction/custom row has no text of
    /// its own (the picker falls back to a short id label).
    pub preview: String,
    /// The tool a `tool_result` entry records (`Some("")` for an empty name),
    /// `None` for every other kind — the fact behind the picker's
    /// `[tool_name]` row, which `preview` (the result's text) cannot express.
    pub tool_name: Option<String>,
}

/// The entry set of a [`SessionManager::for_each_tree_row`] scan.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TreeScanStats {
    /// Non-header entries in the SCANNED SET: the manager's own entries for a
    /// non-paged session, and for a PAGED one the resident window PLUS the
    /// skipped prefix's non-header entries streamed from disk — i.e. the same
    /// count a full open reports, never a silently resident-only count. On a
    /// paged manager `resident_entries()` is `[prepended header] + window`, so
    /// the comparison uses the resident NON-HEADER count:
    /// `entries > resident_entries().len() - 1` whenever the skipped prefix
    /// holds any non-header entry (the header is not counted here), so the
    /// counter shows the prefix was streamed rather than materialized.
    pub entries: usize,
    /// Rows the DFS visited (entries reachable from a root).
    pub rows: usize,
    /// Roots the DFS started from.
    pub roots: usize,
    /// Deepest visited depth (0 for a flat forest; also 0 when nothing was
    /// visited).
    pub max_depth: usize,
}

/// Preview cap for [`TreeRowMeta::preview`] (the `/tree` row convention).
pub const TREE_ROW_PREVIEW_CHARS: usize = 120;

/// [`TreeRowMeta::kind_hint`] for one entry.
fn tree_row_kind(entry: &FileEntry) -> &'static str {
    match entry {
        FileEntry::Session(_) => "session",
        FileEntry::Message(message) => match &message.message {
            StoredMessage::User(user) => match user.role {
                crate::ai::types::UserRole::User => "user_message",
                crate::ai::types::UserRole::System => "system_message",
                crate::ai::types::UserRole::Developer => "developer_message",
            },
            StoredMessage::Assistant(_) => "assistant_message",
            StoredMessage::ToolResult(_) => "tool_result",
            StoredMessage::System(_) => "system_message",
            StoredMessage::BashExecution(_) => "bash_execution",
            StoredMessage::Custom(_) => "custom",
        },
        FileEntry::ThinkingLevelChange(_) => "thinking_level_change",
        FileEntry::ModelChange(_) => "model_change",
        FileEntry::Usage(_) => "usage",
        FileEntry::Compaction(_) => "compaction",
        FileEntry::BranchSummary(_) => "branch_summary",
        FileEntry::Custom(_) => "custom",
        FileEntry::CustomMessage(_) => "custom_message",
        FileEntry::ContextEdit(_) => "context_edit",
        FileEntry::Label(_) => "label",
        FileEntry::SessionInfo(_) => "session_info",
    }
}

/// [`TreeRowMeta::preview`] for one entry (bounded, allocation-light). Text
/// blocks are CONCATENATED (no separator), the flattening the `/tree` picker
/// has always applied to a message row, so a row's `preview` reproduces that
/// display byte for byte.
fn tree_row_preview(entry: &FileEntry) -> String {
    use crate::ai::types::ContentBlock;
    let text_of = |content: &[ContentBlock]| -> String {
        content
            .iter()
            .filter_map(|block| match block {
                ContentBlock::Text(text) => Some(text.text.as_str()),
                _ => None,
            })
            .collect::<String>()
    };
    // Content-block arrays of custom messages/custom entries carry the text
    // under a `text` key (the same reduction the context conversion uses).
    let text_of_value = |content: &JsonValue| -> String {
        match content {
            JsonValue::String(text) => text.clone(),
            JsonValue::Array(blocks) => blocks
                .iter()
                .filter_map(|block| block.get("text").and_then(JsonValue::as_str))
                .collect::<Vec<_>>()
                .join(" "),
            _ => String::new(),
        }
    };
    let raw = match entry {
        FileEntry::Message(message) => match &message.message {
            StoredMessage::User(user) => text_of(&user.content),
            StoredMessage::Assistant(assistant) => text_of(&assistant.content),
            StoredMessage::ToolResult(result) => text_of(&result.content),
            StoredMessage::System(system) => system.content.clone(),
            StoredMessage::BashExecution(bash) => bash.output.clone(),
            StoredMessage::Custom(custom) => text_of_value(&custom.content),
        },
        FileEntry::Compaction(compaction) => compaction.summary.clone(),
        FileEntry::BranchSummary(summary) => summary.summary.clone(),
        FileEntry::CustomMessage(custom) => text_of_value(&custom.content),
        _ => String::new(),
    };
    let normalized: String = raw
        .chars()
        .map(|character| match character {
            '\n' | '\t' => ' ',
            other => other,
        })
        .collect();
    normalized
        .trim()
        .chars()
        .take(TREE_ROW_PREVIEW_CHARS)
        .collect()
}

/// [`TreeRowMeta::tool_name`] for one entry: the tool a result came from
/// (`None` for every non-result entry).
fn tree_row_tool_name(entry: &FileEntry) -> Option<String> {
    match entry {
        FileEntry::Message(message) => match &message.message {
            StoredMessage::ToolResult(result) => Some(result.tool_name.clone()),
            _ => None,
        },
        _ => None,
    }
}

fn set_parent_id(entry: &mut FileEntry, parent_id: Option<String>) {
    match entry {
        FileEntry::Message(inner) => inner.parent_id = parent_id,
        FileEntry::ThinkingLevelChange(inner) => inner.parent_id = parent_id,
        FileEntry::ModelChange(inner) => inner.parent_id = parent_id,
        FileEntry::Usage(inner) => inner.parent_id = parent_id,
        FileEntry::BranchSummary(inner) => inner.parent_id = parent_id,
        FileEntry::Custom(inner) => inner.parent_id = parent_id,
        FileEntry::CustomMessage(inner) => inner.parent_id = parent_id,
        FileEntry::ContextEdit(inner) => inner.parent_id = parent_id,
        FileEntry::Label(inner) => inner.parent_id = parent_id,
        FileEntry::SessionInfo(inner) => inner.parent_id = parent_id,
        FileEntry::Session(_) | FileEntry::Compaction(_) => {}
    }
}

impl SessionHeader {
    fn with_parent(mut self, parent_session: Option<String>) -> Self {
        self.parent_session = parent_session;
        self
    }
}
