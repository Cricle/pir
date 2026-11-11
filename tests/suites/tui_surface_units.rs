//! TUI surface unit tests moved out of `src/tui/**` inline `#[cfg(test)]`
//! modules (the source-side modules were the last inline tests in the TUI
//! tree; see `tests/suites/source_hygiene.rs`):
//!
//! - `src/tui/autocomplete.rs` → the autocomplete visible-window contract
//!   (PIR-TUI-014, `autocompleteMaxVisible` default + clamp);
//! - `src/tui/app.rs` → `outputPad` as a transcript render input (PIR-TUI-016);
//! - `src/tui/core.rs` → `terminal.clearOnShrink` renderer-boundary semantics
//!   (PIR-TUI-018);
//! - `src/tui/settings_menu.rs` → the `autocomplete-max-visible` row agreeing
//!   with the popup's runtime default;
//! - `src/tui/spill_preview.rs` → the mmap preview reader (line index, tail
//!   preview, byte budget, Windows map lifecycle) and the manual 58 MB bench;
//! - `src/tui/terminal.rs` → the "no model configured" notice naming the
//!   actual agent dir (PIR-TUI-008, `no_model_message_tests`) and the `/fork`
//!   composer re-seed + status labels (`fork_seed_tests`, now reachable via
//!   the `#[doc(hidden)] pub` `seed_fork_composer` / `fork_status_text`
//!   seams).

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use pir::tui::ansi::strip_ansi;
use pir::tui::app::{ChatContainer, ChatEntry};
use pir::tui::autocomplete::{
    AutocompleteItem, AutocompletePopup, AutocompleteSuggestions, AUTOCOMPLETE_MAX_VISIBLE_DEFAULT,
};
use pir::tui::core::{Component, Terminal, TuiCore};
use pir::tui::settings_menu::{
    settings_items, SettingsItem as SettingsMenuRow, SettingsMenuContext,
};
use pir::tui::spill_preview::MappedLines;
use pir::tui::stdin_buffer::WriteLog;
use pir::tui::terminal::no_model_configured_message;

use pir::ai::types::{AssistantMessage, ContentBlock, TextContent};
use pir::config::settings::Settings;

// ---------------------------------------------------------------------------
// src/tui/autocomplete.rs — autocomplete_max_visible_tests
// ---------------------------------------------------------------------------

fn suggestions(count: usize) -> AutocompleteSuggestions {
    AutocompleteSuggestions {
        items: (0..count)
            .map(|index| AutocompleteItem::new(format!("/cmd{index}"), format!("cmd{index}")))
            .collect(),
        prefix: "/".to_string(),
    }
}

/// PIR-TUI-014: an unset setting keeps the upstream default (5).
#[test]
fn unset_defaults_to_the_upstream_window() {
    let popup = AutocompletePopup::new(suggestions(32));
    assert_eq!(popup.max_visible, AUTOCOMPLETE_MAX_VISIBLE_DEFAULT);
    assert_eq!(popup.visible_item_rows(), 5);
}

/// PIR-TUI-014: a configured `autocompleteMaxVisible` reaches the popup
/// and is clamped to the upstream 3..=20 range.
#[test]
fn with_max_visible_applies_the_clamped_setting() {
    let popup = AutocompletePopup::new(suggestions(32)).with_max_visible(Some(15));
    assert_eq!(popup.visible_item_rows(), 15);
    let popup = AutocompletePopup::new(suggestions(32)).with_max_visible(Some(3));
    assert_eq!(popup.visible_item_rows(), 3);
    let popup = AutocompletePopup::new(suggestions(32)).with_max_visible(Some(1));
    assert_eq!(popup.visible_item_rows(), 3);
    let popup = AutocompletePopup::new(suggestions(32)).with_max_visible(Some(50));
    assert_eq!(popup.visible_item_rows(), 20);
}

/// PIR-TUI-014 live apply: `set_max_visible` resizes an already-open
/// popup, and `None` returns to the runtime default.
#[test]
fn set_max_visible_updates_an_open_popup() {
    let mut popup = AutocompletePopup::new(suggestions(32));
    assert_eq!(popup.visible_item_rows(), 5);
    popup.set_max_visible(Some(15));
    assert_eq!(popup.visible_item_rows(), 15);
    popup.set_max_visible(None);
    assert_eq!(popup.visible_item_rows(), 5);
}

/// Fewer suggestions than the window still paint all of them.
#[test]
fn small_result_sets_are_not_padded() {
    let popup = AutocompletePopup::new(suggestions(4)).with_max_visible(Some(15));
    assert_eq!(popup.visible_item_rows(), 4);
}

// ---------------------------------------------------------------------------
// src/tui/app.rs — output_pad_tests
// ---------------------------------------------------------------------------

// PIR-TUI-016: `outputPad` is a transcript render input. The container
// bakes it into every child at construction, and `set_output_pad` (the
// `/settings` live path) must rebuild already-committed entries so the
// next frame reflects the new inset.

fn leading_spaces(lines: &[String], needle: &str) -> Option<usize> {
    lines.iter().find_map(|line| {
        let plain = strip_ansi(line);
        (plain.trim() == needle).then(|| plain.len() - plain.trim_start().len())
    })
}

fn assistant_reply(text: &str) -> ChatEntry {
    let mut message = AssistantMessage::default();
    message.content.push(ContentBlock::Text(TextContent {
        text: text.to_string(),
        text_signature: None,
    }));
    ChatEntry::Assistant(Arc::new(message))
}

#[test]
fn user_and_assistant_entries_follow_the_configured_output_pad() {
    let mut container = ChatContainer::new();
    container.set_entries(&[
        ChatEntry::User("user body".to_string()),
        assistant_reply("mock reply"),
    ]);
    let lines = container.render(60);
    assert_eq!(leading_spaces(&lines, "user body"), Some(1));
    assert_eq!(leading_spaces(&lines, "mock reply"), Some(1));

    // The live `/settings` path: changing the pad must move existing
    // committed entries, not only future ones.
    container.set_output_pad(0);
    container.set_entries(&[
        ChatEntry::User("user body".to_string()),
        assistant_reply("mock reply"),
    ]);
    let lines = container.render(60);
    assert_eq!(leading_spaces(&lines, "user body"), Some(0));
    assert_eq!(leading_spaces(&lines, "mock reply"), Some(0));

    container.set_output_pad(1);
    container.set_entries(&[
        ChatEntry::User("user body".to_string()),
        assistant_reply("mock reply"),
    ]);
    let lines = container.render(60);
    assert_eq!(leading_spaces(&lines, "user body"), Some(1));
    assert_eq!(leading_spaces(&lines, "mock reply"), Some(1));
}

#[test]
fn fresh_containers_default_to_the_upstream_output_pad() {
    // Upstream `SETTINGS_DEFAULTS.outputPad` is 1: a caller that never
    // sets the field must keep the pre-settings byte shape.
    let mut container = ChatContainer::new();
    container.set_entries(&[assistant_reply("default pad")]);
    let lines = container.render(60);
    assert_eq!(leading_spaces(&lines, "default pad"), Some(1));
}

// ---------------------------------------------------------------------------
// src/tui/core.rs — clear_on_shrink_tests
// ---------------------------------------------------------------------------

// PIR-TUI-018: `terminal.clearOnShrink` startup/live semantics at the
// renderer boundary. Upstream `tui-main-screen.ts` only takes the
// clearing full-render path when the new frame is shorter than
// `maxLinesRendered` and the setting is on; the default diff keeps the
// previous frame's tail cleanup but never emits `2J`/`3J`.

struct ListComponent(Arc<Mutex<Vec<String>>>);

impl Component for ListComponent {
    fn render(&self, _width: usize) -> Vec<String> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

fn frame_lines(count: usize) -> Vec<String> {
    (0..count).map(|index| format!("line {index}")).collect()
}

#[test]
fn clear_on_shrink_defaults_off_and_gates_the_full_repaint() {
    let dir = std::env::temp_dir().join(format!(
        "pir-tui-clear-on-shrink-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).expect("temp dir");
    let log_path = dir.join("writes.log");
    let terminal = Terminal::with_write_log(WriteLog::at(&log_path));
    terminal.set_size(40, 20);
    let mut core = TuiCore::new(terminal);
    assert!(!core.clear_on_shrink(), "default off (upstream default)");

    let content = Arc::new(Mutex::new(frame_lines(6)));
    core.push_content(Box::new(ListComponent(Arc::clone(&content))));
    core.render();
    assert_eq!(core.max_lines_rendered(), 6);

    let first_len = std::fs::read(&log_path).expect("first paint").len();
    *content.lock().unwrap() = frame_lines(2);
    core.render();
    let raw = std::fs::read(&log_path).expect("paint");
    let diff = String::from_utf8_lossy(&raw[first_len..]).to_string();
    assert!(
        !diff.contains("\x1b[2J"),
        "clearOnShrink=false must not full-repaint: {diff:?}"
    );
    // The deleted-tail branch clears with `\r` + `2K` on each extra row
    // and walks the cursor back up; the exact motion is not the contract.
    assert!(
        diff.contains("\x1b[2K") && diff.contains("\x1b[4A"),
        "the diff still clears the previous frame's tail and returns \
         the cursor: {diff:?}"
    );
    assert_eq!(
        core.max_lines_rendered(),
        6,
        "an off-shrink keeps the working-area high-water mark"
    );

    // Grow back, enable live (the `/settings` path calls this setter),
    // shrink again: the clearing full render runs and resets the mark.
    *content.lock().unwrap() = frame_lines(6);
    core.render();
    core.set_clear_on_shrink(true);
    let enabled_len = std::fs::read(&log_path).expect("paint").len();
    *content.lock().unwrap() = frame_lines(2);
    core.render();
    let raw = std::fs::read(&log_path).expect("paint");
    let full = String::from_utf8_lossy(&raw[enabled_len..]).to_string();
    assert!(
        full.contains("\x1b[2J\x1b[H\x1b[3J"),
        "clearOnShrink=true clears screen + scrollback: {full:?}"
    );
    assert!(
        full.contains("line 0") && full.contains("line 1"),
        "the full repaint writes the new frame: {full:?}"
    );
    assert_eq!(core.max_lines_rendered(), 2, "full render resets the mark");

    core.stop();
    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------
// src/tui/settings_menu.rs — autocomplete_max_visible_row_tests
// ---------------------------------------------------------------------------

fn autocomplete_row(settings: &Settings) -> SettingsMenuRow {
    settings_items(settings, &SettingsMenuContext::default())
        .into_iter()
        .find(|item| item.id == "autocomplete-max-visible")
        .expect("autocomplete-max-visible row is always present")
}

/// PIR-TUI-014: the unset row must show the runtime default (5), not an
/// arbitrary neighbouring value, so the menu and the popup agree.
#[test]
fn unset_row_shows_the_runtime_default() {
    let row = autocomplete_row(&Settings::default());
    assert_eq!(row.values[row.current], "5");
    assert_eq!(row.payloads[row.current], serde_json::json!(5));
}

/// A configured value keeps its stored position and payload.
#[test]
fn configured_row_shows_the_stored_value() {
    let settings = Settings {
        autocomplete_max_visible: Some(15),
        ..Settings::default()
    };
    let row = autocomplete_row(&settings);
    assert_eq!(row.values[row.current], "15");
    assert_eq!(row.payloads[row.current], serde_json::json!(15));
}

// ---------------------------------------------------------------------------
// src/tui/spill_preview.rs — tests
// ---------------------------------------------------------------------------

fn temp_file(name: &str, bytes: &[u8]) -> PathBuf {
    let path = std::env::temp_dir().join(format!(
        "pir-spill-preview-test-{}-{name}",
        std::process::id()
    ));
    std::fs::write(&path, bytes).expect("write fixture");
    path
}

#[test]
fn lines_and_tail_preview() {
    let path = temp_file("lines", b"a\nbb\nccc\n");
    let lines = MappedLines::open(&path).expect("map");
    assert_eq!(lines.line_count(), 3);
    assert_eq!(lines.line(0), Some("a"));
    assert_eq!(lines.line(2), Some("ccc"));
    assert_eq!(lines.line(3), None);
    assert_eq!(lines.tail_preview(2, 1024).as_deref(), Some("bb\nccc"));
    let _ = std::fs::remove_file(&path);
}

#[test]
fn empty_and_non_utf8_fall_back() {
    let empty = temp_file("empty", b"");
    assert!(MappedLines::open(&empty).is_none());
    let binary = temp_file("binary", &[0xff, 0xfe, 0x00, b'\n']);
    let preview = MappedLines::open(&binary).and_then(|lines| lines.tail_preview(10, 1024));
    assert!(
        preview.is_none(),
        "invalid UTF-8 falls back at preview time"
    );
    let _ = std::fs::remove_file(&empty);
    let _ = std::fs::remove_file(&binary);
}

#[test]
fn byte_budget_bounds_one_huge_line() {
    let path = temp_file("huge", b"head\n0123456789\n");
    let lines = MappedLines::open(&path).expect("map");
    let body = lines.tail_preview(2, 8).expect("preview");
    assert_eq!(body, "0123456789");
    let _ = std::fs::remove_file(&path);
}

/// Windows semantics: append while mapped is visible only after remap;
/// truncate/remove requires dropping the map first; empty files cannot be
/// mapped (already covered by the fallback test).
#[test]
fn mmap_lifecycle() {
    let path = temp_file("lifecycle", b"one\ntwo\n");
    let first_len = {
        let lines = MappedLines::open(&path).expect("map");
        assert_eq!(lines.len(), 8);
        std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .and_then(|mut file| {
                use std::io::Write;
                file.write_all(b"three\n")
            })
            .expect("append while mapped");
        assert_eq!(lines.len(), 8, "existing map keeps its old length");
        lines.len()
    };
    assert_eq!(first_len, 8);
    let lines = MappedLines::open(&path).expect("remap");
    assert_eq!(lines.line_count(), 3);
    assert_eq!(lines.line(2), Some("three"));
    drop(lines);
    std::fs::OpenOptions::new()
        .write(true)
        .truncate(true)
        .open(&path)
        .expect("truncate after the map is dropped");
    let _ = std::fs::remove_file(&path);
}

/// Manual 58 MB benchmark (run with `--ignored --nocapture`, or through
/// `scripts/tui_conpty/round61_mmap_bench.py`). Emits one JSON line per
/// mode; timing only — this test binary does not install the tracing
/// allocator (PIR-TUI-020: registration is bin-only), so allocation
/// counters would read zero here. The alloc comparison lives in the
/// round60 mmap prototype evidence (165.3 MB → 16.0 MB, 1,000,023 → 4
/// allocations).
#[test]
#[ignore]
fn bench_58mb() {
    let path = std::env::temp_dir().join(format!(
        "pir-spill-preview-bench-58mb-{}.txt",
        std::process::id()
    ));
    if !path.exists() || std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0) < 58_000_000 {
        use std::io::Write;
        let mut file = std::io::BufWriter::new(std::fs::File::create(&path).expect("create"));
        let line = b"row payload 0123456789 abcdefghijklmnopqrstuvwxyz\n";
        for _ in 0..1_000_000 {
            file.write_all(line).expect("write");
        }
        file.flush().expect("flush");
    }
    let start = std::time::Instant::now();
    let text = std::fs::read_to_string(&path).expect("read");
    let rows = text.lines().count();
    let mut tail: Vec<&str> = text.lines().rev().take(2048).collect();
    tail.reverse();
    let mut preview = String::new();
    for line in tail {
        preview.push_str(line);
        preview.push('\n');
    }
    std::hint::black_box(preview);
    let read_ms = start.elapsed().as_secs_f64() * 1000.0;
    drop(text);
    let start = std::time::Instant::now();
    for _ in 0..5 {
        let lines = MappedLines::open(&path).expect("mmap preview");
        std::hint::black_box(lines.tail_preview(2048, 2 * 1024 * 1024).expect("tail"));
    }
    let mmap_ms = start.elapsed().as_secs_f64() * 1000.0 / 5.0;
    println!("{{\"mode\":\"read\",\"wall_ms\":{read_ms:.3},\"rows\":{rows}}}");
    println!("{{\"mode\":\"mmap\",\"wall_ms\":{mmap_ms:.3},\"rows\":{rows}}}");
    let _ = std::fs::remove_file(&path);
}

// ---------------------------------------------------------------------------
// src/tui/terminal.rs — no_model_message_tests
// ---------------------------------------------------------------------------

/// PIR-TUI-008: the notice must name the agent dir in effect (custom
/// `PIR_AGENT_DIR`), never a hardcoded home path that would be a no-op.
#[test]
fn message_names_the_actual_agent_dir() {
    let message = no_model_configured_message(Path::new("C:/custom/pir-agent"));
    assert!(
        message.contains("C:/custom/pir-agent/models.json"),
        "models.json path must be the actual agent dir: {message}"
    );
    assert!(
        message.contains("C:/custom/pir-agent/settings.json"),
        "settings.json path must be the actual agent dir: {message}"
    );
    assert!(
        !message.contains("~/.pi/agent"),
        "custom agent dir must not fall back to the hardcoded home path: {message}"
    );
}

/// The default dir (whatever `agent_dir()` resolves for the process) is
/// rendered verbatim too; no textual special-casing of home vs env dirs.
#[test]
fn message_is_derived_from_the_given_dir_only() {
    let message = no_model_configured_message(Path::new("/home/user/.pi/agent"));
    assert!(message.contains("/home/user/.pi/agent/models.json"));
    assert!(message.contains("/home/user/.pi/agent/settings.json"));
}

// ---------------------------------------------------------------------------
// src/tui/terminal.rs — fork_seed_tests
// ---------------------------------------------------------------------------

use pir::tui::editor::Editor;
use std::sync::atomic::{AtomicU64, Ordering};

/// `/fork` re-seed (`interactive-mode.ts:5518 editor.setText(result.
/// selectedText ?? "")`): `Some(text)` replaces the composer and bumps
/// the extension-visible revision (the same bump `note_editor_change`
/// performs, so `ctx.ui.getEditorText()` snapshots rebuild).
#[test]
fn seed_replaces_composer_and_bumps_revision() {
    let editor = Arc::new(Editor::new());
    editor.set_text("draft carried by the tab restore");
    let rev = AtomicU64::new(7);
    let seeded =
        pir::tui::terminal::seed_fork_composer(&editor, &rev, Some("R66FORKSEED alpha here"));
    assert!(seeded, "Some(text) must seed");
    assert_eq!(editor.get_text(), "R66FORKSEED alpha here");
    assert_eq!(
        rev.load(Ordering::SeqCst),
        8,
        "revision bumped exactly once"
    );
    // A second identical seed still bumps: upstream also overwrites the
    // composer unconditionally after a successful fork.
    assert!(pir::tui::terminal::seed_fork_composer(
        &editor,
        &rev,
        Some("R66FORKSEED alpha here")
    ));
    assert_eq!(rev.load(Ordering::SeqCst), 9);
}

/// `None` (`At` position / no text) leaves the composer and the revision
/// untouched — upstream writes `""` only when the fork itself succeeded
/// and the payload actually carried text (the missing `selectedText` case
/// is an untouched composer for the picker's default `before` arm).
#[test]
fn none_leaves_composer_untouched() {
    let editor = Arc::new(Editor::new());
    editor.set_text("kept");
    let rev = AtomicU64::new(0);
    assert!(!pir::tui::terminal::seed_fork_composer(&editor, &rev, None));
    assert_eq!(editor.get_text(), "kept");
    assert_eq!(rev.load(Ordering::SeqCst), 0);
}

/// The status labels are the upstream verbatim strings
/// (`interactive-mode.ts:1996/:5519` fork, `:5549` clone) — a regression
/// pin for the PIR-TUI-019-final parity fix (the old pir text was
/// "Forked/Cloned session: <path>", which no upstream fork/clone path
/// prints).
#[test]
fn fork_status_labels_match_upstream() {
    assert_eq!(
        pir::tui::terminal::fork_status_text("Forked"),
        "Forked to new session"
    );
    assert_eq!(
        pir::tui::terminal::fork_status_text("Cloned"),
        "Cloned to new session"
    );
}
