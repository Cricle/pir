//! Bounded server-sent-events parser for the streamable HTTP transport
//! (upstream `consumeSseStream` in
//! `packages/mcp/src/transports/streamable-http.ts`).

use super::error::McpError;

/// One parsed SSE event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SseEvent {
    /// The `event:` field, when present.
    pub event: Option<String>,
    /// The joined `data:` payload.
    pub data: String,
    /// The `id:` field, when present.
    pub id: Option<String>,
}

/// Incremental SSE line parser with a per-event byte cap.
#[derive(Debug)]
pub struct SseParser {
    max_event_bytes: usize,
    event_name: Option<String>,
    event_id: Option<String>,
    data_lines: Vec<String>,
    data_bytes: usize,
}

impl SseParser {
    /// A parser with the given per-event cap.
    pub fn new(max_event_bytes: usize) -> Self {
        Self {
            max_event_bytes,
            event_name: None,
            event_id: None,
            data_lines: Vec::new(),
            data_bytes: 0,
        }
    }

    /// Feed one raw line (without the newline). Returns an event when a blank
    /// line dispatches the pending one (upstream `processLine`/`dispatch`).
    pub fn feed_line(&mut self, raw_line: &str) -> Result<Option<SseEvent>, McpError> {
        let line = raw_line.strip_suffix('\r').unwrap_or(raw_line);
        if line.is_empty() {
            return Ok(self.dispatch());
        }
        if line.starts_with(':') {
            return Ok(None);
        }
        let (field, value) = match line.split_once(':') {
            Some((field, value)) => (field, value.strip_prefix(' ').unwrap_or(value)),
            None => (line, ""),
        };
        match field {
            "data" => {
                self.data_bytes += value.len() + usize::from(!self.data_lines.is_empty());
                if self.data_bytes > self.max_event_bytes {
                    return Err(McpError::MessageTooLarge {
                        limit: self.max_event_bytes,
                    });
                }
                self.data_lines.push(value.to_owned());
            }
            "event" => self.event_name = Some(value.to_owned()),
            "id" if !value.contains('\0') => self.event_id = Some(value.to_owned()),
            _ => {}
        }
        Ok(None)
    }

    /// Flush a pending event at end-of-stream (upstream's final `dispatch`).
    pub fn finish(&mut self) -> Option<SseEvent> {
        self.dispatch()
    }

    fn dispatch(&mut self) -> Option<SseEvent> {
        if self.data_lines.is_empty() {
            self.event_name = None;
            self.event_id = None;
            return None;
        }
        let event = SseEvent {
            event: self.event_name.take(),
            data: self.data_lines.join("\n"),
            id: self.event_id.take(),
        };
        self.data_lines = Vec::new();
        self.data_bytes = 0;
        Some(event)
    }
}

/// Whether an SSE event carries a JSON-RPC message (events without data prime
/// resumption; other event types are not JSON-RPC - upstream `consumeSse`).
pub fn is_json_rpc_event(event: &SseEvent) -> bool {
    if event.data.trim().is_empty() {
        return false;
    }
    match &event.event {
        None => true,
        Some(name) => name == "message",
    }
}
