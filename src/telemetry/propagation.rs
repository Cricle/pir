//! Trace-context propagation across process boundaries.
//!
//! The wire format is W3C Trace Context: a lowercase `traceparent`
//! (`version-trace_id-span_id-flags`) plus an optional `tracestate`. The
//! carrier is a plain string map, so hosts can copy it onto HTTP headers,
//! message metadata, or an IPC envelope without a telemetry dependency.

use std::collections::BTreeMap;

use crate::telemetry::context::{is_lowercase_hex, SpanId, TraceContext, TraceFlags, TraceId};

/// String carrier map (upstream `TelemetryCarrier`).
pub type Carrier = BTreeMap<String, String>;

pub const TRACEPARENT_HEADER: &str = "traceparent";
pub const TRACESTATE_HEADER: &str = "tracestate";

/// Inject/extract capability. Implementations must be passive: malformed input
/// yields `None`, never a panic.
pub trait Propagator: Send + Sync {
    /// Write `context` into `carrier`. Returns `false` when the context is
    /// invalid or the propagator is disabled.
    fn inject(&self, context: &TraceContext, carrier: &mut Carrier) -> bool;

    /// Read a remote parent context. Returns `None` when the carrier has no
    /// valid `traceparent`.
    fn extract(&self, carrier: &Carrier) -> Option<TraceContext>;
}

/// W3C Trace Context propagator (version `00`, lowercase hex).
#[derive(Debug, Clone, Copy, Default)]
pub struct W3cPropagator;

/// Disabled propagator: writes nothing and extracts nothing.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoopPropagator;

/// Render and insert `traceparent` (and valid `tracestate`). Invalid contexts
/// are ignored.
pub fn inject(context: &TraceContext, carrier: &mut Carrier) -> bool {
    if !context.is_valid() {
        return false;
    }
    let traceparent = format!(
        "00-{}-{}-{:02x}",
        context.trace_id(),
        context.span_id(),
        context.trace_flags().bits()
    );
    carrier.insert(TRACEPARENT_HEADER.to_owned(), traceparent);
    if let Some(trace_state) = context.trace_state() {
        if is_valid_trace_state(trace_state) {
            carrier.insert(TRACESTATE_HEADER.to_owned(), trace_state.to_owned());
        }
    }
    true
}

/// Parse a W3C `traceparent`; malformed values return `None`. A malformed
/// `tracestate` is discarded while the valid `traceparent` is still honored.
pub fn extract(carrier: &Carrier) -> Option<TraceContext> {
    let traceparent = carrier.get(TRACEPARENT_HEADER)?;
    let mut parts = traceparent.split('-');

    let version = parts.next()?;
    // version 00 is the only format without extra trailing fields; `ff` is
    // explicitly forbidden by the spec.
    if version != "00" {
        return None;
    }
    let trace_id = TraceId::from_hex(parts.next()?)?;
    let span_id = SpanId::from_hex(parts.next()?)?;
    let flags = parts.next()?;
    if parts.next().is_some() {
        return None;
    }
    if !trace_id.is_valid() || !span_id.is_valid() {
        return None;
    }
    if flags.len() != 2 || !is_lowercase_hex(flags) {
        return None;
    }
    let flags = u8::from_str_radix(flags, 16).ok()?;

    let mut context =
        TraceContext::new(trace_id, span_id, TraceFlags::new(flags)).with_remote(true);
    if let Some(trace_state) = carrier.get(TRACESTATE_HEADER) {
        if is_valid_trace_state(trace_state) {
            context = context.with_trace_state(trace_state.trim());
        }
    }
    Some(context)
}

impl Propagator for W3cPropagator {
    fn inject(&self, context: &TraceContext, carrier: &mut Carrier) -> bool {
        inject(context, carrier)
    }

    fn extract(&self, carrier: &Carrier) -> Option<TraceContext> {
        extract(carrier)
    }
}

impl Propagator for NoopPropagator {
    fn inject(&self, _context: &TraceContext, _carrier: &mut Carrier) -> bool {
        false
    }

    fn extract(&self, _carrier: &Carrier) -> Option<TraceContext> {
        None
    }
}

/// Minimal W3C `tracestate` syntax check: comma-separated `key=value` members,
/// non-empty keys/values, no control characters. Values may contain spaces but
/// not `,` or `=`.
fn is_valid_trace_state(trace_state: &str) -> bool {
    let trace_state = trace_state.trim();
    if trace_state.is_empty() {
        return false;
    }
    trace_state.split(',').all(|member| {
        let member = member.trim();
        let Some((key, value)) = member.split_once('=') else {
            return false;
        };
        if key.is_empty() || value.is_empty() {
            return false;
        }
        key.bytes().all(|byte| {
            byte.is_ascii_lowercase()
                || byte.is_ascii_digit()
                || matches!(byte, b'_' | b'-' | b'*' | b'/')
        }) && value
            .bytes()
            .all(|byte| (0x20..=0x7e).contains(&byte) && byte != b',' && byte != b'=')
    })
}
