//! Windows IME / UTF-8 input and TUI cursor-anchoring regressions.
//!
//! Reported from a real Windows session:
//!
//! 1. With an IME candidate window open (Chinese input), the frame corrupts
//!    and the cursor/characters land in the wrong cells.
//! 2. The rendered screen keeps drifting upward.
//!
//! Both trace back to the same porting gap: upstream `positionHardwareCursor`
//! moves the terminal cursor to the focused component's cell on EVERY frame
//! and only toggles its visibility, while pir moved it only when
//! `showHardwareCursor` was enabled. The terminal therefore anchored the IME
//! candidate window at the last byte the frame wrote (the footer), and any
//! cursor movement done by the IME/terminal left `hardware_cursor_row` stale:
//! the next frame's relative cursor moves started from the wrong row, which
//! accumulates into the upward drift. The renderer also lacked the alt-screen
//! width clamp upstream applies, so one over-wide row (a factory renderer, a
//! decorator, a width mis-measure) wraps and scrolls every later frame.
//!
//! The input half pins two defensive fixes for IME text:
//! - `StdinGate` must never walk a multi-byte character's UTF-8 bytes through
//!   the escape-sequence machinery (a held ESC turned `ESC + 0xE4` into an
//!   Alt chord and dropped the character).
//! - `StdinBuffer` must hold a UTF-8 character split across reads instead of
//!   applying upstream's lone-high-byte `ESC + (byte - 128)` conversion to a
//!   lead byte.
//!
//! Standalone binary: tests/ and tests/suites/ are a concurrent-edit zone.

use std::sync::{Arc, Mutex};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use pir::tui::ansi::{strip_ansi, visible_width};
use pir::tui::core::{Component, Rows, Terminal, TuiCore};
use pir::tui::stdin_buffer::{StdinBuffer, StdinEvent, StdinGate, WriteLog};

/// A static component whose row set is swapped between frames.
struct StaticLines {
    lines: Arc<Mutex<Vec<String>>>,
}

impl StaticLines {
    fn new(lines: Vec<String>) -> Self {
        Self {
            lines: Arc::new(Mutex::new(lines)),
        }
    }
}

impl Component for StaticLines {
    fn render(&self, _width: usize) -> Vec<String> {
        self.lines.lock().unwrap().clone()
    }

    fn render_into(&self, _width: usize, out: &mut Rows<'_>) {
        for line in self.lines.lock().unwrap().iter() {
            out.push(line);
        }
    }
}

/// A capture terminal: frames go to the `PI_TUI_WRITE_LOG` sink, which is
/// appended synchronously at `Terminal::write` call time.
struct Capture {
    dir: std::path::PathBuf,
    path: std::path::PathBuf,
    offset: usize,
}

impl Capture {
    fn new(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "pir-tui-ime-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("writes.log");
        Self {
            dir,
            path,
            offset: 0,
        }
    }

    fn terminal(&self) -> Terminal {
        Terminal::with_write_log(WriteLog::at(&self.path))
    }

    /// Everything written since the previous call.
    fn take(&mut self) -> String {
        let data = std::fs::read_to_string(&self.path).unwrap_or_default();
        let chunk = data.get(self.offset..).unwrap_or("").to_string();
        self.offset = data.len();
        chunk
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

// ---------------------------------------------------------------------------
// Cursor anchoring (IME candidate position / drift origin)
// ---------------------------------------------------------------------------

/// The cursor is moved to the editor cell even while it is hidden: that cell
/// is what the terminal/IME uses to anchor the candidate window. The
/// no-changes frame re-anchors it too (upstream `positionHardwareCursor` in
/// both branches).
#[test]
fn hidden_cursor_is_positioned_at_the_editor_cell_and_reanchored() {
    let mut capture = Capture::new("hidden");
    let terminal = capture.terminal();
    terminal.set_size(20, 6);
    let mut core = TuiCore::new(terminal);
    core.push_content(Box::new(StaticLines::new(
        (0..5).map(|index| format!("line {index}")).collect(),
    )));
    core.set_dock_height(2);
    let cell = Arc::new(Mutex::new(Some((1usize, 3usize))));
    core.set_dock_cursor_cell(Arc::clone(&cell));
    // showHardwareCursor stays off: the reverse-video cell is the visible
    // cursor, but the hardware cursor must still be positioned.

    core.render();
    let first = capture.take();
    assert!(
        first.contains("\x1b[4G"),
        "the hidden cursor moves to the editor column: {first:?}"
    );
    assert!(
        first.contains("\x1b[?25l"),
        "the cursor is hidden after positioning: {first:?}"
    );
    assert!(
        !core.cursor_visible(),
        "showHardwareCursor off keeps the cursor hidden"
    );

    // A frame with no content change still re-anchors the cursor: the
    // terminal may have moved it for IME composition, and the next frame's
    // relative moves trust `hardware_cursor_row`.
    core.render();
    let second = capture.take();
    assert!(
        second.contains("\x1b[4G"),
        "an unchanged frame re-anchors the cursor: {second:?}"
    );

    // A selector replacing the editor drops the cell: back to hiding only.
    *cell.lock().unwrap() = None;
    core.render();
    let third = capture.take();
    assert!(
        !third.contains("\x1b[4G"),
        "no cursor cell means no positioning: {third:?}"
    );
    assert!(third.contains("\x1b[?25l") || third.is_empty());
}

/// The fullscreen (windowed) path uses absolute CUP for the hidden cursor, so
/// the IME candidate window follows the editor cell on the alternate screen.
#[test]
fn hidden_cursor_is_positioned_with_cup_in_the_windowed_path() {
    let mut capture = Capture::new("windowed");
    let terminal = capture.terminal();
    terminal.set_size(20, 5);
    let mut core = TuiCore::new(terminal);
    core.set_windowed_viewport(true);
    core.push_content(Box::new(StaticLines::new(
        (0..7).map(|index| format!("line {index}")).collect(),
    )));
    core.set_dock_height(2);
    core.set_dock_cursor_cell(Arc::new(Mutex::new(Some((0usize, 2usize)))));

    core.render();
    let frame = capture.take();
    // Painted viewport = 5 rows (3 transcript + 2 dock); dock row 0 is
    // buffer row 3 (0-based) => CUP row 4, column 3.
    assert!(
        frame.contains("\x1b[4;3H"),
        "windowed frame addresses the dock cell absolutely: {frame:?}"
    );
    assert!(frame.contains("\x1b[?25l"), "and keeps it hidden");
}

// ---------------------------------------------------------------------------
// Alt-screen width clamp (wrap -> upward drift)
// ---------------------------------------------------------------------------

/// A row wider than the terminal is clamped before it is painted, exactly
/// like upstream alt-screen's `sliceByColumn`: without the clamp it wraps and
/// every subsequent `\r\n` scrolls the fixed viewport one row upward.
#[test]
fn overwide_rows_are_clamped_in_the_scrolled_frame() {
    let mut capture = Capture::new("clamp");
    let terminal = capture.terminal();
    terminal.set_size(10, 6);
    let mut core = TuiCore::new(terminal);
    core.set_windowed_viewport(true);
    core.push_content(Box::new(StaticLines::new(vec![
        "0123456789ABCDEFGHIJ".to_string(),
        "short".to_string(),
        "third".to_string(),
        "dock-0".to_string(),
        "dock-1".to_string(),
    ])));
    core.set_dock_height(2);

    core.render();
    let frame = capture.take();
    assert!(!frame.is_empty(), "the frame is painted");
    for line in core.last_screen_lines() {
        let plain = strip_ansi(&line);
        assert!(
            visible_width(&plain) <= 10,
            "painted row wider than the terminal: {plain:?}"
        );
    }
    assert!(
        core.last_screen_lines()
            .iter()
            .any(|line| strip_ansi(line).starts_with("0123456789")),
        "the over-wide row is clipped, not dropped"
    );
}

/// The main-screen (regular) full repaint applies the same guard.
#[test]
fn overwide_rows_are_clamped_in_the_follow_path() {
    let mut capture = Capture::new("clamp-follow");
    let terminal = capture.terminal();
    terminal.set_size(10, 20);
    let mut core = TuiCore::new(terminal);
    core.push_content(Box::new(StaticLines::new(vec![
        "0123456789ABCDEFGHIJ".to_string(),
        "short".to_string(),
    ])));

    core.render();
    let _ = capture.take();
    for line in core.last_screen_lines() {
        let plain = strip_ansi(&line);
        assert!(
            visible_width(&plain) <= 10,
            "main-screen row wider than the terminal: {plain:?}"
        );
    }
}

// ---------------------------------------------------------------------------
// Input decoding: multi-byte characters vs the escape/meta machinery
// ---------------------------------------------------------------------------

/// A held ESC (the escape-timeout window) followed by an IME-committed
/// character releases ESC as Escape and the character as itself; it must not
/// enter the legacy-meta decoder (`ESC + first UTF-8 byte`), which would drop
/// both.
#[test]
fn gate_keeps_a_multibyte_character_after_a_held_escape() {
    use crossterm::event::Event;

    let base = std::time::Instant::now();
    let mut gate = StdinGate::default().with_console_mouse_claim(false);

    assert!(gate.push(Event::Key(key(KeyCode::Esc)), base).is_empty());
    let out = gate.push(
        Event::Key(key(KeyCode::Char('中'))),
        base + std::time::Duration::from_millis(1),
    );
    assert_eq!(
        out,
        vec![
            Event::Key(key(KeyCode::Esc)),
            Event::Key(key(KeyCode::Char('中'))),
        ],
        "ESC and the character both survive"
    );
    let next = gate.push(
        Event::Key(key(KeyCode::Char('文'))),
        base + std::time::Duration::from_millis(2),
    );
    assert_eq!(
        next,
        vec![Event::Key(key(KeyCode::Char('文')))],
        "the following character is unaffected"
    );
}

/// A UTF-8 character split across single-byte reads is held until its
/// continuation bytes arrive and is then emitted as one sequence — never as
/// `ESC + (byte - 128)` meta keys and never as a partial byte run.
#[test]
fn stdin_buffer_reassembles_a_utf8_character_split_across_reads() {
    let base = std::time::Instant::now();
    let mut buf = StdinBuffer::with_timeouts(
        std::time::Duration::from_millis(50),
        std::time::Duration::from_millis(10),
    );
    assert!(buf.process(&[0xe4], base).is_empty(), "lead byte is held");
    assert!(
        buf.process(&[0xb8], base + std::time::Duration::from_millis(1))
            .is_empty(),
        "first continuation byte is held"
    );
    assert_eq!(
        buf.process(&[0xad], base + std::time::Duration::from_millis(2)),
        vec![StdinEvent::Data("中".as_bytes().to_vec())],
        "the complete character is emitted once"
    );
    // A single high byte that is NOT a UTF-8 lead keeps upstream's rule.
    assert_eq!(
        buf.process(&[0xa1], base + std::time::Duration::from_millis(3)),
        vec![StdinEvent::Data(vec![0x1b, 0x21])]
    );
}
