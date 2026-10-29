//! The provider/host seam.
//!
//! [`Telemetry`] is a cheap, cloneable facade over an optional
//! [`TelemetryRecorder`]. `Telemetry::noop()` (also `Default`) holds no
//! recorder: span starts return a [`Span`](crate::telemetry::Span) with no
//! inner allocation, and metric instruments are sink-less handles whose
//! `add`/`record` methods return before touching their arguments.
//!
//! The AI path stores the caller's parent [`Span`] on
//! [`StreamOptions::telemetry`](crate::ai::types::StreamOptions::telemetry):
//! upstream's `ProviderRequestOptions.telemetry` is a span, and a span there
//! doubles as a context, so [`Span::telemetry`] reconstructs this facade from
//! the span's recorder for the `pi.ai.request` child span and its instruments.
//! Nothing here touches the install-telemetry gate or provider attribution
//! headers.

use std::fmt;
use std::sync::Arc;

use crate::telemetry::context::TraceContext;
use crate::telemetry::metrics::{
    Counter, FloatCounter, Histogram, HistogramOptions, InstrumentOptions, UpDownCounter,
};
use crate::telemetry::propagation::{self, Carrier};
use crate::telemetry::span::{Span, SpanOptions, TelemetrySpan};

/// Backend contract. Hosts implement this once; all instrumentation talks to
/// the [`Telemetry`] facade.
///
/// Propagation defaults to W3C Trace Context; a host with a different carrier
/// format may override `inject`/`extract`.
pub trait TelemetryRecorder: Send + Sync {
    /// Start a span. Return the recorder's span implementation; the facade
    /// wraps it in a [`Span`] handle.
    fn start_span(&self, options: &SpanOptions) -> Arc<dyn TelemetrySpan>;

    fn counter(&self, name: &str, options: &InstrumentOptions) -> Counter;

    /// Fractional monotonic counter (upstream's numeric `counter`, used for
    /// fractional USD costs). Defaults to a noop instrument so recorders that
    /// only aggregate integer counters stay source-compatible.
    fn float_counter(&self, _name: &str, _options: &InstrumentOptions) -> FloatCounter {
        FloatCounter::noop()
    }

    fn up_down_counter(&self, name: &str, options: &InstrumentOptions) -> UpDownCounter;

    fn histogram(&self, name: &str, options: &HistogramOptions) -> Histogram;

    fn inject(&self, context: &TraceContext, carrier: &mut Carrier) -> bool {
        propagation::inject(context, carrier)
    }

    fn extract(&self, carrier: &Carrier) -> Option<TraceContext> {
        propagation::extract(carrier)
    }
}

/// Cloneable telemetry entry point. Cloning shares the recorder.
#[derive(Clone, Default)]
pub struct Telemetry {
    recorder: Option<Arc<dyn TelemetryRecorder>>,
}

impl Telemetry {
    /// Disabled telemetry: every operation is a no-op. This is the zero-cost
    /// default for hosts that do not install a recorder.
    pub const fn noop() -> Self {
        Self { recorder: None }
    }

    pub fn new(recorder: Arc<dyn TelemetryRecorder>) -> Self {
        Self {
            recorder: Some(recorder),
        }
    }

    pub fn from_recorder<R: TelemetryRecorder + 'static>(recorder: R) -> Self {
        Self::new(Arc::new(recorder))
    }

    pub fn is_enabled(&self) -> bool {
        self.recorder.is_some()
    }

    pub fn recorder(&self) -> Option<&Arc<dyn TelemetryRecorder>> {
        self.recorder.as_ref()
    }

    /// Start a span described by `options` (name, attributes, links, parent).
    pub fn start_span(&self, options: SpanOptions) -> Span {
        match &self.recorder {
            Some(recorder) => Span::from_inner(recorder.start_span(&options), Arc::clone(recorder)),
            None => Span::noop(),
        }
    }

    pub fn start_span_named(&self, name: impl Into<String>) -> Span {
        self.start_span(SpanOptions::new(name))
    }

    /// Start a child span: the parent's trace/span identity is copied into
    /// `options.parent` unless the caller already set one. Delegates to the
    /// parent's recorder, matching upstream's span-as-context shape.
    pub fn start_child(&self, parent: &Span, options: SpanOptions) -> Span {
        parent.start_child(options)
    }

    pub fn counter(&self, name: &str, options: &InstrumentOptions) -> Counter {
        match &self.recorder {
            Some(recorder) => recorder.counter(name, options),
            None => Counter::noop(),
        }
    }

    /// Fractional counter (`pi.ai.cost` USD). Noop when telemetry is disabled
    /// or the recorder leaves the trait method at its default.
    pub fn float_counter(&self, name: &str, options: &InstrumentOptions) -> FloatCounter {
        match &self.recorder {
            Some(recorder) => recorder.float_counter(name, options),
            None => FloatCounter::noop(),
        }
    }

    pub fn up_down_counter(&self, name: &str, options: &InstrumentOptions) -> UpDownCounter {
        match &self.recorder {
            Some(recorder) => recorder.up_down_counter(name, options),
            None => UpDownCounter::noop(),
        }
    }

    pub fn histogram(&self, name: &str, options: &HistogramOptions) -> Histogram {
        match &self.recorder {
            Some(recorder) => recorder.histogram(name, options),
            None => Histogram::noop(),
        }
    }

    /// Inject into an existing carrier. Returns `false` when disabled or when
    /// the context is invalid.
    pub fn inject(&self, context: &TraceContext, carrier: &mut Carrier) -> bool {
        match &self.recorder {
            Some(recorder) => recorder.inject(context, carrier),
            None => false,
        }
    }

    /// Convenience: build a fresh carrier. `None` when disabled/invalid.
    pub fn inject_carrier(&self, context: &TraceContext) -> Option<Carrier> {
        let mut carrier = Carrier::new();
        if self.inject(context, &mut carrier) {
            Some(carrier)
        } else {
            None
        }
    }

    /// Extract a remote parent context.
    pub fn extract(&self, carrier: &Carrier) -> Option<TraceContext> {
        self.recorder
            .as_ref()
            .and_then(|recorder| recorder.extract(carrier))
    }
}

impl fmt::Debug for Telemetry {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Telemetry")
            .field("enabled", &self.is_enabled())
            .finish()
    }
}
