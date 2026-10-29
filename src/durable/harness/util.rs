//! Small helpers shared by the harness adapter modules.
//!
//! Port of upstream `packages/durable/src/harness/util.ts`: the engine-free
//! helpers a view mount, the task graph and the scheduler all need. `contained()`
//! (upstream `session.ts:770-800`) is added because two ports in this module
//! family need the same listener policy.
//!
//! # `Waiters` is not ported
//!
//! Upstream's `Waiters<K, T>` is a `Map<K, Set<promiseWithResolvers>>` with
//! settle-once, group-resolve, reject-all and abort-on-context. Its two users
//! are the scheduler (`#taskWaiters`, `#idleWaiters`) and the submission queue
//! (`#waiters`). In the Rust port both own a purpose-built registry of
//! `oneshot::Sender`s instead of sharing a generic one — see
//! [`crate::durable::harness::scheduler`]'s `task_waiters`/`idle_waiters` and
//! [`crate::durable::harness::submissions`]. A generic port would be a third
//! implementation nobody calls, so the semantics live where they are used.
//!
//! | item | upstream |
//! |---|---|
//! | [`scan_all`] | `scanAll()` |
//! | [`closed_error`] | `closedError()` |
//! | [`optional_extension`] | `optionalExtension()` |
//! | [`contained`] | `contained()` (`session.ts`) |
//! | — | `Waiters` (owned by its two users; see above) |

use std::sync::Arc;

use crate::durable::errors::{SessionFailed, StorageError};
use crate::durable::session::facade::Session;
use crate::durable::session::guard::{FailureLatch, FailureLatchCore};
use crate::durable::session::line::SessionHandle;
use crate::durable::types::{Cursor, Page};

/// A host report sink: what a contained listener failure is reported through.
///
/// Upstream's `Session.report(error)` hands the error object itself to a
/// Harness `onReport` callback. The Rust kernel has no report hook yet, so the
/// adapters take a sink of rendered messages instead; [`contained`] renders a
/// panic payload and passes it here.
pub type Report = Arc<dyn Fn(&str) + Send + Sync>;

/// What [`closed_error`] needs of a session: the failure that ended it, if any.
///
/// Upstream types the argument structurally as
/// `{ readonly failure: { readonly error: unknown } | undefined }`; a Rust trait
/// is the same structural argument, implemented for the kernel types that carry
/// the latch.
pub trait HasFailure {
    /// The first failure that ended the session, if any.
    fn failure(&self) -> Option<StorageError>;
}

impl HasFailure for Session {
    fn failure(&self) -> Option<StorageError> {
        // Inherent method: the facade's own accessor.
        crate::durable::session::facade::Session::failure(self)
    }
}

impl HasFailure for SessionHandle {
    fn failure(&self) -> Option<StorageError> {
        SessionHandle::failure(self)
    }
}

impl HasFailure for FailureLatchCore {
    fn failure(&self) -> Option<StorageError> {
        FailureLatch::failure(self)
    }
}

/// What a closed adapter rejects with.
///
/// Upstream `closedError(session)` returns `SessionFailed` with the session's
/// cause when it closed because the session failed, and a plain
/// `Error("Harness is closed")` otherwise. The Rust equivalent of "a plain
/// error" is [`StorageError::Closed`], which is what a closed session already
/// rejects with elsewhere in the kernel.
#[must_use]
pub fn closed_error(session: &dyn HasFailure) -> StorageError {
    match session.failure() {
        Some(cause) => StorageError::from(SessionFailed { cause }),
        None => StorageError::Closed("Harness is closed".to_owned()),
    }
}

/// `Some(extension)` when the extension is known.
///
/// Upstream spreads `optionalExtension(extension)` into a record literal
/// (`{ ...optionalExtension(extension) }`), which contributes the `extension`
/// key only when it is defined. The Rust records express that with
/// `#[serde(skip_serializing_if = "Option::is_none")]`, so the helper's whole
/// job is to keep the `Option` shape: absent stays absent, present becomes an
/// owned field value.
#[must_use]
pub fn optional_extension(extension: Option<&str>) -> Option<String> {
    extension.map(str::to_owned)
}

/// Every item of a paginated scan, in page order.
///
/// The Rust `Storage` scans take `(query, limit, cursor)`; the closure binds the
/// query and limit and receives the cursor, exactly like upstream's
/// `scanAll((cursor) => scan(..., cursor))`. It is generic over the future so a
/// caller can pass a boxed [`StorageFuture`] or a concrete future.
///
/// # Errors
///
/// The first page read that fails.
pub async fn scan_all<T, F, Fut>(mut scan: F) -> Result<Vec<T>, StorageError>
where
    F: FnMut(Option<Cursor>) -> Fut,
    Fut: std::future::Future<Output = Result<Page<T>, StorageError>>,
{
    let mut items = Vec::new();
    let mut cursor = None;
    loop {
        let page = scan(cursor).await?;
        items.extend(page.items);
        match page.next {
            Some(next) => cursor = Some(next),
            None => return Ok(items),
        }
    }
}

/// Run a host callback that must not fail its caller: a panic is reported
/// through `report` and never unwinds into the session.
///
/// Upstream `contained()` also routes a rejected promise; Rust's synchronous
/// listeners have no promise to adopt, so the panic is the whole case.
pub fn contained(report: &Report, callback: impl FnOnce()) {
    if let Err(payload) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(callback)) {
        report(&crate::durable::harness::scheduler::panic_message(payload));
    }
}
