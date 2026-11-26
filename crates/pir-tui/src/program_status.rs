//! Program Status Protocol (OSC 7501): a program tells the terminal whether
//! it is idle, working, blocked on the user, done, or failed. Only the root
//! record is supported.
//!
//! Port of upstream `packages/tui/src/program-status.ts` (formatter + support
//! query/reply matching) and
//! `packages/coding-agent/src/modes/interactive/program-status-reporter.ts`
//! (the interactive-mode state machine that maps agent-run events onto
//! statuses).
//!
//! Spec: <https://www.superlogical.com/rex/docs/build/program-status>
//!
//! Upstream gating: reports are sent only after the terminal answers the
//! [`PROGRAM_STATUS_QUERY`] support probe (or `PI_PROGRAM_STATUS=1` forces
//! support, `0` disables). There is deliberately no settings.json knob
//! upstream — the only override is the `PI_PROGRAM_STATUS` environment
//! variable, which [`Terminal`](super::core::Terminal) reads during
//! negotiation exactly like upstream.

use std::sync::Arc;

use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use base64::Engine as _;

/// Feature detection query. A supporting terminal replies with the same body.
pub const PROGRAM_STATUS_QUERY: &str = "\x1b]7501;?\x1b\\";

/// `PI_PROGRAM_STATUS=1` always reports, `0` never reports; otherwise reports
/// only after the terminal confirms support (upstream env override).
const ENV_OVERRIDE: &str = "PI_PROGRAM_STATUS";

/// One status report (upstream `ProgramStatus`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProgramStatus {
    pub state: ProgramStatusState,
    /// Stable program name, `[A-Za-z0-9_.+-]{1,32}`. Other values are omitted.
    pub app: Option<String>,
    /// What a blocked program waits for. Omitted for other states.
    pub kind: Option<ProgramStatusKind>,
    /// One human-readable line. Control characters become spaces; longer text
    /// is cut to the spec limit.
    pub message: Option<String>,
}

impl ProgramStatus {
    pub fn new(state: ProgramStatusState) -> Self {
        Self {
            state,
            app: None,
            kind: None,
            message: None,
        }
    }
}

/// Status state (upstream `ProgramStatus["state"]`); `clear` removes the
/// status instead of reporting one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProgramStatusState {
    Idle,
    Working,
    Blocked,
    Done,
    Error,
    Clear,
}

impl ProgramStatusState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Working => "working",
            Self::Blocked => "blocked",
            Self::Done => "done",
            Self::Error => "error",
            Self::Clear => "clear",
        }
    }
}

/// What a blocked program waits for (upstream `ProgramStatus["kind"]`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProgramStatusKind {
    Permission,
    Question,
    Auth,
}

impl ProgramStatusKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Permission => "permission",
            Self::Question => "question",
            Self::Auth => "auth",
        }
    }
}

/// Open dialog entry (upstream `BlockedStatus`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockedStatus {
    pub kind: ProgramStatusKind,
    pub message: String,
}

const APP_PATTERN_OK: fn(char) -> bool =
    |c: char| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '+' | '-');

/// Decoded `msg` limit. Its base64 encoding stays under the 2732-byte
/// encoded limit (upstream `MAX_MESSAGE_BYTES`).
const MAX_MESSAGE_BYTES: usize = 2048;

/// The reply to [`PROGRAM_STATUS_QUERY`]. Later spec revisions may add pairs
/// after the `?` (upstream `isProgramStatusReply`).
pub fn is_program_status_reply(sequence: &str) -> bool {
    let Some(rest) = sequence.strip_prefix("\x1b]7501;?") else {
        return false;
    };
    // Terminator is BEL or ST; the body must not contain BEL or ESC.
    let body = if let Some(body) = rest.strip_suffix('\x07') {
        body
    } else if let Some(body) = rest.strip_suffix("\x1b\\") {
        body
    } else {
        return false;
    };
    !body.contains('\x07') && !body.contains('\x1b')
}

fn is_valid_app_name(app: &str) -> bool {
    !app.is_empty() && app.chars().count() <= 32 && app.chars().all(APP_PATTERN_OK)
}

fn is_control_character(c: char) -> bool {
    let u = c as u32;
    u <= 0x1f || (0x7f..=0x9f).contains(&u)
}

/// Terminals discard reports whose text contains control characters, so runs
/// of them are replaced by a single space (upstream `CONTROL_CHARACTERS`
/// regex replace).
fn replace_control_characters(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut in_control_run = false;
    for c in text.chars() {
        if is_control_character(c) {
            in_control_run = true;
        } else {
            if in_control_run {
                out.push(' ');
                in_control_run = false;
            }
            out.push(c);
        }
    }
    if in_control_run {
        out.push(' ');
    }
    out
}

/// Cut at a UTF-8 boundary within `max_bytes` (upstream `truncateUtf8`).
fn truncate_utf8(text: &str, max_bytes: usize) -> &str {
    if text.len() <= max_bytes {
        return text;
    }
    let mut bytes = 0usize;
    let mut end = text.len();
    for (idx, c) in text.char_indices() {
        let size = c.len_utf8();
        if bytes + size > max_bytes {
            end = idx;
            break;
        }
        bytes += size;
    }
    &text[..end]
}

/// Encode a status report (upstream `formatProgramStatus`).
pub fn format_program_status(status: &ProgramStatus) -> String {
    let mut pairs = vec![format!("state={}", status.state.as_str())];
    if let Some(app) = &status.app {
        if is_valid_app_name(app) {
            pairs.push(format!("app={app}"));
        }
    }
    if status.state == ProgramStatusState::Blocked {
        if let Some(kind) = &status.kind {
            pairs.push(format!("kind={}", kind.as_str()));
        }
    }
    let raw = status.message.as_deref().unwrap_or("");
    let replaced = replace_control_characters(raw);
    let message = truncate_utf8(replaced.trim(), MAX_MESSAGE_BYTES);
    if !message.is_empty() {
        pairs.push(format!("msg={}", BASE64_STANDARD.encode(message)));
    }
    format!("\x1b]7501;{}\x1b\\", pairs.join(":"))
}

/// First line of an error message, or `"Error"` (upstream `firstLine`).
fn first_line(text: &str) -> String {
    let line = text.split('\n').next().unwrap_or("");
    let line = line.strip_suffix('\r').unwrap_or(line);
    let trimmed = line.trim();
    if trimmed.is_empty() {
        "Error".to_string()
    } else {
        trimmed.to_string()
    }
}

/// Sink the reporter writes encoded reports to; implemented by
/// [`Terminal`](super::core::Terminal) via `set_program_status`.
pub type ProgramStatusWriter = Arc<dyn Fn(&ProgramStatus) + Send + Sync>;
/// Session-name getter; `working`/`done` reports carry it as their message.
pub type SessionNameGetter = Arc<dyn Fn() -> Option<String> + Send + Sync>;

/// Reports interactive-mode state to the terminal (OSC 7501): `working` during
/// agent runs and compaction, `blocked` while a dialog waits for the user,
/// then `done`, `error`, or `idle` once the run settles. Messages are limited
/// to the session name, dialog titles, and the first line of errors; prompts
/// and assistant output are never reported.
///
/// Port of upstream `ProgramStatusReporter`. Instead of the upstream
/// `AgentSessionEvent` enum the same transitions are exposed as methods
/// (`agent_start`, `assistant_message_end`, `compaction_start`,
/// `compaction_end`, `agent_settled`, `set_blocked`, `reset`); the caller must
/// only feed assistant messages (upstream filters on `message.role`).
pub struct ProgramStatusReporter {
    write_status: ProgramStatusWriter,
    session_name: SessionNameGetter,
    run_active: bool,
    compacting: bool,
    /// Outcome of the current run, reported once it settles.
    run_result: ProgramStatus,
    /// Status while no run is active.
    resting_status: ProgramStatus,
    /// Open dialogs by source, in the order they opened. The most recent one
    /// is reported.
    blocked: Vec<(String, BlockedStatus)>,
    last_report: Option<String>,
}

impl ProgramStatusReporter {
    pub fn new(write_status: ProgramStatusWriter, session_name: SessionNameGetter) -> Self {
        Self {
            write_status,
            session_name,
            run_active: false,
            compacting: false,
            run_result: ProgramStatus::new(ProgramStatusState::Done),
            resting_status: ProgramStatus::new(ProgramStatusState::Idle),
            blocked: Vec::new(),
            last_report: None,
        }
    }

    /// Replace the session-name getter (upstream `session_info_changed`
    /// changes what the next `working`/`done` report carries).
    pub fn set_session_name(&mut self, session_name: SessionNameGetter) {
        self.session_name = session_name;
    }

    /// Upstream `agent_start`.
    pub fn agent_start(&mut self) {
        self.run_active = true;
        self.run_result = ProgramStatus::new(ProgramStatusState::Done);
        self.report();
    }

    /// Upstream `message_end` for an assistant message: the latest response
    /// decides the outcome, so a retried error is replaced by its successful
    /// retry. `stop_reason_is_error` mirrors `stopReason === "error"`.
    pub fn assistant_message_end(
        &mut self,
        stop_reason_is_error: bool,
        error_message: Option<&str>,
    ) {
        self.run_result = if stop_reason_is_error {
            let message = error_message
                .map(first_line)
                .unwrap_or_else(|| "Error".to_string());
            ProgramStatus {
                state: ProgramStatusState::Error,
                message: Some(message),
                ..ProgramStatus::new(ProgramStatusState::Error)
            }
        } else {
            ProgramStatus::new(ProgramStatusState::Done)
        };
        self.report();
    }

    /// Upstream `compaction_start`.
    pub fn compaction_start(&mut self) {
        self.compacting = true;
        self.report();
    }

    /// Upstream `compaction_end`. `manual` mirrors `event.reason === "manual"`.
    pub fn compaction_end(&mut self, aborted: bool, error_message: Option<&str>, manual: bool) {
        self.compacting = false;
        if self.run_active {
            // A failed recovery compaction ends the run unless a later
            // response succeeds.
            if aborted {
                self.run_result = ProgramStatus::new(ProgramStatusState::Idle);
            } else if let Some(error) = error_message {
                self.run_result = ProgramStatus {
                    state: ProgramStatusState::Error,
                    message: Some(first_line(error)),
                    ..ProgramStatus::new(ProgramStatusState::Error)
                };
            }
        } else if aborted {
            self.resting_status = ProgramStatus::new(ProgramStatusState::Idle);
        } else if manual {
            self.resting_status = match error_message {
                Some(error) => ProgramStatus {
                    state: ProgramStatusState::Error,
                    message: Some(first_line(error)),
                    ..ProgramStatus::new(ProgramStatusState::Error)
                },
                None => ProgramStatus::new(ProgramStatusState::Done),
            };
        }
        self.report();
    }

    /// Upstream `agent_settled`.
    pub fn agent_settled(&mut self, aborted: bool) {
        self.run_active = false;
        self.resting_status = if aborted {
            ProgramStatus::new(ProgramStatusState::Idle)
        } else {
            self.run_result.clone()
        };
        self.report();
    }

    /// Report `blocked` for a dialog until it is cleared with `None`.
    /// Reopening a source replaces it (upstream `setBlocked`).
    pub fn set_blocked(&mut self, source: &str, status: Option<BlockedStatus>) {
        self.blocked.retain(|(existing, _)| existing != source);
        if let Some(status) = status {
            self.blocked.push((source.to_string(), status));
        }
        self.report();
    }

    /// Forget the previous session's run, for example after switching sessions
    /// (upstream `reset`).
    pub fn reset(&mut self) {
        self.run_active = false;
        self.compacting = false;
        self.run_result = ProgramStatus::new(ProgramStatusState::Done);
        self.resting_status = ProgramStatus::new(ProgramStatusState::Idle);
        self.report();
    }

    /// Upstream `report`: deduplicates identical statuses before writing.
    pub fn report(&mut self) {
        let mut status = self.current_status();
        status.app = Some(super::app::APP_NAME.to_string());
        let key = format_program_status(&status);
        if self.last_report.as_deref() == Some(key.as_str()) {
            return;
        }
        self.last_report = Some(key);
        (self.write_status)(&status);
    }

    fn current_status(&self) -> ProgramStatus {
        if let Some((_, blocked)) = self.blocked.last() {
            return ProgramStatus {
                state: ProgramStatusState::Blocked,
                kind: Some(blocked.kind),
                message: Some(blocked.message.clone()),
                ..ProgramStatus::new(ProgramStatusState::Blocked)
            };
        }
        if self.compacting {
            return ProgramStatus {
                message: Some("Compacting context".to_string()),
                ..ProgramStatus::new(ProgramStatusState::Working)
            };
        }
        let mut status = if self.run_active {
            ProgramStatus::new(ProgramStatusState::Working)
        } else {
            self.resting_status.clone()
        };
        if matches!(
            status.state,
            ProgramStatusState::Working | ProgramStatusState::Done
        ) {
            status.message = (self.session_name)();
        }
        status
    }
}

/// OSC 7501 support negotiation state for one terminal (upstream
/// `ProcessTerminal`'s `programStatus*` fields).
#[derive(Debug, Default)]
pub struct ProgramStatusNegotiation {
    /// Latest status, kept across stop so a restart can report it again.
    pub latest: Option<ProgramStatus>,
    /// Whether the terminal confirmed OSC 7501 support since the last start,
    /// or `PI_PROGRAM_STATUS=1`.
    pub supported: bool,
    /// The support query was sent and its DA1 sentinel has not arrived yet.
    pub query_pending: bool,
    /// The query/override decision has been made for this connection.
    pub negotiation_started: bool,
}

impl ProgramStatusNegotiation {
    /// Upstream `queryAndEnableKittyProtocol`'s program-status half: read the
    /// `PI_PROGRAM_STATUS` override, decide support, and return the query to
    /// send (empty when the override settles the answer).
    pub fn begin(&mut self) -> String {
        self.negotiation_started = true;
        let override_value = std::env::var(ENV_OVERRIDE).ok();
        self.supported = override_value.as_deref() == Some("1");
        self.query_pending =
            override_value.as_deref() != Some("1") && override_value.as_deref() != Some("0");
        if self.query_pending {
            PROGRAM_STATUS_QUERY.to_string()
        } else {
            String::new()
        }
    }
}
