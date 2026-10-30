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

use std::collections::HashSet;
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
pub(crate) const MAX_FILE_BYTES: u64 = 32 * 1024 * 1024;

/// Current on-disk schema version (`PRAGMA user_version`). Bumped when the
/// table layout changes; `ctx_doctor` reports it and a future migration can
/// branch on it.
pub(crate) const SCHEMA_VERSION: i64 = 1;

/// Upper bound on the number of queries in one `ctx_search` call.
pub(crate) const MAX_SEARCH_QUERIES: usize = 32;

/// Upper bound on one query's length (bytes), so a call cannot force the
/// FTS expression builder / SQL statement to process arbitrary payloads.
pub(crate) const MAX_QUERY_BYTES: usize = 512;

/// Files larger than this are skipped by the directory walk.
const MAX_INDEX_FILE_BYTES: u64 = MAX_FILE_BYTES;

/// Target size for a plain-text chunk (the split happens at a blank line at
/// or after this many bytes).
const PLAIN_CHUNK_TARGET_BYTES: usize = 2 * 1024;

/// Markdown sections larger than this are split at blank lines outside
/// fences (the heading title is carried into each part).
const MARKDOWN_CHUNK_TARGET_BYTES: usize = 4 * 1024;

/// Search defaults.
pub(crate) const DEFAULT_SEARCH_LIMIT: usize = 3;
pub(crate) const MAX_SEARCH_LIMIT: usize = 20;

/// Snippet window around the first matching term.
pub(crate) const SNIPPET_CHARS: usize = 500;

/// `ctx_fetch_and_index` cache defaults.
pub(crate) const DEFAULT_FETCH_TTL_SECS: u64 = 24 * 60 * 60;

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

/// One parsed chunk (title + content + 1-based line range).
#[derive(Debug, Clone)]
pub(crate) struct ParsedChunk {
    pub title: String,
    pub content: String,
    pub line_start: usize,
    pub line_end: usize,
}

/// A document destined for the index: source label, content type, chunks.
#[derive(Debug, Clone)]
pub(crate) struct IndexDoc {
    pub source: String,
    pub content_type: String,
    pub chunks: Vec<ParsedChunk>,
}

impl IndexDoc {
    /// Chunk `text` for `source` and wrap it as a document.
    pub fn from_text(source: String, content_type: String, text: &str) -> Self {
        let chunks = chunk_document(&source, &content_type, text);
        Self {
            source,
            content_type,
            chunks,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SourceSummary {
    pub source: String,
    pub content_type: String,
    pub chunks: usize,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct IndexReport {
    pub files: usize,
    pub chunks: usize,
    pub sources: Vec<SourceSummary>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SearchHit {
    pub id: i64,
    pub title: String,
    pub source: String,
    pub content_type: String,
    pub snippet: String,
    pub line_start: i64,
    pub line_end: i64,
    pub score: f64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct QueryResult {
    pub query: String,
    pub results: Vec<SearchHit>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SearchResponse {
    pub queries: Vec<QueryResult>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct DbStats {
    pub chunks: usize,
    pub sources: usize,
    pub source_list: Vec<SourceSummary>,
    pub db_path: String,
    pub db_size_bytes: u64,
}

#[derive(Debug, Clone)]
pub(crate) struct CachedFetch {
    pub body: String,
    pub content_type: String,
}

/// Read-only database state reported by `ctx_doctor`. Produced without
/// creating the database file: an absent base reports `exists: false` and
/// zero counters, and FTS5 availability is probed in memory.
#[derive(Debug, Clone)]
pub(crate) struct IndexDiagnostics {
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
pub(crate) struct IndexStore {
    explicit_path: Option<PathBuf>,
    /// Base directory override for the default path (hermetic tests: never
    /// write to the real agent dir). `None` = the shared agent dir.
    base_dir: Option<PathBuf>,
    /// Session cwd pushed by the host, preferred over the process cwd.
    session_cwd: Mutex<Option<PathBuf>>,
    /// Last resolved default path plus the `(cwd, base)` it came from.
    /// Single-entry on purpose: bounded, and the previous path is simply
    /// dropped when the inputs change.
    resolved: Mutex<Option<ResolvedPath>>,
    /// The open connection and the path it was opened on, so a resolved-path
    /// change reopens against the new database instead of silently keeping
    /// the old one.
    connection: Mutex<Option<OpenConnection>>,
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
            resolved: Mutex::new(None),
            connection: Mutex::new(None),
        }
    }

    /// A store at an explicit path (hermetic tests, embedders).
    pub fn with_path(path: PathBuf) -> Self {
        Self {
            explicit_path: Some(path),
            base_dir: None,
            session_cwd: Mutex::new(None),
            resolved: Mutex::new(None),
            connection: Mutex::new(None),
        }
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
    pub async fn index_documents(
        self: &Arc<Self>,
        docs: Vec<IndexDoc>,
    ) -> anyhow::Result<IndexReport> {
        self.call(move |connection| {
            let transaction = connection.transaction()?;
            let mut report = IndexReport {
                files: docs.len(),
                chunks: 0,
                sources: Vec::new(),
            };
            for doc in &docs {
                let inserted = replace_source(&transaction, doc)?;
                report.chunks += inserted;
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
                let Some((expression, terms)) = build_match_expression(query) else {
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
                    let rank: f64 = row.get(7)?;
                    let (line_start, line_end): (i64, i64) = (row.get(4)?, row.get(5)?);
                    results.push(SearchHit {
                        id,
                        title: row.get(1)?,
                        source: row.get(2)?,
                        content_type: row.get(3)?,
                        snippet: make_snippet(&content, &terms, SNIPPET_CHARS),
                        line_start,
                        line_end,
                        // FTS5 bm25() is smaller-is-better and negative for
                        // matches; flip it so higher is better for the model.
                        score: -rank,
                    });
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

/// `bm25(fts, 5.0, 1.0)`: a title hit is worth five content hits.
const SEARCH_SQL: &str = "\
SELECT c.id, c.title, c.source, c.content_type, c.line_start, c.line_end, c.content, \
       bm25(chunks_fts, 5.0, 1.0) AS rank \
FROM chunks_fts \
JOIN chunks c ON c.id = chunks_fts.rowid \
WHERE chunks_fts MATCH ?1 \
  AND (?2 IS NULL OR c.source = ?2) \
  AND (?3 IS NULL OR c.content_type = ?3) \
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
             indexed_at INTEGER NOT NULL\
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
         );",
    )?;
    // Stamp the schema version into the database header. Existing databases
    // built before versioning read 0 and are current by construction (the
    // CREATE IF NOT EXISTS block above is the whole schema), so they are
    // stamped on the next open.
    let version: i64 = connection.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if version < SCHEMA_VERSION {
        connection.execute_batch(&format!("PRAGMA user_version = {SCHEMA_VERSION};"))?;
    }
    Ok(())
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
) -> rusqlite::Result<usize> {
    // FTS rows are keyed by chunks.id; delete them first while the metadata
    // rows still identify the ids.
    transaction.execute(
        "DELETE FROM chunks_fts WHERE rowid IN (SELECT id FROM chunks WHERE source = ?1)",
        [&doc.source],
    )?;
    transaction.execute("DELETE FROM chunks WHERE source = ?1", [&doc.source])?;
    let indexed_at = crate::ai::util::now_ms() as i64;
    let mut insert_chunk = transaction.prepare_cached(
        "INSERT INTO chunks \
         (source, content_type, title, content, line_start, line_end, indexed_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
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
        ])?;
        let id = transaction.last_insert_rowid();
        insert_fts.execute(params![id, chunk.title, chunk.content])?;
    }
    Ok(doc.chunks.len())
}

// ---------------------------------------------------------------------------
// Default DB path
// ---------------------------------------------------------------------------

/// Stable FNV-1a 64-bit hash of the canonical project path (hex, 16 chars).
/// Version-independent on purpose: a Rust `DefaultHasher` is not stable across
/// toolchains, and the DB file name must survive upgrades.
pub(crate) fn project_hash(path: &Path) -> String {
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
// Chunking
// ---------------------------------------------------------------------------

/// Split `content` according to `content_type` (`markdown` by headings,
/// everything else by blank lines/size).
pub(crate) fn chunk_document(source: &str, content_type: &str, content: &str) -> Vec<ParsedChunk> {
    if content_type.eq_ignore_ascii_case("markdown") {
        chunk_markdown(content, source)
    } else {
        chunk_plain(content, source)
    }
}

/// One open section while streaming lines.
struct Section {
    title: String,
    part: usize,
    start_line: usize,
    last_line: usize,
    content: String,
}

impl Section {
    fn new(title: String) -> Self {
        Self {
            title,
            part: 0,
            start_line: 0,
            last_line: 0,
            content: String::new(),
        }
    }

    fn push_line(&mut self, line: &str, line_no: usize) {
        if self.content.is_empty() {
            self.start_line = line_no;
        }
        self.content.push_str(line);
        self.content.push('\n');
        self.last_line = line_no;
    }

    /// Emit this section (if it has any non-whitespace content) and start the
    /// next part under the same heading.
    fn flush(&mut self, chunks: &mut Vec<ParsedChunk>) {
        if self.content.trim().is_empty() {
            self.content.clear();
            self.start_line = 0;
            self.last_line = 0;
            return;
        }
        let title = if self.part == 0 {
            self.title.clone()
        } else {
            format!("{} (part {})", self.title, self.part + 1)
        };
        chunks.push(ParsedChunk {
            title,
            content: std::mem::take(&mut self.content),
            line_start: self.start_line,
            line_end: self.last_line,
        });
        self.part += 1;
        self.start_line = 0;
        self.last_line = 0;
    }
}

/// ATX heading outside a fence: `(level, title)`.
fn heading(line: &str) -> Option<(usize, &str)> {
    let indent = line.len() - line.trim_start_matches(' ').len();
    if indent > 3 {
        return None;
    }
    let rest = &line[indent..];
    let level = rest.chars().take_while(|ch| *ch == '#').count();
    if !(1..=6).contains(&level) {
        return None;
    }
    let after = &rest[level..];
    if after.is_empty() {
        return Some((level, ""));
    }
    if !after.starts_with(' ') {
        return None;
    }
    Some((level, after.trim().trim_end_matches('#').trim()))
}

/// Fence marker (` ``` ` / `~~~`) at line start, if any.
fn fence_marker(line: &str) -> Option<&'static str> {
    let trimmed = line.trim_start();
    if trimmed.starts_with("```") {
        Some("```")
    } else if trimmed.starts_with("~~~") {
        Some("~~~")
    } else {
        None
    }
}

fn strip_line_ending(raw: &str) -> &str {
    let line = raw.strip_suffix('\n').unwrap_or(raw);
    line.strip_suffix('\r').unwrap_or(line)
}

fn chunk_markdown(content: &str, title_hint: &str) -> Vec<ParsedChunk> {
    let mut chunks = Vec::new();
    let fallback = if title_hint.trim().is_empty() {
        "Introduction".to_owned()
    } else {
        title_hint.to_owned()
    };
    let mut section = Section::new(fallback);
    let mut fence: Option<&'static str> = None;
    let mut line_no = 0usize;

    for raw in content.split_inclusive('\n') {
        line_no += 1;
        let line = strip_line_ending(raw);
        if fence.is_none() {
            if let Some((_level, heading_title)) = heading(line) {
                section.flush(&mut chunks);
                let title = if heading_title.is_empty() {
                    title_hint.to_owned()
                } else {
                    heading_title.to_owned()
                };
                section = Section::new(title);
            }
        }
        section.push_line(line, line_no);
        match (fence, fence_marker(line)) {
            (None, Some(marker)) => fence = Some(marker),
            (Some(marker), Some(_)) if line.trim_start().starts_with(marker) => fence = None,
            _ => {}
        }
        // Split an oversized section at a blank line OUTSIDE a fence: code
        // blocks are never cut in half.
        if fence.is_none()
            && line.trim().is_empty()
            && section.content.len() >= MARKDOWN_CHUNK_TARGET_BYTES
        {
            section.flush(&mut chunks);
        }
    }
    section.flush(&mut chunks);
    chunks
}

fn chunk_plain(content: &str, title_hint: &str) -> Vec<ParsedChunk> {
    let mut chunks = Vec::new();
    let title = content
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(|line| truncate_chars(line, 80))
        .filter(|line| !line.is_empty())
        .unwrap_or_else(|| title_hint.to_owned());
    let mut section = Section::new(title);
    let mut fence: Option<&'static str> = None;
    let mut line_no = 0usize;

    for raw in content.split_inclusive('\n') {
        line_no += 1;
        let line = strip_line_ending(raw);
        section.push_line(line, line_no);
        match (fence, fence_marker(line)) {
            (None, Some(marker)) => fence = Some(marker),
            (Some(marker), Some(_)) if line.trim_start().starts_with(marker) => fence = None,
            _ => {}
        }
        if fence.is_none()
            && line.trim().is_empty()
            && section.content.len() >= PLAIN_CHUNK_TARGET_BYTES
        {
            section.flush(&mut chunks);
        }
    }
    section.flush(&mut chunks);
    chunks
}

fn truncate_chars(text: &str, max: usize) -> String {
    match text.char_indices().nth(max) {
        Some((byte_index, _)) => text[..byte_index].trim_end().to_owned(),
        None => text.to_owned(),
    }
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
fn build_match_expression(query: &str) -> Option<(String, Vec<String>)> {
    let mut terms = Vec::new();
    let mut current = String::new();
    for ch in query.chars() {
        if ch.is_alphanumeric() || ch == '_' {
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
    Some((expression, terms))
}

// ---------------------------------------------------------------------------
// Directory walk (`ctx_index(path: <dir>)`)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub(crate) struct WalkedFile {
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
pub(crate) struct WalkStats {
    /// Number of directories whose entries were listed.
    pub directories: usize,
    /// Number of directory entries read (bounded by
    /// [`WALK_MAX_ENTRIES_PER_DIR`] per directory).
    pub entries: usize,
}

/// Bounded, symlink-free walk with the fixed extension allow-list and the
/// built-in directory skip list. `.gitignore` at the walk root is applied
/// (simple globs; cheap, no git invocation).
pub(crate) fn walk_directory(root: &Path) -> Result<Vec<WalkedFile>, String> {
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
pub(crate) fn walk_directory_probe(
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
pub(crate) fn content_type_for_extension(extension: &str) -> &'static str {
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
pub(crate) fn documents_for_index(
    cwd: &Path,
    content: Option<String>,
    path: Option<String>,
    source: Option<String>,
    content_type: Option<String>,
) -> Result<Vec<IndexDoc>, String> {
    let content_type = content_type
        .filter(|value| !value.trim().is_empty())
        .map(|value| value.trim().to_ascii_lowercase());
    if let Some(content) = content {
        let source = source
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| "inline".to_owned());
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
        let base = source
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| path.trim_end_matches(['/', '\\']).to_owned());
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
    let source = source
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| path.clone());
    let text = String::from_utf8_lossy(&bytes);
    Ok(vec![IndexDoc::from_text(source, content_type, &text)])
}

pub(crate) struct CtxIndexTool {
    state: Arc<super::ContextModeState>,
}

impl CtxIndexTool {
    pub(crate) fn new(state: Arc<super::ContextModeState>) -> Self {
        Self { state }
    }
}

impl AgentTool for CtxIndexTool {
    fn name(&self) -> &'static str {
        super::CTX_INDEX_TOOL_NAME
    }

    fn description(&self) -> &'static str {
        "Store content in the local FTS5 knowledge base (BM25 with title weighting and porter stemming). Markdown is split by headings with fenced code blocks kept intact; other text is split into ~2 KiB chunks. The raw content stays in storage and is never echoed back. \
Provide exactly one of content or path. A path must resolve inside the session working directory (`..`, absolute outside paths and symlink escapes are rejected) and must be a regular file or directory with an allowed text/code extension (md, mdx, txt, json, yaml, yml, ts, tsx, js, jsx, py, rs, go, sh). \
Directories are walked with bounded defaults (depth 5, 200 files, 4096 entries per directory, 2048 directories, node_modules/.git/dist/build/.next/coverage/.venv/__pycache__ skipped, no symlinks, root .gitignore respected). \
Returns indexing metadata (file/chunk/source counts) and the ctx_search result shape, not the content. Query it later with ctx_search, optionally scoped by source. \
Use this for docs, specs, READMEs and API references you will query across turns. For one-off analysis of logs or command output, use ctx_execute_file instead so the bytes are not persisted."
    }

    fn parameters(&self) -> Option<serde_json::Value> {
        Some(serde_json::json!({
            "type": "object",
            "properties": {
                "content": {
                    "type": "string",
                    "description": "Text/markdown to index. Provide this OR path, not both."
                },
                "path": {
                    "type": "string",
                    "description": "File or directory to read and index (content never enters context). Provide this OR content."
                },
                "source": {
                    "type": "string",
                    "description": "Label for the indexed content (e.g. 'react-docs'). Defaults to 'inline' for content and to the path for files."
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
        let cwd = ctx.cwd.clone();

        Box::pin(async move {
            if content.is_some() == path.is_some() {
                return ToolResult::error("ctx_index: provide exactly one of `content` or `path`");
            }
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
            // An empty directory is a valid (if useless) index: report zero
            // counts rather than failing.
            match state.index.index_documents(docs).await {
                Ok(report) => {
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

pub(crate) struct CtxSearchTool {
    state: Arc<super::ContextModeState>,
}

impl CtxSearchTool {
    pub(crate) fn new(state: Arc<super::ContextModeState>) -> Self {
        Self { state }
    }
}

impl AgentTool for CtxSearchTool {
    fn name(&self) -> &'static str {
        super::CTX_SEARCH_TOOL_NAME
    }

    fn description(&self) -> &'static str {
        "Search the indexed knowledge base with several queries in one call. Each query returns BM25-ranked sections (title hits weighted 5x, porter stemming so 'caching' matches 'cached') with title, source, a ~500-char snippet and line hints; chunk ids are deduplicated across queries. Default 3 results per query, max 20; at most 32 queries per call, each up to 512 bytes. Optional source/contentType filters scope the search. \
The response is the exact shape ctx_index advertises: { queries: [{ query, results: [...] }] }. \
The knowledge base must already contain content — index it first with ctx_index (content or path), ctx_fetch_and_index, or a large ctx_execute output with intent. An empty base returns an error telling you to index first. \
Use 2-4 specific technical terms per query and batch related questions here instead of re-reading raw sources; unlike ctx_execute, this searches content stored in previous calls."
    }

    fn parameters(&self) -> Option<serde_json::Value> {
        Some(serde_json::json!({
            "type": "object",
            "properties": {
                "queries": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "One or more search queries, each with specific technical terms."
                },
                "query": {
                    "type": "string",
                    "description": "Single-query shorthand; ignored when queries is non-empty."
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
            .map(str::trim)
            .filter(|content_type| !content_type.is_empty())
            .map(str::to_owned);
        let limit = args
            .get("limit")
            .and_then(serde_json::Value::as_u64)
            .map(|limit| (limit as usize).clamp(1, MAX_SEARCH_LIMIT))
            .unwrap_or(DEFAULT_SEARCH_LIMIT);
        let _ = ctx;

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
                .search(queries, source, content_type, limit)
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

pub(crate) struct CtxStatsTool {
    state: Arc<super::ContextModeState>,
}

impl CtxStatsTool {
    pub(crate) fn new(state: Arc<super::ContextModeState>) -> Self {
        Self { state }
    }
}

impl AgentTool for CtxStatsTool {
    fn name(&self) -> &'static str {
        super::CTX_STATS_TOOL_NAME
    }

    fn description(&self) -> &'static str {
        "Report this process's context-mode counters and the knowledge-base state: executions (ctx_execute, ctx_execute_file, and each ctx_batch_execute script), bytes read in, bytes returned to context, auto-indexed outputs, chunk/source counts, the SQLite database path and its size. Read-only diagnostics; use it to see what is stored before purging or to check how much context the sandbox has saved."
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
            let process = state.stats.snapshot();
            match state.index.stats().await {
                Ok(db) => {
                    let value = serde_json::json!({
                        "executions": process.executions,
                        "fileExecutions": process.file_executions,
                        "bytesIn": process.bytes_in,
                        "bytesOut": process.bytes_out,
                        "autoIndexes": process.auto_indexes,
                        "chunks": db.chunks,
                        "sources": db.sources,
                        "sourceList": db.source_list,
                        "dbPath": db.db_path,
                        "dbSizeBytes": db.db_size_bytes,
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
                Err(error) => ToolResult::error(format!("ctx_stats: {error}")),
            }
        })
    }
}

pub(crate) struct CtxPurgeTool {
    state: Arc<super::ContextModeState>,
}

impl CtxPurgeTool {
    pub(crate) fn new(state: Arc<super::ContextModeState>) -> Self {
        Self { state }
    }
}

impl AgentTool for CtxPurgeTool {
    fn name(&self) -> &'static str {
        super::CTX_PURGE_TOOL_NAME
    }

    fn description(&self) -> &'static str {
        "DESTRUCTIVE: permanently delete indexed chunks from the local knowledge base. Requires confirm: true; false or absent returns 'purge cancelled' and changes nothing. With source, deletes only that source's chunks (and its fetch cache entry); without source, wipes the whole knowledge base. Returns the number of rows removed. Use only when the user explicitly asks to clear or reset the index; run ctx_stats first to preview what exists."
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
                    "description": "Delete only this source's chunks. Omit to wipe the whole knowledge base."
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
        let source = args
            .get("source")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|source| !source.is_empty())
            .map(str::to_owned);
        let _ = ctx;

        Box::pin(async move {
            if !confirm {
                return ToolResult::ok("purge cancelled");
            }
            let scope = source.clone();
            match state.index.purge(source).await {
                Ok(rows) => {
                    let value = serde_json::json!({ "purged": rows, "source": scope });
                    ToolResult {
                        content: serde_json::to_string_pretty(&value)
                            .unwrap_or_else(|_| format!("purged {rows}")),
                        images: Vec::new(),
                        details: None,
                        structured_content: Some(value),
                        is_error: false,
                        terminate: false,
                    }
                }
                Err(error) => ToolResult::error(format!("ctx_purge: {error}")),
            }
        })
    }
}

// ---------------------------------------------------------------------------
// ctx_doctor
// ---------------------------------------------------------------------------

pub(crate) struct CtxDoctorTool {
    state: Arc<super::ContextModeState>,
}

impl CtxDoctorTool {
    pub(crate) fn new(state: Arc<super::ContextModeState>) -> Self {
        Self { state }
    }
}

impl AgentTool for CtxDoctorTool {
    fn name(&self) -> &'static str {
        super::CTX_DOCTOR_TOOL_NAME
    }

    fn description(&self) -> &'static str {
        "Report local context-mode diagnostics without touching any service: platform, embedded Python status (and worker spawn count), the knowledge-base database path/size/WAL size, FTS5 availability and schema version, chunk/source counts, and the hook settings (routing/auto-index on/off plus thresholds) with routing/index counters and the last hook error. Read-only: it never starts the Python worker and never creates the database; an absent knowledge base reads as zero chunks with exists=false. Use it instead of the upstream npm/cloud-only ctx_upgrade/ctx_insight tools to verify the local install and schema."
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
            let python_started = state.python.has_worker();
            let spawn_count = state.python.spawn_count();
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
            let value = serde_json::json!({
                "platform": {
                    "os": std::env::consts::OS,
                    "arch": std::env::consts::ARCH,
                    "family": std::env::consts::FAMILY,
                },
                "python": {
                    "available": true,
                    "engine": "embedded RustPython 3",
                    "status": if python_started { "running" } else { "not started" },
                    "workerStarted": python_started,
                    "spawnCount": spawn_count,
                },
                "database": database,
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
