//! In-memory test recorder: [`TestTelemetry`] captures spans (with links,
//! events, status, parent ids, and settle order) and aggregates metrics.
//!
//! Mirrors upstream `packages/telemetry/src/memory.ts` (`InMemoryTelemetryContext`)
//! with a Rust start/handle shape. Each instance is an isolated recording
//! scope; clone the handle to share the capture.

use std::borrow::Cow;
use std::collections::{BTreeMap, VecDeque};
use std::fmt;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::SystemTime;

use crate::telemetry::attributes::Attributes;
use crate::telemetry::context::{SpanId, TraceContext, TraceFlags, TraceId};
use crate::telemetry::facade::TelemetryRecorder;
use crate::telemetry::metrics::{
    are_valid_boundaries, Counter, CounterSink, FloatCounter, FloatCounterSink, Histogram,
    HistogramOptions, HistogramSink, InstrumentOptions, UpDownCounter, UpDownCounterSink,
};
use crate::telemetry::span::{SpanEvent, SpanLink, SpanOptions, SpanStatus, TelemetrySpan};
use crate::telemetry::store::{
    filter_metrics, find_traces_in, get_trace_in, interval_ms_for, now_ms, system_time_ms,
    MetricKind, MetricQuery, StoredMetric, StoredSpan, StoredSpanEvent, StoredSpanLink,
    StoredTrace, TelemetryStore, TracePage, TraceQuery,
};

/// Captured span snapshot.
#[derive(Debug, Clone, PartialEq)]
pub struct RecordedSpan {
    pub name: String,
    pub trace_id: TraceId,
    pub span_id: SpanId,
    pub parent_trace_id: Option<TraceId>,
    pub parent_span_id: Option<SpanId>,
    pub attributes: Attributes,
    pub events: Vec<SpanEvent>,
    pub links: Vec<SpanLink>,
    pub status: SpanStatus,
    pub started_at: SystemTime,
    pub ended_at: Option<SystemTime>,
    pub end_sequence: Option<u64>,
    pub ended: bool,
}

/// One aggregated counter point (grouped by attribute set).
#[derive(Debug, Clone, PartialEq)]
pub struct CounterPoint {
    pub attributes: Attributes,
    pub value: u64,
}

/// One aggregated up/down counter point.
#[derive(Debug, Clone, PartialEq)]
pub struct UpDownCounterPoint {
    pub attributes: Attributes,
    pub value: i64,
}

/// One raw histogram measurement.
#[derive(Debug, Clone, PartialEq)]
pub struct MetricPoint {
    pub attributes: Attributes,
    pub value: f64,
}

/// One aggregated fractional-counter point (grouped by attribute set).
#[derive(Debug, Clone, PartialEq)]
pub struct FloatCounterPoint {
    pub attributes: Attributes,
    pub value: f64,
}

/// Fractional counter snapshot: total plus per-attribute aggregation.
#[derive(Debug, Clone, PartialEq)]
pub struct FloatCounterSnapshot {
    pub name: String,
    pub description: Option<String>,
    pub unit: Option<String>,
    pub total: f64,
    pub points: Vec<FloatCounterPoint>,
}

/// Counter snapshot: total plus per-attribute aggregation.
#[derive(Debug, Clone, PartialEq)]
pub struct CounterSnapshot {
    pub name: String,
    pub description: Option<String>,
    pub unit: Option<String>,
    pub total: u64,
    pub points: Vec<CounterPoint>,
}

/// Up/down counter snapshot.
#[derive(Debug, Clone, PartialEq)]
pub struct UpDownCounterSnapshot {
    pub name: String,
    pub description: Option<String>,
    pub unit: Option<String>,
    pub total: i64,
    pub points: Vec<UpDownCounterPoint>,
}

/// Histogram snapshot: cumulative bucket counts (index `i` = values `<=
/// boundaries[i]`; the final bucket is `+Inf`), plus `sum`/`count`.
#[derive(Debug, Clone, PartialEq)]
pub struct HistogramSnapshot {
    pub name: String,
    pub description: Option<String>,
    pub unit: Option<String>,
    pub boundaries: Vec<f64>,
    pub bucket_counts: Vec<u64>,
    pub sum: f64,
    pub count: u64,
    pub points: Vec<MetricPoint>,
}

/// All metric snapshots, ordered by instrument name.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TestMetricsSnapshot {
    pub counters: Vec<CounterSnapshot>,
    pub float_counters: Vec<FloatCounterSnapshot>,
    pub up_down_counters: Vec<UpDownCounterSnapshot>,
    pub histograms: Vec<HistogramSnapshot>,
}

#[derive(Default)]
struct CounterData {
    description: Option<String>,
    unit: Option<String>,
    points: Vec<CounterPoint>,
}

#[derive(Default)]
struct FloatCounterData {
    description: Option<String>,
    unit: Option<String>,
    points: Vec<FloatCounterPoint>,
}

#[derive(Default)]
struct UpDownCounterData {
    description: Option<String>,
    unit: Option<String>,
    points: Vec<UpDownCounterPoint>,
}

struct HistogramData {
    description: Option<String>,
    unit: Option<String>,
    boundaries: Vec<f64>,
    /// Non-cumulative interval counts; length = boundaries + 1.
    counts: Vec<u64>,
    sum: f64,
    count: u64,
    points: Vec<MetricPoint>,
}

#[derive(Default)]
struct TestMetricsData {
    counters: BTreeMap<String, CounterData>,
    float_counters: BTreeMap<String, FloatCounterData>,
    up_down_counters: BTreeMap<String, UpDownCounterData>,
    histograms: BTreeMap<String, HistogramData>,
}

/// One raw measurement, kept for [`TelemetryStore::find_metrics`] (upstream
/// `InMemoryTelemetryContext` measurements).
#[derive(Debug, Clone)]
struct RecordedMeasurement {
    interval_ms: u64,
    kind: MetricKind,
    name: String,
    unit: Option<String>,
    value: f64,
    attributes: Attributes,
}

impl RecordedMeasurement {
    fn stored(&self) -> StoredMetric {
        StoredMetric::measurement(
            self.interval_ms,
            self.name.clone(),
            self.kind,
            self.unit.clone(),
            self.attributes.clone(),
            self.value,
        )
    }
}

#[derive(Default)]
struct TestState {
    /// In span-start order; the oldest is evicted when an optional
    /// [`TestTelemetry::with_max_spans`] cap is reached.
    spans: VecDeque<RecordedSpan>,
    metrics: TestMetricsData,
    measurements: Vec<RecordedMeasurement>,
    next_end_sequence: u64,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// In-memory recorder. Create one per test scope and wrap it with
/// `Telemetry::from_recorder(test.clone())`.
///
/// Retention is unbounded by default (a test recorder should see everything);
/// `InMemoryTelemetry` used as a long-lived host store should call
/// [`TestTelemetry::with_max_spans`] to bound what it keeps.
#[derive(Clone, Default)]
pub struct TestTelemetry {
    state: Arc<Mutex<TestState>>,
    /// Optional ring cap: when non-zero, the oldest span is evicted once this
    /// many are retained. Spans still held by a live [`TestSpan`] handle that
    /// was evicted simply stop recording.
    max_spans: usize,
}

impl TestTelemetry {
    pub fn new() -> Self {
        Self::default()
    }

    /// A recorder that retains at most `max_spans` spans, evicting the oldest
    /// (a bounded in-memory store instead of a vector that grows forever).
    /// `0` keeps the unbounded test behavior.
    pub fn with_max_spans(max_spans: usize) -> Self {
        Self {
            state: Arc::new(Mutex::new(TestState::default())),
            max_spans,
        }
    }

    /// Detached snapshots in span-start order.
    pub fn spans(&self) -> Vec<RecordedSpan> {
        lock(&self.state).spans.iter().cloned().collect()
    }

    /// First recorded span with `name`.
    pub fn span(&self, name: &str) -> Option<RecordedSpan> {
        lock(&self.state)
            .spans
            .iter()
            .find(|span| span.name == name)
            .cloned()
    }

    pub fn counter_snapshot(&self, name: &str) -> Option<CounterSnapshot> {
        let state = lock(&self.state);
        state
            .metrics
            .counters
            .get(name)
            .map(|data| snapshot_counter(name, data))
    }

    pub fn up_down_counter_snapshot(&self, name: &str) -> Option<UpDownCounterSnapshot> {
        let state = lock(&self.state);
        state
            .metrics
            .up_down_counters
            .get(name)
            .map(|data| snapshot_up_down_counter(name, data))
    }

    pub fn float_counter_snapshot(&self, name: &str) -> Option<FloatCounterSnapshot> {
        let state = lock(&self.state);
        state
            .metrics
            .float_counters
            .get(name)
            .map(|data| snapshot_float_counter(name, data))
    }

    pub fn histogram_snapshot(&self, name: &str) -> Option<HistogramSnapshot> {
        let state = lock(&self.state);
        state
            .metrics
            .histograms
            .get(name)
            .map(|data| snapshot_histogram(name, data))
    }

    pub fn metrics_snapshot(&self) -> TestMetricsSnapshot {
        let state = lock(&self.state);
        TestMetricsSnapshot {
            counters: state
                .metrics
                .counters
                .iter()
                .map(|(name, data)| snapshot_counter(name, data))
                .collect(),
            float_counters: state
                .metrics
                .float_counters
                .iter()
                .map(|(name, data)| snapshot_float_counter(name, data))
                .collect(),
            up_down_counters: state
                .metrics
                .up_down_counters
                .iter()
                .map(|(name, data)| snapshot_up_down_counter(name, data))
                .collect(),
            histograms: state
                .metrics
                .histograms
                .iter()
                .map(|(name, data)| snapshot_histogram(name, data))
                .collect(),
        }
    }

    /// Settled spans in the portable [`StoredSpan`] shape (upstream
    /// `storedSpans`), in span-start order.
    pub fn stored_spans(&self) -> Vec<StoredSpan> {
        lock(&self.state)
            .spans
            .iter()
            .filter(|span| span.ended)
            .map(stored_span)
            .collect()
    }

    /// Raw metric measurements in recording order, as count-one rows
    /// (upstream `findMetrics` over `getMetrics()`).
    pub fn measurements(&self) -> Vec<StoredMetric> {
        lock(&self.state)
            .measurements
            .iter()
            .map(RecordedMeasurement::stored)
            .collect()
    }

    /// Drop captured spans and measurements (the recorder stays usable).
    pub fn clear(&self) {
        let mut state = lock(&self.state);
        state.spans.clear();
        state.metrics = TestMetricsData::default();
        state.measurements.clear();
        state.next_end_sequence = 0;
    }
}

impl TelemetryStore for TestTelemetry {
    fn find_traces(&self, query: &TraceQuery) -> TracePage {
        find_traces_in(&self.stored_spans(), query)
    }

    fn get_trace(&self, trace_id: &str) -> Option<StoredTrace> {
        get_trace_in(&self.stored_spans(), trace_id)
    }

    fn find_metrics(&self, query: &MetricQuery) -> Vec<StoredMetric> {
        filter_metrics(&self.measurements(), query)
    }
}

/// Convert one recorded span into the portable [`StoredSpan`] shape
/// (upstream `storedSpans` mapping).
fn stored_span(recorded: &RecordedSpan) -> StoredSpan {
    let started_ms = system_time_ms(recorded.started_at);
    let ended_ms = recorded.ended_at.map_or(started_ms, system_time_ms);
    StoredSpan {
        id: recorded.span_id.to_string(),
        trace_id: recorded.trace_id.to_string(),
        parent_id: recorded.parent_span_id.map(|span_id| span_id.to_string()),
        name: recorded.name.clone(),
        started_ms,
        ended_ms,
        attributes: recorded.attributes.clone(),
        events: recorded
            .events
            .iter()
            .map(|event| StoredSpanEvent {
                name: event.name.clone(),
                time_ms: event.time_ms.unwrap_or(0),
                attributes: event.attributes.clone(),
            })
            .collect(),
        links: recorded
            .links
            .iter()
            .map(|link| StoredSpanLink {
                trace_id: link.context.trace_id().to_string(),
                span_id: link.context.span_id().to_string(),
                attributes: link.attributes.clone(),
            })
            .collect(),
        status: recorded.status.clone(),
    }
}

impl TelemetryRecorder for TestTelemetry {
    fn start_span(&self, options: &SpanOptions) -> Arc<dyn TelemetrySpan> {
        let parent = options.parent.as_ref().filter(|parent| parent.is_valid());
        let trace_id = parent.map_or_else(TraceId::generate, |parent| parent.trace_id());
        let trace_flags = parent.map_or(TraceFlags::SAMPLED, |parent| parent.trace_flags());
        let span_id = SpanId::generate();

        let mut context = TraceContext::new(trace_id, span_id, trace_flags);
        if let Some(trace_state) = parent.and_then(TraceContext::trace_state) {
            context = context.with_trace_state(trace_state);
        }

        let mut state = lock(&self.state);
        if self.max_spans > 0 {
            while state.spans.len() >= self.max_spans {
                state.spans.pop_front();
            }
        }
        let span_id_for_handle = span_id;
        state.spans.push_back(RecordedSpan {
            name: options.name.clone(),
            trace_id,
            span_id,
            parent_trace_id: parent.map(|parent| parent.trace_id()),
            parent_span_id: parent.map(|parent| parent.span_id()),
            attributes: options.attributes.clone(),
            events: Vec::new(),
            links: options
                .links
                .iter()
                .filter(|link| link.context.is_valid())
                .cloned()
                .collect(),
            status: SpanStatus::Ok,
            started_at: SystemTime::now(),
            ended_at: None,
            end_sequence: None,
            ended: false,
        });
        drop(state);

        Arc::new(TestSpan {
            state: Arc::clone(&self.state),
            span_id: span_id_for_handle,
            name: options.name.clone(),
            context,
        })
    }

    fn counter(&self, name: &str, options: &InstrumentOptions) -> Counter {
        let mut state = lock(&self.state);
        state
            .metrics
            .counters
            .entry(name.to_owned())
            .or_insert_with(|| CounterData {
                description: options.description.clone(),
                unit: options.unit.clone(),
                points: Vec::new(),
            });
        drop(state);
        Counter::new(Arc::new(TestCounterSink {
            state: Arc::clone(&self.state),
            name: name.to_owned(),
            unit: options.unit.clone(),
        }))
    }

    fn float_counter(&self, name: &str, options: &InstrumentOptions) -> FloatCounter {
        let mut state = lock(&self.state);
        state
            .metrics
            .float_counters
            .entry(name.to_owned())
            .or_insert_with(|| FloatCounterData {
                description: options.description.clone(),
                unit: options.unit.clone(),
                points: Vec::new(),
            });
        drop(state);
        FloatCounter::new(Arc::new(TestFloatCounterSink {
            state: Arc::clone(&self.state),
            name: name.to_owned(),
            unit: options.unit.clone(),
        }))
    }

    fn up_down_counter(&self, name: &str, options: &InstrumentOptions) -> UpDownCounter {
        let mut state = lock(&self.state);
        state
            .metrics
            .up_down_counters
            .entry(name.to_owned())
            .or_insert_with(|| UpDownCounterData {
                description: options.description.clone(),
                unit: options.unit.clone(),
                points: Vec::new(),
            });
        drop(state);
        UpDownCounter::new(Arc::new(TestUpDownCounterSink {
            state: Arc::clone(&self.state),
            name: name.to_owned(),
            unit: options.unit.clone(),
        }))
    }

    fn histogram(&self, name: &str, options: &HistogramOptions) -> Histogram {
        if !are_valid_boundaries(&options.boundaries) {
            return Histogram::noop();
        }
        let boundaries = options.boundaries.to_vec();
        let mut state = lock(&self.state);
        state
            .metrics
            .histograms
            .entry(name.to_owned())
            .or_insert_with(|| HistogramData {
                description: options.description.clone(),
                unit: options.unit.clone(),
                boundaries: boundaries.clone(),
                counts: vec![0; boundaries.len() + 1],
                sum: 0.0,
                count: 0,
                points: Vec::new(),
            });
        drop(state);
        Histogram::new(
            Arc::new(TestHistogramSink {
                state: Arc::clone(&self.state),
                name: name.to_owned(),
                unit: options.unit.clone(),
                boundaries: boundaries.clone(),
            }),
            Cow::Owned(boundaries),
        )
    }
}

/// Recorded span handle returned by [`TestTelemetry`].
pub struct TestSpan {
    state: Arc<Mutex<TestState>>,
    /// The span is addressed by id, not index: the recorder may evict the
    /// oldest spans (`with_max_spans`), which shifts every index.
    span_id: SpanId,
    name: String,
    context: TraceContext,
}

impl TestSpan {
    fn update(&self, update: impl FnOnce(&mut RecordedSpan)) {
        let mut state = lock(&self.state);
        if let Some(span) = state
            .spans
            .iter_mut()
            .find(|span| span.span_id == self.span_id)
        {
            if !span.ended {
                update(span);
            }
        }
    }
}

impl TelemetrySpan for TestSpan {
    fn name(&self) -> &str {
        &self.name
    }

    fn trace_context(&self) -> &TraceContext {
        &self.context
    }

    fn set_attributes(&self, attributes: &Attributes) {
        self.update(|span| span.attributes.merge(attributes));
    }

    fn add_event(&self, name: &str, attributes: &Attributes) {
        self.update(|span| {
            let time_ms =
                system_time_ms(SystemTime::now()).saturating_sub(system_time_ms(span.started_at));
            span.events.push(SpanEvent {
                name: name.to_owned(),
                attributes: attributes.clone(),
                time_ms: Some(time_ms),
            });
        });
    }

    fn add_link(&self, link: &SpanLink) {
        if !link.context.is_valid() {
            return;
        }
        self.update(|span| span.links.push(link.clone()));
    }

    fn set_status(&self, status: &SpanStatus) {
        self.update(|span| span.status = status.clone());
    }

    fn end(&self) {
        let mut state = lock(&self.state);
        let sequence = state.next_end_sequence;
        if let Some(span) = state
            .spans
            .iter_mut()
            .find(|span| span.span_id == self.span_id)
        {
            if span.ended {
                return;
            }
            span.ended = true;
            span.ended_at = Some(SystemTime::now());
            span.end_sequence = Some(sequence);
        }
        state.next_end_sequence = sequence + 1;
    }

    fn is_recording(&self) -> bool {
        lock(&self.state)
            .spans
            .iter()
            .find(|span| span.span_id == self.span_id)
            .is_some_and(|span| !span.ended)
    }
}

impl fmt::Debug for TestSpan {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("TestSpan")
            .field("name", &self.name)
            .field("span_id", &self.context.span_id())
            .finish()
    }
}

struct TestCounterSink {
    state: Arc<Mutex<TestState>>,
    name: String,
    unit: Option<String>,
}

impl CounterSink for TestCounterSink {
    fn add(&self, value: u64, attributes: &Attributes) {
        let mut state = lock(&self.state);
        let interval_ms = interval_ms_for(now_ms());
        {
            let Some(data) = state.metrics.counters.get_mut(&self.name) else {
                return;
            };
            if let Some(point) = data
                .points
                .iter_mut()
                .find(|point| point.attributes == *attributes)
            {
                point.value = point.value.saturating_add(value);
            } else {
                data.points.push(CounterPoint {
                    attributes: attributes.clone(),
                    value,
                });
            }
        }
        state.measurements.push(RecordedMeasurement {
            interval_ms,
            kind: MetricKind::Counter,
            name: self.name.clone(),
            unit: self.unit.clone(),
            value: value as f64,
            attributes: attributes.clone(),
        });
    }
}

struct TestFloatCounterSink {
    state: Arc<Mutex<TestState>>,
    name: String,
    unit: Option<String>,
}

impl FloatCounterSink for TestFloatCounterSink {
    fn add(&self, value: f64, attributes: &Attributes) {
        if !value.is_finite() {
            return;
        }
        let mut state = lock(&self.state);
        let interval_ms = interval_ms_for(now_ms());
        {
            let Some(data) = state.metrics.float_counters.get_mut(&self.name) else {
                return;
            };
            if let Some(point) = data
                .points
                .iter_mut()
                .find(|point| point.attributes == *attributes)
            {
                point.value += value;
            } else {
                data.points.push(FloatCounterPoint {
                    attributes: attributes.clone(),
                    value,
                });
            }
        }
        state.measurements.push(RecordedMeasurement {
            interval_ms,
            kind: MetricKind::Counter,
            name: self.name.clone(),
            unit: self.unit.clone(),
            value,
            attributes: attributes.clone(),
        });
    }
}

struct TestUpDownCounterSink {
    state: Arc<Mutex<TestState>>,
    name: String,
    unit: Option<String>,
}

impl UpDownCounterSink for TestUpDownCounterSink {
    fn add(&self, value: i64, attributes: &Attributes) {
        let mut state = lock(&self.state);
        let interval_ms = interval_ms_for(now_ms());
        {
            let Some(data) = state.metrics.up_down_counters.get_mut(&self.name) else {
                return;
            };
            if let Some(point) = data
                .points
                .iter_mut()
                .find(|point| point.attributes == *attributes)
            {
                point.value = point.value.saturating_add(value);
            } else {
                data.points.push(UpDownCounterPoint {
                    attributes: attributes.clone(),
                    value,
                });
            }
        }
        state.measurements.push(RecordedMeasurement {
            interval_ms,
            kind: MetricKind::UpDownCounter,
            name: self.name.clone(),
            unit: self.unit.clone(),
            value: value as f64,
            attributes: attributes.clone(),
        });
    }
}

struct TestHistogramSink {
    state: Arc<Mutex<TestState>>,
    name: String,
    unit: Option<String>,
    boundaries: Vec<f64>,
}

impl HistogramSink for TestHistogramSink {
    fn record(&self, value: f64, attributes: &Attributes) {
        if !value.is_finite() {
            return;
        }
        let mut state = lock(&self.state);
        let interval_ms = interval_ms_for(now_ms());
        {
            let Some(data) = state.metrics.histograms.get_mut(&self.name) else {
                return;
            };
            data.count = data.count.saturating_add(1);
            data.sum += value;
            let bucket = self
                .boundaries
                .partition_point(|boundary| *boundary < value);
            if let Some(count) = data.counts.get_mut(bucket) {
                *count = count.saturating_add(1);
            }
            data.points.push(MetricPoint {
                attributes: attributes.clone(),
                value,
            });
        }
        state.measurements.push(RecordedMeasurement {
            interval_ms,
            kind: MetricKind::Histogram,
            name: self.name.clone(),
            unit: self.unit.clone(),
            value,
            attributes: attributes.clone(),
        });
    }
}

fn snapshot_counter(name: &str, data: &CounterData) -> CounterSnapshot {
    CounterSnapshot {
        name: name.to_owned(),
        description: data.description.clone(),
        unit: data.unit.clone(),
        total: data
            .points
            .iter()
            .fold(0u64, |total, point| total.saturating_add(point.value)),
        points: data.points.clone(),
    }
}

fn snapshot_float_counter(name: &str, data: &FloatCounterData) -> FloatCounterSnapshot {
    FloatCounterSnapshot {
        name: name.to_owned(),
        description: data.description.clone(),
        unit: data.unit.clone(),
        total: data.points.iter().map(|point| point.value).sum(),
        points: data.points.clone(),
    }
}

fn snapshot_up_down_counter(name: &str, data: &UpDownCounterData) -> UpDownCounterSnapshot {
    UpDownCounterSnapshot {
        name: name.to_owned(),
        description: data.description.clone(),
        unit: data.unit.clone(),
        total: data
            .points
            .iter()
            .fold(0i64, |total, point| total.saturating_add(point.value)),
        points: data.points.clone(),
    }
}

fn snapshot_histogram(name: &str, data: &HistogramData) -> HistogramSnapshot {
    let mut running = 0u64;
    let bucket_counts = data
        .counts
        .iter()
        .map(|count| {
            running = running.saturating_add(*count);
            running
        })
        .collect();
    HistogramSnapshot {
        name: name.to_owned(),
        description: data.description.clone(),
        unit: data.unit.clone(),
        boundaries: data.boundaries.clone(),
        bucket_counts,
        sum: data.sum,
        count: data.count,
        points: data.points.clone(),
    }
}
