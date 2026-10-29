//! Telemetry policy: structured `tracing` spans plus the vendor-neutral
//! callback surface, extended with a portable span/metrics/propagation core
//! ported from upstream `packages/telemetry/src` (1.3.0 window).
//!
//! pir deliberately does NOT depend on OpenTelemetry (nor any telemetry SDK).
//! Its observability layer is built from vendor-neutral pieces:
//!
//! 1. **Structured spans via `tracing`** — the canonical span anchors mirror
//!    upstream `packages/telemetry/src` (`TelemetryContext.startSpan` /
//!    `addEvent` / `setStatus` semantics) without an OTel dependency:
//!
//!    | anchor (pir) | span name | fields |
//!    |---|---|---|
//!    | `ai/provider.rs` `resolve_and_dispatch` | `provider.dispatch` | `provider`, `model`, `api` |
//!    | `agent/agent_loop.rs` `run_with_options` | `agent.run` | `model` |
//!    | `agent/agent_loop.rs` turn loop | `agent.turn` | `turn` |
//!    | `agent/tool_scheduler/execute.rs` `execute_one` | `tool.exec` | `tool`, `call_id` |
//!    | `compaction/mod.rs` `compact_with_preparation` | `compaction.run` | `tokens_before` |
//!
//!    Termination state is recorded as plain events/fields instead of OTel
//!    status: the run summary `tracing::debug!` (`turns`, `aborted`,
//!    `stop_reason`), the compaction completion line (`summary_tokens`,
//!    `first_kept_index`), and `tracing::warn!` with an `error` field inside
//!    the failing span (dispatch failures, tool error results). Streaming
//!    chunks and other hot paths stay span-free. Enable with
//!    `RUST_LOG=debug` (or `trace`) — nothing is emitted by default and no
//!    behavior changes when the subscriber is absent.
//!
//! 2. **Vendor-neutral callback adapter** — hosts attach UI/session telemetry
//!    through the agent loop's event surface ([`crate::agent::agent_loop`]:
//!    `AgentEvent` handlers, `message`/extension emission hooks) instead of a
//!    telemetry SDK, keeping the agent independent of any backend.
//!
//! 3. **Portable core API** (this module's submodules) — the upstream
//!    `@earendil-works/pi-telemetry` surface in Rust:
//!
//!    - [`span`] — [`SpanOptions`]/[`TelemetrySpan`] with name, attributes,
//!      typed [`SpanLink`]s, status, parent [`TraceContext`], events, and an
//!      explicit `end()`; [`NoopSpan`] and the [`TestTelemetry`] recorder.
//!    - [`metrics`] — [`Counter`], [`FloatCounter`] (fractional numeric
//!      counters such as USD costs), [`UpDownCounter`], and [`Histogram`] with
//!      attributes; histogram options carry bucket boundaries and the test
//!      recorder aggregates sum/count/buckets.
//!    - [`propagation`] — W3C `traceparent`/`tracestate` inject/extract over a
//!      [`Carrier`] string map, plus [`NoopPropagator`].
//!    - [`store`] — [`TelemetryStore`] (`find_traces`/`get_trace`/
//!      `find_metrics`) over the portable [`StoredSpan`]/[`StoredTrace`]/
//!      [`StoredMetric`] shapes, with the opaque [`TraceCursor`] pagination;
//!      implemented by the in-memory recorder and by
//!      [`MemoryTelemetryStore`].
//!    - [`render`] — [`render_trace`], the chronological span forest used by
//!      debug/export surfaces (upstream `render.ts`).
//!    - [`facade`] — [`Telemetry`], a cloneable host seam over an optional
//!      [`TelemetryRecorder`]. `Telemetry::noop()` (the `Default`) allocates
//!      nothing on the span/metric hot path and records nothing.
//!
//! Mapping to upstream OTel semantics: `startSpan` → [`Telemetry::start_span`]
//! returning a [`Span`]; `addEvent` → [`TelemetrySpan::add_event`];
//! `setStatus` → [`TelemetrySpan::set_status`]. The install-telemetry
//! attribution headers (`ai/provider.rs`, gated by `PI_TELEMETRY` /
//! `enableInstallTelemetry`) are a separate, provider-side concern and are
//! unrelated to span collection.
//!
//! AI path integration (upstream `packages/ai/src/telemetry.ts`): the
//! request-scoped [`crate::ai::types::StreamOptions::telemetry`] span parents
//! one `pi.ai.request` span per logical model request, with the
//! `pi.ai.first_chunk` / `pi.ai.block` events, the
//! `pi.ai.request.duration` / `pi.ai.first_chunk.duration` histograms, the
//! `pi.ai.tokens` counter, the fractional `pi.ai.cost` USD counter, and W3C
//! propagation onto the provider's request headers (`ai::provider`). Absent
//! telemetry is the noop path: no span, no instrument, no header.

pub mod attributes;
pub mod context;
pub mod facade;
pub mod metrics;
pub mod propagation;
pub mod render;
pub mod span;
pub mod store;
pub mod testing;

pub use attributes::{Attribute, AttributeValue, Attributes};
pub use context::{SpanId, TraceContext, TraceFlags, TraceId, TraceParseError};
pub use facade::{Telemetry, TelemetryRecorder};
pub use metrics::{
    are_valid_boundaries, Counter, CounterSink, FloatCounter, FloatCounterSink, Histogram,
    HistogramOptions, HistogramSink, InstrumentOptions, UpDownCounter, UpDownCounterSink,
    DEFAULT_HISTOGRAM_BOUNDARIES,
};
pub use propagation::{
    Carrier, NoopPropagator, Propagator, W3cPropagator, TRACEPARENT_HEADER, TRACESTATE_HEADER,
};
pub use render::{render_trace, render_trace_lines};
pub use span::{
    NoopSpan, Span, SpanError, SpanEvent, SpanLink, SpanOptions, SpanStatus, TelemetrySpan,
    NOOP_SPAN,
};
pub use store::{
    attributes_match, collect_traces, filter_metrics, find_traces_in, get_trace_in,
    interval_ms_for, now_ms, sort_stored_spans, system_time_ms, MemoryTelemetryStore, MetricKind,
    MetricQuery, StoredMetric, StoredSpan, StoredSpanEvent, StoredSpanLink, StoredTrace,
    TelemetryStore, TraceCursor, TracePage, TraceQuery, DEFAULT_TRACE_PAGE_SIZE,
};
pub use testing::{
    CounterPoint, CounterSnapshot, FloatCounterPoint, FloatCounterSnapshot, HistogramSnapshot,
    MetricPoint, RecordedSpan, TestMetricsSnapshot, TestSpan, TestTelemetry, UpDownCounterPoint,
    UpDownCounterSnapshot,
};

/// The in-memory recorder used as the host-side [`TelemetryStore`] reference
/// implementation (upstream `InMemoryTelemetryContext`). It is the same
/// implementation as the test recorder; the alias names the production role.
///
/// Retention is unbounded by default (the test recorder must see every span).
/// A host that keeps one for the process lifetime should build it with
/// [`TestTelemetry::with_max_spans`], which evicts the oldest spans instead of
/// growing forever.
pub type InMemoryTelemetry = TestTelemetry;
