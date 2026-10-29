//! Span contract: names, attributes, events, links, status, and lifecycle.
//!
//! The upstream `startSpan(options, callback)` callback shape becomes a
//! start/handle/end shape here: [`Span`] is the handle returned by
//! [`crate::telemetry::Telemetry::start_span`], and [`TelemetrySpan`] is the
//! trait a backend implements. The AI path stores the handle on
//! [`StreamOptions::telemetry`](crate::ai::types::StreamOptions::telemetry) as
//! the explicit parent of its `pi.ai.request` span; [`Span::telemetry`] gives
//! that handle its recorder's metric/propagation capabilities, mirroring the
//! way an upstream span doubles as a telemetry context.

use std::fmt;
use std::sync::Arc;

use crate::telemetry::attributes::{AttributeValue, Attributes};
use crate::telemetry::context::{noop_context, TraceContext};
use crate::telemetry::facade::TelemetryRecorder;

/// Terminal span status. Upstream models `{status:"ok"}` / `{status:"error",
/// error?}`; an explicit status is never overwritten automatically because the
/// call/handle shape has no implicit error callback.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum SpanStatus {
    #[default]
    Ok,
    Error {
        error: Option<SpanError>,
    },
}

impl SpanStatus {
    pub fn error(message: impl Into<String>) -> Self {
        Self::Error {
            error: Some(SpanError::message(message)),
        }
    }
}

/// Error detail attached to an error status.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpanError {
    pub name: String,
    pub message: String,
}

impl SpanError {
    pub fn new(name: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            message: message.into(),
        }
    }

    pub fn message(message: impl Into<String>) -> Self {
        Self::new("Error", message)
    }
}

/// Recorded span event (`addEvent`).
#[derive(Debug, Clone, PartialEq)]
pub struct SpanEvent {
    pub name: String,
    pub attributes: Attributes,
    /// Time since span start, in milliseconds (upstream `timeMs`). Recorders
    /// that do not timestamp events leave it `None`; the in-memory recorder
    /// fills it so a [`crate::telemetry::TelemetryStore`] export keeps the
    /// event timing information.
    pub time_ms: Option<u64>,
}

impl SpanEvent {
    /// An event without a recorded timestamp.
    pub fn new(name: impl Into<String>, attributes: Attributes) -> Self {
        Self {
            name: name.into(),
            attributes,
            time_ms: None,
        }
    }
}

/// Typed span link with its own attributes.
#[derive(Debug, Clone, PartialEq)]
pub struct SpanLink {
    pub context: TraceContext,
    pub attributes: Attributes,
}

impl SpanLink {
    pub fn new(context: TraceContext) -> Self {
        Self {
            context,
            attributes: Attributes::new(),
        }
    }

    #[must_use]
    pub fn with_attribute(
        mut self,
        key: impl Into<std::borrow::Cow<'static, str>>,
        value: impl Into<AttributeValue>,
    ) -> Self {
        self.attributes.insert(key, value);
        self
    }

    #[must_use]
    pub fn with_attributes(mut self, attributes: Attributes) -> Self {
        self.attributes = attributes;
        self
    }
}

/// Options for starting a span, mirroring upstream `SpanOptions` plus the
/// explicit `parent` context (the Rust API has no ambient async context).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SpanOptions {
    pub name: String,
    pub attributes: Attributes,
    pub links: Vec<SpanLink>,
    pub parent: Option<TraceContext>,
}

impl SpanOptions {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            ..Self::default()
        }
    }

    #[must_use]
    pub fn with_attribute(
        mut self,
        key: impl Into<std::borrow::Cow<'static, str>>,
        value: impl Into<AttributeValue>,
    ) -> Self {
        self.attributes.insert(key, value);
        self
    }

    #[must_use]
    pub fn with_attributes(mut self, attributes: Attributes) -> Self {
        self.attributes = attributes;
        self
    }

    #[must_use]
    pub fn with_link(mut self, link: SpanLink) -> Self {
        self.links.push(link);
        self
    }

    #[must_use]
    pub fn with_parent(mut self, parent: TraceContext) -> Self {
        self.parent = Some(parent);
        self
    }
}

/// The span surface exposed to instrumentation code.
///
/// All methods are synchronous and non-throwing; a backend should ignore
/// calls after the span has ended. [`Span`] is the facade returned by the
/// telemetry API, while [`NoopSpan`] and the test recorder's `TestSpan`
/// implement this trait directly.
pub trait TelemetrySpan: Send + Sync {
    fn name(&self) -> &str;

    fn trace_context(&self) -> &TraceContext;

    fn set_attributes(&self, attributes: &Attributes);

    /// Convenience for a single attribute (allocates only when called with a
    /// non-static key/value).
    fn set_attribute(&self, key: &str, value: AttributeValue) {
        let mut attributes = Attributes::new();
        attributes.insert(key.to_owned(), value);
        self.set_attributes(&attributes);
    }

    fn add_event(&self, name: &str, attributes: &Attributes);

    fn add_link(&self, link: &SpanLink);

    fn set_status(&self, status: &SpanStatus);

    /// End the span. Idempotent.
    fn end(&self);

    fn is_recording(&self) -> bool {
        true
    }
}

/// Zero-sized span used when telemetry is disabled. Every call is a no-op and
/// no allocation happens.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoopSpan;

/// Shared no-op span instance.
pub const NOOP_SPAN: NoopSpan = NoopSpan;

impl TelemetrySpan for NoopSpan {
    fn name(&self) -> &str {
        ""
    }

    fn trace_context(&self) -> &TraceContext {
        noop_context()
    }

    fn set_attributes(&self, _attributes: &Attributes) {}

    fn set_attribute(&self, _key: &str, _value: AttributeValue) {}

    fn add_event(&self, _name: &str, _attributes: &Attributes) {}

    fn add_link(&self, _link: &SpanLink) {}

    fn set_status(&self, _status: &SpanStatus) {}

    fn end(&self) {}

    fn is_recording(&self) -> bool {
        false
    }
}

/// Span handle returned by the telemetry facade. Disabled handles hold no
/// allocation; enabled handles delegate to the recorder's span. Clones share
/// the same span (handy for storing the handle on a cloneable request
/// context); `end` is idempotent.
#[derive(Clone, Default)]
pub struct Span {
    inner: Option<Arc<dyn TelemetrySpan>>,
    recorder: Option<Arc<dyn TelemetryRecorder>>,
}

impl Span {
    /// Disabled handle; identical to what `Telemetry::noop()` returns.
    pub const fn noop() -> Self {
        Self {
            inner: None,
            recorder: None,
        }
    }

    pub(crate) fn from_inner(
        inner: Arc<dyn TelemetrySpan>,
        recorder: Arc<dyn TelemetryRecorder>,
    ) -> Self {
        Self {
            inner: Some(inner),
            recorder: Some(recorder),
        }
    }

    pub fn is_noop(&self) -> bool {
        self.inner.is_none()
    }

    pub fn name(&self) -> &str {
        self.inner.as_deref().map_or("", TelemetrySpan::name)
    }

    pub fn trace_context(&self) -> &TraceContext {
        match &self.inner {
            Some(inner) => inner.trace_context(),
            None => noop_context(),
        }
    }

    pub fn is_recording(&self) -> bool {
        self.inner
            .as_deref()
            .is_some_and(TelemetrySpan::is_recording)
    }

    pub fn end(&self) {
        if let Some(inner) = &self.inner {
            inner.end();
        }
    }

    /// The telemetry facade over this span's recorder, or the noop facade for
    /// a disabled span.
    ///
    /// Upstream spans double as contexts: they carry the adapter's `metrics`
    /// and `propagation` capabilities, so instrumentation that only holds a
    /// parent span can still create instruments and inject/extract trace
    /// context on the same adapter. This accessor is the Rust equivalent.
    pub fn telemetry(&self) -> crate::telemetry::Telemetry {
        match &self.recorder {
            Some(recorder) => crate::telemetry::Telemetry::new(Arc::clone(recorder)),
            None => crate::telemetry::Telemetry::noop(),
        }
    }

    /// Start a child span on the same recorder, inheriting this span's trace
    /// identity. Disabled handles return a disabled child.
    pub fn start_child(&self, mut options: SpanOptions) -> Span {
        let Some(recorder) = &self.recorder else {
            return Self::noop();
        };
        if options.parent.is_none() {
            options.parent = Some(self.trace_context().clone());
        }
        Self::from_inner(recorder.start_span(&options), Arc::clone(recorder))
    }
}

impl fmt::Debug for Span {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Span")
            .field("name", &self.name())
            .field("recording", &self.is_recording())
            .finish()
    }
}

impl TelemetrySpan for Span {
    fn name(&self) -> &str {
        Span::name(self)
    }

    fn trace_context(&self) -> &TraceContext {
        Span::trace_context(self)
    }

    fn set_attributes(&self, attributes: &Attributes) {
        if let Some(inner) = &self.inner {
            inner.set_attributes(attributes);
        }
    }

    fn set_attribute(&self, key: &str, value: AttributeValue) {
        if let Some(inner) = &self.inner {
            inner.set_attribute(key, value);
        }
    }

    fn add_event(&self, name: &str, attributes: &Attributes) {
        if let Some(inner) = &self.inner {
            inner.add_event(name, attributes);
        }
    }

    fn add_link(&self, link: &SpanLink) {
        if let Some(inner) = &self.inner {
            inner.add_link(link);
        }
    }

    fn set_status(&self, status: &SpanStatus) {
        if let Some(inner) = &self.inner {
            inner.set_status(status);
        }
    }

    fn end(&self) {
        Span::end(self);
    }

    fn is_recording(&self) -> bool {
        Span::is_recording(self)
    }
}
