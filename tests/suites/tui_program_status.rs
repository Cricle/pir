//! OSC 7501 program-status tests: the escape-sequence formatter, the support
//! query/reply matcher (upstream
//! `packages/tui/test/program-status.test.ts`), and the reporter state machine
//! (upstream `packages/coding-agent/test/program-status-reporter.test.ts`).

use std::sync::{Arc, Mutex};

use pir::tui::app::APP_NAME;
use pir::tui::program_status::{
    format_program_status, is_program_status_reply, BlockedStatus, ProgramStatus,
    ProgramStatusKind, ProgramStatusReporter, ProgramStatusState, ProgramStatusWriter,
    PROGRAM_STATUS_QUERY,
};
use pir::tui::stdin_buffer::WriteLog;

fn status(state: ProgramStatusState) -> ProgramStatus {
    ProgramStatus::new(state)
}

fn with_message(mut s: ProgramStatus, message: &str) -> ProgramStatus {
    s.message = Some(message.to_string());
    s
}

// ---------------------------------------------------------------------------
// format_program_status (upstream formatProgramStatus)
// ---------------------------------------------------------------------------

#[test]
fn encodes_state_app_kind_and_base64_message() {
    let base64 = |text: &str| {
        use base64::Engine as _;
        base64::engine::general_purpose::STANDARD.encode(text)
    };
    let mut blocked = status(ProgramStatusState::Blocked);
    blocked.app = Some("pi".to_string());
    blocked.kind = Some(ProgramStatusKind::Permission);
    blocked.message = Some("Allow bash?".to_string());
    assert_eq!(
        format_program_status(&blocked),
        format!(
            "\x1b]7501;state=blocked:app=pi:kind=permission:msg={}\x1b\\",
            base64("Allow bash?")
        ),
    );
    assert_eq!(
        format_program_status(&status(ProgramStatusState::Clear)),
        "\x1b]7501;state=clear\x1b\\",
    );
}

#[test]
fn omits_kind_outside_blocked_invalid_app_names_and_empty_messages() {
    let mut working = status(ProgramStatusState::Working);
    working.app = Some("my app".to_string());
    working.kind = Some(ProgramStatusKind::Auth);
    working.message = Some(" \n ".to_string());
    assert_eq!(
        format_program_status(&working),
        "\x1b]7501;state=working\x1b\\"
    );

    let mut max_app = status(ProgramStatusState::Idle);
    max_app.app = Some("a".repeat(32));
    assert!(format_program_status(&max_app).contains(":app=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\x1b\\"));
    let mut over_app = status(ProgramStatusState::Idle);
    over_app.app = Some("a".repeat(33));
    assert!(!format_program_status(&over_app).contains("app="));
}

#[test]
fn replaces_control_characters_which_make_terminals_discard_the_report() {
    let sequence = format_program_status(&with_message(
        status(ProgramStatusState::Error),
        "first\nsecond\x1b[31m\u{9b}third\t",
    ));
    let encoded = sequence
        .split(":msg=")
        .nth(1)
        .and_then(|rest| rest.strip_suffix("\x1b\\"))
        .unwrap_or("");
    use base64::Engine as _;
    let message = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .unwrap_or_default();
    assert_eq!(String::from_utf8_lossy(&message), "first second [31m third");
}

#[test]
fn cuts_long_messages_at_a_utf8_boundary_within_the_spec_limits() {
    let mut working = status(ProgramStatusState::Working);
    working.app = Some("pi".to_string());
    working.message = Some("é".repeat(2000));
    let sequence = format_program_status(&working);
    let encoded = sequence
        .split(":msg=")
        .nth(1)
        .and_then(|rest| rest.strip_suffix("\x1b\\"))
        .unwrap_or("");
    use base64::Engine as _;
    let message = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .unwrap_or_default();
    assert_eq!(String::from_utf8_lossy(&message), "é".repeat(1024));
    assert!(message.len() <= 2048);
    assert!(sequence.len() <= 4096);
}

// ---------------------------------------------------------------------------
// is_program_status_reply
// ---------------------------------------------------------------------------

#[test]
fn accepts_the_query_echo_with_either_terminator_and_future_pairs() {
    assert!(is_program_status_reply("\x1b]7501;?\x1b\\"));
    assert!(is_program_status_reply("\x1b]7501;?\x07"));
    assert!(is_program_status_reply("\x1b]7501;?version=2\x1b\\"));
    assert!(!is_program_status_reply("\x1b]7501;state=idle\x1b\\"));
    assert!(!is_program_status_reply("\x1b]11;rgb:0000/0000/0000\x07"));
}

#[test]
fn query_constant_matches_the_upstream_probe() {
    assert_eq!(PROGRAM_STATUS_QUERY, "\x1b]7501;?\x1b\\");
}

// ---------------------------------------------------------------------------
// ProgramStatusReporter (upstream ProgramStatusReporter)
// ---------------------------------------------------------------------------

struct ReporterHarness {
    reporter: ProgramStatusReporter,
    reports: Arc<Mutex<Vec<String>>>,
    session_name: Arc<Mutex<Option<String>>>,
}

impl ReporterHarness {
    fn setup(session_name: Option<&str>) -> Self {
        let reports = Arc::new(Mutex::new(Vec::new()));
        let session_name = Arc::new(Mutex::new(session_name.map(str::to_string)));
        let sink_reports = Arc::clone(&reports);
        let sink: ProgramStatusWriter = Arc::new(move |s: &ProgramStatus| {
            sink_reports
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(format_program_status(s));
        });
        let getter_session = Arc::clone(&session_name);
        let reporter = ProgramStatusReporter::new(
            sink,
            Arc::new(move || {
                getter_session
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .clone()
            }),
        );
        Self {
            reporter,
            reports,
            session_name,
        }
    }

    /// Last report minus the app pair (upstream `last()`).
    fn last(&self) -> String {
        let last = self
            .reports
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .last()
            .cloned()
            .unwrap_or_default();
        last.replace(&format!(":app={APP_NAME}"), "")
    }

    fn report_count(&self) -> usize {
        self.reports
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len()
    }
}

#[test]
fn reports_idle_working_during_a_run_and_done_once_it_settles() {
    let mut harness = ReporterHarness::setup(Some("Fix login"));
    harness.reporter.report();
    assert_eq!(harness.last(), "\x1b]7501;state=idle\x1b\\");

    harness.reporter.agent_start();
    assert_eq!(
        harness.last(),
        format!("\x1b]7501;state=working:{}\x1b\\", base64_of("Fix login")),
    );

    harness.reporter.assistant_message_end(false, None);
    harness.reporter.assistant_message_end(false, None);
    assert!(harness.last().contains("state=working"));

    harness.reporter.agent_settled(false);
    assert!(harness.last().contains("state=done"));
    assert!(harness.last().contains(&base64_of("Fix login")[..]));
    // Assistant output is never reported.
    let all = harness
        .reports
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .join("\n");
    assert!(!all.contains("secret assistant output"));
}

#[test]
fn reports_only_the_outcome_of_the_run_retried_errors_final_errors_and_aborts() {
    // A retried error is replaced by its successful retry.
    let mut harness = ReporterHarness::setup(None);
    harness.reporter.agent_start();
    harness
        .reporter
        .assistant_message_end(true, Some("overloaded"));
    harness.reporter.assistant_message_end(false, None);
    harness.reporter.agent_settled(false);
    assert!(harness.last().contains("state=done"));

    // The final error keeps its first line only.
    harness.reporter.agent_start();
    harness
        .reporter
        .assistant_message_end(true, Some("Invalid API key\n{details}"));
    harness.reporter.agent_settled(false);
    let last = harness.last();
    assert!(last.contains("state=error"));
    assert!(last.contains("msg=SW52YWxpZCBBUEkga2V5"));
    assert!(!last.contains("details"));

    // Aborted runs settle to idle.
    harness.reporter.agent_start();
    harness.reporter.assistant_message_end(true, None);
    harness.reporter.agent_settled(true);
    assert!(harness.last().contains("state=idle"));

    // Aborted after a successful response.
    harness.reporter.agent_start();
    harness.reporter.assistant_message_end(false, None);
    harness.reporter.agent_settled(true);
    assert!(harness.last().contains("state=idle"));
}

#[test]
fn reports_a_failed_recovery_compaction_as_the_runs_error() {
    let mut harness = ReporterHarness::setup(None);
    harness.reporter.agent_start();
    harness.reporter.assistant_message_end(false, None);
    harness.reporter.compaction_start();
    harness
        .reporter
        .compaction_end(false, Some("Compaction failed\nstack"), false);
    harness.reporter.agent_settled(false);
    assert!(harness.last().contains("state=error"));
    assert!(harness.last().contains(&base64_of("Compaction failed")[..]));

    // A later successful response overrides the compaction error.
    let mut harness = ReporterHarness::setup(None);
    harness.reporter.agent_start();
    harness.reporter.compaction_start();
    harness
        .reporter
        .compaction_end(false, Some("Compaction failed"), false);
    harness.reporter.assistant_message_end(false, None);
    harness.reporter.agent_settled(false);
    assert!(harness.last().contains("state=done"));
}

#[test]
fn reports_compaction_inside_a_run_and_the_result_of_a_manual_compaction() {
    let mut harness = ReporterHarness::setup(Some("Session"));
    harness.reporter.agent_start();
    harness.reporter.compaction_start();
    assert!(harness.last().contains("state=working"));
    assert!(harness
        .last()
        .contains(&base64_of("Compacting context")[..]));
    harness.reporter.compaction_end(false, None, false);
    assert!(harness.last().contains("state=working"));
    assert!(harness.last().contains(&base64_of("Session")[..]));
    harness.reporter.assistant_message_end(false, None);
    harness.reporter.agent_settled(false);

    // Manual compactions settle the resting status.
    harness.reporter.compaction_start();
    harness.reporter.compaction_end(false, None, true);
    assert!(harness.last().contains("state=done"));
    harness.reporter.compaction_start();
    harness
        .reporter
        .compaction_end(false, Some("No model"), true);
    assert!(harness.last().contains("state=error"));
    harness.reporter.compaction_start();
    harness.reporter.compaction_end(true, None, true);
    assert!(harness.last().contains("state=idle"));
}

#[test]
fn reports_the_most_recent_open_dialog_and_the_underlying_state_once_all_close() {
    let mut harness = ReporterHarness::setup(None);
    harness.reporter.agent_start();
    harness.reporter.set_blocked(
        "extension-selector",
        Some(BlockedStatus {
            kind: ProgramStatusKind::Permission,
            message: "Allow bash?".to_string(),
        }),
    );
    harness.reporter.set_blocked(
        "login",
        Some(BlockedStatus {
            kind: ProgramStatusKind::Auth,
            message: "Log in to Anthropic".to_string(),
        }),
    );
    let last = harness.last();
    assert!(last.contains("state=blocked"));
    assert!(last.contains("kind=auth"));
    assert!(last.contains(&base64_of("Log in to Anthropic")[..]));

    // The run settles while the selector is still open.
    harness.reporter.set_blocked("login", None);
    harness.reporter.assistant_message_end(false, None);
    harness.reporter.agent_settled(false);
    let last = harness.last();
    assert!(last.contains("state=blocked"));
    assert!(last.contains("kind=permission"));

    // Reopening a source replaces its dialog instead of stacking a second one.
    harness.reporter.set_blocked(
        "extension-selector",
        Some(BlockedStatus {
            kind: ProgramStatusKind::Question,
            message: "Pick one".to_string(),
        }),
    );
    assert!(harness.last().contains("kind=question"));
    harness.reporter.set_blocked("extension-selector", None);
    assert!(harness.last().contains("state=done"));
}

#[test]
fn sends_each_status_once_and_follows_session_name_changes() {
    let mut harness = ReporterHarness::setup(Some("Old"));
    harness.reporter.agent_start();
    harness.reporter.assistant_message_end(false, None);
    harness.reporter.report();
    assert_eq!(harness.report_count(), 1);

    *harness
        .session_name
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some("New".to_string());
    harness.reporter.report();
    assert!(harness.last().contains("state=working"));
    assert!(harness.last().contains(&base64_of("New")[..]));
}

#[test]
fn returns_to_idle_when_the_session_is_replaced() {
    let mut harness = ReporterHarness::setup(None);
    harness.reporter.agent_start();
    harness.reporter.assistant_message_end(false, None);
    harness.reporter.agent_settled(false);
    harness.reporter.reset();
    assert!(harness.last().contains("state=idle"));
}

// ---------------------------------------------------------------------------
// Terminal / TuiCore wiring
// ---------------------------------------------------------------------------

/// Process-level env is shared by every test thread of this binary: the two
/// tests below toggle `PI_PROGRAM_STATUS`, and `ProgramStatusNegotiation::begin`
/// reads it. Hold this lock for the whole body of any test that mutates (or
/// asserts against) the override so parallel runs cannot interleave the
/// set/remove with another test's negotiation.
static ENV_LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();

fn env_lock() -> std::sync::MutexGuard<'static, ()> {
    ENV_LOCK
        .get_or_init(|| std::sync::Mutex::new(()))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[test]
fn negotiation_confirms_support_on_the_reply_and_resends_the_latest_status() {
    // Pin the env override off for this test (upstream
    // terminal.test.ts toggles PI_PROGRAM_STATUS the same way).
    let _env = env_lock();
    let previous = std::env::var("PI_PROGRAM_STATUS").ok();
    std::env::remove_var("PI_PROGRAM_STATUS");

    // Observe the byte stream `handle_program_status_reply`/`set_program_status`
    // write (the resend itself, not just the stored negotiation state).
    let log_path = std::env::temp_dir().join(format!(
        "pir-program-status-{}-{}.log",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos())
            .unwrap_or(0)
    ));
    let _ = std::fs::remove_file(&log_path);
    let terminal = pir::tui::core::Terminal::with_write_log(WriteLog::at(&log_path));
    terminal.begin_program_status_negotiation();

    // Unsupported until the terminal answers; the status is only remembered.
    terminal.set_program_status(&with_message(status(ProgramStatusState::Working), "run"));
    assert!(!terminal.program_status_negotiation().supported);

    // The support reply confirms support and re-sends the latest status.
    assert!(terminal.handle_program_status_reply(PROGRAM_STATUS_QUERY));
    let supported = {
        // The guard borrows the terminal's state mutex: drop it before any
        // mutating call below, or `set_program_status` self-deadlocks.
        let state = terminal.program_status_negotiation();
        assert!(state.supported);
        assert!(!state.query_pending);
        assert!(state.latest.is_some());
        state.supported
    };
    assert!(supported);
    let after_reply = std::fs::read_to_string(&log_path).expect("write log after the reply");
    let expected_resend =
        format_program_status(&with_message(status(ProgramStatusState::Working), "run"));
    assert!(
        after_reply.contains(&expected_resend),
        "the support reply must re-send the latest status: expected {expected_resend:?} in {after_reply:?}"
    );

    // The clear state removes the stored status and writes the clear sequence.
    terminal.set_program_status(&status(ProgramStatusState::Clear));
    assert!(terminal.program_status_negotiation().latest.is_none());
    let after_clear = std::fs::read_to_string(&log_path).expect("write log after the clear");
    let expected_clear = format_program_status(&status(ProgramStatusState::Clear));
    assert!(
        after_clear.contains(&expected_clear),
        "clearing a supported terminal must write the clear sequence: expected {expected_clear:?} in {after_clear:?}"
    );
    let _ = std::fs::remove_file(&log_path);

    // Non-7501 sequences are not consumed.
    assert!(!terminal.handle_program_status_reply("\x1b[<65;1;1M"));

    match previous {
        Some(value) => std::env::set_var("PI_PROGRAM_STATUS", value),
        None => std::env::remove_var("PI_PROGRAM_STATUS"),
    }
}

#[test]
fn env_override_forces_support_and_the_query_timeout_settles_unsupported() {
    let _env = env_lock();
    // Pin the env override off for this test (the interactive event loop
    // reads the same variable; see terminal.rs's grace timeout).
    let previous = std::env::var("PI_PROGRAM_STATUS").ok();

    // PI_PROGRAM_STATUS=1 forces support: the status is written without any
    // terminal reply, and the query is not owed.
    std::env::set_var("PI_PROGRAM_STATUS", "1");
    let forced = pir::tui::core::Terminal::new();
    forced.set_program_status(&with_message(status(ProgramStatusState::Working), "run"));
    assert!(forced.program_status_negotiation().supported);
    assert!(!forced.program_status_negotiation().query_pending);
    assert!(forced.program_status_negotiation().latest.is_some());

    // Without the override the query stays pending until the grace timeout
    // (upstream DA1 equivalent) settles the negotiation as unsupported.
    std::env::remove_var("PI_PROGRAM_STATUS");
    let plain = pir::tui::core::Terminal::new();
    plain.set_program_status(&with_message(status(ProgramStatusState::Working), "run"));
    assert!(plain.program_status_negotiation().query_pending);
    assert!(!plain.program_status_negotiation().supported);
    plain.program_status_query_timeout();
    let state = plain.program_status_negotiation();
    assert!(!state.query_pending);
    assert!(!state.supported);
    drop(state);

    match previous {
        Some(value) => std::env::set_var("PI_PROGRAM_STATUS", value),
        None => std::env::remove_var("PI_PROGRAM_STATUS"),
    }
}

fn base64_of(text: &str) -> String {
    use base64::Engine as _;
    format!(
        "msg={}",
        base64::engine::general_purpose::STANDARD.encode(text)
    )
}
