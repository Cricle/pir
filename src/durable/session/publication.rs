//! Committed publication: what one successful commit makes observable.
//!
//! Port of upstream `session.ts`'s `#publish` plus the `CommitChange` /
//! `TableCommitChange` / `CommitPublication` types from `types.ts`.
//!
//! Publication happens **after** `Storage::commit` returned and after document
//! adoption, synchronously on the mutation line. The order is fixed by
//! upstream and by spec §1 rule 7:
//!
//! 1. build the immutable change list from the write batch and the adopted
//!    document changes;
//! 2. run **internal** observers (the scheduler, the submission queue, the
//!    conversation views) — a throw there fails the session, because their
//!    memory would otherwise lag behind storage;
//! 3. run **host** listeners (`subscribe_commits`) — a throw is reported and
//!    never fails the commit;
//! 4. enqueue the publication for document-state subscribers and watchers,
//!    which run later, off the line.
//!
//! A publication exists only when at least one observer is registered: with no
//! listeners a commit allocates nothing (upstream keeps the same fast path).

use std::sync::Arc;

use crate::durable::context::Context;
use crate::durable::ids::Seq;
use crate::durable::types::{
    ConversationRecord, DocumentCommitChange, EntryRecord, StorageWrite, SubmissionRecord,
    TaskRecord,
};

/// A table write as it appears in a publication.
///
/// Document-safe mirror of the table half of [`StorageWrite`]: every write that
/// changes observable state, without the document content commands (documents
/// arrive separately as [`DocumentCommitChange`]).
// Wire/public enum: boxing `Task` would change every construction and match
// site (including tests) without changing behavior; a publication holds a
// handful of changes at a time.
#[allow(clippy::large_enum_variant)]
#[derive(Clone, Debug, PartialEq)]
pub enum TableCommitChange {
    /// A conversation was created or forked.
    Conversation(ConversationRecord),
    /// An entry was appended.
    Entry(EntryRecord),
    /// A task record was created or replaced.
    Task(TaskRecord),
    /// A submission was created or replaced.
    Submission(SubmissionRecord),
}

impl TableCommitChange {
    /// The conversation the change belongs to, when it is scoped to one.
    ///
    /// Upstream's view mounts filter by this: an entry and a submission carry
    /// their conversation; a task carries its conversation; a conversation
    /// record is its own.
    #[must_use]
    pub fn conversation_id(&self) -> Option<crate::durable::ids::ConversationId> {
        match self {
            Self::Conversation(record) => Some(record.id),
            Self::Entry(entry) => Some(entry.conversation_id),
            Self::Task(task) => Some(task.conversation_id),
            Self::Submission(submission) => Some(submission.conversation_id),
        }
    }
}

/// One observable change in a commit: a table write or a document change.
// Same reasoning as [`TableCommitChange`]: the table variant carries the
// public record types and boxing would break every construction site.
#[allow(clippy::large_enum_variant)]
#[derive(Clone, Debug, PartialEq)]
pub enum CommitChange {
    /// A table record.
    Table(TableCommitChange),
    /// A document creation, change, copy or retirement.
    Document(DocumentCommitChange),
}

/// One complete immutable commit, as listeners observe it.
#[derive(Clone, Debug, PartialEq)]
pub struct CommitPublication {
    /// The commit sequence.
    pub seq: Seq,
    /// Every observable change, table writes first in write order and then the
    /// adopted document changes in plan order (upstream `#publish`).
    pub changes: Vec<CommitChange>,
}

impl CommitPublication {
    /// Build the publication for one commit.
    ///
    /// `writes` is the batch the storage accepted. Document content writes are
    /// dropped because their observable form is the adopted
    /// [`DocumentCommitChange`]; retirement writes appear only through the
    /// adopted change, which carries the `retiredAt` stamp. Only the table
    /// records are copied — once — so a batch's document payloads are never
    /// cloned. The caller appends the adopted document changes in plan order
    /// after the storage call returned (upstream `#publish`).
    #[must_use]
    pub fn build(seq: Seq, writes: &[StorageWrite]) -> Self {
        let mut changes = Vec::with_capacity(writes.len());
        for write in writes {
            match write {
                StorageWrite::Conversation { value } => {
                    changes.push(CommitChange::Table(TableCommitChange::Conversation(
                        value.clone(),
                    )));
                }
                StorageWrite::Entry { value } => {
                    changes.push(CommitChange::Table(TableCommitChange::Entry(value.clone())));
                }
                StorageWrite::Task { value } => {
                    changes.push(CommitChange::Table(TableCommitChange::Task(value.clone())));
                }
                StorageWrite::Submission { value } => {
                    changes.push(CommitChange::Table(TableCommitChange::Submission(
                        value.clone(),
                    )));
                }
                // Document content and retirement reach observers through the
                // adopted changes, which alone carry the storage-stamped
                // lifetime (`createdAt`/`retiredAt`) and the exact ops.
                StorageWrite::DocumentCreate { .. }
                | StorageWrite::DocumentCopy { .. }
                | StorageWrite::DocumentChange { .. }
                | StorageWrite::DocumentRetire { .. } => {}
            }
        }
        Self { seq, changes }
    }
}

/// A synchronous internal observer. A throw fails the session.
pub type InternalListener = Arc<dyn Fn(&CommitPublication, &Context) + Send + Sync + 'static>;

/// A synchronous host observer. A throw is reported, never fatal.
pub type CommitListener = Arc<dyn Fn(&CommitPublication, &Context) + Send + Sync + 'static>;

/// A listener and the id that unsubscribes it.
type Entry = (u64, Arc<dyn Fn(&CommitPublication, &Context) + Send + Sync>);

/// The synchronous listener set of one session.
///
/// Registration order is preserved; removal is by id. Kept as a plain `Vec`
/// because a session has a handful of listeners (the scheduler, the submission
/// queue, at most one view mount per conversation), not thousands.
#[derive(Default)]
pub struct ListenerSet {
    listeners: Vec<Entry>,
    next_id: u64,
}

impl ListenerSet {
    /// Register `listener`; the returned id removes it again.
    pub fn add(&mut self, listener: CommitListener) -> u64 {
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1);
        self.listeners.push((id, listener));
        id
    }

    /// Remove one listener by id. Returns whether it was present.
    pub fn remove(&mut self, id: u64) -> bool {
        let before = self.listeners.len();
        self.listeners.retain(|(key, _)| *key != id);
        before != self.listeners.len()
    }

    /// Whether any listener is registered (the publication fast path).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.listeners.is_empty()
    }

    /// The registered listener count.
    #[must_use]
    pub fn len(&self) -> usize {
        self.listeners.len()
    }

    /// A snapshot of the listeners, in registration order.
    ///
    /// Publication iterates a snapshot so a listener that unsubscribes during
    /// dispatch cannot change the set being walked (upstream spreads the set
    /// for the same reason, `session.ts:792`).
    #[must_use]
    pub fn snapshot(&self) -> Vec<CommitListener> {
        self.listeners.iter().map(|(_, f)| f.clone()).collect()
    }

    /// Drop every listener (close).
    pub fn clear(&mut self) {
        self.listeners.clear();
    }
}

/// Run one listener, containing a panic.
///
/// Upstream's `contained()` (`session.ts:770-800`) runs a listener and routes a
/// throw to the caller's policy without skipping the others. Rust's equivalent
/// of a throw here is a panic in a user-supplied closure, so the containment is
/// `catch_unwind`; the message goes to the caller's policy (fail the session for
/// an internal listener, report for a host listener).
pub fn run_contained(
    listener: &CommitListener,
    publication: &CommitPublication,
    context: &Context,
) -> Result<(), String> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        listener(publication, context);
    }))
    .map_err(|payload| {
        payload
            .downcast_ref::<&str>()
            .map(|text| (*text).to_owned())
            .or_else(|| payload.downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "listener panicked".to_owned())
    })
}
