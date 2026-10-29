//! Durable's content-free span, event and metric vocabulary, as Rust data, plus
//! the host-facing emission seam that is typed by it.
//!
//! Port of upstream `packages/durable/src/harness/telemetry.ts`. Upstream calls
//! `defineTelemetrySchema(...)` from `@earendil-works/pi-telemetry`, which
//! registers the vocabulary with a tracing backend and hands back typed
//! starters. Here the schema is plain data a host can read, validate against,
//! or translate into whatever exporter it uses, and emission goes through
//! [`TelemetrySink`], a small `Send + Sync` trait whose methods default to
//! no-ops. [`HarnessTelemetry`] wraps an optional `Arc<dyn TelemetrySink>` and
//! defaults to [`HarnessTelemetry::none`]; [`NativeTelemetrySink`] bridges into
//! the crate's portable telemetry core ([`crate::telemetry`]) and
//! [`HarnessTelemetry::from_telemetry`] installs it;
//! [`RecordingTelemetry`] is a small Vec-backed implementation for tests.
//!
//! Call sites emit only through the typed helpers on [`HarnessTelemetry`]
//! (`start_submit_span`, `start_phase_span`, `entry_committed`,
//! `record_report`, `phase_duration`, …), so an attribute key, event name or
//! metric name cannot drift from the descriptors below. The helpers are wired
//! into the scheduler, submissions, harness and generation/tool paths; a host
//! that wants durable telemetry supplies a [`TelemetrySink`] implementation
//! (no external exporter/backend ships in-tree).
//!
//! The contract the schema encodes, unchanged from upstream:
//!
//! * **IDs address durable records, never content.** Every high-cardinality
//!   attribute is a numeric id (entry, submission, conversation, task) or a
//!   tool-call id; no message text, tool output or prompt ever appears.
//! * **`pi.durable.submit`** covers public submission admission and its commit.
//!   Inputs start root traces linked to their callers; writes are children.
//!   Internal admissions emit phase events only.
//! * **`pi.durable.phase`** is one task phase invocation including its decision
//!   commit — never a wait, never a restart.
//! * **Events** are attached to the span that raised them; **metrics** are
//!   grouped by the attributes they declare.
//!
//! Cardinailty hints (`high` / `low` / `medium`) are upstream's; a backend uses
//! them to decide whether an attribute is safe as a metric dimension rather
//! than a span tag.
//!
//! # Deliberate Rust deviations
//!
//! * Upstream nests `entry` and `submission` event descriptors inside both
//!   spans by reference. Here they are declared once as statics and referenced,
//!   and the derived `PartialEq` compares by name and shape, so the
//!   duplication is not repeated in memory.
//! * `status` is a pair of strings (`default`, `error_when`) instead of a
//!   callback: it is documentation for a backend, not behaviour.
//! * Upstream's `attributes` maps are `Record<string, …>`; here they are
//!   ordered slices ([`AttributeDescriptor`]) so a schema dump is deterministic
//!   and a test can assert the exact list.
//! * `reportAttributes` is shared between the `pi.durable.reports` counter and
//!   the `pi.durable.report` event, as upstream shares it.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex, PoisonError};

// The core span surface (`set_attributes`/`add_event`/`set_status`) is a trait.
use crate::telemetry::TelemetrySpan as _;

/// The attribute value type a backend must expect.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum AttributeType {
    /// A JSON string.
    String,
    /// A JSON number (ids, durations, attempts).
    Number,
    /// A JSON boolean.
    Boolean,
}

impl AttributeType {
    /// The upstream wire name.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::String => "string",
            Self::Number => "number",
            Self::Boolean => "boolean",
        }
    }
}

/// How safe an attribute is as a metric dimension.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Cardinality {
    /// Bounded, small domain.
    Low,
    /// Moderately bounded.
    Medium,
    /// Unbounded: an id, never a dimension.
    High,
}

impl Cardinality {
    /// The upstream wire name.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
        }
    }
}

/// One attribute of a span, event or metric.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct AttributeDescriptor {
    /// The attribute key, as it appears on the wire (`submission.type`,
    /// `entry.id`, …).
    pub key: &'static str,
    /// The value type a backend must expect.
    pub value_type: AttributeType,
    /// Whether every record of the enclosing item carries it.
    pub required: bool,
    /// Human-readable meaning.
    pub description: &'static str,
    /// The closed value domain, when the attribute has one.
    pub values: Option<&'static [&'static str]>,
    /// Cardinality hint for metric dimensions.
    pub cardinality: Option<Cardinality>,
}

impl AttributeDescriptor {
    /// A required attribute with a closed value domain.
    #[must_use]
    pub const fn enumerated(
        key: &'static str,
        values: &'static [&'static str],
        description: &'static str,
    ) -> Self {
        Self {
            key,
            value_type: AttributeType::String,
            required: true,
            description,
            values: Some(values),
            cardinality: None,
        }
    }

    /// A required numeric attribute (an id or a count).
    #[must_use]
    pub const fn number(
        key: &'static str,
        required: bool,
        description: &'static str,
        cardinality: Option<Cardinality>,
    ) -> Self {
        Self {
            key,
            value_type: AttributeType::Number,
            required,
            description,
            values: None,
            cardinality,
        }
    }

    /// A required or optional string attribute.
    #[must_use]
    pub const fn string(
        key: &'static str,
        required: bool,
        description: &'static str,
        cardinality: Option<Cardinality>,
    ) -> Self {
        Self {
            key,
            value_type: AttributeType::String,
            required,
            description,
            values: None,
            cardinality,
        }
    }

    /// A string attribute with a closed value domain that may be absent.
    #[must_use]
    pub const fn optional_enum(
        key: &'static str,
        values: &'static [&'static str],
        description: &'static str,
    ) -> Self {
        Self {
            key,
            value_type: AttributeType::String,
            required: false,
            description,
            values: Some(values),
            cardinality: None,
        }
    }

    /// A boolean attribute.
    #[must_use]
    pub const fn boolean(key: &'static str, required: bool, description: &'static str) -> Self {
        Self {
            key,
            value_type: AttributeType::Boolean,
            required,
            description,
            values: None,
            cardinality: None,
        }
    }
}

/// One span descriptor: its attributes, its events and its status rule.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SpanDescriptor {
    /// The span name (`pi.durable.submit`, `pi.durable.phase`).
    pub name: &'static str,
    /// How traces are parented (upstream `parents: { kind: "any" }`).
    pub parents: &'static str,
    /// What the span covers.
    pub description: &'static str,
    /// Attributes recorded when the span starts.
    pub start_attributes: &'static [AttributeDescriptor],
    /// Attributes recorded when it ends.
    pub end_attributes: &'static [AttributeDescriptor],
    /// Events this span may raise.
    pub events: &'static [&'static EventDescriptor],
    /// The default status a backend reports for the span.
    pub status_default: &'static str,
    /// When the span's status becomes `error`.
    pub status_error_when: &'static str,
}

/// One event descriptor attached to a span.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct EventDescriptor {
    /// The event name (`pi.durable.entry`, …).
    pub name: &'static str,
    /// What the event marks.
    pub description: &'static str,
    /// Its attributes.
    pub attributes: &'static [AttributeDescriptor],
}

/// The kind of a metric instrument.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum MetricKind {
    /// A monotonically increasing count.
    Counter,
    /// A distribution of values.
    Histogram,
    /// A value that can go up and down.
    UpDownCounter,
}

impl MetricKind {
    /// The upstream wire name.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Counter => "counter",
            Self::Histogram => "histogram",
            Self::UpDownCounter => "upDownCounter",
        }
    }
}

/// One metric descriptor.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MetricDescriptor {
    /// The metric name (`pi.durable.phase.duration`, …).
    pub name: &'static str,
    /// The instrument kind.
    pub kind: MetricKind,
    /// The unit, when the metric has one (`ms`).
    pub unit: Option<&'static str>,
    /// What the metric measures.
    pub description: &'static str,
    /// The dimensions it is grouped by.
    pub attributes: &'static [AttributeDescriptor],
}

/// The durable telemetry schema (upstream `DURABLE_TELEMETRY_SCHEMA`).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DurableTelemetrySchema {
    /// The schema version (upstream `version: 1`).
    pub version: i64,
    /// The spans.
    pub spans: &'static [SpanDescriptor],
    /// The metrics.
    pub metrics: &'static [MetricDescriptor],
}

impl DurableTelemetrySchema {
    /// The span with this name, if any.
    #[must_use]
    pub fn span(&self, name: &str) -> Option<&'static SpanDescriptor> {
        self.spans.iter().find(|span| span.name == name)
    }

    /// The metric with this name, if any.
    #[must_use]
    pub fn metric(&self, name: &str) -> Option<&'static MetricDescriptor> {
        self.metrics.iter().find(|metric| metric.name == name)
    }

    /// Every event reachable through any span, deduplicated by name in
    /// declaration order.
    ///
    /// Upstream declares two of them (`entry`, `submission`) inside both spans;
    /// this returns each once, which is the shape an exporter wants when it
    /// validates a received event name.
    #[must_use]
    pub fn events(&self) -> Vec<&'static EventDescriptor> {
        let mut events: Vec<&'static EventDescriptor> = Vec::new();
        for span in self.spans {
            for event in span.events {
                if !events.iter().any(|seen| seen.name == event.name) {
                    events.push(*event);
                }
            }
        }
        events
    }

    /// The event with this name, if any span raises it.
    #[must_use]
    pub fn event(&self, name: &str) -> Option<&'static EventDescriptor> {
        self.events().into_iter().find(|event| event.name == name)
    }
}

// ─── Shared descriptors ─────────────────────────────────────────────────────

/// The report type domain (upstream `ReportInfo["type"]`).
pub const REPORT_TYPES: &[&str] = &[
    "session_failed",
    "task_faulted",
    "task_parked",
    "migration_failed",
    "task_outdated",
    "task_error",
    "extension_error",
    "host_error",
    "listener_error",
];

/// The fixed report reason domain.
pub const REPORT_REASONS: &[&str] = &[
    "threw",
    "restart_limit",
    "missing_task",
    "incompatible_task",
];

/// Attributes a `pi.durable.report` event and the `pi.durable.reports` counter
/// share (upstream `reportAttributes`).
pub static REPORT_ATTRIBUTES: &[AttributeDescriptor] = &[
    AttributeDescriptor::enumerated("type", REPORT_TYPES, "ReportInfo type."),
    AttributeDescriptor::optional_enum(
        "reason",
        REPORT_REASONS,
        "Fixed report reason, when present.",
    ),
    AttributeDescriptor::string(
        "extension",
        false,
        "Registered extension name, when present.",
        None,
    ),
    AttributeDescriptor::number(
        "task.id",
        false,
        "Task ID, when present.",
        Some(Cardinality::High),
    ),
];

/// The `pi.durable.entry` event: a transcript entry was committed; content
/// remains in the transcript.
pub static ENTRY_EVENT: EventDescriptor = EventDescriptor {
    name: "pi.durable.entry",
    description: "A transcript entry was committed; content remains in the transcript.",
    attributes: &[
        AttributeDescriptor::number(
            "entry.id",
            true,
            "Transcript entry ID.",
            Some(Cardinality::High),
        ),
        AttributeDescriptor::string("entry.kind", true, "Entry kind.", None),
    ],
};

/// The `pi.durable.submission` event: a submission was settled.
pub static SUBMISSION_EVENT: EventDescriptor = EventDescriptor {
    name: "pi.durable.submission",
    description: "A submission was settled.",
    attributes: &[
        AttributeDescriptor::number(
            "submission.id",
            true,
            "Submission ID.",
            Some(Cardinality::High),
        ),
        AttributeDescriptor::enumerated("status", &["done", "unanswered"], "Settlement."),
        AttributeDescriptor::number(
            "answer.entry_id",
            false,
            "Answer entry ID.",
            Some(Cardinality::High),
        ),
    ],
};

/// The `pi.durable.report` event: a report was raised; no error payload is
/// recorded.
pub static REPORT_EVENT: EventDescriptor = EventDescriptor {
    name: "pi.durable.report",
    description: "A report was raised; no error payload is recorded.",
    attributes: REPORT_ATTRIBUTES,
};

/// The `pi.durable.checkpoint` event: the running checkpoint's phase changed
/// while the invocation continued.
pub static CHECKPOINT_EVENT: EventDescriptor = EventDescriptor {
    name: "pi.durable.checkpoint",
    description: "The running checkpoint's phase changed while the invocation continued.",
    attributes: &[AttributeDescriptor::string(
        "phase",
        true,
        "Committed checkpoint phase.",
        None,
    )],
};

/// The `pi.durable.replay` event: a nested tool call was answered from its
/// stored result.
pub static REPLAY_EVENT: EventDescriptor = EventDescriptor {
    name: "pi.durable.replay",
    description: "A nested tool call was answered from its stored result.",
    attributes: &[AttributeDescriptor::string(
        "tool.name",
        true,
        "Called tool name.",
        None,
    )],
};

/// The submission-admission span.
pub static SUBMIT_SPAN: SpanDescriptor = SpanDescriptor {
    name: "pi.durable.submit",
    parents: "any",
    description: "Public submission admission and its commit. Inputs start root traces linked to their callers; writes are children. Internal admissions emit phase events only.",
    start_attributes: &[
        AttributeDescriptor::enumerated("submission.type", &["input", "write"], "Submission type."),
        AttributeDescriptor::number(
            "conversation.id",
            true,
            "Conversation ID.",
            Some(Cardinality::High),
        ),
    ],
    end_attributes: &[
        AttributeDescriptor::number(
            "submission.id",
            false,
            "Admitted submission ID.",
            Some(Cardinality::High),
        ),
        AttributeDescriptor::optional_enum(
            "result",
            &["queued", "placed", "done"],
            "Admission result; stale writes report done.",
        ),
    ],
    events: &[&ENTRY_EVENT, &SUBMISSION_EVENT, &REPORT_EVENT],
    status_default: "ok",
    status_error_when: "Admission failed.",
};

/// The phase-invocation span.
pub static PHASE_SPAN: SpanDescriptor = SpanDescriptor {
    name: "pi.durable.phase",
    parents: "any",
    description:
        "One task phase invocation, including its decision commit, never a wait or restart.",
    start_attributes: &[
        AttributeDescriptor::number("task.id", true, "Task ID.", Some(Cardinality::High)),
        AttributeDescriptor::string("task.name", true, "Registered task name.", None),
        AttributeDescriptor::string("phase", true, "Invoked checkpoint phase.", None),
        AttributeDescriptor::enumerated("kind", &["run", "abort"], "Invocation kind."),
        AttributeDescriptor::number(
            "attempt",
            false,
            "Checkpoint attempt, when present.",
            None,
        ),
        AttributeDescriptor::boolean(
            "resumed",
            true,
            "First invocation of a task loaded in this process.",
        ),
    ],
    end_attributes: &[
        AttributeDescriptor::string(
            "outcome",
            false,
            "next=<phase>, waiting, sleeping, done, faulted, or aborted.",
            None,
        ),
        AttributeDescriptor::string("tool.name", false, "Called tool name.", None),
        AttributeDescriptor::string(
            "tool.call_id",
            false,
            "Transcript tool call ID.",
            Some(Cardinality::High),
        ),
    ],
    events: &[
        &ENTRY_EVENT,
        &SUBMISSION_EVENT,
        &REPORT_EVENT,
        &CHECKPOINT_EVENT,
        &REPLAY_EVENT,
    ],
    status_default: "ok",
    status_error_when: "Faulted, failed, and orphaned outcomes use task_faulted, task_failed, and task_orphaned with a diagnostic capped at 500 characters. Aborted outcomes keep status ok.",
};

/// The report counter: reports raised, grouped by their fixed `ReportInfo`
/// type.
pub static REPORTS_METRIC: MetricDescriptor = MetricDescriptor {
    name: "pi.durable.reports",
    kind: MetricKind::Counter,
    unit: None,
    description: "Reports raised, grouped by their fixed ReportInfo type.",
    attributes: &[REPORT_ATTRIBUTES[0]],
};

/// The phase-duration histogram.
pub static PHASE_DURATION_METRIC: MetricDescriptor = MetricDescriptor {
    name: "pi.durable.phase.duration",
    kind: MetricKind::Histogram,
    unit: Some("ms"),
    description: "Duration of a phase invocation and its decision commit.",
    attributes: &[
        AttributeDescriptor::string("task", true, "Registered task name.", None),
        AttributeDescriptor::string("phase", true, "Invoked checkpoint phase.", None),
        AttributeDescriptor::string("outcome", true, "Phase outcome.", None),
    ],
};

/// The live-task up/down counter.
pub static TASKS_ACTIVE_METRIC: MetricDescriptor = MetricDescriptor {
    name: "pi.durable.tasks.active",
    kind: MetricKind::UpDownCounter,
    unit: None,
    description: "Live tasks held by this Harness; decremented at terminal settlement or close.",
    attributes: &[AttributeDescriptor::string(
        "task",
        true,
        "Registered task name.",
        None,
    )],
};

/// The schema as a single value.
///
/// A `const` rather than a `LazyLock`: everything it references is static, so
/// building it allocates nothing and a caller can embed it in a larger
/// descriptor table.
pub const DURABLE_TELEMETRY_SCHEMA: DurableTelemetrySchema = DurableTelemetrySchema {
    version: 1,
    spans: &[SUBMIT_SPAN, PHASE_SPAN],
    metrics: &[REPORTS_METRIC, PHASE_DURATION_METRIC, TASKS_ACTIVE_METRIC],
};

// ─── Emission seam ──────────────────────────────────────────────────────────
//
// The vocabulary above is data; the types below are the host-facing way to
// emit it. Nothing here buffers, samples, validates or exports on its own: a
// [`TelemetrySink`] implementation owns that policy. The no-op path allocates
// nothing, and no method in this module panics.

/// One attribute value on the wire.
///
/// The durable schema only declares scalars (see [`AttributeType`]); the split
/// between [`Integer`](Self::Integer) and [`Float`](Self::Float) preserves
/// Rust's `i64` ids without going through `f64`, while both map to the wire
/// type `number`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum AttributeValue<'a> {
    /// A JSON string.
    String(&'a str),
    /// A JSON number that is an integer (ids, attempts, deltas).
    Integer(i64),
    /// A JSON number that is not an integer (durations).
    Float(f64),
    /// A JSON boolean.
    Boolean(bool),
}

impl AttributeValue<'_> {
    /// The schema value type this value must be recorded as.
    #[must_use]
    pub const fn wire_type(self) -> AttributeType {
        match self {
            Self::String(_) => AttributeType::String,
            Self::Integer(_) | Self::Float(_) => AttributeType::Number,
            Self::Boolean(_) => AttributeType::Boolean,
        }
    }
}

/// One emitted attribute: a schema key plus its value.
///
/// Keys are `&'static str` because every durable key is declared in a
/// descriptor. The typed [`HarnessTelemetry`] helpers take keys from those
/// descriptors, so call sites normally never spell a key by hand.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Attribute<'a> {
    /// The attribute key (`entry.id`, `submission.type`, …).
    pub key: &'static str,
    /// Its value.
    pub value: AttributeValue<'a>,
}

impl<'a> Attribute<'a> {
    /// A string attribute.
    #[must_use]
    pub const fn string(key: &'static str, value: &'a str) -> Self {
        Self {
            key,
            value: AttributeValue::String(value),
        }
    }

    /// An integer attribute (`number` on the wire).
    #[must_use]
    pub const fn number(key: &'static str, value: i64) -> Self {
        Self {
            key,
            value: AttributeValue::Integer(value),
        }
    }

    /// A floating-point attribute (`number` on the wire).
    #[must_use]
    pub const fn float(key: &'static str, value: f64) -> Self {
        Self {
            key,
            value: AttributeValue::Float(value),
        }
    }

    /// A boolean attribute.
    #[must_use]
    pub const fn boolean(key: &'static str, value: bool) -> Self {
        Self {
            key,
            value: AttributeValue::Boolean(value),
        }
    }
}

/// An opaque span handle assigned by a [`TelemetrySink`].
///
/// Handles are meaningful only to the sink that produced them. `0` is
/// [`SpanHandle::NONE`]: the no-op handle returned when no telemetry is
/// installed. A sink must assign non-zero handles to real spans.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SpanHandle(u64);

impl SpanHandle {
    /// The no-op handle, returned by [`TelemetrySink::start_span`]'s default.
    pub const NONE: Self = Self(0);

    /// A handle with the sink-assigned `id`.
    #[must_use]
    pub const fn new(id: u64) -> Self {
        Self(id)
    }

    /// The sink-assigned id.
    #[must_use]
    pub const fn id(self) -> u64 {
        self.0
    }

    /// Whether this is [`SpanHandle::NONE`].
    #[must_use]
    pub const fn is_none(self) -> bool {
        self.0 == 0
    }
}

/// The maximum length of a span error message (upstream caps diagnostics at
/// 500 characters).
pub const MAX_ERROR_MESSAGE_CHARS: usize = 500;

/// How a span ended.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SpanStatus<'a> {
    /// The default status (`status_default: "ok"` in the descriptors).
    #[default]
    Ok,
    /// The span failed. Error payloads never carry content beyond a bounded
    /// diagnostic; [`SpanStatus::error_capped`] applies the 500-character cap.
    Error {
        /// A fixed category name (`task_faulted`, …), when known.
        name: Option<&'a str>,
        /// A bounded diagnostic, when known.
        message: Option<&'a str>,
    },
}

impl<'a> SpanStatus<'a> {
    /// An error with a category and diagnostic.
    #[must_use]
    pub const fn error(name: &'a str, message: &'a str) -> Self {
        Self::Error {
            name: Some(name),
            message: Some(message),
        }
    }

    /// An error with a category and a diagnostic capped at
    /// [`MAX_ERROR_MESSAGE_CHARS`] characters.
    #[must_use]
    pub fn error_capped(name: &'a str, message: &'a str) -> Self {
        Self::Error {
            name: Some(name),
            message: Some(capped_message(message)),
        }
    }

    /// An error without a payload (upstream allows `{ status: "error" }`).
    #[must_use]
    pub const fn error_only() -> Self {
        Self::Error {
            name: None,
            message: None,
        }
    }

    /// Whether this status is an error.
    #[must_use]
    pub const fn is_error(self) -> bool {
        matches!(self, Self::Error { .. })
    }
}

fn capped_message(message: &str) -> &str {
    match message.char_indices().nth(MAX_ERROR_MESSAGE_CHARS) {
        Some((index, _)) => message.get(..index).unwrap_or(message),
        None => message,
    }
}

/// A link from a started span to another trace context (upstream
/// `SpanOptions.links`).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SpanLink<'a> {
    /// The linked context, usually produced by [`TelemetrySink::extract`].
    pub context: SpanHandle,
    /// Optional link attributes.
    pub attributes: &'a [Attribute<'a>],
}

impl<'a> SpanLink<'a> {
    /// A link without attributes.
    #[must_use]
    pub const fn new(context: SpanHandle) -> Self {
        Self {
            context,
            attributes: &[],
        }
    }

    /// A link with attributes.
    #[must_use]
    pub const fn with_attributes(context: SpanHandle, attributes: &'a [Attribute<'a>]) -> Self {
        Self {
            context,
            attributes,
        }
    }
}

/// Everything a sink needs to start a span.
///
/// `descriptor` is the schema entry the span must be recorded under; the
/// attributes normally come from the typed [`HarnessTelemetry`] helpers.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SpanStart<'a> {
    /// The span's schema descriptor.
    pub descriptor: &'static SpanDescriptor,
    /// The parent context, local or extracted. `None` starts a root span.
    pub parent: Option<SpanHandle>,
    /// The start attributes (`start_attributes` in the descriptor).
    pub attributes: &'a [Attribute<'a>],
    /// Links to other trace contexts (upstream `SpanOptions.links`).
    pub links: &'a [SpanLink<'a>],
}

impl<'a> SpanStart<'a> {
    /// A root span with `attributes`.
    #[must_use]
    pub const fn new(descriptor: &'static SpanDescriptor, attributes: &'a [Attribute<'a>]) -> Self {
        Self {
            descriptor,
            parent: None,
            attributes,
            links: &[],
        }
    }

    /// Set the parent context.
    #[must_use]
    pub const fn with_parent(mut self, parent: Option<SpanHandle>) -> Self {
        self.parent = parent;
        self
    }

    /// Set span links.
    #[must_use]
    pub const fn with_links(mut self, links: &'a [SpanLink<'a>]) -> Self {
        self.links = links;
        self
    }
}

/// Everything a sink needs to end a span.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SpanEnd<'a> {
    /// The span's schema descriptor.
    pub descriptor: &'static SpanDescriptor,
    /// The end attributes (`end_attributes` in the descriptor).
    pub attributes: &'a [Attribute<'a>],
    /// The final status.
    pub status: SpanStatus<'a>,
}

impl<'a> SpanEnd<'a> {
    /// An `ok` end with `attributes`.
    #[must_use]
    pub const fn new(descriptor: &'static SpanDescriptor, attributes: &'a [Attribute<'a>]) -> Self {
        Self {
            descriptor,
            attributes,
            status: SpanStatus::Ok,
        }
    }

    /// Set the final status.
    #[must_use]
    pub const fn with_status(mut self, status: SpanStatus<'a>) -> Self {
        self.status = status;
        self
    }
}

/// One event to record, tagged by its schema descriptor.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct EventRecord<'a> {
    /// The event's schema descriptor.
    pub descriptor: &'static EventDescriptor,
    /// The event's attributes.
    pub attributes: &'a [Attribute<'a>],
}

impl<'a> EventRecord<'a> {
    /// An event with `attributes`.
    #[must_use]
    pub const fn new(
        descriptor: &'static EventDescriptor,
        attributes: &'a [Attribute<'a>],
    ) -> Self {
        Self {
            descriptor,
            attributes,
        }
    }
}

/// An opaque, JSON-serializable trace carrier for propagation.
///
/// The durable contract stores the carrier returned by
/// [`TelemetrySink::inject`] on submissions and tasks and passes it back to
/// [`TelemetrySink::extract`]. Entries are strings so a host can write them to
/// JSON without knowing the adapter (upstream `TelemetryCarrier`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TraceCarrier {
    entries: Vec<(String, String)>,
}

impl TraceCarrier {
    /// An empty carrier.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// A carrier from arbitrary key/value pairs; later duplicates replace
    /// earlier ones.
    #[must_use]
    pub fn from_pairs<I, K, V>(pairs: I) -> Self
    where
        I: IntoIterator<Item = (K, V)>,
        K: Into<String>,
        V: Into<String>,
    {
        let mut carrier = Self::new();
        for (key, value) in pairs {
            carrier.insert(key, value);
        }
        carrier
    }

    /// Insert one entry, replacing an existing entry with the same key.
    pub fn insert(&mut self, key: impl Into<String>, value: impl Into<String>) {
        let key = key.into();
        let value = value.into();
        if let Some(existing) = self.entries.iter_mut().find(|(held, _)| held == &key) {
            existing.1 = value;
        } else {
            self.entries.push((key, value));
        }
    }

    /// The value stored under `key`.
    #[must_use]
    pub fn get(&self, key: &str) -> Option<&str> {
        self.entries
            .iter()
            .find(|(held, _)| held == key)
            .map(|(_, value)| value.as_str())
    }

    /// Whether the carrier holds no entries.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The number of entries.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Iterate the entries in insertion order.
    pub fn pairs(&self) -> impl Iterator<Item = (&str, &str)> {
        self.entries
            .iter()
            .map(|(key, value)| (key.as_str(), value.as_str()))
    }

    /// Consume the carrier into owned entries.
    #[must_use]
    pub fn into_entries(self) -> Vec<(String, String)> {
        self.entries
    }
}

/// The host's telemetry backend seam.
///
/// A sink receives schema-tagged emission calls and implements whatever
/// buffering, sampling or exporting the host wants. Every method has a no-op
/// default, and implementations **must not panic**: telemetry is observational
/// and must not change durable behaviour. The module's own implementations
/// ([`NoTelemetry`], [`RecordingTelemetry`]) never panic and tolerate poisoned
/// locks.
///
/// Implementations are shared across threads, so a sink needs internal
/// synchronization for mutable state.
pub trait TelemetrySink: Send + Sync {
    /// A cheap gate a call site can use to skip building attributes. The
    /// default is `true`; [`NoTelemetry`] returns `false`.
    fn enabled(&self) -> bool {
        true
    }

    /// Start a span. The returned handle is passed to [`end_span`] and to
    /// [`inject`]; the default returns [`SpanHandle::NONE`].
    ///
    /// [`end_span`]: TelemetrySink::end_span
    /// [`inject`]: TelemetrySink::inject
    fn start_span(&self, _start: SpanStart<'_>) -> SpanHandle {
        SpanHandle::NONE
    }

    /// End a span and record its end attributes and status.
    fn end_span(&self, _span: SpanHandle, _end: SpanEnd<'_>) {}

    /// Record an event on `span`, or in the sink's ambient context when
    /// `span` is `None`.
    fn add_event(&self, _span: Option<SpanHandle>, _event: EventRecord<'_>) {}

    /// Add a measurement for `metric`. Counters add a delta, up/down counters
    /// add a signed delta and histograms record a value.
    fn add_metric(
        &self,
        _metric: &'static MetricDescriptor,
        _value: f64,
        _attributes: &[Attribute<'_>],
    ) {
    }

    /// Inject the context of `span` into an opaque carrier, or `None` when the
    /// sink does not propagate.
    fn inject(&self, _span: SpanHandle) -> Option<TraceCarrier> {
        None
    }

    /// Extract a remote context from `carrier`, or `None` when the sink does
    /// not propagate or cannot read it.
    fn extract(&self, _carrier: Option<&TraceCarrier>) -> Option<SpanHandle> {
        None
    }
}

/// The shared no-op sink.
///
/// [`HarnessTelemetry::none`] (the default) skips the sink call entirely; this
/// type exists for hosts that want to install an explicit no-op
/// `Arc<dyn TelemetrySink>` or name the default in configuration.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoTelemetry;

impl TelemetrySink for NoTelemetry {
    fn enabled(&self) -> bool {
        false
    }
}

/// A durable harness's telemetry handle: an optional shared [`TelemetrySink`].
///
/// Cheap to clone (one `Arc`). The default carries no sink at all, so
/// [`start_span`](Self::start_span) returns [`SpanHandle::NONE`] and every
/// other call is a branch, not a virtual call or allocation. The foreman wires
/// this into `HarnessOptions`/`TaskSchedulerOptions`; the helpers below are the
/// only emission surface call sites should use.
///
/// # Example
///
/// ```
/// use std::sync::Arc;
/// use pir::durable::harness::telemetry::{HarnessTelemetry, RecordingTelemetry};
///
/// let recording = RecordingTelemetry::new();
/// let telemetry = recording.handle();
/// let span = telemetry.start_phase_span(
///     7,
///     "generation",
///     "generate",
///     "run",
///     None,
///     false,
///     None,
///     &[],
/// );
/// telemetry.checkpoint_changed(Some(span), "tools");
/// telemetry.end_phase_span(
///     span,
///     "generation",
///     "generate",
///     Some("next=tools"),
///     None,
///     None,
///     Default::default(),
///     Some(12.5),
/// );
/// assert_eq!(recording.spans().len(), 1);
/// ```
#[derive(Clone, Default)]
pub struct HarnessTelemetry {
    sink: Option<Arc<dyn TelemetrySink>>,
}

impl std::fmt::Debug for HarnessTelemetry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HarnessTelemetry")
            .field("enabled", &self.enabled())
            .finish()
    }
}

impl HarnessTelemetry {
    /// A handle over `sink`.
    #[must_use]
    pub fn new(sink: Arc<dyn TelemetrySink>) -> Self {
        Self { sink: Some(sink) }
    }

    /// The default: no sink, no allocation, [`SpanHandle::NONE`] for spans.
    #[must_use]
    pub const fn none() -> Self {
        Self { sink: None }
    }

    /// A [`RecordingTelemetry`] and a handle that emits into it.
    #[must_use]
    pub fn recording() -> (Self, RecordingTelemetry) {
        let recording = RecordingTelemetry::new();
        let handle = recording.handle();
        (handle, recording)
    }

    /// A handle that emits the durable vocabulary into the crate's portable
    /// telemetry core ([`crate::telemetry`]).
    ///
    /// A disabled core ([`crate::telemetry::Telemetry::noop`], the default)
    /// installs no sink at all — this returns [`HarnessTelemetry::none`], so
    /// call sites keep taking the zero-allocation branch. With a recorder
    /// installed, spans, links, events, counters, up/down counters and
    /// histograms are forwarded by [`NativeTelemetrySink`], and
    /// [`inject`](Self::inject)/[`extract`](Self::extract) use the core's W3C
    /// propagation so stored carriers survive a restart.
    ///
    /// Hosts pass the result through `HarnessOptions::telemetry` or
    /// `SessionOptions::telemetry`.
    #[must_use]
    pub fn from_telemetry(telemetry: crate::telemetry::Telemetry) -> Self {
        if telemetry.is_enabled() {
            NativeTelemetrySink::new(telemetry).handle()
        } else {
            Self::none()
        }
    }

    /// Whether a sink is installed and enabled. Call sites may use this to
    /// skip building attributes, though the typed helpers already avoid
    /// allocation.
    #[must_use]
    #[inline]
    pub fn enabled(&self) -> bool {
        self.sink.as_ref().is_some_and(|sink| sink.enabled())
    }

    /// The installed sink, if any.
    #[must_use]
    pub fn sink(&self) -> Option<&dyn TelemetrySink> {
        self.sink.as_deref()
    }

    /// The installed sink's shared handle, if any.
    #[must_use]
    pub fn sink_arc(&self) -> Option<&Arc<dyn TelemetrySink>> {
        self.sink.as_ref()
    }

    /// The schema vocabulary this handle's helpers emit.
    #[must_use]
    pub const fn schema() -> &'static DurableTelemetrySchema {
        &DURABLE_TELEMETRY_SCHEMA
    }

    /// Start a span. Returns [`SpanHandle::NONE`] when no sink is installed.
    #[must_use]
    #[inline]
    pub fn start_span(&self, start: SpanStart<'_>) -> SpanHandle {
        match &self.sink {
            Some(sink) => sink.start_span(start),
            None => SpanHandle::NONE,
        }
    }

    /// End a span. A no-op for [`SpanHandle::NONE`] and when no sink is
    /// installed.
    #[inline]
    pub fn end_span(&self, span: SpanHandle, end: SpanEnd<'_>) {
        if let Some(sink) = &self.sink {
            sink.end_span(span, end);
        }
    }

    /// Record an event on `span`, or in the sink's ambient context for `None`.
    #[inline]
    pub fn add_event(&self, span: Option<SpanHandle>, event: EventRecord<'_>) {
        if let Some(sink) = &self.sink {
            sink.add_event(span, event);
        }
    }

    /// Add a metric measurement.
    #[inline]
    pub fn add_metric(
        &self,
        metric: &'static MetricDescriptor,
        value: f64,
        attributes: &[Attribute<'_>],
    ) {
        if let Some(sink) = &self.sink {
            sink.add_metric(metric, value, attributes);
        }
    }

    /// Inject `span` into a trace carrier, or `None` when propagation is not
    /// available.
    #[must_use]
    #[inline]
    pub fn inject(&self, span: SpanHandle) -> Option<TraceCarrier> {
        self.sink.as_ref().and_then(|sink| sink.inject(span))
    }

    /// Extract a remote context from `carrier`.
    #[must_use]
    #[inline]
    pub fn extract(&self, carrier: Option<&TraceCarrier>) -> Option<SpanHandle> {
        self.sink.as_ref().and_then(|sink| sink.extract(carrier))
    }

    // ─── Typed span helpers (keys from the descriptors) ─────────────────────

    /// Start `pi.durable.submit`.
    ///
    /// `submission_type` is `"input"` or `"write"` (see the descriptor's value
    /// domain). Inputs start a root trace and link to the caller's context;
    /// writes are children of the caller's context.
    #[must_use]
    pub fn start_submit_span(
        &self,
        submission_type: &str,
        conversation_id: i64,
        parent: Option<SpanHandle>,
        links: &[SpanLink<'_>],
    ) -> SpanHandle {
        let attributes = [
            Attribute::string(
                schema_key(SUBMIT_SPAN.start_attributes, "submission.type"),
                submission_type,
            ),
            Attribute::number(
                schema_key(SUBMIT_SPAN.start_attributes, "conversation.id"),
                conversation_id,
            ),
        ];
        self.start_span(
            SpanStart::new(&SUBMIT_SPAN, &attributes)
                .with_parent(parent)
                .with_links(links),
        )
    }

    /// Start `pi.durable.phase` for one phase invocation.
    ///
    /// `kind` is `"run"` or `"abort"`; `attempt` is omitted when `None`;
    /// `resumed` is true only for the first invocation of a task loaded by
    /// this process. `links` carries stored submission/task trace carriers.
    #[must_use]
    #[allow(clippy::too_many_arguments)]
    pub fn start_phase_span(
        &self,
        task_id: i64,
        task_name: &str,
        phase: &str,
        kind: &str,
        attempt: Option<i64>,
        resumed: bool,
        parent: Option<SpanHandle>,
        links: &[SpanLink<'_>],
    ) -> SpanHandle {
        let attributes = [
            Attribute::number(schema_key(PHASE_SPAN.start_attributes, "task.id"), task_id),
            Attribute::string(
                schema_key(PHASE_SPAN.start_attributes, "task.name"),
                task_name,
            ),
            Attribute::string(schema_key(PHASE_SPAN.start_attributes, "phase"), phase),
            Attribute::string(schema_key(PHASE_SPAN.start_attributes, "kind"), kind),
            Attribute::boolean(schema_key(PHASE_SPAN.start_attributes, "resumed"), resumed),
            Attribute::number(
                schema_key(PHASE_SPAN.start_attributes, "attempt"),
                attempt.unwrap_or(0),
            ),
        ];
        let attributes: &[Attribute<'_>] = if attempt.is_some() {
            &attributes[..]
        } else {
            &attributes[..5]
        };
        self.start_span(
            SpanStart::new(&PHASE_SPAN, attributes)
                .with_parent(parent)
                .with_links(links),
        )
    }

    /// End `pi.durable.submit` with its optional end attributes and status.
    pub fn end_submit_span(
        &self,
        span: SpanHandle,
        submission_id: Option<i64>,
        result: Option<&str>,
        status: SpanStatus<'_>,
    ) {
        let mut attributes = [
            Attribute::number(schema_key(SUBMIT_SPAN.end_attributes, "submission.id"), 0),
            Attribute::string(schema_key(SUBMIT_SPAN.end_attributes, "result"), ""),
        ];
        let mut len = 0;
        if let Some(submission_id) = submission_id {
            attributes[len] = Attribute::number(
                schema_key(SUBMIT_SPAN.end_attributes, "submission.id"),
                submission_id,
            );
            len += 1;
        }
        if let Some(result) = result {
            attributes[len] =
                Attribute::string(schema_key(SUBMIT_SPAN.end_attributes, "result"), result);
            len += 1;
        }
        self.end_span(
            span,
            SpanEnd::new(&SUBMIT_SPAN, &attributes[..len]).with_status(status),
        );
    }

    /// End `pi.durable.phase` with its optional end attributes, status and,
    /// when both `outcome` and `duration_millis` are present, the
    /// `pi.durable.phase.duration` histogram.
    ///
    /// `task_name` and `phase` are the start values, used only for the
    /// duration metric.
    #[allow(clippy::too_many_arguments)]
    pub fn end_phase_span(
        &self,
        span: SpanHandle,
        task_name: &str,
        phase: &str,
        outcome: Option<&str>,
        tool_name: Option<&str>,
        tool_call_id: Option<&str>,
        status: SpanStatus<'_>,
        duration_millis: Option<f64>,
    ) {
        let mut attributes = [
            Attribute::string(schema_key(PHASE_SPAN.end_attributes, "outcome"), ""),
            Attribute::string(schema_key(PHASE_SPAN.end_attributes, "tool.name"), ""),
            Attribute::string(schema_key(PHASE_SPAN.end_attributes, "tool.call_id"), ""),
        ];
        let mut len = 0;
        if let Some(outcome) = outcome {
            attributes[len] =
                Attribute::string(schema_key(PHASE_SPAN.end_attributes, "outcome"), outcome);
            len += 1;
        }
        if let Some(tool_name) = tool_name {
            attributes[len] = Attribute::string(
                schema_key(PHASE_SPAN.end_attributes, "tool.name"),
                tool_name,
            );
            len += 1;
        }
        if let Some(tool_call_id) = tool_call_id {
            attributes[len] = Attribute::string(
                schema_key(PHASE_SPAN.end_attributes, "tool.call_id"),
                tool_call_id,
            );
            len += 1;
        }
        self.end_span(
            span,
            SpanEnd::new(&PHASE_SPAN, &attributes[..len]).with_status(status),
        );
        if let (Some(outcome), Some(millis)) = (outcome, duration_millis) {
            self.phase_duration(task_name, phase, outcome, millis);
        }
    }

    // ─── Typed event helpers (names and keys from the descriptors) ──────────

    /// Record `pi.durable.entry` after the entry commit.
    pub fn entry_committed(&self, span: Option<SpanHandle>, entry_id: i64, entry_kind: &str) {
        let attributes = [
            Attribute::number(schema_key(ENTRY_EVENT.attributes, "entry.id"), entry_id),
            Attribute::string(schema_key(ENTRY_EVENT.attributes, "entry.kind"), entry_kind),
        ];
        self.add_event(span, EventRecord::new(&ENTRY_EVENT, &attributes));
    }

    /// Record `pi.durable.submission` after the submission commit.
    ///
    /// `status` is `"done"` or `"unanswered"`.
    pub fn submission_settled(
        &self,
        span: Option<SpanHandle>,
        submission_id: i64,
        status: &str,
        answer_entry_id: Option<i64>,
    ) {
        let mut attributes = [
            Attribute::number(
                schema_key(SUBMISSION_EVENT.attributes, "submission.id"),
                submission_id,
            ),
            Attribute::string(schema_key(SUBMISSION_EVENT.attributes, "status"), status),
            Attribute::number(
                schema_key(SUBMISSION_EVENT.attributes, "answer.entry_id"),
                0,
            ),
        ];
        let len = match answer_entry_id {
            Some(answer_entry_id) => {
                attributes[2] = Attribute::number(
                    schema_key(SUBMISSION_EVENT.attributes, "answer.entry_id"),
                    answer_entry_id,
                );
                3
            }
            None => 2,
        };
        self.add_event(
            span,
            EventRecord::new(&SUBMISSION_EVENT, &attributes[..len]),
        );
    }

    /// Record `pi.durable.report`.
    ///
    /// `report_type` names a fixed `ReportInfo` type and `reason` a fixed
    /// reason; see [`is_known_report_type`] and [`is_known_report_reason`].
    /// The error payload itself is never recorded.
    pub fn report_event(
        &self,
        span: Option<SpanHandle>,
        report_type: &str,
        reason: Option<&str>,
        extension: Option<&str>,
        task_id: Option<i64>,
    ) {
        let mut attributes = [
            Attribute::string(schema_key(REPORT_EVENT.attributes, "type"), report_type),
            Attribute::string(schema_key(REPORT_EVENT.attributes, "reason"), ""),
            Attribute::string(schema_key(REPORT_EVENT.attributes, "extension"), ""),
            Attribute::number(schema_key(REPORT_EVENT.attributes, "task.id"), 0),
        ];
        let mut len = 1;
        if let Some(reason) = reason {
            attributes[len] =
                Attribute::string(schema_key(REPORT_EVENT.attributes, "reason"), reason);
            len += 1;
        }
        if let Some(extension) = extension {
            attributes[len] =
                Attribute::string(schema_key(REPORT_EVENT.attributes, "extension"), extension);
            len += 1;
        }
        if let Some(task_id) = task_id {
            attributes[len] =
                Attribute::number(schema_key(REPORT_EVENT.attributes, "task.id"), task_id);
            len += 1;
        }
        self.add_event(span, EventRecord::new(&REPORT_EVENT, &attributes[..len]));
    }

    /// Record `pi.durable.checkpoint` after the phase change committed.
    pub fn checkpoint_changed(&self, span: Option<SpanHandle>, phase: &str) {
        let attributes = [Attribute::string(
            schema_key(CHECKPOINT_EVENT.attributes, "phase"),
            phase,
        )];
        self.add_event(span, EventRecord::new(&CHECKPOINT_EVENT, &attributes));
    }

    /// Record `pi.durable.replay` when a nested tool call was answered from
    /// its stored result.
    pub fn replay_answered(&self, span: Option<SpanHandle>, tool_name: &str) {
        let attributes = [Attribute::string(
            schema_key(REPLAY_EVENT.attributes, "tool.name"),
            tool_name,
        )];
        self.add_event(span, EventRecord::new(&REPLAY_EVENT, &attributes));
    }

    // ─── Typed metric helpers (names and keys from the descriptors) ─────────

    /// Increment `pi.durable.reports` by one, grouped by report type.
    pub fn report_raised(&self, report_type: &str) {
        let attributes = [Attribute::string(
            schema_key(REPORTS_METRIC.attributes, "type"),
            report_type,
        )];
        self.add_metric(&REPORTS_METRIC, 1.0, &attributes);
    }

    /// Record `pi.durable.phase.duration` in milliseconds.
    pub fn phase_duration(&self, task: &str, phase: &str, outcome: &str, millis: f64) {
        let attributes = [
            Attribute::string(schema_key(PHASE_DURATION_METRIC.attributes, "task"), task),
            Attribute::string(schema_key(PHASE_DURATION_METRIC.attributes, "phase"), phase),
            Attribute::string(
                schema_key(PHASE_DURATION_METRIC.attributes, "outcome"),
                outcome,
            ),
        ];
        self.add_metric(&PHASE_DURATION_METRIC, millis, &attributes);
    }

    /// Add `delta` to `pi.durable.tasks.active` (one at start, `-1` at
    /// terminal settlement or close).
    pub fn tasks_active(&self, task: &str, delta: i64) {
        let attributes = [Attribute::string(
            schema_key(TASKS_ACTIVE_METRIC.attributes, "task"),
            task,
        )];
        self.add_metric(&TASKS_ACTIVE_METRIC, delta as f64, &attributes);
    }

    /// Record the `pi.durable.report` event and increment `pi.durable.reports`
    /// together, as upstream's `safeReport` does.
    pub fn record_report(
        &self,
        span: Option<SpanHandle>,
        report_type: &str,
        reason: Option<&str>,
        extension: Option<&str>,
        task_id: Option<i64>,
    ) {
        self.report_event(span, report_type, reason, extension, task_id);
        self.report_raised(report_type);
    }
}

/// Whether `value` is one of the fixed `ReportInfo` types
/// ([`REPORT_TYPES`]).
#[must_use]
pub fn is_known_report_type(value: &str) -> bool {
    REPORT_TYPES.contains(&value)
}

/// Whether `value` is one of the fixed report reasons ([`REPORT_REASONS`]).
#[must_use]
pub fn is_known_report_reason(value: &str) -> bool {
    REPORT_REASONS.contains(&value)
}

/// Look a key up in a descriptor's attribute list.
///
/// The descriptor's key wins when present, so the helpers below cannot emit a
/// key the schema does not declare. `fallback` is returned only when the
/// descriptor no longer declares `key`, which keeps the helper panic-free.
fn schema_key(attributes: &'static [AttributeDescriptor], key: &'static str) -> &'static str {
    attributes
        .iter()
        .find(|attribute| attribute.key == key)
        .map_or(key, |attribute| attribute.key)
}

// ─── Native telemetry adapter ───────────────────────────────────────────────

/// The number of extracted remote trace contexts [`NativeTelemetrySink`]
/// keeps for parent/link resolution.
///
/// A handle returned by [`TelemetrySink::extract`] is used right away (a phase
/// span is started with the carrier that was just extracted), so a small FIFO
/// cache is enough. The cap matters: the sink lives for the whole process, and
/// retaining every carrier ever seen would grow without bound.
pub const MAX_TRACE_CONTEXTS: usize = 1024;

/// A [`TelemetrySink`] that emits the durable vocabulary into the crate's
/// portable telemetry core ([`crate::telemetry`]).
///
/// Upstream registers `DURABLE_TELEMETRY_SCHEMA` on the host adapter through
/// `defineTelemetrySchema`; the Rust host seam is
/// [`crate::telemetry::Telemetry`] (`packages/telemetry`), and this sink is the
/// bridge:
///
/// * a started span becomes a core span named after the schema descriptor,
///   carrying the durable start attributes, its parent's trace context and one
///   core [`SpanLink`](crate::telemetry::SpanLink) per resolvable durable link;
/// * ended spans get their end attributes and [`SpanStatus`] before `end()`;
/// * events attach to their span (the core has no ambient context, so an event
///   emitted with `span: None` is dropped — every durable call site passes the
///   span it belongs to);
/// * metrics become a `counter`, `up_down_counter` or `histogram`, with the
///   instrument created once per descriptor;
/// * [`inject`](TelemetrySink::inject)/[`extract`](TelemetrySink::extract) use
///   the core's propagation (W3C `traceparent`/`tracestate`), so a carrier
///   stored on a submission or a task is readable in a later process.
///
/// The no-op path is unaffected: [`HarnessTelemetry::none`] never calls a sink,
/// [`HarnessTelemetry::from_telemetry`] returns `none` for a disabled core, and
/// this sink's [`enabled`](TelemetrySink::enabled) forwards the core's own
/// state. Cloning shares the span registry.
#[derive(Clone)]
pub struct NativeTelemetrySink {
    telemetry: crate::telemetry::Telemetry,
    state: Arc<Mutex<NativeState>>,
}

/// Registry of live spans and extracted remote contexts, plus cached metric
/// instruments.
struct NativeState {
    next_id: u64,
    spans: HashMap<u64, TrackedSpan>,
    /// `(trace id, span id)` → handle of every extracted context, so the same
    /// stored carrier reuses its handle. Entries leave with their span when the
    /// handle is evicted or ended.
    remote: HashMap<(crate::telemetry::TraceId, crate::telemetry::SpanId), u64>,
    /// Handles of the extracted contexts in insertion order, for FIFO eviction
    /// once [`MAX_TRACE_CONTEXTS`] is reached.
    remote_order: VecDeque<u64>,
    counters: HashMap<&'static str, crate::telemetry::Counter>,
    up_down_counters: HashMap<&'static str, crate::telemetry::UpDownCounter>,
    histograms: HashMap<&'static str, crate::telemetry::Histogram>,
}

impl Default for NativeState {
    fn default() -> Self {
        Self {
            next_id: 1,
            spans: HashMap::new(),
            remote: HashMap::new(),
            remote_order: VecDeque::new(),
            counters: HashMap::new(),
            up_down_counters: HashMap::new(),
            histograms: HashMap::new(),
        }
    }
}

/// One handle owned by the sink: a started span, or a context extracted from a
/// carrier (a remote parent that has no span of its own here).
enum TrackedSpan {
    Local(crate::telemetry::Span),
    Remote(crate::telemetry::TraceContext),
}

impl TrackedSpan {
    fn context(&self) -> &crate::telemetry::TraceContext {
        match self {
            Self::Local(span) => span.trace_context(),
            Self::Remote(context) => context,
        }
    }
}

impl NativeTelemetrySink {
    /// A sink emitting into `telemetry`.
    #[must_use]
    pub fn new(telemetry: crate::telemetry::Telemetry) -> Self {
        Self {
            telemetry,
            state: Arc::new(Mutex::new(NativeState::default())),
        }
    }

    /// A [`HarnessTelemetry`] handle that emits into this sink.
    #[must_use]
    pub fn handle(&self) -> HarnessTelemetry {
        HarnessTelemetry::new(Arc::new(self.clone()))
    }

    /// The core handle this sink emits into.
    #[must_use]
    pub fn telemetry(&self) -> &crate::telemetry::Telemetry {
        &self.telemetry
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, NativeState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl TelemetrySink for NativeTelemetrySink {
    fn enabled(&self) -> bool {
        self.telemetry.is_enabled()
    }

    fn start_span(&self, start: SpanStart<'_>) -> SpanHandle {
        if !self.telemetry.is_enabled() {
            return SpanHandle::NONE;
        }
        // Resolve the durable handles under the registry lock, then start the
        // core span outside it: the recorder is host code and must never run
        // with our lock held.
        let (parent, links) = {
            let state = self.lock();
            (
                start
                    .parent
                    .and_then(|handle| tracked_context(&state, handle)),
                start
                    .links
                    .iter()
                    .filter_map(|link| {
                        tracked_context(&state, link.context)
                            .map(|context| (context, link.attributes))
                    })
                    .collect::<Vec<_>>(),
            )
        };
        let mut options = crate::telemetry::SpanOptions::new(start.descriptor.name)
            .with_attributes(native_attributes(start.attributes));
        for (context, attributes) in links {
            options.links.push(
                crate::telemetry::SpanLink::new(context)
                    .with_attributes(native_attributes(attributes)),
            );
        }
        options.parent = parent;
        let span = self.telemetry.start_span(options);
        let id = {
            let mut state = self.lock();
            let id = allocate(&mut state);
            state.spans.insert(id, TrackedSpan::Local(span));
            id
        };
        SpanHandle::new(id)
    }

    fn end_span(&self, span: SpanHandle, end: SpanEnd<'_>) {
        if span.is_none() {
            return;
        }
        let tracked = {
            let mut state = self.lock();
            let tracked = state.spans.remove(&span.id());
            if let Some(TrackedSpan::Remote(context)) = &tracked {
                state
                    .remote
                    .remove(&(context.trace_id(), context.span_id()));
            }
            tracked
        };
        let Some(TrackedSpan::Local(span)) = tracked else {
            return;
        };
        let attributes = native_attributes(end.attributes);
        if !attributes.is_empty() {
            span.set_attributes(&attributes);
        }
        span.set_status(&native_status(end.status));
        span.end();
    }

    fn add_event(&self, span: Option<SpanHandle>, event: EventRecord<'_>) {
        let Some(handle) = span.filter(|handle| !handle.is_none()) else {
            return;
        };
        let tracked = match self.lock().spans.get(&handle.id()) {
            Some(TrackedSpan::Local(span)) => span.clone(),
            _ => return,
        };
        tracked.add_event(event.descriptor.name, &native_attributes(event.attributes));
    }

    fn add_metric(
        &self,
        metric: &'static MetricDescriptor,
        value: f64,
        attributes: &[Attribute<'_>],
    ) {
        if !self.telemetry.is_enabled() {
            return;
        }
        let attributes = native_attributes(attributes);
        // The cached instrument is cloned out under the lock; creation and the
        // measurement both run outside it, so recorder code never executes with
        // our registry locked.
        match metric.kind {
            MetricKind::Counter => {
                let cached = self.lock().counters.get(metric.name).cloned();
                let instrument = match cached {
                    Some(instrument) => instrument,
                    None => {
                        let instrument = self
                            .telemetry
                            .counter(metric.name, &native_instrument_options(metric));
                        self.lock().counters.insert(metric.name, instrument.clone());
                        instrument
                    }
                };
                instrument.add(value.max(0.0).round() as u64, &attributes);
            }
            MetricKind::UpDownCounter => {
                let cached = self.lock().up_down_counters.get(metric.name).cloned();
                let instrument = match cached {
                    Some(instrument) => instrument,
                    None => {
                        let instrument = self
                            .telemetry
                            .up_down_counter(metric.name, &native_instrument_options(metric));
                        self.lock()
                            .up_down_counters
                            .insert(metric.name, instrument.clone());
                        instrument
                    }
                };
                instrument.add(value.round() as i64, &attributes);
            }
            MetricKind::Histogram => {
                let cached = self.lock().histograms.get(metric.name).cloned();
                let instrument = match cached {
                    Some(instrument) => instrument,
                    None => {
                        let instrument = self
                            .telemetry
                            .histogram(metric.name, &native_histogram_options(metric));
                        self.lock()
                            .histograms
                            .insert(metric.name, instrument.clone());
                        instrument
                    }
                };
                instrument.record(value, &attributes);
            }
        }
    }

    fn inject(&self, span: SpanHandle) -> Option<TraceCarrier> {
        if span.is_none() {
            return None;
        }
        let context = {
            let state = self.lock();
            tracked_context(&state, span)?
        };
        let mut carrier = crate::telemetry::Carrier::new();
        if !self.telemetry.inject(&context, &mut carrier) || carrier.is_empty() {
            return None;
        }
        Some(TraceCarrier::from_pairs(carrier))
    }

    fn extract(&self, carrier: Option<&TraceCarrier>) -> Option<SpanHandle> {
        let carrier = carrier.filter(|carrier| !carrier.is_empty())?;
        let converted: crate::telemetry::Carrier = carrier
            .pairs()
            .map(|(key, value)| (key.to_owned(), value.to_owned()))
            .collect();
        let context = self
            .telemetry
            .extract(&converted)
            .filter(crate::telemetry::TraceContext::is_valid)?;
        let (trace_id, span_id) = (context.trace_id(), context.span_id());
        let mut state = self.lock();
        // The same carrier is extracted once per phase; reuse its handle so the
        // registry stays proportional to distinct carriers, not invocations.
        if let Some(id) = state.remote.get(&(trace_id, span_id)) {
            return Some(SpanHandle::new(*id));
        }
        let id = allocate(&mut state);
        state.spans.insert(id, TrackedSpan::Remote(context));
        state.remote.insert((trace_id, span_id), id);
        state.remote_order.push_back(id);
        while state.remote_order.len() > MAX_TRACE_CONTEXTS {
            if let Some(oldest) = state.remote_order.pop_front() {
                if let Some(TrackedSpan::Remote(evicted)) = state.spans.remove(&oldest) {
                    state
                        .remote
                        .remove(&(evicted.trace_id(), evicted.span_id()));
                }
            }
        }
        Some(SpanHandle::new(id))
    }
}

/// The next sink-assigned handle; `0` is reserved for [`SpanHandle::NONE`].
fn allocate(state: &mut NativeState) -> u64 {
    let id = state.next_id;
    state.next_id = state.next_id.wrapping_add(1);
    if state.next_id == 0 {
        state.next_id = 1;
    }
    id
}

/// The trace context a handle resolves to, when the sink still tracks it.
fn tracked_context(
    state: &NativeState,
    handle: SpanHandle,
) -> Option<crate::telemetry::TraceContext> {
    if handle.is_none() {
        return None;
    }
    state
        .spans
        .get(&handle.id())
        .map(|tracked| tracked.context().clone())
}

/// Convert durable attributes into the core's attribute bag.
fn native_attributes(attributes: &[Attribute<'_>]) -> crate::telemetry::Attributes {
    let mut converted = crate::telemetry::Attributes::new();
    for attribute in attributes {
        converted.insert(attribute.key, native_value(attribute.value));
    }
    converted
}

/// One durable attribute value in the core's model.
fn native_value(value: AttributeValue<'_>) -> crate::telemetry::AttributeValue {
    match value {
        AttributeValue::String(value) => crate::telemetry::AttributeValue::String(value.to_owned()),
        AttributeValue::Integer(value) => crate::telemetry::AttributeValue::Int(value),
        AttributeValue::Float(value) => crate::telemetry::AttributeValue::Float(value),
        AttributeValue::Boolean(value) => crate::telemetry::AttributeValue::Bool(value),
    }
}

/// Convert a durable span status into the core's status.
fn native_status(status: SpanStatus<'_>) -> crate::telemetry::SpanStatus {
    match status {
        SpanStatus::Ok => crate::telemetry::SpanStatus::Ok,
        SpanStatus::Error { name, message } => crate::telemetry::SpanStatus::Error {
            error: match (name, message) {
                (None, None) => None,
                (name, message) => Some(crate::telemetry::SpanError::new(
                    name.unwrap_or("Error"),
                    message.unwrap_or_default(),
                )),
            },
        },
    }
}

/// Instrument metadata from a metric descriptor.
fn native_instrument_options(metric: &MetricDescriptor) -> crate::telemetry::InstrumentOptions {
    let mut options =
        crate::telemetry::InstrumentOptions::new().with_description(metric.description);
    if let Some(unit) = metric.unit {
        options = options.with_unit(unit);
    }
    options
}

/// Histogram metadata from a metric descriptor (default bucket boundaries).
fn native_histogram_options(metric: &MetricDescriptor) -> crate::telemetry::HistogramOptions {
    let mut options =
        crate::telemetry::HistogramOptions::new().with_description(metric.description);
    if let Some(unit) = metric.unit {
        options = options.with_unit(unit);
    }
    options
}

// ─── Recording telemetry (tests) ────────────────────────────────────────────

/// One recorded attribute, with the value owned.
#[derive(Clone, Debug, PartialEq)]
pub struct RecordedAttribute {
    /// The attribute key.
    pub key: &'static str,
    /// The owned value.
    pub value: RecordedAttributeValue,
}

/// An owned [`AttributeValue`].
#[derive(Clone, Debug, PartialEq)]
pub enum RecordedAttributeValue {
    /// A string.
    String(String),
    /// An integer (`number` on the wire).
    Integer(i64),
    /// A float (`number` on the wire).
    Float(f64),
    /// A boolean.
    Boolean(bool),
}

impl From<AttributeValue<'_>> for RecordedAttributeValue {
    fn from(value: AttributeValue<'_>) -> Self {
        match value {
            AttributeValue::String(value) => Self::String(value.to_owned()),
            AttributeValue::Integer(value) => Self::Integer(value),
            AttributeValue::Float(value) => Self::Float(value),
            AttributeValue::Boolean(value) => Self::Boolean(value),
        }
    }
}

/// One recorded link.
#[derive(Clone, Debug, PartialEq)]
pub struct RecordedLink {
    /// The linked context handle.
    pub context: SpanHandle,
    /// The link's attributes.
    pub attributes: Vec<RecordedAttribute>,
}

/// An owned [`SpanStatus`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum RecordedSpanStatus {
    /// The span ended `ok`.
    #[default]
    Ok,
    /// The span ended in error.
    Error {
        /// The fixed category, when recorded.
        name: Option<String>,
        /// The bounded diagnostic, when recorded.
        message: Option<String>,
    },
}

/// The recorded end of a span.
#[derive(Clone, Debug, PartialEq)]
pub struct RecordedSpanEnd {
    /// The end attributes.
    pub attributes: Vec<RecordedAttribute>,
    /// The final status.
    pub status: RecordedSpanStatus,
}

/// One recorded span.
#[derive(Clone, Debug, PartialEq)]
pub struct RecordedSpan {
    /// The sink-assigned handle.
    pub handle: SpanHandle,
    /// The schema descriptor it was started under.
    pub descriptor: &'static SpanDescriptor,
    /// Its parent, when one was supplied.
    pub parent: Option<SpanHandle>,
    /// The start attributes.
    pub start_attributes: Vec<RecordedAttribute>,
    /// Its links.
    pub links: Vec<RecordedLink>,
    /// Its end, once [`TelemetrySink::end_span`] was called.
    pub end: Option<RecordedSpanEnd>,
}

/// One recorded event.
#[derive(Clone, Debug, PartialEq)]
pub struct RecordedEvent {
    /// The span it was attached to, or `None` for the ambient context.
    pub span: Option<SpanHandle>,
    /// The schema descriptor the event was raised under.
    pub descriptor: &'static EventDescriptor,
    /// The event's attributes.
    pub attributes: Vec<RecordedAttribute>,
}

/// One recorded metric measurement.
#[derive(Clone, Debug, PartialEq)]
pub struct RecordedMetric {
    /// The schema descriptor of the instrument.
    pub descriptor: &'static MetricDescriptor,
    /// The measured value.
    pub value: f64,
    /// The measurement's attributes.
    pub attributes: Vec<RecordedAttribute>,
}

#[derive(Debug)]
struct RecordingState {
    next_id: u64,
    spans: Vec<RecordedSpan>,
    events: Vec<RecordedEvent>,
    metrics: Vec<RecordedMetric>,
}

impl Default for RecordingState {
    fn default() -> Self {
        Self {
            next_id: 1,
            spans: Vec::new(),
            events: Vec::new(),
            metrics: Vec::new(),
        }
    }
}

/// A small Vec-backed [`TelemetrySink`] for tests and host diagnostics.
///
/// It records every span start and end, event and metric in insertion order.
/// Cloning it shares the same log. Its propagation is a single
/// [`CARRIER_KEY`](Self::CARRIER_KEY) entry holding the span id, enough to
/// exercise links and stored carriers in tests.
#[derive(Clone, Debug, Default)]
pub struct RecordingTelemetry {
    state: Arc<Mutex<RecordingState>>,
}

impl RecordingTelemetry {
    /// The carrier key this recorder writes in
    /// [`TelemetrySink::inject`].
    pub const CARRIER_KEY: &'static str = "pi.recorded.span";

    /// An empty recorder.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// A [`HarnessTelemetry`] handle that emits into this recorder.
    #[must_use]
    pub fn handle(&self) -> HarnessTelemetry {
        HarnessTelemetry::new(Arc::new(self.clone()))
    }

    /// A copy of the recorded spans, in start order.
    #[must_use]
    pub fn spans(&self) -> Vec<RecordedSpan> {
        self.lock().spans.clone()
    }

    /// A copy of the recorded events, in emission order.
    #[must_use]
    pub fn events(&self) -> Vec<RecordedEvent> {
        self.lock().events.clone()
    }

    /// A copy of the recorded metrics, in emission order.
    #[must_use]
    pub fn metrics(&self) -> Vec<RecordedMetric> {
        self.lock().metrics.clone()
    }

    /// One recorded span by handle.
    #[must_use]
    pub fn span(&self, handle: SpanHandle) -> Option<RecordedSpan> {
        self.lock()
            .spans
            .iter()
            .find(|span| span.handle == handle)
            .cloned()
    }

    /// Drop every record and restart handles at 1.
    pub fn clear(&self) {
        let mut state = self.lock();
        state.spans.clear();
        state.events.clear();
        state.metrics.clear();
        state.next_id = 1;
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, RecordingState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl TelemetrySink for RecordingTelemetry {
    fn start_span(&self, start: SpanStart<'_>) -> SpanHandle {
        let mut state = self.lock();
        let id = state.next_id;
        state.next_id = state.next_id.wrapping_add(1);
        if state.next_id == 0 {
            state.next_id = 1;
        }
        let handle = SpanHandle::new(id);
        state.spans.push(RecordedSpan {
            handle,
            descriptor: start.descriptor,
            parent: start.parent,
            start_attributes: record_attributes(start.attributes),
            links: start
                .links
                .iter()
                .map(|link| RecordedLink {
                    context: link.context,
                    attributes: record_attributes(link.attributes),
                })
                .collect(),
            end: None,
        });
        handle
    }

    fn end_span(&self, span: SpanHandle, end: SpanEnd<'_>) {
        let mut state = self.lock();
        if let Some(record) = state.spans.iter_mut().find(|record| record.handle == span) {
            record.end = Some(RecordedSpanEnd {
                attributes: record_attributes(end.attributes),
                status: match end.status {
                    SpanStatus::Ok => RecordedSpanStatus::Ok,
                    SpanStatus::Error { name, message } => RecordedSpanStatus::Error {
                        name: name.map(str::to_owned),
                        message: message.map(str::to_owned),
                    },
                },
            });
        }
    }

    fn add_event(&self, span: Option<SpanHandle>, event: EventRecord<'_>) {
        self.lock().events.push(RecordedEvent {
            span,
            descriptor: event.descriptor,
            attributes: record_attributes(event.attributes),
        });
    }

    fn add_metric(
        &self,
        metric: &'static MetricDescriptor,
        value: f64,
        attributes: &[Attribute<'_>],
    ) {
        self.lock().metrics.push(RecordedMetric {
            descriptor: metric,
            value,
            attributes: record_attributes(attributes),
        });
    }

    fn inject(&self, span: SpanHandle) -> Option<TraceCarrier> {
        if span.is_none() {
            return None;
        }
        let mut carrier = TraceCarrier::new();
        carrier.insert(Self::CARRIER_KEY, span.id().to_string());
        Some(carrier)
    }

    fn extract(&self, carrier: Option<&TraceCarrier>) -> Option<SpanHandle> {
        let id = carrier?.get(Self::CARRIER_KEY)?.parse::<u64>().ok()?;
        if id == 0 {
            None
        } else {
            Some(SpanHandle::new(id))
        }
    }
}

fn record_attributes(attributes: &[Attribute<'_>]) -> Vec<RecordedAttribute> {
    attributes
        .iter()
        .map(|attribute| RecordedAttribute {
            key: attribute.key,
            value: attribute.value.into(),
        })
        .collect()
}
