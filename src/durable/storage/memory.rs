//! Detached in-memory reference implementation of [`Storage`].
//!
//! Mirrors upstream `src/storage/memory.ts`, including its validation rules:
//! global ID ownership, document incarnation/address invariants, version
//! transitions and retirement. Reads and retained writes are cloned to match
//! the ownership boundary of serialization-backed stores — that is backend
//! conformance, not defensive copying.
//!
//! Concurrency: every operation takes one `tokio::sync::Mutex` over the whole
//! state, so commits are serialized and the sequence is strictly increasing.
//! [`MemoryStorage::prepare_commit`] and [`MemoryStorage::apply_prepared`] are
//! the two halves the JSONL backend uses to validate a batch before it reaches
//! the log and to apply it only after the main marker is durable. A caller
//! using them directly must serialize the prepare/apply pair itself (the JSONL
//! backend holds its own commit mutex for exactly that reason).

use crate::durable::delta::apply_immutable_batches;
use crate::durable::errors::{StorageError, StorageRequestError};
use crate::durable::ids::{ConversationId, DocumentId, EntryId, Seq, SubmissionId, TaskId};
use crate::durable::storage::scan::{page_of, scan_start, ScanStart};
use crate::durable::types::{
    ConversationQuery, ConversationRecord, Cursor, DocumentAddress, DocumentContent,
    DocumentCreate, DocumentPoint, DocumentQuery, DocumentRecord, DocumentScope, EntryQuery,
    EntryRecord, EntryWithSeq, JsonObject, Page, ScanOrder, StorageFuture, StorageWrite,
    StoredDocument, SubmissionQuery, SubmissionRecord, SubmissionStatus, TaskQuery, TaskRecord,
    TaskStatus,
};
use crate::durable::Storage;
use std::collections::{BTreeMap, HashMap};
use tokio::sync::Mutex;

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
}

#[derive(Clone, Debug)]
struct Revision {
    content: DocumentContent,
    seq: Seq,
}

#[derive(Clone, Debug)]
struct StoredDocumentState {
    record: DocumentRecord,
    revisions: Vec<Revision>,
}

#[derive(Clone, Debug, Default)]
struct DocumentAddressIndex {
    ids: Vec<DocumentId>,
    current_id: Option<DocumentId>,
}

#[derive(Clone, Debug, Default)]
struct DocumentAction {
    create: Option<DocumentCreate>,
    content: Option<DocumentContent>,
    retire: bool,
}

#[derive(Debug, Default)]
struct State {
    record_types: HashMap<i64, TableName>,
    conversations: HashMap<ConversationId, ConversationRecord>,
    conversation_ids: Vec<ConversationId>,
    conversation_ids_by_owner_conversation: HashMap<ConversationId, Vec<ConversationId>>,
    conversation_ids_by_owner_task: HashMap<TaskId, Vec<ConversationId>>,
    entries: HashMap<EntryId, EntryRecord>,
    entry_ids: HashMap<ConversationId, Vec<EntryId>>,
    head_entry_ids: HashMap<ConversationId, Vec<EntryId>>,
    entry_commit_seqs: HashMap<EntryId, Seq>,
    tasks: HashMap<TaskId, TaskRecord>,
    task_ids: Vec<TaskId>,
    task_ids_by_status: HashMap<TaskStatus, Vec<TaskId>>,
    submissions: HashMap<SubmissionId, SubmissionRecord>,
    submission_ids: Vec<SubmissionId>,
    submission_ids_by_status: HashMap<SubmissionStatus, Vec<SubmissionId>>,
    submission_ids_by_request: HashMap<(ConversationId, String), SubmissionId>,
    documents: HashMap<DocumentId, StoredDocumentState>,
    document_addresses: HashMap<String, DocumentAddressIndex>,
    document_ids_by_scope: HashMap<String, Vec<DocumentId>>,
}

#[derive(Debug)]
struct Inner {
    state: State,
    next_id: i64,
    next_seq: i64,
    closed: bool,
}

impl Default for Inner {
    fn default() -> Self {
        Self {
            state: State::default(),
            // The root conversation ID (1) is reserved.
            next_id: 2,
            next_seq: 1,
            closed: false,
        }
    }
}

/// A validated, detached commit that has not changed observable state yet.
#[derive(Clone, Debug)]
pub struct PreparedCommit {
    /// The sequence the commit will carry.
    pub seq: Seq,
    /// Deeply detached writes, with document copies resolved to creates.
    pub writes: Vec<StorageWrite>,
}

/// Detached in-memory reference implementation of [`Storage`].
#[derive(Debug, Default)]
pub struct MemoryStorage {
    inner: Mutex<Inner>,
    /// Serializes `prepare_commit` + `apply_prepared` pairs issued through
    /// [`Storage::commit`], so two concurrent commits cannot prepare the same
    /// sequence. Callers that drive the two halves themselves own this.
    commit_lock: Mutex<()>,
}

impl MemoryStorage {
    /// Create an empty storage. The root conversation ID is reserved, so the
    /// first minted ID is 2.
    pub fn new() -> Self {
        Self::default()
    }

    /// Raise the mint watermark so the next [`Storage::mint_id`] returns an
    /// ID greater than `id`.
    ///
    /// The JSONL backend calls this on reopen to restore IDs that were minted
    /// but never stored in a commit (they are not discoverable from the
    /// records, so the backend journals them separately).
    pub(crate) async fn seed_minted_id(&self, id: i64) {
        let mut inner = self.inner.lock().await;
        inner.next_id = inner.next_id.max(id.saturating_add(1));
    }

    /// Validate and detach one commit without changing observable state.
    ///
    /// The caller must serialize the matching [`Self::apply_prepared`] against
    /// every other commit.
    pub async fn prepare_commit(
        &self,
        writes: Vec<StorageWrite>,
    ) -> Result<PreparedCommit, StorageError> {
        let seq = Seq::from_raw(self.inner.lock().await.next_seq);
        self.prepare_commit_at(writes, seq).await
    }

    /// Validate and detach one commit that must carry `seq`.
    ///
    /// Recovery uses this to replay markers with the sequence they were
    /// written with (gaps are permitted). The caller must serialize the
    /// matching [`Self::apply_prepared`] against every other commit.
    pub async fn prepare_commit_at(
        &self,
        writes: Vec<StorageWrite>,
        seq: Seq,
    ) -> Result<PreparedCommit, StorageError> {
        let inner = self.inner.lock().await;
        assert_open(&inner)?;
        if seq.as_i64() < inner.next_seq {
            return Err(StorageError::invalid(format!(
                "Commit sequence {} does not strictly increase",
                seq
            )));
        }
        let writes = resolve_document_copies(&inner.state, writes)?;
        check_global_ids(&inner.state, &writes)?;
        let actions = prepare_document_actions(&writes)?;
        check_document_actions(&inner.state, &actions)?;
        Ok(PreparedCommit { seq, writes })
    }

    /// Apply a commit produced by [`Self::prepare_commit`].
    pub async fn apply_prepared(&self, prepared: PreparedCommit) -> Result<Seq, StorageError> {
        let mut inner = self.inner.lock().await;
        assert_open(&inner)?;
        if prepared.seq.as_i64() < inner.next_seq {
            return Err(StorageError::invalid(format!(
                "Commit sequence {} does not strictly increase",
                prepared.seq
            )));
        }
        let following_seq = prepared
            .seq
            .as_i64()
            .checked_add(1)
            .ok_or_else(|| StorageError::Io("Memory sequence space is exhausted".to_owned()))?;
        let actions = prepare_document_actions(&prepared.writes)?;
        apply_commit(&mut inner.state, &prepared.writes, &actions, prepared.seq);
        inner.next_seq = following_seq;
        Ok(prepared.seq)
    }

    fn materialize_document(
        state: &State,
        id: DocumentId,
        at: DocumentPoint,
    ) -> Result<Option<StoredDocument>, StorageError> {
        let Some(stored) = state.documents.get(&id) else {
            return Ok(None);
        };
        if at != DocumentPoint::Current && stored.record.is_current_only() {
            return Err(StorageError::Request(StorageRequestError::new(format!(
                "Document {id} does not retain historical content"
            ))));
        }
        if !is_alive_at(&stored.record, at) {
            return Ok(None);
        }
        let revisions: Vec<&Revision> = match at {
            DocumentPoint::Current => stored.revisions.iter().collect(),
            DocumentPoint::Seq(seq) => stored
                .revisions
                .iter()
                .filter(|revision| revision.seq <= seq)
                .collect(),
        };
        let base_index = revisions
            .iter()
            .rposition(|revision| matches!(revision.content, DocumentContent::Base { .. }))
            .ok_or_else(|| {
                StorageError::invalid(format!("Document {id} is missing a required base"))
            })?;
        let (version, value) = match &revisions[base_index].content {
            DocumentContent::Base { version, value } => (*version, value.clone()),
            DocumentContent::Delta { .. } => {
                return Err(StorageError::invalid(format!(
                    "Document {id} is missing a required base"
                )));
            }
        };
        let mut batches = Vec::new();
        for revision in &revisions[base_index + 1..] {
            match &revision.content {
                DocumentContent::Delta {
                    version: delta_version,
                    ops,
                } if *delta_version == version => batches.push(ops.clone()),
                _ => {
                    return Err(StorageError::invalid(format!(
                        "Document {id} crosses a stored version boundary without a base"
                    )));
                }
            }
        }
        let materialized = apply_immutable_batches(&serde_json::Value::Object(value), batches)
            .map_err(|error| {
                StorageError::invalid(format!("Document {id} cannot be materialized: {error}"))
            })?;
        let serde_json::Value::Object(value) = materialized else {
            return Err(StorageError::invalid(format!(
                "Document {id} materialized to a non-object value"
            )));
        };
        Ok(Some(StoredDocument {
            record: stored.record.clone(),
            version,
            value,
            deltas_since_base: revisions.len() - base_index - 1,
        }))
    }

    fn conversation_ids_for_query<'a>(
        state: &'a State,
        query: &ConversationQuery,
    ) -> &'a [ConversationId] {
        if let Some(task_id) = query.owner_task_id {
            state
                .conversation_ids_by_owner_task
                .get(&task_id)
                .map(Vec::as_slice)
                .unwrap_or(&[])
        } else if let Some(conversation_id) = query.owner_conversation_id {
            state
                .conversation_ids_by_owner_conversation
                .get(&conversation_id)
                .map(Vec::as_slice)
                .unwrap_or(&[])
        } else {
            &state.conversation_ids
        }
    }

    /// Visible entries newest first: the fork chain's segments from the
    /// requested conversation back to the root.
    fn visible_entries(
        state: &State,
        conversation_id: ConversationId,
        min_entry_id: Option<EntryId>,
        max_entry_id: Option<EntryId>,
    ) -> Result<Vec<EntryRecord>, StorageError> {
        require_conversation(state, conversation_id)?;
        let min = min_entry_id.map_or(i64::MIN, |id| id.as_i64());
        let max = max_entry_id.map_or(i64::MAX, |id| id.as_i64());
        let mut out = Vec::new();
        let mut current = conversation_id;
        let mut upper = max;
        loop {
            let ids = state
                .entry_ids
                .get(&current)
                .map(Vec::as_slice)
                .unwrap_or(&[]);
            let raw: Vec<i64> = ids.iter().map(|id| id.as_i64()).collect();
            let end = upper_bound(&raw, upper);
            for index in (0..end).rev() {
                let id = ids[index];
                if id.as_i64() < min {
                    break;
                }
                if let Some(entry) = state.entries.get(&id) {
                    out.push(entry.clone());
                }
            }
            let Some(parent) = state
                .conversations
                .get(&current)
                .and_then(|conversation| conversation.parent.as_ref())
            else {
                break;
            };
            upper = upper.min(parent.at.as_i64());
            if upper < min {
                break;
            }
            current = parent.conversation_id;
        }
        Ok(out)
    }

    /// Visible entries oldest first: the fork chain's segments from the root
    /// conversation forward.
    fn visible_entries_ascending(
        state: &State,
        conversation_id: ConversationId,
        min_entry_id: Option<EntryId>,
        max_entry_id: Option<EntryId>,
    ) -> Result<Vec<EntryRecord>, StorageError> {
        require_conversation(state, conversation_id)?;
        let min = min_entry_id.map_or(i64::MIN, |id| id.as_i64());
        let max = max_entry_id.map_or(i64::MAX, |id| id.as_i64());
        let mut segments: Vec<(ConversationId, i64)> = Vec::new();
        let mut current = conversation_id;
        let mut upper = max;
        loop {
            segments.push((current, upper));
            let Some(parent) = state
                .conversations
                .get(&current)
                .and_then(|conversation| conversation.parent.as_ref())
            else {
                break;
            };
            upper = upper.min(parent.at.as_i64());
            if upper < min {
                break;
            }
            current = parent.conversation_id;
        }
        let mut out = Vec::new();
        for (current, upper) in segments.into_iter().rev() {
            let ids = state
                .entry_ids
                .get(&current)
                .map(Vec::as_slice)
                .unwrap_or(&[]);
            let raw: Vec<i64> = ids.iter().map(|id| id.as_i64()).collect();
            let start = lower_bound(&raw, min);
            for id in ids.iter().skip(start) {
                if id.as_i64() > upper {
                    break;
                }
                if let Some(entry) = state.entries.get(id) {
                    out.push(entry.clone());
                }
            }
        }
        Ok(out)
    }
}

fn assert_open(inner: &Inner) -> Result<(), StorageError> {
    if inner.closed {
        return Err(StorageError::closed("MemoryStorage"));
    }
    Ok(())
}

fn require_conversation(
    state: &State,
    conversation_id: ConversationId,
) -> Result<(), StorageError> {
    if state.conversations.contains_key(&conversation_id) {
        Ok(())
    } else {
        Err(StorageError::Request(StorageRequestError::new(format!(
            "Unknown conversation: {conversation_id}"
        ))))
    }
}

fn is_alive_at(record: &DocumentRecord, at: DocumentPoint) -> bool {
    match at {
        DocumentPoint::Current => record.retired_at.is_none(),
        DocumentPoint::Seq(seq) => {
            record.created_at <= seq && record.retired_at.is_none_or(|retired| seq < retired)
        }
    }
}

fn scope_key(scope: &DocumentScope) -> String {
    match scope {
        DocumentScope::Session => "[\"session\"]".to_owned(),
        DocumentScope::Conversation { conversation_id } => {
            format!("[\"conversation\",{}]", conversation_id.as_i64())
        }
        DocumentScope::Task { task_id } => format!("[\"task\",{}]", task_id.as_i64()),
    }
}

fn address_key(address: &DocumentAddress) -> String {
    // `{:?}` is a stable, injective rendering for these strings and cannot
    // fail, so the identity of an address never depends on serialization.
    let family = match &address.key {
        Some(key) => format!("[\"family\",{key:?}]"),
        None => "[\"singleton\"]".to_owned(),
    };
    format!(
        "[{:?}, {}, {}]",
        address.kind,
        scope_key(&address.scope),
        family
    )
}

fn record_address_key(record: &DocumentCreate) -> String {
    address_key(&DocumentAddress {
        kind: record.kind.clone(),
        scope: record.scope,
        key: record.key.clone(),
    })
}

fn record_address_key_of(record: &DocumentRecord) -> String {
    address_key(&DocumentAddress {
        kind: record.kind.clone(),
        scope: record.scope,
        key: record.key.clone(),
    })
}

fn lower_bound<T: Copy + Ord>(values: &[T], target: T) -> usize {
    values.partition_point(|value| *value < target)
}

fn upper_bound<T: Copy + Ord>(values: &[T], target: T) -> usize {
    values.partition_point(|value| *value <= target)
}

fn insert_sorted<T: Copy + Ord>(values: &mut Vec<T>, value: T) {
    if values.last().is_none_or(|last| *last < value) {
        values.push(value);
    } else {
        let index = lower_bound(values, value);
        values.insert(index, value);
    }
}

fn remove_sorted<T: Copy + Ord>(values: &mut Vec<T>, value: T) {
    let index = lower_bound(values, value);
    if values.get(index) == Some(&value) {
        values.remove(index);
    }
}

fn insert_map_id<K: std::hash::Hash + Eq, I: Copy + Ord>(
    index: &mut HashMap<K, Vec<I>>,
    key: K,
    id: I,
) {
    insert_sorted(index.entry(key).or_default(), id);
}

/// Indexes of sorted IDs in scan order, after the cursor's ID when there is one.
fn scan_indexes(ids: &[i64], start: ScanStart) -> Vec<usize> {
    match start.order {
        ScanOrder::Ascending => {
            let first = start
                .after
                .map(|after| upper_bound(ids, after))
                .unwrap_or(0);
            (first..ids.len()).collect()
        }
        ScanOrder::Descending => {
            let end = start
                .after
                .map(|after| lower_bound(ids, after))
                .unwrap_or(ids.len());
            (0..end).rev().collect()
        }
    }
}

fn resolve_document_copies(
    state: &State,
    writes: Vec<StorageWrite>,
) -> Result<Vec<StorageWrite>, StorageError> {
    if !writes
        .iter()
        .any(|write| matches!(write, StorageWrite::DocumentCopy { .. }))
    {
        return Ok(writes);
    }
    let mut changed = Vec::new();
    for write in &writes {
        match write {
            StorageWrite::DocumentCreate { record, .. }
            | StorageWrite::DocumentCopy { record, .. } => {
                changed.push(record.id);
            }
            StorageWrite::DocumentChange { id, .. } | StorageWrite::DocumentRetire { id } => {
                changed.push(*id)
            }
            _ => {}
        }
    }
    let mut resolved = Vec::with_capacity(writes.len());
    for write in writes {
        let StorageWrite::DocumentCopy { record, source } = write else {
            resolved.push(write);
            continue;
        };
        if changed.contains(&source.id) {
            return Err(StorageError::invalid(format!(
                "Document copy {} was rejected: fork source document {} is changed in the copy batch",
                record.id, source.id
            )));
        }
        let stored =
            MemoryStorage::materialize_document(state, source.id, source.at).map_err(|error| {
                StorageError::invalid(format!("Document copy {} was rejected: {error}", record.id))
            })?;
        let stored = stored.ok_or_else(|| {
            StorageError::invalid(format!(
                "Document copy {} was rejected: fork source document {} cannot be read",
                record.id, source.id
            ))
        })?;
        let source_is_conversation =
            matches!(stored.record.scope, DocumentScope::Conversation { .. });
        let target_is_conversation = matches!(record.scope, DocumentScope::Conversation { .. });
        if !source_is_conversation
            || !target_is_conversation
            || stored.record.kind != record.kind
            || stored.record.key != record.key
            || stored.record.history != record.history
            || stored.record.fork != record.fork
        {
            return Err(StorageError::invalid(format!(
                "Document copy {} was rejected: fork source document {} does not match the copied record",
                record.id, source.id
            )));
        }
        resolved.push(StorageWrite::DocumentCreate {
            record,
            content: DocumentContent::Base {
                version: stored.version,
                value: stored.value,
            },
        });
    }
    Ok(resolved)
}

fn check_global_ids(state: &State, writes: &[StorageWrite]) -> Result<(), StorageError> {
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
        let existing = state.record_types.get(&id).copied();
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

fn prepare_document_actions(
    writes: &[StorageWrite],
) -> Result<BTreeMap<DocumentId, DocumentAction>, StorageError> {
    let mut actions: BTreeMap<DocumentId, DocumentAction> = BTreeMap::new();
    for write in writes {
        match write {
            StorageWrite::DocumentCreate { record, content } => {
                let action = actions.entry(record.id).or_default();
                if action.create.is_some() || action.content.is_some() {
                    return Err(StorageError::invalid(format!(
                        "Document {} has more than one content command",
                        record.id
                    )));
                }
                action.create = Some(record.clone());
                action.content = Some(content.clone());
            }
            StorageWrite::DocumentChange { id, content } => {
                let action = actions.entry(*id).or_default();
                if action.content.is_some() {
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

fn check_document_actions(
    state: &State,
    actions: &BTreeMap<DocumentId, DocumentAction>,
) -> Result<(), StorageError> {
    let mut live_counts: HashMap<String, i64> = HashMap::new();
    for (id, action) in actions {
        let existing = state.documents.get(id);
        if action.create.is_none() && existing.is_none() {
            return Err(StorageError::invalid(format!("Unknown document: {id}")));
        }
        if action.create.is_some() && existing.is_some() {
            return Err(StorageError::invalid(format!(
                "Document {id} already exists"
            )));
        }
        if existing
            .and_then(|stored| stored.record.retired_at)
            .is_some()
        {
            return Err(StorageError::invalid(format!("Document {id} is retired")));
        }
        if action.create.is_some()
            && action
                .content
                .as_ref()
                .is_some_and(|content| !matches!(content, DocumentContent::Base { .. }))
        {
            return Err(StorageError::invalid(format!(
                "Document {id} creation requires a base"
            )));
        }
        if let Some(DocumentContent::Delta { version, .. }) = &action.content {
            let Some(previous) = existing.and_then(|stored| stored.revisions.last()) else {
                return Err(StorageError::invalid(format!(
                    "Document {id} delta has no base"
                )));
            };
            if previous.content.version() != *version {
                return Err(StorageError::invalid(format!(
                    "Document {id} version transition requires a base"
                )));
            }
        }

        let key = match (&action.create, existing) {
            (Some(create), _) => record_address_key(create),
            (None, Some(stored)) => record_address_key_of(&stored.record),
            (None, None) => continue,
        };
        let current_id = state
            .document_addresses
            .get(&key)
            .and_then(|index| index.current_id);
        let mut live = live_counts
            .get(&key)
            .copied()
            .unwrap_or_else(|| i64::from(current_id.is_some()));
        if action.retire && current_id == Some(*id) {
            live -= 1;
        }
        if action.create.is_some() && !action.retire {
            live += 1;
        }
        live_counts.insert(key, live);
    }

    for live in live_counts.values() {
        if *live > 1 {
            return Err(StorageError::invalid(
                "Document address already has a current incarnation",
            ));
        }
    }
    Ok(())
}

fn apply_commit(
    state: &mut State,
    writes: &[StorageWrite],
    actions: &BTreeMap<DocumentId, DocumentAction>,
    seq: Seq,
) {
    for write in writes {
        match write {
            StorageWrite::Conversation { value } => {
                state
                    .record_types
                    .insert(value.id.as_i64(), TableName::Conversation);
                state.conversations.insert(value.id, value.clone());
                insert_sorted(&mut state.conversation_ids, value.id);
                if let Some(owner) = &value.owner {
                    insert_map_id(
                        &mut state.conversation_ids_by_owner_conversation,
                        owner.conversation_id,
                        value.id,
                    );
                    insert_map_id(
                        &mut state.conversation_ids_by_owner_task,
                        owner.task_id,
                        value.id,
                    );
                }
            }
            StorageWrite::Entry { value } => {
                state
                    .record_types
                    .insert(value.id.as_i64(), TableName::Entry);
                state.entries.insert(value.id, value.clone());
                state.entry_commit_seqs.insert(value.id, seq);
                insert_sorted(
                    state.entry_ids.entry(value.conversation_id).or_default(),
                    value.id,
                );
                if value.head.is_some() {
                    insert_sorted(
                        state
                            .head_entry_ids
                            .entry(value.conversation_id)
                            .or_default(),
                        value.id,
                    );
                }
            }
            StorageWrite::Task { value } => {
                state
                    .record_types
                    .insert(value.id.as_i64(), TableName::Task);
                let status = value.state.status();
                match state.tasks.get(&value.id) {
                    None => {
                        insert_sorted(&mut state.task_ids, value.id);
                        insert_sorted(
                            state.task_ids_by_status.entry(status).or_default(),
                            value.id,
                        );
                    }
                    Some(previous) => {
                        let previous_status = previous.state.status();
                        if previous_status != status {
                            if let Some(ids) = state.task_ids_by_status.get_mut(&previous_status) {
                                remove_sorted(ids, value.id);
                            }
                            insert_sorted(
                                state.task_ids_by_status.entry(status).or_default(),
                                value.id,
                            );
                        }
                    }
                }
                state.tasks.insert(value.id, value.clone());
            }
            StorageWrite::Submission { value } => {
                state
                    .record_types
                    .insert(value.id.as_i64(), TableName::Submission);
                let previous = state.submissions.get(&value.id).cloned();
                match &previous {
                    None => {
                        insert_sorted(&mut state.submission_ids, value.id);
                        insert_sorted(
                            state
                                .submission_ids_by_status
                                .entry(value.status)
                                .or_default(),
                            value.id,
                        );
                    }
                    Some(previous) => {
                        if previous.status != value.status {
                            if let Some(ids) =
                                state.submission_ids_by_status.get_mut(&previous.status)
                            {
                                remove_sorted(ids, value.id);
                            }
                            insert_sorted(
                                state
                                    .submission_ids_by_status
                                    .entry(value.status)
                                    .or_default(),
                                value.id,
                            );
                        }
                    }
                }
                if let Some(previous) = &previous {
                    if let Some(request_id) = &previous.request_id {
                        let key = (previous.conversation_id, request_id.clone());
                        if state.submission_ids_by_request.get(&key) == Some(&value.id) {
                            state.submission_ids_by_request.remove(&key);
                        }
                    }
                }
                state.submissions.insert(value.id, value.clone());
                if let Some(request_id) = &value.request_id {
                    state
                        .submission_ids_by_request
                        .insert((value.conversation_id, request_id.clone()), value.id);
                }
            }
            StorageWrite::DocumentCreate { .. }
            | StorageWrite::DocumentCopy { .. }
            | StorageWrite::DocumentChange { .. }
            | StorageWrite::DocumentRetire { .. } => {}
        }
    }

    apply_document_actions(state, actions, seq);
}

fn apply_document_actions(
    state: &mut State,
    actions: &BTreeMap<DocumentId, DocumentAction>,
    seq: Seq,
) {
    for (id, action) in actions {
        if let Some(create) = &action.create {
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
            let content = action.content.clone().unwrap_or(DocumentContent::Base {
                version: 1,
                value: JsonObject::new(),
            });
            state.record_types.insert(id.as_i64(), TableName::Document);
            state.documents.insert(
                *id,
                StoredDocumentState {
                    record: record.clone(),
                    revisions: vec![Revision { content, seq }],
                },
            );
            let address = state
                .document_addresses
                .entry(record_address_key_of(&record))
                .or_default();
            insert_sorted(&mut address.ids, *id);
            insert_sorted(
                state
                    .document_ids_by_scope
                    .entry(scope_key(&record.scope))
                    .or_default(),
                *id,
            );
        } else if let Some(content) = &action.content {
            if let Some(stored) = state.documents.get_mut(id) {
                if matches!(content, DocumentContent::Base { .. })
                    && stored.record.is_current_only()
                {
                    stored.revisions = vec![Revision {
                        content: content.clone(),
                        seq,
                    }];
                } else {
                    stored.revisions.push(Revision {
                        content: content.clone(),
                        seq,
                    });
                }
            }
        }

        if action.retire && action.create.is_none() {
            if let Some(stored) = state.documents.get_mut(id) {
                stored.record.retired_at = Some(seq);
                if stored.record.is_current_only() {
                    stored.revisions.clear();
                }
            }
        }

        if action.create.is_some() || action.retire {
            let Some(record) = state.documents.get(id).map(|stored| stored.record.clone()) else {
                continue;
            };
            let key = record_address_key_of(&record);
            let retired = record.retired_at.is_some();
            if let Some(address) = state.document_addresses.get_mut(&key) {
                if action.retire && address.current_id == Some(*id) {
                    address.current_id = None;
                }
                if action.create.is_some() && !retired {
                    address.current_id = Some(*id);
                }
            }
        }
    }
}

impl Storage for MemoryStorage {
    fn commit(&self, writes: Vec<StorageWrite>) -> StorageFuture<'_, Result<Seq, StorageError>> {
        Box::pin(async move {
            let _guard = self.commit_lock.lock().await;
            let prepared = self.prepare_commit(writes).await?;
            self.apply_prepared(prepared).await
        })
    }

    fn mint_id(&self) -> StorageFuture<'_, Result<i64, StorageError>> {
        // Raw number: brands are compile-time only, so the caller brands it at
        // the trusted boundary with `decode_id`.
        Box::pin(async move {
            let mut inner = self.inner.lock().await;
            assert_open(&inner)?;
            let watermark = match inner.state.record_types.keys().copied().max() {
                Some(max) => max
                    .checked_add(1)
                    .ok_or_else(|| StorageError::Io("Memory ID space is exhausted".to_owned()))?,
                None => 2,
            };
            inner.next_id = inner.next_id.max(watermark);
            let id = inner.next_id;
            inner.next_id = inner
                .next_id
                .checked_add(1)
                .ok_or_else(|| StorageError::Io("Memory ID space is exhausted".to_owned()))?;
            Ok(id)
        })
    }

    fn conversation(
        &self,
        id: ConversationId,
    ) -> StorageFuture<'_, Result<Option<ConversationRecord>, StorageError>> {
        Box::pin(async move {
            let inner = self.inner.lock().await;
            assert_open(&inner)?;
            Ok(inner.state.conversations.get(&id).cloned())
        })
    }

    fn scan_conversations(
        &self,
        query: ConversationQuery,
        limit: usize,
        cursor: Option<Cursor>,
    ) -> StorageFuture<'_, Result<Page<ConversationRecord>, StorageError>> {
        Box::pin(async move {
            let inner = self.inner.lock().await;
            assert_open(&inner)?;
            let state = &inner.state;
            let ids = Self::conversation_ids_for_query(state, &query);
            let start = scan_start(query.order, cursor.as_ref(), ScanOrder::Ascending)?;
            let raw: Vec<i64> = ids.iter().map(|id| id.as_i64()).collect();
            let mut values = Vec::new();
            for index in scan_indexes(&raw, start) {
                if values.len() > limit {
                    break;
                }
                let id = ids[index];
                let Some(value) = state.conversations.get(&id) else {
                    continue;
                };
                if let Some(owner) = query.owner_conversation_id {
                    if value.owner.as_ref().map(|owner| owner.conversation_id) != Some(owner) {
                        continue;
                    }
                }
                values.push(value.clone());
            }
            let (items, next) = page_of(&values, limit, start.order, |value| value.id.as_i64());
            Ok(Page { items, next })
        })
    }

    fn entry(&self, id: EntryId) -> StorageFuture<'_, Result<Option<EntryWithSeq>, StorageError>> {
        Box::pin(async move {
            let inner = self.inner.lock().await;
            assert_open(&inner)?;
            let state = &inner.state;
            let Some(entry) = state.entries.get(&id) else {
                return Ok(None);
            };
            Ok(Some(EntryWithSeq {
                entry: entry.clone(),
                commit_seq: state
                    .entry_commit_seqs
                    .get(&id)
                    .copied()
                    .unwrap_or_default(),
            }))
        })
    }

    fn entry_in(
        &self,
        conversation_id: ConversationId,
        id: EntryId,
    ) -> StorageFuture<'_, Result<Option<EntryWithSeq>, StorageError>> {
        Box::pin(async move {
            let inner = self.inner.lock().await;
            assert_open(&inner)?;
            let state = &inner.state;
            let visible = Self::visible_entries(state, conversation_id, Some(id), Some(id))?;
            let Some(entry) = visible.into_iter().next() else {
                return Ok(None);
            };
            Ok(Some(EntryWithSeq {
                entry,
                commit_seq: state
                    .entry_commit_seqs
                    .get(&id)
                    .copied()
                    .unwrap_or_default(),
            }))
        })
    }

    fn find_latest_head_marker(
        &self,
        conversation_id: ConversationId,
        at_or_before_entry_id: Option<EntryId>,
    ) -> StorageFuture<'_, Result<Option<EntryRecord>, StorageError>> {
        Box::pin(async move {
            let inner = self.inner.lock().await;
            assert_open(&inner)?;
            let state = &inner.state;
            require_conversation(state, conversation_id)?;
            let mut current = conversation_id;
            let mut upper = at_or_before_entry_id.map_or(i64::MAX, |id| id.as_i64());
            loop {
                let ids = state
                    .head_entry_ids
                    .get(&current)
                    .map(Vec::as_slice)
                    .unwrap_or(&[]);
                let raw: Vec<i64> = ids.iter().map(|id| id.as_i64()).collect();
                let index = upper_bound(&raw, upper);
                if index > 0 {
                    let id = ids[index - 1];
                    if let Some(entry) = state.entries.get(&id) {
                        return Ok(Some(entry.clone()));
                    }
                }
                let Some(parent) = state
                    .conversations
                    .get(&current)
                    .and_then(|conversation| conversation.parent.as_ref())
                else {
                    return Ok(None);
                };
                upper = upper.min(parent.at.as_i64());
                current = parent.conversation_id;
            }
        })
    }

    fn scan_entries(
        &self,
        query: EntryQuery,
        limit: usize,
        cursor: Option<Cursor>,
    ) -> StorageFuture<'_, Result<Page<EntryRecord>, StorageError>> {
        Box::pin(async move {
            let inner = self.inner.lock().await;
            assert_open(&inner)?;
            let state = &inner.state;
            let start = scan_start(query.order, cursor.as_ref(), ScanOrder::Descending)?;
            let mut min_entry_id = query.min_entry_id;
            let mut max_entry_id = query.max_entry_id;
            if let Some(after) = start.after {
                match start.order {
                    ScanOrder::Descending => {
                        let bound = EntryId::from_raw(after.saturating_sub(1));
                        max_entry_id = Some(max_entry_id.map_or(bound, |max| max.min(bound)));
                    }
                    ScanOrder::Ascending => {
                        let bound = EntryId::from_raw(after.saturating_add(1));
                        min_entry_id = Some(min_entry_id.map_or(bound, |min| min.max(bound)));
                    }
                }
            }
            let entries = match start.order {
                ScanOrder::Descending => {
                    Self::visible_entries(state, query.conversation_id, min_entry_id, max_entry_id)?
                }
                ScanOrder::Ascending => Self::visible_entries_ascending(
                    state,
                    query.conversation_id,
                    min_entry_id,
                    max_entry_id,
                )?,
            };
            let values: Vec<EntryRecord> = entries.into_iter().take(limit + 1).collect();
            let (items, next) = page_of(&values, limit, start.order, |entry| entry.id.as_i64());
            Ok(Page { items, next })
        })
    }

    fn task(&self, id: TaskId) -> StorageFuture<'_, Result<Option<TaskRecord>, StorageError>> {
        Box::pin(async move {
            let inner = self.inner.lock().await;
            assert_open(&inner)?;
            Ok(inner.state.tasks.get(&id).cloned())
        })
    }

    fn scan_tasks(
        &self,
        query: TaskQuery,
        limit: usize,
        cursor: Option<Cursor>,
    ) -> StorageFuture<'_, Result<Page<TaskRecord>, StorageError>> {
        Box::pin(async move {
            let inner = self.inner.lock().await;
            assert_open(&inner)?;
            let state = &inner.state;
            let start = scan_start(query.order, cursor.as_ref(), ScanOrder::Ascending)?;
            let ids = match query.status {
                Some(status) => state
                    .task_ids_by_status
                    .get(&status)
                    .map(Vec::as_slice)
                    .unwrap_or(&[]),
                None => &state.task_ids,
            };
            let raw: Vec<i64> = ids.iter().map(|id| id.as_i64()).collect();
            let mut values = Vec::new();
            for index in scan_indexes(&raw, start) {
                if values.len() > limit {
                    break;
                }
                let Some(value) = state.tasks.get(&ids[index]) else {
                    continue;
                };
                if let Some(conversation_id) = query.conversation_id {
                    if value.conversation_id != conversation_id {
                        continue;
                    }
                }
                if let Some(kind) = &query.kind {
                    if &value.kind != kind {
                        continue;
                    }
                }
                if let Some(abort_requested) = query.abort_requested {
                    if value.abort_requested != abort_requested {
                        continue;
                    }
                }
                if let Some(background) = query.background {
                    if value.background != background {
                        continue;
                    }
                }
                values.push(value.clone());
            }
            let (items, next) = page_of(&values, limit, start.order, |value| value.id.as_i64());
            Ok(Page { items, next })
        })
    }

    fn submission(
        &self,
        id: SubmissionId,
    ) -> StorageFuture<'_, Result<Option<SubmissionRecord>, StorageError>> {
        Box::pin(async move {
            let inner = self.inner.lock().await;
            assert_open(&inner)?;
            Ok(inner.state.submissions.get(&id).cloned())
        })
    }

    fn scan_submissions(
        &self,
        query: SubmissionQuery,
        limit: usize,
        cursor: Option<Cursor>,
    ) -> StorageFuture<'_, Result<Page<SubmissionRecord>, StorageError>> {
        Box::pin(async move {
            let inner = self.inner.lock().await;
            assert_open(&inner)?;
            let state = &inner.state;
            let start = scan_start(query.order, cursor.as_ref(), ScanOrder::Ascending)?;
            let ids = match query.status {
                Some(status) => state
                    .submission_ids_by_status
                    .get(&status)
                    .map(Vec::as_slice)
                    .unwrap_or(&[]),
                None => &state.submission_ids,
            };
            let raw: Vec<i64> = ids.iter().map(|id| id.as_i64()).collect();
            let mut values = Vec::new();
            for index in scan_indexes(&raw, start) {
                if values.len() > limit {
                    break;
                }
                let Some(value) = state.submissions.get(&ids[index]) else {
                    continue;
                };
                if let Some(conversation_id) = query.conversation_id {
                    if value.conversation_id != conversation_id {
                        continue;
                    }
                }
                values.push(value.clone());
            }
            let (items, next) = page_of(&values, limit, start.order, |value| value.id.as_i64());
            Ok(Page { items, next })
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
            let inner = self.inner.lock().await;
            assert_open(&inner)?;
            let state = &inner.state;
            let Some(id) = state
                .submission_ids_by_request
                .get(&(conversation_id, request_id))
            else {
                return Ok(None);
            };
            Ok(state.submissions.get(id).cloned())
        })
    }

    fn find_document(
        &self,
        address: DocumentAddress,
        at: DocumentPoint,
    ) -> StorageFuture<'_, Result<Option<DocumentRecord>, StorageError>> {
        Box::pin(async move {
            let inner = self.inner.lock().await;
            assert_open(&inner)?;
            let state = &inner.state;
            let Some(index) = state.document_addresses.get(&address_key(&address)) else {
                return Ok(None);
            };
            match at {
                DocumentPoint::Current => Ok(index
                    .current_id
                    .and_then(|id| state.documents.get(&id))
                    .map(|stored| stored.record.clone())),
                DocumentPoint::Seq(_) => {
                    for id in &index.ids {
                        if let Some(stored) = state.documents.get(id) {
                            if is_alive_at(&stored.record, at) {
                                return Ok(Some(stored.record.clone()));
                            }
                        }
                    }
                    Ok(None)
                }
            }
        })
    }

    fn document(
        &self,
        id: DocumentId,
        at: DocumentPoint,
    ) -> StorageFuture<'_, Result<Option<StoredDocument>, StorageError>> {
        Box::pin(async move {
            let inner = self.inner.lock().await;
            assert_open(&inner)?;
            Self::materialize_document(&inner.state, id, at)
        })
    }

    fn scan_documents(
        &self,
        query: DocumentQuery,
        limit: usize,
        cursor: Option<Cursor>,
    ) -> StorageFuture<'_, Result<Page<DocumentRecord>, StorageError>> {
        Box::pin(async move {
            let inner = self.inner.lock().await;
            assert_open(&inner)?;
            let state = &inner.state;
            let at = query.at.to_point();
            let ids = state
                .document_ids_by_scope
                .get(&scope_key(&query.scope))
                .map(Vec::as_slice)
                .unwrap_or(&[]);
            let start = match cursor.as_ref() {
                None => 0,
                Some(cursor) => {
                    if cursor.after < 0 {
                        return Err(StorageError::Request(StorageRequestError::new(
                            "Invalid storage cursor",
                        )));
                    }
                    upper_bound(
                        &ids.iter().map(|id| id.as_i64()).collect::<Vec<_>>(),
                        cursor.after,
                    )
                }
            };
            let mut values = Vec::new();
            for id in ids.iter().skip(start) {
                if values.len() > limit {
                    break;
                }
                let Some(stored) = state.documents.get(id) else {
                    continue;
                };
                if let Some(kind) = &query.kind {
                    if &stored.record.kind != kind {
                        continue;
                    }
                }
                if is_alive_at(&stored.record, at) {
                    values.push(stored.record.clone());
                }
            }
            let (items, next) = page_of(&values, limit, ScanOrder::Ascending, |value| {
                value.id.as_i64()
            });
            Ok(Page { items, next })
        })
    }

    fn close(&self) -> StorageFuture<'_, Result<(), StorageError>> {
        Box::pin(async move {
            // Wait for an in-flight commit to finish before declaring the storage
            // closed, so no commit can apply after `close` returned.
            let _guard = self.commit_lock.lock().await;
            self.inner.lock().await.closed = true;
            Ok(())
        })
    }
}
