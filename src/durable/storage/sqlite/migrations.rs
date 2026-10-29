//! Schema history for the SQLite durable backend.
//!
//! Ports upstream `storage/sqlite/migrations.ts`: migration 1 is the initial
//! schema, migration 2 rebuilds `tasks` to admit the `sleeping` status (SQLite
//! cannot alter a `CHECK` constraint in place). Versions are immutable once
//! shipped; a new schema change appends a migration.
//!
//! Migrations run inside one `BEGIN IMMEDIATE` transaction and are recorded in
//! `durable_schema`, so a reopen applies only the missing versions and an
//! interrupted migration leaves the database untouched.

use super::schema::{indexes, tables};
use crate::durable::errors::StorageError;
use rusqlite::{Connection, TransactionBehavior};

/// One immutable, ordered schema step.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SqliteMigration {
    /// Contiguous 1-based version.
    pub version: i64,
    /// Statements applied atomically for this version.
    pub statements: Vec<String>,
}

/// The newest schema version this backend understands.
pub const CURRENT_SQLITE_SCHEMA_VERSION: i64 = 3;

/// Build the ordered migration history.
pub fn create_sqlite_migrations() -> Vec<SqliteMigration> {
    // `next_id` is TEXT to mirror upstream (`node:sqlite` rejects INTEGER
    // results outside JavaScript's safe-integer range); this backend parses it
    // into `i64` on every read.
    let initial_schema = vec![
        format!(
            "CREATE TABLE {} (
                singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
                next_id TEXT NOT NULL,
                next_seq INTEGER NOT NULL
            ) STRICT",
            tables::DURABLE_METADATA
        ),
        format!(
            "INSERT INTO {} (singleton, next_id, next_seq) VALUES (1, '2', 1)",
            tables::DURABLE_METADATA
        ),
        format!(
            "CREATE TABLE {} (
                id INTEGER PRIMARY KEY,
                record_type TEXT NOT NULL CHECK (record_type IN ('conversation', 'entry', 'task', 'submission', 'document'))
            ) STRICT",
            tables::RECORD_IDS
        ),
        format!(
            "CREATE TABLE {} (
                id INTEGER PRIMARY KEY,
                owner_conversation_id INTEGER,
                owner_task_id INTEGER,
                record TEXT NOT NULL CHECK (json_valid(record))
            ) STRICT",
            tables::CONVERSATIONS
        ),
        format!(
            "CREATE INDEX {} ON {} (owner_conversation_id, id)",
            indexes::CONVERSATIONS_BY_OWNER_CONVERSATION,
            tables::CONVERSATIONS
        ),
        format!(
            "CREATE INDEX {} ON {} (owner_task_id, id)",
            indexes::CONVERSATIONS_BY_OWNER_TASK,
            tables::CONVERSATIONS
        ),
        format!(
            "CREATE TABLE {} (
                id INTEGER PRIMARY KEY,
                conversation_id INTEGER NOT NULL,
                head INTEGER,
                commit_seq INTEGER NOT NULL,
                record TEXT NOT NULL CHECK (json_valid(record))
            ) STRICT",
            tables::ENTRIES
        ),
        format!(
            "CREATE INDEX {} ON {} (conversation_id, id DESC)",
            indexes::ENTRIES_BY_CONVERSATION,
            tables::ENTRIES
        ),
        format!(
            "CREATE INDEX {} ON {} (conversation_id, id DESC) WHERE head IS NOT NULL",
            indexes::ENTRY_HEADS_BY_CONVERSATION,
            tables::ENTRIES
        ),
        // Version 1 predates `sleeping`; migration 2 widens the constraint.
        format!(
            "CREATE TABLE {} (
                id INTEGER PRIMARY KEY,
                conversation_id INTEGER NOT NULL,
                kind TEXT NOT NULL,
                status TEXT NOT NULL CHECK (status IN ('pending', 'running', 'waiting', 'completing', 'terminal')),
                abort_requested INTEGER NOT NULL CHECK (abort_requested IN (0, 1)),
                background INTEGER NOT NULL CHECK (background IN (0, 1)),
                record TEXT NOT NULL CHECK (json_valid(record))
            ) STRICT",
            tables::TASKS
        ),
        format!(
            "CREATE INDEX {} ON {} (status, id)",
            indexes::TASKS_BY_STATUS,
            tables::TASKS
        ),
        format!(
            "CREATE INDEX {} ON {} (conversation_id, id)",
            indexes::TASKS_BY_CONVERSATION,
            tables::TASKS
        ),
        format!(
            "CREATE INDEX {} ON {} (kind, id)",
            indexes::TASKS_BY_KIND,
            tables::TASKS
        ),
        format!(
            "CREATE INDEX {} ON {} (abort_requested, id)",
            indexes::TASKS_BY_ABORT_REQUESTED,
            tables::TASKS
        ),
        format!(
            "CREATE INDEX {} ON {} (background, id)",
            indexes::TASKS_BY_BACKGROUND,
            tables::TASKS
        ),
        format!(
            "CREATE TABLE {} (
                id INTEGER PRIMARY KEY,
                conversation_id INTEGER NOT NULL,
                request_id TEXT,
                status TEXT NOT NULL CHECK (status IN ('queued', 'placed', 'done', 'unanswered')),
                record TEXT NOT NULL CHECK (json_valid(record))
            ) STRICT",
            tables::SUBMISSIONS
        ),
        format!(
            "CREATE INDEX {} ON {} (conversation_id, request_id)",
            indexes::SUBMISSIONS_BY_REQUEST,
            tables::SUBMISSIONS
        ),
        format!(
            "CREATE INDEX {} ON {} (conversation_id, id)",
            indexes::SUBMISSIONS_BY_CONVERSATION,
            tables::SUBMISSIONS
        ),
        format!(
            "CREATE INDEX {} ON {} (status, id)",
            indexes::SUBMISSIONS_BY_STATUS,
            tables::SUBMISSIONS
        ),
        format!(
            "CREATE TABLE {} (
                id INTEGER PRIMARY KEY,
                kind TEXT NOT NULL,
                family INTEGER NOT NULL CHECK (family IN (0, 1)),
                key_value TEXT NOT NULL,
                scope_kind TEXT NOT NULL CHECK (scope_kind IN ('session', 'conversation', 'task')),
                owner_id INTEGER NOT NULL,
                created_at INTEGER NOT NULL,
                retired_at INTEGER,
                record TEXT NOT NULL CHECK (json_valid(record))
            ) STRICT",
            tables::DOCUMENTS
        ),
        format!(
            "CREATE INDEX {}
                ON {} (kind, scope_kind, owner_id, family, key_value, created_at DESC, retired_at)",
            indexes::DOCUMENTS_BY_ADDRESS,
            tables::DOCUMENTS
        ),
        format!(
            "CREATE INDEX {} ON {} (scope_kind, owner_id, id)",
            indexes::DOCUMENTS_BY_SCOPE,
            tables::DOCUMENTS
        ),
        format!(
            "CREATE INDEX {} ON {} (scope_kind, owner_id, kind, id)",
            indexes::DOCUMENTS_BY_SCOPE_KIND,
            tables::DOCUMENTS
        ),
        format!(
            "CREATE TABLE {} (
                document_id INTEGER NOT NULL,
                seq INTEGER NOT NULL,
                kind TEXT NOT NULL CHECK (kind IN ('base', 'delta')),
                version INTEGER NOT NULL,
                content TEXT NOT NULL CHECK (json_valid(content)),
                PRIMARY KEY (document_id, seq)
            ) STRICT",
            tables::DOCUMENT_REVISIONS
        ),
        format!(
            "CREATE INDEX {} ON {} (document_id, kind, seq DESC)",
            indexes::DOCUMENT_REVISIONS_BY_KIND,
            tables::DOCUMENT_REVISIONS
        ),
    ];

    let rebuilt_tasks = format!("{}_v2", tables::TASKS);
    let sleeping_status = vec![
        format!(
            "CREATE TABLE {} (
                id INTEGER PRIMARY KEY,
                conversation_id INTEGER NOT NULL,
                kind TEXT NOT NULL,
                status TEXT NOT NULL CHECK (status IN ('pending', 'running', 'waiting', 'sleeping', 'completing', 'terminal')),
                abort_requested INTEGER NOT NULL CHECK (abort_requested IN (0, 1)),
                background INTEGER NOT NULL CHECK (background IN (0, 1)),
                record TEXT NOT NULL CHECK (json_valid(record))
            ) STRICT",
            rebuilt_tasks
        ),
        format!(
            "INSERT INTO {} (id, conversation_id, kind, status, abort_requested, background, record)
                SELECT id, conversation_id, kind, status, abort_requested, background, record FROM {}",
            rebuilt_tasks,
            tables::TASKS
        ),
        format!("DROP TABLE {}", tables::TASKS),
        format!(
            "ALTER TABLE {} RENAME TO {}",
            rebuilt_tasks,
            tables::TASKS
        ),
        format!(
            "CREATE INDEX {} ON {} (status, id)",
            indexes::TASKS_BY_STATUS,
            tables::TASKS
        ),
        format!(
            "CREATE INDEX {} ON {} (conversation_id, id)",
            indexes::TASKS_BY_CONVERSATION,
            tables::TASKS
        ),
        format!(
            "CREATE INDEX {} ON {} (kind, id)",
            indexes::TASKS_BY_KIND,
            tables::TASKS
        ),
        format!(
            "CREATE INDEX {} ON {} (abort_requested, id)",
            indexes::TASKS_BY_ABORT_REQUESTED,
            tables::TASKS
        ),
        format!(
            "CREATE INDEX {} ON {} (background, id)",
            indexes::TASKS_BY_BACKGROUND,
            tables::TASKS
        ),
    ];

    // The request key is scoped per conversation and resolves to exactly one
    // record (the newest write wins, matching the in-memory backend's index).
    // Older databases could hold duplicates (the version-1 index was not
    // unique), so the migration first keeps only the newest row per key and
    // then replaces the plain index with a partial unique one. Every statement
    // is idempotent: `DROP INDEX IF EXISTS`/`IF NOT EXISTS` and the dedupe
    // `DELETE` are safe to replay.
    let unique_request_keys = vec![
        format!(
            "DELETE FROM {} WHERE request_id IS NOT NULL AND id NOT IN (
                SELECT MAX(id) FROM {} WHERE request_id IS NOT NULL
                GROUP BY conversation_id, request_id)",
            tables::SUBMISSIONS,
            tables::SUBMISSIONS
        ),
        format!("DROP INDEX IF EXISTS {}", indexes::SUBMISSIONS_BY_REQUEST),
        format!(
            "CREATE UNIQUE INDEX IF NOT EXISTS {} ON {} (conversation_id, request_id) WHERE request_id IS NOT NULL",
            indexes::SUBMISSIONS_BY_REQUEST,
            tables::SUBMISSIONS
        ),
    ];

    vec![
        SqliteMigration {
            version: 1,
            statements: initial_schema,
        },
        SqliteMigration {
            version: 2,
            statements: sleeping_status,
        },
        SqliteMigration {
            version: 3,
            statements: unique_request_keys,
        },
    ]
}

/// Apply every pending migration atomically.
pub fn apply_sqlite_migrations(connection: &mut Connection) -> Result<(), StorageError> {
    apply_sqlite_migrations_with(connection, &create_sqlite_migrations())
}

/// Apply a caller-supplied migration set atomically.
///
/// Exposed for tests and embedders that need a reduced or extended history;
/// production callers use [`apply_sqlite_migrations`].
pub fn apply_sqlite_migrations_with(
    connection: &mut Connection,
    migrations: &[SqliteMigration],
) -> Result<(), StorageError> {
    for (index, migration) in migrations.iter().enumerate() {
        if migration.version != index as i64 + 1 {
            return Err(StorageError::Io(
                "SQLite migrations must have contiguous versions starting at 1".to_owned(),
            ));
        }
    }

    let transaction = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(|error| migration_error("starting the migration transaction", error))?;
    transaction
        .execute_batch(&format!(
            "CREATE TABLE IF NOT EXISTS {} (
                singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
                version INTEGER NOT NULL CHECK (version >= 0)
            ) STRICT",
            tables::DURABLE_SCHEMA
        ))
        .map_err(|error| migration_error("creating the schema table", error))?;
    transaction
        .execute(
            &format!(
                "INSERT OR IGNORE INTO {} (singleton, version) VALUES (1, 0)",
                tables::DURABLE_SCHEMA
            ),
            [],
        )
        .map_err(|error| migration_error("seeding the schema version", error))?;
    let version: i64 = transaction
        .query_row(
            &format!(
                "SELECT version FROM {} WHERE singleton = 1",
                tables::DURABLE_SCHEMA
            ),
            [],
            |row| row.get(0),
        )
        .map_err(|error| migration_error("reading the schema version", error))?;
    let current = migrations
        .last()
        .map(|migration| migration.version)
        .unwrap_or(0);
    if version > current {
        return Err(StorageError::Io(format!(
            "SQLite schema version {version} is newer than supported version {current}"
        )));
    }
    for migration in migrations {
        if migration.version <= version {
            continue;
        }
        for statement in &migration.statements {
            transaction
                .execute_batch(statement)
                .map_err(|error| migration_error("applying a migration step", error))?;
        }
        transaction
            .execute(
                &format!(
                    "UPDATE {} SET version = ?1 WHERE singleton = 1",
                    tables::DURABLE_SCHEMA
                ),
                [migration.version],
            )
            .map_err(|error| migration_error("recording the schema version", error))?;
    }
    transaction
        .commit()
        .map_err(|error| migration_error("committing the migrations", error))
}

fn migration_error(action: &str, error: rusqlite::Error) -> StorageError {
    StorageError::Io(format!("SQLite {action} failed: {error}"))
}
