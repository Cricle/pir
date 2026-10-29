//! SQLite implementation of [`Storage`].
//!
//! Ports upstream `storage/sqlite/storage.ts`: one file-backed database with
//! typed index columns beside a JSON `record` column per row. The SQL schema
//! lives in [`super::migrations`].
//!
//! # Atomic batches
//!
//! `commit` runs one `BEGIN IMMEDIATE` transaction: it reads the metadata
//! watermarks, validates the batch, applies every table and document write,
//! advances `next_seq`/`next_id`, and commits. Any rejection rolls the whole
//! transaction back, so a failed batch is never partially visible and does not
//! consume a sequence.
//!
//! # Durability and reopen
//!
//! The connection uses WAL with `synchronous = NORMAL` (upstream's node
//! adapter default); [`SqliteStorageOptions::fsync`] upgrades that to `FULL`.
//! `open` applies pending migrations atomically and restores the ID/sequence
//! watermarks from `durable_metadata`, so reopen preserves data, ordering and
//! both watermarks.
//!
//! Unlike the JSONL backend this backend never poisons itself: SQLite rolls a
//! failed transaction back, so the stored state stays internally consistent
//! and the instance remains usable. Reads are serialized through the same
//! connection mutex as commits, which also makes `close` wait for an in-flight
//! operation.
//!
//! # Deviations from upstream
//!
//! * `mint_id` persists the next-ID watermark in `durable_metadata` before
//!   returning. Upstream only advances an in-memory counter, so a minted but
//!   uncommitted ID could be handed out again after a reopen; the
//!   [`Storage`] contract (and the JSONL backend's `ids.jsonl` journal) require
//!   the stronger behavior.
//! * Namespaced storage (the upstream `prefix` option) and the Node/Cloudflare
//!   database facades are not ported: this backend owns one database file and
//!   opens it with `rusqlite` directly.
//! * Every database call runs on the blocking pool (`tokio::task::spawn_blocking`),
//!   as the context-mode index does, so the async executor never blocks on
//!   SQLite.

use super::migrations::apply_sqlite_migrations;
use super::schema::tables;
use crate::durable::delta::apply_immutable_batches;
use crate::durable::errors::{StorageError, StorageRequestError};
use crate::durable::ids::{ConversationId, DocumentId, EntryId, Seq, SubmissionId, TaskId};
use crate::durable::storage::scan::{page_of, scan_start, ScanStart};
use crate::durable::types::{
    ConversationQuery, ConversationRecord, Cursor, DocumentAddress, DocumentContent,
    DocumentCreate, DocumentPoint, DocumentQuery, DocumentRecord, DocumentScope, EntryQuery,
    EntryRecord, EntryWithSeq, JsonObject, Page, ScanOrder, StorageFuture, StorageWrite,
    StoredDocument, SubmissionQuery, SubmissionRecord, TaskQuery, TaskRecord,
};
use crate::durable::Storage;
use rusqlite::types::Value as SqlValue;
use rusqlite::{params, params_from_iter, Connection, OptionalExtension, TransactionBehavior};
use serde::de::DeserializeOwned;
use serde::Serialize;
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// SQLite's default lock wait for this backend (upstream's node adapter uses
/// the same 5 s).
const BUSY_TIMEOUT_MS: i64 = 5_000;
/// WAL auto-checkpoint threshold, in pages (SQLite's and upstream's default).
const WAL_AUTO_CHECKPOINT_PAGES: i64 = 1_000;

/// Options for [`SqliteStorage::open_with_options`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SqliteStorageOptions {
    /// Use `PRAGMA synchronous = FULL` instead of WAL's `NORMAL`.
    pub fsync: bool,
}

/// The record family an identifier belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TableName {
    Conversation,
    Entry,
    Task,
    Submission,
    Document,
}

impl TableName {
    fn as_str(self) -> &'static str {
        match self {
            Self::Conversation => "conversation",
            Self::Entry => "entry",
            Self::Task => "task",
            Self::Submission => "submission",
            Self::Document => "document",
        }
    }

    fn parse(text: &str) -> Option<Self> {
        match text {
            "conversation" => Some(Self::Conversation),
            "entry" => Some(Self::Entry),
            "task" => Some(Self::Task),
            "submission" => Some(Self::Submission),
            "document" => Some(Self::Document),
            _ => None,
        }
    }
}

/// The indexed identity columns of one document address.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct AddressParts {
    kind: String,
    scope_kind: &'static str,
    owner_id: i64,
    family: i64,
    key_value: String,
}

/// The document writes of one commit, grouped by incarnation.
#[derive(Clone, Debug, Default)]
struct DocumentAction {
    create: Option<DocumentCreate>,
    copy: Option<crate::durable::types::DocumentCopySource>,
    content: Option<DocumentContent>,
    retire: bool,
}

#[derive(Debug)]
struct Inner {
    /// `None` after `close`; every operation then rejects with `Closed`.
    connection: Mutex<Option<Connection>>,
    path: PathBuf,
}

/// File-backed SQLite implementation of [`Storage`].
#[derive(Debug)]
pub struct SqliteStorage {
    inner: Arc<Inner>,
}

impl SqliteStorage {
    /// Open or create a database file.
    pub async fn open(path: impl AsRef<Path>) -> Result<Self, StorageError> {
        Self::open_with_options(path, SqliteStorageOptions::default()).await
    }

    /// Open or create a database file with explicit options.
    pub async fn open_with_options(
        path: impl AsRef<Path>,
        options: SqliteStorageOptions,
    ) -> Result<Self, StorageError> {
        let path = path.as_ref().to_path_buf();
        tokio::task::spawn_blocking(move || open_blocking(&path, options))
            .await
            .map_err(|error| StorageError::Io(format!("SQLite open task failed: {error}")))?
    }

    /// The database file this storage owns.
    pub fn path(&self) -> &Path {
        &self.inner.path
    }

    /// Run one operation against the single connection on the blocking pool.
    ///
    /// The mutex is held for the whole operation, so commits and reads observe
    /// one serialized order and `close` cannot race an operation.
    async fn run<T, F>(&self, operation: F) -> Result<T, StorageError>
    where
        T: Send + 'static,
        F: FnOnce(&mut Connection) -> Result<T, StorageError> + Send + 'static,
    {
        let inner = Arc::clone(&self.inner);
        tokio::task::spawn_blocking(move || {
            let mut guard = inner
                .connection
                .lock()
                .map_err(|_| StorageError::Io("SQLite connection lock is poisoned".to_owned()))?;
            let connection = guard
                .as_mut()
                .ok_or_else(|| StorageError::closed("SqliteStorage"))?;
            operation(connection)
        })
        .await
        .map_err(|error| StorageError::Io(format!("SQLite operation task failed: {error}")))?
    }
}

fn open_blocking(
    path: &Path,
    options: SqliteStorageOptions,
) -> Result<SqliteStorage, StorageError> {
    if path != Path::new(":memory:") {
        if let Some(parent) = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            std::fs::create_dir_all(parent).map_err(|error| {
                StorageError::Io(format!("SQLite directory creation failed: {error}"))
            })?;
        }
    }
    let mut connection = Connection::open(path).map_err(|error| sqlite_error("open", error))?;
    let synchronous = if options.fsync { "FULL" } else { "NORMAL" };
    connection
        .execute_batch(&format!(
            "PRAGMA journal_mode = WAL;\
             PRAGMA synchronous = {synchronous};\
             PRAGMA busy_timeout = {BUSY_TIMEOUT_MS};\
             PRAGMA wal_autocheckpoint = {WAL_AUTO_CHECKPOINT_PAGES};"
        ))
        .map_err(|error| sqlite_error("configuring pragmas", error))?;
    apply_sqlite_migrations(&mut connection)?;
    // Fail the open (rather than deferring to the first call) when the metadata
    // row is missing or unreadable.
    read_metadata(&connection)?;
    let path = path.to_path_buf();
    Ok(SqliteStorage {
        inner: Arc::new(Inner {
            connection: Mutex::new(Some(connection)),
            path,
        }),
    })
}

// ─── commit ─────────────────────────────────────────────────────────────────

fn commit_blocking(
    connection: &mut Connection,
    writes: Vec<StorageWrite>,
) -> Result<Seq, StorageError> {
    let actions = prepare_document_actions(&writes)?;
    let transaction = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(|error| sqlite_error("starting a commit", error))?;
    let (next_id, next_seq) = read_metadata(&transaction)?;
    let seq = Seq::from_raw(next_seq);
    check_global_ids(&transaction, &writes)?;
    check_document_actions(&transaction, &actions)?;
    for write in &writes {
        apply_table_write(&transaction, write, seq)?;
    }
    apply_document_actions(&transaction, &actions, seq)?;
    let following_seq = next_seq
        .checked_add(1)
        .ok_or_else(|| StorageError::Io("SQLite sequence space is exhausted".to_owned()))?;
    let candidate_next_id = candidate_next_id(next_id, &writes);
    update_metadata(&transaction, candidate_next_id, following_seq)?;
    transaction
        .commit()
        .map_err(|error| sqlite_error("committing", error))?;
    Ok(seq)
}

fn mint_id_blocking(connection: &mut Connection) -> Result<i64, StorageError> {
    let transaction = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(|error| sqlite_error("starting an ID mint", error))?;
    let (next_id, _) = read_metadata(&transaction)?;
    let following = next_id
        .checked_add(1)
        .ok_or_else(|| StorageError::Io("SQLite ID space is exhausted".to_owned()))?;
    transaction
        .execute(
            &format!(
                "UPDATE {} SET next_id = ?1 WHERE singleton = 1",
                tables::DURABLE_METADATA
            ),
            params![following.to_string()],
        )
        .map_err(|error| sqlite_error("recording a minted ID", error))?;
    transaction
        .commit()
        .map_err(|error| sqlite_error("committing an ID mint", error))?;
    Ok(next_id)
}

fn read_metadata(connection: &Connection) -> Result<(i64, i64), StorageError> {
    let row: Option<(String, i64)> = connection
        .query_row(
            &format!(
                "SELECT next_id, next_seq FROM {} WHERE singleton = 1",
                tables::DURABLE_METADATA
            ),
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()
        .map_err(|error| sqlite_error("reading metadata", error))?;
    let Some((next_id, next_seq)) = row else {
        return Err(StorageError::Io(
            "Durable SQLite metadata is missing".to_owned(),
        ));
    };
    let next_id = next_id
        .parse::<i64>()
        .map_err(|error| StorageError::Io(format!("SQLite next_id is malformed: {error}")))?;
    Ok((next_id, next_seq))
}

fn update_metadata(
    connection: &Connection,
    next_id: i64,
    next_seq: i64,
) -> Result<(), StorageError> {
    connection
        .execute(
            &format!(
                "UPDATE {} SET next_id = ?1, next_seq = ?2 WHERE singleton = 1",
                tables::DURABLE_METADATA
            ),
            params![next_id.to_string(), next_seq],
        )
        .map_err(|error| sqlite_error("updating metadata", error))?;
    Ok(())
}

fn write_id(write: &StorageWrite) -> Option<i64> {
    match write {
        StorageWrite::Conversation { value } => Some(value.id.as_i64()),
        StorageWrite::Entry { value } => Some(value.id.as_i64()),
        StorageWrite::Task { value } => Some(value.id.as_i64()),
        StorageWrite::Submission { value } => Some(value.id.as_i64()),
        StorageWrite::DocumentCreate { record, .. } | StorageWrite::DocumentCopy { record, .. } => {
            Some(record.id.as_i64())
        }
        StorageWrite::DocumentChange { .. } | StorageWrite::DocumentRetire { .. } => None,
    }
}

fn candidate_next_id(next_id: i64, writes: &[StorageWrite]) -> i64 {
    let mut candidate = next_id;
    for write in writes {
        if let Some(id) = write_id(write) {
            candidate = candidate.max(id.saturating_add(1));
        }
    }
    candidate
}

fn prepare_document_actions(
    writes: &[StorageWrite],
) -> Result<BTreeMap<DocumentId, DocumentAction>, StorageError> {
    let mut actions: BTreeMap<DocumentId, DocumentAction> = BTreeMap::new();
    for write in writes {
        match write {
            StorageWrite::DocumentCreate { record, content } => {
                let action = actions.entry(record.id).or_default();
                if action.create.is_some() || action.content.is_some() || action.copy.is_some() {
                    return Err(StorageError::invalid(format!(
                        "Document {} has more than one content command",
                        record.id
                    )));
                }
                action.create = Some(record.clone());
                action.content = Some(content.clone());
            }
            StorageWrite::DocumentCopy { record, source } => {
                let action = actions.entry(record.id).or_default();
                if action.create.is_some() || action.content.is_some() || action.copy.is_some() {
                    return Err(StorageError::invalid(format!(
                        "Document {} has more than one content command",
                        record.id
                    )));
                }
                action.create = Some(record.clone());
                action.copy = Some(*source);
            }
            StorageWrite::DocumentChange { id, content } => {
                let action = actions.entry(*id).or_default();
                if action.content.is_some() || action.copy.is_some() {
                    return Err(StorageError::invalid(format!(
                        "Document {id} has more than one content command"
                    )));
                }
                action.content = Some(content.clone());
            }
            StorageWrite::DocumentRetire { id } => {
                let action = actions.entry(*id).or_default();
                if action.retire {
                    return Err(StorageError::invalid(format!(
                        "Document {id} is retired more than once"
                    )));
                }
                action.retire = true;
            }
            _ => {}
        }
    }
    Ok(actions)
}

fn check_global_ids(connection: &Connection, writes: &[StorageWrite]) -> Result<(), StorageError> {
    let mut claimed: HashMap<i64, TableName> = HashMap::new();
    for write in writes {
        let (table, id) = match write {
            StorageWrite::Conversation { value } => (TableName::Conversation, value.id.as_i64()),
            StorageWrite::Entry { value } => (TableName::Entry, value.id.as_i64()),
            StorageWrite::Task { value } => (TableName::Task, value.id.as_i64()),
            StorageWrite::Submission { value } => (TableName::Submission, value.id.as_i64()),
            StorageWrite::DocumentCreate { record, .. }
            | StorageWrite::DocumentCopy { record, .. } => {
                (TableName::Document, record.id.as_i64())
            }
            StorageWrite::DocumentChange { .. } | StorageWrite::DocumentRetire { .. } => continue,
        };
        let existing_text: Option<String> = connection
            .query_row(
                &format!(
                    "SELECT record_type FROM {} WHERE id = ?1",
                    tables::RECORD_IDS
                ),
                params![id],
                |row| row.get(0),
            )
            .optional()
            .map_err(|error| sqlite_error("reading an ID owner", error))?;
        let existing = match existing_text.as_deref() {
            None => None,
            Some(text) => Some(TableName::parse(text).ok_or_else(|| {
                StorageError::Io(format!(
                    "SQLite record_ids holds an unknown record type: {text}"
                ))
            })?),
        };
        let earlier = claimed.get(&id).copied();
        let exclusive = matches!(
            table,
            TableName::Conversation | TableName::Entry | TableName::Document
        );
        if exclusive {
            if let Some(existing) = existing {
                return Err(StorageError::invalid(format!(
                    "ID {id} already belongs to {}",
                    existing.as_str()
                )));
            }
            if earlier.is_some() {
                return Err(StorageError::invalid(format!(
                    "ID {id} is written more than once"
                )));
            }
        } else {
            if let Some(existing) = existing {
                if existing != table {
                    return Err(StorageError::invalid(format!(
                        "ID {id} already belongs to {}",
                        existing.as_str()
                    )));
                }
            }
            if let Some(earlier) = earlier {
                if earlier != table {
                    return Err(StorageError::invalid(format!(
                        "ID {id} is written as two record types"
                    )));
                }
            }
        }
        claimed.insert(id, table);
    }
    Ok(())
}

fn check_document_actions(
    connection: &Connection,
    actions: &BTreeMap<DocumentId, DocumentAction>,
) -> Result<(), StorageError> {
    let mut live_counts: HashMap<AddressParts, i64> = HashMap::new();
    let mut current_ids: HashMap<AddressParts, Option<DocumentId>> = HashMap::new();
    for (id, action) in actions {
        if let Some(source) = action.copy {
            if actions.contains_key(&source.id) {
                return Err(StorageError::invalid(format!(
                    "Document copy {id} source is changed in the copy batch"
                )));
            }
        }
        let existing = read_document_record(connection, *id)?;
        if action.create.is_none() && existing.is_none() {
            return Err(StorageError::invalid(format!("Unknown document: {id}")));
        }
        if action.create.is_some() && existing.is_some() {
            return Err(StorageError::invalid(format!(
                "Document {id} already exists"
            )));
        }
        if existing
            .as_ref()
            .and_then(|record| record.retired_at)
            .is_some()
        {
            return Err(StorageError::invalid(format!("Document {id} is retired")));
        }
        if action.create.is_some()
            && action.copy.is_none()
            && !matches!(action.content, Some(DocumentContent::Base { .. }))
        {
            return Err(StorageError::invalid(format!(
                "Document {id} creation requires a base"
            )));
        }
        if let Some(DocumentContent::Delta { version, .. }) = &action.content {
            let previous: Option<i64> = connection
                .query_row(
                    &format!(
                        "SELECT version FROM {} WHERE document_id = ?1 ORDER BY seq DESC LIMIT 1",
                        tables::DOCUMENT_REVISIONS
                    ),
                    params![id.as_i64()],
                    |row| row.get(0),
                )
                .optional()
                .map_err(|error| sqlite_error("reading the latest revision", error))?;
            let Some(previous) = previous else {
                return Err(StorageError::invalid(format!(
                    "Document {id} delta has no base"
                )));
            };
            if previous != *version {
                return Err(StorageError::invalid(format!(
                    "Document {id} version transition requires a base"
                )));
            }
        }

        let parts = match &action.create {
            Some(create) => {
                address_parts(create.kind.as_str(), &create.scope, create.key.as_deref())?
            }
            None => {
                let existing = existing
                    .as_ref()
                    .ok_or_else(|| StorageError::invalid(format!("Unknown document: {id}")))?;
                address_parts(
                    existing.kind.as_str(),
                    &existing.scope,
                    existing.key.as_deref(),
                )?
            }
        };
        // Resolve the pre-batch current incarnation once per address. The
        // retire decrement must apply only when the retired incarnation *is*
        // that current one; decrementing for any other incarnation would let a
        // batch retire a non-current incarnation and create a second live one.
        let current = match current_ids.get(&parts) {
            Some(current) => *current,
            None => {
                let current = current_document_id(connection, &parts)?;
                current_ids.insert(parts.clone(), current);
                current
            }
        };
        let mut live = match live_counts.get(&parts).copied() {
            Some(live) => live,
            None => {
                if current.is_some() {
                    1
                } else {
                    0
                }
            }
        };
        if action.retire && current == Some(*id) {
            live -= 1;
        }
        if action.create.is_some() && !action.retire {
            live += 1;
        }
        live_counts.insert(parts, live);
    }
    if live_counts.values().any(|live| *live > 1) {
        return Err(StorageError::invalid(
            "Document address already has a current incarnation",
        ));
    }
    Ok(())
}

fn apply_table_write(
    connection: &Connection,
    write: &StorageWrite,
    seq: Seq,
) -> Result<(), StorageError> {
    match write {
        StorageWrite::Conversation { value } => {
            claim_id(connection, value.id.as_i64(), TableName::Conversation)?;
            connection
                .execute(
                    &format!(
                        "INSERT INTO {} (id, owner_conversation_id, owner_task_id, record) VALUES (?1, ?2, ?3, ?4)",
                        tables::CONVERSATIONS
                    ),
                    params![
                        value.id.as_i64(),
                        value.owner.as_ref().map(|owner| owner.conversation_id.as_i64()),
                        value.owner.as_ref().map(|owner| owner.task_id.as_i64()),
                        encode_json(value)?
                    ],
                )
                .map_err(|error| sqlite_error("writing a conversation", error))?;
        }
        StorageWrite::Entry { value } => {
            claim_id(connection, value.id.as_i64(), TableName::Entry)?;
            connection
                .execute(
                    &format!(
                        "INSERT INTO {} (id, conversation_id, head, commit_seq, record) VALUES (?1, ?2, ?3, ?4, ?5)",
                        tables::ENTRIES
                    ),
                    params![
                        value.id.as_i64(),
                        value.conversation_id.as_i64(),
                        value.head.map(|head| head.as_i64()),
                        seq.as_i64(),
                        encode_json(value)?
                    ],
                )
                .map_err(|error| sqlite_error("writing an entry", error))?;
        }
        StorageWrite::Task { value } => {
            claim_id(connection, value.id.as_i64(), TableName::Task)?;
            connection
                .execute(
                    &format!(
                        "INSERT INTO {} (id, conversation_id, kind, status, abort_requested, background, record)
                            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
                            ON CONFLICT(id) DO UPDATE SET conversation_id = excluded.conversation_id,
                            kind = excluded.kind, status = excluded.status,
                            abort_requested = excluded.abort_requested,
                            background = excluded.background, record = excluded.record",
                        tables::TASKS
                    ),
                    params![
                        value.id.as_i64(),
                        value.conversation_id.as_i64(),
                        encode_indexed_string(&value.kind)?,
                        value.state.status().to_string(),
                        bool_int(value.abort_requested),
                        bool_int(value.background),
                        encode_json(value)?
                    ],
                )
                .map_err(|error| sqlite_error("writing a task", error))?;
        }
        StorageWrite::Submission { value } => {
            claim_id(connection, value.id.as_i64(), TableName::Submission)?;
            let request_id = match &value.request_id {
                Some(request_id) => Some(encode_indexed_string(request_id)?),
                None => None,
            };
            // A request key resolves to exactly one record (the newest write).
            // Migration 3 makes the key unique, so a different id claiming the
            // same key must replace the previous holder inside this commit.
            if let Some(request_id) = &request_id {
                connection
                    .execute(
                        &format!(
                            "DELETE FROM {} WHERE conversation_id = ?1 AND request_id = ?2 AND id <> ?3",
                            tables::SUBMISSIONS
                        ),
                        params![
                            value.conversation_id.as_i64(),
                            request_id,
                            value.id.as_i64()
                        ],
                    )
                    .map_err(|error| sqlite_error("replacing a submission request key", error))?;
            }
            connection
                .execute(
                    &format!(
                        "INSERT INTO {} (id, conversation_id, request_id, status, record) VALUES (?1, ?2, ?3, ?4, ?5)
                            ON CONFLICT(id) DO UPDATE SET conversation_id = excluded.conversation_id,
                            request_id = excluded.request_id, status = excluded.status,
                            record = excluded.record",
                        tables::SUBMISSIONS
                    ),
                    params![
                        value.id.as_i64(),
                        value.conversation_id.as_i64(),
                        request_id,
                        value.status.to_string(),
                        encode_json(value)?
                    ],
                )
                .map_err(|error| sqlite_error("writing a submission", error))?;
        }
        StorageWrite::DocumentCreate { .. }
        | StorageWrite::DocumentCopy { .. }
        | StorageWrite::DocumentChange { .. }
        | StorageWrite::DocumentRetire { .. } => {}
    }
    Ok(())
}

fn apply_document_actions(
    connection: &Connection,
    actions: &BTreeMap<DocumentId, DocumentAction>,
    seq: Seq,
) -> Result<(), StorageError> {
    for (id, action) in actions {
        let mut content = action.content.clone();
        if let Some(source) = action.copy {
            let stored =
                materialize_document(connection, source.id, source.at).map_err(|error| {
                    StorageError::invalid(format!("Document copy {id} was rejected: {error}"))
                })?;
            let stored = stored.ok_or_else(|| {
                StorageError::invalid(format!(
                    "Document copy {id} was rejected: fork source document {} cannot be read",
                    source.id
                ))
            })?;
            let create = action.create.as_ref().ok_or_else(|| {
                StorageError::invalid(format!("Document copy {id} has no record"))
            })?;
            let source_is_conversation =
                matches!(stored.record.scope, DocumentScope::Conversation { .. });
            let target_is_conversation = matches!(create.scope, DocumentScope::Conversation { .. });
            if !source_is_conversation
                || !target_is_conversation
                || stored.record.kind != create.kind
                || stored.record.key != create.key
                || stored.record.history != create.history
                || stored.record.fork != create.fork
            {
                return Err(StorageError::invalid(format!(
                    "Document copy {id} was rejected: fork source document {} does not match the copied record",
                    source.id
                )));
            }
            content = Some(DocumentContent::Base {
                version: stored.version,
                value: stored.value,
            });
        }

        let record: DocumentRecord = if let Some(create) = &action.create {
            let record = DocumentRecord {
                id: create.id,
                kind: create.kind.clone(),
                key: create.key.clone(),
                created_at: seq,
                retired_at: action.retire.then_some(seq),
                scope: create.scope,
                history: create.history,
                fork: create.fork,
            };
            let parts = address_parts(&record.kind, &record.scope, record.key.as_deref())?;
            claim_id(connection, id.as_i64(), TableName::Document)?;
            connection
                .execute(
                    &format!(
                        "INSERT INTO {} (id, kind, family, key_value, scope_kind, owner_id, created_at, retired_at, record)
                            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                        tables::DOCUMENTS
                    ),
                    params![
                        id.as_i64(),
                        parts.kind,
                        parts.family,
                        parts.key_value,
                        parts.scope_kind,
                        parts.owner_id,
                        seq.as_i64(),
                        record.retired_at.map(|retired| retired.as_i64()),
                        encode_json(&record)?
                    ],
                )
                .map_err(|error| sqlite_error("writing a document", error))?;
            record
        } else {
            read_document_record(connection, *id)?
                .ok_or_else(|| StorageError::invalid(format!("Unknown document: {id}")))?
        };

        if let Some(content) = content {
            if matches!(content, DocumentContent::Base { .. }) && record.is_current_only() {
                connection
                    .execute(
                        &format!(
                            "DELETE FROM {} WHERE document_id = ?1",
                            tables::DOCUMENT_REVISIONS
                        ),
                        params![id.as_i64()],
                    )
                    .map_err(|error| sqlite_error("clearing document revisions", error))?;
            }
            let (kind, version, encoded) = match &content {
                DocumentContent::Base { version, value } => ("base", *version, encode_json(value)?),
                DocumentContent::Delta { version, ops } => ("delta", *version, encode_json(ops)?),
            };
            connection
                .execute(
                    &format!(
                        "INSERT INTO {} (document_id, seq, kind, version, content) VALUES (?1, ?2, ?3, ?4, ?5)",
                        tables::DOCUMENT_REVISIONS
                    ),
                    params![id.as_i64(), seq.as_i64(), kind, version, encoded],
                )
                .map_err(|error| sqlite_error("writing a document revision", error))?;
        }

        if action.retire {
            if action.create.is_none() {
                let retired = DocumentRecord {
                    retired_at: Some(seq),
                    ..record.clone()
                };
                connection
                    .execute(
                        &format!(
                            "UPDATE {} SET retired_at = ?1, record = ?2 WHERE id = ?3",
                            tables::DOCUMENTS
                        ),
                        params![seq.as_i64(), encode_json(&retired)?, id.as_i64()],
                    )
                    .map_err(|error| sqlite_error("retiring a document", error))?;
            }
            if record.is_current_only() {
                connection
                    .execute(
                        &format!(
                            "DELETE FROM {} WHERE document_id = ?1",
                            tables::DOCUMENT_REVISIONS
                        ),
                        params![id.as_i64()],
                    )
                    .map_err(|error| sqlite_error("clearing retired document revisions", error))?;
            }
        }
    }
    Ok(())
}

fn claim_id(connection: &Connection, id: i64, table: TableName) -> Result<(), StorageError> {
    connection
        .execute(
            &format!(
                "INSERT OR IGNORE INTO {} (id, record_type) VALUES (?1, ?2)",
                tables::RECORD_IDS
            ),
            params![id, table.as_str()],
        )
        .map_err(|error| sqlite_error("claiming an ID", error))?;
    Ok(())
}

// ─── reads ──────────────────────────────────────────────────────────────────

fn read_conversation(
    connection: &Connection,
    id: ConversationId,
) -> Result<Option<ConversationRecord>, StorageError> {
    query_optional_json(
        connection,
        &format!("SELECT record FROM {} WHERE id = ?1", tables::CONVERSATIONS),
        params![id.as_i64()],
        "conversation",
    )
}

fn read_document_record(
    connection: &Connection,
    id: DocumentId,
) -> Result<Option<DocumentRecord>, StorageError> {
    query_optional_json(
        connection,
        &format!("SELECT record FROM {} WHERE id = ?1", tables::DOCUMENTS),
        params![id.as_i64()],
        "document",
    )
}

fn read_entry(connection: &Connection, id: EntryId) -> Result<Option<EntryWithSeq>, StorageError> {
    let row: Option<(String, i64)> = connection
        .query_row(
            &format!(
                "SELECT record, commit_seq FROM {} WHERE id = ?1",
                tables::ENTRIES
            ),
            params![id.as_i64()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()
        .map_err(|error| sqlite_error("reading an entry", error))?;
    match row {
        Some((json, commit_seq)) => Ok(Some(EntryWithSeq {
            entry: decode_json(&json, "entry")?,
            commit_seq: Seq::from_raw(commit_seq),
        })),
        None => Ok(None),
    }
}

fn read_entry_in(
    connection: &Connection,
    conversation_id: ConversationId,
    id: EntryId,
) -> Result<Option<EntryWithSeq>, StorageError> {
    let Some(mut conversation) = read_conversation(connection, conversation_id)? else {
        return Err(StorageError::Request(StorageRequestError::new(format!(
            "Unknown conversation: {conversation_id}"
        ))));
    };
    let Some(entry) = read_entry(connection, id)? else {
        return Ok(None);
    };
    let mut upper_entry_id = i64::MAX;
    while conversation.id != entry.entry.conversation_id {
        let Some(parent) = conversation.parent else {
            return Ok(None);
        };
        upper_entry_id = upper_entry_id.min(parent.at.as_i64());
        let Some(next) = read_conversation(connection, parent.conversation_id)? else {
            return Ok(None);
        };
        conversation = next;
    }
    if id.as_i64() > upper_entry_id {
        return Ok(None);
    }
    Ok(Some(entry))
}

fn read_latest_head_marker(
    connection: &Connection,
    conversation_id: ConversationId,
    at_or_before_entry_id: Option<EntryId>,
) -> Result<Option<EntryRecord>, StorageError> {
    let Some(mut conversation) = read_conversation(connection, conversation_id)? else {
        return Err(StorageError::Request(StorageRequestError::new(format!(
            "Unknown conversation: {conversation_id}"
        ))));
    };
    let mut upper = at_or_before_entry_id.map(|id| id.as_i64());
    loop {
        let row: Option<String> = match upper {
            Some(upper) => connection
                .query_row(
                    &format!(
                        "SELECT record FROM {} WHERE conversation_id = ?1 AND head IS NOT NULL AND id <= ?2 ORDER BY id DESC LIMIT 1",
                        tables::ENTRIES
                    ),
                    params![conversation.id.as_i64(), upper],
                    |row| row.get(0),
                )
                .optional(),
            None => connection
                .query_row(
                    &format!(
                        "SELECT record FROM {} WHERE conversation_id = ?1 AND head IS NOT NULL ORDER BY id DESC LIMIT 1",
                        tables::ENTRIES
                    ),
                    params![conversation.id.as_i64()],
                    |row| row.get(0),
                )
                .optional(),
        }
        .map_err(|error| sqlite_error("finding a head marker", error))?;
        if let Some(json) = row {
            return Ok(Some(decode_json(&json, "head marker entry")?));
        }
        let Some(parent) = conversation.parent else {
            return Ok(None);
        };
        upper = Some(upper.map_or(parent.at.as_i64(), |upper| upper.min(parent.at.as_i64())));
        let Some(next) = read_conversation(connection, parent.conversation_id)? else {
            return Ok(None);
        };
        conversation = next;
    }
}

fn scan_conversations_blocking(
    connection: &Connection,
    query: ConversationQuery,
    limit: usize,
    cursor: Option<Cursor>,
) -> Result<Page<ConversationRecord>, StorageError> {
    let start = scan_start(query.order, cursor.as_ref(), ScanOrder::Ascending)?;
    let (clause, param, direction) = scan_sql(start);
    let mut sql = format!(
        "SELECT record FROM {} WHERE {}",
        tables::CONVERSATIONS,
        clause
    );
    let mut values: Vec<SqlValue> = vec![SqlValue::Integer(param)];
    if let Some(owner) = query.owner_conversation_id {
        sql.push_str(" AND owner_conversation_id = ?");
        values.push(SqlValue::Integer(owner.as_i64()));
    }
    if let Some(owner) = query.owner_task_id {
        sql.push_str(" AND owner_task_id = ?");
        values.push(SqlValue::Integer(owner.as_i64()));
    }
    sql.push_str(&format!(" ORDER BY id {direction} LIMIT ?"));
    values.push(SqlValue::Integer(page_limit(limit)));
    let records = query_all_json::<ConversationRecord>(
        connection,
        &sql,
        params_from_iter(values.iter()),
        "conversation",
    )?;
    let (items, next) = page_of(&records, limit, start.order, |record| record.id.as_i64());
    Ok(Page { items, next })
}

fn scan_entries_blocking(
    connection: &Connection,
    query: EntryQuery,
    limit: usize,
    cursor: Option<Cursor>,
) -> Result<Page<EntryRecord>, StorageError> {
    let start = scan_start(query.order, cursor.as_ref(), ScanOrder::Descending)?;
    if start.order == ScanOrder::Ascending {
        scan_entries_ascending(connection, &query, limit, start.after)
    } else {
        scan_entries_descending(connection, &query, limit, start.after)
    }
}

fn scan_entries_descending(
    connection: &Connection,
    query: &EntryQuery,
    limit: usize,
    after: Option<i64>,
) -> Result<Page<EntryRecord>, StorageError> {
    let Some(mut conversation) = read_conversation(connection, query.conversation_id)? else {
        return Err(StorageError::Request(StorageRequestError::new(format!(
            "Unknown conversation: {}",
            query.conversation_id
        ))));
    };
    let mut upper = query.max_entry_id.map(|id| id.as_i64());
    if let Some(after) = after {
        let bound = after.saturating_sub(1);
        upper = Some(upper.map_or(bound, |upper| upper.min(bound)));
    }
    let mut values: Vec<EntryRecord> = Vec::new();
    loop {
        let mut clauses = vec!["conversation_id = ?".to_owned()];
        let mut binds: Vec<SqlValue> = vec![SqlValue::Integer(conversation.id.as_i64())];
        if let Some(min) = query.min_entry_id {
            clauses.push("id >= ?".to_owned());
            binds.push(SqlValue::Integer(min.as_i64()));
        }
        if let Some(upper) = upper {
            clauses.push("id <= ?".to_owned());
            binds.push(SqlValue::Integer(upper));
        }
        binds.push(SqlValue::Integer(remaining_limit(limit, values.len())));
        let sql = format!(
            "SELECT record FROM {} WHERE {} ORDER BY id DESC LIMIT ?",
            tables::ENTRIES,
            clauses.join(" AND ")
        );
        let rows = query_all_json(connection, &sql, params_from_iter(binds.iter()), "entry")?;
        values.extend(rows);
        if values.len() > limit {
            break;
        }
        let Some(parent) = conversation.parent else {
            break;
        };
        upper = Some(upper.map_or(parent.at.as_i64(), |upper| upper.min(parent.at.as_i64())));
        if let Some(min) = query.min_entry_id {
            if let Some(upper) = upper {
                if upper < min.as_i64() {
                    break;
                }
            }
        }
        let Some(next) = read_conversation(connection, parent.conversation_id)? else {
            break;
        };
        conversation = next;
    }
    let (items, next) = page_of(&values, limit, ScanOrder::Descending, |entry| {
        entry.id.as_i64()
    });
    Ok(Page { items, next })
}

fn scan_entries_ascending(
    connection: &Connection,
    query: &EntryQuery,
    limit: usize,
    after: Option<i64>,
) -> Result<Page<EntryRecord>, StorageError> {
    let Some(mut conversation) = read_conversation(connection, query.conversation_id)? else {
        return Err(StorageError::Request(StorageRequestError::new(format!(
            "Unknown conversation: {}",
            query.conversation_id
        ))));
    };
    let mut segments: Vec<(ConversationId, Option<i64>)> = Vec::new();
    let mut upper = query.max_entry_id.map(|id| id.as_i64());
    loop {
        segments.push((conversation.id, upper));
        let Some(parent) = conversation.parent else {
            break;
        };
        upper = Some(upper.map_or(parent.at.as_i64(), |upper| upper.min(parent.at.as_i64())));
        if let Some(min) = query.min_entry_id {
            if let Some(upper) = upper {
                if upper < min.as_i64() {
                    break;
                }
            }
        }
        let Some(next) = read_conversation(connection, parent.conversation_id)? else {
            break;
        };
        conversation = next;
    }
    let mut lower = query.min_entry_id.map(|id| id.as_i64());
    if let Some(after) = after {
        let bound = after.saturating_add(1);
        lower = Some(lower.map_or(bound, |lower| lower.max(bound)));
    }
    let mut values: Vec<EntryRecord> = Vec::new();
    for (segment, segment_upper) in segments.iter().rev() {
        let mut clauses = vec!["conversation_id = ?".to_owned()];
        let mut binds: Vec<SqlValue> = vec![SqlValue::Integer(segment.as_i64())];
        if let Some(lower) = lower {
            clauses.push("id >= ?".to_owned());
            binds.push(SqlValue::Integer(lower));
        }
        if let Some(segment_upper) = segment_upper {
            clauses.push("id <= ?".to_owned());
            binds.push(SqlValue::Integer(*segment_upper));
        }
        binds.push(SqlValue::Integer(remaining_limit(limit, values.len())));
        let sql = format!(
            "SELECT record FROM {} WHERE {} ORDER BY id ASC LIMIT ?",
            tables::ENTRIES,
            clauses.join(" AND ")
        );
        let rows = query_all_json(connection, &sql, params_from_iter(binds.iter()), "entry")?;
        values.extend(rows);
        if values.len() > limit {
            break;
        }
    }
    let (items, next) = page_of(&values, limit, ScanOrder::Ascending, |entry| {
        entry.id.as_i64()
    });
    Ok(Page { items, next })
}

fn scan_tasks_blocking(
    connection: &Connection,
    query: TaskQuery,
    limit: usize,
    cursor: Option<Cursor>,
) -> Result<Page<TaskRecord>, StorageError> {
    let start = scan_start(query.order, cursor.as_ref(), ScanOrder::Ascending)?;
    let (clause, param, direction) = scan_sql(start);
    let mut sql = format!("SELECT record FROM {} WHERE {}", tables::TASKS, clause);
    let mut values: Vec<SqlValue> = vec![SqlValue::Integer(param)];
    if let Some(conversation_id) = query.conversation_id {
        sql.push_str(" AND conversation_id = ?");
        values.push(SqlValue::Integer(conversation_id.as_i64()));
    }
    if let Some(kind) = &query.kind {
        sql.push_str(" AND kind = ?");
        values.push(SqlValue::Text(encode_indexed_string(kind)?));
    }
    if let Some(status) = query.status {
        sql.push_str(" AND status = ?");
        values.push(SqlValue::Text(status.to_string()));
    }
    if let Some(abort_requested) = query.abort_requested {
        sql.push_str(" AND abort_requested = ?");
        values.push(SqlValue::Integer(bool_int(abort_requested)));
    }
    if let Some(background) = query.background {
        sql.push_str(" AND background = ?");
        values.push(SqlValue::Integer(bool_int(background)));
    }
    sql.push_str(&format!(" ORDER BY id {direction} LIMIT ?"));
    values.push(SqlValue::Integer(page_limit(limit)));
    let records =
        query_all_json::<TaskRecord>(connection, &sql, params_from_iter(values.iter()), "task")?;
    let (items, next) = page_of(&records, limit, start.order, |record| record.id.as_i64());
    Ok(Page { items, next })
}

fn scan_submissions_blocking(
    connection: &Connection,
    query: SubmissionQuery,
    limit: usize,
    cursor: Option<Cursor>,
) -> Result<Page<SubmissionRecord>, StorageError> {
    let start = scan_start(query.order, cursor.as_ref(), ScanOrder::Ascending)?;
    let (clause, param, direction) = scan_sql(start);
    let mut sql = format!(
        "SELECT record FROM {} WHERE {}",
        tables::SUBMISSIONS,
        clause
    );
    let mut values: Vec<SqlValue> = vec![SqlValue::Integer(param)];
    if let Some(conversation_id) = query.conversation_id {
        sql.push_str(" AND conversation_id = ?");
        values.push(SqlValue::Integer(conversation_id.as_i64()));
    }
    if let Some(status) = query.status {
        sql.push_str(" AND status = ?");
        values.push(SqlValue::Text(status.to_string()));
    }
    sql.push_str(&format!(" ORDER BY id {direction} LIMIT ?"));
    values.push(SqlValue::Integer(page_limit(limit)));
    let records = query_all_json::<SubmissionRecord>(
        connection,
        &sql,
        params_from_iter(values.iter()),
        "submission",
    )?;
    let (items, next) = page_of(&records, limit, start.order, |record| record.id.as_i64());
    Ok(Page { items, next })
}

fn submission_by_request_blocking(
    connection: &Connection,
    conversation_id: ConversationId,
    request_id: &str,
) -> Result<Option<SubmissionRecord>, StorageError> {
    // `ORDER BY id DESC` makes the lookup deterministic even for a database
    // written before the unique-key migration: the newest record wins, which is
    // the in-memory backend's semantics. With migration 3 the key is unique, so
    // the ordering is a tie-break that can no longer matter.
    query_optional_json(
        connection,
        &format!(
            "SELECT record FROM {} WHERE conversation_id = ?1 AND request_id = ?2 ORDER BY id DESC LIMIT 1",
            tables::SUBMISSIONS
        ),
        params![conversation_id.as_i64(), encode_indexed_string(request_id)?],
        "submission",
    )
}

fn find_document_blocking(
    connection: &Connection,
    address: DocumentAddress,
    at: DocumentPoint,
) -> Result<Option<DocumentRecord>, StorageError> {
    let parts = address_parts(&address.kind, &address.scope, address.key.as_deref())?;
    let mut values: Vec<SqlValue> = vec![
        SqlValue::Text(parts.kind),
        SqlValue::Text(parts.scope_kind.to_owned()),
        SqlValue::Integer(parts.owner_id),
        SqlValue::Integer(parts.family),
        SqlValue::Text(parts.key_value),
    ];
    let sql = match at {
        DocumentPoint::Current => format!(
            "SELECT record FROM {} WHERE kind = ?1 AND scope_kind = ?2 AND owner_id = ?3 AND family = ?4 AND key_value = ?5 AND retired_at IS NULL ORDER BY created_at DESC LIMIT 1",
            tables::DOCUMENTS
        ),
        DocumentPoint::Seq(seq) => {
            values.push(SqlValue::Integer(seq.as_i64()));
            values.push(SqlValue::Integer(seq.as_i64()));
            format!(
                "SELECT record FROM {} WHERE kind = ?1 AND scope_kind = ?2 AND owner_id = ?3 AND family = ?4 AND key_value = ?5 AND created_at <= ?6 AND (retired_at IS NULL OR retired_at > ?7) ORDER BY created_at DESC LIMIT 1",
                tables::DOCUMENTS
            )
        }
    };
    query_optional_json(
        connection,
        &sql,
        params_from_iter(values.iter()),
        "document",
    )
}

fn document_blocking(
    connection: &mut Connection,
    id: DocumentId,
    at: DocumentPoint,
) -> Result<Option<StoredDocument>, StorageError> {
    // The record and revision queries must observe one committed state.
    let transaction = connection
        .transaction_with_behavior(TransactionBehavior::Deferred)
        .map_err(|error| sqlite_error("starting a document read", error))?;
    let stored = materialize_document(&transaction, id, at)?;
    drop(transaction);
    Ok(stored)
}

fn scan_documents_blocking(
    connection: &Connection,
    query: DocumentQuery,
    limit: usize,
    cursor: Option<Cursor>,
) -> Result<Page<DocumentRecord>, StorageError> {
    if let Some(cursor) = &cursor {
        if cursor.after < 0 {
            return Err(StorageError::Request(StorageRequestError::new(
                "Invalid storage cursor",
            )));
        }
    }
    let at = query.at.to_point();
    let (scope_kind, owner_id) = scope_columns(&query.scope);
    let mut clauses = vec![
        "scope_kind = ?".to_owned(),
        "owner_id = ?".to_owned(),
        "id > ?".to_owned(),
    ];
    let mut values: Vec<SqlValue> = vec![
        SqlValue::Text(scope_kind.to_owned()),
        SqlValue::Integer(owner_id),
        SqlValue::Integer(cursor.map_or(-1, |cursor| cursor.after)),
    ];
    if let Some(kind) = &query.kind {
        clauses.push("kind = ?".to_owned());
        values.push(SqlValue::Text(encode_indexed_string(kind)?));
    }
    match at {
        DocumentPoint::Current => clauses.push("retired_at IS NULL".to_owned()),
        DocumentPoint::Seq(seq) => {
            clauses.push("created_at <= ?".to_owned());
            clauses.push("(retired_at IS NULL OR retired_at > ?)".to_owned());
            values.push(SqlValue::Integer(seq.as_i64()));
            values.push(SqlValue::Integer(seq.as_i64()));
        }
    }
    values.push(SqlValue::Integer(page_limit(limit)));
    let sql = format!(
        "SELECT record FROM {} WHERE {} ORDER BY id LIMIT ?",
        tables::DOCUMENTS,
        clauses.join(" AND ")
    );
    let records = query_all_json::<DocumentRecord>(
        connection,
        &sql,
        params_from_iter(values.iter()),
        "document",
    )?;
    let (items, next) = page_of(&records, limit, ScanOrder::Ascending, |record| {
        record.id.as_i64()
    });
    Ok(Page { items, next })
}

fn materialize_document(
    connection: &Connection,
    id: DocumentId,
    at: DocumentPoint,
) -> Result<Option<StoredDocument>, StorageError> {
    let Some(record) = read_document_record(connection, id)? else {
        return Ok(None);
    };
    if at != DocumentPoint::Current && record.is_current_only() {
        return Err(StorageError::Request(StorageRequestError::new(format!(
            "Document {id} does not retain historical content"
        ))));
    }
    if !is_alive_at(&record, at) {
        return Ok(None);
    }
    let upper = match at {
        DocumentPoint::Current => i64::MAX,
        DocumentPoint::Seq(seq) => seq.as_i64(),
    };
    let base: Option<(i64, i64, String)> = connection
        .query_row(
            &format!(
                "SELECT seq, version, content FROM {} WHERE document_id = ?1 AND kind = 'base' AND seq <= ?2 ORDER BY seq DESC LIMIT 1",
                tables::DOCUMENT_REVISIONS
            ),
            params![id.as_i64(), upper],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()
        .map_err(|error| sqlite_error("reading a document base", error))?;
    let Some((base_seq, version, base_content)) = base else {
        return Err(StorageError::invalid(format!(
            "Document {id} is missing a required base"
        )));
    };
    let base_value: JsonObject = decode_json(&base_content, "document base")?;
    let revisions = query_revisions(
        connection,
        &format!(
            "SELECT seq, kind, version, content FROM {} WHERE document_id = ?1 AND seq > ?2 AND seq <= ?3 ORDER BY seq",
            tables::DOCUMENT_REVISIONS
        ),
        params![id.as_i64(), base_seq, upper],
    )?;
    let mut batches = Vec::with_capacity(revisions.len());
    for revision in &revisions {
        if revision.kind != "delta" || revision.version != version {
            return Err(StorageError::invalid(format!(
                "Document {id} crosses a stored version boundary without a base"
            )));
        }
        batches.push(decode_json::<Vec<crate::durable::delta::Op>>(
            &revision.content,
            "document delta",
        )?);
    }
    let deltas_since_base = batches.len();
    let materialized = apply_immutable_batches(&serde_json::Value::Object(base_value), batches)
        .map_err(|error| {
            StorageError::invalid(format!("Document {id} cannot be materialized: {error}"))
        })?;
    let serde_json::Value::Object(value) = materialized else {
        return Err(StorageError::invalid(format!(
            "Document {id} materialized to a non-object value"
        )));
    };
    Ok(Some(StoredDocument {
        record,
        version,
        value,
        deltas_since_base,
    }))
}

#[derive(Debug)]
struct RevisionRow {
    kind: String,
    version: i64,
    content: String,
}

fn query_revisions(
    connection: &Connection,
    sql: &str,
    params: impl rusqlite::Params,
) -> Result<Vec<RevisionRow>, StorageError> {
    let mut statement = connection
        .prepare(sql)
        .map_err(|error| sqlite_error("preparing a revision query", error))?;
    let rows = statement
        .query_map(params, |row| {
            Ok(RevisionRow {
                kind: row.get(1)?,
                version: row.get(2)?,
                content: row.get(3)?,
            })
        })
        .map_err(|error| sqlite_error("reading document revisions", error))?;
    let mut revisions = Vec::new();
    for row in rows {
        revisions.push(row.map_err(|error| sqlite_error("reading a document revision", error))?);
    }
    Ok(revisions)
}

fn current_document_id(
    connection: &Connection,
    parts: &AddressParts,
) -> Result<Option<DocumentId>, StorageError> {
    let id: Option<i64> = connection
        .query_row(
            &format!(
                "SELECT id FROM {} WHERE kind = ?1 AND scope_kind = ?2 AND owner_id = ?3 AND family = ?4 AND key_value = ?5 AND retired_at IS NULL LIMIT 1",
                tables::DOCUMENTS
            ),
            params![
                parts.kind,
                parts.scope_kind,
                parts.owner_id,
                parts.family,
                parts.key_value
            ],
            |row| row.get(0),
        )
        .optional()
        .map_err(|error| sqlite_error("reading the current document", error))?;
    Ok(id.map(DocumentId::from_raw))
}

// ─── shared helpers ─────────────────────────────────────────────────────────

fn sqlite_error(action: &str, error: rusqlite::Error) -> StorageError {
    StorageError::Io(format!("SQLite {action} failed: {error}"))
}

fn decode_json<T: DeserializeOwned>(text: &str, context: &str) -> Result<T, StorageError> {
    serde_json::from_str(text).map_err(|error| {
        StorageError::Io(format!(
            "SQLite stored {context} record is malformed: {error}"
        ))
    })
}

fn encode_json<T: Serialize + ?Sized>(value: &T) -> Result<String, StorageError> {
    serde_json::to_string(value)
        .map_err(|error| StorageError::invalid(format!("cannot encode a SQLite record: {error}")))
}

/// Encode a string stored in an indexed column.
///
/// Upstream JSON-encodes indexed strings so identities are lossless regardless
/// of the SQLite binding's Unicode handling; Rust strings are already valid
/// Unicode, but keeping the same encoding preserves the stored representation.
fn encode_indexed_string(value: &str) -> Result<String, StorageError> {
    serde_json::to_string(value)
        .map_err(|error| StorageError::invalid(format!("cannot encode a SQLite key: {error}")))
}

fn query_optional_json<T: DeserializeOwned>(
    connection: &Connection,
    sql: &str,
    params: impl rusqlite::Params,
    context: &str,
) -> Result<Option<T>, StorageError> {
    let text: Option<String> = connection
        .query_row(sql, params, |row| row.get(0))
        .optional()
        .map_err(|error| sqlite_error("querying a record", error))?;
    match text {
        Some(text) => Ok(Some(decode_json(&text, context)?)),
        None => Ok(None),
    }
}

fn query_all_json<T: DeserializeOwned>(
    connection: &Connection,
    sql: &str,
    params: impl rusqlite::Params,
    context: &str,
) -> Result<Vec<T>, StorageError> {
    let mut statement = connection
        .prepare(sql)
        .map_err(|error| sqlite_error("preparing a scan", error))?;
    let rows = statement
        .query_map(params, |row| row.get::<_, String>(0))
        .map_err(|error| sqlite_error("running a scan", error))?;
    let mut values = Vec::new();
    for row in rows {
        let text = row.map_err(|error| sqlite_error("reading a scan row", error))?;
        values.push(decode_json(&text, context)?);
    }
    Ok(values)
}

fn scan_sql(start: ScanStart) -> (&'static str, i64, &'static str) {
    match start.order {
        ScanOrder::Ascending => ("id > ?", start.after.unwrap_or(-1), "ASC"),
        ScanOrder::Descending => ("id < ?", start.after.unwrap_or(i64::MAX), "DESC"),
    }
}

fn page_limit(limit: usize) -> i64 {
    i64::try_from(limit.saturating_add(1)).unwrap_or(i64::MAX)
}

fn remaining_limit(limit: usize, values_len: usize) -> i64 {
    i64::try_from(limit.saturating_add(1).saturating_sub(values_len)).unwrap_or(i64::MAX)
}

fn bool_int(value: bool) -> i64 {
    if value {
        1
    } else {
        0
    }
}

fn scope_columns(scope: &DocumentScope) -> (&'static str, i64) {
    match scope {
        DocumentScope::Session => ("session", 0),
        DocumentScope::Conversation { conversation_id } => {
            ("conversation", conversation_id.as_i64())
        }
        DocumentScope::Task { task_id } => ("task", task_id.as_i64()),
    }
}

fn address_parts(
    kind: &str,
    scope: &DocumentScope,
    key: Option<&str>,
) -> Result<AddressParts, StorageError> {
    let (scope_kind, owner_id) = scope_columns(scope);
    Ok(AddressParts {
        kind: encode_indexed_string(kind)?,
        scope_kind,
        owner_id,
        family: if key.is_some() { 1 } else { 0 },
        key_value: encode_indexed_string(key.unwrap_or(""))?,
    })
}

fn is_alive_at(record: &DocumentRecord, at: DocumentPoint) -> bool {
    match at {
        DocumentPoint::Current => record.retired_at.is_none(),
        DocumentPoint::Seq(seq) => {
            record.created_at <= seq && record.retired_at.is_none_or(|retired| seq < retired)
        }
    }
}

// ─── Storage impl ───────────────────────────────────────────────────────────

impl Storage for SqliteStorage {
    fn commit(&self, writes: Vec<StorageWrite>) -> StorageFuture<'_, Result<Seq, StorageError>> {
        Box::pin(async move {
            self.run(move |connection| commit_blocking(connection, writes))
                .await
        })
    }

    fn mint_id(&self) -> StorageFuture<'_, Result<i64, StorageError>> {
        Box::pin(async move { self.run(mint_id_blocking).await })
    }

    fn conversation(
        &self,
        id: ConversationId,
    ) -> StorageFuture<'_, Result<Option<ConversationRecord>, StorageError>> {
        Box::pin(async move {
            self.run(move |connection| read_conversation(connection, id))
                .await
        })
    }

    fn scan_conversations(
        &self,
        query: ConversationQuery,
        limit: usize,
        cursor: Option<Cursor>,
    ) -> StorageFuture<'_, Result<Page<ConversationRecord>, StorageError>> {
        Box::pin(async move {
            self.run(move |connection| {
                scan_conversations_blocking(connection, query, limit, cursor)
            })
            .await
        })
    }

    fn entry(&self, id: EntryId) -> StorageFuture<'_, Result<Option<EntryWithSeq>, StorageError>> {
        Box::pin(async move { self.run(move |connection| read_entry(connection, id)).await })
    }

    fn entry_in(
        &self,
        conversation_id: ConversationId,
        id: EntryId,
    ) -> StorageFuture<'_, Result<Option<EntryWithSeq>, StorageError>> {
        Box::pin(async move {
            self.run(move |connection| read_entry_in(connection, conversation_id, id))
                .await
        })
    }

    fn find_latest_head_marker(
        &self,
        conversation_id: ConversationId,
        at_or_before_entry_id: Option<EntryId>,
    ) -> StorageFuture<'_, Result<Option<EntryRecord>, StorageError>> {
        Box::pin(async move {
            self.run(move |connection| {
                read_latest_head_marker(connection, conversation_id, at_or_before_entry_id)
            })
            .await
        })
    }

    fn scan_entries(
        &self,
        query: EntryQuery,
        limit: usize,
        cursor: Option<Cursor>,
    ) -> StorageFuture<'_, Result<Page<EntryRecord>, StorageError>> {
        Box::pin(async move {
            self.run(move |connection| scan_entries_blocking(connection, query, limit, cursor))
                .await
        })
    }

    fn task(&self, id: TaskId) -> StorageFuture<'_, Result<Option<TaskRecord>, StorageError>> {
        Box::pin(async move {
            self.run(move |connection| {
                query_optional_json(
                    connection,
                    &format!("SELECT record FROM {} WHERE id = ?1", tables::TASKS),
                    params![id.as_i64()],
                    "task",
                )
            })
            .await
        })
    }

    fn scan_tasks(
        &self,
        query: TaskQuery,
        limit: usize,
        cursor: Option<Cursor>,
    ) -> StorageFuture<'_, Result<Page<TaskRecord>, StorageError>> {
        Box::pin(async move {
            self.run(move |connection| scan_tasks_blocking(connection, query, limit, cursor))
                .await
        })
    }

    fn submission(
        &self,
        id: SubmissionId,
    ) -> StorageFuture<'_, Result<Option<SubmissionRecord>, StorageError>> {
        Box::pin(async move {
            self.run(move |connection| {
                query_optional_json(
                    connection,
                    &format!("SELECT record FROM {} WHERE id = ?1", tables::SUBMISSIONS),
                    params![id.as_i64()],
                    "submission",
                )
            })
            .await
        })
    }

    fn scan_submissions(
        &self,
        query: SubmissionQuery,
        limit: usize,
        cursor: Option<Cursor>,
    ) -> StorageFuture<'_, Result<Page<SubmissionRecord>, StorageError>> {
        Box::pin(async move {
            self.run(move |connection| scan_submissions_blocking(connection, query, limit, cursor))
                .await
        })
    }

    fn submission_by_request(
        &self,
        conversation_id: ConversationId,
        request_id: &str,
    ) -> StorageFuture<'_, Result<Option<SubmissionRecord>, StorageError>> {
        // Own the key so the returned future borrows only `self`.
        let request_id = request_id.to_owned();
        Box::pin(async move {
            self.run(move |connection| {
                submission_by_request_blocking(connection, conversation_id, &request_id)
            })
            .await
        })
    }

    fn find_document(
        &self,
        address: DocumentAddress,
        at: DocumentPoint,
    ) -> StorageFuture<'_, Result<Option<DocumentRecord>, StorageError>> {
        Box::pin(async move {
            self.run(move |connection| find_document_blocking(connection, address, at))
                .await
        })
    }

    fn document(
        &self,
        id: DocumentId,
        at: DocumentPoint,
    ) -> StorageFuture<'_, Result<Option<StoredDocument>, StorageError>> {
        Box::pin(async move {
            self.run(move |connection| document_blocking(connection, id, at))
                .await
        })
    }

    fn scan_documents(
        &self,
        query: DocumentQuery,
        limit: usize,
        cursor: Option<Cursor>,
    ) -> StorageFuture<'_, Result<Page<DocumentRecord>, StorageError>> {
        Box::pin(async move {
            self.run(move |connection| scan_documents_blocking(connection, query, limit, cursor))
                .await
        })
    }

    fn close(&self) -> StorageFuture<'_, Result<(), StorageError>> {
        Box::pin(async move {
            let inner = Arc::clone(&self.inner);
            tokio::task::spawn_blocking(move || {
                let mut guard = inner.connection.lock().map_err(|_| {
                    StorageError::Io("SQLite connection lock is poisoned".to_owned())
                })?;
                if let Some(connection) = guard.take() {
                    // Best-effort: the marker state is already durable, and
                    // dropping the connection checkpoints what it can.
                    let _ = connection.execute_batch("PRAGMA wal_checkpoint(TRUNCATE)");
                }
                Ok::<(), StorageError>(())
            })
            .await
            .map_err(|error| StorageError::Io(format!("SQLite close task failed: {error}")))?
        })
    }
}
