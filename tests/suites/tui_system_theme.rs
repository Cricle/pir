//! System theme + terminal color reporting + theme hot-reload — the port of
//! upstream `terminal-colors.ts`, `system-theme.ts` and `theme.ts`'s watcher.
//!
//! Every expected value in the derivation tests was probed from the upstream
//! TypeScript implementation with node (the same inputs through
//! `generateSystemThemeColors` / `terminalAppearance` / `parseOscColorResponse`
//! / `parseTerminalColorSchemeReport`), so a table transcription error fails
//! here rather than shipping a subtly different theme.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use pir::tui::theme::{
    all_theme_entries, apply_terminal_colors, current_theme_name, detect_terminal_theme,
    generate_system_theme_colors, get_terminal_theme, is_device_attributes_response,
    is_known_theme, mark_terminal_colors_pending, parse_osc_color_response, parse_osc_color_value,
    parse_terminal_color_scheme_report, parse_theme_json, register_custom_theme, reload_theme_file,
    set_terminal_color_scheme, set_terminal_colors, set_theme, set_theme_change_listener,
    start_theme_watcher, terminal_appearance, terminal_colors, terminal_colors_pending,
    theme_appearance, theme_snapshot, wcag_contrast, with_theme, Appearance, ColorValue,
    OscColorTarget, SystemThemeInput, TerminalColorDelivery, TerminalColorFeed,
    TerminalColorQueries, TerminalColorQuery, TerminalColors, ThemeColor, ThemeWatcherPlan,
    DARK_PALETTE, SYSTEM_THEME_NAME, TERMINAL_COLOR_QUERY, TERMINAL_COLOR_REPLY_COUNT,
    TERMINAL_COLOR_SCHEME_NOTIFY_OFF, TERMINAL_COLOR_SCHEME_NOTIFY_ON, TERMINAL_PALETTE_SIZE,
    THEME_RELOAD_DEBOUNCE,
};

/// Serializes the tests that touch the process-global theme/terminal-color
/// state (upstream's module state). The lock is the WHOLE `g_tui` binary's
/// theme lock: every sibling suite that swaps the process-global theme and
/// then asserts on what a render observed must take it too
/// (`tui_regressions` does via `super::tui_system_theme::lock()`), otherwise a
/// parallel swap lands inside another test's window.
static GLOBAL_LOCK: Mutex<()> = Mutex::new(());

/// The `g_tui`-binary-wide theme lock (see [`GLOBAL_LOCK`]).
pub fn lock() -> std::sync::MutexGuard<'static, ()> {
    GLOBAL_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn rgb(hex: &str) -> (u8, u8, u8) {
    parse_osc_color_value(hex).expect("probe color")
}

/// The Dracula terminal the upstream test uses (system-theme.test.ts:14-19).
fn dracula() -> TerminalColors {
    TerminalColors {
        foreground: Some(rgb("#f8f8f2")),
        background: Some(rgb("#282a36")),
        palette: Some(
            [
                "#21222c", "#ff5555", "#50fa7b", "#f1fa8c", "#bd93f9", "#ff79c6", "#8be9fd",
                "#f8f8f2", "#6272a4", "#ff6e6e", "#69ff94", "#ffffa5", "#d6acff", "#ff92df",
                "#a4ffff", "#ffffff",
            ]
            .iter()
            .map(|hex| rgb(hex))
            .collect(),
        ),
    }
}

fn solarized_light() -> TerminalColors {
    TerminalColors {
        foreground: Some(rgb("#657b83")),
        background: Some(rgb("#fdf6e3")),
        palette: None,
    }
}

fn system_input(colors: &TerminalColors) -> SystemThemeInput {
    SystemThemeInput::from_terminal_colors(colors)
}

/// The generated color of one token, as the probe printed it.
fn token(input: &SystemThemeInput, token: &str) -> String {
    match generate_system_theme_colors(input).colors.get(token) {
        Some(ColorValue::Hex(hex)) => hex.clone(),
        Some(ColorValue::Index(index)) => format!("color:{index}"),
        Some(ColorValue::Empty) => String::new(),
        other => panic!("unexpected generated value for {token}: {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// terminal-colors.ts: OSC 10 / 11 / 4 replies
// ---------------------------------------------------------------------------

#[test]
fn osc_color_replies_parse_into_their_targets() {
    let foreground = parse_osc_color_response("\x1b]10;rgb:ffff/0000/8080\x07").expect("OSC 10");
    assert_eq!(foreground.target, OscColorTarget::Foreground);
    assert_eq!(foreground.rgb, Some((255, 0, 128)));

    let background = parse_osc_color_response("\x1b]11;#1a2b3c\x1b\\").expect("OSC 11 ST");
    assert_eq!(background.target, OscColorTarget::Background);
    assert_eq!(background.rgb, Some((26, 43, 60)));

    let palette = parse_osc_color_response("\x1b]4;1;rgb:eaea/6c6c/6d6d\x07").expect("OSC 4");
    assert_eq!(palette.target, OscColorTarget::Palette(1));
    assert_eq!(palette.rgb, Some((234, 108, 109)));

    // Uppercase prefixes and 16-bit channels.
    let upper = parse_osc_color_response("\x1b]11;RGB:ffff/0000/8080\x07").expect("uppercase");
    assert_eq!(upper.rgb, Some((255, 0, 128)));
    assert_eq!(
        parse_osc_color_response("\x1b]11;#0000ffff8000\x07")
            .expect("16-bit hex")
            .rgb,
        Some((0, 255, 128))
    );
    // Three-digit channels scale to 0-255.
    assert_eq!(
        parse_osc_color_response("\x1b]11;rgb:cc/22/11\x07")
            .expect("8-bit channels")
            .rgb,
        Some((204, 34, 17))
    );
}

#[test]
fn osc_color_replies_reject_malformed_and_trailing_input() {
    // An unparseable color is still a reply for its target (upstream returns
    // the target with `rgb: undefined`).
    let bad = parse_osc_color_response("\x1b]11;#zzzzzz\x07").expect("target only");
    assert_eq!(bad.target, OscColorTarget::Background);
    assert_eq!(bad.rgb, None);

    // Trailing bytes break the anchored match.
    assert!(parse_osc_color_response("\x1b]11;#1a2b3c\x07x").is_none());
    // A palette index above 255 is reported as given (upstream `\d{1,3}`).
    assert_eq!(
        parse_osc_color_response("\x1b]4;300;rgb:00/00/00\x07").map(|reply| reply.target),
        Some(OscColorTarget::Palette(300))
    );
    // Four digits is not a palette index.
    assert!(parse_osc_color_response("\x1b]4;1234;rgb:00/00/00\x07").is_none());
    // An unterminated reply is not a reply.
    assert!(parse_osc_color_response("\x1b]11;rgb:ff/00/00").is_none());
    // Other OSC sequences are not color replies.
    assert!(parse_osc_color_response("\x1b[?997;1n").is_none());
    assert!(parse_osc_color_response("nope").is_none());
    assert!(parse_osc_color_response("").is_none());

    // Empty payload: a reply with no color.
    let empty = parse_osc_color_response("\x1b]10;\x07").expect("empty payload");
    assert_eq!(empty.target, OscColorTarget::Foreground);
    assert_eq!(empty.rgb, None);
}

#[test]
fn osc_color_values_follow_the_upstream_forms() {
    assert_eq!(parse_osc_color_value("  #1a2b3c  "), Some((26, 43, 60)));
    assert_eq!(parse_osc_color_value("rgba:ff/00/ff"), Some((255, 0, 255)));
    assert_eq!(parse_osc_color_value("rgb:1/2/3"), Some((17, 34, 51)));
    // Extra channels are ignored (upstream destructures the first three).
    assert_eq!(parse_osc_color_value("rgb:ff/00/00/80"), Some((255, 0, 0)));
    assert_eq!(parse_osc_color_value("#12345"), None);
    assert_eq!(parse_osc_color_value("rgb:gg/00/00"), None);
    assert_eq!(parse_osc_color_value(""), None);
}

#[test]
fn mode_2031_reports_parse_and_the_last_one_wins() {
    assert_eq!(
        parse_terminal_color_scheme_report("\x1b[?997;1n"),
        Some(Appearance::Dark)
    );
    assert_eq!(
        parse_terminal_color_scheme_report("\x1b[?997;2n"),
        Some(Appearance::Light)
    );
    // A repeated group: the last report decides (`match[1]`).
    assert_eq!(
        parse_terminal_color_scheme_report("\x1b[?997;1n\x1b[?997;2n"),
        Some(Appearance::Light)
    );
    assert_eq!(parse_terminal_color_scheme_report("\x1b[?997;3n"), None);
    assert_eq!(parse_terminal_color_scheme_report("\x1b[?997;1nx"), None);
    assert_eq!(parse_terminal_color_scheme_report(""), None);
}

#[test]
fn device_attributes_reply_ends_the_color_query() {
    assert!(is_device_attributes_response("\x1b[?1;2c"));
    assert!(is_device_attributes_response("\x1b[?c"));
    assert!(!is_device_attributes_response("\x1b[?997;1n"));
    assert!(!is_device_attributes_response("\x1b[?1;2c\x07"));
    assert!(!is_device_attributes_response(""));
}

#[test]
fn the_query_string_matches_upstream() {
    let expected = format!(
        "\x1b]10;?\x07\x1b]11;?\x07{}",
        (0..TERMINAL_PALETTE_SIZE)
            .map(|index| format!("\x1b]4;{index};?\x07"))
            .collect::<String>()
    );
    assert_eq!(TERMINAL_COLOR_QUERY, format!("{expected}\x1b[c"));
    assert_eq!(TERMINAL_COLOR_REPLY_COUNT, 18);
    assert_eq!(TERMINAL_COLOR_SCHEME_NOTIFY_ON, "\x1b[?2031h");
    assert_eq!(TERMINAL_COLOR_SCHEME_NOTIFY_OFF, "\x1b[?2031l");
}

// ---------------------------------------------------------------------------
// tui.ts: reply bookkeeping (split replies, late replies, degradation)
// ---------------------------------------------------------------------------

#[test]
fn replies_split_across_reads_accumulate_until_the_da_marker() {
    let mut query = TerminalColorQuery::new();
    // A partial reply is not a reply yet; the reader re-feeds the whole
    // accumulation, so the same target is never counted twice.
    assert_eq!(query.feed("\x1b]11;#1a2b"), TerminalColorFeed::NotAReply);
    assert_eq!(
        query.feed("\x1b]11;#1a2b3c\x07"),
        TerminalColorFeed::Consumed
    );
    assert_eq!(query.colors().background, Some((26, 43, 60)));
    assert_eq!(
        query.feed("\x1b]11;#1a2b3c\x07"),
        TerminalColorFeed::Consumed,
        "a re-fed duplicate is ignored"
    );

    // The palette is only reported when every entry arrived.
    for index in 0..TERMINAL_PALETTE_SIZE - 1 {
        let feed = query.feed(&format!("\x1b]4;{index};rgb:ff/00/00\x07"));
        assert_eq!(feed, TerminalColorFeed::Consumed, "palette {index}");
    }
    assert_eq!(query.colors().palette, None, "15 of 16 is not a palette");
    assert_eq!(
        query.feed("\x1b]4;15;rgb:ff/00/00\x07"),
        TerminalColorFeed::Consumed,
        "16 of 18 targets: still waiting for foreground"
    );
    assert_eq!(
        query.colors().palette.as_ref().map(Vec::len),
        Some(TERMINAL_PALETTE_SIZE)
    );

    // The DA1 reply completes the query with everything reported so far.
    match query.feed("\x1b[?1;2c") {
        TerminalColorFeed::Delivered(colors) => {
            assert_eq!(colors.background, Some((26, 43, 60)));
            assert_eq!(colors.palette.as_ref().map(Vec::len), Some(16));
        }
        other => panic!("DA1 must complete the query: {other:?}"),
    }
    assert!(query.is_complete());
    // Later replies are swallowed (upstream leaves the completed query in
    // front until DA1 shifted it).
    assert_eq!(
        query.feed("\x1b]10;rgb:ff/ff/ff\x07"),
        TerminalColorFeed::Consumed
    );
}

#[test]
fn all_eighteen_replies_complete_without_da1() {
    let mut query = TerminalColorQuery::new();
    assert_eq!(
        query.feed("\x1b]10;#ffffff\x07"),
        TerminalColorFeed::Consumed
    );
    assert_eq!(
        query.feed("\x1b]11;#000000\x07"),
        TerminalColorFeed::Consumed
    );
    let mut delivered = None;
    for index in 0..TERMINAL_PALETTE_SIZE {
        if let TerminalColorFeed::Delivered(colors) =
            query.feed(&format!("\x1b]4;{index};rgb:ff/ff/ff\x07"))
        {
            delivered = Some(colors);
        }
    }
    let colors = delivered.expect("the 18th reply completes the query");
    assert_eq!(colors.foreground, Some((255, 255, 255)));
    assert_eq!(colors.background, Some((0, 0, 0)));
    assert_eq!(colors.palette.as_ref().map(Vec::len), Some(16));
}

#[test]
fn late_replies_after_the_timeout_still_deliver() {
    let mut query = TerminalColorQuery::new();
    assert_eq!(
        query.feed("\x1b]11;#282a36\x07"),
        TerminalColorFeed::Consumed
    );
    // The query window closed: the partial result is delivered, and the query
    // keeps collecting (upstream `deliver = onLateReply`).
    let partial = query.timeout();
    assert_eq!(partial.background, Some((40, 42, 54)));
    assert!(query.timed_out());
    assert!(!query.is_complete());

    for index in 0..TERMINAL_PALETTE_SIZE {
        assert_eq!(
            query.feed(&format!("\x1b]4;{index};rgb:00/00/00\x07")),
            TerminalColorFeed::Consumed
        );
    }
    // The late DA1 delivers the completed colors again.
    match query.feed("\x1b[?1;2c") {
        TerminalColorFeed::Delivered(colors) => {
            assert_eq!(colors.background, Some((40, 42, 54)));
            assert_eq!(colors.palette.as_ref().map(Vec::len), Some(16));
        }
        other => panic!("a late DA1 must deliver: {other:?}"),
    }
}

#[test]
fn a_query_that_never_answers_delivers_nothing_and_keeps_the_previous_colors() {
    // The timeout delivers the colors reported so far (none).
    let mut query = TerminalColorQuery::new();
    let colors = query.timeout();
    assert!(colors.is_empty());
    // A report that misses fields keeps the recorded ones
    // (upstream `applyTerminalColors`).
    let previous = TerminalColors {
        foreground: Some((1, 2, 3)),
        background: Some((4, 5, 6)),
        palette: Some(vec![(0, 0, 0); TERMINAL_PALETTE_SIZE]),
    };
    let merged = previous.merge_reported(&TerminalColors::default());
    assert_eq!(merged, previous);
    // A report that fills a field overrides just that field.
    let merged = previous.merge_reported(&TerminalColors {
        background: Some((9, 9, 9)),
        ..TerminalColors::default()
    });
    assert_eq!(merged.foreground, Some((1, 2, 3)));
    assert_eq!(merged.background, Some((9, 9, 9)));
}

#[test]
fn only_the_oldest_query_consumes_replies() {
    let mut queries = TerminalColorQueries::default();
    queries.begin_query();
    assert_eq!(queries.pending_count(), 1);
    assert!(matches!(
        queries.feed("\x1b]11;#010203\x07"),
        TerminalColorDelivery::Consumed
    ));
    // DA1 completes and drops the query.
    match queries.feed("\x1b[?1;2c") {
        TerminalColorDelivery::Delivered(colors) => {
            assert_eq!(colors.background, Some((1, 2, 3)));
        }
        other => panic!("DA1 must deliver: {other:?}"),
    }
    assert_eq!(queries.pending_count(), 0);
    // Without a query, replies are not consumed at all.
    assert_eq!(
        queries.feed("\x1b]11;#010203\x07"),
        TerminalColorDelivery::NotAReply
    );
    // The non-DA1 completion path leaves the query in place (upstream only
    // shifts on DA1), so later replies are swallowed.
    queries.begin_query();
    for index in 0..TERMINAL_PALETTE_SIZE {
        queries.feed(&format!("\x1b]4;{index};rgb:01/02/03\x07"));
    }
    assert!(matches!(
        queries.feed("\x1b]10;rgb:01/02/03\x07"),
        TerminalColorDelivery::Consumed
    ));
    assert_eq!(queries.pending_count(), 1);
    assert!(matches!(
        queries.feed("\x1b]10;rgb:04/05/06\x07"),
        TerminalColorDelivery::Consumed
    ));
}

// ---------------------------------------------------------------------------
// system-theme.ts: appearance
// ---------------------------------------------------------------------------

#[test]
fn terminal_appearance_prefers_the_foreground_direction() {
    assert_eq!(
        terminal_appearance(rgb("#282a36"), Some(rgb("#f8f8f2"))),
        Appearance::Dark
    );
    assert_eq!(
        terminal_appearance(rgb("#fdf6e3"), Some(rgb("#657b83"))),
        Appearance::Light
    );
    assert_eq!(
        terminal_appearance(rgb("#ffffff"), Some(rgb("#000000"))),
        Appearance::Light
    );
    // No foreground: white-on-background contrast decides.
    assert_eq!(terminal_appearance(rgb("#808080"), None), Appearance::Light);
    assert_eq!(terminal_appearance(rgb("#1e1e1e"), None), Appearance::Dark);

    // WCAG ratios probed from the upstream implementation.
    assert!((wcag_contrast((255, 255, 255), rgb("#282a36")) - 14.240403179767634).abs() < 1e-9);
    assert!((wcag_contrast((0, 0, 0), (255, 255, 255)) - 21.0).abs() < 1e-9);
}

#[test]
fn terminal_theme_falls_back_background_scheme_colorfgbg_dark() {
    let dracula = dracula();
    assert_eq!(
        detect_terminal_theme(&dracula, Some(Appearance::Light), Some(Appearance::Light)),
        Appearance::Dark,
        "a reported background wins over the scheme report"
    );
    let no_background = TerminalColors::default();
    assert_eq!(
        detect_terminal_theme(
            &no_background,
            Some(Appearance::Light),
            Some(Appearance::Dark)
        ),
        Appearance::Light,
        "the mode-2031 report is the second source"
    );
    assert_eq!(
        detect_terminal_theme(&no_background, None, Some(Appearance::Light)),
        Appearance::Light,
        "COLORFGBG is the third"
    );
    assert_eq!(
        detect_terminal_theme(&no_background, None, None),
        Appearance::Dark,
        "dark is the fallback"
    );
}

// ---------------------------------------------------------------------------
// system-theme.ts: generation (values probed from the upstream TS)
// ---------------------------------------------------------------------------

#[test]
fn dracula_generates_the_probed_dark_theme() {
    let input = system_input(&dracula());
    let generated = generate_system_theme_colors(&input);
    assert_eq!(generated.appearance, Some(Appearance::Dark));
    assert!(
        generated.dim.is_empty(),
        "a colored theme needs no dim tokens"
    );
    assert_eq!(generated.colors.len(), 56, "every token is generated");

    // Body text uses the terminal's own foreground ("" = terminal default).
    assert_eq!(token(&input, "text"), "");
    assert_eq!(token(&input, "userMessageText"), "");
    assert_eq!(token(&input, "toolTitle"), "");
    // Panels and accents keep the palette hues.
    assert_eq!(token(&input, "error"), "#f9756f");
    assert_eq!(token(&input, "accent"), "#f16cb9");
    assert_eq!(token(&input, "border"), "#ae85e9");
    assert_eq!(token(&input, "success"), "#39c15c");
    assert_eq!(token(&input, "warning"), "#a4ab3b");
    assert_eq!(token(&input, "mdHeading"), "#a4ab3b");
    assert_eq!(token(&input, "selectedBg"), "#45236c");
    assert_eq!(token(&input, "toolPendingBg"), "#31384c");
    assert_eq!(token(&input, "toolErrorBg"), "#5f2220");
    assert_eq!(token(&input, "muted"), "#96a2c7");
    assert_eq!(token(&input, "dim"), "#7484b5");
    assert_eq!(token(&input, "borderMuted"), "#6c7daf");
    assert_eq!(token(&input, "scrollbarTrack"), "#424d6e");
    assert_eq!(token(&input, "scrollbarThumb"), "#8f9cc4");
    assert_eq!(token(&input, "thinkingOff"), "#6272a4");
    assert_eq!(token(&input, "thinkingMax"), "#ff5555");
    assert_eq!(token(&input, "syntaxString"), "#39c15c");
}

#[test]
fn solarized_light_darkens_its_unreadable_foreground() {
    let input = system_input(&solarized_light());
    let generated = generate_system_theme_colors(&input);
    assert_eq!(generated.appearance, Some(Appearance::Light));
    // The terminal foreground is below 4.5:1 on its own background, so body
    // text is darkened instead of taken from it.
    assert_eq!(token(&input, "text"), "#4a5a60");
    assert_eq!(token(&input, "toolTitle"), "#49595f");
    assert_eq!(token(&input, "muted"), "#677075");
    assert_eq!(token(&input, "dim"), "#869095");
    assert_eq!(token(&input, "accent"), "#755ab5");
    assert_eq!(token(&input, "success"), "#337f59");
    assert_eq!(token(&input, "borderMuted"), "#9aa3a7");
    assert_eq!(token(&input, "selectedBg"), "#dfe7eb");
}

#[test]
fn a_background_without_a_palette_uses_the_family_hues() {
    let input = SystemThemeInput {
        background: Some(rgb("#1e1e1e")),
        ..SystemThemeInput::default()
    };
    let generated = generate_system_theme_colors(&input);
    assert_eq!(generated.appearance, Some(Appearance::Dark));
    assert_eq!(token(&input, "text"), "#dddfe0");
    assert_eq!(token(&input, "muted"), "#99a1a6");
    assert_eq!(token(&input, "dim"), "#7a8489");
    assert_eq!(token(&input, "accent"), "#a493d6");
    assert_eq!(token(&input, "error"), "#eb777a");
    assert_eq!(token(&input, "mdHeading"), "#c9961c");
    assert_eq!(token(&input, "selectedBg"), "#1f343f");
    assert_eq!(token(&input, "toolErrorBg"), "#4a2425");
    assert_eq!(token(&input, "scrollbarTrack"), "#44494c");
}

#[test]
fn a_mid_gray_background_relaxes_levels_to_stay_readable() {
    let input = SystemThemeInput {
        background: Some(rgb("#808080")),
        foreground: Some(rgb("#ffffff")),
        ..SystemThemeInput::default()
    };
    let generated = generate_system_theme_colors(&input);
    assert_eq!(generated.appearance, Some(Appearance::Light));
    // Every level collapsed to the strongest reachable color.
    assert_eq!(token(&input, "text"), "#000000");
    assert_eq!(token(&input, "muted"), "#000000");
    assert_eq!(token(&input, "dim"), "#000000");
    assert_eq!(token(&input, "accent"), "#000000");
    assert_eq!(token(&input, "selectedBg"), "#317c9e");
    assert_eq!(token(&input, "toolErrorBg"), "#dc2d45");
}

#[test]
fn zero_saturation_renders_grayscale() {
    let input = SystemThemeInput {
        saturation: Some(0.0),
        ..system_input(&dracula())
    };
    assert_eq!(token(&input, "error"), "#a3a3a3");
    assert_eq!(token(&input, "accent"), "#a3a3a3");
}

#[test]
fn body_text_reaches_the_wcag_minimum_everywhere() {
    for input in [
        system_input(&dracula()),
        system_input(&solarized_light()),
        SystemThemeInput {
            background: Some(rgb("#1e1e1e")),
            ..SystemThemeInput::default()
        },
        SystemThemeInput {
            background: Some(rgb("#808080")),
            foreground: Some(rgb("#ffffff")),
            ..SystemThemeInput::default()
        },
    ] {
        let generated = generate_system_theme_colors(&input);
        let color = |token_name: &str, fallback: (u8, u8, u8)| -> (u8, u8, u8) {
            match generated.colors.get(token_name) {
                Some(ColorValue::Hex(hex)) => rgb(hex),
                Some(ColorValue::Empty) => fallback,
                _ => panic!("token {token_name} has no color"),
            }
        };
        let background = input.background.expect("probed background");
        let text = color("text", input.foreground.unwrap_or(background));
        assert!(
            wcag_contrast(text, background) >= 4.5,
            "text on the background: {}",
            wcag_contrast(text, background)
        );
        let selected = color("selectedBg", background);
        assert!(
            wcag_contrast(text, selected) >= 4.5,
            "text on selectedBg: {}",
            wcag_contrast(text, selected)
        );
        let tool_title = color("toolTitle", input.foreground.unwrap_or(background));
        let tool_error = color("toolErrorBg", background);
        assert!(
            wcag_contrast(tool_title, tool_error) >= 4.5,
            "toolTitle on toolErrorBg: {}",
            wcag_contrast(tool_title, tool_error)
        );
    }
}

// ---------------------------------------------------------------------------
// Degradation without replies: the indexed tier
// ---------------------------------------------------------------------------

#[test]
fn no_reported_colors_fall_back_to_palette_indices_and_faint_text() {
    let generated = generate_system_theme_colors(&SystemThemeInput {
        appearance_hint: Some(Appearance::Light),
        ..SystemThemeInput::default()
    });
    assert_eq!(generated.appearance, Some(Appearance::Light));
    assert_eq!(generated.colors.get("error"), Some(&ColorValue::Index(1)));
    assert_eq!(generated.colors.get("accent"), Some(&ColorValue::Index(5)));
    assert_eq!(generated.colors.get("border"), Some(&ColorValue::Index(4)));
    assert_eq!(
        generated.colors.get("syntaxNumber"),
        Some(&ColorValue::Index(5))
    );
    assert_eq!(
        generated.colors.get("thinkingXhigh"),
        Some(&ColorValue::Index(13))
    );
    assert_eq!(generated.colors.get("text"), Some(&ColorValue::Empty));
    assert_eq!(
        generated.colors.get("userMessageBg"),
        Some(&ColorValue::Empty)
    );
    assert_eq!(
        generated.colors.get("toolPendingBg"),
        Some(&ColorValue::Empty)
    );

    // Neutral tokens below body text render faint instead of bright black.
    for token_name in [
        "muted",
        "dim",
        "scrollbarTrack",
        "thinkingOff",
        "borderMuted",
    ] {
        assert!(
            generated.dim.contains(&dim_color(token_name)),
            "{token_name} must be faint"
        );
    }
    for token_name in ["text", "userMessageText", "toolTitle"] {
        assert!(!generated.dim.contains(&dim_color(token_name)));
    }

    // Zero saturation keeps the ANSI indices off too (the grayscale first
    // frame of the system theme).
    let grayscale = generate_system_theme_colors(&SystemThemeInput {
        saturation: Some(0.0),
        ..SystemThemeInput::default()
    });
    assert_eq!(grayscale.colors.get("error"), Some(&ColorValue::Empty));
    assert_eq!(
        grayscale.appearance, None,
        "no hint, no colors, no appearance"
    );
}

fn dim_color(token_name: &str) -> ThemeColor {
    *all_dim_colors()
        .get(token_name)
        .unwrap_or_else(|| panic!("{token_name} is not a theme color"))
}

/// Maps a token key to its [`ThemeColor`] through the public surface: the
/// theme's own `dim` list is the only place a generated dim token becomes a
/// `ThemeColor`, so the reverse map is built from the token keys.
fn all_dim_colors() -> &'static std::collections::HashMap<String, ThemeColor> {
    static MAP: std::sync::OnceLock<std::collections::HashMap<String, ThemeColor>> =
        std::sync::OnceLock::new();
    MAP.get_or_init(|| {
        let mut map = std::collections::HashMap::new();
        // `Theme::key` is crate-private; the mapping is reproduced from the
        // token key spelling (each variant's key is its lowerCamelCase name).
        for (key, color) in [
            ("accent", ThemeColor::Accent),
            ("border", ThemeColor::Border),
            ("borderAccent", ThemeColor::BorderAccent),
            ("borderMuted", ThemeColor::BorderMuted),
            ("success", ThemeColor::Success),
            ("error", ThemeColor::Error),
            ("warning", ThemeColor::Warning),
            ("muted", ThemeColor::Muted),
            ("dim", ThemeColor::Dim),
            ("text", ThemeColor::Text),
            ("thinkingText", ThemeColor::ThinkingText),
            ("selectedBg", ThemeColor::SelectedBg),
            ("scrollbarTrack", ThemeColor::ScrollbarTrack),
            ("scrollbarThumb", ThemeColor::ScrollbarThumb),
            ("searchMatchBg", ThemeColor::SearchMatchBg),
            ("searchMatchText", ThemeColor::SearchMatchText),
            ("userMessageBg", ThemeColor::UserMessageBg),
            ("userMessageText", ThemeColor::UserMessageText),
            ("customMessageBg", ThemeColor::CustomMessageBg),
            ("customMessageText", ThemeColor::CustomMessageText),
            ("customMessageLabel", ThemeColor::CustomMessageLabel),
            ("toolPendingBg", ThemeColor::ToolPendingBg),
            ("toolSuccessBg", ThemeColor::ToolSuccessBg),
            ("toolErrorBg", ThemeColor::ToolErrorBg),
            ("toolTitle", ThemeColor::ToolTitle),
            ("toolOutput", ThemeColor::ToolOutput),
            ("mdHeading", ThemeColor::MdHeading),
            ("mdLink", ThemeColor::MdLink),
            ("mdLinkUrl", ThemeColor::MdLinkUrl),
            ("mdCode", ThemeColor::MdCode),
            ("mdCodeBlock", ThemeColor::MdCodeBlock),
            ("mdCodeBlockBorder", ThemeColor::MdCodeBlockBorder),
            ("mdQuote", ThemeColor::MdQuote),
            ("mdQuoteBorder", ThemeColor::MdQuoteBorder),
            ("mdHr", ThemeColor::MdHr),
            ("mdListBullet", ThemeColor::MdListBullet),
            ("toolDiffAdded", ThemeColor::ToolDiffAdded),
            ("toolDiffRemoved", ThemeColor::ToolDiffRemoved),
            ("toolDiffContext", ThemeColor::ToolDiffContext),
            ("syntaxComment", ThemeColor::SyntaxComment),
            ("syntaxKeyword", ThemeColor::SyntaxKeyword),
            ("syntaxFunction", ThemeColor::SyntaxFunction),
            ("syntaxVariable", ThemeColor::SyntaxVariable),
            ("syntaxString", ThemeColor::SyntaxString),
            ("syntaxNumber", ThemeColor::SyntaxNumber),
            ("syntaxType", ThemeColor::SyntaxType),
            ("syntaxOperator", ThemeColor::SyntaxOperator),
            ("syntaxPunctuation", ThemeColor::SyntaxPunctuation),
            ("thinkingOff", ThemeColor::ThinkingOff),
            ("thinkingMinimal", ThemeColor::ThinkingMinimal),
            ("thinkingLow", ThemeColor::ThinkingLow),
            ("thinkingMedium", ThemeColor::ThinkingMedium),
            ("thinkingHigh", ThemeColor::ThinkingHigh),
            ("thinkingXhigh", ThemeColor::ThinkingXhigh),
            ("thinkingMax", ThemeColor::ThinkingMax),
            ("bashMode", ThemeColor::BashMode),
        ] {
            map.insert(key.to_string(), color);
        }
        map
    })
}

// ---------------------------------------------------------------------------
// The system theme through the global theme surface
// ---------------------------------------------------------------------------

#[test]
fn system_theme_uses_the_terminal_defaults_and_faint_tokens_without_colors() {
    let _guard = lock();
    set_terminal_colors(TerminalColors::default());
    set_terminal_color_scheme(Some(Appearance::Dark));
    set_theme(SYSTEM_THEME_NAME);
    let checked = with_theme(|theme| {
        (
            theme.name.clone(),
            theme.get_fg_ansi(ThemeColor::Text),
            theme.fg(ThemeColor::Muted, "x"),
            theme.fg(ThemeColor::Accent, "y"),
        )
    });
    assert_eq!(checked.0, SYSTEM_THEME_NAME);
    assert_eq!(checked.1, "\x1b[39m", "body text is the terminal default");
    assert_eq!(
        checked.2, "\x1b[39m\x1b[2mx\x1b[22;39m",
        "faint neutral tokens (upstream system-theme.test.ts)"
    );
    assert!(
        checked.3.starts_with("\x1b[38;5;5m"),
        "the accent is the palette index: {}",
        checked.3
    );
}

#[test]
fn system_theme_follows_the_recorded_terminal_colors() {
    let _guard = lock();
    set_terminal_colors(dracula());
    set_theme(SYSTEM_THEME_NAME);
    let checked = with_theme(|theme| {
        (
            theme.name.clone(),
            theme.appearance,
            theme.get_fg_ansi(ThemeColor::Text),
            theme.get_fg_ansi(ThemeColor::Error),
            theme.resolved_color_hex(ThemeColor::Accent),
        )
    });
    assert_eq!(checked.0, SYSTEM_THEME_NAME);
    assert_eq!(checked.1, Appearance::Dark);
    assert_eq!(checked.2, "\x1b[39m");
    assert!(
        checked.3.starts_with("\x1b[38;"),
        "the error token is a generated color: {}",
        checked.3
    );
    assert_eq!(checked.4.as_deref(), Some("#f16cb9"));
    assert_eq!(theme_appearance(), Appearance::Dark);
}

#[test]
fn a_late_color_report_regenerates_the_system_theme() {
    let _guard = lock();
    set_terminal_colors(dracula());
    set_theme(SYSTEM_THEME_NAME);
    let before = with_theme(|theme| theme.resolved_color_hex(ThemeColor::Accent));
    assert_eq!(before.as_deref(), Some("#f16cb9"));

    // Upstream `applyTerminalColors`: the light report replaces the colors and
    // `reapplyForTerminal` regenerates the system theme.
    let changed = apply_terminal_colors(&TerminalColors {
        foreground: Some(rgb("#657b83")),
        background: Some(rgb("#fdf6e3")),
        palette: None,
    });
    assert!(changed, "a different report changes the theme");
    assert_eq!(current_theme_name(), SYSTEM_THEME_NAME);
    assert_eq!(get_terminal_theme(), Appearance::Light);
    let after = with_theme(|theme| {
        (
            theme.appearance,
            theme.resolved_color_hex(ThemeColor::Accent),
        )
    });
    assert_eq!(after.0, Appearance::Light);
    assert_ne!(after.1.as_deref(), before.as_deref());

    // An identical report is a no-op (upstream `sameTerminalColors`).
    assert!(!apply_terminal_colors(&TerminalColors {
        foreground: Some(rgb("#657b83")),
        background: Some(rgb("#fdf6e3")),
        palette: None,
    }));

    // Restore for the other suites in this binary.
    set_terminal_colors(TerminalColors::default());
    set_terminal_color_scheme(None);
    set_theme("dark");
}

#[test]
fn the_system_theme_is_a_known_theme_without_a_file() {
    assert!(is_known_theme(SYSTEM_THEME_NAME));
    let entries = all_theme_entries();
    let system = entries
        .iter()
        .find(|entry| entry.name == SYSTEM_THEME_NAME)
        .expect("system is listed");
    assert!(system.path.is_none(), "the generated theme has no file");
    assert!(entries[0].path.is_none(), "built-ins still come first");

    // The extension snapshot surface resolves the generated colors.
    let snapshot = theme_snapshot(SYSTEM_THEME_NAME).expect("system snapshot");
    assert_eq!(snapshot.name, SYSTEM_THEME_NAME);
    assert_eq!(snapshot.colors.len(), 56);
}

// ---------------------------------------------------------------------------
// theme.ts: the hot-reload watcher
// ---------------------------------------------------------------------------

fn theme_json(name: &str, accent: &str) -> String {
    let mut vars = serde_json::Map::new();
    for (var_name, value) in DARK_PALETTE.vars {
        vars.insert(var_name.to_string(), serde_json::json!(value));
    }
    let mut colors = serde_json::Map::new();
    for (color_name, value) in DARK_PALETTE.colors {
        colors.insert(color_name.to_string(), serde_json::json!(value));
    }
    colors.insert("accent".to_string(), serde_json::json!(accent));
    serde_json::json!({ "name": name, "vars": vars, "colors": colors }).to_string()
}

#[test]
fn the_watcher_only_watches_existing_custom_theme_files() {
    let dir = std::env::temp_dir().join("pir-theme-plan");
    let exists = |path: &Path| path == dir.join("mine.json");

    assert!(!ThemeWatcherPlan::start("dark", &dir, &exists).is_watching());
    assert!(!ThemeWatcherPlan::start("light", &dir, &exists).is_watching());
    assert!(!ThemeWatcherPlan::start(SYSTEM_THEME_NAME, &dir, &exists).is_watching());
    // A custom theme whose file does not exist (extension-registered) is not
    // watched either.
    assert!(!ThemeWatcherPlan::start("missing", &dir, &exists).is_watching());

    let plan = ThemeWatcherPlan::start("mine", &dir, &exists);
    assert!(plan.is_watching());
    assert_eq!(plan.name(), Some("mine"));
    assert_eq!(plan.path(), Some(dir.join("mine.json").as_path()));
}

#[test]
fn the_watcher_debounces_a_burst_and_reloads_once() {
    let dir = std::env::temp_dir().join("pir-theme-plan");
    let exists = |_path: &Path| true;
    let mut plan = ThemeWatcherPlan::start("mine", &dir, &exists);
    let start = Instant::now();
    assert_eq!(THEME_RELOAD_DEBOUNCE, Duration::from_millis(100));

    plan.on_fs_event("mine", Some("mine.json"), start);
    assert!(plan.is_pending());
    // A second event restarts the debounce (upstream clears the timer).
    plan.on_fs_event("mine", Some("mine.json"), start + Duration::from_millis(50));
    assert_eq!(
        plan.due_at(),
        Some(start + Duration::from_millis(50) + THEME_RELOAD_DEBOUNCE)
    );
    assert!(!plan.is_due(start + Duration::from_millis(120)));
    assert_eq!(
        plan.take_due("mine", start + Duration::from_millis(120), &exists),
        None
    );
    assert!(plan.is_pending(), "an early check keeps the reload armed");
    assert_eq!(
        plan.take_due("mine", start + Duration::from_millis(150), &exists),
        Some(dir.join("mine.json"))
    );
    assert!(!plan.is_pending(), "a fired reload disarms the debounce");
}

#[test]
fn the_watcher_ignores_other_files_stale_timers_and_missing_files() {
    let dir = std::env::temp_dir().join("pir-theme-plan");
    let exists = |_path: &Path| true;
    let mut plan = ThemeWatcherPlan::start("mine", &dir, &exists);
    let start = Instant::now();

    // Another theme's file is not the watched one.
    plan.on_fs_event("mine", Some("other.json"), start);
    assert!(!plan.is_pending());
    // An event without a file name belongs to the watched directory.
    plan.on_fs_event("mine", None, start);
    assert!(plan.is_pending());
    // An event after the active theme changed is ignored (the armed reload
    // from the previous event stays untouched).
    let armed = plan.due_at();
    plan.on_fs_event(
        "other",
        Some("mine.json"),
        start + Duration::from_millis(20),
    );
    assert_eq!(
        plan.due_at(),
        armed,
        "a foreign current theme ignores events"
    );
    // A stale timer that fires after the switch is dropped.
    plan.on_fs_event("mine", Some("mine.json"), start);
    assert_eq!(
        plan.take_due("other", start + THEME_RELOAD_DEBOUNCE, &exists),
        None,
        "the stale guard drops it"
    );
    assert!(!plan.is_pending());
    // A file that is temporarily missing keeps the last good theme.
    plan.on_fs_event("mine", Some("mine.json"), start);
    assert_eq!(
        plan.take_due("mine", start + THEME_RELOAD_DEBOUNCE, &|_path| false),
        None
    );

    // Stopping the watcher drops any armed reload.
    plan.on_fs_event("mine", Some("mine.json"), start);
    plan.stop();
    assert!(!plan.is_watching());
    assert!(!plan.is_pending());
    assert_eq!(
        plan.take_due("mine", start + THEME_RELOAD_DEBOUNCE, &exists),
        None
    );
}

#[test]
fn reloading_a_theme_file_replaces_the_registry_entry_and_swaps_the_theme() {
    let _guard = lock();
    let dir = std::env::temp_dir().join(format!("pir-theme-reload-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("temp dir");
    let path: PathBuf = dir.join("pir-reload-theme.json");
    std::fs::write(&path, theme_json("pir-reload-theme", "#123456")).expect("write");

    let theme = parse_theme_json(&theme_json("pir-reload-theme", "#123456")).expect("parse");
    register_custom_theme(theme);
    set_theme("pir-reload-theme");
    assert_eq!(
        with_theme(|theme| theme.resolved_color_hex(ThemeColor::Accent)).as_deref(),
        Some("#123456")
    );

    let fired = Arc::new(AtomicBool::new(false));
    {
        let fired = Arc::clone(&fired);
        set_theme_change_listener(Arc::new(move || {
            fired.store(true, Ordering::SeqCst);
        }));
    }

    // The editor writes the file: the reload re-reads it and swaps the theme.
    std::fs::write(&path, theme_json("pir-reload-theme", "#abcdef")).expect("rewrite");
    let name = reload_theme_file(&path).expect("reload");
    assert_eq!(name, "pir-reload-theme");
    assert_eq!(current_theme_name(), "pir-reload-theme");
    assert_eq!(
        with_theme(|theme| theme.resolved_color_hex(ThemeColor::Accent)).as_deref(),
        Some("#abcdef")
    );
    assert!(fired.load(Ordering::SeqCst), "the change callback fired");
    assert_eq!(
        theme_snapshot("pir-reload-theme")
            .expect("snapshot")
            .colors
            .get("accent")
            .map(String::as_str),
        Some("#abcdef")
    );

    // A file in an invalid state while the editor writes keeps the last good
    // theme (upstream ignores the parse error).
    std::fs::write(&path, "{ not json").expect("write invalid");
    assert!(reload_theme_file(&path).is_err());
    assert_eq!(
        with_theme(|theme| theme.resolved_color_hex(ThemeColor::Accent)).as_deref(),
        Some("#abcdef")
    );

    set_theme("dark");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_running_watcher_reloads_a_changed_theme_file() {
    let _guard = lock();
    let dir = std::env::temp_dir().join(format!("pir-theme-live-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("temp dir");
    let path = dir.join("pir-live-theme.json");
    std::fs::write(&path, theme_json("pir-live-theme", "#110000")).expect("write");
    let theme = parse_theme_json(&theme_json("pir-live-theme", "#110000")).expect("parse");
    register_custom_theme(theme);
    set_theme("pir-live-theme");

    let handle = start_theme_watcher("pir-live-theme", &dir).expect("watcher");
    // Give the backend a moment to install the watch before the first write.
    std::thread::sleep(Duration::from_millis(200));

    // The stale-timer guard drops a reload when another test in this binary
    // switched the theme meanwhile, so each attempt re-asserts the watched
    // theme before rewriting the file and then polls the reloaded registry
    // entry (which updates independently of the active theme).
    let snapshot_accent = || {
        theme_snapshot("pir-live-theme").and_then(|snapshot| snapshot.colors.get("accent").cloned())
    };
    let deadline = Instant::now() + Duration::from_secs(8);
    let mut attempt = 0u32;
    let mut seen = 0u32;
    while Instant::now() < deadline && seen < 2 {
        attempt += 1;
        let accent = format!("#{:02x}0000", 0x10 + attempt);
        set_theme("pir-live-theme");
        std::fs::write(&path, theme_json("pir-live-theme", &accent)).expect("rewrite");
        let step = Instant::now() + Duration::from_millis(600);
        while Instant::now() < step {
            if snapshot_accent().as_deref() == Some(accent.as_str()) {
                seen += 1;
                break;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
    }
    handle.stop();
    set_theme("dark");
    let _ = std::fs::remove_dir_all(&dir);
    assert!(
        seen >= 2,
        "the watcher reloaded the changed file ({seen} reloads observed)"
    );
}

#[test]
fn graceful_degradation_when_no_query_can_run() {
    // The query is skipped without a TTY (this test process) and when
    // `PIR_DISABLE_TERMINAL_QUERY` is set, and the gate is not consumed by a
    // failed check (`tests/theme_once_semantics.rs` covers the gate itself).
    let previous = std::env::var_os("PIR_DISABLE_TERMINAL_QUERY");
    std::env::set_var("PIR_DISABLE_TERMINAL_QUERY", "1");
    assert!(pir::tui::theme::query_terminal_colors(Duration::from_millis(10)).is_none());
    match previous {
        Some(value) => std::env::set_var("PIR_DISABLE_TERMINAL_QUERY", value),
        None => std::env::remove_var("PIR_DISABLE_TERMINAL_QUERY"),
    }

    // The settings path degrades to the indexed tier: pending clears, the
    // appearance falls back to the environment, and a theme still applies.
    let _guard = lock();
    mark_terminal_colors_pending();
    assert!(terminal_colors_pending());
    apply_terminal_colors(&TerminalColors::default());
    assert!(!terminal_colors_pending());
    assert!(terminal_colors().is_empty());
    set_theme(SYSTEM_THEME_NAME);
    assert_eq!(current_theme_name(), SYSTEM_THEME_NAME);
    assert_eq!(with_theme(|theme| theme.appearance), get_terminal_theme());
    set_theme("dark");
}

#[test]
fn osc11_helper_agrees_with_the_shared_parser() {
    for reply in [
        "\x1b]11;rgb:1c1c/1c1c/1c1c\x07",
        "\x1b]11;rgb:ffff/0000/8080\x1b\\",
        "\x1b]11;#1a2b3c\x07",
        "\x1b]11;#1a1a2b2b3c3c\x07",
        "\x1b]11;RGB:ff/00/ff\x07",
    ] {
        let shared = parse_osc_color_response(reply).expect("shared parser");
        assert_eq!(shared.target, OscColorTarget::Background);
        assert_eq!(
            pir::tui::theme::parse_osc11_background(reply),
            shared.rgb,
            "{reply}"
        );
    }
    assert_eq!(
        pir::tui::theme::parse_osc11_background("\x1b]10;rgb:ff/ff/ff\x07"),
        None
    );
    assert_eq!(
        pir::tui::theme::parse_osc11_background("\x1b]11;rgb:ff/00/00"),
        None
    );
}
