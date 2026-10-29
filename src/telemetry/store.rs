//! Portable telemetry read surface: settled spans, traces, and metrics.
//!
//! Mirrors upstream `packages/telemetry/src/index.ts`
//! (`StoredSpan`/`StoredTrace`/`TraceQuery`/`MetricQuery`/`StoredMetric`/
//! `TelemetryStore`) and the reference implementation in `memory.ts`
//! (`storedSpans`/`getTrace`/`findTraces`/`findMetrics`).
//!
//! Upstream's contract methods return promises; the Rust port is synchronous
//! because the in-memory implementation and every host-side store pir ships
//! are synchronous. A backend that needs I/O can still implement the trait
//! behind its own lock/blocking wrapper.
//!
//! Two in-memory implementations are provided:
//!
//! - [`crate::telemetry::TestTelemetry`] records while it runs and implements
//!   [`TelemetryStore`] over its settled spans/raw measurements (the
//!   `InMemoryTelemetryContext` role in upstream `memory.ts`).
//! - [`MemoryTelemetryStore`] is a plain, backend-neutral collection of
//!   already-materialized [`StoredSpan`]/[`StoredMetric`] rows with the same
//!   query semantics, for hosts that export from a different recorder.
//!
//! Query semantics follow upstream `findTraces` exactly: spans are grouped by
//! trace id after a `(startedMs, id)` sort, a trace matches when its EARLIEST
//! span matches the name/bounds and ANY span matches the attribute/error
//! filter, and pages are ordered newest-trace-first with the opaque cursor
//! encoding `[startedMs, traceId]`.

use std::collections::BTreeMap;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::telemetry::attributes::Attributes;
use crate::telemetry::span::SpanStatus;

/// Default page size for [`TelemetryStore::find_traces`] (upstream `limit ??
/// 20`).
pub const DEFAULT_TRACE_PAGE_SIZE: usize = 20;

/// One settled span snapshot (upstream `StoredSpan`). Events carry times
/// relative to the span start; attributes are detached copies.
#[derive(Debug, Clone, PartialEq)]
pub struct StoredSpan {
    /// Backend span id. The in-memory recorder uses the span's hex id.
    pub id: String,
    pub trace_id: String,
    pub parent_id: Option<String>,
    pub name: String,
    /// Unix epoch milliseconds.
    pub started_ms: u64,
    /// Unix epoch milliseconds (`started_ms` when the end is unknown).
    pub ended_ms: u64,
    pub attributes: Attributes,
    pub events: Vec<StoredSpanEvent>,
    pub links: Vec<StoredSpanLink>,
    pub status: SpanStatus,
}

impl StoredSpan {
    /// Span duration in milliseconds (saturating for clock skew).
    pub fn duration_ms(&self) -> u64 {
        self.ended_ms.saturating_sub(self.started_ms)
    }
}

/// One stored span event (upstream `StoredSpan.events[]`).
#[derive(Debug, Clone, PartialEq)]
pub struct StoredSpanEvent {
    pub name: String,
    /// Milliseconds since the span started.
    pub time_ms: u64,
    pub attributes: Attributes,
}

/// One stored span link (upstream `StoredSpan.links[]`).
#[derive(Debug, Clone, PartialEq)]
pub struct StoredSpanLink {
    pub trace_id: String,
    pub span_id: String,
    pub attributes: Attributes,
}

/// All stored spans of one trace, ordered by start time (upstream
/// `StoredTrace`).
#[derive(Debug, Clone, PartialEq)]
pub struct StoredTrace {
    pub trace_id: String,
    pub spans: Vec<StoredSpan>,
}

/// Trace filter (upstream `TraceQuery`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TraceQuery {
    /// Name of the earliest stored span.
    pub name: Option<String>,
    /// Every listed equality must match one span (that span may be anywhere in
    /// the trace).
    pub attributes: Option<Attributes>,
    /// Only traces with at least one error span.
    pub errors: bool,
    /// Inclusive lower bound on the earliest span's start time (epoch ms).
    pub start_min: Option<u64>,
    /// Inclusive upper bound on the earliest span's start time (epoch ms).
    pub start_max: Option<u64>,
    /// Positive page size; `None` uses [`DEFAULT_TRACE_PAGE_SIZE`].
    pub limit: Option<usize>,
    /// Opaque continuation from the previous page; keep the same filters.
    pub cursor: Option<String>,
}

impl TraceQuery {
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn with_name(mut self, name: impl Into<String>) -> Self {
        self.name = Some(name.into());
        self
    }

    #[must_use]
    pub fn with_attributes(mut self, attributes: Attributes) -> Self {
        self.attributes = Some(attributes);
        self
    }

    #[must_use]
    pub fn with_errors(mut self, errors: bool) -> Self {
        self.errors = errors;
        self
    }

    #[must_use]
    pub fn with_start_range(mut self, start_min: Option<u64>, start_max: Option<u64>) -> Self {
        self.start_min = start_min;
        self.start_max = start_max;
        self
    }

    #[must_use]
    pub fn with_limit(mut self, limit: usize) -> Self {
        self.limit = Some(limit);
        self
    }

    #[must_use]
    pub fn with_cursor(mut self, cursor: impl Into<String>) -> Self {
        self.cursor = Some(cursor.into());
        self
    }
}

/// One page of [`TelemetryStore::find_traces`] (upstream
/// `{traces, cursor?}`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TracePage {
    pub traces: Vec<StoredTrace>,
    /// Present when more matching traces remain; pass it back as
    /// [`TraceQuery::cursor`].
    pub cursor: Option<String>,
}

/// The opaque `findTraces` continuation (upstream `JSON.stringify([startedMs,
/// traceId])`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TraceCursor {
    pub started_ms: u64,
    pub trace_id: String,
}

impl TraceCursor {
    pub fn new(started_ms: u64, trace_id: impl Into<String>) -> Self {
        Self {
            started_ms,
            trace_id: trace_id.into(),
        }
    }

    /// Encode as the opaque cursor string.
    pub fn encode(&self) -> String {
        serde_json::to_string(&(self.started_ms, self.trace_id.as_str()))
            .unwrap_or_else(|_| "[]".to_owned())
    }

    /// Decode an opaque cursor string; malformed cursors yield `None` (the
    /// query then starts from the first page instead of failing mid-read).
    pub fn decode(text: &str) -> Option<Self> {
        let (started_ms, trace_id) = serde_json::from_str::<(u64, String)>(text).ok()?;
        Some(Self {
            started_ms,
            trace_id,
        })
    }
}

impl std::fmt::Display for TraceCursor {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.encode())
    }
}

/// Metric filter (upstream `MetricQuery`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct MetricQuery {
    /// Exact instrument name.
    pub name: String,
    /// Inclusive lower bound on the measurement interval start (epoch ms).
    pub start_min: Option<u64>,
    /// Inclusive upper bound on the measurement interval start (epoch ms).
    pub start_max: Option<u64>,
    /// Every listed equality must match the measurement's attributes.
    pub attributes: Option<Attributes>,
}

impl MetricQuery {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            ..Self::default()
        }
    }

    #[must_use]
    pub fn with_start_range(mut self, start_min: Option<u64>, start_max: Option<u64>) -> Self {
        self.start_min = start_min;
        self.start_max = start_max;
        self
    }

    #[must_use]
    pub fn with_attributes(mut self, attributes: Attributes) -> Self {
        self.attributes = Some(attributes);
        self
    }
}

/// Instrument kind carried by every raw measurement (upstream
/// `StoredMetric.kind`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum MetricKind {
    Counter,
    UpDownCounter,
    Histogram,
}

impl MetricKind {
    /// The upstream wire spelling (`"counter"` / `"upDownCounter"` /
    /// `"histogram"`).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Counter => "counter",
            Self::UpDownCounter => "upDownCounter",
            Self::Histogram => "histogram",
        }
    }
}

impl std::fmt::Display for MetricKind {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// One stored raw measurement row (upstream `StoredMetric`): count is always
/// 1 and `sum`/`min`/`max` are the measured value, exactly like
/// `InMemoryTelemetryContext.findMetrics`.
#[derive(Debug, Clone, PartialEq)]
pub struct StoredMetric {
    /// Start of the 10-second aggregation interval (upstream
    /// `Math.floor(Date.now() / 10_000) * 10_000`).
    pub interval_ms: u64,
    pub name: String,
    pub kind: MetricKind,
    pub unit: Option<String>,
    pub attributes: Attributes,
    pub count: u64,
    pub sum: f64,
    pub min: f64,
    pub max: f64,
}

impl StoredMetric {
    /// A count-one row for one raw measurement.
    pub fn measurement(
        interval_ms: u64,
        name: impl Into<String>,
        kind: MetricKind,
        unit: Option<String>,
        attributes: Attributes,
        value: f64,
    ) -> Self {
        Self {
            interval_ms,
            name: name.into(),
            kind,
            unit,
            attributes,
            count: 1,
            sum: value,
            min: value,
            max: value,
        }
    }
}

/// Read settled traces and metrics without depending on a storage backend
/// (upstream `TelemetryStore`).
pub trait TelemetryStore {
    fn find_traces(&self, query: &TraceQuery) -> TracePage;

    fn get_trace(&self, trace_id: &str) -> Option<StoredTrace>;

    fn find_metrics(&self, query: &MetricQuery) -> Vec<StoredMetric>;

    /// `find_traces` with the default query.
    fn find_traces_default(&self) -> TracePage {
        self.find_traces(&TraceQuery::default())
    }
}

/// Backend-neutral in-memory store over already-materialized rows.
///
/// Rows are queried in insertion order with the same filters/pagination the
/// recording store applies; use it to export a trace from any recorder (or to
/// test a host's rendering/export pipeline without a live telemetry scope).
#[derive(Debug, Clone, Default)]
pub struct MemoryTelemetryStore {
    spans: Vec<StoredSpan>,
    metrics: Vec<StoredMetric>,
}

impl MemoryTelemetryStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Build a store from spans only (`find_metrics` then returns empty).
    pub fn from_spans(spans: impl IntoIterator<Item = StoredSpan>) -> Self {
        Self {
            spans: spans.into_iter().collect(),
            metrics: Vec::new(),
        }
    }

    pub fn push_span(&mut self, span: StoredSpan) {
        self.spans.push(span);
    }

    pub fn extend_spans(&mut self, spans: impl IntoIterator<Item = StoredSpan>) {
        self.spans.extend(spans);
    }

    pub fn push_metric(&mut self, metric: StoredMetric) {
        self.metrics.push(metric);
    }

    pub fn extend_metrics(&mut self, metrics: impl IntoIterator<Item = StoredMetric>) {
        self.metrics.extend(metrics);
    }

    /// Detached copies of every stored span, in insertion order.
    pub fn spans(&self) -> Vec<StoredSpan> {
        self.spans.clone()
    }

    /// Detached copies of every stored measurement, in insertion order.
    pub fn metrics(&self) -> Vec<StoredMetric> {
        self.metrics.clone()
    }

    pub fn clear(&mut self) {
        self.spans.clear();
        self.metrics.clear();
    }

    pub fn is_empty(&self) -> bool {
        self.spans.is_empty() && self.metrics.is_empty()
    }
}

impl TelemetryStore for MemoryTelemetryStore {
    fn find_traces(&self, query: &TraceQuery) -> TracePage {
        find_traces_in(&self.spans, query)
    }

    fn get_trace(&self, trace_id: &str) -> Option<StoredTrace> {
        get_trace_in(&self.spans, trace_id)
    }

    fn find_metrics(&self, query: &MetricQuery) -> Vec<StoredMetric> {
        filter_metrics(&self.metrics, query)
    }
}

// ---------------------------------------------------------------------------
// Query implementation (shared by every in-memory store)
// ---------------------------------------------------------------------------

/// Current Unix epoch milliseconds (0 before the epoch).
pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

/// Unix epoch milliseconds for a `SystemTime` (0 before the epoch).
pub fn system_time_ms(time: SystemTime) -> u64 {
    time.duration_since(UNIX_EPOCH)
        .map(|elapsed| u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

/// The measurement interval start for `now` (upstream's 10 s buckets).
pub fn interval_ms_for(now_ms: u64) -> u64 {
    now_ms / 10_000 * 10_000
}

/// Upstream `storedSpans` ordering: `startedMs` ascending, span id breaking
/// ties.
pub fn sort_stored_spans(spans: &mut [StoredSpan]) {
    spans.sort_by(|left, right| {
        left.started_ms
            .cmp(&right.started_ms)
            .then_with(|| left.id.cmp(&right.id))
    });
}

/// Group sorted spans into traces, preserving the per-trace span order.
pub fn collect_traces(spans: &[StoredSpan]) -> Vec<StoredTrace> {
    let mut sorted = spans.to_vec();
    sort_stored_spans(&mut sorted);
    let mut grouped: BTreeMap<String, StoredTrace> = BTreeMap::new();
    for span in sorted {
        grouped
            .entry(span.trace_id.clone())
            .or_insert_with(|| StoredTrace {
                trace_id: span.trace_id.clone(),
                spans: Vec::new(),
            })
            .spans
            .push(span);
    }
    grouped.into_values().collect()
}

/// `findTraces` over a span collection (upstream `storedSpans` +
/// `findTraces`).
pub fn find_traces_in(spans: &[StoredSpan], query: &TraceQuery) -> TracePage {
    let limit = query.limit.unwrap_or(DEFAULT_TRACE_PAGE_SIZE);
    if limit == 0 {
        // Upstream would throw on `traces.at(-1)!`; an empty page is the
        // useful, non-panicking equivalent.
        return TracePage::default();
    }
    let after = query.cursor.as_deref().and_then(TraceCursor::decode);
    let mut traces: Vec<StoredTrace> = collect_traces(spans)
        .into_iter()
        .filter(|trace| {
            let Some(first) = trace.spans.first() else {
                return false;
            };
            if let Some(name) = &query.name {
                if &first.name != name {
                    return false;
                }
            }
            if let Some(start_min) = query.start_min {
                if first.started_ms < start_min {
                    return false;
                }
            }
            if let Some(start_max) = query.start_max {
                if first.started_ms > start_max {
                    return false;
                }
            }
            if let Some(cursor) = &after {
                let past_cursor = first.started_ms < cursor.started_ms
                    || (first.started_ms == cursor.started_ms && trace.trace_id > cursor.trace_id);
                if !past_cursor {
                    return false;
                }
            }
            if query.errors
                && !trace
                    .spans
                    .iter()
                    .any(|span| matches!(span.status, SpanStatus::Error { .. }))
            {
                return false;
            }
            if let Some(filter) = &query.attributes {
                let matched = trace
                    .spans
                    .iter()
                    .any(|span| attributes_match(&span.attributes, filter));
                if !matched {
                    return false;
                }
            }
            true
        })
        .collect();
    // Newest trace first; trace id breaks ties (upstream sort).
    traces.sort_by(|left, right| {
        let left_start = left.spans.first().map_or(0, |span| span.started_ms);
        let right_start = right.spans.first().map_or(0, |span| span.started_ms);
        right_start
            .cmp(&left_start)
            .then_with(|| left.trace_id.cmp(&right.trace_id))
    });
    let has_more = traces.len() > limit;
    traces.truncate(limit);
    let cursor = if has_more {
        traces
            .last()
            .and_then(|trace| trace.spans.first())
            .map(|span| TraceCursor::new(span.started_ms, span.trace_id.clone()).encode())
    } else {
        None
    };
    TracePage { traces, cursor }
}

/// `getTrace` over a span collection: `None` when the trace has no spans.
pub fn get_trace_in(spans: &[StoredSpan], trace_id: &str) -> Option<StoredTrace> {
    let mut matched: Vec<StoredSpan> = spans
        .iter()
        .filter(|span| span.trace_id == trace_id)
        .cloned()
        .collect();
    if matched.is_empty() {
        return None;
    }
    sort_stored_spans(&mut matched);
    Some(StoredTrace {
        trace_id: trace_id.to_owned(),
        spans: matched,
    })
}

/// `findMetrics` over raw measurement rows (upstream `findMetrics`).
pub fn filter_metrics(metrics: &[StoredMetric], query: &MetricQuery) -> Vec<StoredMetric> {
    metrics
        .iter()
        .filter(|metric| {
            metric.name == query.name
                && query
                    .start_min
                    .is_none_or(|start_min| metric.interval_ms >= start_min)
                && query
                    .start_max
                    .is_none_or(|start_max| metric.interval_ms <= start_max)
                && query
                    .attributes
                    .as_ref()
                    .is_none_or(|filter| attributes_match(&metric.attributes, filter))
        })
        .cloned()
        .collect()
}

/// Whether every `filter` attribute equals the corresponding `attributes`
/// value (upstream `Object.entries(query.attributes).every(...)`).
pub fn attributes_match(attributes: &Attributes, filter: &Attributes) -> bool {
    filter
        .iter()
        .all(|(key, value)| attributes.get(key) == Some(value))
}
