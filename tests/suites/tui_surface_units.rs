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
use pir::tui::keybindings::Keybindings;
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
    // PIR-TUI-027: the shrink repaint clears the SCREEN (`2J` + home) but must
    // NOT emit `3J` — that would erase the terminal's scrollback, which in
    // regular mode is the transcript history the user already scrolled past.
    // Upstream's `fullRender(true)` does emit `3J`; the divergence is the
    // point of this issue (PROTOCOL v8 §5c).
    assert!(
        full.contains("\x1b[2J\x1b[H"),
        "clearOnShrink=true repaints from a cleared screen: {full:?}"
    );
    assert!(
        !full.contains("\x1b[3J"),
        "clearOnShrink=true must NOT erase the terminal scrollback: {full:?}"
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
// src/tui/terminal.rs — transcript_truncation_hint_tests (PIR-TUI-027)
// ---------------------------------------------------------------------------
//
// A huge session materializes only a bounded tail (`regular_window_start` /
// `TranscriptWindow`). Before PIR-TUI-027 the truncation was SILENT: the band
// simply started at `window.start()` and nothing told the renderer — let alone
// the user — how many entries were dropped. `ChatBlockCache` now publishes a
// lead row above block 0 carrying `window.start()` as its count.
//
// These tests pin the three observable facts:
//   1. an untruncated install (`window_start == 0`) has NO lead row;
//   2. a truncated install paints it, with the exact count and the key text;
//   3. the count tracks the window (a slide updates it, and the block-0 frame
//      revision moves so the frame cache repaints the row instead of serving
//      stale rows).

fn hint_entries(count: usize) -> Vec<ChatEntry> {
    (0..count)
        .map(|index| ChatEntry::User(format!("trunc tag {index:04}")))
        .collect()
}

fn block_cache() -> pir::tui::terminal::ChatBlockCache {
    pir::tui::terminal::ChatBlockCache::new(
        Arc::new(std::sync::atomic::AtomicBool::new(false)),
        false,
    )
}

/// The exact strings a user must see, with the styling stripped.
fn lead_texts(cache: &pir::tui::terminal::ChatBlockCache) -> Vec<String> {
    cache.truncation_hint_text().into_iter().collect()
}

#[test]
fn transcript_truncation_hint_is_published_with_the_window_count() {
    let entries = hint_entries(8);
    let mut cache = block_cache();

    // Whole transcript materialized: no truncation row at all.
    cache.set_entries(&entries);
    assert_eq!(cache.truncation_start(), 0);
    assert!(
        lead_texts(&cache).is_empty(),
        "an untruncated band must not paint a truncation row: {:?}",
        lead_texts(&cache)
    );

    // A bounded tail window: the row carries the DROPPED count, which is
    // exactly the window's absolute start.
    cache.set_entries_windowed(&entries[5..], 5);
    assert_eq!(cache.truncation_start(), 5, "window start is the count");
    let texts = lead_texts(&cache);
    assert_eq!(texts.len(), 1, "exactly one lead row: {texts:?}");
    assert!(
        texts[0].contains("(5 earlier entries,"),
        "the row must report the 5 dropped entries: {:?}",
        texts[0]
    );
    assert!(
        texts[0].contains("scroll up to load)"),
        "the regular-mode row must say how to reach the history: {:?}",
        texts[0]
    );

    // The windowed surface names its jump-to-top chord instead.
    cache.set_truncation(5, Some("Ctrl+Home".to_string()));
    let texts = lead_texts(&cache);
    assert!(
        texts[0].contains("Ctrl+Home to load)"),
        "a bound chord must be named in the row: {:?}",
        texts[0]
    );

    // A wider window shrinks the count; fully materializing suppresses it.
    cache.set_entries_windowed(&entries[2..], 2);
    let texts = lead_texts(&cache);
    assert!(
        texts[0].contains("(2 earlier entries,"),
        "the count must follow the window: {:?}",
        texts[0]
    );
    cache.set_entries_windowed(&entries, 0);
    assert!(
        lead_texts(&cache).is_empty(),
        "materializing the whole transcript removes the row again"
    );
}

/// The row is part of block 0's rendered identity: a window slide must move
/// block 0's revision so `TuiCore`'s per-block frame cache re-renders that
/// block instead of serving the previous frame's rows (which still hold the
/// old hint text). Pinned against the real `block_revisions` path.
#[test]
fn transcript_truncation_hint_moves_the_first_block_revision() {
    let entries = hint_entries(8);
    let mut cache = block_cache();
    cache.set_entries_windowed(&entries[4..], 4);
    let before = cache.block_revisions().expect("primed cache");
    assert_eq!(before.len(), 4);

    // Same entries, different window origin: the lead row text changes, so
    // block 0's identity must change while blocks 1.. keep theirs.
    cache.set_entries_windowed(&entries[3..], 3);
    let after = cache.block_revisions().expect("primed cache");
    assert_ne!(
        before[0], after[0],
        "block 0 owns the truncation row, so a moved window must move it"
    );

    // The load-key text is the row's other input.
    let count_only = after[0];
    cache.set_truncation(3, Some("Ctrl+Home".to_string()));
    let with_key = cache.block_revisions().expect("primed cache")[0];
    assert_ne!(
        count_only, with_key,
        "naming the load chord must repaint the row"
    );
}

// ---------------------------------------------------------------------------
// src/tui/core.rs — windowed_frame_never_erases_scrollback (PIR-TUI-027)
// ---------------------------------------------------------------------------
//
// The 512-entry transcript window (`regular_window_start`) is a bounded TAIL:
// entries `[0, start)` are not materialized. On the main screen the terminal's
// scrollback IS the readable transcript history, and `3J` erases it. A
// COMPLETE frame repairs that in the same frame (it re-emits every row), which
// is why upstream's `fullRender(true)` is lossless there — verified against pi:
// pi emits `2J 3J` per full redraw and its scrollback is refilled with the whole
// transcript. A WINDOWED frame re-emits only the tail, so the same `3J`
// destroys history permanently (the live report: 4044 scrollback rows -> 0,
// turn 1 unreachable, no key pressed; see
// `.play/evidence/ab-round1/lane-fix-historyreset/REPORT.md`).
//
// These tests pin the renderer half: with the frame declared windowed
// (`set_frame_is_complete_transcript(false)`), EVERY clearing repaint path is
// screen-only (`2J` + home, no `3J`); with the default (`true`) the byte shape
// stays exactly upstream's.

fn byte_log_core(dir: &Path, name: &str) -> (std::path::PathBuf, TuiCore) {
    let log_path = dir.join(name);
    let _ = std::fs::remove_file(&log_path);
    let terminal = Terminal::with_write_log(WriteLog::at(&log_path));
    terminal.set_size(40, 20);
    (log_path.clone(), TuiCore::new(terminal))
}

/// The upstream `firstChanged < viewportTop` guard — the path the real
/// 512-entry crossing takes — is the one repaint that STILL clears screen +
/// scrollback (`tests/suites/tui_regressions.rs` pins that byte shape for the
/// `/resume` rebase). Left ungated it erases the regular-mode history, so the
/// two arms below are the contrast that matters: a complete frame keeps
/// upstream's `2J H 3J` (its re-write repairs the erasure), a windowed one must
/// be screen-only.
#[test]
fn windowed_frame_shrink_repaint_keeps_scrollback() {
    let dir = std::env::temp_dir().join(format!(
        "pir-tui-windowed-no-3j-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).expect("temp dir");

    // Control arm: a COMPLETE frame keeps upstream's `2J`+home+`3J` — this is
    // the shape `clear_on_shrink_defaults_off_and_gates_the_full_repaint`
    // asserts after the concurrent screen-only change, expressed here as a
    // whole-frame top rewrite (the guard branch).
    let complete = repaint_after_top_rewrite(&dir, "complete.log", true);
    assert!(
        complete.contains("\x1b[2J\x1b[H\x1b[3J"),
        "a complete frame keeps upstream's clearing full render: {complete:?}"
    );

    // Windowed arm: the same repaint, declared as a tail-only frame.
    let windowed = repaint_after_top_rewrite(&dir, "windowed.log", false);
    assert!(
        windowed.contains("\x1b[2J\x1b[H"),
        "a windowed repaint still clears the screen first: {windowed:?}"
    );
    assert!(
        !windowed.contains("\x1b[3J"),
        "a windowed (tail-only) frame must NEVER erase the terminal \
         scrollback — it cannot re-emit the rows it drops: {windowed:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Render a 40-row frame (taller than the 20-row terminal so
/// `previous_viewport_top > 0`), rewrite its HEAD, render again: the change
/// sits above the viewport, so the renderer takes the upstream full-repaint
/// guard. Returns the bytes written by the second render.
fn repaint_after_top_rewrite(dir: &Path, log_name: &str, complete: bool) -> String {
    let (log_path, mut core) = byte_log_core(dir, log_name);
    let content = Arc::new(Mutex::new(frame_lines(40)));
    core.push_content(Box::new(ListComponent(Arc::clone(&content))));
    core.set_frame_is_complete_transcript(complete);
    core.render();
    let before = std::fs::read(&log_path).unwrap_or_default().len();
    let mut rows = frame_lines(40);
    rows[0] = "head changed".to_string();
    *content.lock().unwrap() = rows;
    core.render();
    let raw = std::fs::read(&log_path).unwrap_or_default();
    let repaint = String::from_utf8_lossy(&raw[before.min(raw.len())..]).to_string();
    assert!(
        repaint.contains("head changed"),
        "the guard repaint writes the new frame: {repaint:?}"
    );
    core.stop();
    repaint
}

/// The upstream `firstChanged < viewportTop` guard (the path that fires on the
/// real 512-entry crossing in regular mode) takes a clearing full render. With
/// the frame declared windowed, that repaint must be screen-only too;
/// otherwise the LIVE history loss reproduced on 2026-11-12 stays.
#[test]
fn windowed_frame_top_change_repaint_keeps_scrollback() {
    let dir = std::env::temp_dir().join(format!(
        "pir-tui-windowed-guard-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).expect("temp dir");
    let repaint = repaint_after_top_rewrite(&dir, "guard-windowed.log", false);
    assert!(
        !repaint.contains("\x1b[3J"),
        "the viewport-top guard must not erase scrollback for a windowed \
         frame: {repaint:?}"
    );
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

// ---------------------------------------------------------------------------
// src/tui/terminal.rs — PIR-TUI-030: `ctrl+o more` expands the startup header
// ---------------------------------------------------------------------------
//
// The startup line advertises `{tools.expand} more` (`compact_line`), so
// pressing it must show something. Upstream's `setToolsExpanded` calls
// `builtInHeader.setExpanded(...)` on an `ExpandableText`, swapping the
// compact instruction row for the 19-line expanded key list
// (interactive-mode.ts:4553-4567 / :1026-1070). pir built the built-in header
// once from `startup_header_kinds(false)` and never passed `true`, so on a
// session with no tool cards ctrl+o restyled nothing the user could see.
//
// The fix keeps the built-in header's rows derived from the live
// `tools_expanded` cell: `builtin_header_rows(expanded, kb, width)` is the one
// place the header becomes frame rows, and `ExtensionHeaderSlot` rebuilds from
// it when the cell (or the width/palette) moves.
//
// These tests pin the four observable requirements:
//   1. the collapsed frame is byte-identical to the pre-fix frame (the same
//      `pad_to_width(header_line_styled_for(kind, kb), width)` rows, in the
//      same order, and the same row COUNT — nothing new appears);
//   2. the expanded frame DIFFERS and carries hint rows that only the
//      expanded shape contains (`to clear`, `to suspend`, …), one row each;
//   3. collapsing again reproduces the collapsed rows exactly;
//   4. the same holds for a header rebuilt after a `/clear` (a fresh slot with
//      the same live cell renders the same rows).

/// The pre-PIR-TUI-030 collapsed header, built the way
/// `ExtensionHeaderSlot` used to build it: one `LiveHeaderLine` per
/// `startup_header_kinds(false)` entry, each `pad_to_width`-ed to the frame
/// width. This is the independent expectation (not a call into the builder
/// under test).
fn collapsed_header_rows_reference(kb: &Keybindings, width: usize) -> Vec<String> {
    use pir::tui::app::{header_line_styled_for, startup_header_kinds};
    startup_header_kinds(false)
        .into_iter()
        .map(|kind| pir::tui::ansi::pad_to_width(&header_line_styled_for(kind, kb), width))
        .collect()
}

/// The row-level shape a single header row must have: the full frame width,
/// exactly ONE terminal row (no embedded newline — a `\n`-joined expanded key
/// list pushed as one row is the defect this test exists to catch).
fn assert_header_row(row: &str, width: usize, label: &str) {
    assert!(
        !row.contains('\n'),
        "{label}: a header row must be one terminal row, got embedded newline: {row:?}"
    );
    assert_eq!(
        pir::tui::ansi::visible_width(row),
        width,
        "{label}: padded to the frame width"
    );
}

/// The expanded-only hint texts (upstream `expandedInstructions`): a row
/// carrying one of these exists ONLY while `tools_expanded` is true. The
/// compact line carries `interrupt`, `clear/exit`, `/ commands`, `! bash`,
/// `more` — never these descriptions.
const EXPANDED_ONLY_HINTS: [&str; 5] = [
    "to clear",
    "to exit (empty)",
    "to delete to end",
    "to expand tools",
    "to attach",
];

#[test]
fn ctrl_o_expands_the_builtin_startup_header_and_collapses_back() {
    // The header's styled text reads the process-global theme; serialize with
    // the sibling suites' swaps so a parallel `set_theme` cannot repaint it
    // mid-assertion.
    let _guard = super::tui_system_theme::lock();
    pir::tui::theme::set_theme("dark");
    let kb = Keybindings::new();
    let width = 100;

    // --- 1. collapsed default: byte-identical to the pre-fix frame ----------
    let collapsed = pir::tui::terminal::builtin_header_rows(false, &kb, width);
    assert_eq!(
        collapsed,
        collapsed_header_rows_reference(&kb, width),
        "the collapsed header must stay byte-identical to the pre-fix rows"
    );
    assert_eq!(
        collapsed.len(),
        pir::tui::app::startup_header_kinds(false).len(),
        "one row per collapsed header line, nothing more"
    );
    for (index, row) in collapsed.iter().enumerate() {
        assert_header_row(row, width, &format!("collapsed row {index}"));
    }
    let collapsed_text = collapsed
        .iter()
        .map(|row| strip_ansi(row))
        .collect::<Vec<_>>();
    assert!(
        collapsed_text.iter().any(|row| row.contains(" more")),
        "the collapsed line still advertises the expand key: {collapsed_text:?}"
    );
    assert!(
        !collapsed_text
            .iter()
            .any(|row| EXPANDED_ONLY_HINTS.iter().any(|hint| row.contains(hint))),
        "no expanded-only hint may appear while collapsed: {collapsed_text:?}"
    );

    // --- 2. expanded frame DIFFERS and carries the per-hint rows -----------
    let expanded = pir::tui::terminal::builtin_header_rows(true, &kb, width);
    assert_ne!(
        expanded, collapsed,
        "after ctrl+o the header frame must differ from the collapsed one"
    );
    assert!(
        expanded.len() > collapsed.len(),
        "the expanded header is taller: {} rows vs {} collapsed",
        expanded.len(),
        collapsed.len()
    );
    for (index, row) in expanded.iter().enumerate() {
        assert_header_row(row, width, &format!("expanded row {index}"));
    }
    let expanded_text = expanded
        .iter()
        .map(|row| strip_ansi(row))
        .collect::<Vec<_>>();

    // One row carries exactly one hint: the block of hint rows equals the live
    // `expanded_key_hints` list, in upstream order, styled.
    let hint_rows: Vec<&String> = expanded_text
        .iter()
        .filter(|row| row.starts_with("  ") || !row.trim_start().is_empty())
        .filter(|row| {
            pir::tui::app::expanded_key_hints(&kb)
                .iter()
                .any(|(key, description)| {
                    // Upstream renders `${key} ${description}` unconditionally
                    // (`keyHint`, keybinding-hints.ts): on Windows
                    // `app.suspend` has no default key ([] in keybindings.ts),
                    // so its row really is " to suspend" there too.
                    let expected = format!("{key} {description}");
                    row.trim_end() == expected
                })
        })
        .collect();
    assert_eq!(
        hint_rows.len(),
        pir::tui::app::expanded_key_hints(&kb).len(),
        "every expanded hint gets its own row: {expanded_text:?}"
    );
    for hint in EXPANDED_ONLY_HINTS {
        assert!(
            expanded_text.iter().any(|row| row.contains(hint)),
            "the expanded frame is missing the {hint:?} hint row: {expanded_text:?}"
        );
    }
    // The logo/compact/onboarding rows survive the toggle (upstream keeps them
    // around the expanded list).
    assert!(
        expanded_text.iter().any(|row| row.contains(" more")),
        "the compact row stays above the expanded list: {expanded_text:?}"
    );
    assert!(
        expanded_text
            .iter()
            .any(|row| row.contains("Pi can explain")),
        "the onboarding row stays below the expanded list: {expanded_text:?}"
    );

    // --- 3. collapsing again reproduces the collapsed rows exactly ----------
    let recollapsed = pir::tui::terminal::builtin_header_rows(false, &kb, width);
    assert_eq!(
        recollapsed, collapsed,
        "a second ctrl+o returns the header to the collapsed rows"
    );

    // --- 4. a fresh slot (`/clear`) renders the same rows -------------------
    // `/clear` keeps the process's header slot; the cell value is what decides
    // the shape, so a rebuilt slot must reproduce the same rows (this is the
    // slot's own build path, exercised through the shared builder).
    let rebuilt_collapsed = pir::tui::terminal::builtin_header_rows(false, &kb, width);
    let rebuilt_expanded = pir::tui::terminal::builtin_header_rows(true, &kb, width);
    assert_eq!(rebuilt_collapsed, collapsed);
    assert_eq!(rebuilt_expanded, expanded);
}

/// The SAME round trip through the real header SLOT (the live cell is read by
/// `render_builtin`, not by the test): ctrl+o's write is an atomic store, and
/// the slot must repaint on the next render. This is the seam that would have
/// caught the original defect — the pre-fix slot kept a construction-frozen
/// `Vec<LiveHeaderLine>`, so every render returned the collapsed rows no
/// matter what the cell said.
#[test]
fn live_header_slot_follows_the_tools_expanded_cell() {
    let _guard = super::tui_system_theme::lock();
    pir::tui::theme::set_theme("dark");
    let kb = Keybindings::new();
    let width = 100;
    // The shared cell Ctrl+O writes (`Action::ToolExpand` flips it; the
    // extension face reads/writes the same Arc).
    let cell = Arc::new(std::sync::atomic::AtomicBool::new(false));

    let collapsed = pir::tui::terminal::live_builtin_header_rows(Arc::clone(&cell), &kb, width);
    assert_eq!(
        collapsed,
        collapsed_header_rows_reference(&kb, width),
        "a fresh slot paints the pre-fix collapsed header byte for byte"
    );

    // ctrl+o: the TUI's own write path (`App::run_action` ToolExpand arm).
    cell.store(true, std::sync::atomic::Ordering::SeqCst);
    let expanded = pir::tui::terminal::live_builtin_header_rows(Arc::clone(&cell), &kb, width);
    assert_ne!(
        expanded, collapsed,
        "the slot must rebuild its rows from the live cell (ctrl+o has a visible effect)"
    );
    let expanded_text: Vec<String> = expanded.iter().map(|row| strip_ansi(row)).collect();
    for hint in EXPANDED_ONLY_HINTS {
        assert!(
            expanded_text.iter().any(|row| row.contains(hint)),
            "the live slot's expanded frame is missing {hint:?}: {expanded_text:?}"
        );
    }
    for (index, row) in expanded.iter().enumerate() {
        assert_header_row(row, width, &format!("live expanded row {index}"));
    }

    // A second ctrl+o: back to the collapsed shape, byte for byte.
    cell.store(false, std::sync::atomic::Ordering::SeqCst);
    let recollapsed = pir::tui::terminal::live_builtin_header_rows(Arc::clone(&cell), &kb, width);
    assert_eq!(
        recollapsed, collapsed,
        "a second ctrl+o returns the live slot to the collapsed rows"
    );
}
