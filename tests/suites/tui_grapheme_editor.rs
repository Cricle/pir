//! Grapheme-cluster editing — cursor motion, deletion, word navigation,
//! wrapping, the width math and selection slicing all operate on
//! `Intl.Segmenter` grapheme clusters (upstream `utils.ts` `graphemeWidth` /
//! `visibleWidth` / `truncateToWidth` / `sliceByColumn`,
//! `components/editor.ts` `handleBackspace` / `handleForwardDelete` /
//! `moveCursor` / `wordWrapLine`, `word-navigation.ts`).
//!
//! The expected values below are the upstream behavior probed against the
//! real `packages/tui/src` code on Node (`Intl.Segmenter` +
//! `get-east-asian-width` + `graphemeWidth` / `truncateToWidth`). pir stores
//! cursor columns as Unicode-scalar indices (upstream stores UTF-16
//! indices), so the stops are expressed in scalars.

use pir::tui::ansi::{
    grapheme_clusters, grapheme_spans, strip_ansi, truncate_to_width, visible_width,
};
use pir::tui::editor::Editor;

/// 👨‍👩‍👧‍👦 — one cluster, seven scalars (four emoji + three ZWJ), two cells.
const FAMILY: &str = "\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}\u{200D}\u{1F466}";
/// 👍🏽 — emoji + skin-tone modifier: one cluster.
const THUMB_SKIN: &str = "\u{1F44D}\u{1F3FD}";
/// 🇺🇸 — a regional-indicator flag pair.
const FLAG_US: &str = "\u{1F1FA}\u{1F1F8}";
/// 1️⃣ — keycap sequence.
const KEYCAP_1: &str = "1\u{FE0F}\u{20E3}";
/// é (decomposed: e + combining acute).
const E_ACUTE: &str = "e\u{0301}";
/// 각 as jamo: L + V + T (one Hangul cluster).
const JAMO: &str = "\u{1100}\u{1161}\u{11A8}";

// ---------------------------------------------------------------------------
// Width: one cluster contributes its own width, never the sum of its scalars
// (upstream `graphemeWidth`, `utils.ts:182-245`).
// ---------------------------------------------------------------------------

#[test]
fn grapheme_widths_match_upstream() {
    let cases: &[(&str, usize)] = &[
        // Emoji sequences are ONE double-width cluster.
        (FAMILY, 2),
        (THUMB_SKIN, 2),
        (FLAG_US, 2),
        (KEYCAP_1, 2),
        ("\u{1F1FA}", 2),        // lone regional indicator
        ("\u{1F3FD}", 2),        // lone skin-tone modifier
        ("\u{2764}\u{FE0F}", 2), // ❤️ (VS16)
        ("\u{2764}", 1),         // ❤ text presentation
        ("\u{263A}\u{FE0F}", 2), // ☺️
        ("\u{2602}\u{FE0F}", 2), // ☂️
        ("\u{00A9}\u{FE0F}", 2), // ©️
        ("\u{00A9}", 1),         // © text presentation
        ("\u{231A}", 2),         // ⌚ emoji presentation
        ("1\u{FE0F}", 1),        // digit + VS16 is NOT an RGI emoji
        ("#\u{20E3}", 1),        // keycap without VS16 stays narrow
        (
            "\u{1F3F4}\u{E0067}\u{E0062}\u{E0073}\u{E0063}\u{E0074}\u{E007F}",
            2,
        ), // tag flag
        ("\u{1F44D}\u{0301}", 2), // emoji + combining mark
        ("\u{6F22}\u{1F3FD}", 2), // CJK + skin tone: base width only
        ("\u{1F44D}\u{20E3}", 2), // emoji + enclosing keycap
        // Combining marks ride their base.
        (E_ACUTE, 1),
        ("\u{0301}", 0),
        ("\u{200D}", 0),
        // CJK / Hangul / Indic.
        ("\u{6F22}", 2),
        ("\u{6F22}\u{5B57}", 4),
        (JAMO, 2),
        ("\u{AC01}", 2),
        ("\u{0915}\u{093F}", 2), // कि — ka + spacing vowel sign
        ("\u{0915}", 1),
        ("\u{0E01}\u{0E33}", 2), // Thai ko kai + AM
        ("\u{0E33}", 1),
        ("\u{FF8A}", 1), // halfwidth katakana
    ];
    for (text, want) in cases {
        assert_eq!(
            visible_width(text),
            *want,
            "visible_width({text:?}) should be {want}"
        );
    }
    // Multi-cluster strings sum their clusters.
    assert_eq!(visible_width(&format!("a{FAMILY}b")), 4);
    assert_eq!(visible_width(&format!("{E_ACUTE}{E_ACUTE}")), 2);
}

#[test]
fn grapheme_segmentation_matches_intl_segmenter() {
    // Cluster counts probed from `Intl.Segmenter` (granularity: grapheme).
    let cases: &[(&str, usize)] = &[
        (FAMILY, 1),
        (THUMB_SKIN, 1),
        (FLAG_US, 1),
        (KEYCAP_1, 1),
        (E_ACUTE, 1),
        (JAMO, 1),
        ("\u{0915}\u{093F}", 1),            // कि
        ("\u{0915}\u{094D}\u{0915}", 1),    // क्क conjunct
        ("\u{1F1FA}\u{1F1F8}\u{1F1EB}", 2), // flag + lone RI
        ("\u{1F468}\u{1F469}", 2),          // two emoji without ZWJ
        ("a\u{0301}\u{200D}b", 2),          // mark + ZWJ sticks to `a`
    ];
    for (text, want) in cases {
        assert_eq!(
            grapheme_clusters(text).len(),
            *want,
            "cluster count of {text:?}"
        );
    }
    // Char-index spans of the family: one span covering all seven scalars.
    assert_eq!(grapheme_spans(FAMILY), vec![(0, 7)]);
    assert_eq!(
        grapheme_spans(&format!("a{FAMILY}b")),
        vec![(0, 1), (1, 7), (8, 1)]
    );
}

// ---------------------------------------------------------------------------
// truncate_to_width never splits a cluster (upstream `truncateToWidth`,
// `utils.ts:1121-1257` — probed values below).
// ---------------------------------------------------------------------------

#[test]
fn truncate_to_width_keeps_clusters_whole() {
    let line = format!("ab{FAMILY}cd");
    // 4 cells: `ab` + the whole family (it does not fit `abc`).
    assert_eq!(truncate_to_width(&line, 4, ""), format!("ab{FAMILY}"));
    // Ellipsis cases from the upstream probe.
    assert_eq!(truncate_to_width(&line, 3, "\u{2026}"), "ab\u{2026}");
    assert_eq!(truncate_to_width(&line, 2, "\u{2026}"), "a\u{2026}");

    // A cluster wider than the budget is dropped whole (upstream returns "").
    assert_eq!(truncate_to_width("\u{1F3FD}\u{1F3FD}", 1, ""), "");
    assert_eq!(truncate_to_width("\u{6F22}\u{5B57}", 1, ""), "");
    assert_eq!(truncate_to_width("\u{6F22}\u{5B57}", 2, ""), "\u{6F22}");
    assert_eq!(truncate_to_width(&format!("{FLAG_US}x"), 1, ""), "");
    assert_eq!(truncate_to_width(&format!("{FLAG_US}x"), 2, ""), FLAG_US);

    // Combining sequences clip at the cluster boundary, never mid-cluster.
    assert_eq!(
        truncate_to_width(&format!("{E_ACUTE}{E_ACUTE}"), 1, ""),
        E_ACUTE
    );
    assert_eq!(
        truncate_to_width(&format!("aa{THUMB_SKIN}bb"), 3, "\u{2026}"),
        "aa\u{2026}"
    );
    // ANSI codes are preserved and still cluster-safe.
    let styled = format!("\x1b[31m{FAMILY}\x1b[0mxy");
    let clipped = truncate_to_width(&styled, 3, "");
    assert!(
        clipped.starts_with("\x1b[31m"),
        "style preserved: {clipped:?}"
    );
    assert!(clipped.contains(FAMILY), "cluster kept whole: {clipped:?}");
    // The family occupies two of the three cells, so the following `x` still
    // fits (upstream probe: `\x1b[31m<family>\x1b[0mx`, visible width 3).
    assert!(
        clipped.contains('x'),
        "the next cluster still fits: {clipped:?}"
    );
    assert_eq!(visible_width(&clipped), 3);
}

// ---------------------------------------------------------------------------
// Cursor motion / deletion step by whole clusters
// (upstream `moveCursor(0, ±1)`, `handleBackspace`, `handleForwardDelete`).
// ---------------------------------------------------------------------------

/// Walk `steps` moves to the right, collecting the scalar cursor columns.
fn right_stops(editor: &Editor, steps: usize) -> Vec<usize> {
    let mut stops = Vec::new();
    for _ in 0..steps {
        let before = editor.state_snapshot();
        stops.push(before.cursor_col);
        editor.move_cursor_right();
        if editor.state_snapshot().cursor_col == before.cursor_col {
            break;
        }
    }
    stops
}

/// Walk `steps` moves to the left, collecting the scalar cursor columns.
fn left_stops(editor: &Editor, steps: usize) -> Vec<usize> {
    let mut stops = Vec::new();
    for _ in 0..steps {
        let state = editor.state_snapshot();
        stops.push(state.cursor_col);
        editor.move_cursor_left();
        if editor.state_snapshot().cursor_col == state.cursor_col {
            break;
        }
    }
    stops
}

#[test]
fn cursor_motion_steps_over_emoji_clusters() {
    // Upstream probe: right = 0,1,8,9 / left = 9,8,1,0 for "a" + family + "b".
    let editor = Editor::new();
    editor.set_text(&format!("a{FAMILY}b"));
    editor.move_cursor_home();
    assert_eq!(right_stops(&editor, 4), vec![0, 1, 8, 9]);
    let editor = Editor::new();
    editor.set_text(&format!("a{FAMILY}b"));
    editor.move_cursor_end();
    assert_eq!(left_stops(&editor, 4), vec![9, 8, 1, 0]);

    // Skin tone / flag / keycap / combining / Hangul clusters.
    for (line, want_right, want_left) in [
        (
            format!("a{THUMB_SKIN}b"),
            vec![0, 1, 3, 4],
            vec![4, 3, 1, 0],
        ),
        (format!("a{FLAG_US}b"), vec![0, 1, 3, 4], vec![4, 3, 1, 0]),
        (format!("a{KEYCAP_1}b"), vec![0, 1, 4, 5], vec![5, 4, 1, 0]),
        (format!("{E_ACUTE}x"), vec![0, 2, 3], vec![3, 2, 0]),
        (format!("{JAMO}x"), vec![0, 3, 4], vec![4, 3, 0]),
    ] {
        let editor = Editor::new();
        editor.set_text(&line);
        editor.move_cursor_home();
        assert_eq!(right_stops(&editor, 4), want_right, "right over {line:?}");
        let editor = Editor::new();
        editor.set_text(&line);
        editor.move_cursor_end();
        assert_eq!(left_stops(&editor, 4), want_left, "left over {line:?}");
    }
}

#[test]
fn backspace_deletes_one_whole_cluster() {
    // Upstream: backspace segments the text before the cursor and removes the
    // last grapheme (`editor.ts:1392-1400`).
    for (line, want) in [
        (format!("a{FAMILY}b"), format!("a{FAMILY}")),
        (format!("a{THUMB_SKIN}b"), format!("a{THUMB_SKIN}")),
        (format!("a{FLAG_US}b"), format!("a{FLAG_US}")),
        (format!("a{KEYCAP_1}b"), format!("a{KEYCAP_1}")),
        (format!("{JAMO}x"), JAMO.to_string()),
    ] {
        let editor = Editor::new();
        editor.set_text(&line);
        editorial_backspace_at_end(&editor);
        assert_eq!(editor.get_text(), want, "backspace at end of {line:?}");
    }
    // Deleting the second cluster of the family leaves the whole family.
    let editor = Editor::new();
    editor.set_text(&format!("a{FAMILY}b"));
    editor.move_cursor_home();
    editor.move_cursor_right(); // after `a`
    editor.move_cursor_right(); // past the whole family
    editor.backspace();
    assert_eq!(editor.get_text(), "ab");
    assert_eq!(editor.state_snapshot().cursor_col, 1);
}

/// Press backspace with the cursor at the end of the text.
fn editorial_backspace_at_end(editor: &Editor) {
    editor.move_cursor_end();
    editor.backspace();
}

#[test]
fn forward_delete_removes_one_whole_cluster() {
    let editor = Editor::new();
    editor.set_text(&format!("a{FAMILY}b"));
    editor.move_cursor_home();
    editor.move_cursor_right(); // after `a`
    editor.delete_char_forward();
    assert_eq!(editor.get_text(), "ab");
    assert_eq!(editor.state_snapshot().cursor_col, 1);

    for (line, want) in [
        (format!("a{THUMB_SKIN}b"), "ab".to_string()),
        (format!("a{FLAG_US}b"), "ab".to_string()),
        (format!("a{KEYCAP_1}b"), "ab".to_string()),
        (format!("a{E_ACUTE}b"), "ab".to_string()),
    ] {
        let editor = Editor::new();
        editor.set_text(&line);
        editor.move_cursor_home();
        editor.move_cursor_right();
        editor.delete_char_forward();
        assert_eq!(editor.get_text(), want, "forward delete in {line:?}");
    }
}

// ---------------------------------------------------------------------------
// Word navigation treats a cluster as one unit and keeps marks inside words
// (upstream `word-navigation.ts` over `Intl.Segmenter` word segments).
// ---------------------------------------------------------------------------

fn word_left_stops(editor: &Editor, steps: usize) -> Vec<usize> {
    let mut stops = Vec::new();
    for _ in 0..steps {
        let col = editor.state_snapshot().cursor_col;
        stops.push(col);
        editor.move_word_left();
        if editor.state_snapshot().cursor_col == col {
            break;
        }
    }
    stops
}

fn word_right_stops(editor: &Editor, steps: usize) -> Vec<usize> {
    let mut stops = Vec::new();
    for _ in 0..steps {
        let col = editor.state_snapshot().cursor_col;
        stops.push(col);
        editor.move_word_right();
        if editor.state_snapshot().cursor_col == col {
            break;
        }
    }
    stops
}

#[test]
fn word_motion_matches_upstream_stops() {
    let cases: &[(&str, &[usize], &[usize])] = &[
        // "cafe\u0301 bar": the combining acute stays inside the word.
        ("cafe\u{0301} bar", &[9, 6, 0], &[0, 5, 9]),
        // Emoji cluster is one non-word segment.
        ("a\u{1F44D}\u{1F3FD}b", &[4, 3, 1, 0], &[0, 1, 3, 4]),
        ("x\u{1F468}\u{200D}\u{1F469}y", &[5, 4, 1, 0], &[0, 1, 4, 5]),
        // CJK + ASCII split at the script boundary.
        ("\u{6F22}\u{5B57}abc", &[5, 2, 0], &[0, 2, 5]),
        ("a\u{0301}b", &[3, 0], &[0, 3]),
        // Paths keep their connectors inside the word run.
        (
            "src/ma\u{0301}in.rs",
            &[12, 10, 9, 4, 3, 0],
            &[0, 3, 4, 9, 10, 12],
        ),
        ("one two", &[7, 4, 0], &[0, 3, 7]),
        // `_` (UAX #29 ExtendNumLet) joins a non-CJK word run on either
        // side; `MidLetter` connectors still need word characters on both
        // sides, so `a_.b` splits after the underscore.
        ("_abc", &[4, 0], &[0, 4]),
        ("abc_", &[4, 0], &[0, 4]),
        ("a_b", &[3, 0], &[0, 3]),
        ("a_.b", &[4, 3, 2, 0], &[0, 2, 3, 4]),
        ("\u{6F22}_abc", &[5, 1, 0], &[0, 1, 5]),
    ];
    for (line, back, fwd) in cases {
        let editor = Editor::new();
        editor.set_text(line);
        editor.move_cursor_end();
        assert_eq!(
            word_left_stops(&editor, back.len()),
            *back,
            "word-left in {line:?}"
        );
        let editor = Editor::new();
        editor.set_text(line);
        editor.move_cursor_home();
        assert_eq!(
            word_right_stops(&editor, fwd.len()),
            *fwd,
            "word-right in {line:?}"
        );
    }
}

// ---------------------------------------------------------------------------
// Rendering: wrapping never splits a cluster and the cursor highlights the
// whole cluster under it (upstream `wordWrapLine` + `renderFakeCursor`).
// ---------------------------------------------------------------------------

#[test]
fn word_wrap_never_splits_a_cluster() {
    let editor = Editor::new();
    editor.set_text(&format!("aaa {FAMILY} bbb"));
    let (lines, _, _) = editor.render_with_cursor_rows(12, 10);
    let mut saw_family = false;
    for line in &lines {
        let text = strip_ansi(line);
        let fragments = text
            .chars()
            .filter(|c| "\u{1F468}\u{1F469}\u{1F467}\u{1F466}\u{200D}".contains(*c))
            .count();
        if fragments > 0 {
            assert!(text.contains(FAMILY), "row splits the ZWJ family: {text:?}");
            saw_family = true;
        }
    }
    assert!(saw_family, "the family renders on some row: {lines:?}");
    // Every row still measures exactly the requested width.
    for line in &lines {
        assert_eq!(visible_width(line), 12, "row width: {line:?}");
    }
}

#[test]
fn cursor_highlights_the_whole_cluster() {
    let editor = Editor::new();
    editor.set_text(&format!("ab{FAMILY}cd"));
    editor.move_cursor_home();
    editor.move_cursor_right();
    editor.move_cursor_right(); // before the family
    let (lines, _, cursor_col) = editor.render_with_cursor_rows(40, 5);
    let row = lines
        .iter()
        .find(|line| line.contains("\x1b[7m"))
        .expect("a fake-cursor row");
    assert!(
        row.contains(&format!("\x1b[7m{FAMILY}\x1b[0m")),
        "cursor covers the whole cluster: {row:?}"
    );
    // The hardware-cursor column is the cell where the cluster starts
    // (paddingX 1 + the two ASCII cells before it).
    assert_eq!(cursor_col, 3);
}

#[test]
fn camera_column_accounts_for_wide_clusters() {
    // The cursor column is a VISIBLE column: the family occupies two cells,
    // so the cursor after it sits at scalar 9 (2 + 7) but visible column 5
    // (paddingX 1 + `ab` + the family's two cells).
    let editor = Editor::new();
    editor.set_text(&format!("ab{FAMILY}"));
    editor.move_cursor_end();
    let (_, _, cursor_col) = editor.render_with_cursor_rows(40, 5);
    assert_eq!(editor.state_snapshot().cursor_col, 9);
    assert_eq!(cursor_col, 5);
}

// ---------------------------------------------------------------------------
// Selection: column ranges cut only at cluster boundaries (upstream
// `sliceByColumn(line, start, length, true)`, `utils.ts:1263-1265`).
// ---------------------------------------------------------------------------

#[test]
fn selection_columns_keep_clusters_whole() {
    use pir::tui::altscreen_search::{selection_text, slice_columns, SelectionSpan};

    // Cells: `a` 0, the family 1..3, `b` 3.
    let line = format!("a{FAMILY}b");
    assert_eq!(slice_columns(&line, 0, 1), "a");
    assert_eq!(slice_columns(&line, 1, 3), FAMILY);
    assert_eq!(slice_columns(&line, 3, 4), "b");
    // Strict boundary: a range that would halve the family drops it whole.
    assert_eq!(slice_columns(&line, 1, 2), "");
    assert_eq!(slice_columns(&line, 2, 4), "b");
    // Wide CJK cells behave the same way.
    assert_eq!(slice_columns("\u{6F22}\u{5B57}x", 0, 2), "\u{6F22}");
    assert_eq!(slice_columns("\u{6F22}\u{5B57}x", 2, 5), "\u{5B57}x");

    // `selection_text` (the copy path) extracts the cluster, not a fragment.
    let frame = vec![line];
    let span = SelectionSpan {
        start: (0, 1),
        end: (0, 3),
    };
    assert_eq!(selection_text(&frame, &span).as_deref(), Some(FAMILY));
}

#[test]
fn selection_paint_wraps_whole_clusters() {
    use pir::tui::altscreen_search::splice_columns;

    // The highlight covers the family's two cells: the whole cluster is
    // wrapped, not just the first code point.
    let line = format!("a{FAMILY}b");
    assert_eq!(
        splice_columns(&line, &[(1, 3, "\x1b[7m", "\x1b[27m")]),
        format!("a\x1b[7m{FAMILY}\x1b[27mb")
    );
    // Wide CJK cells: a one-cell range still wraps the whole cluster.
    let cjk = "\u{6F22}\u{5B57}x";
    assert_eq!(
        splice_columns(cjk, &[(0, 2, "\x1b[7m", "\x1b[27m")]),
        "\x1b[7m\u{6F22}\x1b[27m\u{5B57}x"
    );
    assert_eq!(
        splice_columns(cjk, &[(0, 3, "\x1b[7m", "\x1b[27m")]),
        "\x1b[7m\u{6F22}\u{5B57}\x1b[27mx"
    );
}
