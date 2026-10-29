//! Diagnostic routing for host-facing extension/agent notices.
//!
//! ## Why this module exists
//!
//! The session runtime used to report every extension notice, dispatch failure
//! and repair diagnostic with a raw `eprintln!("[ext] …")`. That is fine for
//! headless hosts (print mode surfaces errors on stderr, RPC/JSON keep stderr
//! free for diagnostics), but the interactive TUI owns the terminal: a raw
//! write lands in the middle of the rendered frame and corrupts it
//! (`[ext] …` lines appearing inside the alternate screen), and a repeated
//! failure writes once per event / per message, which both spams the screen
//! and drags on the run. A write can even panic the process: `eprintln!`
//! panics with `failed printing to stderr` when the stderr handle's pipe is
//! closed (`<agentDir>/crashes.json` carries five such crashes, three of them
//! inside the streaming path).
//!
//! ## The rule
//!
//! ONE router, applied at every diagnostic site that a TUI session can reach:
//!
//! - **A `tracing` subscriber is installed** (every real host installs one in
//!   `main_cli::run_mode`: the TUI routes it into `<agentDir>/pir.log`, the
//!   headless modes route it to stderr) → the diagnostic becomes a `tracing`
//!   record at the matching level. The TUI therefore keeps the information in
//!   the log file and never touches the frame; print/RPC/JSON keep their
//!   one-line stderr diagnostic.
//! - **No subscriber** (embedded SDK hosts, unit harnesses) → a plain stderr
//!   line, i.e. the historical behavior, because there is no log destination
//!   to route to. That fallback never panics: `eprintln!` aborts the process on
//!   a closed stderr handle, so the write is error-tolerant.
//!
//! Levels: [`DiagnosticLevel::Info`] for informational notices (`[ext] session
//! name: …`), [`DiagnosticLevel::Warning`] for failures the run survives
//! (a dispatch failure, an ignored request), [`DiagnosticLevel::Error`] for a
//! diagnostic that breaks the model request (the leading-system-message
//! removal) — error records also survive an explicit `RUST_LOG=error` filter.
//!
//! ## Repeat collapsing
//!
//! A failing dispatch repeats identically per event (a `message_update`
//! dispatch fires once per streamed delta), so identical messages are
//! collapsed by a per-message rate limiter: the first occurrence is emitted
//! verbatim, then at most one line per [`REPEAT_WINDOW`] per distinct message
//! with the running total appended (`… (repeated 37x)`). Distinct messages are
//! never collapsed, so the sequence of failure *kinds* stays visible; only the
//! identical flood is bounded. Bounded state ([`MAX_TRACKED_MESSAGES`]) keeps
//! the map from growing across a long session. [`DiagnosticLevel::Error`]
//! bypasses the limiter: a fatal diagnostic is reported every time.

use std::collections::HashMap;
use std::fmt::Display;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

/// Informational extension/plugin diagnostic: routed at info level,
/// repeat-collapsed. Same argument syntax as `eprintln!`
/// (`crate::ext_diag_info!("[ext] session name: {name}")`).
#[macro_export]
macro_rules! ext_diag_info {
    ($($arg:tt)*) => {
        $crate::logging::ext_info(format_args!($($arg)*))
    };
}

/// Extension/plugin failure diagnostic the run survives: routed at warn level,
/// repeat-collapsed. Same argument syntax as `eprintln!`
/// (`crate::ext_diag!("[ext] {event} dispatch failed: {error}")`).
#[macro_export]
macro_rules! ext_diag {
    ($($arg:tt)*) => {
        $crate::logging::ext_warn(format_args!($($arg)*))
    };
}

/// Fatal diagnostic (a run failure, a request that lost its system prompt):
/// routed at error level and never repeat-collapsed.
#[macro_export]
macro_rules! ext_diag_fatal {
    ($($arg:tt)*) => {
        $crate::logging::ext_error(format_args!($($arg)*))
    };
}

/// Severity of a routed diagnostic, mapped onto the matching `tracing` level.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DiagnosticLevel {
    /// Informational extension/plugin notice.
    Info,
    /// A failure the run survives (dispatch failure, ignored request).
    Warning,
    /// A diagnostic that breaks the model request; never collapse-suppressed.
    Error,
}

/// At most one emitted line per distinct message per window.
pub const REPEAT_WINDOW: Duration = Duration::from_millis(1_000);

/// Upper bound on tracked distinct messages (longest-lived map growth).
pub const MAX_TRACKED_MESSAGES: usize = 64;

/// Entries untouched for this long are dropped first when the map is full.
const ENTRY_STALE_AFTER: Duration = Duration::from_secs(60);

/// Informational extension/plugin diagnostic, repeat-collapsed.
pub fn ext_info(message: impl Display) {
    route(DiagnosticLevel::Info, message.to_string());
}

/// Extension/plugin failure diagnostic, repeat-collapsed.
pub fn ext_warn(message: impl Display) {
    route(DiagnosticLevel::Warning, message.to_string());
}

/// Fatal diagnostic: routed at error level and never collapse-suppressed.
pub fn ext_error(message: impl Display) {
    emit(DiagnosticLevel::Error, &message.to_string());
}

/// Repeat-collapsing entry point shared by [`ext_info`] / [`ext_warn`].
fn route(level: DiagnosticLevel, message: String) {
    let now = Instant::now();
    let report = {
        let state = repeat_state();
        let mut guard = state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        guard.record(&message, now, REPEAT_WINDOW)
    };
    match report {
        RepeatReport::First => emit(level, &message),
        RepeatReport::Repeat(count) => emit(level, &format!("{message} (repeated {count}x)")),
        RepeatReport::Suppressed => {}
    }
}

/// Decision for one diagnostic occurrence.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RepeatReport {
    /// First occurrence of this message: emit verbatim.
    First,
    /// Repeat inside the window: emit with the running total.
    Repeat(u64),
    /// Repeat inside the window that the limiter drops.
    Suppressed,
}

/// The process-wide repeat limiter state.
struct RepeatState {
    entries: HashMap<String, RepeatEntry>,
}

/// Occurrence bookkeeping for one distinct message.
struct RepeatEntry {
    /// Total occurrences seen (the first included).
    count: u64,
    /// When this message was last allowed through.
    last_emit: Instant,
    /// When this message was first seen (staleness eviction).
    first_seen: Instant,
}

impl RepeatState {
    /// Default (empty) state.
    fn new() -> Self {
        Self {
            entries: HashMap::new(),
        }
    }

    /// Account for one occurrence of `message` and decide what to do. The
    /// first occurrence always reports [`RepeatReport::First`]; a repeat
    /// reports [`RepeatReport::Repeat`] once per `window` and
    /// [`RepeatReport::Suppressed`] otherwise.
    fn record(&mut self, message: &str, now: Instant, window: Duration) -> RepeatReport {
        if !self.entries.contains_key(message) {
            self.make_room(now, window);
        }
        let entry = self
            .entries
            .entry(message.to_owned())
            .or_insert(RepeatEntry {
                count: 0,
                last_emit: now,
                first_seen: now,
            });
        entry.count += 1;
        if entry.count == 1 {
            entry.last_emit = now;
            return RepeatReport::First;
        }
        if now.saturating_duration_since(entry.last_emit) >= window {
            entry.last_emit = now;
            return RepeatReport::Repeat(entry.count);
        }
        RepeatReport::Suppressed
    }

    /// Keep the map bounded: on a new message with a full map, drop stale
    /// entries, or clear everything when every entry is recent (rare; the
    /// alternative is unbounded memory across a long session).
    fn make_room(&mut self, now: Instant, window: Duration) {
        if self.entries.len() < MAX_TRACKED_MESSAGES {
            return;
        }
        let stale_after = ENTRY_STALE_AFTER.max(window);
        self.entries
            .retain(|_, entry| now.saturating_duration_since(entry.first_seen) < stale_after);
        if self.entries.len() >= MAX_TRACKED_MESSAGES {
            self.entries.clear();
        }
    }
}

/// The process-wide limiter, created on first diagnostic.
fn repeat_state() -> &'static Mutex<RepeatState> {
    static STATE: OnceLock<Mutex<RepeatState>> = OnceLock::new();
    STATE.get_or_init(|| Mutex::new(RepeatState::new()))
}

/// Send one diagnostic to the host's logging destination.
///
/// With a subscriber installed (every real host) the record reaches that
/// subscriber's writer: `<agentDir>/pir.log` in the interactive TUI, stderr in
/// print/RPC/JSON modes. Without one (embedded hosts, harnesses) stderr keeps
/// the historical behavior.
fn emit(level: DiagnosticLevel, message: &str) {
    if !tracing::dispatcher::has_been_set() {
        write_stderr(message);
        return;
    }
    match level {
        DiagnosticLevel::Info => tracing::info!("{}", message),
        DiagnosticLevel::Warning => tracing::warn!("{}", message),
        DiagnosticLevel::Error => tracing::error!("{}", message),
    }
}

/// The no-subscriber fallback. `eprintln!` panics when the stderr handle is
/// gone (a closed pipe: `failed printing to stderr`, recorded in
/// `<agentDir>/crashes.json`) and would take the whole run down from a
/// diagnostic; losing one line is the lesser evil.
fn write_stderr(message: &str) {
    use std::io::Write;
    let mut stderr = std::io::stderr().lock();
    let _ = writeln!(stderr, "{message}");
}
