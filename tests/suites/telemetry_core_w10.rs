//! Telemetry core (W10-H): span links, nested parentage, metric aggregation,
//! W3C propagation, and the disabled/noop path.
//!
//! Ported surface: upstream `@earendil-works/pi-telemetry` `index.ts` /
//! `metrics.ts` / `memory.ts` / `noop.ts` from the pi 1.3.0 window. These tests
//! use only the public `pir::telemetry` API and the in-memory test recorder.

use pir::telemetry::{
    AttributeValue, Attributes, Carrier, HistogramOptions, InstrumentOptions, Propagator,
    SpanError, SpanLink, SpanOptions, SpanStatus, Telemetry, TelemetrySpan, TestTelemetry,
    TraceContext, W3cPropagator, NOOP_SPAN, TRACEPARENT_HEADER, TRACESTATE_HEADER,
};

fn test_telemetry() -> (TestTelemetry, Telemetry) {
    let recorder = TestTelemetry::new();
    let telemetry = Telemetry::from_recorder(recorder.clone());
    (recorder, telemetry)
}

#[test]
fn span_links_are_recorded_with_attributes() {
    let (recorder, telemetry) = test_telemetry();

    let producer = telemetry.start_span_named("producer");
    producer.set_attributes(&Attributes::new().with("kind", "source"));
    producer.end();
    let producer_context = producer.trace_context().clone();
    assert!(producer_context.is_valid());

    let linked = telemetry.start_span(
        SpanOptions::new("linked")
            .with_attribute("ordinal", 1i64)
            .with_link(
                SpanLink::new(producer_context.clone())
                    .with_attribute("reason", "initial")
                    .with_attribute("sequence", 1i64),
            ),
    );
    linked.add_link(
        &SpanLink::new(producer_context.clone())
            .with_attribute("reason", "late")
            .with_attribute("sequence", 2i64),
    );
    // A link to a disabled span has no identity and is ignored.
    linked.add_link(&SpanLink::new(NOOP_SPAN.trace_context().clone()));
    linked.add_event("linked-event", &Attributes::new().with("phase", "mid"));
    linked.set_status(&SpanStatus::Error {
        error: Some(SpanError::new("LinkError", "late failure")),
    });
    assert!(linked.is_recording());
    linked.end();
    assert!(!linked.is_recording());
    linked.end(); // idempotent

    let recorded = recorder.span("linked").expect("linked span captured");
    assert_eq!(recorded.links.len(), 2);
    assert_eq!(recorded.links[0].context, producer_context);
    assert_eq!(
        recorded.links[0].attributes.get("reason"),
        Some(&AttributeValue::String("initial".into()))
    );
    assert_eq!(
        recorded.links[1].attributes.get("sequence"),
        Some(&AttributeValue::Int(2))
    );
    assert_eq!(
        recorded.attributes.get("ordinal"),
        Some(&AttributeValue::Int(1))
    );
    assert_eq!(recorded.events.len(), 1);
    assert_eq!(recorded.events[0].name, "linked-event");
    assert_eq!(
        recorded.status,
        SpanStatus::Error {
            error: Some(SpanError::new("LinkError", "late failure"))
        }
    );
    assert!(recorded.ended);
    assert!(recorded.ended_at.is_some());
    assert!(recorded.parent_span_id.is_none());

    let producer_record = recorder.span("producer").expect("producer captured");
    assert_eq!(
        producer_record.attributes.get("kind"),
        Some(&AttributeValue::String("source".into()))
    );
}

#[test]
fn nested_spans_record_parent_ids_and_settle_order() {
    let (recorder, telemetry) = test_telemetry();

    let root = telemetry.start_span_named("root");
    let child = telemetry.start_child(&root, SpanOptions::new("child"));
    // A span handle can start its own children (upstream span-as-context).
    let grandchild = child.start_child(SpanOptions::new("grandchild"));

    assert!(root.trace_context().is_valid());
    assert_eq!(
        child.trace_context().trace_id(),
        root.trace_context().trace_id()
    );
    assert_eq!(
        grandchild.trace_context().trace_id(),
        root.trace_context().trace_id()
    );

    // Cloned handles share the same span; ending one ends them all.
    let root_handle = root.clone();
    assert!(root_handle.is_recording());

    grandchild.end();
    child.end();
    root.end();
    assert!(!root_handle.is_recording());
    assert_eq!(root_handle.trace_context(), root.trace_context());

    let spans = recorder.spans();
    assert_eq!(spans.len(), 3);
    let root_record = &spans[0];
    let child_record = &spans[1];
    let grandchild_record = &spans[2];

    assert_eq!(root_record.name, "root");
    assert!(root_record.parent_trace_id.is_none());
    assert!(root_record.parent_span_id.is_none());

    assert_eq!(child_record.parent_trace_id, Some(root_record.trace_id));
    assert_eq!(child_record.parent_span_id, Some(root_record.span_id));
    assert_eq!(
        grandchild_record.parent_trace_id,
        Some(child_record.trace_id)
    );
    assert_eq!(grandchild_record.parent_span_id, Some(child_record.span_id));

    assert_eq!(root_record.trace_id, child_record.trace_id);
    assert_eq!(child_record.trace_id, grandchild_record.trace_id);

    // Children settle before their parent (upstream `endSequence` ordering).
    let grandchild_end = grandchild_record.end_sequence.expect("grandchild ended");
    let child_end = child_record.end_sequence.expect("child ended");
    let root_end = root_record.end_sequence.expect("root ended");
    assert!(grandchild_end < child_end);
    assert!(child_end < root_end);
}

#[test]
fn metrics_aggregate_with_attributes() {
    let (recorder, telemetry) = test_telemetry();

    let requests = telemetry.counter(
        "requests",
        &InstrumentOptions::new()
            .with_description("Requests served")
            .with_unit("1"),
    );
    let ok = Attributes::new().with("status", "ok");
    let error = Attributes::new().with("status", "error");
    requests.add(2, &ok);
    requests.add(3, &ok);
    requests.add(4, &error);

    let counter = recorder
        .counter_snapshot("requests")
        .expect("counter captured");
    assert_eq!(counter.total, 9);
    assert_eq!(counter.description.as_deref(), Some("Requests served"));
    assert_eq!(counter.unit.as_deref(), Some("1"));
    assert_eq!(counter.points.len(), 2);
    let ok_point = counter
        .points
        .iter()
        .find(|point| point.attributes == ok)
        .expect("ok attribute set aggregated");
    assert_eq!(ok_point.value, 5);

    let active = telemetry.up_down_counter("active", &InstrumentOptions::new());
    active.add(4, &Attributes::EMPTY);
    active.add(-2, &Attributes::EMPTY);
    let up_down = recorder
        .up_down_counter_snapshot("active")
        .expect("up/down counter captured");
    assert_eq!(up_down.total, 2);
    assert_eq!(up_down.points.len(), 1);
    assert_eq!(up_down.points[0].value, 2);

    let histogram = telemetry.histogram(
        "duration",
        &HistogramOptions::new()
            .with_unit("ms")
            .with_boundaries(vec![10.0, 100.0]),
    );
    assert_eq!(histogram.boundaries(), &[10.0, 100.0]);
    for value in [5.0, 50.0, 500.0, 50.0] {
        histogram.record(value, &Attributes::EMPTY);
    }
    // Non-finite samples are ignored, matching the upstream adapter contract.
    histogram.record(f64::NAN, &Attributes::EMPTY);
    histogram.record(f64::INFINITY, &Attributes::EMPTY);

    let histogram_snapshot = recorder
        .histogram_snapshot("duration")
        .expect("histogram captured");
    assert_eq!(histogram_snapshot.boundaries, vec![10.0, 100.0]);
    assert_eq!(histogram_snapshot.count, 4);
    assert_eq!(histogram_snapshot.sum, 605.0);
    // Cumulative buckets: <=10, <=100, +Inf.
    assert_eq!(histogram_snapshot.bucket_counts, vec![1, 3, 4]);
    assert_eq!(histogram_snapshot.points.len(), 4);

    // Invalid (non-ascending) boundaries degrade to a disabled instrument.
    let invalid = telemetry.histogram(
        "invalid",
        &HistogramOptions::new().with_boundaries(vec![2.0, 1.0]),
    );
    assert!(!invalid.is_enabled());
    assert!(recorder.histogram_snapshot("invalid").is_none());

    let snapshot = recorder.metrics_snapshot();
    assert_eq!(snapshot.counters.len(), 1);
    assert_eq!(snapshot.up_down_counters.len(), 1);
    assert_eq!(snapshot.histograms.len(), 1);
}

#[test]
fn propagation_roundtrip_and_malformed_traceparent_rejection() {
    let (recorder, telemetry) = test_telemetry();

    let sender = telemetry.start_span_named("sender");
    let sender_context = sender.trace_context().clone();
    let carrier = telemetry
        .inject_carrier(&sender_context)
        .expect("enabled telemetry injects a valid context");

    let traceparent = carrier
        .get(TRACEPARENT_HEADER)
        .expect("traceparent header present");
    assert_eq!(
        traceparent,
        &format!(
            "00-{}-{}-01",
            sender_context.trace_id(),
            sender_context.span_id()
        )
    );

    let remote_parent = telemetry
        .extract(&carrier)
        .expect("valid traceparent extracts");
    assert!(remote_parent.is_remote());
    assert!(remote_parent.is_sampled());
    assert_eq!(remote_parent.trace_id(), sender_context.trace_id());
    assert_eq!(remote_parent.span_id(), sender_context.span_id());

    let receiver = telemetry.start_span(SpanOptions::new("receiver").with_parent(remote_parent));
    assert_eq!(
        receiver.trace_context().trace_id(),
        sender_context.trace_id()
    );
    receiver.end();
    sender.end();
    let receiver_record = recorder.span("receiver").expect("receiver captured");
    assert_eq!(
        receiver_record.parent_span_id,
        Some(sender_context.span_id())
    );

    // tracestate survives a full inject/extract round-trip.
    let with_state = sender_context
        .clone()
        .with_trace_state("vendor=value,other=42");
    let mut state_carrier = Carrier::new();
    assert!(W3cPropagator.inject(&with_state, &mut state_carrier));
    assert_eq!(
        state_carrier.get(TRACESTATE_HEADER).map(String::as_str),
        Some("vendor=value,other=42")
    );
    let extracted_state = W3cPropagator
        .extract(&state_carrier)
        .expect("traceparent with tracestate extracts");
    assert_eq!(extracted_state.trace_state(), Some("vendor=value,other=42"));

    // Malformed traceparent values are rejected.
    let trace_id = sender_context.trace_id().to_string();
    let span_id = sender_context.span_id().to_string();
    let malformed = [
        format!("01-{trace_id}-{span_id}-01"), // unsupported version
        format!("ff-{trace_id}-{span_id}-01"), // forbidden version
        format!("00-{trace_id}-{span_id}"),    // missing flags
        format!("00-{trace_id}-{span_id}-01-extra"), // trailing field
        format!("00-{}-{span_id}-01", "0".repeat(32)), // zero trace id
        format!("00-{trace_id}-{}-01", "0".repeat(16)), // zero span id
        format!("00-ABCDEF0123456789ABCDEF0123456789-{span_id}-01"), // uppercase
        format!("00-{trace_id}-{span_id}-0"),  // short flags
        format!("00-{trace_id}-{span_id}-gg"), // non-hex flags
        format!("00-{trace_id}-{span_id}-01\n"), // trailing newline
        "garbage".to_owned(),
    ];
    for value in malformed {
        let mut carrier = Carrier::new();
        carrier.insert(TRACEPARENT_HEADER.to_owned(), value.clone());
        assert!(
            W3cPropagator.extract(&carrier).is_none(),
            "accepted malformed traceparent: {value:?}"
        );
        assert!(
            telemetry.extract(&carrier).is_none(),
            "facade accepted malformed traceparent: {value:?}"
        );
    }

    assert!(W3cPropagator.extract(&Carrier::new()).is_none());
    let mut invalid_carrier = Carrier::new();
    assert!(!telemetry.inject(&TraceContext::invalid(), &mut invalid_carrier));
    assert!(invalid_carrier.is_empty());
}

#[test]
fn noop_path_allocates_nothing_and_counts_nothing() {
    let telemetry = Telemetry::noop();
    assert!(!telemetry.is_enabled());
    assert!(!Telemetry::default().is_enabled());

    let span = telemetry.start_span(
        SpanOptions::new("noop")
            .with_attribute("key", "value")
            .with_link(SpanLink::new(TraceContext::invalid())),
    );
    // A disabled span holds no inner box and no identity.
    assert!(span.is_noop());
    assert!(!span.is_recording());
    assert_eq!(span.name(), "");
    assert!(!span.trace_context().is_valid());

    span.set_attributes(&Attributes::new().with("late", 1i64));
    span.set_attribute("late", 2.into());
    span.add_event("event", &Attributes::EMPTY);
    span.add_link(&SpanLink::new(TraceContext::invalid()));
    span.set_status(&SpanStatus::error("ignored"));
    span.end();
    assert!(!span.is_recording());

    // Sink-less instruments: `add`/`record` return before reading attributes.
    let counter = telemetry.counter("noop.counter", &InstrumentOptions::new());
    let up_down = telemetry.up_down_counter("noop.active", &InstrumentOptions::new());
    let histogram = telemetry.histogram("noop.duration", &HistogramOptions::new());
    assert!(!counter.is_enabled());
    assert!(!up_down.is_enabled());
    assert!(!histogram.is_enabled());
    counter.add(u64::MAX, &Attributes::new().with("ignored", "yes"));
    up_down.add(i64::MIN, &Attributes::new());
    histogram.record(f64::NAN, &Attributes::new());

    assert!(!NOOP_SPAN.is_recording());
    assert_eq!(NOOP_SPAN.name(), "");
    assert!(!NOOP_SPAN.trace_context().is_valid());

    let mut carrier = Carrier::new();
    assert!(!telemetry.inject(span.trace_context(), &mut carrier));
    assert!(carrier.is_empty());
    assert!(telemetry.extract(&Carrier::new()).is_none());
    assert!(telemetry.inject_carrier(span.trace_context()).is_none());
}

/// The in-memory recorder's optional ring cap: a long-lived host store built
/// with `with_max_spans` keeps the newest N spans instead of growing without
/// bound; an evicted span handle simply stops recording. The unbounded test
/// default is unchanged.
#[test]
fn in_memory_recorder_caps_retained_spans() {
    let recorder = TestTelemetry::with_max_spans(3);
    let telemetry = Telemetry::from_recorder(recorder.clone());
    for index in 0..10 {
        let span = telemetry.start_span_named(format!("s{index}"));
        span.end();
    }
    let spans = recorder.spans();
    assert_eq!(spans.len(), 3, "the cap retains exactly the newest spans");
    assert_eq!(
        spans
            .iter()
            .map(|span| span.name.as_str())
            .collect::<Vec<_>>(),
        vec!["s7", "s8", "s9"],
        "the oldest spans are evicted first"
    );
    assert!(recorder.span("s0").is_none());
    assert!(recorder.span("s9").is_some());

    // A handle whose span was evicted is inert, not a panic or a wrong span.
    let evicted = telemetry.start_span_named("s10");
    let later = telemetry.start_span_named("s11");
    later.end();
    let _ = telemetry.start_span_named("s12");
    evicted.end();
    assert!(!evicted.is_recording());
    assert_eq!(recorder.spans().len(), 3);

    // `TestTelemetry::new()` stays unbounded (the test recorder contract).
    let unbounded = TestTelemetry::new();
    let telemetry = Telemetry::from_recorder(unbounded.clone());
    for index in 0..5 {
        telemetry.start_span_named(format!("t{index}")).end();
    }
    assert_eq!(unbounded.spans().len(), 5);
}
