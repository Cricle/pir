//! Invocation-scoped values passed explicitly through every durable operation.
//!
//! Port of `@earendil-works/chord`'s `Context` (`packages/chord/src/context/
//! index.ts`): an immutable, cheap-to-clone carrier of the abort signal and the
//! telemetry span. The upstream `value<T>(key)` extension point for arbitrary
//! typed values is not ported — the durable runtime passes what it needs
//! explicitly, and a Rust equivalent would need `TypeId` juggling for no
//! consumer.
//!
//! Semantics that matter:
//!
//! * [`Context::background`] is upstream's `BACKGROUND_CONTEXT`: work that must
//!   never inherit a caller's cancellation (storage commits, off-line
//!   callbacks).
//! * [`Context::without_abort_signal`] marks an operation that must settle even
//!   when its caller cancels — upstream `withoutAbortSignal`, used around
//!   `Storage::commit`.
//! * Cloning shares the signal (a [`CancellationToken`]), so an `abort()` on
//!   one clone is observed by every other clone and by any waiter.
//!
//! The context also carries the runtime-only telemetry handle and propagation
//! carrier introduced by [`SpanHandle`] / [`TraceCarrier`]: a phase invocation
//! stores its started span here so events and reports can attach to the active
//! span, and stores the carrier so a nested invocation can link back to it.
//! Neither field is serialized; durable records persist carriers explicitly.

use std::sync::Arc;

use crate::ai::types::AbortSignal;
use crate::durable::harness::telemetry::{SpanHandle, TraceCarrier};

/// An invocation-scoped context: an optional abort signal, an optional
/// telemetry span name, and the runtime-only telemetry span handle and trace
/// carrier of the active invocation.
///
/// Cheap to clone (a handful of `Arc`s; [`SpanHandle`] is `Copy`). Equality is
/// by identity of the underlying allocations, which is what the scheduler
/// compares when it asks "is this the same invocation?".
#[derive(Clone, Debug, Default)]
pub struct Context {
    signal: Option<Arc<AbortSignal>>,
    span: Option<Arc<str>>,
    /// The telemetry span started for this invocation, when telemetry is
    /// installed. Runtime-only: never serialized.
    span_handle: Option<SpanHandle>,
    /// The propagation carrier to link nested work back to this invocation,
    /// when one was extracted or injected. Runtime-only: never serialized.
    trace_carrier: Option<Arc<TraceCarrier>>,
    /// The scope's display name, for diagnostics (`Context.toString()`).
    name: Option<Arc<str>>,
    /// True when the operation must settle regardless of its caller's abort
    /// (upstream `withoutAbortSignal`).
    detached: bool,
}

impl Context {
    /// A context with no signal and no span (upstream `BACKGROUND_CONTEXT`
    /// minus its name).
    #[must_use]
    pub fn background() -> Self {
        Self {
            signal: None,
            span: None,
            span_handle: None,
            trace_carrier: None,
            name: Some(Arc::from("[Context BACKGROUND_CONTEXT]")),
            detached: false,
        }
    }

    /// A context carrying `signal`.
    #[must_use]
    pub fn with_signal(signal: AbortSignal) -> Self {
        Self {
            signal: Some(Arc::new(signal)),
            ..Self::default()
        }
    }

    /// A context carrying a shared signal (the usual construction: the
    /// invocation's own signal is shared by everything it spawns).
    #[must_use]
    pub fn with_shared_signal(signal: Arc<AbortSignal>) -> Self {
        Self {
            signal: Some(signal),
            ..Self::default()
        }
    }

    /// Attach a telemetry span name (upstream `Context.telemetry`).
    #[must_use]
    pub fn with_span(mut self, span: impl Into<Arc<str>>) -> Self {
        self.span = Some(span.into());
        self
    }

    /// Attach the telemetry span handle started for this invocation, so
    /// events and reports can be recorded on the active phase span.
    #[must_use]
    pub fn with_span_handle(mut self, span: SpanHandle) -> Self {
        self.span_handle = Some(span);
        self
    }

    /// Attach the propagation carrier linking nested work back to this
    /// invocation.
    #[must_use]
    pub fn with_trace_carrier(mut self, carrier: impl Into<Arc<TraceCarrier>>) -> Self {
        self.trace_carrier = Some(carrier.into());
        self
    }

    /// Give this context a diagnostic name.
    #[must_use]
    pub fn with_name(mut self, name: impl Into<Arc<str>>) -> Self {
        self.name = Some(name.into());
        self
    }

    /// The abort signal, if this context has one.
    #[must_use]
    pub fn signal(&self) -> Option<&Arc<AbortSignal>> {
        self.signal.as_ref()
    }

    /// Whether this context's signal has been aborted. A context without a
    /// signal is never aborted.
    #[must_use]
    pub fn is_aborted(&self) -> bool {
        self.signal.as_ref().is_some_and(|s| s.is_aborted())
    }

    /// The telemetry span name, if any.
    #[must_use]
    pub fn span(&self) -> Option<&str> {
        self.span.as_deref()
    }

    /// The telemetry span handle started for this invocation, if any.
    #[must_use]
    pub fn span_handle(&self) -> Option<SpanHandle> {
        self.span_handle
    }

    /// The propagation carrier linking nested work back to this invocation,
    /// if any.
    #[must_use]
    pub fn trace_carrier(&self) -> Option<&TraceCarrier> {
        self.trace_carrier.as_deref()
    }

    /// Whether this context is detached from its caller's cancellation.
    #[must_use]
    pub fn is_detached(&self) -> bool {
        self.detached
    }

    /// The same context with its signal removed and marked detached: the
    /// operation settles regardless of the caller (upstream
    /// `withoutAbortSignal`). The span, span handle, trace carrier and name
    /// are preserved.
    #[must_use]
    pub fn without_abort_signal(&self) -> Self {
        Self {
            signal: None,
            span: self.span.clone(),
            span_handle: self.span_handle,
            trace_carrier: self.trace_carrier.clone(),
            name: self.name.clone(),
            detached: true,
        }
    }

    /// The same context with `signal` as its abort signal (upstream
    /// `withAbortSignal`), preserving span, span handle, trace carrier, name
    /// and the detached flag.
    #[must_use]
    pub fn with_abort_signal(&self, signal: Arc<AbortSignal>) -> Self {
        Self {
            signal: Some(signal),
            span: self.span.clone(),
            span_handle: self.span_handle,
            trace_carrier: self.trace_carrier.clone(),
            name: self.name.clone(),
            detached: self.detached,
        }
    }

    /// Resolve as soon as this context is aborted; never resolves for a
    /// context without a signal.
    pub async fn cancelled(&self) {
        match &self.signal {
            Some(signal) => signal.cancelled().await,
            None => std::future::pending().await,
        }
    }

    /// Return an error if this context is aborted.
    pub fn check(&self) -> Result<(), crate::ai::types::AbortedError> {
        match &self.signal {
            Some(signal) => signal.check(),
            None => Ok(()),
        }
    }
}

impl std::fmt::Display for Context {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.name {
            Some(name) => f.write_str(name),
            None => f.write_str("[Context]"),
        }
    }
}
