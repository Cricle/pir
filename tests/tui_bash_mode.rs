//! BASHMODE — `!` shell-mode composer state machine.
//!
//! The mode is entered only by a real key `!` typed into an EMPTY composer
//! (`Editor::insert_char`, the terminal key path). Bracketed paste goes
//! through `Editor::paste_text` and never arms it. Enter runs the parsed
//! command instead of submitting a prompt; Esc exits.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use pir::tui::ansi::{strip_ansi, visible_width};
use pir::tui::editor::{parse_bash_submission, BashSubmission, Editor};
use pir::tui::terminal::{embed_status_in_border, is_text_input_key, BASH_STATUS_LABEL};

#[test]
fn bang_key_enters_bash_mode_only_on_an_empty_composer() {
    // The terminal key gate must accept the SHIFT-composed `!` event (US
    // layout) rather than treating it as a control chord.
    assert!(is_text_input_key(&KeyEvent::new(
        KeyCode::Char('!'),
        KeyModifiers::SHIFT
    )));

    let editor = Editor::new();
    assert!(!editor.is_bash_mode());
    editor.insert_char('!');
    assert!(editor.is_bash_mode(), "`!` first key must arm bash mode");
    assert_eq!(editor.get_text(), "!");

    // A `!` typed into a non-empty composer is ordinary text.
    let editor = Editor::new();
    editor.insert_text("hello");
    editor.insert_char('!');
    assert!(!editor.is_bash_mode());
    assert_eq!(editor.get_text(), "hello!");
}

#[test]
fn bracketed_paste_starting_with_bang_never_arms_shell_mode() {
    let editor = Editor::new();
    editor.paste_text("!echo from paste");
    assert!(
        !editor.is_bash_mode(),
        "a pasted `!` prefix must stay plain text"
    );
    assert_eq!(editor.get_text(), "!echo from paste");
    // Normal-mode submit routing: not a shell command.
    assert_eq!(parse_bash_submission(false, &editor.get_text()), None);

    // The key-burst path (`insert_text_burst`) is paste-origin too.
    let editor = Editor::new();
    editor.insert_text_burst("!ls");
    assert!(!editor.is_bash_mode());
}

#[test]
fn backspace_deleting_the_leading_bang_exits_bash_mode() {
    let editor = Editor::new();
    editor.insert_char('!');
    editor.insert_char('l');
    editor.insert_char('s');
    assert!(editor.is_bash_mode());

    editor.backspace(); // deletes `s`
    assert!(editor.is_bash_mode(), "only the mode `!` itself exits");
    assert_eq!(editor.get_text(), "!l");
    editor.backspace(); // deletes `l`
    assert!(editor.is_bash_mode());
    assert_eq!(editor.get_text(), "!");
    editor.backspace(); // deletes the `!`
    assert!(!editor.is_bash_mode());
    assert_eq!(editor.get_text(), "");

    // Forward-delete on the leading `!` exits as well.
    let editor = Editor::new();
    editor.insert_char('!');
    editor.move_cursor_left();
    editor.delete_char_forward();
    assert!(!editor.is_bash_mode());
    assert_eq!(editor.get_text(), "");
}

#[test]
fn escape_exits_bash_mode_and_returns_normal_ai_text() {
    let editor = Editor::new();
    editor.insert_char('!');
    editor.insert_text("echo hi");
    assert_eq!(editor.get_text(), "!echo hi");

    assert!(editor.exit_bash_mode());
    assert!(!editor.is_bash_mode());
    assert_eq!(
        editor.get_text(),
        "echo hi",
        "the mode indicator is stripped, the draft stays"
    );
    assert!(!editor.exit_bash_mode(), "exit is idempotent");
}

#[test]
fn enter_in_bash_mode_parses_and_runs_the_mock_shell() {
    #[derive(Default)]
    struct MockShell {
        runs: Vec<BashSubmission>,
    }
    impl MockShell {
        fn run(&mut self, submission: BashSubmission) {
            self.runs.push(submission);
        }
    }

    let mut shell = MockShell::default();

    // First command: the mode `!` is part of the composer text.
    let editor = Editor::new();
    editor.insert_char('!');
    editor.insert_text("printf hi");
    assert!(editor.is_bash_mode());
    let bash_mode = editor.is_bash_mode();
    let text = editor.submit().expect("non-empty command");
    let submission = parse_bash_submission(bash_mode, &text).expect("Enter must run the shell");
    shell.run(submission);
    assert_eq!(shell.runs[0].command, "printf hi");
    assert!(!shell.runs[0].exclude_from_context);

    // The mode persists after execution: the next command needs no prefix.
    assert!(editor.is_bash_mode(), "shell mode survives a command");
    editor.insert_text("pwd");
    let text = editor.submit().expect("second command");
    let submission = parse_bash_submission(editor.is_bash_mode(), &text)
        .expect("mode-only submission is a shell command");
    shell.run(submission);
    assert_eq!(shell.runs[1].command, "pwd");

    // `!!` keeps the exclude-from-context semantics.
    let submission = parse_bash_submission(true, "!!pwd").expect("excluded shell command");
    shell.run(submission);
    assert_eq!(shell.runs[2].command, "pwd");
    assert!(shell.runs[2].exclude_from_context);

    // Bare `!` and a normal-mode `!…` prompt are not executed.
    assert_eq!(parse_bash_submission(true, "!"), None);
    assert_eq!(parse_bash_submission(true, "   !   "), None);
    assert_eq!(parse_bash_submission(false, "!pwd"), None);
}

/// PIR-TUI-034: the live `!`-bash snapshot accumulator behind the worker's
/// partial `ToolUpdate`s emits the first chunk immediately, then at most one
/// snapshot per throttle window (pi's component appends every chunk; its
/// frame loop coalesces — 100 ms at the producer is the same cadence), and it
/// keeps a rolling tail wide enough that the block's painted preview (the
/// last `DEFAULT_MAX_LINES`/`DEFAULT_MAX_BYTES`) can never change from a
/// longer stream.
#[test]
fn live_bash_tail_emits_first_chunk_then_throttles_and_bounds_the_window() {
    use std::time::{Duration, Instant};

    use pir::coding_agent::truncate::DEFAULT_MAX_BYTES;
    use pir::tui::terminal::{LiveBashTail, LIVE_BASH_UPDATE_THROTTLE};

    let mut tail = LiveBashTail::new();
    let t0 = Instant::now();

    let first = tail
        .push("USERBASH-1\n", t0)
        .expect("the first chunk must emit immediately");
    assert_eq!(first, "USERBASH-1\n");

    // Within the throttle window the chunk accumulates but no snapshot goes
    // out; the next snapshot carries the whole accumulated tail.
    assert!(tail
        .push("USERBASH-2\n", t0 + LIVE_BASH_UPDATE_THROTTLE / 2)
        .is_none());
    assert!(tail
        .push(
            "USERBASH-3\n",
            t0 + LIVE_BASH_UPDATE_THROTTLE - Duration::from_millis(1)
        )
        .is_none());
    let due = tail
        .push("USERBASH-4\n", t0 + LIVE_BASH_UPDATE_THROTTLE)
        .expect("the throttle window must release a snapshot");
    assert_eq!(due, "USERBASH-1\nUSERBASH-2\nUSERBASH-3\nUSERBASH-4\n");

    // A stream longer than the window keeps only the tail: the retained
    // window is 2× the truncation cap, so the last `DEFAULT_MAX_BYTES` the
    // block paints are always intact.
    let big = "x".repeat(4 * DEFAULT_MAX_BYTES);
    let snapshot = tail
        .push(&big, t0 + 2 * LIVE_BASH_UPDATE_THROTTLE)
        .expect("the snapshot after a burst must emit");
    assert_eq!(
        tail.tail().len(),
        2 * DEFAULT_MAX_BYTES,
        "the rolling window is 2× the truncation cap"
    );
    assert_eq!(snapshot.len(), 2 * DEFAULT_MAX_BYTES);
    assert!(snapshot.chars().all(|c| c == 'x'));
}

#[test]
fn bash_mode_has_a_visible_shell_label_in_the_composer_border() {
    assert!(BASH_STATUS_LABEL.contains("BASH"));
    assert!(BASH_STATUS_LABEL.contains("shell"));
    assert!(BASH_STATUS_LABEL.contains("Esc"));

    let border = format!("\x1b[38;5;1m{}\x1b[39m", "─".repeat(48));
    let painted = embed_status_in_border(&border, 48, "!", BASH_STATUS_LABEL);
    let plain = strip_ansi(&painted);
    assert!(
        plain.contains("BASH"),
        "composer border shows the mode: {plain}"
    );
    assert!(plain.contains("Enter runs"));
    assert_eq!(visible_width(&painted), 48);
}
