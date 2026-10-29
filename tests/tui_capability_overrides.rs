//! `terminal` settings must reach the two render-time capability gates: the
//! markdown OSC-8 switch and the theme's truecolor/256 color mode.
//!
//! Upstream applies `setCapabilityOverrides(settingsManager.getTerminalCapabilityOverrides())`
//! at TUI start (`interactive-mode.ts:610`, `main.ts:873`) and again on settings
//! reload (`:2061`); pir's `App::new` applies the same snapshot before the first
//! render, and has no reload hook yet (recorded as a residual).
//!
//! This is its own test BINARY on purpose: both gates are process-global, so an
//! override window here must never race a sibling suite's default-rendering
//! assertions. What it pins is the settings → gate bridge and the gates' render
//! effects; the `App::new` call site itself needs a real terminal, so that wiring
//! stays with the TUI acceptance battery.

use pir::config::settings::{AutoBool, TerminalSettings};
use pir::tui::core::Component;
use pir::tui::markdown::{set_global_hyperlinks, Markdown};
use pir::tui::terminal_image::overrides_from_terminal_settings;
use pir::tui::theme::{set_color_mode_override, set_theme, with_theme, ThemeColor};

fn render_one(text: &str) -> String {
    Markdown::new(text, 0, 0)
        .render(400)
        .into_iter()
        .next()
        .unwrap_or_default()
}

#[test]
fn terminal_settings_pin_the_osc8_gate_and_the_color_mode() {
    // ---- hyperlinks: the OSC-8 gate the markdown link renderer reads -------
    let off = TerminalSettings {
        hyperlinks: Some(AutoBool::Enabled(false)),
        ..Default::default()
    };
    let on = TerminalSettings {
        hyperlinks: Some(AutoBool::Enabled(true)),
        ..Default::default()
    };
    let auto = TerminalSettings {
        hyperlinks: Some(AutoBool::Auto),
        ..Default::default()
    };

    let overrides = overrides_from_terminal_settings(Some(&off));
    assert_eq!(overrides.hyperlinks, Some(false), "the settings bridge");
    set_global_hyperlinks(overrides.hyperlinks);
    let fallback = render_one("[docs](https://example.com/x)");
    assert!(
        pir::tui::ansi::strip_ansi(&fallback).contains("(https://example.com/x)")
            && !fallback.contains("\u{1b}]8;;"),
        "hyperlinks:false must render the `text (url)` fallback, got {fallback:?}"
    );

    let overrides = overrides_from_terminal_settings(Some(&on));
    assert_eq!(overrides.hyperlinks, Some(true));
    set_global_hyperlinks(overrides.hyperlinks);
    let linked = render_one("[docs](https://example.com/x)");
    assert!(
        linked.contains("\u{1b}]8;;https://example.com/x"),
        "hyperlinks:true must emit an OSC-8 run, got {linked:?}"
    );

    assert_eq!(
        overrides_from_terminal_settings(Some(&auto)).hyperlinks,
        None,
        "\"auto\" keeps the detected capability"
    );
    assert_eq!(
        overrides_from_terminal_settings(None).hyperlinks,
        None,
        "no `terminal` settings keeps the detected capability"
    );

    // ---- trueColor: the mode the style table is built in ------------------
    let mode_256 = TerminalSettings {
        true_color: Some(AutoBool::Enabled(false)),
        ..Default::default()
    };
    let mode_true = TerminalSettings {
        true_color: Some(AutoBool::Enabled(true)),
        ..Default::default()
    };
    assert_eq!(
        overrides_from_terminal_settings(Some(&mode_256)).true_color,
        Some(false)
    );

    set_color_mode_override(overrides_from_terminal_settings(Some(&mode_256)).true_color);
    set_theme("dark"); // the style table is built at theme-set time
    let quantized = with_theme(|theme| theme.fg(ThemeColor::Accent, "x"));
    assert!(
        quantized.starts_with("\u{1b}[38;5;"),
        "trueColor:false must quantize to the 256-color cube, got {quantized:?}"
    );

    set_color_mode_override(overrides_from_terminal_settings(Some(&mode_true)).true_color);
    set_theme("dark");
    let direct = with_theme(|theme| theme.fg(ThemeColor::Accent, "x"));
    assert!(
        direct.starts_with("\u{1b}[38;2;"),
        "trueColor:true must emit 24-bit color, got {direct:?}"
    );

    // `"auto"` must not pin anything (the detected default is environment
    // dependent, so only the tri-state is asserted).
    assert_eq!(
        overrides_from_terminal_settings(Some(&TerminalSettings::default())).true_color,
        None
    );

    // Restore the process defaults for anything else that runs in this binary.
    set_global_hyperlinks(None);
    set_color_mode_override(None);
    set_theme("dark");
}
