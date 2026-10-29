//! Byte-level tests + bandwidth quantification for the scrolled-render line
//! diff (`pir::tui::core::scrolled_changed_rows` / `scrolled_diff_output` /
//! `scrolled_full_output`), covering the three scenarios the F7 review
//! called out:
//! 1. Row set unchanged (stable viewport while streaming): the diff rewrites
//!    nothing but the anchor row.
//! 2. Single changed row (spinner tick inside the dock): the diff rewrites
//!    one row.
//! 3. Whole-window shift (wheel scroll): every row is rewritten with
//!    absolute addressing — no `\x1b[2J` flicker.
//!
//! `scrolled_full_output` reproduces the pre-diff `render_scrolled` frame
//! byte-for-byte (`\x1b[2J\x1b[H` + every row), so its length is the
//! "before" figure and `scrolled_diff_output`'s the "after" one. Independent
//! binary: does not touch the `tests/g_*.rs` / `tests/suites/` contract area.

use pir::tui::core::{scrolled_changed_rows, scrolled_diff_output, scrolled_full_output};

const COLS: usize = 80;
const ROWS: usize = 24;

/// An 80-column padded transcript row with visible content derived from the
/// row number (no ANSI here; painted rows are compared as raw bytes either
/// way — ANSI sequences live inside the String and diff byte-wise).
fn row(index: usize) -> String {
    let label = format!("transcript line {index}");
    let mut line = String::from(&label);
    line.push_str(&" ".repeat(COLS - label.len()));
    line
}

/// A full scrolled window: 23 transcript rows + 1 dock row (the spinner /
/// status line the streaming loop repaints).
fn window(transcript_base: usize, spinner: &str) -> Vec<String> {
    let mut painted: Vec<String> = (transcript_base..transcript_base + ROWS - 1)
        .map(row)
        .collect();
    let dock = format!("spinner:{spinner}");
    let mut dock_line = String::from(&dock);
    dock_line.push_str(&" ".repeat(COLS - dock.len()));
    painted.push(dock_line);
    painted
}

// ---------------------------------------------------------------------------
// Scenario 1: row set unchanged (stable viewport, streaming appends absorbed
// by compensated_scroll_offset — the window content stays pinned, only the
// hidden spinner below changes nothing visible here).
// ---------------------------------------------------------------------------

#[test]
fn diff_unchanged_rows_rewrites_only_the_anchor_row() {
    let previous = window(100, "tick0");
    let painted = window(100, "tick0");
    let changed = scrolled_changed_rows(&previous, &painted);
    assert!(changed.is_empty());

    let out = scrolled_diff_output(&painted, &changed);
    // Synchronized-output brackets wrap the frame.
    assert!(out.starts_with("\x1b[?2026h"));
    assert!(out.ends_with("\x1b[?2026l"));
    // No clear-screen: nothing else on the display is touched.
    assert!(!out.contains("\x1b[2J"));
    // The unconditional anchor-row rewrite (visual no-op) is the only body.
    let last = ROWS;
    assert_eq!(
        out,
        format!(
            "\x1b[?2026h\x1b[{last};1H\x1b[2K{}{}",
            painted[last - 1],
            "\x1b[?2026l"
        )
    );
    // Bandwidth: anchor rewrite vs full repaint.
    let diff_bytes = out.len();
    let full_bytes = scrolled_full_output(&painted).len();
    assert!(
        diff_bytes < full_bytes / 10,
        "unchanged frame: diff {diff_bytes} bytes vs full {full_bytes} bytes"
    );
}

// ---------------------------------------------------------------------------
// Scenario 2: single changed row (spinner tick inside the dock).
// ---------------------------------------------------------------------------

#[test]
fn diff_single_changed_row_rewrites_one_row() {
    let previous = window(100, "tick0");
    let painted = window(100, "tick1");
    let changed = scrolled_changed_rows(&previous, &painted);
    assert_eq!(changed, vec![ROWS - 1]);

    let out = scrolled_diff_output(&painted, &changed);
    assert!(!out.contains("\x1b[2J"));
    // The transcript rows are untouched; only the dock row is addressed.
    assert!(out.contains(&format!("\x1b[{ROWS};1H\x1b[2K{}", painted[ROWS - 1])));
    // The anchor-row rewrite targets the same last row (deduplicated content,
    // written twice by design — end-state anchoring).
    assert_eq!(out.matches(&painted[ROWS - 1]).count(), 2);
    for transcript_row in painted.iter().take(ROWS - 1) {
        assert!(!out.contains(transcript_row.as_str()));
    }
    // Bandwidth: one row (written twice) vs the whole screen.
    let diff_bytes = out.len();
    let full_bytes = scrolled_full_output(&painted).len();
    assert!(
        diff_bytes < full_bytes / 5,
        "single-row frame: diff {diff_bytes} bytes vs full {full_bytes} bytes"
    );
}

// ---------------------------------------------------------------------------
// Scenario 3: whole-window shift (wheel scroll moves the window by one row;
// also what a *de-pinned* viewport would do every streaming frame).
// ---------------------------------------------------------------------------

#[test]
fn diff_window_shift_rewrites_every_row_without_clearing() {
    let previous = window(100, "tick0");
    let painted = window(101, "tick0"); // window slid one row down the transcript
    let changed = scrolled_changed_rows(&previous, &painted);
    // Every row differs (the bottom row happens to be equal — the dock did
    // not move — so the shift changes ROWS-1 rows; assert the full-slide
    // shape explicitly).
    assert_eq!(changed, (0..ROWS - 1).collect::<Vec<usize>>());

    let out = scrolled_diff_output(&painted, &changed);
    // Absolute addressing, no full clear: shift redraws rows in place.
    assert!(!out.contains("\x1b[2J"));
    for &r in &changed {
        assert!(out.contains(&format!("\x1b[{};1H\x1b[2K{}", r + 1, painted[r])));
    }
    // A de-pinned shift-by-one every frame would cost this per frame; assert
    // it stays in the same order of magnitude as a full repaint (no savings
    // on a full shift — the win is flicker removal and the streaming case,
    // where pinning keeps the diff in scenario 1/2 territory).
    let diff_bytes = out.len();
    let full_bytes = scrolled_full_output(&painted).len();
    assert!(
        diff_bytes < full_bytes * 2 && full_bytes < diff_bytes * 2,
        "shift frame: diff {diff_bytes} bytes vs full {full_bytes} bytes"
    );
}

// ---------------------------------------------------------------------------
// Safety: non-diffable frames still take the full-repaint builder, and the
// full builder reproduces the pre-diff frame byte-for-byte.
// ---------------------------------------------------------------------------

#[test]
fn non_diffable_frames_use_the_full_repaint_builder() {
    // Row-count mismatch (first scrolled frame, resize, invalidate) → the
    // row-diff reports every painted row; the renderer then takes
    // `scrolled_full_output`, which is exactly the pre-diff render_scrolled
    // frame: sync + `\x1b[2J\x1b[H` + every row (2K-prefixed, \r\n-joined).
    let previous: Vec<String> = Vec::new();
    let painted = window(100, "tick0");
    assert_eq!(
        scrolled_changed_rows(&previous, &painted),
        (0..ROWS).collect::<Vec<usize>>()
    );

    let full = scrolled_full_output(&painted);
    let mut expected = String::from("\x1b[?2026h\x1b[2J\x1b[H");
    for (index, line) in painted.iter().enumerate() {
        if index > 0 {
            expected.push_str("\r\n");
        }
        expected.push_str("\x1b[2K");
        expected.push_str(line);
    }
    expected.push_str("\x1b[?2026l");
    assert_eq!(full, expected);
    assert_eq!(
        // 8 (sync open) + 4 (\x1b[2J) + 3 (\x1b[H) + per row: 4 (\x1b[2K) + 80
        // content + 23 × 2 (\r\n joins) + 8 (sync close).
        full.len(),
        8 + 4 + 3 + ROWS * 4 + (ROWS - 1) * 2 + ROWS * COLS + 8
    );
}

#[test]
fn diff_with_ansi_rows_compares_bytes_including_escapes() {
    // Painted rows carry ANSI SGR sequences; equality is raw byte equality on
    // the painted String, so an attribute-only change counts as changed.
    let mut previous = window(100, "tick0");
    let painted = window(100, "tick0");
    previous[3] = format!("\x1b[32m{}\x1b[0m", previous[3]);
    let changed = scrolled_changed_rows(&previous, &painted);
    assert_eq!(changed, vec![3]);
    let out = scrolled_diff_output(&painted, &changed);
    assert!(out.contains(&format!("\x1b[4;1H\x1b[2K{}", painted[3])));
    // Exactly six ESC introducers: open sync, the row-4 addressing AND its
    // 2K clear, the anchor-row addressing AND its 2K clear, close sync
    // (painted rows here carry none).
    assert_eq!(out.matches("\x1b[").count(), 6);
}
