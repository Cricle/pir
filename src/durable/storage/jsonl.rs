//! Append-only JSONL implementation of [`Storage`].
//!
//! Ports upstream `src/storage/jsonl/storage.ts`. The durable state is:
//!
//! * `main.jsonl` — one commit marker per line, the log's commit point;
//! * `doc-<id>.jsonl` / `task-<id>.jsonl` — sidecars holding the bulk payloads
//!   (document content, live task records) that a marker references by
//!   `(seq, ordinal)`;
//! * `*.reclaim` — temporary files used to rewrite or delete a sidecar.
//!
//! # Atomic batches
//!
//! `commit` validates the batch, appends every affected sidecar (flushing them
//! first when `fsync` is enabled), then appends **one** main marker line. A
//! marker that is fully on disk is the commit: recovery replays markers in
//! order and materializes sidecar state from them. A crash before the marker
//! leaves only orphan sidecar lines, which recovery truncates away; a crash
//! mid-marker leaves a torn last line, which recovery truncates. After
//! `commit` returns, the in-memory view is updated, so later reads observe the
//! batch.
//!
//! # Recovery
//!
//! `open` reads `main.jsonl`, drops a torn trailing line, validates every
//! marker (format, strictly increasing sequence, operation shapes), then reads
//! the sidecars. Confirmed sidecar records are replayed; unconfirmed tails are
//! truncated. Sidecar data for a document whose content was reclaimed (a
//! current-only document replaced by a newer base, or a retired one) or for a
//! task that ended terminal is optional, so its absence is not corruption.
//!
//! # Corruption and poisoning
//!
//! Malformed complete lines, invalid UTF-8, unknown write types, out-of-order
//! sequences and confirmed-state inconsistencies raise
//! [`JsonlCorruptionError`]. An IO failure during `commit` poisons the
//! instance: the in-memory view may no longer match the log, so every later
//! operation fails with [`JsonlStoragePoisonedError`] until the storage is
//! reopened. Reads never poison.
//!
//! # Deviation from upstream
//!
//! With `fsync: true` the main marker is flushed as well as the sidecars. That
//! is strictly stronger than upstream (which flushes only sidecars before the
//! marker) and costs one sync per commit.

use crate::durable::errors::{
    JsonlCorruptionError, JsonlStoragePoisonedError, StorageError, StorageRequestError,
};
use crate::durable::ids::{ConversationId, DocumentId, Seq, SubmissionId, TaskId};
use crate::durable::storage::memory::MemoryStorage;
use crate::durable::types::{
    ConversationQuery, ConversationRecord, Cursor, DocumentAddress, DocumentContent,
    DocumentCreate, DocumentPoint, DocumentQuery, DocumentRecord, EntryQuery, EntryRecord,
    EntryWithSeq, Page, StorageFuture, StorageWrite, StoredDocument, SubmissionQuery,
    SubmissionRecord, TaskQuery, TaskRecord, TaskStatus,
};
use crate::durable::Storage;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::io::AsyncWriteExt;
use tokio::sync::Mutex;

/// The on-disk format version.
pub const FORMAT_VERSION: i64 = 1;
/// The main log's file name.
pub const MAIN_FILE: &str = "main.jsonl";
/// The mint journal's file name: one numeric ID per line, appended by
/// [`Storage::mint_id`].
///
/// A minted ID is not part of any commit record, so the in-memory watermark
/// (which only knows stored records) would forget it on reopen and could hand
/// the same ID out twice across a restart. The journal is append-only and is
/// not a commit point; recovery only reads it to seed the watermark.
const IDS_FILE: &str = "ids.jsonl";
/// The suffix of a sidecar rewrite in progress.
const RECLAIM_SUFFIX: &str = ".reclaim";

/// Options for [`JsonlStorage::open_with_options`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct JsonlStorageOptions {
    /// Flush every affected sidecar (and the marker) before `commit` returns.
    pub fsync: bool,
}

/// One operation stored in a main commit marker.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
// Wire-shaped (serde-tagged) enum: boxing a variant would only add an
// indirection at every construction and match site without changing the
// serialized bytes, and a marker holds a handful of operations at a time.
#[allow(clippy::large_enum_variant)]
enum MainOperation {
    #[serde(rename = "conversation")]
    Conversation { value: ConversationRecord },
    #[serde(rename = "entry")]
    Entry { value: EntryRecord },
    #[serde(rename = "submission")]
    Submission { value: SubmissionRecord },
    #[serde(rename = "task")]
    Task { value: TaskRecord },
    #[serde(rename = "task.sidecar")]
    TaskSidecar { id: TaskId, ordinal: i64 },
    #[serde(rename = "document.retire")]
    DocumentRetire { id: DocumentId },
    #[serde(rename = "document.create")]
    DocumentCreate {
        record: DocumentCreate,
        ordinal: i64,
    },
    #[serde(rename = "document.change")]
    DocumentChange { id: DocumentId, ordinal: i64 },
}

impl MainOperation {
    fn validate(&self, description: &str) -> Result<(), JsonlCorruptionError> {
        match self {
            Self::Conversation { .. } | Self::Entry { .. } | Self::Submission { .. } => Ok(()),
            Self::Task { value } => {
                if value.state.status() != TaskStatus::Terminal {
                    return Err(JsonlCorruptionError::new(format!(
                        "Invalid terminal task write in {description}"
                    )));
                }
                Ok(())
            }
            Self::TaskSidecar { ordinal, .. } => {
                if *ordinal < 0 {
                    return Err(JsonlCorruptionError::new(format!(
                        "Invalid task sidecar write in {description}"
                    )));
                }
                Ok(())
            }
            Self::DocumentRetire { .. } => Ok(()),
            Self::DocumentCreate { ordinal, .. } | Self::DocumentChange { ordinal, .. } => {
                if *ordinal < 0 {
                    return Err(JsonlCorruptionError::new(format!(
                        "Invalid document write in {description}"
                    )));
                }
                Ok(())
            }
        }
    }
}

/// One main commit marker line.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
struct MainMarker {
    format: i64,
    #[serde(rename = "type")]
    kind: String,
    seq: Seq,
    writes: Vec<MainOperation>,
}

/// The bulk payload a sidecar record carries.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
// Wire-shaped enum (see [`MainOperation`]): one sidecar record is materialized
// at a time, so the variant size is not a hot-path concern.
#[allow(clippy::large_enum_variant)]
enum SidecarPayload {
    #[serde(rename = "task")]
    Task { value: TaskRecord },
    #[serde(rename = "document")]
    Document {
        id: DocumentId,
        content: DocumentContent,
    },
}

/// One sidecar line.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
struct SidecarRecord {
    format: i64,
    #[serde(rename = "type")]
    kind: String,
    seq: Seq,
    ordinal: i64,
    payload: SidecarPayload,
}

impl SidecarRecord {
    fn validate(&self, description: &str) -> Result<(), JsonlCorruptionError> {
        if self.format != FORMAT_VERSION
            || self.kind != "record"
            || self.seq.as_i64() < 1
            || self.ordinal < 0
        {
            return Err(JsonlCorruptionError::new(format!(
                "Invalid sidecar record in {description}"
            )));
        }
        match &self.payload {
            SidecarPayload::Task { value } => {
                if value.state.status() == TaskStatus::Terminal {
                    return Err(JsonlCorruptionError::new(format!(
                        "Invalid live task record in {description}"
                    )));
                }
            }
            SidecarPayload::Document { content, .. } => match content {
                DocumentContent::Base { version, .. } | DocumentContent::Delta { version, .. } => {
                    if *version < 1 {
                        return Err(JsonlCorruptionError::new(format!(
                            "Invalid document content in {description}"
                        )));
                    }
                }
            },
        }
        Ok(())
    }
}

#[derive(Clone, Debug)]
struct EncodedCommit {
    marker: String,
    /// file name -> complete file content for this commit.
    sidecars: BTreeMap<String, String>,
}

#[derive(Clone, Debug)]
struct ParsedLine<T> {
    value: T,
    start: usize,
}

#[derive(Clone, Debug)]
struct ParsedFile<T> {
    lines: Vec<ParsedLine<T>>,
}

/// Append-only JSONL implementation of [`Storage`].
#[derive(Debug)]
pub struct JsonlStorage {
    directory: PathBuf,
    main_path: PathBuf,
    fsync: bool,
    memory: Arc<MemoryStorage>,
    current_only_documents: Arc<Mutex<HashSet<DocumentId>>>,
    live_task_sidecars: Arc<Mutex<HashSet<TaskId>>>,
    closed: Arc<Mutex<bool>>,
    poison: Arc<Mutex<Option<JsonlStoragePoisonedError>>>,
    commit_lock: Arc<Mutex<()>>,
}

/// The state one commit needs after it has been handed to a spawned task.
///
/// A commit is run as its own task so that dropping the caller's future after
/// `commit_inner` has started cannot interrupt the marker/apply critical
/// section. The commit lock is owned by this task, so a following commit cannot
/// prepare against a sequence this one has not applied yet.
#[derive(Clone, Debug)]
struct CommitShared {
    directory: PathBuf,
    main_path: PathBuf,
    fsync: bool,
    memory: Arc<MemoryStorage>,
    current_only_documents: Arc<Mutex<HashSet<DocumentId>>>,
    live_task_sidecars: Arc<Mutex<HashSet<TaskId>>>,
    closed: Arc<Mutex<bool>>,
    poison: Arc<Mutex<Option<JsonlStoragePoisonedError>>>,
    commit_lock: Arc<Mutex<()>>,
}

impl JsonlStorage {
    /// Open or create a storage directory.
    pub async fn open(directory: impl AsRef<Path>) -> Result<Self, StorageError> {
        Self::open_with_options(directory, JsonlStorageOptions::default()).await
    }

    /// Open or create a storage directory with explicit options.
    pub async fn open_with_options(
        directory: impl AsRef<Path>,
        options: JsonlStorageOptions,
    ) -> Result<Self, StorageError> {
        let directory = directory.as_ref().to_path_buf();
        tokio::fs::create_dir_all(&directory)
            .await
            .map_err(|error| StorageError::io("directory creation", error))?;
        let main_path = directory.join(MAIN_FILE);
        let storage = Self {
            directory,
            main_path,
            fsync: options.fsync,
            memory: Arc::new(MemoryStorage::new()),
            current_only_documents: Arc::new(Mutex::new(HashSet::new())),
            live_task_sidecars: Arc::new(Mutex::new(HashSet::new())),
            closed: Arc::new(Mutex::new(false)),
            poison: Arc::new(Mutex::new(None)),
            commit_lock: Arc::new(Mutex::new(())),
        };
        storage.recover().await?;
        Ok(storage)
    }

    /// The directory this storage owns.
    pub fn directory(&self) -> &Path {
        &self.directory
    }

    async fn assert_usable(&self) -> Result<(), StorageError> {
        ensure_usable(&self.closed, &self.poison).await
    }

    async fn poison(&self, cause: StorageError) -> StorageError {
        poison_error(&self.poison, cause).await
    }

    // ── commit ──────────────────────────────────────────────────────────────

    async fn commit_inner(&self, writes: Vec<StorageWrite>) -> Result<Seq, StorageError> {
        let shared = CommitShared {
            directory: self.directory.clone(),
            main_path: self.main_path.clone(),
            fsync: self.fsync,
            memory: Arc::clone(&self.memory),
            current_only_documents: Arc::clone(&self.current_only_documents),
            live_task_sidecars: Arc::clone(&self.live_task_sidecars),
            closed: Arc::clone(&self.closed),
            poison: Arc::clone(&self.poison),
            commit_lock: Arc::clone(&self.commit_lock),
        };
        // The spawned task owns the commit lock and every step after the
        // caller's first poll. Dropping the returned future detaches the task;
        // it still finishes marker append, apply, adoption and reclamation.
        tokio::spawn(async move { shared.commit(writes).await })
            .await
            .map_err(|error| StorageError::Io(format!("JSONL commit task failed: {error}")))?
    }

    /// The marker already published this state, so reclamation is retryable
    /// best-effort maintenance.
    async fn reclaim_sidecars(&self, replacements: &BTreeMap<String, String>) {
        reclaim_sidecars_files(&self.directory, self.fsync, replacements).await;
    }

    // ── recovery ────────────────────────────────────────────────────────────

    async fn recover(&self) -> Result<(), StorageError> {
        let main = read_lines(&self.main_path, MAIN_FILE).await?;
        let mut markers = Vec::with_capacity(main.lines.len());
        let mut previous_seq = 0i64;
        for (index, line) in main.lines.iter().enumerate() {
            let description = format!("{MAIN_FILE} line {}", index + 1);
            let marker: MainMarker = parse_json(&line.value, &description)?;
            if marker.format != FORMAT_VERSION || marker.kind != "commit" || marker.seq.as_i64() < 1
            {
                return Err(StorageError::Corruption(JsonlCorruptionError::new(
                    format!("Invalid commit marker in {description}"),
                )));
            }
            if marker.seq.as_i64() <= previous_seq {
                return Err(StorageError::Corruption(JsonlCorruptionError::new(
                    format!("Commit sequence does not strictly increase in {MAIN_FILE}"),
                )));
            }
            previous_seq = marker.seq.as_i64();
            for operation in &marker.writes {
                operation
                    .validate(&description)
                    .map_err(StorageError::Corruption)?;
            }
            markers.push(marker);
        }

        let listing = tokio::fs::read_dir(&self.directory)
            .await
            .map_err(|error| StorageError::io("directory listing", error))?;
        let mut names = Vec::new();
        {
            let mut entries = listing;
            while let Some(entry) = entries
                .next_entry()
                .await
                .map_err(|error| StorageError::io("directory listing", error))?
            {
                let file_type = entry
                    .file_type()
                    .await
                    .map_err(|error| StorageError::io("directory listing", error))?;
                if !file_type.is_file() {
                    continue;
                }
                let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                    continue;
                };
                if is_reclaim_file_name(&name) {
                    let _ = tokio::fs::remove_file(entry.path()).await;
                    continue;
                }
                if is_sidecar_file_name(&name) {
                    names.push(name);
                }
            }
        }
        names.sort();

        let mut parsed_files: BTreeMap<String, ParsedFile<SidecarRecord>> = BTreeMap::new();
        let mut record_by_key: HashMap<String, SidecarRecord> = HashMap::new();
        for file in &names {
            let path = self.directory.join(file);
            let parsed = read_lines(&path, file).await?;
            let mut records = Vec::with_capacity(parsed.lines.len());
            let mut previous: Option<SidecarRecord> = None;
            for (index, line) in parsed.lines.into_iter().enumerate() {
                let description = format!("{file} line {}", index + 1);
                let record: SidecarRecord = parse_json(&line.value, &description)?;
                record
                    .validate(&description)
                    .map_err(StorageError::Corruption)?;
                if let Some(previous) = &previous {
                    if record.seq < previous.seq
                        || (record.seq == previous.seq && record.ordinal <= previous.ordinal)
                    {
                        return Err(StorageError::Corruption(JsonlCorruptionError::new(
                            format!("Sidecar records are out of order in {file}"),
                        )));
                    }
                }
                previous = Some(record.clone());
                record_by_key.insert(
                    sidecar_key(file, record.seq, record.ordinal),
                    record.clone(),
                );
                records.push(ParsedLine {
                    value: record,
                    start: line.start,
                });
            }
            parsed_files.insert(file.clone(), ParsedFile { lines: records });
        }

        let mut current_only_documents: HashSet<DocumentId> = HashSet::new();
        let mut retired_documents: HashSet<DocumentId> = HashSet::new();
        let mut final_task_is_live: HashMap<TaskId, bool> = HashMap::new();
        for marker in &markers {
            for operation in &marker.writes {
                match operation {
                    MainOperation::DocumentCreate { record, .. } => {
                        if record.is_current_only() {
                            current_only_documents.insert(record.id);
                        }
                    }
                    MainOperation::DocumentRetire { id } => {
                        retired_documents.insert(*id);
                    }
                    MainOperation::Task { value } => {
                        final_task_is_live.insert(value.id, false);
                    }
                    MainOperation::TaskSidecar { id, .. } => {
                        final_task_is_live.insert(*id, true);
                    }
                    _ => {}
                }
            }
        }
        let retired_current_only: HashSet<DocumentId> = retired_documents
            .iter()
            .filter(|id| current_only_documents.contains(id))
            .copied()
            .collect();

        let mut latest_bases: HashMap<DocumentId, SidecarRecord> = HashMap::new();
        for marker in &markers {
            for operation in &marker.writes {
                let (id, ordinal) = match operation {
                    MainOperation::DocumentCreate { record, ordinal } => (record.id, *ordinal),
                    MainOperation::DocumentChange { id, ordinal } => (*id, *ordinal),
                    _ => continue,
                };
                if !current_only_documents.contains(&id) {
                    continue;
                }
                let key = sidecar_key(&sidecar_file_name("doc", id), marker.seq, ordinal);
                let Some(record) = record_by_key.get(&key) else {
                    continue;
                };
                let SidecarPayload::Document {
                    id: payload_id,
                    content,
                } = &record.payload
                else {
                    continue;
                };
                if *payload_id != id || !matches!(content, DocumentContent::Base { .. }) {
                    continue;
                }
                let replace = match latest_bases.get(&id) {
                    None => true,
                    Some(previous) => {
                        record.seq > previous.seq
                            || (record.seq == previous.seq && record.ordinal > previous.ordinal)
                    }
                };
                if replace {
                    latest_bases.insert(id, record.clone());
                }
            }
        }
        let is_before_latest_base = |id: DocumentId, seq: Seq, ordinal: i64| -> bool {
            latest_bases
                .get(&id)
                .is_some_and(|base| seq < base.seq || (seq == base.seq && ordinal < base.ordinal))
        };

        let terminal_tasks: HashSet<TaskId> = final_task_is_live
            .iter()
            .filter(|(_, live)| !**live)
            .map(|(id, _)| *id)
            .collect();
        let mut confirmed: HashSet<String> = HashSet::new();
        for marker in &markers {
            let mut writes = Vec::new();
            for operation in &marker.writes {
                match operation {
                    MainOperation::Conversation { value } => {
                        writes.push(StorageWrite::Conversation {
                            value: value.clone(),
                        })
                    }
                    MainOperation::Entry { value } => writes.push(StorageWrite::Entry {
                        value: value.clone(),
                    }),
                    MainOperation::Submission { value } => writes.push(StorageWrite::Submission {
                        value: value.clone(),
                    }),
                    MainOperation::Task { value } => writes.push(StorageWrite::Task {
                        value: value.clone(),
                    }),
                    MainOperation::DocumentRetire { id } => {
                        writes.push(StorageWrite::DocumentRetire { id: *id })
                    }
                    MainOperation::TaskSidecar { id, ordinal } => {
                        let optional = terminal_tasks.contains(id);
                        let record = confirm_record(
                            marker,
                            *ordinal,
                            &sidecar_file_name("task", *id),
                            &record_by_key,
                            &mut confirmed,
                            optional,
                        )?;
                        if let Some(record) = record {
                            let SidecarPayload::Task { value } = &record.payload else {
                                return Err(StorageError::Corruption(JsonlCorruptionError::new(
                                    format!(
                                        "Confirmed task sidecar data does not match commit {}",
                                        marker.seq
                                    ),
                                )));
                            };
                            if value.id != *id {
                                return Err(StorageError::Corruption(JsonlCorruptionError::new(
                                    format!(
                                        "Confirmed task sidecar data does not match commit {}",
                                        marker.seq
                                    ),
                                )));
                            }
                            if !optional {
                                writes.push(StorageWrite::Task {
                                    value: value.clone(),
                                });
                            }
                        }
                    }
                    MainOperation::DocumentCreate { record, ordinal } => {
                        let reclaimed = retired_current_only.contains(&record.id)
                            || is_before_latest_base(record.id, marker.seq, *ordinal);
                        let sidecar = confirm_record(
                            marker,
                            *ordinal,
                            &sidecar_file_name("doc", record.id),
                            &record_by_key,
                            &mut confirmed,
                            reclaimed,
                        )?;
                        let mut content: Option<DocumentContent> = None;
                        if let Some(sidecar_record) = sidecar {
                            let SidecarPayload::Document {
                                id: payload_id,
                                content: payload_content,
                            } = &sidecar_record.payload
                            else {
                                return Err(StorageError::Corruption(JsonlCorruptionError::new(
                                    format!(
                                        "Confirmed document sidecar data does not match commit {}",
                                        marker.seq
                                    ),
                                )));
                            };
                            if *payload_id != record.id {
                                return Err(StorageError::Corruption(JsonlCorruptionError::new(
                                    format!(
                                        "Confirmed document sidecar data does not match commit {}",
                                        marker.seq
                                    ),
                                )));
                            }
                            content = Some(payload_content.clone());
                        }
                        if let Some(content) = &content {
                            if !matches!(content, DocumentContent::Base { .. }) {
                                return Err(StorageError::Corruption(JsonlCorruptionError::new(
                                    format!(
                                        "Document creation lacks a confirmed base in commit {}",
                                        marker.seq
                                    ),
                                )));
                            }
                        }
                        writes.push(StorageWrite::DocumentCreate {
                            record: record.clone(),
                            content: match content {
                                Some(content) if !reclaimed => content,
                                _ => DocumentContent::Base {
                                    version: 1,
                                    value: serde_json::Map::new(),
                                },
                            },
                        });
                    }
                    MainOperation::DocumentChange { id, ordinal } => {
                        let reclaimed = retired_current_only.contains(id)
                            || is_before_latest_base(*id, marker.seq, *ordinal);
                        let sidecar = confirm_record(
                            marker,
                            *ordinal,
                            &sidecar_file_name("doc", *id),
                            &record_by_key,
                            &mut confirmed,
                            reclaimed,
                        )?;
                        if reclaimed {
                            continue;
                        }
                        let Some(record) = sidecar else {
                            continue;
                        };
                        let SidecarPayload::Document {
                            id: payload_id,
                            content,
                        } = &record.payload
                        else {
                            return Err(StorageError::Corruption(JsonlCorruptionError::new(
                                format!(
                                    "Confirmed document sidecar data does not match commit {}",
                                    marker.seq
                                ),
                            )));
                        };
                        if *payload_id != *id {
                            return Err(StorageError::Corruption(JsonlCorruptionError::new(
                                format!(
                                    "Confirmed document sidecar data does not match commit {}",
                                    marker.seq
                                ),
                            )));
                        }
                        writes.push(StorageWrite::DocumentChange {
                            id: *id,
                            content: content.clone(),
                        });
                    }
                }
            }
            let prepared = self
                .memory
                .prepare_commit_at(writes, marker.seq)
                .await
                .map_err(|error| {
                    StorageError::Corruption(JsonlCorruptionError::with_cause(
                        format!("Invalid committed state at sequence {}", marker.seq),
                        error,
                    ))
                })?;
            self.memory
                .apply_prepared(prepared)
                .await
                .map_err(|error| {
                    StorageError::Corruption(JsonlCorruptionError::with_cause(
                        format!("Invalid committed state at sequence {}", marker.seq),
                        error,
                    ))
                })?;
        }

        let mut reclamations: BTreeMap<String, String> = BTreeMap::new();
        for (file, parsed) in &parsed_files {
            let mut unconfirmed_at: Option<usize> = None;
            for line in &parsed.lines {
                let key = sidecar_key(file, line.value.seq, line.value.ordinal);
                if confirmed.contains(&key) {
                    if unconfirmed_at.is_some() {
                        return Err(StorageError::Corruption(JsonlCorruptionError::new(
                            format!("Confirmed record follows an unconfirmed tail in {file}"),
                        )));
                    }
                } else if unconfirmed_at.is_none() {
                    unconfirmed_at = Some(line.start);
                }
            }
            if let Some(offset) = unconfirmed_at {
                truncate_file(&self.directory.join(file), offset).await?;
            }

            let numeric_id = numeric_id_from_file(file).ok_or_else(|| {
                StorageError::Corruption(JsonlCorruptionError::new(format!(
                    "Invalid sidecar file name: {file}"
                )))
            })?;
            let confirmed_lines: Vec<&ParsedLine<SidecarRecord>> = parsed
                .lines
                .iter()
                .filter(|line| {
                    confirmed.contains(&sidecar_key(file, line.value.seq, line.value.ordinal))
                })
                .collect();
            let mut retained: Option<Vec<&ParsedLine<SidecarRecord>>> = None;
            if file.starts_with("task-") && terminal_tasks.contains(&TaskId::from_raw(numeric_id)) {
                retained = Some(Vec::new());
            } else if file.starts_with("doc-") {
                let document_id = DocumentId::from_raw(numeric_id);
                if retired_current_only.contains(&document_id) {
                    retained = Some(Vec::new());
                } else if latest_bases.contains_key(&document_id) {
                    retained = Some(
                        confirmed_lines
                            .iter()
                            .copied()
                            .filter(|line| {
                                !is_before_latest_base(
                                    document_id,
                                    line.value.seq,
                                    line.value.ordinal,
                                )
                            })
                            .collect(),
                    );
                }
            }
            if let Some(retained) = retained {
                if retained.len() < confirmed_lines.len() || retained.is_empty() {
                    let mut content = String::new();
                    for line in retained {
                        content.push_str(&json_line(&line.value)?);
                    }
                    reclamations.insert(file.clone(), content);
                }
            }
        }
        self.reclaim_sidecars(&reclamations).await;

        // Minted IDs are not part of any commit record; the journal restores
        // the watermark so a reopen never hands the same ID out twice. A torn
        // trailing line is dropped by `read_lines` (the caller never observed
        // that ID, so skipping it is safe); an unparseable complete line is
        // ignored for the same reason — the journal is advisory.
        let ids = read_lines(&self.directory.join(IDS_FILE), IDS_FILE).await?;
        let mut highest_minted = 0i64;
        for line in &ids.lines {
            if let Ok(id) = line.value.trim().parse::<i64>() {
                highest_minted = highest_minted.max(id);
            }
        }
        if highest_minted > 0 {
            self.memory.seed_minted_id(highest_minted).await;
        }

        *self.current_only_documents.lock().await = current_only_documents;
        *self.live_task_sidecars.lock().await = final_task_is_live
            .iter()
            .filter(|(_, live)| **live)
            .map(|(id, _)| *id)
            .collect();
        Ok(())
    }
}

async fn ensure_usable(
    closed: &Mutex<bool>,
    poison: &Mutex<Option<JsonlStoragePoisonedError>>,
) -> Result<(), StorageError> {
    if *closed.lock().await {
        return Err(StorageError::closed("JsonlStorage"));
    }
    if let Some(poison) = poison.lock().await.clone() {
        return Err(StorageError::Poisoned(poison));
    }
    Ok(())
}

async fn poison_error(
    poison: &Mutex<Option<JsonlStoragePoisonedError>>,
    cause: StorageError,
) -> StorageError {
    let mut poison = poison.lock().await;
    let error = poison
        .get_or_insert_with(|| JsonlStoragePoisonedError::new(cause.to_string()))
        .clone();
    StorageError::Poisoned(error)
}

async fn adopt_sidecar_state(
    current_only_documents: &Mutex<HashSet<DocumentId>>,
    live_task_sidecars: &Mutex<HashSet<TaskId>>,
    writes: &[StorageWrite],
) {
    let mut current_only = current_only_documents.lock().await;
    let mut live_tasks = live_task_sidecars.lock().await;
    for write in writes {
        match write {
            StorageWrite::DocumentCreate { record, .. } => {
                if record.is_current_only() {
                    current_only.insert(record.id);
                }
            }
            StorageWrite::Task { value } => {
                if value.state.status() == TaskStatus::Terminal {
                    live_tasks.remove(&value.id);
                } else {
                    live_tasks.insert(value.id);
                }
            }
            _ => {}
        }
    }
}

/// Reclamation is best-effort maintenance: the marker already published this
/// state, so an IO failure here is retried by the next recovery.
async fn reclaim_sidecars_files(
    directory: &Path,
    fsync: bool,
    replacements: &BTreeMap<String, String>,
) {
    if replacements.is_empty() {
        return;
    }
    for (file, content) in replacements {
        replace_sidecar_file(directory, fsync, file, content).await;
    }
}

async fn replace_sidecar_file(directory: &Path, fsync: bool, file: &str, content: &str) {
    let path = directory.join(file);
    if content.is_empty() {
        let _ = tokio::fs::remove_file(&path).await;
        return;
    }
    let temporary = directory.join(format!("{file}{RECLAIM_SUFFIX}"));
    if tokio::fs::write(&temporary, content.as_bytes())
        .await
        .is_err()
    {
        return;
    }
    if fsync {
        if let Ok(handle) = tokio::fs::OpenOptions::new()
            .write(true)
            .open(&temporary)
            .await
        {
            if handle.sync_all().await.is_err() {
                return;
            }
        } else {
            return;
        }
    }
    let _ = tokio::fs::rename(&temporary, &path).await;
}

impl CommitShared {
    async fn commit(self, writes: Vec<StorageWrite>) -> Result<Seq, StorageError> {
        let _guard = self.commit_lock.lock().await;
        ensure_usable(&self.closed, &self.poison).await?;
        let prepared = self.memory.prepare_commit(writes).await?;
        let encoded = encode_commit(prepared.seq, &prepared.writes)?;
        let reclamations = {
            let current_only = self.current_only_documents.lock().await;
            let live_tasks = self.live_task_sidecars.lock().await;
            plan_reclamations(&prepared.writes, &encoded, &current_only, &live_tasks)
        };

        let mut sidecar_paths = Vec::with_capacity(encoded.sidecars.len());
        for (file, content) in &encoded.sidecars {
            sidecar_paths.push((file.clone(), self.directory.join(file), content));
        }
        for (file, path, content) in &sidecar_paths {
            if let Err(error) = append_file(path, content.as_bytes(), self.fsync).await {
                return Err(poison_error(
                    &self.poison,
                    StorageError::io(format!("append to {file}"), error),
                )
                .await);
            }
        }
        if let Err(error) =
            append_file(&self.main_path, encoded.marker.as_bytes(), self.fsync).await
        {
            return Err(poison_error(
                &self.poison,
                StorageError::io(format!("append to {MAIN_FILE}"), error),
            )
            .await);
        }

        let seq = match self.memory.apply_prepared(prepared.clone()).await {
            Ok(seq) => seq,
            Err(error) => return Err(poison_error(&self.poison, error).await),
        };
        adopt_sidecar_state(
            &self.current_only_documents,
            &self.live_task_sidecars,
            &prepared.writes,
        )
        .await;
        reclaim_sidecars_files(&self.directory, self.fsync, &reclamations).await;
        Ok(seq)
    }
}

fn confirm_record(
    marker: &MainMarker,
    ordinal: i64,
    file: &str,
    record_by_key: &HashMap<String, SidecarRecord>,
    confirmed: &mut HashSet<String>,
    optional: bool,
) -> Result<Option<SidecarRecord>, StorageError> {
    let key = sidecar_key(file, marker.seq, ordinal);
    if confirmed.contains(&key) {
        return Err(StorageError::Corruption(JsonlCorruptionError::new(
            "Sidecar record is confirmed more than once",
        )));
    }
    let Some(record) = record_by_key.get(&key) else {
        if optional {
            return Ok(None);
        }
        return Err(StorageError::Corruption(JsonlCorruptionError::new(
            format!(
                "Missing confirmed sidecar record {file} at sequence {}",
                marker.seq
            ),
        )));
    };
    confirmed.insert(key);
    Ok(Some(record.clone()))
}

fn encode_commit(seq: Seq, writes: &[StorageWrite]) -> Result<EncodedCommit, StorageError> {
    let mut main = Vec::new();
    let mut sidecars: BTreeMap<String, String> = BTreeMap::new();
    let mut next_ordinal: i64 = 0;
    for write in writes {
        match write {
            StorageWrite::Conversation { value } => main.push(MainOperation::Conversation {
                value: value.clone(),
            }),
            StorageWrite::Entry { value } => main.push(MainOperation::Entry {
                value: value.clone(),
            }),
            StorageWrite::Submission { value } => main.push(MainOperation::Submission {
                value: value.clone(),
            }),
            StorageWrite::DocumentRetire { id } => {
                main.push(MainOperation::DocumentRetire { id: *id })
            }
            StorageWrite::Task { value } => {
                if value.state.status() == TaskStatus::Terminal {
                    main.push(MainOperation::Task {
                        value: value.clone(),
                    });
                } else {
                    let ordinal = next_ordinal;
                    next_ordinal += 1;
                    let file = sidecar_file_name("task", value.id);
                    let record = SidecarRecord {
                        format: FORMAT_VERSION,
                        kind: "record".to_owned(),
                        seq,
                        ordinal,
                        payload: SidecarPayload::Task {
                            value: value.clone(),
                        },
                    };
                    sidecars
                        .entry(file)
                        .or_default()
                        .push_str(&json_line(&record)?);
                    main.push(MainOperation::TaskSidecar {
                        id: value.id,
                        ordinal,
                    });
                }
            }
            StorageWrite::DocumentCreate { record, content } => {
                if !matches!(content, DocumentContent::Base { .. }) {
                    return Err(StorageError::invalid(format!(
                        "Document creation for {} must carry a base",
                        record.id
                    )));
                }
                let ordinal = next_ordinal;
                next_ordinal += 1;
                let file = sidecar_file_name("doc", record.id);
                let sidecar = SidecarRecord {
                    format: FORMAT_VERSION,
                    kind: "record".to_owned(),
                    seq,
                    ordinal,
                    payload: SidecarPayload::Document {
                        id: record.id,
                        content: content.clone(),
                    },
                };
                sidecars
                    .entry(file)
                    .or_default()
                    .push_str(&json_line(&sidecar)?);
                main.push(MainOperation::DocumentCreate {
                    record: record.clone(),
                    ordinal,
                });
            }
            StorageWrite::DocumentChange { id, content } => {
                let ordinal = next_ordinal;
                next_ordinal += 1;
                let file = sidecar_file_name("doc", *id);
                let sidecar = SidecarRecord {
                    format: FORMAT_VERSION,
                    kind: "record".to_owned(),
                    seq,
                    ordinal,
                    payload: SidecarPayload::Document {
                        id: *id,
                        content: content.clone(),
                    },
                };
                sidecars
                    .entry(file)
                    .or_default()
                    .push_str(&json_line(&sidecar)?);
                main.push(MainOperation::DocumentChange { id: *id, ordinal });
            }
            StorageWrite::DocumentCopy { record, .. } => {
                return Err(StorageError::invalid(format!(
                    "Document copy {} was not resolved before encoding",
                    record.id
                )));
            }
        }
    }
    let marker = MainMarker {
        format: FORMAT_VERSION,
        kind: "commit".to_owned(),
        seq,
        writes: main,
    };
    Ok(EncodedCommit {
        marker: json_line(&marker)?,
        sidecars,
    })
}

fn plan_reclamations(
    writes: &[StorageWrite],
    encoded: &EncodedCommit,
    current_only: &HashSet<DocumentId>,
    live_tasks: &HashSet<TaskId>,
) -> BTreeMap<String, String> {
    let mut created_current_only = HashSet::new();
    let mut retired_documents = HashSet::new();
    let mut base_documents = HashSet::new();
    let mut final_tasks: HashMap<TaskId, &TaskRecord> = HashMap::new();
    for write in writes {
        match write {
            StorageWrite::DocumentCreate { record, .. } => {
                if record.is_current_only() {
                    created_current_only.insert(record.id);
                }
            }
            StorageWrite::DocumentChange { id, content } => {
                if matches!(content, DocumentContent::Base { .. }) {
                    base_documents.insert(*id);
                }
            }
            StorageWrite::DocumentRetire { id } => {
                retired_documents.insert(*id);
            }
            StorageWrite::Task { value } => {
                final_tasks.insert(value.id, value);
            }
            _ => {}
        }
    }

    let is_current_only_document =
        |id: DocumentId| current_only.contains(&id) || created_current_only.contains(&id);
    let mut replacements = BTreeMap::new();
    for id in &retired_documents {
        if is_current_only_document(*id) {
            replacements.insert(sidecar_file_name("doc", *id), String::new());
        }
    }
    for id in &base_documents {
        if !is_current_only_document(*id) || retired_documents.contains(id) {
            continue;
        }
        let file = sidecar_file_name("doc", *id);
        if let Some(content) = encoded.sidecars.get(&file) {
            replacements.insert(file, content.clone());
        }
    }
    for (id, task) in &final_tasks {
        if task.state.status() == TaskStatus::Terminal
            && (live_tasks.contains(id)
                || encoded
                    .sidecars
                    .contains_key(&sidecar_file_name("task", *id)))
        {
            replacements.insert(sidecar_file_name("task", *id), String::new());
        }
    }
    replacements
}

fn sidecar_file_name(kind: &str, id: impl std::fmt::Display) -> String {
    format!("{kind}-{id}.jsonl")
}

fn is_sidecar_file_name(name: &str) -> bool {
    let Some(rest) = name
        .strip_prefix("doc-")
        .or_else(|| name.strip_prefix("task-"))
    else {
        return false;
    };
    let Some(digits) = rest.strip_suffix(".jsonl") else {
        return false;
    };
    is_canonical_id(digits)
}

fn is_reclaim_file_name(name: &str) -> bool {
    name.strip_suffix(RECLAIM_SUFFIX)
        .is_some_and(is_sidecar_file_name)
}

fn is_canonical_id(text: &str) -> bool {
    !text.is_empty()
        && text.bytes().all(|byte| byte.is_ascii_digit())
        && (text == "0" || !text.starts_with('0'))
}

fn numeric_id_from_file(name: &str) -> Option<i64> {
    let rest = name
        .strip_prefix("doc-")
        .or_else(|| name.strip_prefix("task-"))?;
    let digits = rest.strip_suffix(".jsonl")?;
    digits.parse::<i64>().ok()
}

fn sidecar_key(file: &str, seq: Seq, ordinal: i64) -> String {
    format!("{file}\u{1f}{}\u{1f}{ordinal}", seq.as_i64())
}

fn json_line<T: Serialize>(value: &T) -> Result<String, StorageError> {
    let mut text = serde_json::to_string(value)
        .map_err(|error| StorageError::invalid(format!("cannot encode a JSONL record: {error}")))?;
    text.push('\n');
    Ok(text)
}

fn parse_json<T: for<'de> Deserialize<'de>>(
    text: &str,
    description: &str,
) -> Result<T, StorageError> {
    serde_json::from_str(text).map_err(|error| {
        StorageError::Corruption(JsonlCorruptionError::with_cause(
            format!("Malformed complete {description}"),
            error,
        ))
    })
}

async fn append_file(path: &Path, data: &[u8], sync: bool) -> std::io::Result<()> {
    let mut file = tokio::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .await?;
    file.write_all(data).await?;
    // `tokio::fs::File` copies the payload into an internal buffer and hands
    // the actual write to a blocking task; `write_all` returning does not
    // mean that task finished. Flush before dropping the handle so a caller
    // that immediately reads/reopens the journal sees the complete marker
    // even when `fsync` is off (the default).
    file.flush().await?;
    if sync {
        file.sync_all().await?;
    }
    Ok(())
}

async fn truncate_file(path: &Path, length: usize) -> Result<(), StorageError> {
    let file = tokio::fs::OpenOptions::new()
        .write(true)
        .open(path)
        .await
        .map_err(|error| StorageError::io("truncation", error))?;
    file.set_len(length as u64)
        .await
        .map_err(|error| StorageError::io("truncation", error))
}

/// Read complete lines of a log file, truncating a torn trailing line first.
async fn read_lines(path: &Path, name: &str) -> Result<ParsedFile<String>, StorageError> {
    let bytes = match tokio::fs::read(path).await {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(ParsedFile { lines: Vec::new() });
        }
        Err(error) => return Err(StorageError::io(format!("read of {name}"), error)),
    };
    let mut complete = bytes.len();
    if complete > 0 && bytes[complete - 1] != b'\n' {
        complete = bytes
            .iter()
            .rposition(|byte| *byte == b'\n')
            .map(|index| index + 1)
            .unwrap_or(0);
        truncate_file(path, complete).await?;
    }
    let mut lines = Vec::new();
    let mut start = 0usize;
    let mut line_number = 1usize;
    for end in 0..complete {
        if bytes[end] != b'\n' {
            continue;
        }
        let text = std::str::from_utf8(&bytes[start..end]).map_err(|error| {
            StorageError::Corruption(JsonlCorruptionError::with_cause(
                format!("Invalid UTF-8 in complete {name} line {line_number}"),
                error,
            ))
        })?;
        lines.push(ParsedLine {
            value: text.to_owned(),
            start,
        });
        start = end + 1;
        line_number += 1;
    }
    Ok(ParsedFile { lines })
}

impl Storage for JsonlStorage {
    fn commit(&self, writes: Vec<StorageWrite>) -> StorageFuture<'_, Result<Seq, StorageError>> {
        Box::pin(async move { self.commit_inner(writes).await })
    }

    fn mint_id(&self) -> StorageFuture<'_, Result<i64, StorageError>> {
        Box::pin(async move {
            // Serialize the mint with commits so the journal append order matches
            // the IDs actually handed out (a mint never needs the memory lock for
            // the whole commit, but the journal line must land before returning).
            let _guard = self.commit_lock.lock().await;
            self.assert_usable().await?;
            let id = self.memory.mint_id().await?;
            let line = format!("{}\n", id);
            if let Err(error) =
                append_file(&self.directory.join(IDS_FILE), line.as_bytes(), self.fsync).await
            {
                // The ID was handed out internally but cannot be recorded: a
                // reopen would mint it again. Poison, matching the commit path's
                // IO-failure rule.
                return Err(self
                    .poison(StorageError::io(format!("append to {IDS_FILE}"), error))
                    .await);
            }
            Ok(id)
        })
    }

    fn conversation(
        &self,
        id: ConversationId,
    ) -> StorageFuture<'_, Result<Option<ConversationRecord>, StorageError>> {
        Box::pin(async move {
            self.assert_usable().await?;
            self.memory.conversation(id).await
        })
    }

    fn scan_conversations(
        &self,
        query: ConversationQuery,
        limit: usize,
        cursor: Option<Cursor>,
    ) -> StorageFuture<'_, Result<Page<ConversationRecord>, StorageError>> {
        Box::pin(async move {
            self.assert_usable().await?;
            self.memory.scan_conversations(query, limit, cursor).await
        })
    }

    fn entry(
        &self,
        id: crate::durable::ids::EntryId,
    ) -> StorageFuture<'_, Result<Option<EntryWithSeq>, StorageError>> {
        Box::pin(async move {
            self.assert_usable().await?;
            self.memory.entry(id).await
        })
    }

    fn entry_in(
        &self,
        conversation_id: ConversationId,
        id: crate::durable::ids::EntryId,
    ) -> StorageFuture<'_, Result<Option<EntryWithSeq>, StorageError>> {
        Box::pin(async move {
            self.assert_usable().await?;
            self.memory.entry_in(conversation_id, id).await
        })
    }

    fn find_latest_head_marker(
        &self,
        conversation_id: ConversationId,
        at_or_before_entry_id: Option<crate::durable::ids::EntryId>,
    ) -> StorageFuture<'_, Result<Option<EntryRecord>, StorageError>> {
        Box::pin(async move {
            self.assert_usable().await?;
            self.memory
                .find_latest_head_marker(conversation_id, at_or_before_entry_id)
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
            self.assert_usable().await?;
            self.memory.scan_entries(query, limit, cursor).await
        })
    }

    fn task(&self, id: TaskId) -> StorageFuture<'_, Result<Option<TaskRecord>, StorageError>> {
        Box::pin(async move {
            self.assert_usable().await?;
            self.memory.task(id).await
        })
    }

    fn scan_tasks(
        &self,
        query: TaskQuery,
        limit: usize,
        cursor: Option<Cursor>,
    ) -> StorageFuture<'_, Result<Page<TaskRecord>, StorageError>> {
        Box::pin(async move {
            self.assert_usable().await?;
            self.memory.scan_tasks(query, limit, cursor).await
        })
    }

    fn submission(
        &self,
        id: SubmissionId,
    ) -> StorageFuture<'_, Result<Option<SubmissionRecord>, StorageError>> {
        Box::pin(async move {
            self.assert_usable().await?;
            self.memory.submission(id).await
        })
    }

    fn scan_submissions(
        &self,
        query: SubmissionQuery,
        limit: usize,
        cursor: Option<Cursor>,
    ) -> StorageFuture<'_, Result<Page<SubmissionRecord>, StorageError>> {
        Box::pin(async move {
            self.assert_usable().await?;
            self.memory.scan_submissions(query, limit, cursor).await
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
            self.assert_usable().await?;
            self.memory
                .submission_by_request(conversation_id, &request_id)
                .await
        })
    }

    fn find_document(
        &self,
        address: DocumentAddress,
        at: DocumentPoint,
    ) -> StorageFuture<'_, Result<Option<DocumentRecord>, StorageError>> {
        Box::pin(async move {
            self.assert_usable().await?;
            self.memory.find_document(address, at).await
        })
    }

    fn document(
        &self,
        id: DocumentId,
        at: DocumentPoint,
    ) -> StorageFuture<'_, Result<Option<StoredDocument>, StorageError>> {
        Box::pin(async move {
            self.assert_usable().await?;
            self.memory.document(id, at).await
        })
    }

    fn scan_documents(
        &self,
        query: DocumentQuery,
        limit: usize,
        cursor: Option<Cursor>,
    ) -> StorageFuture<'_, Result<Page<DocumentRecord>, StorageError>> {
        Box::pin(async move {
            self.assert_usable().await?;
            self.memory.scan_documents(query, limit, cursor).await
        })
    }

    fn close(&self) -> StorageFuture<'_, Result<(), StorageError>> {
        Box::pin(async move {
            // Serialize with commits: a commit already in flight completes first,
            // and no later commit can pass the closed check.
            let _guard = self.commit_lock.lock().await;
            let mut closed = self.closed.lock().await;
            if *closed {
                return Ok(());
            }
            *closed = true;
            drop(closed);
            self.memory.close().await
        })
    }
}

/// Reject a cursor that is not a valid continuation, for callers that decode
/// cursors themselves.
pub fn validate_cursor(cursor: &Cursor) -> Result<(), StorageError> {
    if cursor.after < 0 {
        return Err(StorageError::Request(StorageRequestError::new(
            "Invalid storage cursor",
        )));
    }
    Ok(())
}

/// The sidecar files currently present in a storage directory, sorted.
pub async fn sidecar_files(directory: &Path) -> std::io::Result<Vec<String>> {
    let mut names = Vec::new();
    let mut entries = tokio::fs::read_dir(directory).await?;
    while let Some(entry) = entries.next_entry().await? {
        if entry.file_type().await?.is_file() {
            if let Some(name) = entry.file_name().to_str() {
                if is_sidecar_file_name(name) {
                    names.push(name.to_owned());
                }
            }
        }
    }
    names.sort();
    Ok(names)
}
