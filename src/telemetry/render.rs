//! Trace rendering for humans and debug logs.
//!
//! Mirrors upstream `packages/telemetry/src/render.ts` (`renderTrace`):
//! stored spans are rendered as a chronological forest — spans whose parent is
//! absent (or unknown to this collection) become roots — with two-space
//! indentation, `key=JSON` attributes, `(Nms)` durations, error suffixes, and
//! one line per event (`·`, `+Nms`) and link (`→ link trace … span …`).

use crate::telemetry::attributes::{AttributeValue, Attributes};
use crate::telemetry::span::SpanStatus;
use crate::telemetry::store::StoredSpan;

/// Render stored spans as a chronological forest (upstream `renderTrace`).
pub fn render_trace(spans: &[StoredSpan]) -> String {
    render_trace_lines(spans).join("\n")
}

/// [`render_trace`] as individual lines (useful for embedding in a log dump).
pub fn render_trace_lines(spans: &[StoredSpan]) -> Vec<String> {
    let mut ordered: Vec<&StoredSpan> = spans.iter().collect();
    // Chronological order, with a stable tie-break on the span id so two
    // spans started in the same millisecond keep a deterministic shape.
    ordered.sort_by(|left, right| {
        left.started_ms
            .cmp(&right.started_ms)
            .then_with(|| left.id.cmp(&right.id))
    });

    let ids: std::collections::BTreeSet<&str> =
        ordered.iter().map(|span| span.id.as_str()).collect();
    let mut roots: Vec<&StoredSpan> = Vec::new();
    let mut children: std::collections::BTreeMap<&str, Vec<&StoredSpan>> =
        std::collections::BTreeMap::new();
    for span in &ordered {
        match span.parent_id.as_deref() {
            Some(parent) if ids.contains(parent) => {
                children.entry(parent).or_default().push(span);
            }
            _ => roots.push(span),
        }
    }

    let mut lines = Vec::new();
    // Depth-first, children in chronological order: upstream pushes children
    // in reverse onto a stack so the first child pops first.
    let mut pending: Vec<(&StoredSpan, usize)> =
        roots.iter().rev().map(|span| (*span, 0)).collect();
    while let Some((span, depth)) = pending.pop() {
        let indent = "  ".repeat(depth);
        let mut error = String::new();
        if let SpanStatus::Error { error: detail } = &span.status {
            error.push_str(" error");
            if let Some(detail) = detail {
                error.push(' ');
                error.push_str(&detail.name);
                error.push_str(": ");
                error.push_str(&detail.message);
            }
        }
        lines.push(format!(
            "{indent}{}{} ({}ms){error}",
            span.name,
            format_attributes(&span.attributes),
            span.duration_ms()
        ));
        for event in &span.events {
            lines.push(format!(
                "{indent}  · {}{} (+{}ms)",
                event.name,
                format_attributes(&event.attributes),
                event.time_ms
            ));
        }
        for link in &span.links {
            lines.push(format!(
                "{indent}  → link trace {} span {}{}",
                link.trace_id,
                link.span_id,
                format_attributes(&link.attributes)
            ));
        }
        if let Some(descendants) = children.get(span.id.as_str()) {
            for child in descendants.iter().rev() {
                pending.push((*child, depth + 1));
            }
        }
    }
    lines
}

/// ` key=JSON` for every attribute, in insertion order (upstream
/// `formatAttributes`).
fn format_attributes(attributes: &Attributes) -> String {
    let mut out = String::new();
    for (key, value) in attributes.iter() {
        out.push(' ');
        out.push_str(key);
        out.push('=');
        out.push_str(&format_value(value));
    }
    out
}

fn format_value(value: &AttributeValue) -> String {
    match value {
        AttributeValue::String(value) => {
            serde_json::to_string(value).unwrap_or_else(|_| "\"\"".to_owned())
        }
        AttributeValue::Int(value) => value.to_string(),
        AttributeValue::Float(value) => format_float(*value),
        AttributeValue::Bool(value) => value.to_string(),
        AttributeValue::StringList(values) => {
            serde_json::to_string(values).unwrap_or_else(|_| "[]".to_owned())
        }
        AttributeValue::IntList(values) => {
            serde_json::to_string(values).unwrap_or_else(|_| "[]".to_owned())
        }
        AttributeValue::FloatList(values) => {
            let rendered: Vec<String> = values.iter().map(|value| format_float(*value)).collect();
            format!("[{}]", rendered.join(","))
        }
        AttributeValue::BoolList(values) => {
            serde_json::to_string(values).unwrap_or_else(|_| "[]".to_owned())
        }
    }
}

/// JSON number spelling for an f64 (`null` for non-finite values, matching
/// `serde_json`'s behavior).
fn format_float(value: f64) -> String {
    serde_json::Number::from_f64(value)
        .map(|number| number.to_string())
        .unwrap_or_else(|| "null".to_owned())
}
