//! Protocol schema error type.

use std::fmt;

// ---------------------------------------------------------------------------
// Error type
// ---------------------------------------------------------------------------

/// Returned when a protocol value violates the schema.
#[derive(Debug)]
pub struct SchemaError {
    message: String,
}

impl SchemaError {
    pub(crate) fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for SchemaError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for SchemaError {}
