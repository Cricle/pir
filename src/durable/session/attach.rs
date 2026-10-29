//! Attaching an observer to one committed incarnation.
//!
//! Port of upstream `packages/durable/src/session/session.ts`
//! `#attachDocument` (`:824-863`) and `observedOperations` (`:930-947`), with
//! the observer itself from `session/observation.ts`.
//!
//! # What an attachment is
//!
//! ```text
//! check definition + scope          session.ts:829-830 (done before attach)
//!   -> pin the incarnation          #pinDocument (:699-714), called at :834
//!   -> observer over the value      CommittedStateSource (observation.ts:22-73)
//!   -> internal commit listener      :848-860
//!   -> close listener                :861 -> closeSession (:220-225)
//! ```
//!
//! The observer is bound to exactly one incarnation: changes to any other
//! record — and every table change — are ignored (`:849-851`). Retirement is
//! terminal: the observer receives the canonical `[["r", null]]` frame and
//! keeps it, while the incarnation stops being pinned (`:857-859`).
//!
//! # Why the state is shared
//!
//! The commit listener is a `ListenerSet` entry: it runs synchronously on the
//! line but without access to [`SessionInner`] (`publication.rs` listener
//! signature). The pin, however, lives in the session's [`DocumentCache`], so
//! only a `&mut SessionInner` call can release it. [`attach_state`],
//! [`deliver`] and [`detach`] therefore share one small [`AttachState`] behind
//! a mutex:
//!
//! * the registered listener applies publications (it must be cheap and must
//!   not panic — it only locks, filters and advances the observer);
//! * [`deliver`] is the line entry point the facade calls through
//!   `SessionHandle::maintain`; it applies the same fan-out and performs the
//!   pin release a listener cannot do. A publication already applied by the
//!   listener (same `seq`) is not applied twice: only the deferred pin release
//!   runs;
//! * [`detach`] removes the listener and releases the pin.
//!
//! [`DocumentCache`]: super::documents::DocumentCache

use std::sync::{Arc, Mutex, MutexGuard};

use serde_json::Value;

use crate::durable::context::Context;
use crate::durable::delta::Op;
use crate::durable::ids::{DocumentId, Seq};
use crate::durable::session::documents::LoadedDocument;
use crate::durable::session::guard::FailureLatch;
use crate::durable::session::line::SessionInner;
use crate::durable::session::observation::{retirement_operations, CommittedStateSource, WatchEnd};
use crate::durable::session::publication::{CommitChange, CommitPublication};
use crate::durable::types::DocumentCommitChange;

/// One observer attached to one committed incarnation.
///
/// Held by the caller for as long as it wants the committed stream; the
/// session keeps the matching listener and pin until [`detach`].
#[derive(Clone, Debug)]
pub struct Attached {
    /// The observer. Its value starts at the incarnation's committed revision.
    pub source: CommittedStateSource,
    /// The logical address the incarnation lives at.
    pub address_id: String,
    /// The pinned incarnation.
    pub record_id: DocumentId,
    /// The internal listener that forwards this incarnation's commits.
    pub listener_id: u64,
    /// Delivery bookkeeping shared with the listener and [`detach`].
    state: Arc<Mutex<AttachState>>,
}

/// What a host holds for one attached observer: the incarnation and the
/// definition version its observer was hydrated at.
///
/// Upstream carries this inline as `observed.version` (`session.ts:847`); it is
/// exposed here so a host can describe an attachment without borrowing the
/// observer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AttachHost {
    /// The logical address of the incarnation.
    pub address_id: String,
    /// The incarnation record.
    pub record_id: DocumentId,
    /// Value version the observer is hydrated at.
    pub version: i64,
}

impl Attached {
    /// The host-held description of this observer, at its current version.
    #[must_use]
    pub fn host(&self) -> AttachHost {
        AttachHost {
            address_id: self.address_id.clone(),
            record_id: self.record_id,
            version: lock(&self.state).version,
        }
    }
}

/// Per-attachment delivery state.
///
/// `version` mirrors upstream's `observed.version` (`session.ts:847`) and is
/// advanced only when a committed change carries a different definition
/// version. `last_seq` makes the listener and [`deliver`] idempotent together.
#[derive(Debug)]
struct AttachState {
    source: CommittedStateSource,
    address_id: String,
    record_id: DocumentId,
    version: i64,
    retired: bool,
    closed: bool,
    pinned: bool,
    detached: bool,
    last_seq: Option<Seq>,
}

fn lock(state: &Arc<Mutex<AttachState>>) -> MutexGuard<'_, AttachState> {
    state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Register the observer on the session's internal listener set and pin the
/// incarnation. Called on the line (inside `maintain`).
///
/// The definition checks upstream runs first (`checkRecordScope` /
/// `checkRecordVersion`, `session.ts:829-830`) are the loader's: a
/// [`LoadedDocument`] reaching this function is already materialized and
/// version-checked.
pub fn attach_state(
    inner: &mut SessionInner,
    address_id: String,
    loaded: &LoadedDocument,
) -> Attached {
    // `session.ts:834` — attached, the incarnation stays cached; once its last
    // observer detaches, or it retires, it is idle again (`#pinDocument`,
    // `:699-714`). The guard is inert: the pin is released explicitly by
    // `detach` / retirement, which are the only paths that run `unpin`.
    let _ = inner.documents.pin(loaded);
    let source = CommittedStateSource::new(Some(loaded.value.clone()), loaded.value_version);
    let attached_address = address_id.clone();
    let state = Arc::new(Mutex::new(AttachState {
        source: source.clone(),
        address_id,
        record_id: loaded.record.id,
        version: loaded.value_version,
        retired: false,
        closed: false,
        pinned: true,
        detached: false,
        last_seq: None,
    }));

    // `session.ts:848-860` — forward this incarnation's committed changes. The
    // listener is cheap and panic-free: it delegates to the state-side delivery
    // (`deliver_to_source`), which locks, filters and advances the observer; it
    // never touches the session.
    let listening = state.clone();
    let listener_id = inner.internal_listeners.add(Arc::new(
        move |publication: &CommitPublication, context: &Context| {
            deliver_to_source(
                &listening,
                publication,
                // `session.ts:852` — a document state's frames carry no caller
                // cancellation.
                context.without_abort_signal(),
            );
        },
    ));

    // `session.ts:861` — `subscribeClose(() => observer.closeSession(#failure))`.
    // A close listener runs synchronously on the line during `shutdown`, before
    // the listener sets are cleared, so the observer ends exactly once.
    let closing = state.clone();
    let latch = inner.latch.clone();
    inner.close_listeners.push(Arc::new(move || {
        let mut state = lock(&closing);
        if state.detached {
            // Detached observers are not the session's to end.
            return;
        }
        if state.retired {
            // Retirement already won: the source records `Retired` and the
            // terminal frame must remain drainable. A later close/failure must
            // not replace it with `SessionClosed`/`SessionFailed`.
            return;
        }
        state.closed = true;
        let reason = match latch.failure() {
            Some(error) => WatchEnd::SessionFailed(error),
            None => WatchEnd::SessionClosed,
        };
        state.source.end(reason);
    }));

    let attached = Attached {
        source,
        address_id: attached_address,
        record_id: loaded.record.id,
        listener_id,
        state,
    };
    // Keep the line-side handle: retirement delivery needs `&mut SessionInner`
    // to release the pin and evict the cache entry, which a commit listener
    // cannot reach on its own.
    inner.attachments.push(attached.clone());
    attached
}

/// Drop the observer's listener and unpin the incarnation.
pub fn detach(inner: &mut SessionInner, attached: &Attached) {
    inner.internal_listeners.remove(attached.listener_id);
    inner
        .attachments
        .retain(|registered| registered.listener_id != attached.listener_id);
    let mut state = lock(&attached.state);
    if state.detached {
        return;
    }
    state.detached = true;
    release_pin(inner, &mut state);
}

/// The publication fan-out for one attached observer.
///
/// The facade calls this on the line after a commit's publication; the
/// observer's registered listener applies the same publication during
/// `SessionInner`'s internal fan-out, so a publication already seen by the
/// listener is skipped here (only the pin release a listener cannot perform is
/// applied).
pub fn deliver(inner: &mut SessionInner, attached: &Attached, publication: &CommitPublication) {
    let mut state = lock(&attached.state);
    if state.detached {
        return;
    }
    if state.last_seq != Some(publication.seq) {
        apply(
            &mut state,
            publication,
            Context::background().without_abort_signal(),
        );
    }
    // `session.ts:857-859` — a retired incarnation never changes again: its
    // observers keep their last value but pin nothing. Releasing the pin needs
    // the line's `DocumentCache`, which is why the listener cannot do it itself.
    if state.retired {
        release_pin(inner, &mut state);
        // A retired incarnation must not keep the address occupied: drop it so
        // the next edit starts a fresh incarnation instead of reusing retired
        // cached state. Only the matching incarnation is removed, never a
        // replacement created at the same address.
        if inner
            .documents
            .get(&state.address_id)
            .is_some_and(|document| document.record.id == state.record_id)
        {
            inner.documents.remove(&state.address_id);
        }
    }
}

/// The state-side half of [`deliver`]: the registered listener's whole body.
///
/// Shares the attachment's state with the observer; it holds no session, so it
/// cannot release the pin and does not try to.
fn deliver_to_source(
    state: &Arc<Mutex<AttachState>>,
    publication: &CommitPublication,
    context: Context,
) {
    let mut state = lock(state);
    apply(&mut state, publication, context);
}

/// Apply one committed publication to the observer (`session.ts:849-860`).
fn apply(state: &mut AttachState, publication: &CommitPublication, context: Context) {
    state.last_seq = Some(publication.seq);
    // Retirement is terminal and must survive a close that raced ahead of this
    // listener. In particular, an internal listener failure runs its close
    // listener before this fan-out continues; the closed flag must not suppress
    // the retired marker, the terminal frame, or the later pin/cache cleanup.
    let retirement_version = publication.changes.iter().find_map(|change| match change {
        CommitChange::Document(DocumentCommitChange::Document {
            record,
            version,
            value: None,
            ..
        }) if record.id == state.record_id => Some(*version),
        _ => None,
    });
    if let Some(version) = retirement_version {
        if !state.retired {
            state.retired = true;
            state
                .source
                .advance_retirement(version.unwrap_or(state.version), context);
        }
        return;
    }
    // `observation.ts:50-53` — a retired or closed source ignores later frames.
    if state.retired || state.closed {
        return;
    }
    for change in &publication.changes {
        let CommitChange::Document(DocumentCommitChange::Document {
            record,
            version,
            value,
            ops,
            ..
        }) = change
        else {
            // `session.ts:850` — only document changes can match; a copy is a
            // different incarnation and never observes this one.
            continue;
        };
        // `session.ts:850` — only the attached incarnation is delivered.
        if record.id != state.record_id {
            continue;
        }
        let ops = observed_operations(state, *version, value, ops);
        // `session.ts:855-856` — a migration-only base changes nothing for an
        // observer of the new version.
        if ops.is_empty() {
            continue;
        }
        if value.is_none() {
            // Defensive: the scan above handles retirement, but keep this
            // path terminal should a future publication shape bypass it.
            if !state.retired {
                state.retired = true;
                state
                    .source
                    .advance_retirement(version.unwrap_or(state.version), context.clone());
            }
            continue;
        }
        state.source.advance(
            value.clone(),
            ops,
            version.unwrap_or(state.version),
            context.clone(),
        );
    }
}

/// Upstream `observedOperations` (`session.ts:934-943`).
///
/// Retirement always yields `[["r", null]]`. A version change yields one root
/// replacement and advances the observer's version, because the observer holds
/// a value of the old shape. Otherwise the exact adopted operations pass
/// through unchanged.
fn observed_operations(
    state: &mut AttachState,
    version: Option<i64>,
    value: &Option<serde_json::Map<String, Value>>,
    ops: &[Op],
) -> Vec<Op> {
    match (value, version) {
        // `session.ts:938`
        (None, _) => retirement_operations(),
        // `session.ts:939-941`
        (Some(_), Some(version)) if version != state.version => {
            state.version = version;
            vec![Op::Replace(
                value.clone().map(Value::Object).unwrap_or(Value::Null),
            )]
        }
        // `session.ts:939`
        (Some(_), _) => ops.to_vec(),
    }
}

/// Release the incarnation's pin once, on the line.
fn release_pin(inner: &mut SessionInner, state: &mut AttachState) {
    if !state.pinned {
        return;
    }
    state.pinned = false;
    let now = inner.now();
    inner
        .documents
        .unpin(state.record_id, &state.address_id, now);
    // The incarnation is idle again; arm its eviction.
    inner.arm_document_sweep();
}
