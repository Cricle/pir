//! Portable telemetry store/export surface (audit §7 #18): `findTraces` /
//! `getTrace` / `findMetrics` over the in-memory recorder and the
//! backend-neutral `MemoryTelemetryStore`, the opaque trace cursor, and
//! `renderTrace`.
//!
//! Upstream reference: `packages/telemetry/src/index.ts` (`TelemetryStore`),
//! `memory.ts` (`InMemoryTelemetryContext.findTraces/getTrace/findMetrics`),
//! `render.ts` (`renderTrace`).

use pir::telemetry::{
    render_trace, Attributes, MemoryTelemetryStore, MetricKind, MetricQuery, SpanError, SpanStatus,
    StoredMetric, StoredSpan, StoredSpanEvent, StoredSpanLink, Telemetry, TelemetrySpan,
    TelemetryStore, TestTelemetry, TraceCursor, TraceQuery,
};

fn attr(key: &'static str, value: impl Into<pir::telemetry::AttributeValue>) -> Attributes {
    Attributes::new().with(key, value)
}

fn span(
    id: &str,
    trace_id: &str,
    parent_id: Option<&str>,
    name: &str,
    started_ms: u64,
    ended_ms: u64,
    attributes: Attributes,
) -> StoredSpan {
    StoredSpan {
        id: id.to_owned(),
        trace_id: trace_id.to_owned(),
        parent_id: parent_id.map(str::to_owned),
        name: name.to_owned(),
        started_ms,
        ended_ms,
        attributes,
        events: Vec::new(),
        links: Vec::new(),
        status: SpanStatus::Ok,
    }
}

#[test]
fn render_trace_draws_the_chronological_forest_with_events_and_links() {
    let mut root = span(
        "a",
        "t1",
        None,
        "agent.run",
        1_000,
        1_250,
        attr("model", "fake-model"),
    );
    root.events.push(StoredSpanEvent {
        name: "pi.ai.first_chunk".into(),
        time_ms: 12,
        attributes: Attributes::new(),
    });
    root.links.push(StoredSpanLink {
        trace_id: "trace-link".into(),
        span_id: "span-link".into(),
        attributes: attr("reason", "retry"),
    });
    let child = StoredSpan {
        status: SpanStatus::Error {
            error: Some(SpanError::new("ToolError", "boom")),
        },
        ..span(
            "b",
            "t1",
            Some("a"),
            "tool.exec",
            1_010,
            1_030,
            attr("tool", "bash"),
        )
    };
    let orphan = span(
        "c",
        "t2",
        Some("missing"),
        "orphan",
        1_200,
        1_205,
        Attributes::new(),
    );

    let rendered = render_trace(&[orphan.clone(), child.clone(), root.clone()]);
    assert_eq!(
        rendered,
        "agent.run model=\"fake-model\" (250ms)\n  · pi.ai.first_chunk (+12ms)\n  → link trace trace-link span span-link reason=\"retry\"\n  tool.exec tool=\"bash\" (20ms) error ToolError: boom\norphan (5ms)"
    );

    // A span whose parent is unknown to the collection is a root.
    let only_child = render_trace(&[child]);
    assert!(only_child.starts_with("tool.exec tool=\"bash\" (20ms) error"));
}

#[test]
fn memory_store_filters_traces_like_upstream_and_pages_with_a_cursor() {
    let mut store = MemoryTelemetryStore::new();
    // Older trace: root + an attributed child.
    store.push_span(span(
        "root1",
        "t1",
        None,
        "agent.run",
        1_000,
        1_100,
        attr("provider", "fake"),
    ));
    store.push_span(span(
        "child1",
        "t1",
        Some("root1"),
        "tool.exec",
        1_005,
        1_010,
        attr("tool", "bash"),
    ));
    // Newer trace with an error span.
    store.push_span(span(
        "root2",
        "t2",
        None,
        "agent.run",
        2_000,
        2_100,
        attr("provider", "fake"),
    ));
    store.push_span(StoredSpan {
        status: SpanStatus::Error {
            error: Some(SpanError::message("failed")),
        },
        ..span(
            "child2",
            "t2",
            Some("root2"),
            "tool.exec",
            2_010,
            2_020,
            Attributes::new(),
        )
    });
    // Unrelated newest trace.
    store.push_span(span(
        "root3",
        "t3",
        None,
        "other.op",
        3_000,
        3_100,
        Attributes::new(),
    ));

    let all = store.find_traces(&TraceQuery::default());
    let ids: Vec<&str> = all
        .traces
        .iter()
        .map(|trace| trace.trace_id.as_str())
        .collect();
    assert_eq!(ids, ["t3", "t2", "t1"], "newest trace first");
    assert!(all.cursor.is_none());

    let named: Vec<String> = store
        .find_traces(&TraceQuery::new().with_name("agent.run"))
        .traces
        .into_iter()
        .map(|trace| trace.trace_id)
        .collect();
    assert_eq!(named, ["t2", "t1"]);

    let errored = store.find_traces(&TraceQuery::new().with_errors(true));
    assert_eq!(
        errored
            .traces
            .iter()
            .map(|trace| trace.trace_id.as_str())
            .collect::<Vec<_>>(),
        ["t2"]
    );

    // Attribute equality may match ANY span of the trace (the child here).
    let attributed = store.find_traces(&TraceQuery::new().with_attributes(attr("tool", "bash")));
    assert_eq!(
        attributed
            .traces
            .iter()
            .map(|trace| trace.trace_id.as_str())
            .collect::<Vec<_>>(),
        ["t1"]
    );

    // Inclusive bounds apply to the earliest span's start time.
    let bounded = store.find_traces(&TraceQuery::new().with_start_range(Some(1_500), Some(2_500)));
    assert_eq!(bounded.traces.len(), 1);
    assert_eq!(bounded.traces[0].trace_id, "t2");

    // Pagination: one trace per page, the opaque cursor continues the walk.
    let first = store.find_traces(&TraceQuery::new().with_limit(1));
    assert_eq!(first.traces.len(), 1);
    assert_eq!(first.traces[0].trace_id, "t3");
    let cursor = first.cursor.expect("a cursor while more traces remain");
    let decoded = TraceCursor::decode(&cursor).expect("cursor decodes");
    assert_eq!(decoded.trace_id, "t3");
    assert_eq!(decoded.started_ms, 3_000);

    let second = store.find_traces(&TraceQuery::new().with_limit(1).with_cursor(cursor));
    assert_eq!(second.traces[0].trace_id, "t2");
    let second_cursor = second.cursor.expect("still more");
    let third = store.find_traces(&TraceQuery::new().with_limit(1).with_cursor(second_cursor));
    assert_eq!(third.traces[0].trace_id, "t1");
    assert!(third.cursor.is_none());

    // A malformed cursor starts from the first page instead of failing.
    let reset = store.find_traces(&TraceQuery::new().with_limit(1).with_cursor("not-json"));
    assert_eq!(reset.traces[0].trace_id, "t3");

    // limit 0 is an empty page, not a panic.
    let empty = store.find_traces(&TraceQuery::new().with_limit(0));
    assert!(empty.traces.is_empty());
    assert!(empty.cursor.is_none());
}

#[test]
fn memory_store_get_trace_orders_spans_and_returns_none_for_unknown() {
    let mut store = MemoryTelemetryStore::new();
    store.push_span(span(
        "later",
        "t1",
        None,
        "child",
        1_100,
        1_120,
        Attributes::new(),
    ));
    store.push_span(span(
        "earlier",
        "t1",
        None,
        "root",
        1_000,
        1_010,
        Attributes::new(),
    ));

    let trace = store.get_trace("t1").expect("stored trace");
    assert_eq!(trace.trace_id, "t1");
    assert_eq!(trace.spans.len(), 2);
    assert_eq!(trace.spans[0].id, "earlier");
    assert_eq!(trace.spans[1].id, "later");
    assert_eq!(trace.spans[0].duration_ms(), 10);

    assert!(store.get_trace("missing").is_none());
}

#[test]
fn memory_store_find_metrics_filters_by_name_time_and_attributes() {
    let mut store = MemoryTelemetryStore::new();
    let cost = |interval_ms, value, provider: &str| {
        StoredMetric::measurement(
            interval_ms,
            "pi.ai.cost",
            MetricKind::Counter,
            Some("USD".into()),
            Attributes::new()
                .with("provider", provider.to_owned())
                .with("model", "fake-model"),
            value,
        )
    };
    store.push_metric(cost(10_000, 0.25, "fake"));
    store.push_metric(cost(20_000, 0.5, "other"));
    store.push_metric(StoredMetric::measurement(
        10_000,
        "pi.ai.tokens",
        MetricKind::Counter,
        Some("token".into()),
        Attributes::new(),
        11.0,
    ));

    let named = store.find_metrics(&MetricQuery::new("pi.ai.cost"));
    assert_eq!(named.len(), 2);
    assert_eq!(named[0].count, 1);
    assert_eq!(named[0].sum, 0.25);
    assert_eq!(named[0].min, 0.25);
    assert_eq!(named[0].max, 0.25);
    assert_eq!(named[0].kind, MetricKind::Counter);
    assert_eq!(named[0].kind.as_str(), "counter");
    assert_eq!(named[0].unit.as_deref(), Some("USD"));

    let filtered = store.find_metrics(
        &MetricQuery::new("pi.ai.cost")
            .with_attributes(Attributes::new().with("provider", "other"))
            .with_start_range(Some(15_000), None),
    );
    assert_eq!(filtered.len(), 1);
    assert_eq!(filtered[0].sum, 0.5);

    assert!(store
        .find_metrics(&MetricQuery::new("pi.ai.cost").with_start_range(None, Some(5_000)))
        .is_empty());
    assert!(store.find_metrics(&MetricQuery::new("unknown")).is_empty());
}

#[test]
fn test_telemetry_store_exports_settled_spans_and_raw_measurements() {
    let recorder = TestTelemetry::new();
    let telemetry = Telemetry::from_recorder(recorder.clone());

    let root = telemetry.start_span_named("agent.run");
    root.set_attributes(&attr("provider", "fake"));
    // Give the two spans distinct millisecond start times: the portable
    // export truncates SystemTime to whole milliseconds, and the earliest
    // span decides the trace's name/bounds filters.
    std::thread::sleep(std::time::Duration::from_millis(2));
    let child = telemetry.start_child(&root, {
        let mut options = pir::telemetry::SpanOptions::new("tool.exec");
        options.attributes = attr("tool", "bash");
        options
    });
    child.add_event("pi.ai.block", &attr("type", "tool_call"));
    child.set_status(&SpanStatus::error("tool failed"));
    child.end();
    root.end();

    let stored_root = recorder.span("agent.run").expect("recorded root");
    assert!(recorder.span("tool.exec").is_some());

    let traces = recorder.find_traces(&TraceQuery::default());
    assert_eq!(traces.traces.len(), 1, "one trace for root+child");
    assert_eq!(traces.traces[0].trace_id, stored_root.trace_id.to_string());
    assert_eq!(traces.traces[0].spans.len(), 2);

    let trace = recorder
        .get_trace(&stored_root.trace_id.to_string())
        .expect("trace by id");
    let child_span = trace
        .spans
        .iter()
        .find(|span| span.name == "tool.exec")
        .expect("child span exported");
    assert_eq!(
        child_span.parent_id.as_deref(),
        Some(stored_root.span_id.to_string().as_str())
    );
    assert_eq!(child_span.events.len(), 1);
    assert_eq!(child_span.events[0].name, "pi.ai.block");
    assert!(matches!(child_span.status, SpanStatus::Error { .. }));
    assert_eq!(
        child_span.attributes.get("tool"),
        Some(&pir::telemetry::AttributeValue::String("bash".into()))
    );

    // Attribute filters match the child; name filters match the earliest
    // span's name.
    assert_eq!(
        recorder
            .find_traces(&TraceQuery::new().with_attributes(attr("tool", "bash")))
            .traces
            .len(),
        1
    );
    assert_eq!(
        recorder
            .find_traces(&TraceQuery::new().with_name("tool.exec"))
            .traces
            .len(),
        0
    );
    assert_eq!(
        recorder
            .find_traces(&TraceQuery::new().with_name("agent.run"))
            .traces
            .len(),
        1
    );
    assert_eq!(
        recorder
            .find_traces(&TraceQuery::new().with_errors(true))
            .traces
            .len(),
        1
    );

    // In-flight spans are not exported until they end (upstream `settled`).
    let running = telemetry.start_span_named("still.running");
    assert!(recorder
        .stored_spans()
        .iter()
        .all(|span| span.name != "still.running"));
    running.end();
    assert!(recorder
        .stored_spans()
        .iter()
        .any(|span| span.name == "still.running"));

    // Fractional counters and raw measurement rows: `pi.ai.cost` is a USD
    // counter, not an integer one, so 0.0123 must survive exactly.
    telemetry
        .counter("pi.ai.tokens", &pir::telemetry::InstrumentOptions::new())
        .add(11, &attr("kind", "input"));
    telemetry
        .float_counter(
            "pi.ai.cost",
            &pir::telemetry::InstrumentOptions::new().with_unit("USD"),
        )
        .add(0.0123, &attr("provider", "fake"));

    let cost = recorder.find_metrics(&MetricQuery::new("pi.ai.cost"));
    assert_eq!(cost.len(), 1);
    assert!((cost[0].sum - 0.0123).abs() < f64::EPSILON);
    assert_eq!(cost[0].unit.as_deref(), Some("USD"));
    assert_eq!(cost[0].kind, MetricKind::Counter);

    let snapshot = recorder
        .float_counter_snapshot("pi.ai.cost")
        .expect("fractional counter snapshot");
    assert!((snapshot.total - 0.0123).abs() < f64::EPSILON);
    assert_eq!(snapshot.unit.as_deref(), Some("USD"));

    let tokens = recorder.find_metrics(&MetricQuery::new("pi.ai.tokens"));
    assert_eq!(tokens.len(), 1);
    assert_eq!(tokens[0].sum, 11.0);

    // The same store renders the exported trace as a forest.
    let rendered = render_trace(&trace.spans);
    assert!(
        rendered.starts_with("agent.run provider=\"fake\""),
        "{rendered}"
    );
    assert!(
        rendered.contains("\n  tool.exec tool=\"bash\""),
        "{rendered}"
    );
    assert!(rendered.contains(" error "), "{rendered}");
    assert!(rendered.contains("pi.ai.block"), "{rendered}");

    // clear() drops spans and measurements together.
    recorder.clear();
    assert!(recorder.stored_spans().is_empty());
    assert!(recorder.measurements().is_empty());
    assert!(recorder
        .find_traces(&TraceQuery::default())
        .traces
        .is_empty());
}

#[test]
fn trace_cursor_roundtrips_and_rejects_garbage() {
    let cursor = TraceCursor::new(1_725_000_000_123, "4bf92f3577b34da6a3ce929d0e0e4736");
    let encoded = cursor.encode();
    assert_eq!(
        encoded,
        "[1725000000123,\"4bf92f3577b34da6a3ce929d0e0e4736\"]"
    );
    assert_eq!(TraceCursor::decode(&encoded), Some(cursor));
    assert_eq!(TraceCursor::decode("[]"), None);
    assert_eq!(TraceCursor::decode("{\"started_ms\":1}"), None);
    assert_eq!(TraceCursor::decode("[1]"), None);
}
