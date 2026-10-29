//! Regression tests for the TUI audit findings: paste routing (P1-1),
//! raw-vs-display submit split (P1-2), resize handling (P1-5), the editor's
//! visible-window budget (P2-7) and render-time header theme styling
//! (P2-12).

use pir::tui::ansi::strip_ansi;
use pir::tui::app::{
    header_line_raw, header_line_styled, header_line_styled_with, startup_header_kinds,
    HeaderLineKind, LiveHeaderLine,
};
use pir::tui::core::{Component, Terminal, Text, TuiCore};
use pir::tui::editor::Editor;
use pir::tui::keybindings::Keybindings;
use pir::tui::stdin_buffer::WriteLog;
use pir::tui::terminal::{
    coalesce_paste_allowed, paste_route, submit_user_texts, HeaderLine, PasteRoute, ReloadGuard,
};
use pir::tui::theme::{builtin_theme, set_theme};

/// Occurrences of `needle` in `haystack` (the header-once assertions).
fn count_occurrences(haystack: &str, needle: &str) -> usize {
    haystack.match_indices(needle).count()
}

// ---------------------------------------------------------------------------
// P1-1: paste routing while a dialog is open
// ---------------------------------------------------------------------------

#[test]
fn paste_routes_by_overlay_state() {
    // No overlay: the paste reaches the main editor.
    assert_eq!(paste_route(false, false, false), PasteRoute::Editor);
    assert_eq!(paste_route(false, true, false), PasteRoute::Editor);
    // Buffer dialog (input/editor): the paste goes into the dialog buffer.
    assert_eq!(paste_route(true, true, false), PasteRoute::DialogBuffer);
    // Non-buffer dialog (select/confirm): the paste is consumed — it must
    // NOT fall through into the hidden main editor.
    assert_eq!(paste_route(true, false, false), PasteRoute::Swallowed);
    // Model/thinking selector overlay: the paste is consumed too — the
    // hidden editor behind the selector must not receive it.
    assert_eq!(paste_route(false, false, true), PasteRoute::Swallowed);
    assert_eq!(paste_route(true, true, true), PasteRoute::DialogBuffer);
}

/// The raw-key paste coalescer is an editor optimization. Selectors, the
/// transcript search and non-text dialogs consume a paste instead of feeding
/// it to a filter, so a rapid typed run there must stay keys (a Settings
/// search term is exactly eight characters).
#[test]
fn rapid_key_coalescing_is_limited_to_text_focus() {
    // Main editor: coalesce.
    assert!(coalesce_paste_allowed(false, false, false, false));
    // Text dialog: a paste edits its buffer.
    assert!(coalesce_paste_allowed(false, true, true, false));
    // Selector/menu or transcript search: keep keys as keys.
    assert!(!coalesce_paste_allowed(true, false, false, false));
    assert!(!coalesce_paste_allowed(false, false, false, true));
    // Non-text confirm consumes a paste; coalescing would lose the keys.
    assert!(!coalesce_paste_allowed(false, true, false, false));
    // A selector wins even with a text dialog behind it.
    assert!(!coalesce_paste_allowed(true, true, true, false));
}

// ---------------------------------------------------------------------------
// P1-2: the model gets the raw prompt, the transcript the transformer output
// ---------------------------------------------------------------------------

#[test]
fn submit_splits_raw_and_display_text() {
    let texts = submit_user_texts("**raw** prompt", "transformed prompt");
    assert_eq!(texts.display, "transformed prompt");
    assert_eq!(texts.to_model, "**raw** prompt");

    // Without a transformer the two coincide.
    let texts = submit_user_texts("plain", "plain");
    assert_eq!(texts.display, texts.to_model);
}

// ---------------------------------------------------------------------------
// P1-5: resize events update the width the renderer uses
// ---------------------------------------------------------------------------

#[test]
fn resize_updates_width_used_by_renderer() {
    // The process-global theme: serialize with the sibling suites' swaps (a
    // parallel `set_theme` between the swap and the render would restyle it).
    let _guard = super::tui_system_theme::lock();
    set_theme("dark");
    let mut core = TuiCore::new(Terminal::new());
    core.push_content(Box::new(Text::with_padding("hello", 0, 0)));
    core.render();
    let before = core.last_render_width();
    assert!(before > 0, "first render records its width");
    core.terminal().set_size(40, 20);
    core.render();
    assert_eq!(core.last_render_width(), 40, "resize updates the width");
    core.stop();
}

// ---------------------------------------------------------------------------
// P2-7: the editor's visible window matches the dock budget
// ---------------------------------------------------------------------------

#[test]
fn editor_visible_window_matches_dock_budget() {
    let editor = Editor::new();
    editor.set_text("one\ntwo\nthree\nfour\nfive\nsix\nseven\neight");
    // set_text leaves the cursor on the last line: rendering with the dock
    // budget of 3 content rows must keep the cursor inside the window (the
    // old code truncated a 7-row window down to 3, vanishing the cursor).
    let (lines, cursor_line_index, _cursor_col) = editor.render_with_cursor_rows(60, 3);
    assert_eq!(lines.len(), 5, "border + 3 rows + border");
    assert!(
        (1..=3).contains(&cursor_line_index),
        "cursor line {cursor_line_index} outside the visible window"
    );
    // The cursor's line actually carries the inverse-video cursor.
    let cursor_line = &lines[cursor_line_index];
    assert!(cursor_line.contains("\x1b[7m"), "cursor rendered");
    // The window scrolls to follow the cursor: the top border reports the
    // hidden rows above.
    assert!(
        strip_ansi(&lines[0]).contains('↑'),
        "top border shows hidden rows: {}",
        strip_ansi(&lines[0])
    );
}

// ---------------------------------------------------------------------------
// P2-12: header lines restyle on a live theme swap
// ---------------------------------------------------------------------------

#[test]
fn header_line_styled_matches_raw_text() {
    let _guard = super::tui_system_theme::lock();
    set_theme("dark");
    for kind in startup_header_kinds(false) {
        assert_eq!(
            strip_ansi(&header_line_styled(kind)),
            header_line_raw(kind),
            "styled text of {kind:?} equals its raw text"
        );
    }
}

#[test]
fn header_lines_restyle_on_theme_swap() {
    // Explicit themes: the process-global theme is shared by every test in
    // this binary, so a swap-based capture races with parallel set_theme
    // callers. Render under explicit Theme objects instead.
    let kb = Keybindings::new();
    let dark_theme = builtin_theme("dark").expect("built-in dark theme");
    let light_theme = builtin_theme("light").expect("built-in light theme");
    let dark: Vec<String> = startup_header_kinds(false)
        .iter()
        .map(|kind| header_line_styled_with(&dark_theme, *kind, &kb))
        .collect();
    let light: Vec<String> = startup_header_kinds(false)
        .iter()
        .map(|kind| header_line_styled_with(&light_theme, *kind, &kb))
        .collect();
    for (d, l) in dark.iter().zip(light.iter()) {
        assert_eq!(
            strip_ansi(d),
            strip_ansi(l),
            "visible text is theme-independent"
        );
    }
    assert_ne!(dark[1], light[1], "logo line restyled");
    assert_ne!(dark[2], light[2], "compact hint line restyled");
}

#[test]
fn header_line_component_applies_theme_at_render_time() {
    let _guard = super::tui_system_theme::lock();
    set_theme("dark");
    let line = HeaderLine(HeaderLineKind::Logo);
    let dark = line.render(80);
    set_theme("light");
    let light = line.render(80);
    assert_ne!(dark, light, "re-render after the swap restyles the header");
    assert_eq!(strip_ansi(&dark[0]), strip_ansi(&light[0]));
    set_theme("dark");
}

// ---------------------------------------------------------------------------
// Startup header emitted once per app start (the `/resume` duplicate-banner
// regression)
// ---------------------------------------------------------------------------

/// The content tree `App::new` builds for a start: the built-in header slot
/// (one `LiveHeaderLine` per `startup_header_kinds`) above the transcript.
fn header_content(core: &mut TuiCore, kb: &Keybindings) {
    for kind in startup_header_kinds(false) {
        core.push_content(Box::new(LiveHeaderLine::new(kind, kb)));
    }
    core.push_content(Box::new(Text::with_padding("transcript", 0, 0)));
}

/// The startup block is written ONCE per app start. The `/resume`/attach step
/// forced a full repaint of the whole frame; when that repaint did not clear
/// first, a regular-mode (main screen) run wrote the boot block — logo,
/// compact hints, onboarding — a second time into the terminal's scrollback
/// (the reported screenshot). The forced repaint must therefore clear the
/// screen AND scrollback before re-writing the frame (`2J` + home + `3J`,
/// upstream `fullRender(true)`).
#[test]
fn startup_header_is_painted_once_per_app_start() {
    let _guard = super::tui_system_theme::lock();
    set_theme("dark");
    let dir = std::env::temp_dir().join(format!("pir-tui-header-once-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("temp dir");
    let log_path = dir.join("writes.log");
    let _ = std::fs::remove_file(&log_path);

    // The write log is the `PI_TUI_WRITE_LOG` sink, injected so the terminal
    // byte stream is observable without the process-global environment.
    let terminal = Terminal::with_write_log(WriteLog::at(&log_path));
    terminal.set_size(80, 24);
    let mut core = TuiCore::new(terminal);
    let kb = Keybindings::new();
    header_content(&mut core, &kb);

    // Fresh start: the first paint writes the boot block exactly once and
    // does NOT clear (upstream's first render assumes a clean screen).
    core.render();
    let first = std::fs::read_to_string(&log_path).expect("first paint");
    assert_eq!(
        count_occurrences(&first, "Pi can explain"),
        1,
        "one boot block on the first paint: {first:?}"
    );
    assert!(
        !first.contains("\x1b[2J"),
        "the first paint must not clear the screen"
    );

    // Resume/attach: `invalidate_frame` (the `/resume` picker path in
    // fullscreen mode, the Unix suspend rebuild) drops the cached frame, so
    // the next render is a full repaint. It must clear first — a plain
    // re-write stacks a second copy of the boot block on the main screen's
    // scrollback.
    core.invalidate_frame();
    core.render();
    let stream = std::fs::read_to_string(&log_path).expect("repaint");
    let repaint = &stream[first.len()..];
    assert!(
        repaint.contains("\x1b[2J\x1b[H\x1b[3J"),
        "the forced repaint clears screen + scrollback first: {repaint:?}"
    );
    assert_eq!(
        count_occurrences(repaint, "Pi can explain"),
        1,
        "the repaint writes the boot block once: {repaint:?}"
    );
    core.stop();
    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------
// Copy-on-select verdicts (benign conditions must not read as failures)
// ---------------------------------------------------------------------------

/// A selection whose rows run past the frame (the anchor outlived a shrinking
/// frame, or the drag ended below the transcript) still yields the rows that
/// exist: upstream's `getActiveSelectionText` reads `sourceLines[row] ?? ""`
/// (`tui-alt-screen.js:955-961`), while returning `None` here flipped the
/// copy-on-select flash to "Copy failed" without copying anything.
#[test]
fn selection_past_the_frame_end_copies_the_existing_rows() {
    use pir::tui::altscreen_search::{selection_text, SelectionSpan};
    let frame = vec!["alpha".to_string(), "\x1b[38;5;1mbe\x1b[mta".to_string()];
    let span = SelectionSpan {
        start: (0, 0),
        end: (3, 4),
    };
    assert_eq!(
        selection_text(&frame, &span).as_deref(),
        Some("alpha\nbeta\n\n"),
        "out-of-frame rows are empty lines, not a failed extraction"
    );
    // Upstream joins every covered row, so a span that lies entirely past
    // the frame is a blank string rather than `None` (only a genuinely empty
    // SELECTION — no columns at all — is nothing to copy; that case is pinned
    // in `altscreen_parity`).
    let past = SelectionSpan {
        start: (5, 0),
        end: (6, 2),
    };
    assert_eq!(selection_text(&frame, &past).as_deref(), Some("\n"));
    let empty_span = SelectionSpan {
        start: (0, 2),
        end: (0, 2),
    };
    assert!(selection_text(&frame, &empty_span).is_none());
}

/// The platform clipboard commands of the copy verdict (upstream
/// `copyToClipboard`'s second stage, `utils/clipboard.js:96-121`): the copy
/// is reported as successful when the native write OR the platform command
/// worked. Windows takes the UTF-8 temp-file route instead of `clip.exe`
/// (whose stdin decoding mangles non-ASCII through the console code page —
/// upstream's own reason for the WSL path, `clipboard.js:28-59`).
#[test]
fn clipboard_fallback_commands_match_upstream() {
    use pir::tui::clipboard::{stdin_clipboard_commands, windows_set_clipboard_args};
    let none = |_: &str| None;
    let programs = |commands: Vec<(&'static str, Vec<&'static str>)>| {
        commands
            .into_iter()
            .map(|(program, _)| program)
            .collect::<Vec<_>>()
    };

    assert_eq!(
        programs(stdin_clipboard_commands("macos", &none)),
        vec!["pbcopy"]
    );
    // A plain Linux terminal with neither a display nor Termux: no command
    // can own the selection, so the stage is empty (OSC 52 covers the rest).
    assert!(programs(stdin_clipboard_commands("linux", &none)).is_empty());
    let linux_env = |name: &str| match name {
        "TERMUX_VERSION" => Some("1".to_string()),
        "WAYLAND_DISPLAY" => Some("wayland-0".to_string()),
        "DISPLAY" => Some(":0".to_string()),
        _ => None,
    };
    assert_eq!(
        programs(stdin_clipboard_commands("linux", &linux_env)),
        vec!["termux-clipboard-set", "wl-copy", "xclip", "xsel"]
    );
    // Windows: never `clip.exe` (see above).
    assert!(stdin_clipboard_commands("windows", &none).is_empty());

    let args = windows_set_clipboard_args(r"C:\tmp\pir-clip-1.txt");
    assert_eq!(args[0], "-NoProfile");
    assert!(args[2].contains("Set-Clipboard"));
    assert!(
        args[2].contains(r"([System.IO.File]::ReadAllText('C:\tmp\pir-clip-1.txt', [System.Text.Encoding]::UTF8))"),
        "the script reads the UTF-8 temp file: {}",
        args[2]
    );
    // A path containing a quote stays a valid PowerShell single-quoted string.
    assert!(windows_set_clipboard_args("a'b")[2].contains("'a''b'"));
}

// ---------------------------------------------------------------------------
// `/skill:<name>` is prompt text, not an unknown command
// ---------------------------------------------------------------------------

/// The `/resume`-era report: `/skill:lark-im` (an autocompleted resource
/// command) warned `Unknown command` and the text was swallowed. Upstream has
/// no unknown-command notice at all: its editor submit handler falls through
/// to the normal prompt submission (`interactive-mode.js:2540-2700`), and the
/// session's prompt path expands skill commands
/// (`AgentSession._expandSkillCommand`, `agent-session.js:1661-1687`, which
/// returns unknown text verbatim) and prompt templates. The dispatch contract
/// is pinned here without a terminal.
#[test]
fn skill_commands_fall_through_to_the_prompt_path() {
    use pir::skills::SkillInfo;
    use pir::tui::terminal::{unknown_command_route, UnknownCommandRoute};

    // Built-ins keep their actions; extension-registered names dispatch to
    // the extension; everything else is submitted as a prompt.
    assert_eq!(unknown_command_route(true), UnknownCommandRoute::Extension);
    assert_eq!(unknown_command_route(false), UnknownCommandRoute::Prompt);

    // The parsed invocation keeps the skill name and its argument, and the
    // name is NOT a built-in action (`skill:<name>` entries are resource
    // commands registered by the resource loader for autocomplete).
    let parsed = pir::tui::slash::parse_slash_command("/skill:lark-im 总结").expect("parsed");
    assert_eq!(parsed.name, "skill:lark-im");
    assert_eq!(parsed.args, "总结");
    assert_eq!(pir::tui::slash::builtin_action(parsed.name), None);
    assert_eq!(pir::tui::slash::builtin_action("lark-im"), None);

    // The submitted text is expanded by the session prompt path: known skill
    // → `<skill …>` block with the args appended, unknown skill → verbatim.
    let dir = std::env::temp_dir().join(format!("pir-skill-fallthrough-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("temp dir");
    let skill_path = dir.join("lark-im.md");
    std::fs::write(
        &skill_path,
        "---\nname: lark-im\ndescription: Read Feishu messages\n---\n\nRead the group messages.\n",
    )
    .expect("skill file");
    let skills = vec![SkillInfo {
        name: "lark-im".to_string(),
        description: "Read Feishu messages".to_string(),
        path: skill_path.clone(),
        disable_model_invocation: false,
    }];
    let expanded =
        pir::coding_agent::resources::expand_skill_command("/skill:lark-im 总结三天", &skills);
    assert!(
        expanded.starts_with("<skill name=\"lark-im\" location=") && expanded.ends_with("总结三天"),
        "the skill body expands with the arguments appended: {expanded}"
    );
    assert!(
        expanded.contains("Read the group messages."),
        "the skill file body is inlined: {expanded}"
    );
    assert_eq!(
        pir::coding_agent::resources::expand_skill_command("/skill:not-installed x", &skills),
        "/skill:not-installed x",
        "an unknown skill passes through verbatim (upstream behavior)"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------
// P1-C: `/reload` in-progress latch
// ---------------------------------------------------------------------------

/// The reload guard is independent of the "Reloading…" display label: a
/// run-end event clearing the label must not let a submit/Esc/second reload
/// through while the off-thread discovery is still in flight. Only the
/// outcome application (every outcome) releases it.
#[test]
fn reload_guard_lifecycle() {
    let mut guard = ReloadGuard::default();
    assert!(!guard.is_active());
    assert!(guard.begin(), "the first /reload starts");
    assert!(guard.is_active());
    assert!(!guard.begin(), "a second /reload while one runs is refused");
    assert!(guard.is_active(), "the refused attempt keeps the latch");
    guard.finish();
    assert!(!guard.is_active());
    // A failed/panicked discovery applies an outcome too: the latch is free
    // for the next attempt.
    assert!(guard.begin());
    guard.finish();
    assert!(!guard.is_active());
}
