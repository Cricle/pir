//! Same-window session tabs: the pure tab-list / draft / title model and the
//! `/sessions` argument grammar. The live attach path is covered by the
//! ConPTY scenario `scripts/tui_conpty/scenarios/s35_sessions_switch.py`; the
//! tests here exercise every decision that path makes without a terminal.

use pir::tui::app::{
    activate_tab, active_tab_index, add_or_activate_tab, close_tab, cycle_tab, save_active_draft,
    session_tab_indicator, session_tab_title, set_active_title, take_active_draft, NoticeKind,
    SessionTab,
};
use pir::tui::slash::{
    builtin_action, parse_slash_command, sessions_argument, SessionsArgument, SlashAction,
};
use pir::tui::terminal::{
    refresh_active_tab_title, session_switch_failed_effect, session_switch_opened_effect,
    SessionSwitchGate,
};

fn tab(path: &str, title: &str) -> SessionTab {
    SessionTab {
        path: path.to_string(),
        title: title.to_string(),
        draft: String::new(),
    }
}

// ---------------------------------------------------------------------------
// Title precedence: session name > first user message preview > file stem
// ---------------------------------------------------------------------------

#[test]
fn tab_title_prefers_session_name_then_first_message_then_stem() {
    let path = "/sessions/2026-01-01T00-00-00-000Z_abc.jsonl";
    assert_eq!(
        session_tab_title(Some("named session"), Some("hello there"), path),
        "named session"
    );
    // A blank name is no name.
    assert_eq!(
        session_tab_title(Some("   "), Some("hello there"), path),
        "hello there"
    );
    assert_eq!(
        session_tab_title(None, Some("hello there"), path),
        "hello there"
    );
    assert_eq!(
        session_tab_title(None, None, path),
        "2026-01-01T00-00-00-000Z_abc"
    );
    // An all-blank message falls through to the stem too.
    assert_eq!(
        session_tab_title(None, Some("\n  \n"), path),
        "2026-01-01T00-00-00-000Z_abc"
    );
}

#[test]
fn tab_title_preview_is_first_line_whitespace_collapsed_and_capped() {
    assert_eq!(
        session_tab_title(
            None,
            Some("\n\n  fix   the\tparser \nsecond line"),
            "/s/x.jsonl"
        ),
        "fix the parser"
    );
    let long = "x".repeat(120);
    let title = session_tab_title(None, Some(&long), "/s/x.jsonl");
    assert_eq!(title.chars().count(), 60);
    assert!(title.ends_with('…'));
}

#[test]
fn tab_title_uses_the_full_path_when_it_has_no_stem() {
    assert_eq!(session_tab_title(None, None, ""), "");
}

// ---------------------------------------------------------------------------
// First-prompt title refresh (a fresh tab's stem -> prompt preview)
// ---------------------------------------------------------------------------

#[test]
fn first_prompt_replaces_a_fresh_stem_title() {
    let path = "/sessions/2026-01-01T00-00-00-000Z_abc.jsonl";
    let mut tabs = vec![tab(path, "2026-01-01T00-00-00-000Z_abc")];
    assert!(refresh_active_tab_title(
        &mut tabs,
        Some(path),
        None,
        Some("\n  fix   the\tparser \nsecond line"),
    ));
    assert_eq!(tabs[0].title, "fix the parser");
}

#[test]
fn title_refresh_does_not_fire_when_the_computed_title_is_unchanged() {
    let path = "/s/x.jsonl";
    let mut tabs = vec![tab(path, "x")];
    // The preview collapses to exactly the stem: the computed title equals
    // the current one, so no rewrite (and no footer repaint) happens.
    assert!(!refresh_active_tab_title(
        &mut tabs,
        Some(path),
        None,
        Some("x")
    ));
    assert_eq!(tabs[0].title, "x");

    // The first real preview changes the title once; repeating it (or a
    // later prompt) must not fire another update.
    assert!(refresh_active_tab_title(
        &mut tabs,
        Some(path),
        None,
        Some("hello")
    ));
    assert!(!refresh_active_tab_title(
        &mut tabs,
        Some(path),
        None,
        Some("hello")
    ));
    assert!(!refresh_active_tab_title(
        &mut tabs,
        Some(path),
        None,
        Some("later prompt")
    ));
    assert_eq!(tabs[0].title, "hello");
}

#[test]
fn title_refresh_ignores_tabs_that_are_not_active() {
    let mut tabs = vec![tab("/a.jsonl", "a"), tab("/b.jsonl", "b")];
    assert!(!refresh_active_tab_title(
        &mut tabs,
        None,
        None,
        Some("hello")
    ));
    assert!(!refresh_active_tab_title(
        &mut tabs,
        Some("/missing.jsonl"),
        None,
        Some("hello")
    ));
    assert_eq!(tabs[0].title, "a");
    assert_eq!(tabs[1].title, "b");
}

// ---------------------------------------------------------------------------
// Adding / activating / deduplicating tabs
// ---------------------------------------------------------------------------

#[test]
fn add_or_activate_tab_adds_then_dedupes_by_path() {
    let mut tabs = Vec::new();
    let mut active = None;
    let first = add_or_activate_tab(&mut tabs, &mut active, "/a.jsonl", "A");
    assert_eq!(first, 0);
    assert_eq!(tabs.len(), 1);
    assert_eq!(active.as_deref(), Some("/a.jsonl"));

    let second = add_or_activate_tab(&mut tabs, &mut active, "/b.jsonl", "B");
    assert_eq!(second, 1);
    assert_eq!(tabs.len(), 2);
    assert_eq!(active.as_deref(), Some("/b.jsonl"));

    // Re-opening the same path activates the existing slot (no duplicate)
    // and refreshes its title.
    let again = add_or_activate_tab(&mut tabs, &mut active, "/a.jsonl", "A renamed");
    assert_eq!(again, 0);
    assert_eq!(tabs.len(), 2);
    assert_eq!(active.as_deref(), Some("/a.jsonl"));
    assert_eq!(tabs[0].title, "A renamed");
    assert_eq!(tabs[1].title, "B");
}

#[test]
fn add_or_activate_tab_preserves_an_existing_draft() {
    let mut tabs = vec![tab("/a.jsonl", "A"), tab("/b.jsonl", "B")];
    let mut active = Some("/b.jsonl".to_string());
    save_active_draft(&mut tabs, active.as_deref(), "unsent text");
    let index = add_or_activate_tab(&mut tabs, &mut active, "/b.jsonl", "B renamed");
    assert_eq!(index, 1);
    assert_eq!(tabs[1].draft, "unsent text");
    assert_eq!(tabs[1].title, "B renamed");
}

#[test]
fn activate_tab_switches_only_to_known_paths() {
    let tabs = vec![tab("/a.jsonl", "A"), tab("/b.jsonl", "B")];
    let mut active = Some("/a.jsonl".to_string());
    assert!(activate_tab(&tabs, &mut active, "/b.jsonl"));
    assert_eq!(active.as_deref(), Some("/b.jsonl"));
    assert!(!activate_tab(&tabs, &mut active, "/missing.jsonl"));
    assert_eq!(active.as_deref(), Some("/b.jsonl"));
    assert_eq!(active_tab_index(&tabs, active.as_deref()), Some(1));
    assert_eq!(active_tab_index(&tabs, Some("/missing.jsonl")), None);
}

#[test]
fn set_active_title_renames_only_the_active_tab() {
    let mut tabs = vec![tab("/a.jsonl", "A"), tab("/b.jsonl", "B")];
    set_active_title(&mut tabs, Some("/b.jsonl"), "renamed");
    assert_eq!(tabs[0].title, "A");
    assert_eq!(tabs[1].title, "renamed");
    // An empty name never blanks a title.
    set_active_title(&mut tabs, Some("/b.jsonl"), "");
    assert_eq!(tabs[1].title, "renamed");
}

// ---------------------------------------------------------------------------
// Draft save / restore
// ---------------------------------------------------------------------------

#[test]
fn drafts_survive_switching_away_and_back() {
    let mut tabs = Vec::new();
    let mut active = None;
    add_or_activate_tab(&mut tabs, &mut active, "/a.jsonl", "A");
    add_or_activate_tab(&mut tabs, &mut active, "/b.jsonl", "B");

    // A is active and owns the editor text; saving on switch captures it.
    add_or_activate_tab(&mut tabs, &mut active, "/a.jsonl", "A");
    save_active_draft(&mut tabs, active.as_deref(), "draft for A");
    assert_eq!(tabs[0].draft, "draft for A");

    // B becomes active; A's draft stays behind.
    add_or_activate_tab(&mut tabs, &mut active, "/b.jsonl", "B");
    assert_eq!(tabs[0].draft, "draft for A");
    assert_eq!(take_active_draft(&mut tabs, active.as_deref()), "");

    // Back to A: the draft is restored (and moved into the editor, so the
    // tab's slot is empty again — never two copies).
    add_or_activate_tab(&mut tabs, &mut active, "/a.jsonl", "A");
    assert_eq!(
        take_active_draft(&mut tabs, active.as_deref()),
        "draft for A"
    );
    assert_eq!(tabs[0].draft, "");

    // B's draft is independent.
    add_or_activate_tab(&mut tabs, &mut active, "/b.jsonl", "B");
    assert_eq!(take_active_draft(&mut tabs, active.as_deref()), "");
}

#[test]
fn saving_a_draft_without_an_active_tab_is_a_no_op() {
    let mut tabs = vec![tab("/a.jsonl", "A")];
    save_active_draft(&mut tabs, None, "text");
    assert_eq!(tabs[0].draft, "");
    assert_eq!(take_active_draft(&mut tabs, None), "");
}

// ---------------------------------------------------------------------------
// next / prev wraparound
// ---------------------------------------------------------------------------

#[test]
fn cycle_tab_wraps_forward_and_backward() {
    let tabs = vec![
        tab("/a.jsonl", "A"),
        tab("/b.jsonl", "B"),
        tab("/c.jsonl", "C"),
    ];
    assert_eq!(
        cycle_tab(&tabs, Some("/a.jsonl"), true).as_deref(),
        Some("/b.jsonl")
    );
    assert_eq!(
        cycle_tab(&tabs, Some("/c.jsonl"), true).as_deref(),
        Some("/a.jsonl")
    );
    assert_eq!(
        cycle_tab(&tabs, Some("/a.jsonl"), false).as_deref(),
        Some("/c.jsonl")
    );
    assert_eq!(
        cycle_tab(&tabs, Some("/b.jsonl"), false).as_deref(),
        Some("/a.jsonl")
    );
    // A single tab cycles to itself.
    let single = vec![tab("/a.jsonl", "A")];
    assert_eq!(
        cycle_tab(&single, Some("/a.jsonl"), true).as_deref(),
        Some("/a.jsonl")
    );
}

#[test]
fn cycle_tab_starts_at_an_end_when_no_tab_is_active() {
    let tabs = vec![
        tab("/a.jsonl", "A"),
        tab("/b.jsonl", "B"),
        tab("/c.jsonl", "C"),
    ];
    assert_eq!(cycle_tab(&tabs, None, true).as_deref(), Some("/a.jsonl"));
    assert_eq!(cycle_tab(&tabs, None, false).as_deref(), Some("/c.jsonl"));
    assert_eq!(
        cycle_tab(&tabs, Some("/missing.jsonl"), true).as_deref(),
        Some("/a.jsonl")
    );
    assert_eq!(cycle_tab(&[], None, true), None);
}

// ---------------------------------------------------------------------------
// Closing tabs
// ---------------------------------------------------------------------------

#[test]
fn closing_the_active_tab_falls_back_to_the_nearest_survivor() {
    // Middle tab: the tab that slid into its slot (the next one).
    let mut tabs = vec![
        tab("/a.jsonl", "A"),
        tab("/b.jsonl", "B"),
        tab("/c.jsonl", "C"),
    ];
    let mut active = Some("/b.jsonl".to_string());
    let next = close_tab(&mut tabs, &mut active, "/b.jsonl");
    assert_eq!(next.as_deref(), Some("/c.jsonl"));
    assert_eq!(active.as_deref(), Some("/c.jsonl"));
    assert_eq!(tabs.len(), 2);

    // First tab: the new first tab.
    let mut active = Some("/a.jsonl".to_string());
    let next = close_tab(&mut tabs, &mut active, "/a.jsonl");
    assert_eq!(next.as_deref(), Some("/c.jsonl"));
    assert_eq!(active.as_deref(), Some("/c.jsonl"));

    // Last tab: the previous one.
    let mut active = Some("/c.jsonl".to_string());
    let next = close_tab(&mut tabs, &mut active, "/c.jsonl");
    assert_eq!(next, None);
    assert_eq!(active, None);
    assert!(tabs.is_empty());
}

#[test]
fn closing_a_non_active_tab_keeps_the_active_one() {
    let mut tabs = vec![tab("/a.jsonl", "A"), tab("/b.jsonl", "B")];
    let mut active = Some("/b.jsonl".to_string());
    let next = close_tab(&mut tabs, &mut active, "/a.jsonl");
    assert_eq!(next.as_deref(), Some("/b.jsonl"));
    assert_eq!(active.as_deref(), Some("/b.jsonl"));
    assert_eq!(tabs.len(), 1);
}

#[test]
fn closing_an_unknown_path_keeps_the_active_tab() {
    let mut tabs = vec![tab("/a.jsonl", "A")];
    let mut active = Some("/a.jsonl".to_string());
    let next = close_tab(&mut tabs, &mut active, "/missing.jsonl");
    assert_eq!(next.as_deref(), Some("/a.jsonl"));
    assert_eq!(tabs.len(), 1);
}

// ---------------------------------------------------------------------------
// Footer indicator
// ---------------------------------------------------------------------------

#[test]
fn tab_indicator_shows_position_and_title_with_two_or_more_tabs() {
    let one = vec![tab("/a.jsonl", "A")];
    assert_eq!(session_tab_indicator(&one, Some("/a.jsonl")), None);
    assert_eq!(session_tab_indicator(&[], None), None);

    let tabs = vec![tab("/a.jsonl", "A"), tab("/b.jsonl", "B")];
    assert_eq!(
        session_tab_indicator(&tabs, Some("/b.jsonl")),
        Some("[2/2] B".to_string())
    );
    assert_eq!(
        session_tab_indicator(&tabs, Some("/a.jsonl")),
        Some("[1/2] A".to_string())
    );
    // A missing active path renders the first tab rather than no marker.
    assert_eq!(
        session_tab_indicator(&tabs, None),
        Some("[1/2] A".to_string())
    );
}

// ---------------------------------------------------------------------------
// `/sessions` routing + argument grammar
// ---------------------------------------------------------------------------

#[test]
fn sessions_is_a_builtin_slash_command() {
    assert_eq!(builtin_action("sessions"), Some(SlashAction::Sessions));
    // The singular command still routes to session info.
    assert_eq!(builtin_action("session"), Some(SlashAction::Session));

    let invocation = parse_slash_command("/sessions next").expect("parsed");
    assert_eq!(invocation.name, "sessions");
    assert_eq!(invocation.args, "next");
}

#[test]
fn sessions_is_in_the_autocomplete_registry_with_a_description() {
    let commands = pir::tui::autocomplete::builtin_commands();
    let sessions = commands
        .iter()
        .find(|command| command.name == "sessions")
        .expect("/sessions must be offered by the command autocomplete");
    assert!(
        sessions
            .description
            .as_deref()
            .is_some_and(|description| !description.trim().is_empty()),
        "the /sessions autocomplete row needs a description: {sessions:?}"
    );
    assert_eq!(
        sessions.argument_hint.as_deref(),
        Some("[new|next|prev|close|<index>|<path>]")
    );
}

#[test]
fn sessions_argument_grammar() {
    assert_eq!(sessions_argument(""), SessionsArgument::Picker);
    assert_eq!(sessions_argument("   "), SessionsArgument::Picker);
    assert_eq!(sessions_argument("new"), SessionsArgument::New);
    assert_eq!(sessions_argument("NEXT"), SessionsArgument::Next);
    assert_eq!(sessions_argument("prev"), SessionsArgument::Prev);
    assert_eq!(sessions_argument("previous"), SessionsArgument::Prev);
    assert_eq!(sessions_argument("close"), SessionsArgument::Close);

    assert_eq!(sessions_argument("1"), SessionsArgument::Index(1));
    assert_eq!(sessions_argument("12"), SessionsArgument::Index(12));
    assert_eq!(sessions_argument("0"), SessionsArgument::Index(0));

    assert_eq!(
        sessions_argument("notes.jsonl"),
        SessionsArgument::Path("notes.jsonl")
    );
    // A quoted path keeps its inner spaces and loses the quotes.
    assert_eq!(
        sessions_argument("\"my notes.jsonl\""),
        SessionsArgument::Path("my notes.jsonl")
    );
    assert_eq!(
        sessions_argument("'my notes.jsonl'"),
        SessionsArgument::Path("my notes.jsonl")
    );
    // Unquoted paths with spaces keep the whole remaining argument.
    assert_eq!(
        sessions_argument("dir/my notes.jsonl"),
        SessionsArgument::Path("dir/my notes.jsonl")
    );
}

// ---------------------------------------------------------------------------
// Worker-side switch open: the transcript-reset contract
//
// The tab switch hands the session FILE to the worker (`OpenSessionPath`) so
// a large session's parse never blocks the UI thread. These tests pin the
// reply's transcript-side contract: the outgoing transcript is reset ONLY
// once the target opened, the abort warning reflects the run state captured
// when the switch was REQUESTED, and a failed open leaves the outgoing
// session on screen.
// ---------------------------------------------------------------------------

#[test]
fn switch_open_resets_and_warns_when_a_run_was_streaming() {
    let effect = session_switch_opened_effect("/s/b.jsonl", true);
    assert!(
        effect.reset_transcript,
        "an opened target commits the switch (reset the outgoing transcript)"
    );
    assert_eq!(
        effect.notices,
        vec![
            (
                NoticeKind::Warning,
                "Switching aborts the current run".to_string()
            ),
            (
                NoticeKind::Status,
                "Switching session: /s/b.jsonl".to_string()
            ),
        ]
    );
}

#[test]
fn switch_open_without_a_run_resets_with_only_the_status_notice() {
    let effect = session_switch_opened_effect("/s/b.jsonl", false);
    assert!(effect.reset_transcript);
    assert_eq!(
        effect.notices,
        vec![(
            NoticeKind::Status,
            "Switching session: /s/b.jsonl".to_string()
        )]
    );
}

#[test]
fn switch_open_failure_keeps_the_outgoing_transcript() {
    let effect = session_switch_failed_effect("Session file is not a valid pi session: /s/x");
    assert!(
        !effect.reset_transcript,
        "a failed open must not wipe the outgoing transcript/view state"
    );
    assert_eq!(
        effect.notices,
        vec![(
            NoticeKind::Error,
            "cannot open session: Session file is not a valid pi session: /s/x".to_string()
        )]
    );
}

// ---------------------------------------------------------------------------
// P1-A: stale events from the outgoing run must not reach the reset
// transcript while a switch waits for the worker's acknowledgement.
// ---------------------------------------------------------------------------

#[test]
fn switch_gate_drops_outgoing_run_events_until_acknowledged() {
    let mut gate = SessionSwitchGate::default();
    assert!(!gate.is_pending(), "no switch in flight");
    assert!(!gate.drops(true), "idle gate passes everything");

    let epoch = gate.begin();
    assert!(gate.is_pending());
    // Stream/assistant/tool/usage/queue events belong to the aborted run.
    assert!(gate.drops(true));
    // Replay/attach/lifecycle events still flow (the reset transcript needs
    // the incoming session's replay).
    assert!(!gate.drops(false));

    gate.settle(epoch);
    assert!(!gate.is_pending(), "the worker's ack opens the gate");
    assert!(!gate.drops(true), "the attached run's events apply again");
}

#[test]
fn switch_gate_an_old_ack_cannot_open_a_newer_switch() {
    let mut gate = SessionSwitchGate::default();
    let first = gate.begin();
    let second = gate.begin();
    gate.settle(first);
    assert!(
        gate.is_pending(),
        "the second switch is still awaiting its own ack"
    );
    assert!(gate.drops(true));
    gate.settle(second);
    assert!(!gate.is_pending());
}

#[test]
fn switch_gate_an_abandoned_switch_is_settled_by_a_later_ack() {
    let mut gate = SessionSwitchGate::default();
    // A `/tree` selection whose summarize prompt was abandoned never gets an
    // ack of its own; the next switch's ack must clear the gate anyway.
    let _abandoned = gate.begin();
    assert!(gate.is_pending());
    let latest = gate.begin();
    gate.settle(latest);
    assert!(
        !gate.is_pending(),
        "settling the latest epoch also retires earlier abandoned requests"
    );
    // A stale/unknown ack never opens a newer switch.
    let newer = gate.begin();
    gate.settle(latest);
    assert!(gate.is_pending());
    gate.settle(newer);
    assert!(!gate.is_pending());
}
