//! FIX-B05 regression tests: restoring the Windows QuickEdit bit must be a
//! read-modify-write of the *current* console mode.
//!
//! The bug: `disable_windows_quick_edit` ran after `enable_raw_mode()` and
//! saved the whole mode (with `ENABLE_LINE_INPUT | ENABLE_ECHO_INPUT |
//! ENABLE_PROCESSED_INPUT` already cleared); cleanup then `disable_raw_mode()`
//! (which only ORs those bits back) followed by a full-mode write-back, so the
//! raw bits were cleared again — the shell lost echo and line editing.
//!
//! The pure helpers are the fix's observable core: they take the mode read at
//! restore time and only toggle the saved bit. The table tests below pin that
//! contract on every platform; [`windows_restore_after_raw_mode_keeps_console_bits`]
//! exercises the real `GetConsoleMode`/`SetConsoleMode` path when a console is
//! available and skips otherwise (CI / redirected stdin).

use pir::tui::stdin_buffer::{
    quick_edit_disabled_mode, restore_quick_edit_bits, restore_vt_input_bits,
    ENABLE_EXTENDED_FLAGS, ENABLE_QUICK_EDIT_MODE, ENABLE_VIRTUAL_TERMINAL_INPUT,
};
use pir::tui::terminal::{TerminalModeOps, TerminalModeStep};

const ENABLE_PROCESSED_INPUT: u32 = 0x0001;
const ENABLE_LINE_INPUT: u32 = 0x0002;
const ENABLE_ECHO_INPUT: u32 = 0x0004;
const ENABLE_WINDOW_INPUT: u32 = 0x0008;
const ENABLE_MOUSE_INPUT: u32 = 0x0010;

/// The bits `disable_raw_mode()` restores (crossterm ORs them back into the
/// mode `enable_raw_mode()` had cleared).
const RAW_RESTORED_BITS: u32 = ENABLE_PROCESSED_INPUT | ENABLE_LINE_INPUT | ENABLE_ECHO_INPUT;

/// A representative mode read *after* `disable_raw_mode()`, plus unrelated
/// flags (window/mouse input) that a full-mode write-back would also discard.
const CURRENT_AFTER_DISABLE_RAW: u32 = RAW_RESTORED_BITS | ENABLE_WINDOW_INPUT | ENABLE_MOUSE_INPUT;

#[test]
fn quick_edit_disabled_mode_clears_only_the_quick_edit_bit() {
    let cases = [
        // Already disabled: still enables extended flags, keeps everything else.
        (
            CURRENT_AFTER_DISABLE_RAW,
            CURRENT_AFTER_DISABLE_RAW | ENABLE_EXTENDED_FLAGS,
        ),
        // Enabled: the bit is cleared, the rest is untouched.
        (
            CURRENT_AFTER_DISABLE_RAW | ENABLE_QUICK_EDIT_MODE | ENABLE_EXTENDED_FLAGS,
            CURRENT_AFTER_DISABLE_RAW | ENABLE_EXTENDED_FLAGS,
        ),
        // Windows default input mode shape.
        (
            0x0000_01F7,
            (0x0000_01F7 | ENABLE_EXTENDED_FLAGS) & !ENABLE_QUICK_EDIT_MODE,
        ),
    ];
    for (current, expected) in cases {
        assert_eq!(
            quick_edit_disabled_mode(current),
            expected,
            "current={current:#010x}"
        );
        assert_eq!(
            quick_edit_disabled_mode(current) & RAW_RESTORED_BITS,
            current & RAW_RESTORED_BITS,
            "disable must not touch line/echo/processed"
        );
        assert_eq!(
            quick_edit_disabled_mode(current) & ENABLE_QUICK_EDIT_MODE,
            0,
            "QuickEdit must be cleared"
        );
    }
}

#[test]
fn restore_quick_edit_bits_preserves_the_current_mode() {
    // `saved_enabled == true`: QuickEdit was on when the TUI cleared it.
    let cases = [
        // The common cleanup shape: disable_raw_mode() already restored the
        // line/echo/processed bits; restore must keep them and set QuickEdit.
        (
            CURRENT_AFTER_DISABLE_RAW,
            true,
            CURRENT_AFTER_DISABLE_RAW | ENABLE_QUICK_EDIT_MODE,
        ),
        // Restore-time mode happens to have QuickEdit still set (idempotent).
        (
            CURRENT_AFTER_DISABLE_RAW | ENABLE_QUICK_EDIT_MODE,
            true,
            CURRENT_AFTER_DISABLE_RAW | ENABLE_QUICK_EDIT_MODE,
        ),
        // Saved disabled: clear it, keep everything else.
        (
            CURRENT_AFTER_DISABLE_RAW | ENABLE_QUICK_EDIT_MODE | ENABLE_EXTENDED_FLAGS,
            false,
            CURRENT_AFTER_DISABLE_RAW | ENABLE_EXTENDED_FLAGS,
        ),
        // Empty-ish current mode: only the saved bit is installed.
        (0, true, ENABLE_QUICK_EDIT_MODE),
        (0, false, 0),
    ];
    for (current, saved_enabled, expected) in cases {
        let restored = restore_quick_edit_bits(current, saved_enabled);
        assert_eq!(
            restored, expected,
            "current={current:#010x} saved={saved_enabled}"
        );
        assert_eq!(
            restored & RAW_RESTORED_BITS,
            current & RAW_RESTORED_BITS,
            "line/echo/processed bits from disable_raw_mode must survive"
        );
        assert_eq!(
            restored & ENABLE_QUICK_EDIT_MODE != 0,
            saved_enabled,
            "QuickEdit follows the saved bit"
        );
    }
}

/// The old implementation wrote back the whole mode captured under raw mode.
/// This models that write-back and proves the fixed helper produces a
/// different (correct) result: the assertion on the old value would fail for
/// the old logic, and the fixed value keeps every restored bit.
#[test]
fn full_mode_write_back_would_clear_raw_restored_bits() {
    // What `disable_windows_quick_edit` captured after `enable_raw_mode()`:
    // crossterm had cleared line/echo/processed; QuickEdit was still set.
    const CAPTURED_UNDER_RAW: u32 =
        ENABLE_QUICK_EDIT_MODE | ENABLE_EXTENDED_FLAGS | ENABLE_WINDOW_INPUT;
    assert_eq!(
        CAPTURED_UNDER_RAW & RAW_RESTORED_BITS,
        0,
        "the raw-mode capture is missing the line/echo/processed bits"
    );

    // Old restore: SetConsoleMode(saved_full_mode) after disable_raw_mode.
    let old_style_restore = CAPTURED_UNDER_RAW;
    assert_eq!(
        old_style_restore & RAW_RESTORED_BITS,
        0,
        "old full write-back reproduces the lost-echo bug"
    );

    // Fixed restore: toggle the saved bit on the mode read after
    // disable_raw_mode.
    let fixed = restore_quick_edit_bits(CURRENT_AFTER_DISABLE_RAW, true);
    assert_eq!(
        fixed & RAW_RESTORED_BITS,
        RAW_RESTORED_BITS,
        "fixed restore keeps the bits disable_raw_mode restored"
    );
    assert_ne!(
        fixed, old_style_restore,
        "the test must distinguish the fixed behavior from the old write-back"
    );
}

#[test]
fn restore_vt_input_bits_preserves_the_current_mode() {
    let cases = [
        (CURRENT_AFTER_DISABLE_RAW, false, CURRENT_AFTER_DISABLE_RAW),
        (
            CURRENT_AFTER_DISABLE_RAW | ENABLE_VIRTUAL_TERMINAL_INPUT,
            false,
            CURRENT_AFTER_DISABLE_RAW,
        ),
        (
            CURRENT_AFTER_DISABLE_RAW,
            true,
            CURRENT_AFTER_DISABLE_RAW | ENABLE_VIRTUAL_TERMINAL_INPUT,
        ),
        (
            CURRENT_AFTER_DISABLE_RAW | ENABLE_VIRTUAL_TERMINAL_INPUT,
            true,
            CURRENT_AFTER_DISABLE_RAW | ENABLE_VIRTUAL_TERMINAL_INPUT,
        ),
    ];
    for (current, saved_enabled, expected) in cases {
        let restored = restore_vt_input_bits(current, saved_enabled);
        assert_eq!(
            restored, expected,
            "current={current:#010x} saved={saved_enabled}"
        );
        assert_eq!(
            restored & RAW_RESTORED_BITS,
            current & RAW_RESTORED_BITS,
            "VT restore must not touch line/echo/processed"
        );
        assert_eq!(
            restored & ENABLE_VIRTUAL_TERMINAL_INPUT != 0,
            saved_enabled,
            "VT input follows the saved bit"
        );
    }
}

/// Real console API path: mimic `App::new` / `cleanup()` exactly — enter raw
/// mode, clear QuickEdit, disable raw mode, restore QuickEdit — and assert the
/// line/echo/processed bits survive. Skips when stdin is not a console.
#[cfg(windows)]
#[test]
fn windows_restore_after_raw_mode_keeps_console_bits() {
    use crossterm::terminal::{disable_raw_mode, enable_raw_mode};
    use crossterm_winapi::{ConsoleMode, Handle, HandleType};
    use pir::tui::stdin_buffer::{
        disable_windows_quick_edit, restore_windows_quick_edit, windows_vt_input_status,
        VtInputStatus,
    };

    if windows_vt_input_status() == VtInputStatus::Unavailable {
        eprintln!("skipping: no console input handle (redirected stdin)");
        return;
    }
    let Ok(handle) = Handle::new(HandleType::InputHandle) else {
        eprintln!("skipping: no console input handle");
        return;
    };
    let console = ConsoleMode::from(handle);
    let Ok(before) = console.mode() else {
        eprintln!("skipping: console mode unreadable");
        return;
    };
    let saved_quick_edit = before & ENABLE_QUICK_EDIT_MODE != 0;

    if enable_raw_mode().is_err() {
        eprintln!("skipping: enable_raw_mode failed (no console)");
        return;
    }
    let raw = console.mode().expect("console mode under raw mode");
    if raw & RAW_RESTORED_BITS != 0 {
        // crossterm's raw-mode semantics changed: this scenario no longer
        // models App::new, so there is nothing to regress here.
        let _ = disable_raw_mode();
        eprintln!("skipping: raw mode no longer clears line/echo/processed");
        return;
    }

    let cleared = disable_windows_quick_edit();
    assert_eq!(
        cleared, saved_quick_edit,
        "QuickEdit is cleared when present; otherwise the helper is a no-op"
    );
    let _ = disable_raw_mode();
    restore_windows_quick_edit();

    let after = console.mode().expect("console mode after restore");
    assert_eq!(
        after & RAW_RESTORED_BITS,
        RAW_RESTORED_BITS,
        "disable_raw_mode's line/echo/processed bits must survive QuickEdit restore"
    );
    assert_eq!(
        after & ENABLE_QUICK_EDIT_MODE != 0,
        saved_quick_edit,
        "QuickEdit returns to its pre-TUI state"
    );
}

// ---------------------------------------------------------------------------
// FIX-B05 wiring: App::new / cleanup call order (P3-新-2)
// ---------------------------------------------------------------------------

#[derive(Clone, Default)]
struct RecordingSteps(std::sync::Arc<std::sync::Mutex<Vec<TerminalModeStep>>>);

struct RecordingOps {
    steps: RecordingSteps,
}

impl RecordingOps {
    fn record(&mut self, step: TerminalModeStep) {
        self.steps
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(step);
    }
}

impl TerminalModeOps for RecordingOps {
    fn enable_raw_mode(&mut self) -> std::io::Result<()> {
        self.record(TerminalModeStep::EnableRawMode);
        Ok(())
    }

    fn disable_raw_mode(&mut self) -> std::io::Result<()> {
        self.record(TerminalModeStep::DisableRawMode);
        Ok(())
    }

    fn disable_quick_edit(&mut self) {
        self.record(TerminalModeStep::DisableQuickEdit);
    }

    fn restore_quick_edit(&mut self) {
        self.record(TerminalModeStep::RestoreQuickEdit);
    }

    fn enable_vt_input(&mut self) {
        self.record(TerminalModeStep::EnableVtInput);
    }

    fn restore_vt_input(&mut self) {
        self.record(TerminalModeStep::RestoreVtInput);
    }
}

/// The console-mode ORDER `App::new` and the cleanup path run, driven through
/// the same helpers production calls. No TTY is required: the recorder stands
/// in for `enable_raw_mode`/`SetConsoleMode`. Deleting the VT restore (the
/// P3-新-1 fix) or moving it before `disable_raw_mode` fails here.
#[test]
fn terminal_mode_startup_and_teardown_order_is_pinned() {
    use pir::tui::terminal::{
        enable_vt_input_if, startup_terminal_modes, teardown_terminal_modes, TerminalModeStep,
    };

    let steps = RecordingSteps::default();
    let mut ops = RecordingOps {
        steps: steps.clone(),
    };
    startup_terminal_modes(&mut ops).expect("raw mode");
    enable_vt_input_if(&mut ops, true);
    assert_eq!(
        *steps
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner),
        vec![
            TerminalModeStep::EnableRawMode,
            TerminalModeStep::DisableQuickEdit,
            TerminalModeStep::EnableVtInput,
        ],
        "App::new: raw mode, then QuickEdit off, then the PIR_TUI_VT_INPUT opt-in"
    );

    steps
        .0
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clear();
    teardown_terminal_modes(&mut ops).expect("raw mode restore");
    assert_eq!(
        *steps
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner),
        vec![
            TerminalModeStep::DisableRawMode,
            TerminalModeStep::RestoreVtInput,
            TerminalModeStep::RestoreQuickEdit,
        ],
        "cleanup: disable raw mode FIRST, then the bit-level VT/QuickEdit restores (VT before QuickEdit)"
    );
}

/// The production `cleanup()` body: the same `cleanup_terminal_surface` call
/// with an injected recorder and output sink. Asserts the console-mode
/// restore happens BEFORE the first surface write (`LeaveAlternateScreen`),
/// so deleting the teardown call from the cleanup pipeline is caught too.
#[test]
fn cleanup_surface_restores_console_modes_before_leaving_the_alt_screen() {
    use std::io::Write;

    use pir::config::settings::FullscreenExitOutput;
    use pir::tui::core::{Terminal, TuiCore};
    use pir::tui::stdin_buffer::WriteLog;
    use pir::tui::terminal::{cleanup_terminal_surface, TerminalModeStep};

    struct RecordingOut {
        steps: RecordingSteps,
        bytes: Vec<u8>,
        steps_at_first_write: Option<Vec<TerminalModeStep>>,
    }

    impl Write for RecordingOut {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            if self.steps_at_first_write.is_none() {
                self.steps_at_first_write = Some(
                    self.steps
                        .0
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .clone(),
                );
            }
            self.bytes.extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    let steps = RecordingSteps::default();
    let mut ops = RecordingOps {
        steps: steps.clone(),
    };
    let mut out = RecordingOut {
        steps: steps.clone(),
        bytes: Vec::new(),
        steps_at_first_write: None,
    };
    let terminal = Terminal::with_sink_factory(|| Box::new(std::io::sink()), WriteLog::disabled());
    let mut core = TuiCore::new(terminal);

    cleanup_terminal_surface(
        &mut core,
        false,
        FullscreenExitOutput::ResumeHint,
        &mut ops,
        &mut out,
    )
    .expect("cleanup surface");

    let expected = vec![
        TerminalModeStep::DisableRawMode,
        TerminalModeStep::RestoreVtInput,
        TerminalModeStep::RestoreQuickEdit,
    ];
    assert_eq!(
        *steps
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner),
        expected,
        "cleanup must restore raw mode, then VT input, then QuickEdit"
    );
    assert_eq!(
        out.steps_at_first_write.as_deref(),
        Some(expected.as_slice()),
        "the restore must complete before the first surface write"
    );
    assert!(
        out.bytes
            .windows(b"\x1b[?1049l".len())
            .any(|window| window == b"\x1b[?1049l"),
        "cleanup still leaves the alternate screen: {:?}",
        String::from_utf8_lossy(&out.bytes)
    );
}

// ---------------------------------------------------------------------------
// PIR-TUI-040: the exit transcript dump (`fullscreenExitOutput=transcript`,
// the default) writes the WINDOWED frame — the whole materialized band, not
// just the painted viewport — and streams it in bounded chunks. On a big
// session this dump is the entire `tui.cleanup` cost, so these tests pin both
// the source slice and the exact byte stream (a change that dumps the wrong
// slice, drops rows, or allocates the whole message at once turns them red).
// ---------------------------------------------------------------------------

/// Sink that records the byte stream AND the write-call boundaries.
#[derive(Default)]
struct DumpSink {
    bytes: Vec<u8>,
    calls: usize,
    max_call: usize,
}

impl std::io::Write for DumpSink {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.calls += 1;
        self.max_call = self.max_call.max(buf.len());
        self.bytes.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[test]
fn transcript_dump_streams_the_joined_rows_without_one_giant_buffer() {
    use pir::tui::terminal::write_transcript_dump;

    let rows: Vec<String> = (0..4000)
        .map(|index| format!("row {index:05} {}", "x".repeat(200)))
        .collect();
    let mut sink = DumpSink::default();
    write_transcript_dump(&mut sink, &rows).expect("dump");
    let expected = format!("\r{}\r\n", rows.join("\r\n"));
    assert_eq!(
        String::from_utf8(sink.bytes.clone()).expect("utf8 dump"),
        expected,
        "the dump byte stream is `\\r` + rows joined by `\\r\\n` + `\\r\\n`"
    );
    assert!(
        sink.calls > 1,
        "a {}-byte dump must stream, not build one message-sized string (calls={})",
        sink.bytes.len(),
        sink.calls
    );
    assert!(
        sink.max_call < sink.bytes.len(),
        "no single write carries the whole dump (max_call={} of {})",
        sink.max_call,
        sink.bytes.len()
    );

    // Degenerate shapes keep the exact byte stream.
    let mut empty = DumpSink::default();
    write_transcript_dump(&mut empty, &[]).expect("empty dump");
    assert!(empty.bytes.is_empty(), "an empty frame writes nothing");
    let mut one = DumpSink::default();
    write_transcript_dump(&mut one, &["only row".to_string()]).expect("one row");
    assert_eq!(one.bytes, b"\ronly row\r\n".to_vec());
}

/// The windowed (fullscreen) dump carries the materialized BAND frame, not the
/// painted viewport: `cleanup_terminal_surface` must dump `last_frame_lines()`
/// when the app owns the alt screen, and `last_screen_lines()` otherwise.
#[test]
fn transcript_dump_is_the_windowed_band_frame() {
    use pir::config::settings::FullscreenExitOutput;
    use pir::tui::core::{Terminal, Text, TuiCore};
    use pir::tui::stdin_buffer::WriteLog;
    use pir::tui::terminal::cleanup_terminal_surface;

    let rows: Vec<String> = (1..=30)
        .map(|index| format!("transcript row {index}"))
        .collect();
    let terminal = Terminal::with_sink_factory(|| Box::new(std::io::sink()), WriteLog::disabled());
    terminal.set_size(40, 8);
    let mut core = TuiCore::new(terminal);
    core.push_content(Box::new(Text::new(rows.join("\n"))));
    core.set_windowed_viewport(true);
    core.render();
    let frame = core.last_frame_lines().to_vec();
    let screen = core.last_screen_lines();
    assert!(
        frame.len() > screen.len(),
        "the band frame ({}) is taller than the painted viewport ({})",
        frame.len(),
        screen.len()
    );

    let steps = RecordingSteps::default();
    let mut ops = RecordingOps {
        steps: steps.clone(),
    };
    let mut out = DumpSink::default();
    cleanup_terminal_surface(
        &mut core,
        true,
        FullscreenExitOutput::Transcript,
        &mut ops,
        &mut out,
    )
    .expect("cleanup surface");

    let text = String::from_utf8_lossy(&out.bytes).to_string();
    let dumped = format!("\r{}\r\n", frame.join("\r\n"));
    assert!(
        text.contains(&dumped),
        "the windowed dump carries the whole band frame ({} rows)",
        frame.len()
    );
    let viewport_dump = format!("\r{}\r\n", screen.join("\r\n"));
    assert_ne!(dumped, viewport_dump);
    assert!(
        !text.contains(&viewport_dump),
        "the dump is not the painted viewport slice"
    );
}
