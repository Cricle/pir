//! Metrics: counter, up/down counter, and histogram instruments.
//!
//! Instruments mirror upstream `packages/telemetry/src/metrics.ts`. Creation is
//! allowed to allocate (names/metadata); the hot path is `add`/`record`, which
//! takes attributes by reference and performs one `Option` check when telemetry
//! is disabled. A noop instrument owns no sink and never touches its arguments.

use std::borrow::Cow;
use std::fmt;
use std::sync::Arc;

use crate::telemetry::attributes::Attributes;

/// OpenTelemetry-style default histogram bucket boundaries (milliseconds-like
/// scale, matching the upstream default adapter configuration).
pub const DEFAULT_HISTOGRAM_BOUNDARIES: &[f64] = &[
    0.0, 5.0, 10.0, 25.0, 50.0, 75.0, 100.0, 250.0, 500.0, 750.0, 1000.0, 2500.0, 5000.0, 7500.0,
    10000.0,
];

/// Optional instrument metadata (`description` / `unit`). Built once per
/// instrument, never on the measurement hot path.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct InstrumentOptions {
    pub description: Option<String>,
    pub unit: Option<String>,
}

impl InstrumentOptions {
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn with_description(mut self, description: impl Into<String>) -> Self {
        self.description = Some(description.into());
        self
    }

    #[must_use]
    pub fn with_unit(mut self, unit: impl Into<String>) -> Self {
        self.unit = Some(unit.into());
        self
    }
}

/// Histogram metadata plus bucket boundaries.
#[derive(Debug, Clone, PartialEq)]
pub struct HistogramOptions {
    pub description: Option<String>,
    pub unit: Option<String>,
    /// Strictly ascending finite boundaries. Use [`Self::with_boundaries`].
    pub boundaries: Cow<'static, [f64]>,
}

impl Default for HistogramOptions {
    fn default() -> Self {
        Self {
            description: None,
            unit: None,
            boundaries: Cow::Borrowed(DEFAULT_HISTOGRAM_BOUNDARIES),
        }
    }
}

impl HistogramOptions {
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn with_description(mut self, description: impl Into<String>) -> Self {
        self.description = Some(description.into());
        self
    }

    #[must_use]
    pub fn with_unit(mut self, unit: impl Into<String>) -> Self {
        self.unit = Some(unit.into());
        self
    }

    #[must_use]
    pub fn with_boundaries(mut self, boundaries: impl Into<Cow<'static, [f64]>>) -> Self {
        self.boundaries = boundaries.into();
        self
    }
}

/// Validate histogram boundaries: finite and strictly ascending.
pub fn are_valid_boundaries(boundaries: &[f64]) -> bool {
    boundaries.windows(2).all(|window| window[0] < window[1])
        && boundaries.iter().all(|boundary| boundary.is_finite())
}

/// Backend sink for a counter. Only called when the instrument is enabled.
pub trait CounterSink: Send + Sync {
    fn add(&self, value: u64, attributes: &Attributes);
}

/// Backend sink for a floating-point counter.
pub trait FloatCounterSink: Send + Sync {
    fn add(&self, value: f64, attributes: &Attributes);
}

/// Backend sink for an up/down counter.
pub trait UpDownCounterSink: Send + Sync {
    fn add(&self, value: i64, attributes: &Attributes);
}

/// Backend sink for a histogram.
pub trait HistogramSink: Send + Sync {
    fn record(&self, value: f64, attributes: &Attributes);
}

/// Monotonic counter handle.
#[derive(Clone, Default)]
pub struct Counter {
    sink: Option<Arc<dyn CounterSink>>,
}

impl Counter {
    /// Disabled instrument: `add` returns immediately without reading
    /// attributes.
    pub const fn noop() -> Self {
        Self { sink: None }
    }

    pub fn new(sink: Arc<dyn CounterSink>) -> Self {
        Self { sink: Some(sink) }
    }

    #[inline]
    pub fn add(&self, value: u64, attributes: &Attributes) {
        if let Some(sink) = &self.sink {
            sink.add(value, attributes);
        }
    }

    pub fn is_enabled(&self) -> bool {
        self.sink.is_some()
    }
}

impl fmt::Debug for Counter {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Counter")
            .field("enabled", &self.is_enabled())
            .finish()
    }
}

/// Floating-point monotonic counter handle.
///
/// Upstream's `TelemetryCounter.add(value)` accepts JavaScript numbers, so a
/// reported USD cost (`message.usage.cost.total`) is a fractional counter
/// value, not an integer. pir's [`Counter`] payload is `u64` (tokens, bytes,
/// counts); this instrument is the fractional counterpart used by
/// `pi.ai.cost` and any other f64-valued counter. Additive: a recorder that
/// does not implement [`TelemetryRecorder::float_counter`] returns a noop
/// instrument, so existing recorders keep compiling and behaving unchanged.
#[derive(Clone, Default)]
pub struct FloatCounter {
    sink: Option<Arc<dyn FloatCounterSink>>,
}

impl FloatCounter {
    pub const fn noop() -> Self {
        Self { sink: None }
    }

    pub fn new(sink: Arc<dyn FloatCounterSink>) -> Self {
        Self { sink: Some(sink) }
    }

    #[inline]
    pub fn add(&self, value: f64, attributes: &Attributes) {
        if let Some(sink) = &self.sink {
            sink.add(value, attributes);
        }
    }

    pub fn is_enabled(&self) -> bool {
        self.sink.is_some()
    }
}

impl fmt::Debug for FloatCounter {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("FloatCounter")
            .field("enabled", &self.is_enabled())
            .finish()
    }
}

/// Up/down counter handle; `add` takes signed deltas.
#[derive(Clone, Default)]
pub struct UpDownCounter {
    sink: Option<Arc<dyn UpDownCounterSink>>,
}

impl UpDownCounter {
    pub const fn noop() -> Self {
        Self { sink: None }
    }

    pub fn new(sink: Arc<dyn UpDownCounterSink>) -> Self {
        Self { sink: Some(sink) }
    }

    #[inline]
    pub fn add(&self, value: i64, attributes: &Attributes) {
        if let Some(sink) = &self.sink {
            sink.add(value, attributes);
        }
    }

    pub fn is_enabled(&self) -> bool {
        self.sink.is_some()
    }
}

impl fmt::Debug for UpDownCounter {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("UpDownCounter")
            .field("enabled", &self.is_enabled())
            .finish()
    }
}

/// Histogram handle. The sink owns aggregation; the handle keeps the bucket
/// boundaries for introspection.
#[derive(Clone, Default)]
pub struct Histogram {
    sink: Option<Arc<dyn HistogramSink>>,
    boundaries: Cow<'static, [f64]>,
}

impl Histogram {
    pub const fn noop() -> Self {
        Self {
            sink: None,
            boundaries: Cow::Borrowed(&[]),
        }
    }

    pub fn new(sink: Arc<dyn HistogramSink>, boundaries: Cow<'static, [f64]>) -> Self {
        Self {
            sink: Some(sink),
            boundaries,
        }
    }

    #[inline]
    pub fn record(&self, value: f64, attributes: &Attributes) {
        if let Some(sink) = &self.sink {
            sink.record(value, attributes);
        }
    }

    pub fn boundaries(&self) -> &[f64] {
        self.boundaries.as_ref()
    }

    pub fn is_enabled(&self) -> bool {
        self.sink.is_some()
    }
}

impl fmt::Debug for Histogram {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Histogram")
            .field("enabled", &self.is_enabled())
            .field("boundaries", &self.boundaries())
            .finish()
    }
}
