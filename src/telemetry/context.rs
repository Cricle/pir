//! W3C-compatible trace identity.
//!
//! `TraceContext` is the typed parent/identity object carried by spans, links,
//! and the propagation carrier. IDs render as lowercase hex exactly as W3C
//! `traceparent` requires.

use std::fmt;
use std::str::FromStr;

/// 128-bit W3C trace id.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, PartialOrd, Ord)]
pub struct TraceId([u8; 16]);

impl TraceId {
    pub const INVALID: Self = Self([0; 16]);

    pub const fn from_bytes(bytes: [u8; 16]) -> Self {
        Self(bytes)
    }

    pub const fn to_bytes(self) -> [u8; 16] {
        self.0
    }

    /// A fresh random trace id. Never returns the all-zero invalid id.
    pub fn generate() -> Self {
        let mut bytes = uuid::Uuid::new_v4().into_bytes();
        if bytes.iter().all(|byte| *byte == 0) {
            bytes[15] = 1;
        }
        Self(bytes)
    }

    pub fn is_valid(&self) -> bool {
        self.0.iter().any(|byte| *byte != 0)
    }

    /// Parse exactly 32 lowercase hex characters.
    pub fn from_hex(value: &str) -> Option<Self> {
        decode_hex::<16>(value).map(Self)
    }
}

impl fmt::Display for TraceId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&encode_hex(&self.0))
    }
}

impl FromStr for TraceId {
    type Err = TraceParseError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::from_hex(value).ok_or(TraceParseError)
    }
}

/// 64-bit W3C span id.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, PartialOrd, Ord)]
pub struct SpanId([u8; 8]);

impl SpanId {
    pub const INVALID: Self = Self([0; 8]);

    pub const fn from_bytes(bytes: [u8; 8]) -> Self {
        Self(bytes)
    }

    pub const fn to_bytes(self) -> [u8; 8] {
        self.0
    }

    /// A fresh random span id. Never returns the all-zero invalid id.
    pub fn generate() -> Self {
        let bytes = uuid::Uuid::new_v4().into_bytes();
        let mut id = [0u8; 8];
        id.copy_from_slice(&bytes[..8]);
        if id.iter().all(|byte| *byte == 0) {
            id[7] = 1;
        }
        Self(id)
    }

    pub fn is_valid(&self) -> bool {
        self.0.iter().any(|byte| *byte != 0)
    }

    /// Parse exactly 16 lowercase hex characters.
    pub fn from_hex(value: &str) -> Option<Self> {
        decode_hex::<8>(value).map(Self)
    }
}

impl fmt::Display for SpanId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&encode_hex(&self.0))
    }
}

impl FromStr for SpanId {
    type Err = TraceParseError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::from_hex(value).ok_or(TraceParseError)
    }
}

/// W3C trace flags; bit 0 is the sampled flag.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TraceFlags(u8);

impl TraceFlags {
    pub const SAMPLED: Self = Self(0x01);

    pub const fn new(bits: u8) -> Self {
        Self(bits)
    }

    pub const fn bits(self) -> u8 {
        self.0
    }

    pub const fn is_sampled(self) -> bool {
        self.0 & 0x01 == 0x01
    }
}

/// Typed trace identity: trace id + current span id + flags/state. Extracted
/// contexts are marked `remote`, which a later integration wave can use to
/// decide whether to restart a trace.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TraceContext {
    trace_id: TraceId,
    span_id: SpanId,
    trace_flags: TraceFlags,
    trace_state: Option<String>,
    remote: bool,
}

impl TraceContext {
    pub const fn new(trace_id: TraceId, span_id: SpanId, trace_flags: TraceFlags) -> Self {
        Self {
            trace_id,
            span_id,
            trace_flags,
            trace_state: None,
            remote: false,
        }
    }

    /// Invalid all-zero context, the identity of a disabled span.
    pub const fn invalid() -> Self {
        Self::new(TraceId::INVALID, SpanId::INVALID, TraceFlags::new(0))
    }

    pub const fn trace_id(&self) -> TraceId {
        self.trace_id
    }

    pub const fn span_id(&self) -> SpanId {
        self.span_id
    }

    pub const fn trace_flags(&self) -> TraceFlags {
        self.trace_flags
    }

    /// Raw `tracestate` value, preserved verbatim across propagation.
    pub fn trace_state(&self) -> Option<&str> {
        self.trace_state.as_deref()
    }

    pub fn is_remote(&self) -> bool {
        self.remote
    }

    pub fn is_valid(&self) -> bool {
        self.trace_id.is_valid() && self.span_id.is_valid()
    }

    pub fn is_sampled(&self) -> bool {
        self.trace_flags.is_sampled()
    }

    #[must_use]
    pub fn with_trace_state(mut self, trace_state: impl Into<String>) -> Self {
        self.trace_state = Some(trace_state.into());
        self
    }

    #[must_use]
    pub const fn with_remote(mut self, remote: bool) -> Self {
        self.remote = remote;
        self
    }
}

/// Parse error for [`TraceId`] / [`SpanId`] hex strings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TraceParseError;

impl fmt::Display for TraceParseError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("invalid trace id")
    }
}

impl std::error::Error for TraceParseError {}

/// Process-wide invalid context used by [`crate::telemetry::NoopSpan`].
static NOOP_CONTEXT: TraceContext = TraceContext::invalid();

pub(crate) fn noop_context() -> &'static TraceContext {
    &NOOP_CONTEXT
}

fn encode_hex(bytes: &[u8]) -> String {
    use fmt::Write as _;

    let mut encoded = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        // Writing into a String cannot fail.
        let _ = write!(encoded, "{byte:02x}");
    }
    encoded
}

fn decode_hex<const N: usize>(value: &str) -> Option<[u8; N]> {
    let bytes = value.as_bytes();
    if bytes.len() != N * 2 {
        return None;
    }
    let mut decoded = [0u8; N];
    for (index, pair) in bytes.as_chunks::<2>().0.iter().enumerate() {
        let high = lowercase_hex_digit(pair[0])?;
        let low = lowercase_hex_digit(pair[1])?;
        decoded[index] = (high << 4) | low;
    }
    Some(decoded)
}

pub(crate) fn lowercase_hex_digit(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        _ => None,
    }
}

pub(crate) fn is_lowercase_hex(value: &str) -> bool {
    value
        .bytes()
        .all(|byte| lowercase_hex_digit(byte).is_some())
}
