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
//! A live task's sidecar is compacted to its newest record (a complete
//! replacement), so only that newest record is required to rebuild the task.
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
use std::cell::Cell;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncSeekExt, AsyncWriteExt, BufReader};
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

/// A live task's sidecar is rewritten to its newest record once appending
/// another record would grow it past this many bytes.
///
/// [`TaskRecord`] is a complete replacement record, so every older snapshot in
/// the file is dead weight: it costs disk space and forces recovery to parse
/// and replay it. Compacting at a fixed byte threshold bounds both the file
/// size and its record count independently of how many times the task changes
/// state; the newest record always survives, so recovery still rebuilds the
/// latest state.
#[doc(hidden)]
pub const TASK_SIDECAR_COMPACT_BYTES: u64 = 32 * 1024;

/// Options for [`JsonlStorage::open_with_options`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct JsonlStorageOptions {
    /// Flush every affected sidecar (and the marker) before `commit` returns.
    pub fsync: bool,
}

/// One operation stored in a main commit marker.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
// Wire-shaped (serde-tagged) enum: `Box` only adds private indirection inside
// the binary and serializes exactly like the inner record.
enum MainOperation {
    #[serde(rename = "conversation")]
    Conversation { value: Box<ConversationRecord> },
    #[serde(rename = "entry")]
    Entry { value: Box<EntryRecord> },
    #[serde(rename = "submission")]
    Submission { value: Box<SubmissionRecord> },
    #[serde(rename = "task")]
    Task { value: Box<TaskRecord> },
    #[serde(rename = "task.sidecar")]
    TaskSidecar { id: TaskId, ordinal: i64 },
    #[serde(rename = "document.retire")]
    DocumentRetire { id: DocumentId },
    #[serde(rename = "document.create")]
    DocumentCreate {
        record: Box<DocumentCreate>,
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
// Wire-shaped enum (see [`MainOperation`]): `Box` serializes like the record.
enum SidecarPayload {
    #[serde(rename = "task")]
    Task { value: Box<TaskRecord> },
    #[serde(rename = "document")]
    Document {
        id: DocumentId,
        content: DocumentContent,
    },
}

/// One sidecar line.
#[derive(Debug, PartialEq, Serialize, Deserialize)]
struct SidecarRecord {
    format: i64,
    #[serde(rename = "type")]
    kind: String,
    seq: Seq,
    ordinal: i64,
    payload: SidecarPayload,
}

impl Clone for SidecarRecord {
    fn clone(&self) -> Self {
        // Test seam (R1-L3-008): recovery must keep one copy of each sidecar
        // record and move it into the replayed write instead of deep-cloning
        // the payload into an index or a second parse list.
        SIDECAR_RECORD_CLONES.with(|clones| clones.set(clones.get().saturating_add(1)));
        Self {
            format: self.format,
            kind: self.kind.clone(),
            seq: self.seq,
            ordinal: self.ordinal,
            payload: self.payload.clone(),
        }
    }
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

/// Where one sidecar line starts, plus the key that a marker confirms it with.
///
/// Recovery keeps this instead of the parsed record so a sidecar file's
/// bookkeeping costs a few bytes per line rather than a full copy of its
/// payload.
#[derive(Clone, Copy, Debug)]
struct SidecarLine {
    seq: Seq,
    ordinal: i64,
    start: u64,
}

/// What recovery must do with one sidecar file after replay.
#[derive(Clone, Copy, Debug)]
enum SidecarReclamation {
    /// The file holds nothing that must survive (a terminal task or a retired
    /// current-only document).
    Delete,
    /// Rewrite the file to the bytes starting at this offset, streaming them
    /// from the old file instead of building the whole content in memory.
    KeepFrom(u64),
}

/// One line handed out by [`LogLineReader`]. `text` borrows the reader's
/// reusable buffer and excludes the trailing `\n`.
struct LogLine<'a> {
    number: usize,
    start: u64,
    text: &'a str,
}

/// A streaming line reader for JSONL logs.
///
/// A torn (newline-less) trailing line is truncated on [`Self::open`], matching
/// the whole-file reader this replaces. Lines are then read one at a time
/// through [`tokio::io::BufReader`]: the file and its decoded text are never
/// materialized in full, so a large session bounds recovery's line overhead to
/// one line instead of the whole log.
struct LogLineReader {
    name: String,
    reader: Option<BufReader<tokio::fs::File>>,
    buffer: Vec<u8>,
    offset: u64,
    line_number: usize,
}

impl LogLineReader {
    async fn open(path: &Path, name: &str) -> Result<Self, StorageError> {
        truncate_torn_tail(path, name).await?;
        let reader = match tokio::fs::File::open(path).await {
            Ok(file) => Some(BufReader::new(file)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return Err(StorageError::io(format!("read of {name}"), error)),
        };
        Ok(Self {
            name: name.to_owned(),
            reader,
            buffer: Vec::new(),
            offset: 0,
            line_number: 0,
        })
    }

    async fn next_line(&mut self) -> Result<Option<LogLine<'_>>, StorageError> {
        let Some(reader) = self.reader.as_mut() else {
            return Ok(None);
        };
        self.buffer.clear();
        let count = reader
            .read_until(b'\n', &mut self.buffer)
            .await
            .map_err(|error| StorageError::io(format!("read of {}", self.name), error))?;
        if count == 0 {
            return Ok(None);
        }
        let start = self.offset;
        self.offset += count as u64;
        self.line_number += 1;
        if self.buffer.last() != Some(&b'\n') {
            // `open` truncated a torn tail, so a partial line here means the
            // file changed underneath us. Ignore it like a torn tail rather
            // than parsing a fragment.
            return Ok(None);
        }
        let text = std::str::from_utf8(&self.buffer[..self.buffer.len() - 1]).map_err(|error| {
            StorageError::Corruption(JsonlCorruptionError::with_cause(
                format!(
                    "Invalid UTF-8 in complete {} line {}",
                    self.name, self.line_number
                ),
                error,
            ))
        })?;
        Ok(Some(LogLine {
            number: self.line_number,
            start,
            text,
        }))
    }
}

/// Truncate a torn trailing line (the bytes after the last `\n`) before a
/// [`LogLineReader`] starts. Missing files are fine (they read as empty).
async fn truncate_torn_tail(path: &Path, name: &str) -> Result<(), StorageError> {
    let mut file = match tokio::fs::File::open(path).await {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(StorageError::io(format!("read of {name}"), error)),
    };
    let length = file
        .metadata()
        .await
        .map_err(|error| StorageError::io(format!("read of {name}"), error))?
        .len();
    if length == 0 {
        return Ok(());
    }
    file.seek(std::io::SeekFrom::Start(length - 1))
        .await
        .map_err(|error| StorageError::io(format!("read of {name}"), error))?;
    let mut last = [0u8; 1];
    file.read_exact(&mut last)
        .await
        .map_err(|error| StorageError::io(format!("read of {name}"), error))?;
    if last[0] == b'\n' {
        return Ok(());
    }

    // Scan backwards in fixed-size chunks for the last complete line. The
    // buffer is one chunk regardless of file size.
    let mut position = length;
    let mut chunk = vec![0u8; 64 * 1024];
    let complete = loop {
        let start = position.saturating_sub(chunk.len() as u64);
        let span = (position - start) as usize;
        file.seek(std::io::SeekFrom::Start(start))
            .await
            .map_err(|error| StorageError::io(format!("read of {name}"), error))?;
        file.read_exact(&mut chunk[..span])
            .await
            .map_err(|error| StorageError::io(format!("read of {name}"), error))?;
        if let Some(index) = chunk[..span].iter().rposition(|byte| *byte == b'\n') {
            break start + index as u64 + 1;
        }
        if start == 0 {
            break 0;
        }
        position = start;
    };
    drop(file);
    truncate_file(path, complete).await
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

    // ── recovery ────────────────────────────────────────────────────────────

    async fn recover(&self) -> Result<(), StorageError> {
        // ── pass 1: validate the main log, keeping only a light index ─────────
        //
        // Marker lines carry the session's entries (including `model`/`data`),
        // so holding every `MainMarker` alive would keep a full second copy of
        // the session. Validate here and retain only what replay and sidecar
        // reclamation need; replay re-reads the log line by line below.
        let mut previous_seq = 0i64;
        let mut document_operations: HashMap<DocumentId, Vec<(Seq, i64)>> = HashMap::new();
        let mut current_only_documents: HashSet<DocumentId> = HashSet::new();
        let mut retired_documents: HashSet<DocumentId> = HashSet::new();
        let mut final_task_is_live: HashMap<TaskId, bool> = HashMap::new();
        // The newest sidecar record a live task may need. Older live-task
        // records are superseded snapshots that compaction is free to drop.
        let mut latest_task_markers: HashMap<TaskId, (Seq, i64)> = HashMap::new();
        let mut main_lines = LogLineReader::open(&self.main_path, MAIN_FILE).await?;
        while let Some(line) = main_lines.next_line().await? {
            let description = format!("{MAIN_FILE} line {}", line.number);
            let marker: MainMarker = parse_json(line.text, &description)?;
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
                match operation {
                    MainOperation::DocumentCreate { record, ordinal } => {
                        if record.is_current_only() {
                            current_only_documents.insert(record.id);
                        }
                        document_operations
                            .entry(record.id)
                            .or_default()
                            .push((marker.seq, *ordinal));
                    }
                    MainOperation::DocumentChange { id, ordinal } => {
                        document_operations
                            .entry(*id)
                            .or_default()
                            .push((marker.seq, *ordinal));
                    }
                    MainOperation::DocumentRetire { id } => {
                        retired_documents.insert(*id);
                    }
                    MainOperation::Task { value } => {
                        final_task_is_live.insert(value.id, false);
                    }
                    MainOperation::TaskSidecar { id, ordinal } => {
                        final_task_is_live.insert(*id, true);
                        latest_task_markers.insert(*id, (marker.seq, *ordinal));
                    }
                    _ => {}
                }
            }
        }
        drop(main_lines);

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

        // Sidecar records are parsed exactly once. `record_by_key` owns the
        // only copy (`Arc` shares it with no one until replay moves it out);
        // `parsed_files` keeps only per-line keys and byte offsets for
        // truncation and reclamation.
        let mut parsed_files: BTreeMap<String, Vec<SidecarLine>> = BTreeMap::new();
        let mut record_by_key: HashMap<String, Arc<SidecarRecord>> = HashMap::new();
        for file in &names {
            let path = self.directory.join(file);
            let mut lines = LogLineReader::open(&path, file).await?;
            let mut records = Vec::new();
            let mut previous: Option<(Seq, i64)> = None;
            while let Some(line) = lines.next_line().await? {
                let description = format!("{file} line {}", line.number);
                let record: SidecarRecord = parse_json(line.text, &description)?;
                record
                    .validate(&description)
                    .map_err(StorageError::Corruption)?;
                if let Some((previous_seq, previous_ordinal)) = previous {
                    if record.seq < previous_seq
                        || (record.seq == previous_seq && record.ordinal <= previous_ordinal)
                    {
                        return Err(StorageError::Corruption(JsonlCorruptionError::new(
                            format!("Sidecar records are out of order in {file}"),
                        )));
                    }
                }
                previous = Some((record.seq, record.ordinal));
                let seq = record.seq;
                let ordinal = record.ordinal;
                record_by_key.insert(sidecar_key(file, seq, ordinal), Arc::new(record));
                records.push(SidecarLine {
                    seq,
                    ordinal,
                    start: line.start,
                });
            }
            parsed_files.insert(file.clone(), records);
        }

        let retired_current_only: HashSet<DocumentId> = retired_documents
            .iter()
            .filter(|id| current_only_documents.contains(id))
            .copied()
            .collect();

        // Only the winning base's key is kept. The old code cloned the whole
        // base sidecar record here, which for a large document was a third
        // full copy of its content.
        let mut latest_bases: HashMap<DocumentId, (Seq, i64)> = HashMap::new();
        for (id, operations) in &document_operations {
            if !current_only_documents.contains(id) {
                continue;
            }
            let file = sidecar_file_name("doc", *id);
            for (seq, ordinal) in operations {
                let key = sidecar_key(&file, *seq, *ordinal);
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
                if *payload_id != *id || !matches!(content, DocumentContent::Base { .. }) {
                    continue;
                }
                let replace = match latest_bases.get(id) {
                    None => true,
                    Some((previous_seq, previous_ordinal)) => {
                        *seq > *previous_seq
                            || (*seq == *previous_seq && *ordinal > *previous_ordinal)
                    }
                };
                if replace {
                    latest_bases.insert(*id, (*seq, *ordinal));
                }
            }
        }
        let is_before_latest_base = |id: DocumentId, seq: Seq, ordinal: i64| -> bool {
            latest_bases
                .get(&id)
                .is_some_and(|(base_seq, base_ordinal)| {
                    seq < *base_seq || (seq == *base_seq && ordinal < *base_ordinal)
                })
        };

        let terminal_tasks: HashSet<TaskId> = final_task_is_live
            .iter()
            .filter(|(_, live)| !**live)
            .map(|(id, _)| *id)
            .collect();
        // ── pass 2: replay markers one at a time ─────────────────────────────
        //
        // Consuming each `MainMarker` lets every operation move its payload
        // into the `StorageWrite`; sidecar records likewise leave their `Arc`
        // by value. Nothing on this path deep-clones a record.
        let mut confirmed: HashSet<String> = HashSet::new();
        let mut main_lines = LogLineReader::open(&self.main_path, MAIN_FILE).await?;
        while let Some(line) = main_lines.next_line().await? {
            let description = format!("{MAIN_FILE} line {}", line.number);
            let marker: MainMarker = parse_json(line.text, &description)?;
            let marker_seq = marker.seq;
            let mut writes = Vec::with_capacity(marker.writes.len());
            for operation in marker.writes {
                match operation {
                    MainOperation::Conversation { value } => {
                        writes.push(StorageWrite::Conversation { value: *value })
                    }
                    MainOperation::Entry { value } => {
                        writes.push(StorageWrite::Entry { value: *value })
                    }
                    MainOperation::Submission { value } => {
                        writes.push(StorageWrite::Submission { value: *value })
                    }
                    MainOperation::Task { value } => writes.push(StorageWrite::Task {
                        value: std::sync::Arc::new(*value),
                    }),
                    MainOperation::DocumentRetire { id } => {
                        writes.push(StorageWrite::DocumentRetire { id })
                    }
                    MainOperation::TaskSidecar { id, ordinal } => {
                        let terminal = terminal_tasks.contains(&id);
                        let is_latest =
                            latest_task_markers
                                .get(&id)
                                .is_some_and(|&(seq, latest_ordinal)| {
                                    seq == marker_seq && latest_ordinal == ordinal
                                });
                        // A live task keeps only its newest sidecar record (the
                        // older snapshots are complete replacements that a
                        // compaction, or a crash before one, may have dropped).
                        // Only the newest record is required to rebuild the
                        // state; a terminal task needs no sidecar at all.
                        let optional = terminal || !is_latest;
                        let record = confirm_record(
                            marker_seq,
                            ordinal,
                            &sidecar_file_name("task", id),
                            &mut record_by_key,
                            &mut confirmed,
                            optional,
                        )?;
                        if let Some(record) = record {
                            let record = into_owned_record(record);
                            let SidecarPayload::Task { value } = record.payload else {
                                return Err(StorageError::Corruption(JsonlCorruptionError::new(
                                    format!(
                                        "Confirmed task sidecar data does not match commit {marker_seq}"
                                    ),
                                )));
                            };
                            if value.id != id {
                                return Err(StorageError::Corruption(JsonlCorruptionError::new(
                                    format!(
                                        "Confirmed task sidecar data does not match commit {marker_seq}"
                                    ),
                                )));
                            }
                            if !terminal {
                                writes.push(StorageWrite::Task {
                                    value: std::sync::Arc::new(*value),
                                });
                            }
                        }
                    }
                    MainOperation::DocumentCreate { record, ordinal } => {
                        let reclaimed = retired_current_only.contains(&record.id)
                            || is_before_latest_base(record.id, marker_seq, ordinal);
                        let sidecar = confirm_record(
                            marker_seq,
                            ordinal,
                            &sidecar_file_name("doc", record.id),
                            &mut record_by_key,
                            &mut confirmed,
                            reclaimed,
                        )?;
                        let mut content: Option<DocumentContent> = None;
                        if let Some(sidecar_record) = sidecar {
                            let sidecar_record = into_owned_record(sidecar_record);
                            let SidecarPayload::Document {
                                id: payload_id,
                                content: payload_content,
                            } = sidecar_record.payload
                            else {
                                return Err(StorageError::Corruption(JsonlCorruptionError::new(
                                    format!(
                                        "Confirmed document sidecar data does not match commit {marker_seq}"
                                    ),
                                )));
                            };
                            if payload_id != record.id {
                                return Err(StorageError::Corruption(JsonlCorruptionError::new(
                                    format!(
                                        "Confirmed document sidecar data does not match commit {marker_seq}"
                                    ),
                                )));
                            }
                            content = Some(payload_content);
                        }
                        if let Some(content) = &content {
                            if !matches!(content, DocumentContent::Base { .. }) {
                                return Err(StorageError::Corruption(JsonlCorruptionError::new(
                                    format!(
                                        "Document creation lacks a confirmed base in commit {marker_seq}"
                                    ),
                                )));
                            }
                        }
                        writes.push(StorageWrite::DocumentCreate {
                            record: *record,
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
                        let reclaimed = retired_current_only.contains(&id)
                            || is_before_latest_base(id, marker_seq, ordinal);
                        let sidecar = confirm_record(
                            marker_seq,
                            ordinal,
                            &sidecar_file_name("doc", id),
                            &mut record_by_key,
                            &mut confirmed,
                            reclaimed,
                        )?;
                        if reclaimed {
                            continue;
                        }
                        let Some(record) = sidecar else {
                            continue;
                        };
                        let record = into_owned_record(record);
                        let SidecarPayload::Document {
                            id: payload_id,
                            content,
                        } = record.payload
                        else {
                            return Err(StorageError::Corruption(JsonlCorruptionError::new(
                                format!(
                                    "Confirmed document sidecar data does not match commit {marker_seq}"
                                ),
                            )));
                        };
                        if payload_id != id {
                            return Err(StorageError::Corruption(JsonlCorruptionError::new(
                                format!(
                                    "Confirmed document sidecar data does not match commit {marker_seq}"
                                ),
                            )));
                        }
                        writes.push(StorageWrite::DocumentChange { id, content });
                    }
                }
            }
            let prepared = self
                .memory
                .prepare_commit_at(writes, marker_seq)
                .await
                .map_err(|error| {
                    StorageError::Corruption(JsonlCorruptionError::with_cause(
                        format!("Invalid committed state at sequence {marker_seq}"),
                        error,
                    ))
                })?;
            self.memory
                .apply_prepared(prepared)
                .await
                .map_err(|error| {
                    StorageError::Corruption(JsonlCorruptionError::with_cause(
                        format!("Invalid committed state at sequence {marker_seq}"),
                        error,
                    ))
                })?;
        }
        drop(main_lines);
        // Unconfirmed records are not replayed and hold no further interest.
        // Release every remaining record before building the (streamed)
        // reclamation rewrites so the two peaks cannot add up.
        drop(record_by_key);

        let mut reclamations: BTreeMap<String, SidecarReclamation> = BTreeMap::new();
        for (file, lines) in &parsed_files {
            let mut unconfirmed_at: Option<u64> = None;
            let mut confirmed_count = 0usize;
            for line in lines {
                let key = sidecar_key(file, line.seq, line.ordinal);
                if confirmed.contains(&key) {
                    confirmed_count += 1;
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
            if file.starts_with("task-") {
                let task_id = TaskId::from_raw(numeric_id);
                if terminal_tasks.contains(&task_id) {
                    reclamations.insert(file.clone(), SidecarReclamation::Delete);
                    continue;
                }
                // A live task needs only its newest record. A crash between
                // the marker append and its compaction leaves older snapshots
                // behind; drop that prefix here, streaming the retained tail.
                if let Some(latest) = latest_task_markers.get(&task_id) {
                    if let Some(first_retained) = lines
                        .iter()
                        .find(|line| (line.seq, line.ordinal) >= *latest)
                    {
                        if first_retained.start > 0 {
                            reclamations.insert(
                                file.clone(),
                                SidecarReclamation::KeepFrom(first_retained.start),
                            );
                        }
                    }
                }
                continue;
            }
            if !file.starts_with("doc-") {
                continue;
            }
            let document_id = DocumentId::from_raw(numeric_id);
            if retired_current_only.contains(&document_id) {
                reclamations.insert(file.clone(), SidecarReclamation::Delete);
                continue;
            }
            let Some((base_seq, base_ordinal)) = latest_bases.get(&document_id).copied() else {
                continue;
            };
            let is_before_base = |line: &SidecarLine| {
                line.seq < base_seq || (line.seq == base_seq && line.ordinal < base_ordinal)
            };
            let retained_count = lines
                .iter()
                .filter(|line| {
                    confirmed.contains(&sidecar_key(file, line.seq, line.ordinal))
                        && !is_before_base(line)
                })
                .count();
            if retained_count < confirmed_count {
                let first_retained = lines.iter().find(|line| {
                    confirmed.contains(&sidecar_key(file, line.seq, line.ordinal))
                        && !is_before_base(line)
                });
                if let Some(first_retained) = first_retained {
                    reclamations.insert(
                        file.clone(),
                        SidecarReclamation::KeepFrom(first_retained.start),
                    );
                }
            }
        }
        reclaim_recovered_sidecars(&self.directory, self.fsync, &reclamations).await;

        // Minted IDs are not part of any commit record; the journal restores
        // the watermark so a reopen never hands the same ID out twice. A torn
        // trailing line is dropped by the reader (the caller never observed
        // that ID, so skipping it is safe); an unparseable complete line is
        // ignored for the same reason — the journal is advisory.
        let mut highest_minted = 0i64;
        let mut ids = LogLineReader::open(&self.directory.join(IDS_FILE), IDS_FILE).await?;
        while let Some(line) = ids.next_line().await? {
            if let Ok(id) = line.text.trim().parse::<i64>() {
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

/// Apply recovery's sidecar reclamations without materializing the retained
/// content: `KeepFrom` streams the old file's suffix into the rewrite temp.
async fn reclaim_recovered_sidecars(
    directory: &Path,
    fsync: bool,
    reclamations: &BTreeMap<String, SidecarReclamation>,
) {
    if reclamations.is_empty() {
        return;
    }
    for (file, reclamation) in reclamations {
        match reclamation {
            SidecarReclamation::Delete => replace_sidecar_file(directory, fsync, file, "").await,
            SidecarReclamation::KeepFrom(offset) => {
                replace_sidecar_suffix(directory, fsync, file, *offset).await
            }
        }
    }
}

/// Atomically rewrite a sidecar to the bytes after `offset`, copying them from
/// the existing file in fixed-size chunks (the retained suffix can be almost
/// the whole sidecar).
///
/// `offset` must be the byte offset of a retained line start, so it is always
/// strictly inside the file. If it is not, the file is left untouched: seeking
/// to (or past) EOF and copying zero bytes would otherwise silently replace
/// the sidecar with an empty file, discarding records that were never proven
/// reclaimable. The next recovery recomputes the plan from a fresh scan.
async fn replace_sidecar_suffix(directory: &Path, fsync: bool, file: &str, offset: u64) {
    let path = directory.join(file);
    let length = match tokio::fs::metadata(&path).await {
        Ok(metadata) => metadata.len(),
        Err(_) => return,
    };
    if offset >= length {
        return;
    }
    let temporary = directory.join(format!("{file}{RECLAIM_SUFFIX}"));
    let result = async {
        let mut source = tokio::fs::File::open(&path).await?;
        source.seek(std::io::SeekFrom::Start(offset)).await?;
        let mut target = tokio::fs::File::create(&temporary).await?;
        tokio::io::copy(&mut source, &mut target).await?;
        target.flush().await?;
        if fsync {
            target.sync_all().await?;
        }
        // Windows refuses (or behaves unpredictably) on renaming over a file
        // this process still has open; close both handles first.
        drop(source);
        drop(target);
        tokio::fs::rename(&temporary, &path).await?;
        Ok::<(), std::io::Error>(())
    }
    .await;
    match result {
        Ok(()) => {
            if fsync {
                sync_parent_dir(&path);
            }
        }
        Err(_) => {
            let _ = tokio::fs::remove_file(&temporary).await;
        }
    }
}

/// Atomically rewrite (`content` non-empty) or delete a sidecar.
///
/// Public so the durable storage tests can exercise the failure path with a
/// rename target that cannot be replaced.
pub async fn replace_sidecar_file(directory: &Path, fsync: bool, file: &str, content: &str) {
    let path = directory.join(file);
    if content.is_empty() {
        let _ = tokio::fs::remove_file(&path).await;
        return;
    }
    let temporary = directory.join(format!("{file}{RECLAIM_SUFFIX}"));
    let result = async {
        tokio::fs::write(&temporary, content.as_bytes()).await?;
        if fsync {
            let handle = tokio::fs::OpenOptions::new()
                .write(true)
                .open(&temporary)
                .await?;
            handle.sync_all().await?;
        }
        tokio::fs::rename(&temporary, &path).await?;
        Ok::<(), std::io::Error>(())
    }
    .await;
    match result {
        Ok(()) => {
            if fsync {
                sync_parent_dir(&path);
            }
        }
        Err(_) => {
            // A failed rewrite must not leave a full sidecar-sized copy
            // behind; recovery only reaps it on the next `open`.
            let _ = tokio::fs::remove_file(&temporary).await;
        }
    }
}

thread_local! {
    /// Test seam (R1-L3-007): calls into [`sync_parent_dir`] on this thread.
    ///
    /// `append_file` and `replace_sidecar_file` bump it whenever `fsync` is
    /// enabled. It is thread-local so independent integration tests cannot
    /// perturb each other, and it counts the CALL even on platforms where the
    /// directory sync itself is a no-op, so a dropped call site fails the
    /// regression test on every platform.
    static PARENT_DIR_SYNC_CALLS: Cell<usize> = const { Cell::new(0) };
}

thread_local! {
    /// Test seam (R1-L3-008): deep clones of [`SidecarRecord`] on this thread.
    ///
    /// Recovery must keep one copy of every sidecar record: the parsed record
    /// is wrapped in an `Arc` once, indexed by key, and moved into the replayed
    /// write. A regression that re-introduces `record.clone()` into the map or
    /// the parse list makes this counter jump by one full record per line.
    static SIDECAR_RECORD_CLONES: Cell<usize> = const { Cell::new(0) };
}

/// Number of sidecar-record deep clones on the current thread (test seam,
/// R1-L3-008).
#[doc(hidden)]
pub fn sidecar_record_clones() -> usize {
    SIDECAR_RECORD_CLONES.with(Cell::get)
}

/// Reset the current thread's sidecar-record clone counter (test seam,
/// R1-L3-008).
#[doc(hidden)]
pub fn reset_sidecar_record_clones() {
    SIDECAR_RECORD_CLONES.with(|clones| clones.set(0));
}

/// Number of parent-directory sync calls on the current thread
/// (test seam, R1-L3-007).
#[doc(hidden)]
pub fn parent_dir_sync_calls() -> usize {
    PARENT_DIR_SYNC_CALLS.with(Cell::get)
}

/// Reset the current thread's parent-directory sync counter
/// (test seam, R1-L3-007).
#[doc(hidden)]
pub fn reset_parent_dir_sync_calls() {
    PARENT_DIR_SYNC_CALLS.with(|calls| calls.set(0));
}

/// Flush the directory entry that names `path`.
///
/// A file's own `sync_all` does not make its directory entry durable: a newly
/// created sidecar or a completed rename can still be lost on a crash. Only
/// called when `fsync` is enabled; best-effort because the marker (or the next
/// recovery) already owns the state.
fn sync_parent_dir(path: &Path) {
    PARENT_DIR_SYNC_CALLS.with(|calls| calls.set(calls.get().saturating_add(1)));
    sync_parent_dir_impl(path);
}

#[cfg(unix)]
fn sync_parent_dir_impl(path: &Path) {
    let Some(parent) = path.parent() else {
        return;
    };
    if let Ok(directory) = std::fs::File::open(parent) {
        let _ = directory.sync_all();
    }
}

/// Windows journals metadata through `MoveFileExW`; there is no directory
/// handle to sync.
#[cfg(not(unix))]
fn sync_parent_dir_impl(_path: &Path) {}

impl CommitShared {
    async fn commit(self, writes: Vec<StorageWrite>) -> Result<Seq, StorageError> {
        let _guard = self.commit_lock.lock().await;
        ensure_usable(&self.closed, &self.poison).await?;
        let prepared = self.memory.prepare_commit(writes).await?;
        let encoded = encode_commit(prepared.seq, &prepared.writes)?;
        // Compaction is decided before the append: once the file would cross
        // the threshold, the newest record replaces every older snapshot after
        // the marker has made it durable.
        let mut compact_tasks: HashSet<String> = HashSet::new();
        for (file, content) in &encoded.sidecars {
            if !file.starts_with("task-") {
                continue;
            }
            let existing = match tokio::fs::metadata(self.directory.join(file)).await {
                Ok(metadata) => metadata.len(),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => 0,
                Err(_) => 0,
            };
            if content.lines().count() > 1
                || (existing > 0
                    && existing.saturating_add(content.len() as u64) > TASK_SIDECAR_COMPACT_BYTES)
            {
                compact_tasks.insert(file.clone());
            }
        }
        let reclamations = {
            let current_only = self.current_only_documents.lock().await;
            let live_tasks = self.live_task_sidecars.lock().await;
            plan_reclamations(
                &prepared.writes,
                &encoded,
                &current_only,
                &live_tasks,
                &compact_tasks,
            )
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

        // Adopt the sidecar bookkeeping from a BORROW of the prepared batch:
        // `apply_prepared` consumes the writes, and cloning the whole
        // `PreparedCommit` just to keep this slice alive copied every entry
        // and document payload a second time per durable commit. A failed
        // apply poisons the storage, so the bookkeeping it would leave
        // behind can never be observed through a later successful commit.
        adopt_sidecar_state(
            &self.current_only_documents,
            &self.live_task_sidecars,
            &prepared.writes,
        )
        .await;
        let seq = match self.memory.apply_prepared(prepared).await {
            Ok(seq) => seq,
            Err(error) => return Err(poison_error(&self.poison, error).await),
        };
        reclaim_sidecars_files(&self.directory, self.fsync, &reclamations).await;
        Ok(seq)
    }
}

fn confirm_record(
    marker_seq: Seq,
    ordinal: i64,
    file: &str,
    record_by_key: &mut HashMap<String, Arc<SidecarRecord>>,
    confirmed: &mut HashSet<String>,
    optional: bool,
) -> Result<Option<Arc<SidecarRecord>>, StorageError> {
    let key = sidecar_key(file, marker_seq, ordinal);
    if confirmed.contains(&key) {
        return Err(StorageError::Corruption(JsonlCorruptionError::new(
            "Sidecar record is confirmed more than once",
        )));
    }
    let Some(record) = record_by_key.remove(&key) else {
        if optional {
            return Ok(None);
        }
        return Err(StorageError::Corruption(JsonlCorruptionError::new(
            format!("Missing confirmed sidecar record {file} at sequence {marker_seq}"),
        )));
    };
    confirmed.insert(key);
    Ok(Some(record))
}

/// Take the record out of its `Arc`.
///
/// A key is confirmed at most once, so `record_by_key` normally holds the only
/// reference and this is a move; the clone fallback only keeps the function
/// total if a future caller keeps another handle.
fn into_owned_record(record: Arc<SidecarRecord>) -> SidecarRecord {
    Arc::try_unwrap(record).unwrap_or_else(|shared| (*shared).clone())
}

fn encode_commit(seq: Seq, writes: &[StorageWrite]) -> Result<EncodedCommit, StorageError> {
    let mut main = Vec::new();
    let mut sidecars: BTreeMap<String, String> = BTreeMap::new();
    let mut next_ordinal: i64 = 0;
    for write in writes {
        match write {
            StorageWrite::Conversation { value } => main.push(MainOperation::Conversation {
                value: Box::new(value.clone()),
            }),
            StorageWrite::Entry { value } => main.push(MainOperation::Entry {
                value: Box::new(value.clone()),
            }),
            StorageWrite::Submission { value } => main.push(MainOperation::Submission {
                value: Box::new(value.clone()),
            }),
            StorageWrite::DocumentRetire { id } => {
                main.push(MainOperation::DocumentRetire { id: *id })
            }
            StorageWrite::Task { value } => {
                if value.state.status() == TaskStatus::Terminal {
                    main.push(MainOperation::Task {
                        value: Box::new(value.as_ref().clone()),
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
                            value: Box::new(value.as_ref().clone()),
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
                    record: Box::new(record.clone()),
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
    compact_tasks: &HashSet<String>,
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
                final_tasks.insert(value.id, value.as_ref());
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
        let file = sidecar_file_name("task", *id);
        if task.state.status() == TaskStatus::Terminal {
            if live_tasks.contains(id) || encoded.sidecars.contains_key(&file) {
                replacements.insert(file, String::new());
            }
            continue;
        }
        if compact_tasks.contains(&file) {
            // Every record in this commit is a complete replacement; keep the
            // last one (highest ordinal) and drop every superseded snapshot.
            if let Some(latest) = encoded
                .sidecars
                .get(&file)
                .and_then(|content| content.lines().last())
            {
                replacements.insert(file, format!("{latest}\n"));
            }
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
        // Creating a new sidecar or main log must also persist its directory
        // entry, otherwise a crash can drop the file the marker references.
        sync_parent_dir(path);
    }
    Ok(())
}

async fn truncate_file(path: &Path, length: u64) -> Result<(), StorageError> {
    let file = tokio::fs::OpenOptions::new()
        .write(true)
        .open(path)
        .await
        .map_err(|error| StorageError::io("truncation", error))?;
    file.set_len(length)
        .await
        .map_err(|error| StorageError::io("truncation", error))
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

#[cfg(test)]
mod tests {
    use super::{replace_sidecar_suffix, RECLAIM_SUFFIX};
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_directory(tag: &str) -> PathBuf {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos())
            .unwrap_or(0);
        std::env::temp_dir().join(format!("pir-jsonl-{tag}-{}-{stamp}", std::process::id()))
    }

    /// `KeepFrom` offsets always point at a retained line start, so anything
    /// at or past EOF is a corrupted/raced state. Rewriting then would truncate
    /// the sidecar to an empty file; the guard must leave it untouched instead.
    #[tokio::test]
    async fn replace_sidecar_suffix_preserves_the_file_when_offset_reaches_eof() {
        let dir = temp_directory("suffix-eof");
        tokio::fs::create_dir_all(&dir)
            .await
            .expect("create fixture directory");
        let file = "task-20.jsonl";
        let original = b"{\"format\":1,\"type\":\"record\",\"seq\":1}\n\
                         {\"format\":1,\"type\":\"record\",\"seq\":2}\n";
        tokio::fs::write(dir.join(file), original)
            .await
            .expect("write sidecar");

        for offset in [original.len() as u64, original.len() as u64 + 4096] {
            replace_sidecar_suffix(&dir, false, file, offset).await;
            assert_eq!(
                tokio::fs::read(dir.join(file)).await.expect("read sidecar"),
                original,
                "an out-of-range offset ({offset}) must not truncate the sidecar"
            );
        }
        assert!(
            !dir.join(format!("{file}{RECLAIM_SUFFIX}")).exists(),
            "the guard must not leave a rewrite temporary behind"
        );
        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    /// The normal path still copies exactly the bytes from a valid line-start
    /// offset to EOF.
    #[tokio::test]
    async fn replace_sidecar_suffix_keeps_the_retained_tail() {
        let dir = temp_directory("suffix-tail");
        tokio::fs::create_dir_all(&dir)
            .await
            .expect("create fixture directory");
        let file = "task-20.jsonl";
        let first = b"{\"format\":1,\"type\":\"record\",\"seq\":1}\n";
        let second = b"{\"format\":1,\"type\":\"record\",\"seq\":2}\n";
        let original = [first.as_slice(), second.as_slice()].concat();
        tokio::fs::write(dir.join(file), &original)
            .await
            .expect("write sidecar");

        replace_sidecar_suffix(&dir, false, file, first.len() as u64).await;
        assert_eq!(
            tokio::fs::read(dir.join(file)).await.expect("read sidecar"),
            second,
            "the retained suffix must be copied verbatim"
        );
        let _ = tokio::fs::remove_dir_all(&dir).await;
    }
}
