//! TUI foundation tests: ANSI text utilities and the theme system.
//!
//! Behavior contracts are pinned against the upstream pi-tui TypeScript
//! implementation (`packages/tui/src/utils.ts`, coding-agent
//! `modes/interactive/theme/theme.ts` + `dark.json`/`light.json`).

use std::sync::{Mutex, MutexGuard};

use pir::tui::ansi::{
    apply_background_to_line, pad_to_width, strip_ansi, truncate_to_width, visible_width,
    wrap_text_with_ansi,
};
use pir::tui::theme::{
    set_color_mode_override, set_theme, set_theme_from_settings, with_theme, ThemeColor,
};

/// Serializes the tests that mutate the process-global theme/color mode (the
/// module state behind `set_theme`/`with_theme`). Every theme-touching test in
/// this binary takes it; without it a parallel `set_theme` can land between
/// another test's swap and its assertion.
static THEME_LOCK: Mutex<()> = Mutex::new(());

fn theme_lock() -> MutexGuard<'static, ()> {
    THEME_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

// ---------------------------------------------------------------------------
// visibleWidth
// ---------------------------------------------------------------------------

#[test]
fn visible_width_counts_plain_ascii() {
    assert_eq!(visible_width(""), 0);
    assert_eq!(visible_width("hello"), 5);
    assert_eq!(visible_width("a b"), 3);
}

#[test]
fn visible_width_skips_ansi_sequences() {
    assert_eq!(visible_width("\x1b[31mred\x1b[39m"), 3);
    assert_eq!(visible_width("\x1b[38;5;123mx\x1b[39m"), 1);
    assert_eq!(visible_width("\x1b[2K\x1b[31mx"), 1);
    // OSC hyperlink wrapper is zero-width.
    assert_eq!(visible_width("\x1b]8;;http://x\x07link\x1b]8;;\x07"), 4);
}

#[test]
fn visible_width_tabs_are_three_cells() {
    assert_eq!(visible_width("\t"), 3);
    assert_eq!(visible_width("a\tb"), 5);
}

#[test]
fn visible_width_wide_chars_count_two() {
    assert_eq!(visible_width("你好"), 4);
    assert_eq!(visible_width("a你b"), 4);
    // Combining marks are zero-width.
    assert_eq!(visible_width("e\u{0301}"), 1);
}

// ---------------------------------------------------------------------------
// truncateToWidth
// ---------------------------------------------------------------------------

#[test]
fn truncate_short_text_is_untouched() {
    assert_eq!(truncate_to_width("short", 10, "..."), "short");
    assert_eq!(truncate_to_width("exactly10c", 10, "..."), "exactly10c");
}

#[test]
fn truncate_appends_ellipsis_inside_budget() {
    assert_eq!(truncate_to_width("hello world", 8, "..."), "hello...");
    assert_eq!(truncate_to_width("hello world", 5, "..."), "he...");
}

#[test]
fn truncate_tiny_budget_clips_ellipsis() {
    // Ellipsis wider than the budget: the clipped ellipsis fills it
    // (upstream `truncateFragmentToWidth` behavior).
    assert_eq!(truncate_to_width("hello", 2, "..."), "..");
    assert_eq!(truncate_to_width("hello", 0, "..."), "");
}

#[test]
fn truncate_keeps_ansi_wrapped_content() {
    let styled = "\x1b[31mhello world\x1b[39m";
    let out = truncate_to_width(styled, 8, "...");
    assert_eq!(visible_width(&out), 8);
    assert!(out.starts_with("\x1b[31m"));
}

// ---------------------------------------------------------------------------
// wrapTextWithAnsi
// ---------------------------------------------------------------------------

#[test]
fn wrap_breaks_at_word_boundaries() {
    let wrapped = wrap_text_with_ansi("hello world", 8);
    assert_eq!(wrapped, vec!["hello", "world"]);
}

#[test]
fn wrap_breaks_long_tokens() {
    let wrapped = wrap_text_with_ansi("aaaaaaaaaaaa", 5);
    assert_eq!(wrapped.len(), 3);
    assert_eq!(strip_ansi(&wrapped[0]), "aaaaa");
    assert_eq!(strip_ansi(&wrapped[2]), "aa");
}

#[test]
fn wrap_carries_ansi_state_to_next_line() {
    // Upstream contract: a style opened before the wrap point continues on
    // the next line (ANSI tracker replays active codes).
    let wrapped = wrap_text_with_ansi("\x1b[31mhello world\x1b[39m", 8);
    assert!(wrapped.len() >= 2);
    assert!(
        wrapped[1].starts_with("\x1b[31m"),
        "second line lost style: {:?}",
        wrapped[1]
    );
    assert!(wrapped[1].contains("world"));
}

#[test]
fn wrap_handles_multiline_input() {
    let wrapped = wrap_text_with_ansi("one\ntwo", 10);
    assert_eq!(wrapped, vec!["one", "two"]);
}

#[test]
fn wrap_wide_chars_do_not_split() {
    // Characters are atomic: the break lands on a char boundary (upstream
    // `breakLongWord`), so no partial glyph ever appears.
    let wrapped = wrap_text_with_ansi("你好世界", 6);
    assert_eq!(wrapped.len(), 2, "{wrapped:?}");
    assert_eq!(strip_ansi(&wrapped.join("")), "你好世界");
    assert_eq!(visible_width(&wrapped[0]), 6);
    assert_eq!(visible_width(&wrapped[1]), 2);
}

// ---------------------------------------------------------------------------
// backgrounds & padding
// ---------------------------------------------------------------------------

#[test]
fn background_pads_to_width_before_painting() {
    let painted = apply_background_to_line("ab", 5, &|s| format!("[{s}]"));
    assert_eq!(painted, "[ab   ]");
}

#[test]
fn pad_to_width_fills_with_spaces() {
    assert_eq!(pad_to_width("ab", 5), "ab   ");
    assert_eq!(pad_to_width("abcde", 3), "abcde");
}

#[test]
fn strip_ansi_removes_all_sequences() {
    assert_eq!(strip_ansi("\x1b[1;32mx\x1b[0m"), "x");
    assert_eq!(strip_ansi("plain"), "plain");
}

// ---------------------------------------------------------------------------
// Theme
// ---------------------------------------------------------------------------

#[test]
fn dark_theme_resolves_builtin_palette() {
    let _guard = theme_lock();
    set_theme("dark");
    with_theme(|theme| {
        let fg = theme.fg(ThemeColor::Accent, "x");
        assert!(fg.starts_with("\x1b[38;"), "accent fg: {fg}");
        assert!(fg.ends_with("\x1b[39m"), "fg must reset with 39 only");
        // userMessageBg is the ChatGPT-style block background.
        let bg = theme.bg(ThemeColor::UserMessageBg, "x");
        assert!(bg.starts_with("\x1b[48;"), "userMessageBg: {bg}");
    });
}

#[test]
fn light_theme_switches_palette() {
    let _guard = theme_lock();
    set_theme("light");
    with_theme(|theme| {
        assert_eq!(theme.name, "light");
        // borderMuted in light = lightGray #b0b0b0.
        let fg = theme.fg(ThemeColor::BorderMuted, "x");
        assert!(fg.starts_with("\x1b[38;"), "{fg}");
    });
    set_theme("dark");
}

#[test]
fn truecolor_mode_produces_exact_hex_upstream_dark_text() {
    let _guard = theme_lock();
    // Upstream `dark.json` (packages/coding-agent/src/modes/interactive/theme/
    // dark.json) defines `text` as `okhsl(234 3% 89%)`, not a hex value. The
    // SGR below is that triple through this crate's converter; pinning it also
    // proves the global theme otherwise follows the host-detected capability
    // (a 256-color host would quantize the same color into the cube).
    set_color_mode_override(Some(true));
    set_theme("dark");
    with_theme(|theme| {
        assert_eq!(
            theme.fg(ThemeColor::UserMessageText, "body"),
            "\x1b[38;2;222;224;225mbody\x1b[39m",
            "truecolor must emit the exact upstream okhsl(234 3% 89%) text color"
        );
    });
    // Restore the detected mode and the default palette for the rest of the
    // binary.
    set_color_mode_override(None);
    set_theme("dark");
}

#[test]
fn settings_theme_selector_falls_back_to_dark() {
    let _guard = theme_lock();
    set_theme_from_settings(Some("light"));
    with_theme(|theme| assert_eq!(theme.name, "light"));
    set_theme_from_settings(Some("auto"));
    with_theme(|theme| assert_eq!(theme.name, "dark"));
    set_theme_from_settings(Some("nonexistent"));
    with_theme(|theme| {
        assert_eq!(theme.name, "nonexistent");
        // Falls back to the dark palette (dark branch is the default).
        let fg = theme.fg(ThemeColor::Accent, "x");
        assert!(fg.starts_with("[38;"));
    });
    set_theme("dark");
}

#[test]
fn thinking_border_levels_map_to_distinct_colors() {
    let _guard = theme_lock();
    set_theme("dark");
    with_theme(|theme| {
        let off = theme.thinking_border("off");
        let max = theme.thinking_border("max");
        assert_ne!(off, max, "off and max borders must differ");
        assert!(theme.bash_border().starts_with("\x1b[38;"));
    });
}
