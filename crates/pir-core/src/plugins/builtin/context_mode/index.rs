//! SQLite/FTS5 knowledge base for the `context-mode` builtin.
//!
//! ## Storage
//!
//! One SQLite database per project at
//! `<agent_dir>/context-mode/<project-hash>.db` (`agent_dir` is the shared
//! agent-dir resolution; the hash is a stable FNV-1a of the canonical cwd).
//! The connection is opened lazily on the first index/search/stats/purge and
//! lives behind one `Mutex`; every database operation runs on
//! `spawn_blocking`, so the async executor never blocks on SQLite. WAL mode +
//! a busy timeout let a second pir process read while this one writes.
//!
//! `chunks` is the metadata table (source, content type, title, line range);
//! `chunks_fts` is a standalone FTS5 table (`porter unicode61`, so "caching"
//! finds "cached"/"caches") whose `rowid` equals `chunks.id`. Inserts/deletes
//! touch both in one transaction, so the pair never drifts.
//!
//! ## Chunking
//!
//! Markdown is split at ATX headings; fenced code blocks are opaque (a `#`
//! line inside a fence never starts a section, and fences are never split
//! across chunks). Sections larger than ~4 KiB are split at blank lines
//! outside fences, keeping the heading title with a `(part N)` suffix. Other
//! content is split at blank lines around ~2 KiB. Every chunk records its
//! 1-based line range.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{anyhow, Context as _};
use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;

// ---------------------------------------------------------------------------
// Budgets
// ---------------------------------------------------------------------------

/// One indexed file's upper bound (also the `ctx_execute_file` read cap).
pub const MAX_FILE_BYTES: u64 = 32 * 1024 * 1024;

/// Current on-disk schema version (`PRAGMA user_version`). Bumped when the
/// table layout changes; `ctx_doctor` reports it and the open path migrates
/// older databases (v1 → v2 adds the session/worktree scope columns and the
/// journal `events` table).
pub const SCHEMA_VERSION: i64 = 2;

// ---------------------------------------------------------------------------
// Per-session retention
// ---------------------------------------------------------------------------

// The knowledge base is an append-only store fed by hooks, so without a bound
// a long-lived project grows until the disk fills. Every limit below is
// enforced per session: one session can never evict another session's rows,
// and a session that stops being used stops contributing new rows.
//
// Ordering is always "newest survives": a journal row is kept by `seq` (insert
// order) and a chunk by `(indexed_at, id)`, so eviction removes the oldest
// material first.

/// Journal retention caps, re-exported from the engine-free crate (the same
/// values the journal shaping in `journal.rs` uses).
pub use pir_builtin_context_mode::{
    SESSION_JOURNAL_MAX_BYTES, SESSION_JOURNAL_MAX_ROWS, SESSION_JOURNAL_TTL_SECS,
};
/// Chunks kept per session.
pub const SESSION_CHUNK_MAX_ROWS: usize = 4_000;
/// Chunk text bytes kept per session (`title + content`).
pub const SESSION_CHUNK_MAX_BYTES: u64 = 64 * 1024 * 1024;
/// Cached fetch bodies kept for the whole project (they are raw pages, not
/// per-session material): oldest `fetched_at` rows are evicted first.
pub const FETCH_CACHE_MAX_ROWS: usize = 256;
/// Cached fetch bytes kept for the whole project.
pub const FETCH_CACHE_MAX_BYTES: u64 = 64 * 1024 * 1024;

/// Upper bound on the number of queries in one `ctx_search` call.
pub const MAX_SEARCH_QUERIES: usize = 32;

/// Upper bound on one query's length (bytes), so a call cannot force the
/// FTS expression builder / SQL statement to process arbitrary payloads.
pub const MAX_QUERY_BYTES: usize = 512;

/// Files larger than this are skipped by the directory walk.
const MAX_INDEX_FILE_BYTES: u64 = MAX_FILE_BYTES;

/// `ctx_resume`'s state-block byte budget (upstream `RESUME_CAP`).
pub const RESUME_CAP: usize = 2_048;

/// Search defaults.
pub const DEFAULT_SEARCH_LIMIT: usize = 3;
pub const MAX_SEARCH_LIMIT: usize = 20;

/// Snippet window around the first matching term.
pub const SNIPPET_CHARS: usize = 500;

/// `ctx_fetch_and_index` cache defaults.
pub const DEFAULT_FETCH_TTL_SECS: u64 = 24 * 60 * 60;

/// Directory-walk bounds for `ctx_index(path: <dir>)`.
const WALK_MAX_DEPTH: usize = 5;
const WALK_MAX_FILES: usize = 200;

/// Breadth bound: at most this many directory entries are read per
/// directory (the rest are ignored). Without it a single huge flat directory
/// forces an unbounded listing + sort before the file cap can stop the walk.
const WALK_MAX_ENTRIES_PER_DIR: usize = 4096;

/// Total-directory bound: a tree of empty directories must not keep the walk
/// running after the file budget was never filled.
const WALK_MAX_DIRECTORIES: usize = 2048;

const WALK_SKIP_DIRS: &[&str] = &[
    "node_modules",
    ".git",
    "dist",
    "build",
    ".next",
    "coverage",
    ".venv",
    "__pycache__",
];

const WALK_EXTENSIONS: &[&str] = &[
    "md", "mdx", "txt", "json", "yaml", "yml", "ts", "tsx", "js", "jsx", "py", "rs", "go", "sh",
];

// ---------------------------------------------------------------------------
// Data shapes
// ---------------------------------------------------------------------------
// `ParsedChunk` / `IndexDoc` and the chunker are pure text shaping owned by the
// engine-free `pir-builtin-context-mode` crate; this module re-exports them.
pub use pir_builtin_context_mode::{chunk_document, IndexDoc, ParsedChunk};

/// Which rows a search may see.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SearchScope {
    Session,
    Worktree,
    Project,
}

impl SearchScope {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Session => "session",
            Self::Worktree => "worktree",
            Self::Project => "project",
        }
    }

    pub fn parse(value: Option<&str>) -> Self {
        match value.map(str::trim) {
            Some("session") => Self::Session,
            Some("project") => Self::Project,
            _ => Self::Worktree,
        }
    }
}

/// Result ordering.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SearchSort {
    Relevance,
    Timeline,
}

impl SearchSort {
    pub fn parse(value: Option<&str>) -> Self {
        match value.map(str::trim) {
            Some("timeline") => Self::Timeline,
            _ => Self::Relevance,
        }
    }
}

/// One scoped search request (upstream `ctx_search` inputs).
pub struct ScopedSearch {
    pub queries: Vec<String>,
    pub source: Option<String>,
    pub content_type: Option<String>,
    pub limit: usize,
    pub offset: usize,
    pub scope: SearchScope,
    pub sort: SearchSort,
    /// Timeline cursor: return rows with `indexed_at > after` (epoch ms).
    pub after: Option<i64>,
    pub session_id: Option<String>,
    pub worktree: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SourceSummary {
    pub source: String,
    pub content_type: String,
    pub chunks: usize,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct IndexReport {
    pub files: usize,
    pub chunks: usize,
    pub sources: Vec<SourceSummary>,
    /// Older chunks removed by this write to keep the session within its
    /// retention caps.
    pub evicted_chunks: usize,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchHit {
    pub id: i64,
    pub title: String,
    pub source: String,
    pub content_type: String,
    pub snippet: String,
    pub line_start: i64,
    pub line_end: i64,
    /// Epoch milliseconds; also the cursor `sort:"timeline", after:<ms>` pages by.
    pub indexed_at: i64,
    pub score: f64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct QueryResult {
    pub query: String,
    pub results: Vec<SearchHit>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchResponse {
    pub queries: Vec<QueryResult>,
}

/// One journal row (upstream `events`).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct JournalEvent {
    pub seq: i64,
    pub session_id: String,
    pub turn: i64,
    pub kind: String,
    pub tool: String,
    pub path: Option<String>,
    pub ok: bool,
    pub exit_code: Option<i64>,
    pub bytes: usize,
    pub body: String,
    pub created_at: i64,
}

/// Usage totals read back from the journal (upstream `ctx_stats` scope rows).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EventUsage {
    pub calls: i64,
    pub bytes: i64,
    pub by_kind: Vec<EventKindUsage>,
}

/// One journal `kind` row inside [`EventUsage`].
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EventKindUsage {
    pub kind: String,
    pub calls: i64,
    pub bytes: i64,
}

/// The active retention limits (surfaced by `ctx_doctor`).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RetentionLimits {
    pub journal_max_rows: usize,
    pub journal_max_bytes: u64,
    pub journal_ttl_secs: u64,
    pub chunk_max_rows: usize,
    pub chunk_max_bytes: u64,
    pub fetch_cache_max_rows: usize,
    pub fetch_cache_max_bytes: u64,
}

impl RetentionLimits {
    /// The compiled-in defaults (every limit is process-constant).
    pub fn defaults() -> Self {
        Self {
            journal_max_rows: SESSION_JOURNAL_MAX_ROWS,
            journal_max_bytes: SESSION_JOURNAL_MAX_BYTES,
            journal_ttl_secs: SESSION_JOURNAL_TTL_SECS,
            chunk_max_rows: SESSION_CHUNK_MAX_ROWS,
            chunk_max_bytes: SESSION_CHUNK_MAX_BYTES,
            fetch_cache_max_rows: FETCH_CACHE_MAX_ROWS,
            fetch_cache_max_bytes: FETCH_CACHE_MAX_BYTES,
        }
    }
}

/// What one session currently stores, and what the last eviction removed.
/// `over_limit` is computed against [`RetentionLimits`], so `ctx_doctor` can
/// report a session that predates the limits before the next write trims it.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RetentionReport {
    pub session_id: String,
    pub journal_rows: usize,
    pub journal_bytes: u64,
    pub journal_oldest_secs: u64,
    pub chunk_rows: usize,
    pub chunk_bytes: u64,
    pub over_limit: bool,
}

/// A journal row to append (shape + builder owned by the engine-free crate).
pub use pir_builtin_context_mode::JournalInput;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DbStats {
    pub chunks: usize,
    pub sources: usize,
    pub source_list: Vec<SourceSummary>,
    pub db_path: String,
    pub db_size_bytes: u64,
}

#[derive(Debug, Clone)]
pub struct CachedFetch {
    pub body: String,
    pub content_type: String,
}

/// Read-only database state reported by `ctx_doctor`. Produced without
/// creating the database file: an absent base reports `exists: false` and
/// zero counters, and FTS5 availability is probed in memory.
#[derive(Debug, Clone)]
pub struct IndexDiagnostics {
    pub path: String,
    pub exists: bool,
    pub open: bool,
    pub size_bytes: u64,
    pub wal_bytes: u64,
    pub fts5_available: bool,
    pub schema_version: i64,
    pub chunks: usize,
    pub sources: usize,
}

// ---------------------------------------------------------------------------
// The store
// ---------------------------------------------------------------------------

/// Lazy handle onto the project's FTS5 database. `with_path` makes tests
/// hermetic; the default resolves the agent-dir path on first use so plugin
/// construction never touches the filesystem or `~/.pi`.
///
/// ## Which project?
///
/// The default path is keyed by the session cwd the host pushes through
/// [`AgentPlugin::on_session_context`](crate::plugins::plugin::AgentPlugin::on_session_context)
/// (via [`Self::set_session_cwd`]), not by the process cwd. Two sessions with
/// different cwds in one process therefore get different databases and
/// cannot purge each other's index. `std::env::current_dir()` remains the
/// fallback for hosts that never push a snapshot.
///
/// The path is memoized on the inputs it was derived from — the effective
/// cwd and the agent-dir base — and re-resolved when EITHER changes (upstream
/// has no equivalent: there each `RemoteExecutionEnv`-like consumer is built
/// once). A long-lived process that changes its agent dir, or a host that
/// pushes the session cwd only after the first DB use, therefore moves to the
/// matching database instead of keeping the first resolution for the process
/// lifetime; the open connection follows the new path (see [`Self::call`]).
pub struct IndexStore {
    explicit_path: Option<PathBuf>,
    /// Base directory override for the default path (hermetic tests: never
    /// write to the real agent dir). `None` = the shared agent dir.
    base_dir: Option<PathBuf>,
    /// Session cwd pushed by the host, preferred over the process cwd.
    session_cwd: Mutex<Option<PathBuf>>,
    /// Session identity written with every chunk (`ctx_search(scope:"session")`).
    scope_session: Mutex<Option<String>>,
    /// Worktree root written with every chunk (`ctx_search(scope:"worktree")`).
    scope_worktree: Mutex<Option<String>>,
    /// Last resolved default path plus the `(cwd, base)` it came from.
    /// Single-entry on purpose: bounded, and the previous path is simply
    /// dropped when the inputs change.
    resolved: Mutex<Option<ResolvedPath>>,
    /// The open connection and the path it was opened on, so a resolved-path
    /// change reopens against the new database instead of silently keeping
    /// the old one.
    connection: Mutex<Option<OpenConnection>>,
    /// Per-session retention limits enforced on every write. Process
    /// constant in production; tests and embedders may lower them through
    /// [`Self::with_retention_limits`].
    limits: RetentionLimits,
}

/// One memoized default-path resolution.
struct ResolvedPath {
    /// The effective cwd (session push or process cwd), canonicalized.
    cwd: PathBuf,
    /// The agent-dir base the path was rooted at.
    base: PathBuf,
    path: PathBuf,
}

struct OpenConnection {
    path: PathBuf,
    connection: Connection,
}

impl IndexStore {
    /// A store at the project's default agent-dir path (resolved lazily).
    pub fn project_default() -> Self {
        Self::default_at(None)
    }

    /// A default-path store whose agent-dir base is overridden (hermetic
    /// tests and embedders that must not touch `~/.pi`).
    pub fn with_base_dir(base: PathBuf) -> Self {
        Self::default_at(Some(base))
    }

    fn default_at(base_dir: Option<PathBuf>) -> Self {
        Self {
            explicit_path: None,
            base_dir,
            session_cwd: Mutex::new(None),
            scope_session: Mutex::new(None),
            scope_worktree: Mutex::new(None),
            resolved: Mutex::new(None),
            connection: Mutex::new(None),
            limits: RetentionLimits::defaults(),
        }
    }

    /// A store at an explicit path (hermetic tests, embedders).
    pub fn with_path(path: PathBuf) -> Self {
        Self {
            explicit_path: Some(path),
            base_dir: None,
            session_cwd: Mutex::new(None),
            scope_session: Mutex::new(None),
            scope_worktree: Mutex::new(None),
            resolved: Mutex::new(None),
            connection: Mutex::new(None),
            limits: RetentionLimits::defaults(),
        }
    }

    /// Override the per-session retention limits (tests exercising eviction
    /// without writing megabytes). Must be called before the first write.
    #[doc(hidden)]
    pub fn with_retention_limits(mut self, limits: RetentionLimits) -> Self {
        self.limits = limits;
        self
    }

    /// The active retention limits.
    pub fn retention_limits(&self) -> &RetentionLimits {
        &self.limits
    }

    /// Record the session identity + worktree written with every chunk. The
    /// plugin calls this from `on_session_context` and from each tool's
    /// working directory, so a doc that carries no explicit scope still lands
    /// in the right visibility bucket.
    pub fn set_scope(&self, session_id: Option<String>, worktree: Option<String>) {
        if let Ok(mut guard) = self.scope_session.lock() {
            if let Some(session_id) = session_id {
                *guard = Some(session_id);
            }
        }
        if let Some(worktree) = worktree {
            *self
                .scope_worktree
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(worktree);
        }
    }

    fn scope_defaults(&self) -> (String, String) {
        let session = self
            .scope_session
            .lock()
            .ok()
            .and_then(|guard| guard.clone())
            .unwrap_or_default();
        let worktree = self
            .scope_worktree
            .lock()
            .ok()
            .and_then(|guard| guard.clone())
            .unwrap_or_default();
        (session, worktree)
    }

    /// Record the host-pushed session cwd (from
    /// `AgentPlugin::on_session_context`). Only the default-path resolution
    /// uses it; an explicit `with_path` is unaffected.
    pub fn set_session_cwd(&self, cwd: PathBuf) {
        let mut guard = match self.session_cwd.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        *guard = Some(cwd);
    }

    /// The database path (resolved without opening the DB).
    pub fn path(&self) -> PathBuf {
        if let Some(path) = &self.explicit_path {
            return path.clone();
        }
        let pushed = self
            .session_cwd
            .lock()
            .map_err(|poisoned| poisoned.into_inner())
            .ok()
            .and_then(|guard| guard.clone());
        let cwd = pushed
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
        let cwd = cwd.canonicalize().unwrap_or(cwd);
        let base = match &self.base_dir {
            Some(base) => base.clone(),
            None => crate::coding_agent::context::agent_dir(),
        };
        let mut resolved = match self.resolved.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        if let Some(entry) = resolved.as_ref() {
            if entry.cwd == cwd && entry.base == base {
                return entry.path.clone();
            }
        }
        let path = default_db_path(&cwd, &base);
        *resolved = Some(ResolvedPath {
            cwd,
            base,
            path: path.clone(),
        });
        path
    }

    /// Close the connection (plugin shutdown): checkpoint the WAL and drop
    /// the handle so SQLite releases the database cleanly. Idempotent and
    /// best effort — a poisoned lock or a failed checkpoint must not fail
    /// teardown. A later [`Self::call`] reopens lazily.
    pub fn close(&self) {
        let connection = match self.connection.lock() {
            Ok(mut guard) => guard.take(),
            Err(poisoned) => poisoned.into_inner().take(),
        };
        if let Some(open) = connection {
            let _ = open
                .connection
                .execute_batch("PRAGMA wal_checkpoint(TRUNCATE);");
        }
    }

    /// Whether a connection is currently open (shutdown test seam).
    pub fn is_open(&self) -> bool {
        match self.connection.lock() {
            Ok(guard) => guard.is_some(),
            Err(poisoned) => poisoned.into_inner().is_some(),
        }
    }

    /// Run `f` against the single connection on the blocking pool. The
    /// connection opens (and the schema initializes) on first use, and
    /// reopens when the resolved path changed since it was opened (agent-dir
    /// or session-cwd change).
    pub async fn call<T, F>(self: &Arc<Self>, f: F) -> anyhow::Result<T>
    where
        T: Send + 'static,
        F: FnOnce(&mut Connection) -> rusqlite::Result<T> + Send + 'static,
    {
        let store = Arc::clone(self);
        tokio::task::spawn_blocking(move || -> anyhow::Result<T> {
            let mut guard = store
                .connection
                .lock()
                .map_err(|_| anyhow!("context-mode index: database lock poisoned"))?;
            // Resolve AFTER taking the connection lock: a session-cwd/agent-dir
            // change that lands while we were waiting must not leave the
            // connection opened on the superseded path (the next `call` would
            // otherwise reopen once more). Lock order is connection ->
            // resolved/session-cwd; `path()` alone never takes the connection
            // lock, so this cannot deadlock.
            let path = store.path();
            let reopen = guard.as_ref().is_none_or(|open| open.path != path);
            if reopen {
                if let Some(open) = guard.take() {
                    let _ = open
                        .connection
                        .execute_batch("PRAGMA wal_checkpoint(TRUNCATE);");
                }
                *guard = Some(OpenConnection {
                    path: path.clone(),
                    connection: Self::open_connection_at(&path)?,
                });
            }
            let open = guard
                .as_mut()
                .ok_or_else(|| anyhow!("context-mode index: connection disappeared"))?;
            f(&mut open.connection).map_err(|error| anyhow!("context-mode index: {error}"))
        })
        .await
        .map_err(|error| anyhow!("context-mode index: blocking task failed: {error}"))?
    }

    /// Open (and initialize) the database at `path`. The path is passed in
    /// rather than re-resolved so the connection always matches the path the
    /// caller resolved and recorded.
    fn open_connection_at(path: &Path) -> anyhow::Result<Connection> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create index directory {}", parent.display()))?;
        }
        let connection = Connection::open(path)
            .with_context(|| format!("open context-mode index {}", path.display()))?;
        // WAL: a reader never blocks the writer; synchronous=NORMAL is the
        // standard WAL durability/latency trade-off for a rebuildable index.
        connection.execute_batch(
            "PRAGMA journal_mode = WAL;\
             PRAGMA synchronous = NORMAL;\
             PRAGMA busy_timeout = 5000;",
        )?;
        init_schema(&connection)?;
        Ok(connection)
    }

    /// Insert documents, replacing any previous chunks with the same source
    /// (re-indexing a source updates it instead of duplicating it).
    ///
    /// Retention is enforced per write: each affected session is trimmed to
    /// its chunk row/byte caps inside the same transaction, so the store can
    /// never grow without bound. `report.evicted_chunks` says how many older
    /// chunks had to go.
    pub async fn index_documents(
        self: &Arc<Self>,
        docs: Vec<IndexDoc>,
    ) -> anyhow::Result<IndexReport> {
        let (default_session, default_worktree) = self.scope_defaults();
        let limits = self.limits.clone();
        self.call(move |connection| {
            let transaction = connection.transaction()?;
            let mut report = IndexReport {
                files: docs.len(),
                chunks: 0,
                sources: Vec::new(),
                evicted_chunks: 0,
            };
            let mut sessions: Vec<String> = Vec::new();
            for doc in &docs {
                let inserted =
                    replace_source(&transaction, doc, &default_session, &default_worktree)?;
                report.chunks += inserted;
                // The chunk row's session column is the doc's own id when it
                // has one, else the store's current scope. An empty id is its
                // own bucket: unscoped chunks are trimmed too, so a host that
                // never pushes a session id cannot grow the store unbounded.
                let session = if doc.session_id.is_empty() {
                    default_session.as_str()
                } else {
                    doc.session_id.as_str()
                };
                if !sessions.iter().any(|known| known == session) {
                    sessions.push(session.to_owned());
                }
                if let Some(existing) = report
                    .sources
                    .iter_mut()
                    .find(|summary| summary.source == doc.source)
                {
                    existing.chunks += inserted;
                } else {
                    report.sources.push(SourceSummary {
                        source: doc.source.clone(),
                        content_type: doc.content_type.clone(),
                        chunks: inserted,
                    });
                }
            }
            for session in &sessions {
                report.evicted_chunks += trim_session_chunks(&transaction, session, &limits)?;
            }
            trim_fetch_cache(&transaction, &limits)?;
            transaction.commit()?;
            Ok(report)
        })
        .await
    }

    /// Run all queries in one call. `Ok(None)` means the knowledge base is
    /// empty (the caller turns that into the "index first" error).
    pub async fn search(
        self: &Arc<Self>,
        queries: Vec<String>,
        source: Option<String>,
        content_type: Option<String>,
        limit: usize,
    ) -> anyhow::Result<Option<SearchResponse>> {
        let limit = limit.clamp(1, MAX_SEARCH_LIMIT) as i64;
        self.call(move |connection| {
            // Existence check, not COUNT(*): the search path must never scan
            // the chunks table; one index probe answers "is the base empty?".
            let has_any: Option<i64> = connection
                .query_row("SELECT 1 FROM chunks LIMIT 1", [], |row| row.get(0))
                .optional()?;
            if has_any.is_none() {
                return Ok(None);
            }
            let mut statement = connection.prepare_cached(SEARCH_SQL)?;
            let mut response = SearchResponse {
                queries: Vec::with_capacity(queries.len()),
            };
            // Chunk ids are deduplicated across every query in this call, so
            // one section never occupies two result slots.
            let mut seen: HashSet<i64> = HashSet::new();
            for query in &queries {
                let Some((expression, terms, non_ascii)) = build_match_expression(query) else {
                    response.queries.push(QueryResult {
                        query: query.clone(),
                        results: Vec::new(),
                    });
                    continue;
                };
                let mut rows = statement.query(params![
                    expression,
                    source.as_deref(),
                    content_type.as_deref(),
                    limit
                ])?;
                let mut results = Vec::new();
                while let Some(row) = rows.next()? {
                    let id: i64 = row.get(0)?;
                    if !seen.insert(id) {
                        continue;
                    }
                    let content: String = row.get(6)?;
                    let indexed_at: i64 = row.get(7)?;
                    let rank: f64 = row.get(8)?;
                    let (line_start, line_end): (i64, i64) = (row.get(4)?, row.get(5)?);
                    results.push(SearchHit {
                        id,
                        title: row.get(1)?,
                        source: row.get(2)?,
                        content_type: row.get(3)?,
                        snippet: make_snippet(&content, &terms, SNIPPET_CHARS),
                        line_start,
                        line_end,
                        indexed_at,
                        // FTS5 bm25() is smaller-is-better and negative for
                        // matches; flip it so higher is better for the model.
                        score: -rank,
                    });
                }
                // CJK and other non-ASCII queries often cannot match the FTS
                // token stream at all (`unicode61` folds contiguous CJK runs
                // into one token), so an empty FTS result is not evidence of
                // absence. Fall back to a bounded LIKE scan; tokens that DO
                // match FTS keep their bm25 ranking untouched.
                if results.is_empty() && non_ascii {
                    for mut hit in like_fallback_search(
                        connection,
                        &terms,
                        source.as_deref(),
                        content_type.as_deref(),
                        limit,
                    )? {
                        if !seen.insert(hit.id) {
                            continue;
                        }
                        // Fetch the content for snippet computation; the
                        // fallback scan keeps it out of the row projection to
                        // avoid dragging every candidate's body through the
                        // ranking pass.
                        let content: Option<String> = connection
                            .query_row(
                                "SELECT content FROM chunks WHERE id = ?1",
                                params![hit.id],
                                |row| row.get(0),
                            )
                            .optional()?;
                        if let Some(content) = content.as_deref() {
                            fill_snippet(&mut hit, content, &terms);
                        }
                        results.push(hit);
                    }
                }
                response.queries.push(QueryResult {
                    query: query.clone(),
                    results,
                });
            }
            Ok(Some(response))
        })
        .await
    }

    /// Scoped search (upstream `ctx_search`): `session`/`worktree`/`project`
    /// visibility, `offset` paging, `timeline` ordering and the project-scope
    /// BM25 + recency reciprocal-rank fusion.
    pub async fn search_scoped(
        self: &Arc<Self>,
        request: ScopedSearch,
    ) -> anyhow::Result<Option<SearchResponse>> {
        let limit = request.limit.clamp(1, MAX_SEARCH_LIMIT);
        self.call(move |connection| {
            let has_any: Option<i64> = connection
                .query_row("SELECT 1 FROM chunks LIMIT 1", [], |row| row.get(0))
                .optional()?;
            if has_any.is_none() {
                return Ok(None);
            }
            let scope = request.scope.as_str();
            let session = request.session_id.as_deref().unwrap_or("");
            let worktree = request.worktree.as_deref().unwrap_or("");

            let mut response = SearchResponse {
                queries: Vec::with_capacity(request.queries.len()),
            };
            let mut seen: HashSet<i64> = HashSet::new();

            if request.sort == SearchSort::Timeline {
                // One time-ordered page for the whole call; queries only shape
                // the snippet window.
                let terms = request
                    .queries
                    .first()
                    .and_then(|query| build_match_expression(query))
                    .map(|(_, terms, _)| terms)
                    .unwrap_or_default();
                let mut hits =
                    timeline_page(connection, &request, scope, session, worktree, &terms)?;
                let first = request.queries.first().cloned().unwrap_or_default();
                response.queries.push(QueryResult {
                    query: first,
                    results: std::mem::take(&mut hits),
                });
                for query in request.queries.iter().skip(1) {
                    response.queries.push(QueryResult {
                        query: query.clone(),
                        results: Vec::new(),
                    });
                }
                return Ok(Some(response));
            }

            let fused = request.scope == SearchScope::Project;
            for query in &request.queries {
                let Some((expression, terms, non_ascii)) = build_match_expression(query) else {
                    response.queries.push(QueryResult {
                        query: query.clone(),
                        results: Vec::new(),
                    });
                    continue;
                };
                let mut results = if fused {
                    rrf_project(
                        connection,
                        &request,
                        scope,
                        session,
                        worktree,
                        &expression,
                        &terms,
                    )?
                } else {
                    scoped_relevance(
                        connection,
                        &request,
                        scope,
                        session,
                        worktree,
                        &expression,
                        &terms,
                        limit,
                    )?
                };
                if results.is_empty() && non_ascii {
                    // CJK/non-ASCII terms often cannot match the FTS token
                    // stream; fall back to a bounded LIKE scan in the same
                    // scope (the legacy path's behaviour).
                    for mut hit in like_fallback_search_scoped(
                        connection, &terms, &request, scope, session, worktree, limit,
                    )? {
                        if let Some(content) = connection
                            .query_row(
                                "SELECT content FROM chunks WHERE id = ?1",
                                params![hit.id],
                                |row| row.get::<_, String>(0),
                            )
                            .optional()?
                        {
                            fill_snippet(&mut hit, &content, &terms);
                        }
                        results.push(hit);
                    }
                }
                results.retain(|hit| seen.insert(hit.id));
                response.queries.push(QueryResult {
                    query: query.clone(),
                    results,
                });
            }
            Ok(Some(response))
        })
        .await
    }

    /// Append one journal row (upstream `events`): the raw material
    /// `ctx_resume` renders after a compaction.
    ///
    /// The row is written inside a transaction that then enforces this
    /// session's retention limits (TTL, row cap, byte cap), so a session can
    /// never grow the database without bound. Evicted rows are reported
    /// through [`Self::retention`] and `ctx_doctor`.
    pub async fn append_event(self: &Arc<Self>, event: JournalInput) -> anyhow::Result<i64> {
        let limits = self.limits.clone();
        self.call(move |connection| {
            let transaction = connection.transaction()?;
            transaction.execute(
                "INSERT INTO events \
                 (session_id, turn, kind, tool, path, key, ok, exit_code, bytes, body, created_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
                params![
                    event.session_id,
                    event.turn,
                    event.kind,
                    event.tool,
                    event.path,
                    event.key,
                    i64::from(event.ok),
                    event.exit_code,
                    event.bytes as i64,
                    event.body,
                    crate::ai::util::now_ms() as i64,
                ],
            )?;
            let seq = transaction.last_insert_rowid();
            trim_session_journal(&transaction, &event.session_id, &limits)?;
            transaction.commit()?;
            Ok(seq)
        })
        .await
    }

    /// This session's current retention state (plus the active limits).
    /// Read-only and non-creating: an absent database reports zero usage and
    /// is never created (same contract as [`Self::diagnostics`]).
    pub async fn retention(self: &Arc<Self>, session: String) -> anyhow::Result<RetentionReport> {
        let path = self.path();
        if !std::fs::metadata(&path)
            .map(|meta| meta.is_file())
            .unwrap_or(false)
        {
            return Ok(RetentionReport {
                session_id: session,
                journal_rows: 0,
                journal_bytes: 0,
                journal_oldest_secs: 0,
                chunk_rows: 0,
                chunk_bytes: 0,
                over_limit: false,
            });
        }
        let limits = self.limits.clone();
        self.call(move |connection| read_retention(connection, &session, &limits))
            .await
    }

    /// This session's journal rows in sequence order (optionally only the last
    /// `limit`; `None` = all).
    pub async fn session_events(
        self: &Arc<Self>,
        session: String,
        limit: Option<usize>,
    ) -> anyhow::Result<Vec<JournalEvent>> {
        self.call(move |connection| {
            let sql = if limit.is_some() {
                "SELECT seq, session_id, turn, kind, tool, path, ok, exit_code, bytes, body, created_at \
                 FROM events WHERE session_id = ?1 ORDER BY seq DESC LIMIT ?2"
            } else {
                "SELECT seq, session_id, turn, kind, tool, path, ok, exit_code, bytes, body, created_at \
                 FROM events WHERE session_id = ?1 ORDER BY seq ASC"
            };
            let mut statement = connection.prepare(sql)?;
            let mut events = Vec::new();
            let mut rows = match limit {
                Some(limit) => statement.query(params![session, limit as i64])?,
                None => statement.query(params![session])?,
            };
            while let Some(row) = rows.next()? {
                events.push(JournalEvent {
                    seq: row.get(0)?,
                    session_id: row.get(1)?,
                    turn: row.get(2)?,
                    kind: row.get(3)?,
                    tool: row.get(4)?,
                    path: row.get(5)?,
                    ok: row.get::<_, i64>(6)? != 0,
                    exit_code: row.get(7)?,
                    bytes: row.get::<_, i64>(8)?.max(0) as usize,
                    body: row.get(9)?,
                    created_at: row.get(10)?,
                });
            }
            // A tail read comes back newest-first; callers want sequence order.
            if limit.is_some() {
                events.reverse();
            }
            Ok(events)
        })
        .await
    }

    /// Journal totals for one session, or the whole project when `session`
    /// is `None` (upstream `ctx_stats` session/project/all-time rows).
    pub async fn event_usage(
        self: &Arc<Self>,
        session: Option<String>,
    ) -> anyhow::Result<EventUsage> {
        self.call(move |connection| {
            let (totals_sql, kinds_sql) = if session.is_some() {
                (
                    "SELECT COUNT(*), COALESCE(SUM(bytes), 0) FROM events WHERE session_id = ?1",
                    "SELECT kind, COUNT(*), COALESCE(SUM(bytes), 0) FROM events \
                     WHERE session_id = ?1 GROUP BY kind ORDER BY 3 DESC, 1",
                )
            } else {
                (
                    "SELECT COUNT(*), COALESCE(SUM(bytes), 0) FROM events",
                    "SELECT kind, COUNT(*), COALESCE(SUM(bytes), 0) FROM events \
                     GROUP BY kind ORDER BY 3 DESC, 1",
                )
            };
            let (calls, bytes) = match session.as_deref() {
                Some(session) => connection.query_row(totals_sql, [session], |row| {
                    Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?))
                })?,
                None => connection.query_row(totals_sql, [], |row| {
                    Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?))
                })?,
            };
            let mut statement = connection.prepare(kinds_sql)?;
            let mut rows = match session.as_deref() {
                Some(session) => statement.query([session])?,
                None => statement.query([])?,
            };
            let mut by_kind = Vec::new();
            while let Some(row) = rows.next()? {
                by_kind.push(EventKindUsage {
                    kind: row.get(0)?,
                    calls: row.get(1)?,
                    bytes: row.get(2)?,
                });
            }
            Ok(EventUsage {
                calls,
                bytes,
                by_kind,
            })
        })
        .await
    }

    /// One journal row by sequence (nil when absent).
    pub async fn event_by_seq(self: &Arc<Self>, seq: i64) -> anyhow::Result<Option<JournalEvent>> {
        self.call(move |connection| {
            connection
                .query_row(
                    "SELECT seq, session_id, turn, kind, tool, path, ok, exit_code, bytes, body, created_at \
                     FROM events WHERE seq = ?1",
                    params![seq],
                    |row| {
                        Ok(JournalEvent {
                            seq: row.get(0)?,
                            session_id: row.get(1)?,
                            turn: row.get(2)?,
                            kind: row.get(3)?,
                            tool: row.get(4)?,
                            path: row.get(5)?,
                            ok: row.get::<_, i64>(6)? != 0,
                            exit_code: row.get(7)?,
                            bytes: row.get::<_, i64>(8)?.max(0) as usize,
                            body: row.get(9)?,
                            created_at: row.get(10)?,
                        })
                    },
                )
                .optional()
        })
        .await
    }

    /// Knowledge-base counters + per-source chunk counts.
    pub async fn stats(self: &Arc<Self>) -> anyhow::Result<DbStats> {
        let (chunks, source_list) = self
            .call(|connection| {
                let chunks: i64 =
                    connection.query_row("SELECT COUNT(*) FROM chunks", [], |row| row.get(0))?;
                let mut statement = connection.prepare_cached(
                    "SELECT source, content_type, COUNT(*) FROM chunks \
                     GROUP BY source, content_type ORDER BY source, content_type",
                )?;
                let mut rows = statement.query([])?;
                let mut list = Vec::new();
                while let Some(row) = rows.next()? {
                    let count: i64 = row.get(2)?;
                    list.push(SourceSummary {
                        source: row.get(0)?,
                        content_type: row.get(1)?,
                        chunks: count.max(0) as usize,
                    });
                }
                Ok((chunks.max(0) as usize, list))
            })
            .await?;
        let db_path = self.path();
        let db_size_bytes = std::fs::metadata(&db_path)
            .map(|meta| meta.len())
            .unwrap_or(0);
        Ok(DbStats {
            chunks,
            sources: source_list.len(),
            source_list,
            db_path: db_path.to_string_lossy().into_owned(),
            db_size_bytes,
        })
    }

    /// Read-only diagnostics for `ctx_doctor`: path, on-disk size (database
    /// and WAL), FTS5 availability, schema version and chunk/source counts.
    ///
    /// An absent database is NOT created: the method reports
    /// `exists: false` with zero counters (FTS5 availability still comes from
    /// an in-memory probe). An existing database is opened through the normal
    /// lazy path (schema init is idempotent); no chunk data is modified.
    pub async fn diagnostics(self: &Arc<Self>) -> anyhow::Result<IndexDiagnostics> {
        let path = self.path();
        let file_metadata = std::fs::metadata(&path).ok().filter(|meta| meta.is_file());
        let exists = file_metadata.is_some();
        let size_bytes = file_metadata.as_ref().map(|meta| meta.len()).unwrap_or(0);
        let wal_path = PathBuf::from(format!("{}-wal", path.to_string_lossy()));
        let wal_bytes = std::fs::metadata(&wal_path)
            .map(|meta| meta.len())
            .unwrap_or(0);

        if !exists {
            // No file permission/creation: probe FTS5 in memory so the report
            // says whether this build could ever serve the knowledge base.
            let fts5_available = tokio::task::spawn_blocking(|| {
                Connection::open_in_memory()
                    .map(|connection| fts5_available(&connection))
                    .unwrap_or(false)
            })
            .await
            .unwrap_or(false);
            return Ok(IndexDiagnostics {
                path: path.to_string_lossy().into_owned(),
                exists: false,
                open: false,
                size_bytes: 0,
                wal_bytes: 0,
                fts5_available,
                schema_version: 0,
                chunks: 0,
                sources: 0,
            });
        }

        let (fts5_available, schema_version, chunks, sources) =
            self.call(|connection| read_diagnostics(connection)).await?;
        Ok(IndexDiagnostics {
            path: path.to_string_lossy().into_owned(),
            exists,
            open: self.is_open(),
            size_bytes,
            wal_bytes,
            fts5_available,
            schema_version,
            chunks,
            sources,
        })
    }

    /// Delete every journal row of `session` (upstream `ctx_purge({session})`)
    /// and return the number of events removed. Chunks written by the session
    /// stay addressable through `source`, matching upstream's split between
    /// the event journal and the content store.
    pub async fn purge_session(self: &Arc<Self>, session: String) -> anyhow::Result<usize> {
        self.call(move |connection| {
            let removed = connection.execute(
                "DELETE FROM events WHERE session_id = ?1",
                [session.as_str()],
            )?;
            Ok(removed)
        })
        .await
    }

    /// Delete the whole project knowledge base: chunks, FTS rows, sources,
    /// fetch cache and the event journal. Returns `(chunks, events)`.
    pub async fn purge_project(self: &Arc<Self>) -> anyhow::Result<(usize, usize)> {
        self.call(move |connection| {
            let transaction = connection.transaction()?;
            let chunks: i64 =
                transaction.query_row("SELECT COUNT(*) FROM chunks", [], |row| row.get(0))?;
            let events: i64 =
                transaction.query_row("SELECT COUNT(*) FROM events", [], |row| row.get(0))?;
            transaction.execute("DELETE FROM chunks_fts", [])?;
            transaction.execute("DELETE FROM chunks", [])?;
            transaction.execute("DELETE FROM fetch_cache", [])?;
            transaction.execute("DELETE FROM events", [])?;
            transaction.commit()?;
            Ok((chunks.max(0) as usize, events.max(0) as usize))
        })
        .await
    }

    /// Delete every chunk (or every chunk of `source`) and return the row
    /// count that was removed. The fetch cache is cleared too, so a later
    /// re-fetch actually re-reads the URL.
    pub async fn purge(self: &Arc<Self>, source: Option<String>) -> anyhow::Result<usize> {
        self.call(move |connection| {
            let transaction = connection.transaction()?;
            let deleted: i64 = match source.as_deref() {
                Some(source) => transaction.query_row(
                    "SELECT COUNT(*) FROM chunks WHERE source = ?1",
                    [source],
                    |row| row.get(0),
                )?,
                None => {
                    transaction.query_row("SELECT COUNT(*) FROM chunks", [], |row| row.get(0))?
                }
            };
            match source.as_deref() {
                Some(source) => {
                    transaction.execute(
                        "DELETE FROM chunks_fts WHERE rowid IN \
                         (SELECT id FROM chunks WHERE source = ?1)",
                        [source],
                    )?;
                    transaction.execute("DELETE FROM chunks WHERE source = ?1", [source])?;
                    transaction.execute("DELETE FROM fetch_cache WHERE source = ?1", [source])?;
                }
                None => {
                    transaction.execute("DELETE FROM chunks_fts", [])?;
                    transaction.execute("DELETE FROM chunks", [])?;
                    transaction.execute("DELETE FROM fetch_cache", [])?;
                }
            }
            transaction.commit()?;
            Ok(deleted.max(0) as usize)
        })
        .await
    }

    /// Number of chunks stored for `source`.
    pub async fn source_chunk_count(self: &Arc<Self>, source: String) -> anyhow::Result<usize> {
        self.call(move |connection| {
            let count: i64 = connection.query_row(
                "SELECT COUNT(*) FROM chunks WHERE source = ?1",
                [&source],
                |row| row.get(0),
            )?;
            Ok(count.max(0) as usize)
        })
        .await
    }

    /// A cached fetch younger than `max_age`, if present.
    pub async fn cached_fetch(
        self: &Arc<Self>,
        url: String,
        source: String,
        max_age: Duration,
    ) -> anyhow::Result<Option<CachedFetch>> {
        self.call(move |connection| {
            let row = connection
                .query_row(
                    "SELECT body, content_type, fetched_at FROM fetch_cache \
                     WHERE url = ?1 AND source = ?2",
                    params![url, source],
                    |row| {
                        let body: String = row.get(0)?;
                        let content_type: String = row.get(1)?;
                        let fetched_at: i64 = row.get(2)?;
                        Ok((body, content_type, fetched_at))
                    },
                )
                .optional()?;
            let Some((body, content_type, fetched_at)) = row else {
                return Ok(None);
            };
            let age_ms = crate::ai::util::now_ms().saturating_sub(fetched_at.max(0) as u64);
            if age_ms > max_age.as_millis() as u64 {
                return Ok(None);
            }
            Ok(Some(CachedFetch { body, content_type }))
        })
        .await
    }

    /// Store a fetched body for the URL/source cache key.
    pub async fn store_fetch(
        self: &Arc<Self>,
        url: String,
        source: String,
        content_type: String,
        body: String,
    ) -> anyhow::Result<()> {
        self.call(move |connection| {
            connection.execute(
                "INSERT INTO fetch_cache (url, source, fetched_at, content_type, body) \
                 VALUES (?1, ?2, ?3, ?4, ?5) \
                 ON CONFLICT(url, source) DO UPDATE SET \
                   fetched_at = excluded.fetched_at, \
                   content_type = excluded.content_type, \
                   body = excluded.body",
                params![
                    url,
                    source,
                    crate::ai::util::now_ms() as i64,
                    content_type,
                    body
                ],
            )?;
            Ok(())
        })
        .await
    }
}

// `bm25(fts, 5.0, 1.0)`: a title hit is worth five content hits.
// ---------------------------------------------------------------------------
// Scoped search helpers (session / worktree / project, RRF, timeline)
// ---------------------------------------------------------------------------

/// `(?n = 'project' OR (?n = 'session' AND c.session_id = ?m) OR (?n =
/// 'worktree' AND c.worktree = ?k))` with the caller's parameter indexes.
fn scope_predicate(scope_index: usize, session_index: usize, worktree_index: usize) -> String {
    // A row written before the scope columns existed (or by a host that never
    // pushed a cwd) has an empty worktree; keep it visible in worktree scope.
    // Session scope stays strict: an unscoped row does not belong to a session.
    format!(
        "(?{scope_index} = 'project' \
          OR (?{scope_index} = 'session' AND c.session_id = ?{session_index}) \
          OR (?{scope_index} = 'worktree' AND (c.worktree = ?{worktree_index} OR c.worktree = '')))"
    )
}

fn row_to_hit(row: &rusqlite::Row<'_>, terms: &[String]) -> rusqlite::Result<SearchHit> {
    let content: String = row.get(6)?;
    let indexed_at: i64 = row.get(7)?;
    let rank: f64 = row.get(8)?;
    Ok(SearchHit {
        id: row.get(0)?,
        title: row.get(1)?,
        source: row.get(2)?,
        content_type: row.get(3)?,
        snippet: make_snippet(&content, terms, SNIPPET_CHARS),
        line_start: row.get(4)?,
        line_end: row.get(5)?,
        indexed_at,
        // BM25 is smaller-is-better; flip it so higher is better.
        score: -rank,
    })
}

// The positional SQL bindings (scope/session/worktree/limit/offset) stay
// separate so this hot path does not allocate a request struct per query.
#[allow(clippy::too_many_arguments)]
fn scoped_relevance(
    connection: &Connection,
    request: &ScopedSearch,
    scope: &str,
    session: &str,
    worktree: &str,
    expression: &str,
    terms: &[String],
    limit: usize,
) -> rusqlite::Result<Vec<SearchHit>> {
    let sql = format!(
        "SELECT c.id, c.title, c.source, c.content_type, c.line_start, c.line_end, c.content, \
                c.indexed_at, bm25(chunks_fts, 5.0, 1.0) AS rank \
         FROM chunks_fts \
         JOIN chunks c ON c.id = chunks_fts.rowid \
         WHERE chunks_fts MATCH ?1 \
           AND (?2 IS NULL OR c.source = ?2) \
           AND (?3 IS NULL OR LOWER(c.content_type) = ?3) \
           AND {} \
         ORDER BY rank \
         LIMIT ?7 OFFSET ?8",
        scope_predicate(4, 5, 6)
    );
    let mut statement = connection.prepare_cached(&sql)?;
    let mut rows = statement.query(params![
        expression,
        request.source.as_deref(),
        request.content_type.as_deref(),
        scope,
        session,
        worktree,
        limit as i64,
        request.offset as i64,
    ])?;
    let mut hits = Vec::new();
    while let Some(row) = rows.next()? {
        hits.push(row_to_hit(row, terms)?);
    }
    Ok(hits)
}

/// Recency-ordered candidates (project-scope RRF side channel).
fn recent_candidates(
    connection: &Connection,
    request: &ScopedSearch,
    scope: &str,
    session: &str,
    worktree: &str,
    terms: &[String],
    limit: usize,
) -> rusqlite::Result<Vec<SearchHit>> {
    let sql = format!(
        "SELECT c.id, c.title, c.source, c.content_type, c.line_start, c.line_end, c.content, \
                c.indexed_at, -CAST(c.indexed_at AS REAL) AS rank \
         FROM chunks c \
         WHERE (?1 IS NULL OR c.source = ?1) \
           AND (?2 IS NULL OR LOWER(c.content_type) = ?2) \
           AND {} \
         ORDER BY c.indexed_at DESC, c.id DESC \
         LIMIT ?6",
        scope_predicate(3, 4, 5)
    );
    let mut statement = connection.prepare_cached(&sql)?;
    let mut rows = statement.query(params![
        request.source.as_deref(),
        request.content_type.as_deref(),
        scope,
        session,
        worktree,
        limit as i64,
    ])?;
    let mut hits = Vec::new();
    while let Some(row) = rows.next()? {
        hits.push(row_to_hit(row, terms)?);
    }
    Ok(hits)
}

/// Reciprocal Rank Fusion of BM25 and recency (k = 60, the standard constant).
fn rrf_project(
    connection: &Connection,
    request: &ScopedSearch,
    scope: &str,
    session: &str,
    worktree: &str,
    expression: &str,
    terms: &[String],
) -> rusqlite::Result<Vec<SearchHit>> {
    const RRF_K: f64 = 60.0;
    let pool = request.limit.saturating_mul(4).max(request.limit);
    let lexical = scoped_relevance(
        connection,
        &ScopedSearch {
            offset: 0,
            ..clone_request(request)
        },
        scope,
        session,
        worktree,
        expression,
        terms,
        pool,
    )?;
    let recent = recent_candidates(connection, request, scope, session, worktree, terms, pool)?;

    let mut fused: HashMap<i64, (f64, SearchHit)> = HashMap::with_capacity(pool * 2);
    for (rank, hit) in lexical.into_iter().enumerate() {
        let score = 1.0 / (RRF_K + (rank + 1) as f64);
        fused.insert(hit.id, (score, hit));
    }
    for (rank, hit) in recent.into_iter().enumerate() {
        let score = 1.0 / (RRF_K + (rank + 1) as f64);
        match fused.get_mut(&hit.id) {
            Some((total, _)) => *total += score,
            None => {
                fused.insert(hit.id, (score, hit));
            }
        }
    }
    let mut ordered: Vec<(f64, SearchHit)> = fused.into_values().collect();
    ordered.sort_by(|left, right| {
        right
            .0
            .total_cmp(&left.0)
            .then_with(|| left.1.id.cmp(&right.1.id))
    });
    Ok(ordered
        .into_iter()
        .skip(request.offset)
        .take(request.limit)
        .map(|(score, mut hit)| {
            hit.score = score;
            hit
        })
        .collect())
}

/// Borrow-friendly clone for a one-off offset override (the struct is small;
/// the query strings are not duplicated because this path runs once).
fn clone_request(request: &ScopedSearch) -> ScopedSearch {
    ScopedSearch {
        queries: Vec::new(),
        source: request.source.clone(),
        content_type: request.content_type.clone(),
        limit: request.limit,
        offset: request.offset,
        scope: request.scope,
        sort: request.sort,
        after: request.after,
        session_id: request.session_id.clone(),
        worktree: request.worktree.clone(),
    }
}

fn timeline_page(
    connection: &Connection,
    request: &ScopedSearch,
    scope: &str,
    session: &str,
    worktree: &str,
    terms: &[String],
) -> rusqlite::Result<Vec<SearchHit>> {
    let sql = format!(
        "SELECT c.id, c.title, c.source, c.content_type, c.line_start, c.line_end, c.content, \
                c.indexed_at, -CAST(c.indexed_at AS REAL) AS rank \
         FROM chunks c \
         WHERE (?1 IS NULL OR c.source = ?1) \
           AND (?2 IS NULL OR LOWER(c.content_type) = ?2) \
           AND (?3 IS NULL OR c.indexed_at > ?3) \
           AND {} \
         ORDER BY c.indexed_at ASC, c.id ASC \
         LIMIT ?7 OFFSET ?8",
        scope_predicate(4, 5, 6)
    );
    let mut statement = connection.prepare_cached(&sql)?;
    let mut rows = statement.query(params![
        request.source.as_deref(),
        request.content_type.as_deref(),
        request.after,
        scope,
        session,
        worktree,
        request.limit as i64,
        request.offset as i64,
    ])?;
    let mut hits = Vec::new();
    while let Some(row) = rows.next()? {
        hits.push(row_to_hit(row, terms)?);
    }
    Ok(hits)
}

fn like_fallback_search_scoped(
    connection: &Connection,
    terms: &[String],
    request: &ScopedSearch,
    scope: &str,
    session: &str,
    worktree: &str,
    limit: usize,
) -> rusqlite::Result<Vec<SearchHit>> {
    let likes = terms
        .iter()
        .map(|term| {
            format!(
                "%{}%",
                term.replace('\\', "\\\\")
                    .replace('%', "\\%")
                    .replace('_', "\\_")
            )
        })
        .collect::<Vec<_>>();
    let conditions = terms
        .iter()
        .map(|_| "(c.title LIKE ? ESCAPE '\\' OR c.content LIKE ? ESCAPE '\\')".to_string())
        .collect::<Vec<_>>()
        .join(" OR ");
    let source_index = likes.len() * 2 + 1;
    let type_index = source_index + 1;
    let scope_index = type_index + 1;
    let session_index = scope_index + 1;
    let worktree_index = session_index + 1;
    let sql = format!(
        "SELECT c.id, c.title, c.source, c.content_type, c.line_start, c.line_end, c.content, \
                c.indexed_at, 0.0 AS rank \
         FROM chunks c \
         WHERE ({conditions}) \
           AND (?{source_index} IS NULL OR c.source = ?{source_index}) \
           AND (?{type_index} IS NULL OR LOWER(c.content_type) = ?{type_index}) \
           AND {} \
         LIMIT ?{limit_index}",
        scope_predicate(scope_index, session_index, worktree_index),
        limit_index = worktree_index + 1,
    );
    let mut statement = connection.prepare_cached(&sql)?;
    let mut bindings: Vec<rusqlite::types::Value> = Vec::with_capacity(likes.len() * 2 + 5);
    for like in &likes {
        bindings.push(like.clone().into());
        bindings.push(like.clone().into());
    }
    bindings.push(request.source.clone().into());
    bindings.push(request.content_type.clone().into());
    bindings.push(scope.to_owned().into());
    bindings.push(session.to_owned().into());
    bindings.push(worktree.to_owned().into());
    bindings.push((limit as i64).into());
    let mut rows = statement.query(rusqlite::params_from_iter(bindings.iter()))?;
    let mut hits = Vec::new();
    while let Some(row) = rows.next()? {
        let mut hit = row_to_hit(row, terms)?;
        hit.score = 0.0;
        hits.push(hit);
    }
    Ok(hits)
}

const SEARCH_SQL: &str = "\
SELECT c.id, c.title, c.source, c.content_type, c.line_start, c.line_end, c.content, c.indexed_at, \
       bm25(chunks_fts, 5.0, 1.0) AS rank \
FROM chunks_fts \
JOIN chunks c ON c.id = chunks_fts.rowid \
WHERE chunks_fts MATCH ?1 \
  AND (?2 IS NULL OR c.source = ?2) \
  AND (?3 IS NULL OR LOWER(c.content_type) = ?3) \
ORDER BY rank \
LIMIT ?4";

fn init_schema(connection: &Connection) -> rusqlite::Result<()> {
    connection.execute_batch(
        "CREATE TABLE IF NOT EXISTS chunks (\
             id INTEGER PRIMARY KEY AUTOINCREMENT,\
             source TEXT NOT NULL,\
             content_type TEXT NOT NULL,\
             title TEXT NOT NULL,\
             content TEXT NOT NULL,\
             line_start INTEGER NOT NULL,\
             line_end INTEGER NOT NULL,\
             indexed_at INTEGER NOT NULL,\
             session_id TEXT NOT NULL DEFAULT '',\
             worktree TEXT NOT NULL DEFAULT ''\
         );\
         CREATE INDEX IF NOT EXISTS chunks_source_idx ON chunks(source);\
         CREATE INDEX IF NOT EXISTS chunks_content_type_idx ON chunks(content_type);\
         CREATE VIRTUAL TABLE IF NOT EXISTS chunks_fts USING fts5(\
             title, content, tokenize='porter unicode61'\
         );\
         CREATE TABLE IF NOT EXISTS fetch_cache (\
             url TEXT NOT NULL,\
             source TEXT NOT NULL,\
             fetched_at INTEGER NOT NULL,\
             content_type TEXT NOT NULL,\
             body TEXT NOT NULL,\
             PRIMARY KEY (url, source)\
         );\
         CREATE TABLE IF NOT EXISTS events (\
             seq INTEGER PRIMARY KEY AUTOINCREMENT,\
             session_id TEXT NOT NULL,\
             turn INTEGER NOT NULL DEFAULT 0,\
             kind TEXT NOT NULL,\
             tool TEXT NOT NULL,\
             path TEXT,\
             key TEXT,\
             ok INTEGER NOT NULL DEFAULT 1,\
             exit_code INTEGER,\
             bytes INTEGER NOT NULL DEFAULT 0,\
             body TEXT NOT NULL,\
             created_at INTEGER NOT NULL\
         );\
         CREATE INDEX IF NOT EXISTS events_session_idx ON events(session_id, seq);",
    )?;
    // Schema v2: scoped search columns on databases created by v1. SQLite
    // has no `ADD COLUMN IF NOT EXISTS`, so probe the table first.
    for column in ["session_id", "worktree"] {
        if !has_column(connection, "chunks", column)? {
            connection.execute_batch(&format!(
                "ALTER TABLE chunks ADD COLUMN {column} TEXT NOT NULL DEFAULT '';"
            ))?;
        }
    }
    connection.execute_batch(
        "CREATE INDEX IF NOT EXISTS chunks_scope_idx ON chunks(session_id, worktree);",
    )?;
    // Stamp the schema version into the database header. Existing databases
    // built before versioning read 0 and are migrated by the block above.
    let version: i64 = connection.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if version < SCHEMA_VERSION {
        connection.execute_batch(&format!("PRAGMA user_version = {SCHEMA_VERSION};"))?;
    }
    Ok(())
}

/// Whether `table` has `column` (schema migration probe).
fn has_column(connection: &Connection, table: &str, column: &str) -> rusqlite::Result<bool> {
    let mut statement = connection.prepare(&format!("PRAGMA table_info({table})"))?;
    let mut rows = statement.query([])?;
    while let Some(row) = rows.next()? {
        let name: String = row.get(1)?;
        if name.eq_ignore_ascii_case(column) {
            return Ok(true);
        }
    }
    Ok(false)
}

/// `(fts5_available, schema_version, chunks, sources)`.
fn read_diagnostics(connection: &Connection) -> rusqlite::Result<(bool, i64, usize, usize)> {
    let fts5 = fts5_available(connection);
    let schema_version: i64 = connection.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    let chunks: i64 = connection.query_row("SELECT COUNT(*) FROM chunks", [], |row| row.get(0))?;
    let sources: i64 =
        connection.query_row("SELECT COUNT(DISTINCT source) FROM chunks", [], |row| {
            row.get(0)
        })?;
    Ok((
        fts5,
        schema_version,
        chunks.max(0) as usize,
        sources.max(0) as usize,
    ))
}

/// Whether this SQLite build has FTS5. Checks the compile options first
/// (fast, bundled SQLite reports `ENABLE_FTS5`) and falls back to creating a
/// TEMP virtual table, which touches no file.
fn fts5_available(connection: &Connection) -> bool {
    let compiled = connection
        .query_row(
            "SELECT sqlite_compileoption_used('ENABLE_FTS5')",
            [],
            |row| row.get::<_, i64>(0),
        )
        .map(|value| value != 0)
        .unwrap_or(false);
    if compiled {
        return true;
    }
    connection
        .execute_batch(
            "CREATE VIRTUAL TABLE temp.ctx_fts5_probe USING fts5(x);\
             DROP TABLE temp.ctx_fts5_probe;",
        )
        .is_ok()
}

fn replace_source(
    transaction: &rusqlite::Transaction<'_>,
    doc: &IndexDoc,
    default_session: &str,
    default_worktree: &str,
) -> rusqlite::Result<usize> {
    // Replacement is scoped to (source, session): re-indexing the same label
    // from one session updates that session's rows instead of deleting the
    // other sessions' rows for the same label. The user-facing source label is
    // stored verbatim, so `ctx_search(source:)` keeps working across sessions.
    let session = if doc.session_id.is_empty() {
        default_session
    } else {
        doc.session_id.as_str()
    };
    let worktree = if doc.worktree.is_empty() {
        default_worktree
    } else {
        doc.worktree.as_str()
    };
    // FTS rows are keyed by chunks.id; delete them first while the metadata
    // rows still identify the ids.
    transaction.execute(
        "DELETE FROM chunks_fts WHERE rowid IN \
         (SELECT id FROM chunks WHERE source = ?1 AND session_id = ?2)",
        params![doc.source, session],
    )?;
    transaction.execute(
        "DELETE FROM chunks WHERE source = ?1 AND session_id = ?2",
        params![doc.source, session],
    )?;
    let indexed_at = crate::ai::util::now_ms() as i64;
    let mut insert_chunk = transaction.prepare_cached(
        "INSERT INTO chunks \
         (source, content_type, title, content, line_start, line_end, indexed_at, \
          session_id, worktree) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
    )?;
    let mut insert_fts = transaction
        .prepare_cached("INSERT INTO chunks_fts (rowid, title, content) VALUES (?1, ?2, ?3)")?;
    for chunk in &doc.chunks {
        insert_chunk.execute(params![
            doc.source,
            doc.content_type,
            chunk.title,
            chunk.content,
            chunk.line_start as i64,
            chunk.line_end as i64,
            indexed_at,
            session,
            worktree,
        ])?;
        let id = transaction.last_insert_rowid();
        insert_fts.execute(params![id, chunk.title, chunk.content])?;
    }
    Ok(doc.chunks.len())
}

// ---------------------------------------------------------------------------
// Retention (per session)
// ---------------------------------------------------------------------------

/// Journal rows and stored BYTES (`body`) currently held for `session`.
///
/// The cap measures the stored body, not the `bytes` metadata column: the
/// column records the tool's original output size, while `body` is what
/// actually occupies the database and the WAL.
///
/// `LENGTH(text)` counts CHARACTERS in SQLite, so a UTF-8 body would count
/// once per code point rather than once per byte. Casting to BLOB measures the
/// encoded bytes, which is what the retention caps are actually bounding.
fn journal_usage(connection: &Connection, session: &str) -> rusqlite::Result<(i64, i64)> {
    connection.query_row(
        "SELECT COUNT(*), COALESCE(SUM(LENGTH(CAST(body AS BLOB))), 0) \
         FROM events WHERE session_id = ?1",
        [session],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )
}

/// Chunk rows and encoded bytes (`title + content`) currently stored for
/// `session` (see [`journal_usage`] for why the BLOB cast is required).
fn chunk_usage(connection: &Connection, session: &str) -> rusqlite::Result<(i64, i64)> {
    connection.query_row(
        "SELECT COUNT(*), \
                COALESCE(SUM(LENGTH(CAST(title AS BLOB)) + LENGTH(CAST(content AS BLOB))), 0) \
         FROM chunks WHERE session_id = ?1",
        [session],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )
}

/// Drop a session's journal rows beyond the TTL, the row cap and the byte
/// cap, oldest first. Every query is scoped to `session`, so one session can
/// never evict another session's rows.
fn trim_session_journal(
    connection: &Connection,
    session: &str,
    limits: &RetentionLimits,
) -> rusqlite::Result<usize> {
    let now = crate::ai::util::now_ms() as i64;
    let oldest = now - (limits.journal_ttl_secs as i64) * 1_000;
    let mut removed = connection.execute(
        "DELETE FROM events WHERE session_id = ?1 AND created_at < ?2",
        params![session, oldest],
    )?;

    let (rows, _) = journal_usage(connection, session)?;
    if rows > limits.journal_max_rows as i64 {
        // Keep exactly the newest `max_rows` rows.
        removed += connection.execute(
            "DELETE FROM events WHERE seq IN \
             (SELECT seq FROM events WHERE session_id = ?1 ORDER BY seq ASC LIMIT ?2)",
            params![session, rows - limits.journal_max_rows as i64],
        )?;
    }

    let (_, stored_bytes) = journal_usage(connection, session)?;
    if stored_bytes > limits.journal_max_bytes as i64 {
        // Newest-first accumulation: the first row that would push the total
        // past the cap is the cutoff (`seq` is append order within a session).
        let mut statement = connection.prepare(
            "SELECT seq, LENGTH(CAST(body AS BLOB)) FROM events \
             WHERE session_id = ?1 ORDER BY seq DESC",
        )?;
        let mut scanned = statement.query([session])?;
        let mut total = 0i64;
        let mut cutoff = None;
        while let Some(row) = scanned.next()? {
            let seq: i64 = row.get(0)?;
            let size: i64 = row.get(1)?;
            if total + size.max(0) > limits.journal_max_bytes as i64 {
                cutoff = Some(seq);
                break;
            }
            total += size.max(0);
        }
        drop(scanned);
        drop(statement);
        if let Some(cutoff) = cutoff {
            removed += connection.execute(
                "DELETE FROM events WHERE session_id = ?1 AND seq <= ?2",
                params![session, cutoff],
            )?;
        }
    }
    Ok(removed)
}

/// Drop a session's oldest chunks until the session is within the row and
/// byte caps. Chunks are ordered by `(indexed_at, id)`, so the newest
/// material always survives.
fn trim_session_chunks(
    connection: &Connection,
    session: &str,
    limits: &RetentionLimits,
) -> rusqlite::Result<usize> {
    let (rows, bytes) = chunk_usage(connection, session)?;
    if rows <= limits.chunk_max_rows as i64 && bytes <= limits.chunk_max_bytes as i64 {
        return Ok(0);
    }
    let mut statement = connection.prepare(
        "SELECT LENGTH(CAST(title AS BLOB)) + LENGTH(CAST(content AS BLOB)) FROM chunks \
         WHERE session_id = ?1 ORDER BY indexed_at ASC, id ASC",
    )?;
    let mut scanned = statement.query([session])?;
    let mut rows_left = rows;
    let mut bytes_left = bytes;
    let mut drop_rows = 0i64;
    while let Some(row) = scanned.next()? {
        if rows_left <= limits.chunk_max_rows as i64 && bytes_left <= limits.chunk_max_bytes as i64
        {
            break;
        }
        bytes_left -= row.get::<_, i64>(0)?.max(0);
        rows_left -= 1;
        drop_rows += 1;
    }
    drop(scanned);
    drop(statement);
    if drop_rows == 0 {
        return Ok(0);
    }
    // FTS rows are keyed by chunks.id: delete them first, while the metadata
    // rows still identify the ids. `drop_rows` already counts the chunks that
    // fall outside the caps, so that is the number removed.
    connection.execute(
        "DELETE FROM chunks_fts WHERE rowid IN \
         (SELECT id FROM chunks WHERE session_id = ?1 ORDER BY indexed_at ASC, id ASC LIMIT ?2)",
        params![session, drop_rows],
    )?;
    connection.execute(
        "DELETE FROM chunks WHERE id IN \
         (SELECT id FROM chunks WHERE session_id = ?1 ORDER BY indexed_at ASC, id ASC LIMIT ?2)",
        params![session, drop_rows],
    )?;
    Ok(drop_rows as usize)
}

/// Keep the project-wide fetch cache bounded (oldest `fetched_at` first).
fn trim_fetch_cache(connection: &Connection, limits: &RetentionLimits) -> rusqlite::Result<usize> {
    let (rows, bytes): (i64, i64) = connection.query_row(
        "SELECT COUNT(*), COALESCE(SUM(LENGTH(CAST(body AS BLOB))), 0) FROM fetch_cache",
        [],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    if rows <= limits.fetch_cache_max_rows as i64 && bytes <= limits.fetch_cache_max_bytes as i64 {
        return Ok(0);
    }
    let mut statement = connection.prepare(
        "SELECT LENGTH(CAST(body AS BLOB)) FROM fetch_cache \
         ORDER BY fetched_at ASC, url ASC, source ASC",
    )?;
    let mut scanned = statement.query([])?;
    let mut rows_left = rows;
    let mut bytes_left = bytes;
    let mut drop_rows = 0i64;
    while let Some(row) = scanned.next()? {
        if rows_left <= limits.fetch_cache_max_rows as i64
            && bytes_left <= limits.fetch_cache_max_bytes as i64
        {
            break;
        }
        bytes_left -= row.get::<_, i64>(0)?.max(0);
        rows_left -= 1;
        drop_rows += 1;
    }
    drop(scanned);
    drop(statement);
    if drop_rows == 0 {
        return Ok(0);
    }
    connection.execute(
        "DELETE FROM fetch_cache WHERE rowid IN \
         (SELECT rowid FROM fetch_cache ORDER BY fetched_at ASC, url ASC, source ASC LIMIT ?1)",
        [drop_rows],
    )
}

/// What a session stores now, and whether that exceeds a limit.
fn read_retention(
    connection: &Connection,
    session: &str,
    limits: &RetentionLimits,
) -> rusqlite::Result<RetentionReport> {
    let (journal_rows, journal_bytes) = journal_usage(connection, session)?;
    let (chunk_rows, chunk_bytes) = chunk_usage(connection, session)?;
    let oldest_created: Option<i64> = connection
        .query_row(
            "SELECT MIN(created_at) FROM events WHERE session_id = ?1",
            [session],
            |row| row.get(0),
        )
        .optional()?
        .flatten();
    let now = crate::ai::util::now_ms() as i64;
    let journal_oldest_secs = oldest_created
        .map(|created| (now.saturating_sub(created).max(0) / 1_000) as u64)
        .unwrap_or(0);
    Ok(RetentionReport {
        session_id: session.to_owned(),
        journal_rows: journal_rows.max(0) as usize,
        journal_bytes: journal_bytes.max(0) as u64,
        journal_oldest_secs,
        chunk_rows: chunk_rows.max(0) as usize,
        chunk_bytes: chunk_bytes.max(0) as u64,
        over_limit: journal_rows > limits.journal_max_rows as i64
            || journal_bytes > limits.journal_max_bytes as i64
            || journal_oldest_secs > limits.journal_ttl_secs
            || chunk_rows > limits.chunk_max_rows as i64
            || chunk_bytes > limits.chunk_max_bytes as i64,
    })
}

// ---------------------------------------------------------------------------
// Default DB path
// ---------------------------------------------------------------------------

/// Stable FNV-1a 64-bit hash of the canonical project path (hex, 16 chars).
/// Version-independent on purpose: a Rust `DefaultHasher` is not stable across
/// toolchains, and the DB file name must survive upgrades.
pub fn project_hash(path: &Path) -> String {
    let mut text = path.to_string_lossy().into_owned();
    // Windows paths are case-insensitive; normalize so the same project
    // reached through differently-cased paths shares one database.
    if cfg!(windows) {
        text.make_ascii_lowercase();
    }
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in text.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{hash:016x}")
}

/// `<base>/context-mode/<hash-of-cwd>.db`. Both inputs are already resolved
/// (session cwd or process cwd; the agent dir or the hermetic override) and
/// the cwd is canonicalized, so differently-spelled paths to the same
/// directory hash identically.
fn default_db_path(cwd: &Path, base: &Path) -> PathBuf {
    base.join("context-mode")
        .join(format!("{}.db", project_hash(cwd)))
}

// ---------------------------------------------------------------------------
// Snippets
// ---------------------------------------------------------------------------

/// A ~`max_chars` window around the first matching query term (or the head of
/// the section when nothing matches).
fn make_snippet(content: &str, terms: &[String], max_chars: usize) -> String {
    let window = max_chars.max(80);
    // A byte length at or below the window bounds the char count too: the
    // whole section fits, so skip the char scan entirely (the common case for
    // ~2 KiB chunks).
    if content.len() <= window {
        return content.trim().to_owned();
    }
    let chars_len = content.chars().count();
    if chars_len == 0 {
        return String::new();
    }
    // The whole section fits: no lowercase scan and no second allocation
    // beyond the trimmed result.
    if chars_len <= window {
        return content.trim().to_owned();
    }
    let mut position = None;
    if !terms.is_empty() {
        // ASCII is the common case for code/log chunks; `to_ascii_lowercase`
        // avoids the Unicode case-fold machinery without changing the match
        // semantics for non-ASCII input (that still uses `to_lowercase`).
        let lowered = if content.is_ascii() {
            content.to_ascii_lowercase()
        } else {
            content.to_lowercase()
        };
        for term in terms {
            let needle = term.to_lowercase();
            if needle.is_empty() {
                continue;
            }
            if let Some(byte_index) = lowered.find(&needle) {
                // Lowercasing can change byte lengths (e.g. 'İ'), so the match
                // offset is only reusable when it is still a char boundary.
                position = Some(if content.is_char_boundary(byte_index) {
                    content[..byte_index].chars().count()
                } else {
                    0
                });
                break;
            }
        }
    }
    let center = position.unwrap_or(0);
    let start = center.saturating_sub(window / 3);
    let end = (start + window).min(chars_len);
    let start = end.saturating_sub(window);
    if start == 0 && end == chars_len {
        return content.trim().to_owned();
    }
    // Convert the two char indices to byte offsets in one pass instead of
    // collecting the whole section into a `Vec<char>` (4 bytes per char).
    let mut start_byte = content.len();
    let mut end_byte = content.len();
    for (char_index, (byte_index, _)) in content.char_indices().enumerate() {
        if char_index == start {
            start_byte = byte_index;
        }
        if char_index == end {
            end_byte = byte_index;
            break;
        }
    }
    let mut snippet = content[start_byte..end_byte].trim().to_owned();
    if start > 0 {
        snippet.insert(0, '…');
    }
    if end < chars_len {
        snippet.push('…');
    }
    snippet
}

/// FTS5 MATCH expression from a natural-language query: alphanumeric terms
/// OR-ed, each quoted (so FTS operators in user input cannot make the query
/// fail). `None` when the query has no indexable terms.
///
/// The second return value reports whether any term contains non-ASCII
/// characters (CJK and friends). The `unicode61` tokenizer folds every run of
/// contiguous CJK characters into a single FTS token (`令牌轮换会记录审计事件`
/// is one token), so a shorter CJK query never matches it; the caller uses the
/// flag to run a LIKE fallback scan when FTS returns no rows.
fn build_match_expression(query: &str) -> Option<(String, Vec<String>, bool)> {
    let mut terms = Vec::new();
    let mut non_ascii = false;
    let mut current = String::new();
    for ch in query.chars() {
        if ch.is_alphanumeric() || ch == '_' {
            if !ch.is_ascii() {
                non_ascii = true;
            }
            current.push(ch);
        } else if !current.is_empty() {
            terms.push(std::mem::take(&mut current));
        }
    }
    if !current.is_empty() {
        terms.push(current);
    }
    if terms.is_empty() {
        return None;
    }
    let expression = terms
        .iter()
        .map(|term| format!("\"{}\"", term.replace('"', "")))
        .collect::<Vec<_>>()
        .join(" OR ");
    Some((expression, terms, non_ascii))
}

/// LIKE fallback for queries the FTS tokenizer cannot match — specifically CJK
/// runs, which `unicode61` collapses into one token per contiguous run. A
/// substring scan over `chunks.content` (and `title`) is the only tokenizer-
/// independent way to serve them, and it works on existing databases with no
/// schema change or reindex. Ranked like FTS: title matches weigh five times a
/// content match, then by hit count; deterministic tiebreak on id. Bounded by
/// the same filters/limit as [`SEARCH_SQL`].
fn like_fallback_search(
    connection: &Connection,
    terms: &[String],
    source: Option<&str>,
    content_type: Option<&str>,
    limit: i64,
) -> rusqlite::Result<Vec<SearchHit>> {
    // Escape LIKE wildcards so a query containing % or _ keeps its literal
    // meaning. The escaped pattern is paired with `ESCAPE '\'` per LIKE so
    // backslash, percent and underscore all stay literal.
    let likes = terms
        .iter()
        .map(|term| {
            format!(
                "%{}%",
                term.replace('\\', "\\\\")
                    .replace('%', "\\%")
                    .replace('_', "\\_")
            )
        })
        .collect::<Vec<_>>();
    let conditions = terms
        .iter()
        .map(|_| "(c.title LIKE ? ESCAPE '\\' OR c.content LIKE ? ESCAPE '\\')".to_string())
        .collect::<Vec<_>>()
        .join(" OR ");
    let source_index = likes.len() * 2 + 1;
    let type_index = source_index + 1;
    let sql = format!(
        "SELECT c.id, c.title, c.source, c.content_type, c.line_start, c.line_end, c.content, \
                c.indexed_at \
         FROM chunks c \
         WHERE ({conditions}) \
           AND (?{source_index} IS NULL OR c.source = ?{source_index}) \
           AND (?{type_index} IS NULL OR LOWER(c.content_type) = ?{type_index}) \
         LIMIT 200"
    );
    let mut statement = connection.prepare_cached(&sql)?;
    let mut bindings: Vec<rusqlite::types::Value> = Vec::with_capacity(likes.len() * 2 + 2);
    for like in &likes {
        bindings.push(like.clone().into());
        bindings.push(like.clone().into());
    }
    bindings.push(source.map(str::to_owned).into());
    bindings.push(content_type.map(str::to_owned).into());
    let mut rows = statement.query(rusqlite::params_from_iter(bindings.iter()))?;
    let mut scored: Vec<(f64, i64, SearchHit)> = Vec::new();
    while let Some(row) = rows.next()? {
        let id: i64 = row.get(0)?;
        let title: String = row.get(1)?;
        let content: String = row.get(6)?;
        let mut score = 0.0;
        for term in terms {
            let title_hits = count_occurrences(&title, term);
            let content_hits = count_occurrences(&content, term);
            // Mirror `bm25(fts, 5.0, 1.0)`: a title hit is worth five
            // content hits. Fractional weight per occurrence keeps multiple
            // occurrences ranked above single ones without exploding scores.
            score += 5.0 * title_hits as f64 + content_hits as f64;
        }
        let hit = SearchHit {
            id,
            title: row.get(1)?,
            source: row.get(2)?,
            content_type: row.get(3)?,
            // Reuse the full content for snippet computation below.
            snippet: String::new(),
            line_start: row.get(4)?,
            line_end: row.get(5)?,
            indexed_at: row.get(7)?,
            score: 0.0,
        };
        scored.push((score, id, hit));
    }
    scored.sort_by(|a, b| b.0.total_cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    Ok(scored
        .into_iter()
        .take(limit.max(0) as usize)
        .map(|(score, _, mut hit)| {
            hit.score = score;
            hit
        })
        .collect())
}

/// Case-sensitive occurrence count; CJK terms have no case folding, and the
/// LIKE comparison above is case-sensitive for them by SQLite's default
/// (non-ASCII LIKE is case-sensitive), so both sides agree.
fn count_occurrences(haystack: &str, needle: &str) -> usize {
    if needle.is_empty() {
        return 0;
    }
    let mut count = 0usize;
    let mut rest = haystack;
    while let Some(position) = rest.find(needle) {
        count += 1;
        rest = &rest[position + needle.len()..];
    }
    count
}

/// Fill `snippet` for LIKE-fallback hits with the same [`make_snippet`]
/// window the FTS path uses, centered on the first matching term.
fn fill_snippet(hit: &mut SearchHit, content: &str, terms: &[String]) {
    hit.snippet = make_snippet(content, terms, SNIPPET_CHARS);
}

// ---------------------------------------------------------------------------
// Directory walk (`ctx_index(path: <dir>)`)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct WalkedFile {
    pub path: PathBuf,
    /// Slash-separated path relative to the walk root.
    pub relative: String,
}

struct IgnoreRule {
    pattern: String,
    dir_only: bool,
    anchored: bool,
}

/// Work actually performed by one directory walk (hidden test seam: proves
/// the walk stops at the file cap instead of traversing/sorting whole trees).
#[derive(Debug, Clone, Copy, Default)]
pub struct WalkStats {
    /// Number of directories whose entries were listed.
    pub directories: usize,
    /// Number of directory entries read (bounded by
    /// [`WALK_MAX_ENTRIES_PER_DIR`] per directory).
    pub entries: usize,
}

/// Bounded, symlink-free walk with the fixed extension allow-list and the
/// built-in directory skip list. `.gitignore` at the walk root is applied
/// (simple globs; cheap, no git invocation).
pub fn walk_directory(root: &Path) -> Result<Vec<WalkedFile>, String> {
    let mut stats = WalkStats::default();
    walk_directory_bounded(root, WALK_MAX_FILES, &mut stats)
}

/// Walk at most `max_files` allowed files, reporting the work performed.
fn walk_directory_bounded(
    root: &Path,
    max_files: usize,
    stats: &mut WalkStats,
) -> Result<Vec<WalkedFile>, String> {
    let metadata = std::fs::metadata(root)
        .map_err(|error| format!("cannot stat `{}`: {error}", root.display()))?;
    if !metadata.is_dir() {
        return Err(format!("`{}` is not a directory", root.display()));
    }
    let rules = read_gitignore(root);
    let mut files = Vec::new();
    walk_dir(root, root, 0, &rules, &mut files, max_files, stats)?;
    Ok(files)
}

/// Hidden test seam: run the walk and report `(files, dirs, entries)`, so a
/// bounded-walk test can assert a pathological subtree was never listed
/// without relying on timing.
pub fn walk_directory_probe(
    root: &Path,
    max_files: usize,
) -> Result<(usize, usize, usize), String> {
    let mut stats = WalkStats::default();
    let files = walk_directory_bounded(root, max_files, &mut stats)?;
    Ok((files.len(), stats.directories, stats.entries))
}

fn walk_dir(
    root: &Path,
    dir: &Path,
    depth: usize,
    rules: &[IgnoreRule],
    files: &mut Vec<WalkedFile>,
    max_files: usize,
    stats: &mut WalkStats,
) -> Result<(), String> {
    // Stop as soon as the file budget is spent: no further directory is
    // listed, let alone sorted. The directory budget independently bounds a
    // tree that never produces files.
    if files.len() >= max_files || stats.directories >= WALK_MAX_DIRECTORIES {
        return Ok(());
    }
    stats.directories += 1;
    let entries = std::fs::read_dir(dir)
        .map_err(|error| format!("cannot read directory `{}`: {error}", dir.display()))?;
    // Bounded breadth: read at most WALK_MAX_ENTRIES_PER_DIR entries, then
    // sort only that window. A flat directory of hundreds of thousands of
    // entries costs one bounded scan instead of an unbounded collect+sort.
    let mut paths: Vec<PathBuf> = Vec::new();
    for (scanned, entry) in entries.enumerate() {
        if scanned >= WALK_MAX_ENTRIES_PER_DIR {
            break;
        }
        stats.entries += 1;
        let Ok(entry) = entry else {
            continue;
        };
        paths.push(entry.path());
    }
    paths.sort();

    // Files first, then subdirectories: if this directory already supplies
    // the remaining budget, no subtree is listed at all.
    let mut subdirectories: Vec<PathBuf> = Vec::new();
    for path in paths {
        if files.len() >= max_files {
            break;
        }
        let Ok(metadata) = std::fs::symlink_metadata(&path) else {
            continue;
        };
        let name = path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        // Never follow symlinks: cycles and escape-by-link are both avoided.
        if metadata.file_type().is_symlink() {
            continue;
        }
        let relative = path
            .strip_prefix(root)
            .unwrap_or(&path)
            .to_string_lossy()
            .replace('\\', "/");
        if metadata.is_dir() {
            if WALK_SKIP_DIRS.contains(&name.as_str()) {
                continue;
            }
            if depth >= WALK_MAX_DEPTH {
                continue;
            }
            if is_ignored(rules, &relative, &name, true) {
                continue;
            }
            subdirectories.push(path);
        } else if metadata.is_file() {
            if metadata.len() > MAX_INDEX_FILE_BYTES {
                continue;
            }
            let extension = path
                .extension()
                .map(|extension| extension.to_string_lossy().to_ascii_lowercase())
                .unwrap_or_default();
            if !WALK_EXTENSIONS.contains(&extension.as_str()) {
                continue;
            }
            if is_ignored(rules, &relative, &name, false) {
                continue;
            }
            files.push(WalkedFile { path, relative });
        }
    }
    for path in subdirectories {
        if files.len() >= max_files {
            break;
        }
        walk_dir(root, &path, depth + 1, rules, files, max_files, stats)?;
    }
    Ok(())
}

fn read_gitignore(root: &Path) -> Vec<IgnoreRule> {
    let Ok(contents) = std::fs::read_to_string(root.join(".gitignore")) else {
        return Vec::new();
    };
    let mut rules = Vec::new();
    for raw in contents.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with('!') {
            continue;
        }
        let dir_only = line.ends_with('/');
        let line = line.trim_end_matches('/');
        let anchored = line.starts_with('/');
        let line = line.trim_start_matches('/');
        if line.is_empty() {
            continue;
        }
        rules.push(IgnoreRule {
            pattern: line.to_owned(),
            dir_only,
            anchored,
        });
    }
    rules
}

fn is_ignored(rules: &[IgnoreRule], relative: &str, name: &str, is_dir: bool) -> bool {
    rules.iter().any(|rule| {
        if rule.dir_only && !is_dir {
            return false;
        }
        if rule.anchored {
            glob_match(&rule.pattern, relative)
        } else {
            glob_match(&rule.pattern, name) || glob_match(&rule.pattern, relative)
        }
    })
}

/// Tiny gitignore glob: `*` (any run), `?` (one char), literals. `*` may cross
/// `/` — deliberately permissive; the goal is cheap default noise filtering.
fn glob_match(pattern: &str, text: &str) -> bool {
    let pattern: Vec<char> = pattern.chars().collect();
    let text: Vec<char> = text.chars().collect();
    let mut p = 0usize;
    let mut t = 0usize;
    let mut star: Option<usize> = None;
    let mut star_text = 0usize;
    while t < text.len() {
        if p < pattern.len() && (pattern[p] == '?' || pattern[p] == text[t]) {
            p += 1;
            t += 1;
        } else if p < pattern.len() && pattern[p] == '*' {
            star = Some(p);
            star_text = t;
            p += 1;
        } else if let Some(star_index) = star {
            p = star_index + 1;
            star_text += 1;
            t = star_text;
        } else {
            return false;
        }
    }
    while p < pattern.len() && pattern[p] == '*' {
        p += 1;
    }
    p == pattern.len()
}

/// Content type for an indexed file extension.
pub fn content_type_for_extension(extension: &str) -> &'static str {
    match extension {
        "md" | "mdx" => "markdown",
        "json" => "json",
        "yaml" | "yml" => "yaml",
        "ts" | "tsx" | "js" | "jsx" | "py" | "rs" | "go" | "sh" => "code",
        _ => "text",
    }
}

// ---------------------------------------------------------------------------
// Tools: ctx_index / ctx_search / ctx_stats / ctx_purge
// ---------------------------------------------------------------------------

use crate::agent::tool_batch::ToolResult;
use crate::agent::tool_execution::{AgentTool, BoxFuture, ToolContext};

/// Resolve a `ctx_index` path against the session cwd and require it to stay
/// inside that cwd after canonicalization (so `..`, absolute paths and
/// symlink/junction escapes are all rejected). Mirrors the resolution in
/// `ctx_execute_file::read_confined_file`.
fn resolve_index_path(cwd: &Path, raw: &str) -> Result<PathBuf, String> {
    if Path::new(raw)
        .components()
        .any(|component| matches!(component, std::path::Component::ParentDir))
    {
        return Err(format!(
            "ctx_index: file access blocked: outside the project root (`{raw}` contains a `..` component)"
        ));
    }
    let cwd = cwd.canonicalize().map_err(|error| {
        format!(
            "ctx_index: cannot resolve the session working directory `{}`: {error}",
            cwd.display()
        )
    })?;
    let candidate = {
        let path = Path::new(raw);
        if path.is_absolute() {
            path.to_path_buf()
        } else {
            cwd.join(path)
        }
    };
    let resolved = candidate
        .canonicalize()
        .map_err(|error| format!("ctx_index: cannot resolve `{raw}` (it must exist): {error}"))?;
    if !resolved.starts_with(&cwd) {
        return Err(format!(
            "ctx_index: file access blocked: outside the project root; only paths under `{}` are allowed",
            cwd.display()
        ));
    }
    Ok(resolved)
}

/// Build the documents for a `ctx_index` call. Runs on the blocking pool
/// (directory walk + file reads); returns owned docs, never the content.
pub fn documents_for_index(
    cwd: &Path,
    content: Option<String>,
    path: Option<String>,
    source: Option<String>,
    content_type: Option<String>,
) -> Result<Vec<IndexDoc>, String> {
    let content_type = content_type
        .filter(|value| !value.trim().is_empty())
        .map(|value| value.trim().to_ascii_lowercase());
    // Source labels are user-facing keys, and both `ctx_search` and
    // `ctx_purge` trim their source filter before the exact-match SQL. Trim
    // the stored label here too; otherwise `source: " docs "` is indexed
    // verbatim and can never be addressed (or scoped-purged) as `"docs"`.
    let source = source
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty());
    if let Some(content) = content {
        let source = source.unwrap_or_else(|| "inline".to_owned());
        let content_type = content_type.unwrap_or_else(|| "markdown".to_owned());
        return Ok(vec![IndexDoc::from_text(source, content_type, &content)]);
    }
    let Some(path) = path else {
        return Err("ctx_index: provide exactly one of `content` or `path`".to_owned());
    };
    // Same confinement as `ctx_execute_file`: canonicalize the session cwd and
    // the target, reject `..` components and anything that resolves outside
    // the cwd (absolute paths and symlink escapes included). Without this,
    // `ctx_index(path: "~/.ssh/id_rsa")` would be an exfiltration primitive.
    let resolved = resolve_index_path(cwd, &path)?;
    let metadata = std::fs::metadata(&resolved)
        .map_err(|error| format!("ctx_index: cannot stat `{path}`: {error}"))?;

    if metadata.is_dir() {
        let base = source.unwrap_or_else(|| path.trim().trim_end_matches(['/', '\\']).to_owned());
        let files = walk_directory(&resolved)?;
        let mut docs = Vec::with_capacity(files.len());
        for file in files {
            let Ok(bytes) = std::fs::read(&file.path) else {
                continue;
            };
            if bytes.len() as u64 > MAX_INDEX_FILE_BYTES {
                continue;
            }
            let extension = file
                .path
                .extension()
                .map(|extension| extension.to_string_lossy().to_ascii_lowercase())
                .unwrap_or_default();
            let file_type = content_type
                .clone()
                .unwrap_or_else(|| content_type_for_extension(&extension).to_owned());
            let text = String::from_utf8_lossy(&bytes);
            docs.push(IndexDoc::from_text(
                format!("{base}/{}", file.relative),
                file_type,
                &text,
            ));
        }
        return Ok(docs);
    }

    if !metadata.is_file() {
        return Err(format!(
            "ctx_index: `{path}` is neither a regular file nor a directory"
        ));
    }
    if metadata.len() > MAX_INDEX_FILE_BYTES {
        return Err(format!(
            "ctx_index: `{path}` is {} bytes; the limit is {} MiB",
            metadata.len(),
            MAX_INDEX_FILE_BYTES / (1024 * 1024)
        ));
    }
    // Single-file indexing enforces the same extension allow-list as the
    // directory walk; otherwise any in-cwd binary/credential file could be
    // stored in the searchable database.
    let extension = resolved
        .extension()
        .map(|extension| extension.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    if !WALK_EXTENSIONS.contains(&extension.as_str()) {
        return Err(format!(
            "ctx_index: `{path}` has an unsupported file extension; single files must be one of: {}",
            WALK_EXTENSIONS.join(", ")
        ));
    }
    let bytes = std::fs::read(&resolved)
        .map_err(|error| format!("ctx_index: cannot read `{path}`: {error}"))?;
    let content_type =
        content_type.unwrap_or_else(|| content_type_for_extension(&extension).to_owned());
    let source = source.unwrap_or_else(|| path.trim().to_owned());
    let text = String::from_utf8_lossy(&bytes);
    Ok(vec![IndexDoc::from_text(source, content_type, &text)])
}

pub struct CtxIndexTool {
    state: Arc<super::ContextModeState>,
}

impl CtxIndexTool {
    pub fn new(state: Arc<super::ContextModeState>) -> Self {
        Self { state }
    }
}

impl AgentTool for CtxIndexTool {
    fn name(&self) -> &'static str {
        super::CTX_INDEX_TOOL_NAME
    }

    fn description(&self) -> &'static str {
        "Store content in the local FTS5 knowledge base (BM25 with title weighting and porter stemming). Markdown is split by headings with fenced code blocks kept intact; other text is split into ~2 KiB chunks. The raw content stays in storage and is never echoed back. \
Provide exactly one of content, path or url. A path must resolve inside the session working directory (`..`, absolute outside paths and symlink escapes are rejected) and must be a regular file or directory with an allowed text/code extension (md, mdx, txt, json, yaml, yml, ts, tsx, js, jsx, py, rs, go, sh). \
A url is fetched over http/https and converted to readable text (HTML → markdown-ish) before indexing; the raw page is never echoed. \
Directories are walked with bounded defaults (depth 5, 200 files, 4096 entries per directory, 2048 directories, node_modules/.git/dist/build/.next/coverage/.venv/__pycache__ skipped, no symlinks, root .gitignore respected). \
Returns indexing metadata (file/chunk/source counts) and the ctx_search result shape, not the content. Query it later with ctx_search, optionally scoped by source. \
Use this for docs, specs, READMEs and API references you will query across turns. For one-off analysis of logs or command output, use ctx_execute or ctx_execute_file instead so the bytes are not persisted."
    }

    fn prompt_snippet(&self) -> Option<String> {
        Some(
            "Index a file, directory or URL into the local searchable knowledge base instead \
of reading it into context; retrieve sections later with ctx_search"
                .to_owned(),
        )
    }

    fn parameters(&self) -> Option<serde_json::Value> {
        Some(serde_json::json!({
            "type": "object",
            "properties": {
                "content": {
                    "type": "string",
                    "description": "Text/markdown to index. Provide this OR path OR url."
                },
                "path": {
                    "type": "string",
                    "description": "File or directory to read and index (content never enters context). Provide this OR content OR url."
                },
                "url": {
                    "type": "string",
                    "description": "http/https page to fetch, convert to text and index. Provide this OR content OR path."
                },
                "source": {
                    "type": "string",
                    "description": "Label for the indexed content (e.g. 'react-docs'). Defaults to 'inline' for content and to the path/url otherwise."
                },
                "contentType": {
                    "type": "string",
                    "description": "Content type label used by ctx_search filters: markdown, text, code, json, yaml. Inferred from the extension for paths."
                }
            },
            "additionalProperties": false
        }))
    }

    fn has_external_side_effects(&self) -> bool {
        // Reads files from the cwd and writes the SQLite knowledge base, so
        // approval/permission layers must not treat this as read-only.
        true
    }

    fn run(&self, args: &serde_json::Value, ctx: &ToolContext) -> BoxFuture<'static, ToolResult> {
        let state = Arc::clone(&self.state);
        let content = args
            .get("content")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned);
        let path = args
            .get("path")
            .and_then(serde_json::Value::as_str)
            .filter(|path| !path.trim().is_empty())
            .map(str::to_owned);
        let source = args
            .get("source")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned);
        let content_type = args
            .get("contentType")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned);
        let url = args
            .get("url")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|url| !url.is_empty())
            .map(str::to_owned);
        let cwd = ctx.cwd.clone();
        let journal_path = path.clone();

        Box::pin(async move {
            let provided = usize::from(content.is_some())
                + usize::from(path.is_some())
                + usize::from(url.is_some());
            if provided != 1 {
                return ToolResult::error(
                    "ctx_index: provide exactly one of `content`, `path` or `url`",
                );
            }
            if let Some(url) = url {
                state.bind_scope(&cwd);
                let page = match super::fetch::fetch_page(&url).await {
                    Ok(page) => page,
                    Err(message) => return ToolResult::error(message),
                };
                let is_html = page.content_type.to_ascii_lowercase().contains("html");
                let text = if is_html {
                    super::fetch::html_to_text(&page.body)
                } else {
                    page.body
                };
                let label = source.unwrap_or_else(|| url.clone());
                let bytes = text.len();
                let doc = IndexDoc::from_text(
                    label.clone(),
                    if is_html { "markdown" } else { "text" }.to_owned(),
                    &text,
                );
                return match state.index.index_documents(vec![doc]).await {
                    Ok(report) => {
                        state
                            .journal(
                                "index",
                                "ctx_index",
                                Some(url),
                                true,
                                None,
                                bytes,
                                format!(
                                    "fetched + indexed {} chunk(s) from {label}",
                                    report.chunks
                                ),
                            )
                            .await;
                        let value = serde_json::json!({
                            "files": report.files,
                            "chunks": report.chunks,
                            "indexed": report,
                            "queries": [],
                        });
                        ToolResult {
                            content: serde_json::to_string_pretty(&value)
                                .unwrap_or_else(|_| "{}".to_owned()),
                            images: Vec::new(),
                            details: None,
                            structured_content: Some(value),
                            is_error: false,
                            terminate: false,
                        }
                    }
                    Err(error) => ToolResult::error(format!("ctx_index: {error}")),
                };
            }
            let scope_cwd = cwd.clone();
            let built = tokio::task::spawn_blocking(move || {
                documents_for_index(&cwd, content, path, source, content_type)
            })
            .await;
            let docs = match built {
                Ok(Ok(docs)) => docs,
                Ok(Err(message)) => return ToolResult::error(message),
                Err(error) => {
                    return ToolResult::error(format!("ctx_index: indexing task failed: {error}"))
                }
            };
            state.bind_scope(&scope_cwd);
            let label = docs
                .first()
                .map(|doc| doc.source.clone())
                .unwrap_or_else(|| "content".to_owned());
            let bytes: usize = docs
                .iter()
                .flat_map(|doc| doc.chunks.iter())
                .map(|chunk| chunk.content.len())
                .sum();
            // An empty directory is a valid (if useless) index: report zero
            // counts rather than failing.
            match state.index.index_documents(docs).await {
                Ok(report) => {
                    state
                        .journal(
                            "index",
                            "ctx_index",
                            journal_path,
                            true,
                            None,
                            bytes,
                            format!("indexed {} chunk(s) from {label}", report.chunks),
                        )
                        .await;
                    // Counts + the exact ctx_search top-level shape; the
                    // content itself is never echoed. The counts are repeated
                    // at the top level for callers that only look there.
                    let files = report.files;
                    let chunks = report.chunks;
                    let value = serde_json::json!({
                        "files": files,
                        "chunks": chunks,
                        "indexed": report,
                        "queries": [],
                    });
                    ToolResult {
                        content: serde_json::to_string_pretty(&value).unwrap_or_else(|_| {
                            "{\"files\":0,\"chunks\":0,\"indexed\":null,\"queries\":[]}".to_owned()
                        }),
                        images: Vec::new(),
                        details: None,
                        structured_content: Some(value),
                        is_error: false,
                        terminate: false,
                    }
                }
                Err(error) => ToolResult::error(format!("ctx_index: {error}")),
            }
        })
    }
}

pub struct CtxSearchTool {
    state: Arc<super::ContextModeState>,
}

impl CtxSearchTool {
    pub fn new(state: Arc<super::ContextModeState>) -> Self {
        Self { state }
    }
}

impl AgentTool for CtxSearchTool {
    fn name(&self) -> &'static str {
        super::CTX_SEARCH_TOOL_NAME
    }

    fn description(&self) -> &'static str {
        "Search the indexed knowledge base with several queries in one call. Each query returns BM25-ranked sections (title hits weighted 5x, porter stemming so 'caching' matches 'cached') with title, source, a ~500-char snippet and line hints; chunk ids are deduplicated across queries. Default 3 results per query, max 20; at most 32 queries per call, each up to 512 bytes. \
`scope` selects visibility: `session` (this session's writes), `worktree` (the current working tree; default) or `project` (every row in this project's database, ranked by BM25 + recency fusion). \
`offset` pages within a query, `sort:\"timeline\"` returns oldest-first by real time and uses `after` as an epoch-ms cursor. Optional source/contentType filters scope the search. \
The response is the exact shape ctx_index advertises: { queries: [{ query, results: [...] }] }. \
The knowledge base must already contain content — index it first with ctx_index (content, path or url), ctx_fetch_and_index, or a large ctx_execute output with intent. An empty base returns an error telling you to index first. \
Use 2-4 specific technical terms per query and batch related questions here instead of re-reading raw sources; unlike ctx_execute, this searches content stored in previous calls."
    }

    fn prompt_snippet(&self) -> Option<String> {
        Some(
            "Search the indexed knowledge base with several queries in one call and get \
BM25-ranked sections with line hints"
                .to_owned(),
        )
    }

    fn parameters(&self) -> Option<serde_json::Value> {
        Some(serde_json::json!({
            "type": "object",
            "properties": {
                "queries": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "One or more search queries, each with specific technical terms. An empty array lists the source/timeline when `source` or `sort:\"timeline\"` is set."
                },
                "query": {
                    "type": "string",
                    "description": "Single-query shorthand; ignored when queries is non-empty."
                },
                "scope": {
                    "type": "string",
                    "enum": ["session", "worktree", "project"],
                    "description": "Visibility: this session, this worktree (default), or the whole project (BM25 + recency RRF)."
                },
                "source": {
                    "type": "string",
                    "description": "Only return chunks whose source label matches exactly."
                },
                "contentType": {
                    "type": "string",
                    "description": "Only return chunks with this content type (markdown, text, code, json, yaml)."
                },
                "limit": {
                    "type": "number",
                    "description": "Results per query. Default 3, clamped to 1..20."
                },
                "offset": {
                    "type": "number",
                    "description": "Skip this many ranked hits (paging)."
                },
                "sort": {
                    "type": "string",
                    "enum": ["relevance", "timeline"],
                    "description": "relevance (default) or timeline (oldest first; `after` is the epoch-ms cursor)."
                },
                "after": {
                    "type": "number",
                    "description": "Timeline cursor: only rows indexed after this epoch-ms timestamp."
                }
            },
            "additionalProperties": false
        }))
    }

    fn has_external_side_effects(&self) -> bool {
        false
    }

    fn run(&self, args: &serde_json::Value, ctx: &ToolContext) -> BoxFuture<'static, ToolResult> {
        let state = Arc::clone(&self.state);
        let mut queries: Vec<String> = args
            .get("queries")
            .and_then(serde_json::Value::as_array)
            .map(|queries| {
                queries
                    .iter()
                    .filter_map(serde_json::Value::as_str)
                    .map(str::trim)
                    .filter(|query| !query.is_empty())
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default();
        if queries.is_empty() {
            if let Some(query) = args
                .get("query")
                .and_then(serde_json::Value::as_str)
                .map(str::trim)
                .filter(|query| !query.is_empty())
            {
                queries.push(query.to_owned());
            }
        }
        let source = args
            .get("source")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|source| !source.is_empty())
            .map(str::to_owned);
        let content_type = args
            .get("contentType")
            .and_then(serde_json::Value::as_str)
            .map(|content_type| content_type.trim().to_ascii_lowercase())
            .filter(|content_type| !content_type.is_empty());
        let limit = args
            .get("limit")
            .and_then(serde_json::Value::as_u64)
            .map(|limit| (limit as usize).clamp(1, MAX_SEARCH_LIMIT))
            .unwrap_or(DEFAULT_SEARCH_LIMIT);
        let scope = SearchScope::parse(args.get("scope").and_then(serde_json::Value::as_str));
        let mut sort = SearchSort::parse(args.get("sort").and_then(serde_json::Value::as_str));
        let offset = args
            .get("offset")
            .and_then(serde_json::Value::as_u64)
            .map(|offset| offset as usize)
            .unwrap_or(0);
        let after = args
            .get("after")
            .and_then(serde_json::Value::as_i64)
            .filter(|after| *after >= 0);
        // An empty query set is the "list this source / this timeline" form:
        // the scope stays useful, the ordering is by time, and BM25 is skipped.
        if queries.is_empty() && (source.is_some() || sort == SearchSort::Timeline) {
            queries.push(String::new());
            sort = SearchSort::Timeline;
        }
        let worktree = ctx
            .cwd
            .canonicalize()
            .unwrap_or_else(|_| ctx.cwd.clone())
            .to_string_lossy()
            .into_owned();
        let session_id = state.hooks.session_id();

        Box::pin(async move {
            if queries.is_empty() {
                return ToolResult::error(
                    "ctx_search: provide `queries` (an array of strings) or `query` (a single string)",
                );
            }
            if queries.len() > MAX_SEARCH_QUERIES {
                return ToolResult::error(format!(
                    "ctx_search: at most {MAX_SEARCH_QUERIES} queries are allowed per call (got {})",
                    queries.len()
                ));
            }
            if let Some((index, query)) = queries
                .iter()
                .enumerate()
                .filter(|(_, query)| !query.is_empty())
                .find(|(_, query)| query.len() > MAX_QUERY_BYTES)
            {
                return ToolResult::error(format!(
                    "ctx_search: query #{} is {} bytes; each query is limited to {MAX_QUERY_BYTES} bytes",
                    index + 1,
                    query.len()
                ));
            }
            match state
                .index
                .search_scoped(ScopedSearch {
                    queries,
                    source,
                    content_type,
                    limit,
                    offset,
                    scope,
                    sort,
                    after,
                    session_id,
                    worktree: Some(worktree),
                })
                .await
            {
                Ok(Some(response)) => {
                    let value = serde_json::to_value(&response)
                        .unwrap_or(serde_json::Value::Null);
                    ToolResult {
                        content: serde_json::to_string_pretty(&value)
                            .unwrap_or_else(|_| "{\"queries\":[]}".to_owned()),
                        images: Vec::new(),
                        details: None,
                        structured_content: Some(value),
                        is_error: false,
                        terminate: false,
                    }
                }
                Ok(None) => ToolResult::error(
                    "ctx_search: the knowledge base is empty — index content first with ctx_index (content or path), ctx_fetch_and_index, or ctx_execute with intent, then search again",
                ),
                Err(error) => ToolResult::error(format!("ctx_search: {error}")),
            }
        })
    }
}

pub struct CtxStatsTool {
    state: Arc<super::ContextModeState>,
}

impl CtxStatsTool {
    pub fn new(state: Arc<super::ContextModeState>) -> Self {
        Self { state }
    }
}

impl AgentTool for CtxStatsTool {
    fn name(&self) -> &'static str {
        super::CTX_STATS_TOOL_NAME
    }

    fn description(&self) -> &'static str {
        "Report this process's context-mode counters and the knowledge-base state: executions (ctx_execute, ctx_execute_file, and each ctx_batch_execute/ctx_batch command), bytes read in, bytes returned to context, auto-indexed outputs, journal usage for this session and for the whole project/all-time, chunk/source counts, the SQLite database path and its size. Read-only diagnostics; use it to see what is stored before purging or to check how much context the sandbox has saved."
    }

    fn prompt_snippet(&self) -> Option<String> {
        Some(
            "Report context-mode counters and knowledge-base state: bytes kept out of context, \
chunk/source counts, database size"
                .to_owned(),
        )
    }

    fn parameters(&self) -> Option<serde_json::Value> {
        Some(serde_json::json!({
            "type": "object",
            "properties": {},
            "additionalProperties": false
        }))
    }

    fn has_external_side_effects(&self) -> bool {
        false
    }

    fn run(&self, _args: &serde_json::Value, _ctx: &ToolContext) -> BoxFuture<'static, ToolResult> {
        let state = Arc::clone(&self.state);
        let session = state.hooks.session_id();
        Box::pin(async move {
            let process = state.stats.snapshot();
            let db = match state.index.stats().await {
                Ok(db) => db,
                Err(error) => return ToolResult::error(format!("ctx_stats: {error}")),
            };
            let project = match state.index.event_usage(None).await {
                Ok(usage) => usage,
                Err(error) => return ToolResult::error(format!("ctx_stats: {error}")),
            };
            let session_usage = match session.clone() {
                Some(session) => match state.index.event_usage(Some(session)).await {
                    Ok(usage) => usage,
                    Err(error) => return ToolResult::error(format!("ctx_stats: {error}")),
                },
                None => super::index::EventUsage {
                    calls: 0,
                    bytes: 0,
                    by_kind: Vec::new(),
                },
            };
            // The session's retention state proves the store is bounded: it
            // reports what is stored now against the active caps.
            let retention = match session.clone() {
                Some(session) => match state.index.retention(session).await {
                    Ok(report) => serde_json::to_value(&report).unwrap_or(serde_json::Value::Null),
                    Err(error) => {
                        return ToolResult::error(format!("ctx_stats: {error}"));
                    }
                },
                None => serde_json::Value::Null,
            };
            let value = serde_json::json!({
                "executions": process.executions,
                "fileExecutions": process.file_executions,
                "bytesIn": process.bytes_in,
                "bytesOut": process.bytes_out,
                "autoIndexes": process.auto_indexes,
                "usage": {
                    "session": session_usage,
                    "project": project,
                    "allTime": project,
                },
                "retention": {
                    "limits": state.index.retention_limits(),
                    "session": retention,
                },
                "chunks": db.chunks,
                "sources": db.sources,
                "sourceList": db.source_list,
                "dbPath": db.db_path,
                "dbSizeBytes": db.db_size_bytes,
            });
            ToolResult {
                content: serde_json::to_string_pretty(&value).unwrap_or_else(|_| "{}".to_owned()),
                images: Vec::new(),
                details: None,
                structured_content: Some(value),
                is_error: false,
                terminate: false,
            }
        })
    }
}

pub struct CtxPurgeTool {
    state: Arc<super::ContextModeState>,
}

impl CtxPurgeTool {
    pub fn new(state: Arc<super::ContextModeState>) -> Self {
        Self { state }
    }
}

impl AgentTool for CtxPurgeTool {
    fn name(&self) -> &'static str {
        super::CTX_PURGE_TOOL_NAME
    }

    fn description(&self) -> &'static str {
        "DESTRUCTIVE: permanently delete indexed data. Requires confirm: true; false or absent returns 'purge cancelled' and changes nothing. Scopes (supply at most one): `source` deletes that source's chunks and fetch cache; `session` deletes that session's journal rows (so ctx_resume no longer sees it); `project: true` wipes the whole project knowledge base (chunks, sources, fetch cache and journal). With no scope the whole knowledge base's chunks are removed. Returns the number of rows removed. Use only when the user explicitly asks to clear or reset the index; run ctx_stats first to preview what exists."
    }

    fn prompt_snippet(&self) -> Option<String> {
        Some(
            "Permanently delete indexed data (a source, this session's journal, or the whole \
project base); destructive, needs confirm: true"
                .to_owned(),
        )
    }

    fn parameters(&self) -> Option<serde_json::Value> {
        Some(serde_json::json!({
            "type": "object",
            "properties": {
                "confirm": {
                    "type": "boolean",
                    "description": "Must be true. Destructive operation; false or absent returns 'purge cancelled'."
                },
                "source": {
                    "type": "string",
                    "description": "Delete only this source's chunks and fetch cache."
                },
                "session": {
                    "type": "string",
                    "description": "Delete only this session's journal rows."
                },
                "project": {
                    "type": "boolean",
                    "description": "true wipes the whole project knowledge base (chunks, sources, fetch cache, journal)."
                }
            },
            "additionalProperties": false
        }))
    }

    fn has_external_side_effects(&self) -> bool {
        true
    }

    fn run(&self, args: &serde_json::Value, ctx: &ToolContext) -> BoxFuture<'static, ToolResult> {
        let state = Arc::clone(&self.state);
        let confirm = args
            .get("confirm")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);
        // Destructive scopes: a present-but-null, blank, or non-string value
        // must never silently widen a scoped purge into a global one. Only
        // `project: true` counts as the project scope; `project: false` asks
        // for no project purge (upstream).
        let source = match args.get("source") {
            None => None,
            Some(value) => match value
                .as_str()
                .map(str::trim)
                .filter(|source| !source.is_empty())
            {
                Some(source) => Some(source.to_owned()),
                None => {
                    return Box::pin(async {
                        ToolResult::error(
                            "ctx_purge: the `source` parameter must be a non-empty string when provided",
                        )
                    });
                }
            },
        };
        let session = match args.get("session") {
            None => None,
            Some(value) => match value
                .as_str()
                .map(str::trim)
                .filter(|session| !session.is_empty())
            {
                Some(session) => Some(session.to_owned()),
                None => {
                    return Box::pin(async {
                        ToolResult::error(
                            "ctx_purge: the `session` parameter must be a non-empty string when provided",
                        )
                    });
                }
            },
        };
        let project = args
            .get("project")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);
        let scopes =
            usize::from(source.is_some()) + usize::from(session.is_some()) + usize::from(project);
        if scopes > 1 {
            return Box::pin(async {
                ToolResult::error(
                    "ctx_purge: supply at most one scope (`source`, `session`, or `project: true`)",
                )
            });
        }
        let _ = ctx;

        Box::pin(async move {
            if !confirm {
                return ToolResult::ok("purge cancelled");
            }
            let value = if project {
                match state.index.purge_project().await {
                    Ok((chunks, events)) => serde_json::json!({
                        "purged": chunks,
                        "purgedEvents": events,
                        "project": true,
                    }),
                    Err(error) => return ToolResult::error(format!("ctx_purge: {error}")),
                }
            } else if let Some(session) = session {
                match state.index.purge_session(session.clone()).await {
                    Ok(events) => serde_json::json!({
                        "purgedEvents": events,
                        "session": session,
                    }),
                    Err(error) => return ToolResult::error(format!("ctx_purge: {error}")),
                }
            } else {
                let scope = source.clone();
                match state.index.purge(source).await {
                    Ok(rows) => serde_json::json!({ "purged": rows, "source": scope }),
                    Err(error) => return ToolResult::error(format!("ctx_purge: {error}")),
                }
            };
            ToolResult {
                content: serde_json::to_string_pretty(&value).unwrap_or_else(|_| "{}".to_owned()),
                images: Vec::new(),
                details: None,
                structured_content: Some(value),
                is_error: false,
                terminate: false,
            }
        })
    }
}

// ---------------------------------------------------------------------------
// ctx_doctor
// ---------------------------------------------------------------------------

pub struct CtxDoctorTool {
    state: Arc<super::ContextModeState>,
}

impl CtxDoctorTool {
    pub fn new(state: Arc<super::ContextModeState>) -> Self {
        Self { state }
    }
}

impl AgentTool for CtxDoctorTool {
    fn name(&self) -> &'static str {
        super::CTX_DOCTOR_TOOL_NAME
    }

    fn description(&self) -> &'static str {
        "Report local context-mode diagnostics without touching any service: platform, embedded JavaScript sandbox status (and worker spawn count), the knowledge-base database path/size/WAL size, FTS5 availability and schema version, chunk/source counts, and the hook settings (routing/auto-index on/off plus thresholds) with routing/index counters and the last hook error. Read-only: it never starts the JS worker and never creates the database; an absent knowledge base reads as zero chunks with exists=false. Use it instead of the upstream npm/cloud-only ctx_upgrade/ctx_insight tools to verify the local install and schema."
    }

    fn prompt_snippet(&self) -> Option<String> {
        Some(
            "Report local context-mode diagnostics: sandbox status, FTS5/schema version, \
database size, hook settings and last error"
                .to_owned(),
        )
    }

    fn parameters(&self) -> Option<serde_json::Value> {
        Some(serde_json::json!({
            "type": "object",
            "properties": {},
            "additionalProperties": false
        }))
    }

    fn has_external_side_effects(&self) -> bool {
        false
    }

    fn run(&self, _args: &serde_json::Value, _ctx: &ToolContext) -> BoxFuture<'static, ToolResult> {
        let state = Arc::clone(&self.state);
        Box::pin(async move {
            let js_started = state.js.has_worker();
            let spawn_count = state.js.spawn_count();
            let executions = state.stats.snapshot();
            let database = match state.index.diagnostics().await {
                Ok(db) => serde_json::json!({
                    "path": db.path,
                    "exists": db.exists,
                    "open": db.open,
                    "sizeBytes": db.size_bytes,
                    "walBytes": db.wal_bytes,
                    "fts5Available": db.fts5_available,
                    "schemaVersion": db.schema_version,
                    "chunks": db.chunks,
                    "sources": db.sources,
                }),
                Err(error) => {
                    let message = error.to_string();
                    state.hooks.record_error(format!("ctx_doctor: {message}"));
                    serde_json::json!({
                        "path": state.index.path().to_string_lossy(),
                        "exists": std::fs::metadata(state.index.path())
                            .map(|meta| meta.is_file())
                            .unwrap_or(false),
                        "open": state.index.is_open(),
                        "error": message,
                    })
                }
            };
            let hooks = state.hooks.snapshot();
            // Retention: the active caps plus this session's stored rows. A
            // session that already exceeds a cap (written by an older build)
            // reads `overLimit: true` until its next write trims it.
            let retention = match hooks.session_id.clone() {
                Some(session) => match state.index.retention(session).await {
                    Ok(report) => serde_json::json!({
                        "limits": state.index.retention_limits(),
                        "session": report,
                    }),
                    Err(error) => serde_json::json!({
                        "limits": state.index.retention_limits(),
                        "error": error.to_string(),
                    }),
                },
                None => serde_json::json!({
                    "limits": state.index.retention_limits(),
                    "session": serde_json::Value::Null,
                }),
            };
            let value = serde_json::json!({
                "platform": {
                    "os": std::env::consts::OS,
                    "arch": std::env::consts::ARCH,
                    "family": std::env::consts::FAMILY,
                },
                "sandbox": {
                    "available": true,
                    "engine": "embedded QuickJS + node compatibility suite",
                    "languages": ["javascript", "typescript"],
                    "status": if js_started { "running" } else { "not started" },
                    "workerStarted": js_started,
                    "spawnCount": spawn_count,
                },
                "database": database,
                "retention": retention,
                "hooks": {
                    "routingEnabled": super::routing_enabled(),
                    "indexResultsEnabled": super::result_indexing_enabled(),
                    "fileRoutingThresholdBytes": super::ROUTING_FILE_THRESHOLD_BYTES,
                    "toolResultIndexThresholdBytes": super::TOOL_RESULT_INDEX_THRESHOLD_BYTES,
                    "toolResultIndexMaxBytes": super::TOOL_RESULT_INDEX_MAX_BYTES,
                },
                "session": {
                    "id": hooks.session_id,
                    "sourcePrefix": "session:",
                },
                "counters": {
                    "routingBlocks": hooks.routing_blocks,
                    "indexedResults": hooks.indexed_results,
                    "hookErrors": hooks.hook_errors,
                    "lastError": hooks.last_error,
                },
                "executions": {
                    "executions": executions.executions,
                    "fileExecutions": executions.file_executions,
                    "bytesIn": executions.bytes_in,
                    "bytesOut": executions.bytes_out,
                    "autoIndexes": executions.auto_indexes,
                },
            });
            ToolResult {
                content: serde_json::to_string_pretty(&value).unwrap_or_else(|_| "{}".to_owned()),
                images: Vec::new(),
                details: None,
                structured_content: Some(value),
                is_error: false,
                terminate: false,
            }
        })
    }
}
