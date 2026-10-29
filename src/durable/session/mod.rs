//! The durable Session kernel: one mutation line, atomic commits over records
//! and documents, committed publication, and document observation.
//!
//! Port of upstream `packages/durable/src/session/{session,transaction,
//! observation,forks}.ts`. The spec it implements is `docs/durable-spec.md`
//! §1 (terms and invariants), §2 (core records), §4 (transactions and storage
//! ownership), §9 (document observation and views) and §10 (storage contract);
//! upstream's own `docs/spec.md` remains the final authority for anything the
//! digest does not restate, and §12 there-in records the Rust deviations.
//!
//! # The one rule
//!
//! *Only committed state is observable.* A commit callback stages table writes
//! and document edits; nothing becomes visible — to any reader, any watcher,
//! any listener — until the underlying [`Storage::commit`] settled.
//!
//! # The mutation line
//!
//! Upstream serializes commits on a promise chain (`SessionImpl.#tail`). Rust
//! gets the same guarantee structurally: [`SessionInner`] is owned by one
//! `tokio` task ([`line`]) and is never shared. Callers hold a
//! [`SessionHandle`] and send [`LineCommand`]s; the line answers on a `oneshot`.
//! Consequences that the kernel relies on:
//!
//! * No `Send` future ever holds a lock, so a phase handler may await a commit
//!   while running off the line (the scheduler does exactly that).
//! * Commit listeners run synchronously on the line, with `&mut SessionInner`.
//! * Close seals admission by refusing new commands; anything already admitted
//!   settles because the line owns it, not the caller.
//! * `Tx` deliberately has **no** `SessionHandle`: a commit callback physically
//!   cannot re-enter the line, which is what makes the single-writer invariant
//!   true by construction rather than by documentation.
//!
//! # Invariants (spec §1)
//!
//! 1. One commit is atomic across every record and document write.
//! 2. A document update is published only after its storage commit succeeded.
//! 3. Every visible progress is durable; there is no volatile publication path.
//! 4. External effects never run inside the mutation transaction.
//! 5. Entries and IDs are immutable and never reused.
//! 6. Document drafts are fully revoked when their transaction callback
//!    settles.
//! 7. The line is held through storage settlement and committed-state adoption.
//! 8. A storage error fails the session: it publishes nothing and must be
//!    reopened.
//!
//! # Module map
//!
//! | module | upstream | contents |
//! |---|---|---|
//! | [`line`] | `session.ts` `#enqueue`/`#runCommit` | owner task, commands, commit pipeline |
//! | [`guard`] | `session.ts` `GuardedStorage` | failure latch, in-flight tracking, close ordering |
//! | [`transaction`] | `transaction.ts` | `Tx`, staging, assembly, adoption |
//! | [`documents`] | `session.ts` document cache | loaded trackers, retention, pinning |
//! | [`publication`] | `session.ts` `#publish` | `CommitPublication`, listener fan-out |
//! | [`observation`] | `observation.ts` | committed state sources and watches |
//! | [`forks`] | `forks.ts` | fork document copies |

pub mod assemble;
pub mod attach;
pub mod documents;
pub mod facade;
pub mod forks;
pub mod guard;
pub mod line;
pub mod observation;
pub mod publication;
pub mod transaction;

pub use assemble::{
    adopt, apply_submission_change, assemble, assemble_with, plan_document, AssemblyInput,
    DocBehaviours, DocumentCreateOrRecord, DocumentPlan, NoDocBehaviours, PlanContent, TaskLookup,
};
pub use attach::{attach_state, deliver, detach, AttachHost, Attached};
pub use documents::{DocumentCache, DocumentCacheSizes, LoadedDocument, PinGuard};
pub use facade::{
    ConversationCreatedFuture, ConversationCreatedHook, ConversationOwnership, Session,
    SessionClosed, SubmissionCreate, TaskDefinitionLike,
};
pub use guard::{FailureLatch, FailureLatchCore, GuardedStorage, SessionReport};
pub use line::{
    CloseListener, CommitOptions, CommitPersistence, DeferDeadline, HeldDocument, LineCommand,
    SessionEnd, SessionHandle, SessionInner, SessionOptions, DEFAULT_PERSIST_WITHIN_MS,
};
pub use publication::{
    run_contained, CommitChange, CommitListener, CommitPublication, InternalListener, ListenerSet,
    TableCommitChange,
};
pub use transaction::{
    HeadTarget, StagedDocument, SubmissionChange, TaskOptions, TaskOwnership, Transaction,
    TransactionScope,
};
