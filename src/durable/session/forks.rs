//! Fork document copies.
//!
//! Port of upstream `session/forks.ts`: when a conversation forks at an entry,
//! every conversation document of the parent that the child inherits is copied
//! into the child **from storage**, so definition code never runs at fork time
//! and the child can never observe a later change in the parent.
//!
//! Two policies exist per definition (spec §3.7):
//!
//! * `DocumentFork::AsOf` — rewindable documents alive at the fork entry's
//!   commit sequence are copied as of that sequence (the child sees exactly
//!   what the parent saw when the fork cut).
//! * `DocumentFork::Current` — latest-only documents are copied from the
//!   parent's current state.
//! * `DocumentFork::Initial` — the child starts from the definition's initial
//!   value and nothing is copied.
//!
//! The copy is expressed as a [`StorageWrite::DocumentCopy`]: storage reads the
//! source pair (pre-batch, so a parent change in the same transaction cannot
//! affect it), materializes one independent base at the source's stored
//! version, and stamps the child record. The transaction never sees the value.

use crate::durable::context::Context;
use crate::durable::documents::{address_id, DocumentSemantics};
use crate::durable::errors::StorageError;
use crate::durable::ids::{ConversationId, DocumentId, EntryId, Seq};
use crate::durable::session::guard::GuardedStorage;
use crate::durable::types::{
    DocumentCopySource, DocumentCreate, DocumentFork, DocumentHistory, DocumentPoint,
    DocumentPointQuery, DocumentQuery, DocumentRecord, DocumentScope, StorageWrite,
};

/// One planned child document copy.
#[derive(Clone, Debug, PartialEq)]
pub struct ForkDocumentCopy {
    /// The child create record; `id` is `0` until the Session mints one.
    pub record: DocumentCreate,
    /// The exact source incarnation and point.
    pub source: DocumentCopySource,
}

/// How many document records one page of the fork scan returns.
///
/// Upstream scans with a page size of 256; the number is internal to the scan
/// and never observable, so the Rust port uses the same value.
const FORK_SCAN_PAGE_SIZE: usize = 256;

/// Plan every document copy a fork at `at` needs for the child `child_id`.
///
/// The scan is fork-aware: `find_document`/`scan_documents` take a
/// [`DocumentPoint`] and the storage applies the conversation ancestry caps
/// itself, so a document inherited from the parent's own parent is copied too.
///
/// Returns the copies in ascending source incarnation order. Two sources for
/// one child address is a definition bug and is rejected (upstream
/// `prepareForkDocumentCopies` does the same).
pub async fn prepare_fork_document_copies(
    storage: &GuardedStorage,
    parent_conversation_id: ConversationId,
    at: EntryId,
    child_id: ConversationId,
    context: &Context,
) -> Result<Vec<ForkDocumentCopy>, StorageError> {
    // The fork point is the commit that appended the entry: a rewindable
    // document must be copied as of exactly the state that entry saw.
    //
    // The lookup is scoped to the parent's ancestry (`entry_in`, not `entry`):
    // a fork names one point in *this* conversation's timeline, so an entry
    // that exists but belongs to an unrelated conversation must be rejected
    // rather than used to write a dangling `ConversationParent`.
    let Some(entry) = storage
        .read(
            context,
            storage.storage().entry_in(parent_conversation_id, at),
        )
        .await?
    else {
        return Err(StorageError::invalid(format!(
            "Fork entry {at} is not visible from conversation {parent_conversation_id}"
        )));
    };
    let at_seq = entry.commit_seq;

    // Upstream `prepareForkDocumentCopies` (`forks.ts:44-60`) makes two
    // passes, with the policy part of the scan rather than inferred from each
    // source record:
    //
    // * the fork entry's own conversation as of the entry's commit for
    //   `asOf` documents — this is what makes a fork at an inherited entry
    //   carry the ancestor documents the parent itself only copied;
    // * the parent conversation at `current` for `current` documents — those
    //   are latest-state copies and must include documents created after the
    //   fork point.
    //
    // `plan_copy` still validates each source's own history/fork pair.
    let mut copies: Vec<ForkDocumentCopy> = Vec::new();
    collect_fork_copies(
        storage,
        entry.entry.conversation_id,
        DocumentPoint::Seq(at_seq),
        DocumentFork::AsOf,
        at_seq,
        child_id,
        context,
        &mut copies,
    )
    .await?;
    collect_fork_copies(
        storage,
        parent_conversation_id,
        DocumentPoint::Current,
        DocumentFork::Current,
        at_seq,
        child_id,
        context,
        &mut copies,
    )
    .await?;
    Ok(copies)
}

/// One upstream `collectCopies` pass (`forks.ts:62-92`).
#[allow(clippy::too_many_arguments)]
async fn collect_fork_copies(
    storage: &GuardedStorage,
    conversation_id: ConversationId,
    at: DocumentPoint,
    policy: DocumentFork,
    at_seq: Seq,
    child_id: ConversationId,
    context: &Context,
    copies: &mut Vec<ForkDocumentCopy>,
) -> Result<(), StorageError> {
    let mut cursor = None;
    loop {
        let page = storage
            .read(
                context,
                storage.storage().scan_documents(
                    DocumentQuery {
                        scope: DocumentScope::Conversation { conversation_id },
                        at: DocumentPointQuery::from(at),
                        kind: None,
                    },
                    FORK_SCAN_PAGE_SIZE,
                    cursor,
                ),
            )
            .await?;
        for source in &page.items {
            // Upstream filters by the pass's policy before planning. A record
            // written without a fork policy is selected by neither pass.
            if source.fork != Some(policy) {
                continue;
            }
            let Some(copy) = plan_copy(source, at_seq, child_id)? else {
                continue;
            };
            let child_address = crate::durable::types::DocumentAddress {
                kind: copy.record.kind.clone(),
                scope: copy.record.scope,
                key: copy.record.key.clone(),
            };
            if copies.iter().any(|existing| {
                let existing_address = crate::durable::types::DocumentAddress {
                    kind: existing.record.kind.clone(),
                    scope: existing.record.scope,
                    key: existing.record.key.clone(),
                };
                address_id(&existing_address) == address_id(&child_address)
            }) {
                return Err(StorageError::invalid(format!(
                    "Two fork sources for child document kind {} key {:?}",
                    copy.record.kind, copy.record.key
                )));
            }
            copies.push(copy);
        }
        match page.next {
            Some(next) => cursor = Some(next),
            None => break,
        }
    }
    Ok(())
}

/// Plan one copy, or `None` when the document does not follow the child.
///
/// The `(latest, asOf)` pair is rejected rather than silently downgraded to
/// current state: a latest-only document keeps no history, so there is no
/// value "as of" the fork point to copy. [`DocDefinition::define`] refuses that
/// semantics at definition time, but a record persisted by other means can
/// still carry it, and copying current state would hand the child a value the
/// parent never had at the fork point.
fn plan_copy(
    source: &DocumentRecord,
    at_seq: Seq,
    child_id: ConversationId,
) -> Result<Option<ForkDocumentCopy>, StorageError> {
    // Only conversation documents fork; a task/session document never does.
    let DocumentScope::Conversation { .. } = source.scope else {
        return Ok(None);
    };
    let history = source.history.unwrap_or(DocumentHistory::Latest);
    let fork = source.fork.unwrap_or(DocumentFork::Initial);
    let point = match (history, fork) {
        // The definitions decide: `initial` starts empty, `current` copies the
        // parent's current state, `asOf` copies the state the fork entry saw.
        (_, DocumentFork::Initial) => return Ok(None),
        (_, DocumentFork::Current) => DocumentPoint::Current,
        (DocumentHistory::Rewindable, DocumentFork::AsOf) => DocumentPoint::Seq(at_seq),
        (DocumentHistory::Latest, DocumentFork::AsOf) => {
            return Err(StorageError::invalid(format!(
                "Document {} has latest-only history and cannot fork asOf",
                source.kind
            )));
        }
    };
    let record = DocumentCreate {
        id: DocumentId::from_raw(0),
        kind: source.kind.clone(),
        key: source.key.clone(),
        scope: DocumentScope::Conversation {
            conversation_id: child_id,
        },
        history: source.history,
        fork: source.fork,
    };
    Ok(Some(ForkDocumentCopy {
        record,
        source: DocumentCopySource {
            id: source.id,
            at: point,
        },
    }))
}

/// Build the plan for one copy, given the child id the Session minted.
///
/// The copy is planned rather than staged as a bare write so that adoption
/// produces a [`crate::durable::types::DocumentCommitChange::Copy`] and the
/// session's document cache learns the new incarnation. A bare write would
/// reach storage and no observer.
#[must_use]
pub fn copy_plan(
    copy: &ForkDocumentCopy,
    child_document_id: DocumentId,
) -> crate::durable::session::assemble::DocumentPlan {
    let mut record = copy.record.clone();
    record.id = child_document_id;
    crate::durable::session::assemble::DocumentPlan::copy(record, copy.source)
}

/// Build the storage write for one copy, given the child id the Session minted.
///
/// Prefer [`copy_plan`]; this remains for a caller that wants the raw write
/// without a plan (storage-level conformance tests, the fork scan).
#[must_use]
pub fn copy_write(copy: &ForkDocumentCopy, child_document_id: DocumentId) -> StorageWrite {
    let mut record = copy.record.clone();
    record.id = child_document_id;
    StorageWrite::DocumentCopy {
        record,
        source: copy.source,
    }
}

/// The fork semantics a definition declares, or `Initial` for a definition
/// that does not fork (a session or task document).
#[must_use]
pub fn fork_semantics(semantics: DocumentSemantics) -> DocumentFork {
    semantics.fork().unwrap_or(DocumentFork::Initial)
}
