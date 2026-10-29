//! Regression pins for the interactive error surfaces (play-lane findings
//! F2–F4). The decisions under test are the pure extraction surfaces in
//! `pir::tui::terminal`, exactly like the F13 worker-queue pin in
//! `tests/tui_loop_fixes.rs`:
//!
//! - `worker_level_failure_signals` / `worker_error_ends_run_state` (F2): a
//!   worker-level error ends the UI's run state (the optimistically armed
//!   `Working` spinner), whether it arrives through the worker's own
//!   Error→Idle tail or through the UI's receive-side rule.
//! - `ctrl_d_action` (F3): upstream's ctrl+d rule — exit on an empty editor,
//!   otherwise the editor's own delete-char-forward runs.
//! - `settled_run_error_notice` (F4): one abort produces one visible error,
//!   not the two identical `Error: Request was aborted` rows.
//! - `event_loop_poll_timeout` (idle-CPU lane): the loop's next-tick timeout —
//!   the frame cadence only while work is in flight, a longer idle sleep
//!   otherwise, always clamped to the input gate's pending sequence window.
//!
//! The F1 `/quit` finding was a ConPTY harness artifact (the driver judged
//! liveness from pty-pipe EOF, which a ConPTY never delivers after the child
//! exits; the fixed driver uses the process handle). The exit path itself was
//! correct and is deliberately unchanged here.

use std::time::{Duration, Instant};

use pir::ai::types::StopReason;
use pir::tui::editor::Editor;
use pir::tui::terminal::{
    ctrl_d_action, event_loop_poll_timeout, initial_interactive_thinking,
    prompt_arms_working_label, settled_run_error_notice, worker_error_ends_run_state,
    worker_level_failure_signals, CtrlDAction, WorkerFailureSignal, ACTIVE_POLL_TIMEOUT,
    GIT_BRANCH_POLL_INTERVAL, IDLE_POLL_TIMEOUT, UI_WATCHDOG_CHECK_INTERVAL,
};
use pir::tui::theme::{theme_watch_timeout, ThemeWatcherPlan};

// ---------------------------------------------------------------------------
// F2: a worker-level error ends the "working" state
// ---------------------------------------------------------------------------

/// A worker-level failure (no model configured, session construction failed)
/// emits the error notice AND the run-end signal; without the run-end signal
/// the spinner armed by `submit_prompt` spins forever behind the notice — the
/// play p01 screens show it still animating (`⠏` at one snapshot, `⠹` at the
/// next) after `Error: No pi model configured…`.
#[test]
fn f2_worker_level_failure_ends_the_run_state() {
    assert_eq!(
        worker_level_failure_signals(),
        [WorkerFailureSignal::Error, WorkerFailureSignal::RunEnd],
        "the error notice must be followed by the run-end signal"
    );
    // The run-end signal is `WorkerEvent::Idle`, whose handler clears the
    // working label (plus the retry countdown and the pending queues).
    assert_ne!(
        worker_level_failure_signals()[1],
        WorkerFailureSignal::Error,
        "the last signal must be the run-end one, not another error"
    );
}

/// The receive-side rule: an error that arrives with no run in flight ends the
/// run state; an error raised inside a run leaves the reset to that run's own
/// `Idle`, so a mid-turn error cannot hide the spinner of a live turn.
#[test]
fn f2_error_at_idle_ends_the_run_state() {
    assert!(
        worker_error_ends_run_state(false),
        "an error with no run in flight ends the run state"
    );
    assert!(
        !worker_error_ends_run_state(true),
        "an error inside a run is ended by the run's own Idle"
    );
}

// ---------------------------------------------------------------------------
// F3: ctrl+d at the editor (exit when empty, delete-char-forward otherwise)
// ---------------------------------------------------------------------------

/// Upstream `custom-editor.ts:117-125` verbatim: empty editor → exit;
/// otherwise the editor's own binding handles the key.
#[test]
fn f3_ctrl_d_decision_matches_upstream() {
    assert_eq!(ctrl_d_action(true), CtrlDAction::Exit);
    assert_eq!(ctrl_d_action(false), CtrlDAction::DeleteCharForward);
}

/// The decision reads the real editor state and the editor arm does not clear
/// the text: with `abc` and the cursor at the end, delete-char-forward is a
/// no-op (nothing after the cursor) and the text survives — ctrl+d never wipes
/// a draft (clearing is ctrl+c's job, `app.clear`).
#[test]
fn f3_ctrl_d_with_text_keeps_the_draft() {
    let editor = Editor::new();
    assert_eq!(
        ctrl_d_action(editor.get_text().is_empty()),
        CtrlDAction::Exit
    );

    editor.set_text("abc");
    editor.move_cursor_end();
    assert_eq!(
        ctrl_d_action(editor.get_text().is_empty()),
        CtrlDAction::DeleteCharForward
    );
    editor.delete_char_forward();
    assert_eq!(
        editor.get_text(),
        "abc",
        "delete-char-forward at the end of the line deletes nothing"
    );

    // With the cursor before the text it deletes one character — still not a
    // clear.
    editor.move_cursor_home();
    editor.delete_char_forward();
    assert_eq!(editor.get_text(), "bc");

    // An emptied editor is the exit case again.
    editor.set_text("");
    assert_eq!(
        ctrl_d_action(editor.get_text().is_empty()),
        CtrlDAction::Exit
    );
}

// ---------------------------------------------------------------------------
// F4: one abort produces one visible error
// ---------------------------------------------------------------------------

/// The abort the play lane hit: the provider's abort builds the terminal
/// message with `error_message = "Request was aborted"` and (through
/// `ai::provider::error_message`) `stop_reason = Error`. The settled assistant
/// entry already renders that error, so the run-level notice must stay silent;
/// before the fix this emitted the same row a second time.
#[test]
fn f4_one_abort_one_error_row() {
    assert_eq!(
        settled_run_error_notice(true, StopReason::Error, Some("Request was aborted")),
        None,
        "an aborted run must not add a second error notice"
    );
    assert_eq!(
        settled_run_error_notice(true, StopReason::Aborted, Some("Request was aborted")),
        None
    );
}

/// The non-abort paths keep their single notice, and the notice is only
/// emitted for an actual error stop with a message.
#[test]
fn f4_real_error_keeps_its_single_notice() {
    assert_eq!(
        settled_run_error_notice(false, StopReason::Error, Some("provider exploded")),
        Some("provider exploded".to_string())
    );
    assert_eq!(
        settled_run_error_notice(false, StopReason::Error, None),
        None,
        "an error stop without a message has nothing to report"
    );
    assert_eq!(
        settled_run_error_notice(false, StopReason::Stop, Some("ignored")),
        None,
        "a settled turn is not an error"
    );
    assert_eq!(
        settled_run_error_notice(false, StopReason::ToolUse, None),
        None
    );
}

// ---------------------------------------------------------------------------
// Idle poll cadence: the loop's next-tick timeout
// ---------------------------------------------------------------------------

/// The idle-CPU lane's loop decision: with work in flight the poll keeps the
/// 16 ms frame cadence (streaming deltas must reach the screen at frame rate
/// and the optimistic `Working` label is armed synchronously by
/// `submit_prompt`, so a new run never starts in the idle regime); with
/// nothing in flight the loop sleeps an order of magnitude longer instead of
/// waking at the frame cadence. Input is unaffected either way — `event::poll`
/// returns the moment a key/mouse/resize record is available — so the idle
/// wait only bounds how late a worker message / extension notification /
/// `ui_dirty` signal can be noticed while the session is otherwise quiet.
#[test]
fn idle_poll_cadence_keeps_the_frame_rate_only_while_work_is_in_flight() {
    assert_eq!(
        event_loop_poll_timeout(true, None),
        ACTIVE_POLL_TIMEOUT,
        "a run/dialog/countdown keeps the frame cadence"
    );
    assert_eq!(
        event_loop_poll_timeout(false, None),
        IDLE_POLL_TIMEOUT,
        "an idle loop sleeps the idle interval"
    );
    assert!(
        IDLE_POLL_TIMEOUT > ACTIVE_POLL_TIMEOUT,
        "the idle sleep must actually be longer than a frame: idle={IDLE_POLL_TIMEOUT:?}, \
         active={ACTIVE_POLL_TIMEOUT:?}"
    );
    // The low-resource budget: on a 1-2 core device that runs for days, an
    // idle loop may not wake (and sweep every lock/channel) more than twice
    // per second.
    assert!(
        IDLE_POLL_TIMEOUT > Duration::from_millis(500),
        "the idle loop must wake fewer than twice per second: idle={IDLE_POLL_TIMEOUT:?}"
    );
    assert_eq!(ACTIVE_POLL_TIMEOUT, Duration::from_millis(16));
}

/// The low-resource budget is PROCESS-WIDE, not just the event loop: the
/// git-branch watcher, the UI-liveness watchdog and (with a custom theme) the
/// theme watcher each add a periodic timer. Counting only the loop's sleep
/// made the old 750 ms cadence look compliant (1.33/s) while the process
/// actually woke at >2/s before the theme watcher even started.
#[test]
fn process_wide_idle_wakeup_budget_stays_under_two_per_second() {
    let theme_idle = theme_watch_timeout(&ThemeWatcherPlan::default(), Instant::now());
    let per_second = 1.0 / IDLE_POLL_TIMEOUT.as_secs_f64()
        + 1.0 / GIT_BRANCH_POLL_INTERVAL.as_secs_f64()
        + 1.0 / UI_WATCHDOG_CHECK_INTERVAL.as_secs_f64()
        + 1.0 / theme_idle.as_secs_f64();
    assert!(
        per_second < 2.0,
        "idle wakeups are {per_second:.2}/s (loop {:?}, git {:?}, watchdog {:?}, theme {:?}; budget: < 2/s)",
        IDLE_POLL_TIMEOUT,
        GIT_BRANCH_POLL_INTERVAL,
        UI_WATCHDOG_CHECK_INTERVAL,
        theme_idle
    );
}

/// The input gate's pending sequence window always wins: a held ESC/sequence
/// tail must flush on its own deadline, so the idle sleep (or the frame
/// cadence) is clamped to the remaining window — including an already-expired
/// deadline, which polls without waiting so the next tick flushes it.
#[test]
fn idle_poll_cadence_is_clamped_by_the_gates_sequence_window() {
    let window = Duration::from_millis(5);
    assert_eq!(
        event_loop_poll_timeout(true, Some(window)),
        window,
        "a pending sequence window beats the frame cadence"
    );
    assert_eq!(
        event_loop_poll_timeout(false, Some(window)),
        window,
        "a pending sequence window beats the idle sleep"
    );
    assert_eq!(
        event_loop_poll_timeout(false, Some(Duration::ZERO)),
        Duration::ZERO,
        "an expired window polls without waiting"
    );
    assert_eq!(
        event_loop_poll_timeout(false, Some(Duration::from_secs(30))),
        IDLE_POLL_TIMEOUT,
        "a longer window does not extend the sleep"
    );
}

// ---------------------------------------------------------------------------
// First-token window: the optimistic `Working` label is the loop's in-flight
// state before the first delta
// ---------------------------------------------------------------------------

/// `prompt_arms_working_label` is the shared submission rule behind the
/// editor's Enter AND the positional startup prompts (`pir "…"`): a submission
/// to a live worker with no run in flight arms the optimistic `Working`
/// label, which is what keeps the loop's `work_in_flight` state true for the
/// WHOLE first-token window — worker boot, session build, the request,
/// time-to-first-token. Without it that window runs in the idle regime
/// (`IDLE_POLL_TIMEOUT`) and each of the run's own events — the `Working` row,
/// then the first deltas — waits out its own idle sleep, which is exactly the
/// "nothing moves for a long time at the start of a turn" lag.
/// A steer/follow-up keeps the live run's label (it is not a new
/// submission), and a dead worker (degraded headless UI) never arms one —
/// its failure path reports the error and clears the state (F2).
#[test]
fn prompt_submission_arms_the_first_token_window() {
    assert!(
        prompt_arms_working_label(true, false),
        "a fresh prompt on a live worker must arm the optimistic Working label"
    );
    assert!(
        !prompt_arms_working_label(false, false),
        "a degraded (dead) worker must not arm a label nothing can clear"
    );
    assert!(
        !prompt_arms_working_label(true, true),
        "a steer/follow-up must keep the live run's label, not re-arm it"
    );
    // The armed label is the in-flight state the cadence reads: with it the
    // loop polls at the frame cadence for the whole first-token window
    // instead of sleeping the idle interval.
    assert_eq!(event_loop_poll_timeout(true, None), ACTIVE_POLL_TIMEOUT);
    assert_ne!(
        event_loop_poll_timeout(true, None),
        event_loop_poll_timeout(false, None),
        "the first-token window must not run in the idle regime"
    );
}

// ---------------------------------------------------------------------------
// The interactive session's starting thinking level
// ---------------------------------------------------------------------------

/// `initial_interactive_thinking` is the level a fresh interactive session
/// asks the provider for on every request. It must be the user's
/// `settings.defaultThinkingLevel` — the interactive path used to hard-code
/// `Medium`, so a session configured for `max` sent
/// `reasoning_effort: "medium"` (measured in the request body) while the
/// footer displayed `max`: the level the user chose never reached the
/// provider, and `medium` is a level some models' `thinkingLevelMap` does not
/// define at all (no reasoning stream). `off` is not a starting level
/// upstream either (the resolver filters it), so it falls back to the shared
/// default.
#[test]
fn interactive_session_starts_at_the_settings_thinking_level() {
    use pir::ai::types::ThinkingLevel;
    assert_eq!(
        initial_interactive_thinking(Some(ThinkingLevel::Max)),
        ThinkingLevel::Max,
        "defaultThinkingLevel=max must reach the provider"
    );
    assert_eq!(
        initial_interactive_thinking(Some(ThinkingLevel::High)),
        ThinkingLevel::High
    );
    assert_eq!(
        initial_interactive_thinking(Some(ThinkingLevel::Low)),
        ThinkingLevel::Low
    );
    assert_eq!(
        initial_interactive_thinking(None),
        ThinkingLevel::Medium,
        "no setting keeps the shared default"
    );
    assert_eq!(
        initial_interactive_thinking(Some(ThinkingLevel::Off)),
        ThinkingLevel::Medium,
        "`off` is not a starting level (upstream filters it out)"
    );
}
