//! The storage failure guard and close ordering.
//!
//! Port of upstream `session.ts`'s `GuardedStorage` (`:951-1119`) and
//! `FailureLatch`. Every storage call the session makes goes through here.
//!
//! # The failure rule (spec §10.1)
//!
//! *Any* error a storage method returns ends the session: the first one wins,
//! it is reported once as `session_failed`, admission stops, and every later
//! call — plus every call still in flight — fails with [`SessionFailed`].
//! Nothing retries; transients are the backend's to retry, and reopening
//! recovers from what was committed.
//!
//! Two exemptions, both for **reads** only: a [`StorageError::Request`] (an
//! invalid read has no durable effect and says nothing about the backend), and
//! a read whose caller's context was already aborted. `commit` and `mint_id`
//! are never exempt: once a batch is admitted, whether it committed is unknown.
//!
//! # Close ordering
//!
//! Close must not let a backend call outlive the backend. The guard tracks
//! calls in flight and [`GuardedStorage::close`] waits for all of them before
//! reaching `Storage::close`. New calls are refused from that point, so a
//! backend read cannot start a next page while the backend closes.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use crate::durable::context::Context;
use crate::durable::errors::{ReportInfo, SessionFailed, StorageError};
use crate::durable::types::Storage;

/// Where a session reports failures that do not fail the calling operation.
///
/// The message is the rendered error; `info` says what happened, matching
/// upstream `Session.report(error, info)` and `HarnessOptions.onReport`.
/// A plain session drops reports (upstream's default `report()`). A handler
/// runs on the mutation line and must not panic; the harness contains its own
/// handler, as upstream does.
pub type SessionReport = Arc<dyn Fn(&str, ReportInfo) + Send + Sync>;

/// Where a guard reports the failure that ends its session.
///
/// The line implements it; tests can supply a counter. Kept as a trait so the
/// guard does not need the whole session to be constructed first (the session
/// constructs the guard).
pub trait FailureLatch: Send + Sync {
    /// The first failure, if the session already failed.
    fn failure(&self) -> Option<StorageError>;

    /// Record `error` as the session's failure, the first one winning.
    fn fail(&self, error: StorageError);
}

/// A callback run once, after the first failure is latched.
///
/// The session installs one that marks its line closing and asks it to run the
/// close listeners, so observers end on the failure without waiting for an
/// explicit [`Session::close`](crate::durable::session::facade::Session::close).
pub type FailureHook = Arc<dyn Fn(StorageError) + Send + Sync>;

/// The latch the session itself owns.
///
/// `fail` is idempotent: the first error wins and every later one is ignored.
/// The winning error is reported once as [`ReportInfo::SessionFailed`], after
/// the latch is already failed, so a report handler that calls back into the
/// session finds it failed (upstream `fail`, `session.ts:215-230`).
#[derive(Default)]
pub struct FailureLatchCore {
    failure: Mutex<Option<StorageError>>,
    failed: AtomicBool,
    report: Option<SessionReport>,
    hook: Mutex<Option<FailureHook>>,
}

impl std::fmt::Debug for FailureLatchCore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FailureLatchCore")
            .field("failed", &self.is_failed())
            .finish_non_exhaustive()
    }
}

impl FailureLatchCore {
    /// An empty latch that drops reports.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// An empty latch that reports its failure to `report`.
    #[must_use]
    pub fn with_report(report: SessionReport) -> Self {
        Self {
            report: Some(report),
            ..Self::default()
        }
    }

    /// Install the callback run once on the first failure.
    ///
    /// Replacing an installed hook is allowed; only the hook present when the
    /// first failure is latched runs.
    pub fn set_fail_hook(&self, hook: FailureHook) {
        *self
            .hook
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(hook);
    }

    /// Whether the session has failed.
    #[must_use]
    pub fn is_failed(&self) -> bool {
        self.failed.load(Ordering::Acquire)
    }
}

impl FailureLatch for FailureLatchCore {
    fn failure(&self) -> Option<StorageError> {
        self.failure
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    fn fail(&self, error: StorageError) {
        let first = {
            let mut slot = self
                .failure
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if slot.is_some() {
                false
            } else {
                *slot = Some(error.clone());
                true
            }
        };
        if !first {
            return;
        }
        self.failed.store(true, Ordering::Release);
        // Run the session's close-on-failure hook before reporting: observers
        // must see `SessionFailed` even if the host report handler misbehaves.
        let hook = self
            .hook
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        if let Some(hook) = hook {
            hook(error.clone());
        }
        // Reported outside the lock: a handler may call back into the session,
        // which reads the latch.
        if let Some(report) = &self.report {
            report(&error.to_string(), ReportInfo::SessionFailed);
        }
    }
}

/// A reservation taken before a guarded backend call is dispatched.
///
/// It is created only after the caller has passed the health check, and it is
/// dropped only after the call settles (or is cancelled), so `close` cannot
/// observe an empty counter while a call is about to start.
struct InFlightGuard<'a>(&'a AtomicUsize);

impl Drop for InFlightGuard<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

/// A [`Storage`] wrapper implementing the failure rule.
///
/// `commit` and `mint_id` go through [`Self::write`]; reads go through
/// [`Self::read`] with the caller's context. The distinction is the whole
/// exemption policy, and it is expressed in the call site rather than in a
/// sentinel argument (upstream passes `undefined` for the same purpose).
pub struct GuardedStorage {
    inner: Arc<dyn Storage>,
    latch: Arc<dyn FailureLatch>,
    in_flight: AtomicUsize,
    closing: AtomicBool,
}

impl GuardedStorage {
    /// Wrap `inner`, failing `latch` on the first error.
    #[must_use]
    pub fn new(inner: Arc<dyn Storage>, latch: Arc<dyn FailureLatch>) -> Self {
        Self {
            inner,
            latch,
            in_flight: AtomicUsize::new(0),
            closing: AtomicBool::new(false),
        }
    }

    /// The backend behind the guard, for building a call future.
    ///
    /// Only meaningful together with [`Self::write`] / [`Self::read`], which
    /// apply the failure rule to whatever the returned handle produces.
    #[must_use]
    pub fn storage(&self) -> Arc<dyn Storage> {
        self.inner.clone()
    }

    /// Calls currently in flight.
    #[must_use]
    pub fn in_flight(&self) -> usize {
        self.in_flight.load(Ordering::Acquire)
    }

    /// Whether close has begun (new calls are refused).
    #[must_use]
    pub fn is_closing(&self) -> bool {
        self.closing.load(Ordering::Acquire)
    }

    /// The first failure, if the session failed.
    #[must_use]
    pub fn failure(&self) -> Option<StorageError> {
        self.latch.failure()
    }

    /// Run one **write** call: never exempt from the failure rule.
    pub async fn write<T>(
        &self,
        call: impl std::future::Future<Output = Result<T, StorageError>>,
    ) -> Result<T, StorageError> {
        self.run(None, call).await
    }

    /// Run one **read** call, exempt when it is an invalid request or its
    /// caller's context was aborted.
    pub async fn read<T>(
        &self,
        context: &Context,
        call: impl std::future::Future<Output = Result<T, StorageError>>,
    ) -> Result<T, StorageError> {
        self.run(Some(context), call).await
    }

    async fn run<T>(
        &self,
        read: Option<&Context>,
        call: impl std::future::Future<Output = Result<T, StorageError>>,
    ) -> Result<T, StorageError> {
        self.assert_healthy()?;
        // Reserve before the closing check. `close` publishes `closing` and
        // then waits for this counter, so either it observes this reservation
        // and waits, or this call observes `closing` after the increment and
        // refuses without touching the backend.
        self.in_flight.fetch_add(1, Ordering::AcqRel);
        let _in_flight = InFlightGuard(&self.in_flight);
        self.assert_healthy()?;
        if self.is_closing() {
            return Err(StorageError::closed("Session"));
        }
        let result = call.await;
        match result {
            Ok(value) => {
                // A call still underway when another failed ends with
                // `SessionFailed` too: nothing it read or wrote is used.
                self.assert_healthy()?;
                Ok(value)
            }
            Err(error) => {
                // Another call already failed the session: this caller sees
                // `SessionFailed`, whatever its own call returned.
                if let Some(failure) = self.latch.failure() {
                    return Err(SessionFailed { cause: failure }.into());
                }
                let exempt = read.is_some_and(|context| {
                    matches!(error, StorageError::Request(_)) || context.is_aborted()
                });
                if !exempt {
                    self.latch.fail(error.clone());
                }
                Err(error)
            }
        }
    }

    /// Refuse everything once close began, after every in-flight call settled.
    pub async fn close(&self) -> Result<(), StorageError> {
        self.closing.store(true, Ordering::Release);
        while self.in_flight.load(Ordering::Acquire) > 0 {
            tokio::task::yield_now().await;
        }
        self.inner.close().await
    }

    fn assert_healthy(&self) -> Result<(), StorageError> {
        match self.latch.failure() {
            Some(error) => Err(SessionFailed { cause: error }.into()),
            None => Ok(()),
        }
    }
}

impl std::fmt::Debug for GuardedStorage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GuardedStorage")
            .field("in_flight", &self.in_flight())
            .field("closing", &self.is_closing())
            .finish_non_exhaustive()
    }
}
