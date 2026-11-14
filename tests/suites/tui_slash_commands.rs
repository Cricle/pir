//! Built-in slash commands (item 2 of the port stream).
//!
//! Mirrors `packages/coding-agent/src/core/slash-commands.ts`
//! (`BUILTIN_SLASH_COMMANDS`) and the interactive dispatch in
//! `modes/interactive/interactive-mode.ts` (`handleSlashCommand`,
//! `handleCopyCommand`, `handleNameCommand`).

use std::sync::Arc;

use pir::ai::types::{AssistantMessage, ContentBlock, TextContent};
use pir::diagnostics::{
    RunDiagnostics, RunTotals, ToolCallDiagnostics, TurnDiagnostics, UsageDiagnostics,
};
use pir::tui::app::ChatEntry;
use pir::tui::autocomplete::builtin_commands;
use pir::tui::slash::{
    action_quits, builtin_action, copy_last_assistant_message, debug_command_argument,
    exact_match_only, last_assistant_text, name_argument, name_display, parse_slash_command,
    run_diagnostics_text, CopyOutcome, DebugArgument, SlashAction, BUILTIN_ACTION_NAMES,
    EXACT_MATCH_ONLY_COMMANDS, EXTRA_BUILTIN_COMMAND_NAMES,
};

// ---------------------------------------------------------------------------
// Parsing (upstream splits the command from its arguments on the first space)
// ---------------------------------------------------------------------------

#[test]
fn parse_slash_command_splits_name_and_arguments() {
    let parsed = parse_slash_command("/name my session").expect("parsed");
    assert_eq!(parsed.name, "name");
    assert_eq!(parsed.args, "my session");

    let parsed = parse_slash_command("/quit").expect("parsed");
    assert_eq!(parsed.name, "quit");
    assert_eq!(parsed.args, "");

    // Extra whitespace is trimmed off the argument.
    let parsed = parse_slash_command("/name    spaced   ").expect("parsed");
    assert_eq!(parsed.args, "spaced");
}

#[test]
fn parse_slash_command_rejects_non_commands() {
    assert!(parse_slash_command("hello").is_none());
    assert!(parse_slash_command("/").is_none());
    assert!(parse_slash_command("").is_none());
    // `/  x` has an empty name.
    assert!(parse_slash_command("/  x").is_none());
}

// ---------------------------------------------------------------------------
// Routing (upstream `BUILTIN_SLASH_COMMANDS` dispatch)
// ---------------------------------------------------------------------------

#[test]
fn every_upstream_required_command_routes_to_an_action() {
    // The nine commands this stream adds, plus the pre-existing set.
    let expected = [
        ("settings", SlashAction::Settings),
        ("model", SlashAction::Model),
        ("thinking", SlashAction::Thinking),
        ("copy", SlashAction::Copy),
        ("name", SlashAction::Name),
        ("session", SlashAction::Session),
        ("fork", SlashAction::Fork),
        ("clone", SlashAction::Clone),
        ("new", SlashAction::New),
        ("compact", SlashAction::Compact),
        ("resume", SlashAction::Resume),
        ("clear", SlashAction::Clear),
        ("quit", SlashAction::Quit),
        ("help", SlashAction::Help),
    ];
    for (name, action) in expected {
        assert_eq!(builtin_action(name), Some(action), "routing for /{name}");
    }
}

#[test]
fn non_builtin_names_fall_through_to_the_extension_path() {
    // Upstream's editor submit handler checks its built-in texts and then
    // falls through: extension-registered names execute through the session
    // (`prompt()` → `_tryExecuteExtensionCommand`), and every other name is
    // submitted as a NORMAL PROMPT — upstream prints no "unknown command"
    // notice (the `/skill:<name>` case is pinned in `tui_regressions`).
    // tree/hotkeys/changelog ARE built-in upstream (BUILTIN_SLASH_COMMANDS)
    // and pir routes them too; share/import/bug are implemented as built-ins
    // as well now. Only UNKNOWN names reach the fall-through.
    for name in ["share", "bug", "import", "tree", "hotkeys", "changelog"] {
        assert!(builtin_action(name).is_some(), "/{name} is built-in");
    }
    assert_eq!(builtin_action("nope"), None, "/nope must not be built-in");
}

#[test]
fn only_quit_ends_the_event_loop() {
    assert!(action_quits(SlashAction::Quit));
    for action in [
        SlashAction::Settings,
        SlashAction::Model,
        SlashAction::Thinking,
        SlashAction::Copy,
        SlashAction::Name,
        SlashAction::Session,
        SlashAction::Fork,
        SlashAction::Clone,
        SlashAction::New,
        SlashAction::Compact,
        SlashAction::Resume,
        SlashAction::Clear,
        SlashAction::Help,
    ] {
        assert!(!action_quits(action), "{action:?} must not quit");
    }
}

#[test]
fn autocomplete_exposes_every_routable_builtin() {
    let listed: Vec<String> = builtin_commands()
        .into_iter()
        .map(|command| command.name)
        .collect();
    for name in [
        "settings", "model", "thinking", "copy", "name", "session", "fork", "clone", "new",
        "compact", "export", "resume", "clear", "quit", "help",
    ] {
        assert!(
            listed.iter().any(|entry| entry == name),
            "autocomplete is missing /{name}: {listed:?}"
        );
        assert!(
            builtin_action(name).is_some(),
            "/{name} is listed but not routable"
        );
    }
}

/// The autocomplete list must equal the set of commands pir can execute.
///
/// Both directions: every name the dispatcher routes (`BUILTIN_ACTION_NAMES`,
/// plus the pir-only non-action commands such as `/image`) appears in the
/// popup, and the popup offers nothing that is not executable. This is the
/// regression for "typing `/` shows only ~30 commands and important builtins
/// are missing".
#[test]
fn autocomplete_matches_the_executable_builtin_set_exactly() {
    let listed: Vec<String> = builtin_commands()
        .into_iter()
        .map(|command| command.name)
        .collect();

    for name in BUILTIN_ACTION_NAMES {
        assert!(
            builtin_action(name).is_some(),
            "/{name} is in BUILTIN_ACTION_NAMES but does not route"
        );
        assert!(
            listed.iter().any(|entry| entry == name),
            "autocomplete is missing the routed builtin /{name}: {listed:?}"
        );
    }
    for name in EXTRA_BUILTIN_COMMAND_NAMES {
        assert!(
            listed.iter().any(|entry| entry == name),
            "autocomplete is missing the handled builtin /{name}: {listed:?}"
        );
    }

    // No phantom entries: every listed name is executable by one of the two
    // dispatcher faces.
    for name in &listed {
        assert!(
            builtin_action(name).is_some() || EXTRA_BUILTIN_COMMAND_NAMES.contains(&name.as_str()),
            "autocomplete offers /{name}, but no handler executes it"
        );
    }

    // `/image` is the pir-only command that used to be missing; pin it
    // explicitly so a future list rewrite cannot drop it silently.
    assert!(listed.iter().any(|entry| entry == "image"));
}

// ---------------------------------------------------------------------------
// /name (upstream `handleNameCommand`)
// ---------------------------------------------------------------------------

#[test]
fn name_argument_requires_a_non_empty_value() {
    assert_eq!(name_argument("my session"), Ok("my session".to_string()));
    assert_eq!(name_argument("  padded  "), Ok("padded".to_string()));
    assert_eq!(name_argument(""), Err("Usage: /name <name>".to_string()));
    assert_eq!(name_argument("   "), Err("Usage: /name <name>".to_string()));
}

#[test]
fn bare_name_shows_the_current_session_name() {
    // `/name [name]` upstream (`handleNameCommand`, interactive-mode.ts
    // :6664-6690): with no argument it prints the CURRENT name; only an
    // unnamed session falls back to the usage warning.
    assert_eq!(
        name_display(Some("refactor auth")),
        Ok("Session name: refactor auth".to_string())
    );
    assert_eq!(
        name_display(Some("  padded  ")),
        Ok("Session name: padded".to_string())
    );
    assert_eq!(name_display(None), Err("Usage: /name <name>".to_string()));
    assert_eq!(
        name_display(Some("   ")),
        Err("Usage: /name <name>".to_string())
    );
}

// ---------------------------------------------------------------------------
// Trailing arguments on upstream's exact-match-only commands (PIR-BIN-089 #3)
// ---------------------------------------------------------------------------

#[test]
fn exact_match_only_covers_the_upstream_exact_dispatch_set() {
    // Upstream's editor submit handler checks these with `text === "/<name>"`
    // (interactive-mode.ts:3161-3300): a trailing argument makes the text a
    // normal prompt there. `arminsayshi` / `dementedelves` are pi easter eggs
    // pir does not ship.
    let expected = [
        "settings",
        "tree",
        "scoped-models",
        "share",
        "session",
        "changelog",
        "hotkeys",
        "fork",
        "clone",
        "trust",
        "logout",
        "new",
        "resume",
        "reload",
        "quit",
    ];
    assert_eq!(EXACT_MATCH_ONLY_COMMANDS, &expected[..]);
    for name in expected {
        assert!(
            exact_match_only(name),
            "/{name} must fall through with args"
        );
        assert!(
            builtin_action(name).is_some(),
            "/{name} must still route when bare"
        );
    }
    // `/copy` and `/debug` are upstream-exact but pir documents argument
    // forms for them: they must keep executing those arguments.
    for name in ["copy", "debug"] {
        assert!(
            !exact_match_only(name),
            "/{name} is a pir argument superset"
        );
    }
    // Upstream's prefix-matched commands keep their argument; so do the
    // pir-only routable names.
    for name in [
        "model",
        "thinking",
        "export",
        "import",
        "bug",
        "name",
        "login",
        "compact",
        "clear",
        "sessions",
        "help",
        "image",
        "arminsayshi",
        "dementedelves",
        "llama",
        "nope",
    ] {
        assert!(
            !exact_match_only(name),
            "/{name} must not be in the exact-match set"
        );
    }
}

// ---------------------------------------------------------------------------
// Popup rows (upstream `BUILTIN_SLASH_COMMANDS`, PIR-BIN-089 #4)
// ---------------------------------------------------------------------------

#[test]
fn popup_rows_match_upstream_descriptions_and_hints() {
    // Upstream `core/slash-commands.ts` strings for every shared row that
    // used to drift; the five that understated pir's own behavior
    // (settings/scoped-models/trust/reload/export) now say what the
    // dispatcher really does.
    let commands = builtin_commands();
    let find_row = |name: &str| {
        commands
            .iter()
            .find(|command| command.name == name)
            .unwrap_or_else(|| panic!("the /{name} popup row is missing"))
    };
    let expected: [(&str, &str, Option<&str>); 14] = [
        ("settings", "Open settings menu", None),
        (
            "model",
            "Select model (opens selector UI)",
            Some("<provider/model>"),
        ),
        ("tree", "Navigate session tree (switch branches)", None),
        ("thinking", "Set thinking level", Some("<level>")),
        (
            "scoped-models",
            "Enable/disable models for Ctrl+P cycling",
            None,
        ),
        (
            "export",
            "Export session (HTML default, or specify path: .html/.jsonl)",
            Some("<path>"),
        ),
        ("changelog", "Show changelog entries", None),
        ("hotkeys", "Show all keyboard shortcuts", None),
        (
            "fork",
            "Create a new fork from a previous user message",
            None,
        ),
        (
            "clone",
            "Duplicate the current session at the current position",
            None,
        ),
        (
            "trust",
            "Save project trust decision for future sessions",
            None,
        ),
        (
            "login",
            "Configure provider authentication",
            Some("[provider]"),
        ),
        ("logout", "Remove provider authentication", None),
        (
            "reload",
            "Reload keybindings, extensions, skills, prompts, themes, and context files",
            None,
        ),
    ];
    for (name, description, hint) in expected {
        let row = find_row(name);
        assert_eq!(row.description.as_deref(), Some(description), "/{name}");
        assert_eq!(row.argument_hint.as_deref(), hint, "/{name} argument hint");
    }
    // Bare `/name` shows the current name (`/name [name]`), so the hint marks
    // the argument optional.
    assert_eq!(find_row("name").argument_hint.as_deref(), Some("[name]"));
    // The rows that already matched upstream stay unchanged.
    for (name, description) in [
        ("import", "Import and resume a session from a JSONL file"),
        (
            "share",
            "Share session via a Radius artifact or a GitHub gist",
        ),
        ("bug", "Report a bug to the Pi developers"),
        ("copy", "Copy last agent message to clipboard"),
        ("name", "Set session display name"),
        ("session", "Show session info and stats"),
        ("new", "Start a new session"),
        ("compact", "Manually compact the session context"),
        ("resume", "Resume a different session"),
    ] {
        assert_eq!(
            find_row(name).description.as_deref(),
            Some(description),
            "/{name}"
        );
    }
}

// ---------------------------------------------------------------------------
// /copy (upstream `handleCopyCommand`; the clipboard write is mocked)
// ---------------------------------------------------------------------------

fn assistant_with_text(text: &str) -> ChatEntry {
    ChatEntry::Assistant(Arc::new(AssistantMessage {
        content: vec![ContentBlock::Text(TextContent {
            text: text.to_string(),
            text_signature: None,
        })],
        ..Default::default()
    }))
}

#[test]
fn last_assistant_text_picks_the_final_assistant_message() {
    let entries = vec![
        ChatEntry::User("hi".to_string()),
        assistant_with_text("first"),
        ChatEntry::User("again".to_string()),
        assistant_with_text("second"),
    ];
    assert_eq!(last_assistant_text(&entries).as_deref(), Some("second"));
    assert_eq!(last_assistant_text(&[]), None);
    assert_eq!(
        last_assistant_text(&[ChatEntry::User("only".to_string())]),
        None
    );
}

#[test]
fn copy_uses_the_last_agent_message_via_the_clipboard_sink() {
    let entries = vec![
        ChatEntry::User("hi".to_string()),
        assistant_with_text("copied payload"),
    ];
    let mut captured: Option<String> = None;
    let outcome = copy_last_assistant_message(&entries, |text| {
        captured = Some(text.to_string());
        true
    });
    assert_eq!(outcome, CopyOutcome::Copied);
    assert_eq!(captured.as_deref(), Some("copied payload"));
}

#[test]
fn copy_reports_nothing_to_copy_without_an_agent_message() {
    let entries = vec![ChatEntry::User("hi".to_string())];
    let outcome = copy_last_assistant_message(&entries, |_| panic!("sink must not run"));
    assert_eq!(outcome, CopyOutcome::NothingToCopy);
}

#[test]
fn copy_reports_an_unavailable_clipboard() {
    let entries = vec![assistant_with_text("payload")];
    let outcome = copy_last_assistant_message(&entries, |_| false);
    assert_eq!(outcome, CopyOutcome::ClipboardUnavailable);
}

// ---------------------------------------------------------------------------
// /debug (v1): the argument parser and the TUI-width timing block. The
// numbers come from `crate::diagnostics::RunDiagnostics` (collected by
// `AgentSession` on every run); the worker appends the block after each run
// once `/debug enable` latched the session-scoped mode.
// ---------------------------------------------------------------------------

#[test]
fn debug_argument_parses_the_four_forms() {
    assert_eq!(debug_command_argument(""), Ok(DebugArgument::Status));
    assert_eq!(debug_command_argument("   "), Ok(DebugArgument::Status));
    assert_eq!(debug_command_argument("enable"), Ok(DebugArgument::Enable));
    assert_eq!(
        debug_command_argument(" ENABLE "),
        Ok(DebugArgument::Enable)
    );
    assert_eq!(
        debug_command_argument("disable"),
        Ok(DebugArgument::Disable)
    );
    assert_eq!(debug_command_argument("last"), Ok(DebugArgument::Last));
    // An unknown word (typo) is a usage error, never a silent fallback to the
    // status dump.
    let error = debug_command_argument("enabl").expect_err("typo must be rejected");
    assert!(error.contains("Unknown /debug option \"enabl\""), "{error}");
    assert!(error.contains("/debug [enable|disable|last]"), "{error}");
}

fn diag_tool(name: &str, ms: Option<u64>, is_error: bool) -> ToolCallDiagnostics {
    ToolCallDiagnostics {
        name: name.to_string(),
        ms,
        is_error,
        nested: false,
    }
}

fn diag_turn(
    turn: usize,
    provider_ms: u64,
    stream_ms: u64,
    total_ms: u64,
    deltas: u64,
    tool_calls: Vec<ToolCallDiagnostics>,
    usage: UsageDiagnostics,
) -> TurnDiagnostics {
    TurnDiagnostics {
        turn,
        provider_ms,
        stream_ms,
        total_ms,
        deltas,
        tool_calls,
        usage,
    }
}

/// A run whose totals are the turns' sums (the collector's own rule).
fn diag_run(turns: Vec<TurnDiagnostics>) -> RunDiagnostics {
    let mut total = RunTotals {
        turns: turns.len(),
        ..RunTotals::default()
    };
    for turn in &turns {
        total.provider_ms += turn.provider_ms;
        total.stream_ms += turn.stream_ms;
        total.total_ms += turn.total_ms;
        total.deltas += turn.deltas;
        total.tool_calls += turn.tool_calls.len();
        total.usage.input += turn.usage.input;
        total.usage.output += turn.usage.output;
        total.usage.cache_read += turn.usage.cache_read;
        total.usage.cache_write += turn.usage.cache_write;
        total.usage.total_tokens += turn.usage.total_tokens;
    }
    RunDiagnostics {
        wall_ms: total.total_ms,
        turns,
        total,
        slowest_turn: None,
        slowest_tool: None,
    }
}

#[test]
fn debug_timings_single_turn() {
    let run = diag_run(vec![diag_turn(
        1,
        8_990,
        9_010,
        9_130,
        12,
        vec![
            diag_tool("bash", Some(420), false),
            diag_tool("read", Some(30), false),
        ],
        UsageDiagnostics {
            input: 17_708,
            output: 3,
            ..UsageDiagnostics::default()
        },
    )]);
    assert_eq!(
        run_diagnostics_text(&run),
        "turn 1  provider 8.99s  stream 9.01s  total 9.13s  deltas 12  \
         tools: bash 0.42s, read 0.03s  tokens in 17708/out 3"
    );
}

#[test]
fn debug_timings_multi_turn_ends_with_total() {
    let run = diag_run(vec![
        diag_turn(
            1,
            8_990,
            9_010,
            9_130,
            12,
            vec![diag_tool("bash", Some(420), false)],
            UsageDiagnostics {
                input: 100,
                output: 50,
                ..UsageDiagnostics::default()
            },
        ),
        diag_turn(
            2,
            1_200,
            2_000,
            2_100,
            7,
            Vec::new(),
            UsageDiagnostics::default(),
        ),
    ]);
    assert_eq!(
        run_diagnostics_text(&run),
        "turn 1  provider 8.99s  stream 9.01s  total 9.13s  deltas 12  \
         tools: bash 0.42s  tokens in 100/out 50\n\
         turn 2  provider 1.20s  stream 2.00s  total 2.10s  deltas 7  tools: -\n\
         TOTAL  provider 10.19s  stream 11.01s  total 11.23s  deltas 19  \
         tools: 1 call  tokens in 100/out 50"
    );
}

#[test]
fn debug_timings_mark_tool_errors_and_missing_durations() {
    let run = diag_run(vec![diag_turn(
        1,
        10,
        20,
        30,
        0,
        vec![
            diag_tool("bash", Some(420), true),
            diag_tool("read", None, false),
        ],
        UsageDiagnostics::default(),
    )]);
    assert_eq!(
        run_diagnostics_text(&run),
        "turn 1  provider 0.01s  stream 0.02s  total 0.03s  deltas 0  \
         tools: bash 0.42s!, read -"
    );
}

#[test]
fn debug_timings_omit_tokens_when_the_provider_reported_no_usage() {
    let run = diag_run(vec![diag_turn(
        1,
        10,
        20,
        30,
        5,
        Vec::new(),
        UsageDiagnostics::default(),
    )]);
    let text = run_diagnostics_text(&run);
    assert_eq!(
        text,
        "turn 1  provider 0.01s  stream 0.02s  total 0.03s  deltas 5  tools: -"
    );
    assert!(
        !text.contains("tokens"),
        "no usage must print no tokens: {text}"
    );
}

#[test]
fn debug_timings_render_cache_activity_when_reported() {
    let run = diag_run(vec![diag_turn(
        1,
        10,
        20,
        30,
        1,
        Vec::new(),
        UsageDiagnostics {
            input: 1_000,
            output: 20,
            cache_read: 400,
            cache_write: 100,
            total_tokens: 1_520,
        },
    )]);
    assert_eq!(
        run_diagnostics_text(&run),
        "turn 1  provider 0.01s  stream 0.02s  total 0.03s  deltas 1  tools: -  \
         tokens in 1000/out 20 cache r 400/w 100"
    );
}

#[test]
fn debug_timings_truncate_over_long_tool_names() {
    // 36 chars: the first 23 stay, the tail becomes `…` (24-char cell).
    let long_name = "mcp__filesystem__read_multiple_files";
    let run = diag_run(vec![diag_turn(
        1,
        10,
        20,
        30,
        1,
        vec![diag_tool(long_name, Some(1_500), false)],
        UsageDiagnostics::default(),
    )]);
    let text = run_diagnostics_text(&run);
    assert_eq!(
        text,
        "turn 1  provider 0.01s  stream 0.02s  total 0.03s  deltas 1  \
         tools: mcp__filesystem__read_m… 1.50s"
    );
    assert!(!text.contains(long_name), "the full name must be truncated");
}

#[test]
fn debug_timings_empty_run_renders_an_explanation() {
    // The collector never stores a turn-less run, but the type allows it and
    // the renderer must not produce an empty notice.
    let run = diag_run(Vec::new());
    assert_eq!(run_diagnostics_text(&run), "no turn timings recorded");
}
