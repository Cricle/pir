//! Session journal input shaping for the context-mode index.
//!
//! The host owns the SQLite journal table; this module owns the row shape and
//! the bounds a host applies when it stores rows.

/// Maximum rows retained per session journal.
pub const SESSION_JOURNAL_MAX_ROWS: usize = 2_000;
/// Maximum bytes retained per session journal.
pub const SESSION_JOURNAL_MAX_BYTES: u64 = 32 * 1024 * 1024;
/// Age at which journal rows expire.
pub const SESSION_JOURNAL_TTL_SECS: u64 = 7 * 24 * 60 * 60;

/// A journal row to append.
#[derive(Debug, Clone)]
pub struct JournalInput {
    pub session_id: String,
    pub turn: i64,
    pub kind: String,
    pub tool: String,
    pub path: Option<String>,
    pub key: Option<String>,
    pub ok: bool,
    pub exit_code: Option<i64>,
    pub bytes: usize,
    pub body: String,
}

impl JournalInput {
    pub fn new(
        session_id: String,
        kind: impl Into<String>,
        tool: impl Into<String>,
        body: impl Into<String>,
    ) -> Self {
        let body = body.into();
        Self {
            session_id,
            turn: 0,
            kind: kind.into(),
            tool: tool.into(),
            path: None,
            key: None,
            ok: true,
            exit_code: None,
            bytes: body.len(),
            body,
        }
    }

    pub fn with_path(mut self, path: Option<String>) -> Self {
        self.path = path;
        self
    }

    pub fn with_outcome(mut self, ok: bool, exit_code: Option<i64>, bytes: usize) -> Self {
        self.ok = ok;
        self.exit_code = exit_code;
        self.bytes = bytes;
        self
    }

    /// Whether this row exceeds the per-row body budget a host should apply
    /// (the journal body is diagnostic, not the primary record).
    pub fn body_within_budget(&self, max_bytes: usize) -> bool {
        self.body.len() <= max_bytes
    }
}
