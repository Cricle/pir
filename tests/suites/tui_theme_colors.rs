//! Theme color-parity tests for the 2026-10-10 feature-alignment fixes:
//! - the built-in dark/light palettes match upstream
//!   `packages/coding-agent/src/modes/interactive/theme/{dark,light}.json`
//!   (the current OKHSL token themes), including var resolution and the
//!   `export` block;
//! - OKLCH → RGB gamut mapping preserves hue via chroma bisection (upstream
//!   `colors.ts:190-216`), not channel clamping;
//! - `rgbToOkhsl` (oklab.ts:215-233) round-trips through `okhslToRgb`;
//! - `mixColors` (colors.ts:241-257) endpoints, spaces and shortest hue path;
//! - the dim-token mechanism (theme.ts:198, 269-272, 297-301, 335-338).

use pir::tui::markdown::MarkdownTheme;
use pir::tui::theme::{
    mix_colors, okhsl_to_rgb, oklch_to_rgb, parse_theme_json, relative_luminance, rgb_to_okhsl,
    set_theme, theme_appearance, theme_export_colors, with_theme, Appearance, ColorValue, MixSpace,
    Theme, ThemeColor, DARK_PALETTE, LIGHT_PALETTE,
};

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

/// A valid theme JSON body from a palette (okhsl values resolve through the
/// same parser as custom themes).
fn theme_json_from(palette: &pir::tui::theme::Palette, name: &str) -> String {
    let mut vars = serde_json::Map::new();
    for (var_name, value) in palette.vars {
        vars.insert((*var_name).to_string(), serde_json::json!(value));
    }
    let mut colors = serde_json::Map::new();
    for (color_name, value) in palette.colors {
        colors.insert((*color_name).to_string(), serde_json::json!(value));
    }
    serde_json::json!({ "name": name, "vars": vars, "colors": colors }).to_string()
}

fn hex_of(r: u8, g: u8, b: u8) -> String {
    format!("#{r:02x}{g:02x}{b:02x}")
}

fn okhsl_hex(h: f64, s: f64, l: f64) -> String {
    let (r, g, b) = okhsl_to_rgb(h, s, l);
    hex_of(r, g, b)
}

fn circular_hue_distance(a: f64, b: f64) -> f64 {
    let diff = (a - b).rem_euclid(360.0);
    diff.min(360.0 - diff)
}

// ---------------------------------------------------------------------------
// Palette regression vs the upstream theme JSONs
// ---------------------------------------------------------------------------

#[test]
fn dark_palette_matches_upstream_dark_json() {
    let expected_vars: &[(&str, &str)] = &[
        ("text", "okhsl(234 3% 89%)"),
        ("muted", "okhsl(229 6% 67%)"),
        ("violet", "okhsl(295 50% 67%)"),
        ("blue", "okhsl(232 54% 67%)"),
        ("green", "okhsl(159 59% 67%)"),
        ("red", "okhsl(20 72% 67%)"),
        ("yellow", "okhsl(83 88% 67%)"),
        ("blueBg", "okhsl(233 41% 24%)"),
    ];
    for (name, value) in expected_vars {
        assert_eq!(
            DARK_PALETTE
                .vars
                .iter()
                .find(|(n, _)| n == name)
                .map(|(_, v)| *v),
            Some(*value),
            "dark var {name}"
        );
    }
    let expected_colors: &[(&str, &str)] = &[
        ("accent", "violet"),
        ("border", "okhsl(231 57% 65%)"),
        ("borderAccent", "okhsl(295 53% 64%)"),
        ("borderMuted", "okhsl(229 8% 53%)"),
        ("success", "green"),
        ("error", "red"),
        ("warning", "yellow"),
        ("dim", "okhsl(229 8% 56%)"),
        ("thinkingText", "okhsl(226 7% 65%)"),
        ("selectedBg", "blueBg"),
        ("userMessageBg", "blueBg"),
        ("customMessageLabel", "violet"),
        ("toolPendingBg", "okhsl(229 5% 24%)"),
        ("toolSuccessBg", "okhsl(158 46% 25%)"),
        ("toolErrorBg", "okhsl(19 54% 25%)"),
        ("mdHeading", "yellow"),
        ("mdLink", "blue"),
        ("mdLinkUrl", "muted"),
        ("mdCode", "violet"),
        ("mdCodeBlock", "green"),
        ("mdCodeBlockBorder", "muted"),
        ("mdQuote", "muted"),
        ("mdQuoteBorder", "muted"),
        ("mdHr", "muted"),
        ("mdListBullet", "violet"),
        ("toolDiffAdded", "green"),
        ("toolDiffRemoved", "red"),
        ("toolDiffContext", "muted"),
        ("syntaxComment", "muted"),
        ("syntaxKeyword", "blue"),
        ("syntaxFunction", "yellow"),
        ("syntaxVariable", "okhsl(202 58% 67%)"),
        ("syntaxString", "okhsl(52 67% 67%)"),
        ("syntaxNumber", "green"),
        ("syntaxType", "violet"),
        ("thinkingOff", "okhsl(229 8% 49%)"),
        ("thinkingMinimal", "okhsl(232 20% 52%)"),
        ("thinkingLow", "okhsl(232 45% 54%)"),
        ("thinkingMedium", "okhsl(263 59% 56%)"),
        ("thinkingHigh", "okhsl(295 73% 59%)"),
        ("thinkingXhigh", "okhsl(337 81% 61%)"),
        ("thinkingMax", "okhsl(20 99% 63%)"),
        ("bashMode", "okhsl(159 64% 65%)"),
    ];
    for (name, value) in expected_colors {
        assert_eq!(
            DARK_PALETTE
                .colors
                .iter()
                .find(|(n, _)| n == name)
                .map(|(_, v)| *v),
            Some(*value),
            "dark color {name}"
        );
    }
}

#[test]
fn light_palette_matches_upstream_light_json() {
    let expected_vars: &[(&str, &str)] = &[
        ("text", "okhsl(225 5% 27%)"),
        ("muted", "okhsl(229 8% 47%)"),
        ("violet", "okhsl(295 60% 46%)"),
        ("blue", "okhsl(231 68% 47%)"),
        ("green", "okhsl(159 75% 46%)"),
        ("red", "okhsl(20 91% 47%)"),
        ("yellow", "okhsl(83 99% 47%)"),
        ("blueBg", "okhsl(235 19% 91%)"),
    ];
    for (name, value) in expected_vars {
        assert_eq!(
            LIGHT_PALETTE
                .vars
                .iter()
                .find(|(n, _)| n == name)
                .map(|(_, v)| *v),
            Some(*value),
            "light var {name}"
        );
    }
    let expected_colors: &[(&str, &str)] = &[
        ("accent", "violet"),
        ("dim", "okhsl(229 7% 59%)"),
        ("mdHeading", "yellow"),
        ("mdLink", "blue"),
        ("mdCode", "violet"),
        ("mdCodeBlock", "green"),
        ("mdListBullet", "violet"),
        ("syntaxVariable", "okhsl(203 73% 46%)"),
        ("syntaxString", "okhsl(52 84% 46%)"),
        ("thinkingMax", "okhsl(20 98% 68%)"),
        ("bashMode", "okhsl(159 74% 55%)"),
    ];
    for (name, value) in expected_colors {
        assert_eq!(
            LIGHT_PALETTE
                .colors
                .iter()
                .find(|(n, _)| n == name)
                .map(|(_, v)| *v),
            Some(*value),
            "light color {name}"
        );
    }
}

#[test]
fn builtin_palettes_parse_as_custom_theme_json() {
    // The okhsl transcription must survive the theme-JSON validation the
    // custom-theme path uses (same var set, same token colors).
    for (name, palette) in [("dark", &DARK_PALETTE), ("light", &LIGHT_PALETTE)] {
        let parsed = parse_theme_json(&theme_json_from(palette, name))
            .unwrap_or_else(|error| panic!("{name} palette must validate: {error}"));
        match parsed.vars.get("violet") {
            Some(ColorValue::Okhsl { h, s, l }) => {
                assert_eq!(*h, 295.0);
                assert!((0.0..=1.0).contains(s) && (0.0..=1.0).contains(l));
            }
            other => panic!("violet must parse as okhsl, got {other:?}"),
        }
        assert_eq!(
            parsed.colors.get("mdHeading"),
            Some(&ColorValue::Var("yellow".to_string()))
        );
    }
}

// ---------------------------------------------------------------------------
// Rendering + appearance + export
// ---------------------------------------------------------------------------

#[test]
fn builtin_dark_theme_resolves_tokens_from_the_okhsl_vars() {
    // The process-global theme: serialize with the sibling suites' swaps.
    let _guard = super::tui_system_theme::lock();
    set_theme("dark");
    assert_eq!(theme_appearance(), Appearance::Dark);
    // mdHeading = yellow = okhsl(83 88% 67%).
    assert_eq!(
        with_theme(|t| t.resolved_color_hex(ThemeColor::MdHeading)),
        Some(okhsl_hex(83.0, 0.88, 0.67))
    );
    // mdCode = violet = okhsl(295 50% 67%).
    assert_eq!(
        with_theme(|t| t.resolved_color_hex(ThemeColor::MdCode)),
        Some(okhsl_hex(295.0, 0.50, 0.67))
    );
    // dim = okhsl(229 8% 56%) renders via fg without SGR 2 (built-ins have
    // no dim tokens).
    let dim = with_theme(|t| t.fg(ThemeColor::Dim, "x"));
    assert!(!dim.contains("\x1b[2m"), "{dim:?}");
    set_theme("dark");
}

#[test]
fn builtin_light_theme_appearance_and_export() {
    let _guard = super::tui_system_theme::lock();
    set_theme("light");
    assert_eq!(theme_appearance(), Appearance::Light);
    set_theme("dark");

    // The built-ins carry the upstream `export` blocks (dark.json).
    let export = theme_export_colors(Some("dark"));
    assert_eq!(
        export.page_bg.as_deref(),
        Some(okhsl_hex(262.0, 0.14, 0.16).as_str())
    );
    assert_eq!(
        export.card_bg.as_deref(),
        Some(okhsl_hex(264.0, 0.13, 0.19).as_str())
    );
    assert_eq!(
        export.info_bg.as_deref(),
        Some(okhsl_hex(53.0, 0.51, 0.24).as_str())
    );
    let export = theme_export_colors(Some("light"));
    assert_eq!(
        export.page_bg.as_deref(),
        Some(okhsl_hex(17.0, 0.03, 0.94).as_str())
    );
}

#[test]
fn markdown_theme_md_colors_come_from_the_new_palette() {
    let _guard = super::tui_system_theme::lock();
    set_theme("dark");
    // The markdown theme hooks still resolve (smoke: heading styled with the
    // theme's mdHeading escape, not the legacy #f0c674 value).
    let t = MarkdownTheme::default();
    let heading_ansi = (t.heading)("x");
    let expected = with_theme(|th| th.fg(ThemeColor::MdHeading, "x"));
    assert_eq!(heading_ansi, expected);
    assert!(
        !heading_ansi.contains("240;198"),
        "legacy #f0c674 leaked: {heading_ansi:?}"
    );
    set_theme("dark");
}

// ---------------------------------------------------------------------------
// OKLCH gamut mapping (colors.ts:190-216)
// ---------------------------------------------------------------------------

#[test]
fn oklch_out_of_gamut_falls_back_to_achromatic() {
    // Upstream example: `oklch(100% 0.3 150)` must map to white, not clip.
    assert_eq!(oklch_to_rgb(1.0, 0.3, 150.0), (255, 255, 255));
}

#[test]
fn oklch_gamut_mapping_preserves_hue() {
    // Far out of gamut: the bisection keeps the hue direction instead of
    // clipping channels (which shifts the hue).
    let (r, g, b) = oklch_to_rgb(0.7, 0.37, 30.0);
    let (hue, _, _) = rgb_to_okhsl(r, g, b);
    assert!(
        circular_hue_distance(hue, 30.0) < 10.0,
        "hue {hue} drifted from 30 for rgb({r},{g},{b})"
    );
    // Achromatic chroma stays exact gray.
    let (r, g, b) = oklch_to_rgb(0.5, 0.0, 217.0);
    assert_eq!(r, g);
    assert_eq!(g, b);
}

// ---------------------------------------------------------------------------
// rgbToOkhsl (oklab.ts:215-233)
// ---------------------------------------------------------------------------

#[test]
fn rgb_to_okhsl_round_trips_through_okhsl_to_rgb() {
    for (r, g, b) in [
        (255u8, 0u8, 0u8),
        (0, 100, 200),
        (30, 200, 90),
        (255, 200, 0),
        (90, 60, 180),
        (12, 12, 200),
    ] {
        let (h, s, l) = rgb_to_okhsl(r, g, b);
        let (r2, g2, b2) = okhsl_to_rgb(h, s, l);
        let drift = [
            (r2 as i32 - r as i32).abs(),
            (g2 as i32 - g as i32).abs(),
            (b2 as i32 - b as i32).abs(),
        ]
        .into_iter()
        .max()
        .unwrap_or(i32::MAX);
        assert!(
            drift <= 3,
            "rgb({r},{g},{b}) round-tripped to ({r2},{g2},{b2})"
        );
    }
}

#[test]
fn rgb_to_okhsl_gray_edge_cases() {
    assert_eq!(rgb_to_okhsl(0, 0, 0), (0.0, 0.0, 0.0));
    // Floating point: upstream JS returns 0.9999999999999996 for white too.
    let (h, s, l) = rgb_to_okhsl(255, 255, 255);
    assert_eq!((h, s), (0.0, 0.0));
    assert!(l > 0.999, "white lightness {l}");
    let (h, s, l) = rgb_to_okhsl(128, 128, 128);
    assert_eq!((h, s), (0.0, 0.0));
    assert!(l > 0.0 && l < 1.0, "mid-gray lightness {l}");
}

// ---------------------------------------------------------------------------
// mixColors (colors.ts:241-257)
// ---------------------------------------------------------------------------

#[test]
fn mix_colors_srgb_endpoints_and_midpoint() {
    let red = (255u8, 0u8, 0u8);
    let blue = (0u8, 0u8, 255u8);
    assert_eq!(mix_colors(red, blue, 0.0, MixSpace::Srgb), red);
    assert_eq!(mix_colors(red, blue, 1.0, MixSpace::Srgb), blue);
    assert_eq!(mix_colors(red, blue, 0.5, MixSpace::Srgb), (128, 0, 128));
}

#[test]
fn mix_colors_oklch_endpoints_and_gray_mix() {
    let white = (255u8, 255u8, 255u8);
    let black = (0u8, 0u8, 0u8);
    assert_eq!(mix_colors(black, white, 0.0, MixSpace::Oklch), black);
    assert_eq!(mix_colors(black, white, 1.0, MixSpace::Oklch), white);
    // Mid gray: the channels agree.
    let mid = mix_colors(black, white, 0.5, MixSpace::Oklch);
    assert_eq!(mid.0, mid.1);
    assert_eq!(mid.1, mid.2);
}

#[test]
fn mix_colors_oklch_achromatic_adopts_the_other_hue() {
    // A gray endpoint has chroma ~0, so the mix keeps the red hue.
    let red_hue = rgb_to_okhsl(255, 0, 0).0;
    let mixed = mix_colors((128, 128, 128), (255, 0, 0), 0.5, MixSpace::Oklch);
    let (hue, s, _) = rgb_to_okhsl(mixed.0, mixed.1, mixed.2);
    assert!(
        circular_hue_distance(hue, red_hue) < 10.0,
        "hue {hue} vs red {red_hue}"
    );
    assert!(s > 0.05, "mixed saturation {s}");
}

#[test]
fn mix_colors_oklch_takes_the_shortest_hue_path() {
    // 30° → 240°: the shortest path goes down through 315° (Δ = -150°), so
    // the midpoint hue is 315°, not 105° (upstream hueDelta, colors.ts:255).
    let a = okhsl_to_rgb(30.0, 0.5, 0.6);
    let b = okhsl_to_rgb(240.0, 0.5, 0.6);
    let mixed = mix_colors(a, b, 0.5, MixSpace::Oklch);
    let (hue, _, _) = rgb_to_okhsl(mixed.0, mixed.1, mixed.2);
    assert!(
        circular_hue_distance(hue, 315.0) < 25.0,
        "hue {hue} is not on the 30°→240° shortest path near 315°"
    );
}

// ---------------------------------------------------------------------------
// Dim tokens (theme.ts:198, 269-272, 297-301, 335-338)
// ---------------------------------------------------------------------------

#[test]
fn dim_tokens_add_sgr_2_and_mix_toward_the_background() {
    let theme_json = parse_theme_json(&theme_json_from(&DARK_PALETTE, "dim-mechanism"))
        .unwrap_or_else(|error| panic!("palette theme must validate: {error}"));
    let mut theme = Theme::from_theme_json(&theme_json)
        .unwrap_or_else(|error| panic!("palette theme must build: {error}"));

    // Without dim tokens: plain fg, plain hex.
    let dim_hex = theme
        .resolved_color_hex(ThemeColor::Dim)
        .unwrap_or_default();
    assert_eq!(dim_hex, okhsl_hex(229.0, 0.08, 0.56));
    let plain = theme.fg(ThemeColor::Dim, "x");
    assert!(!plain.contains("\x1b[2m"), "{plain:?}");

    // With dim tokens (upstream `Theme.fg`, theme.ts:299):
    // `${ansi}\x1b[2m${text}\x1b[22;39m`.
    theme.set_dim_tokens([ThemeColor::Dim, ThemeColor::Muted]);
    let dim_ansi = theme.get_fg_ansi(ThemeColor::Dim);
    let base = dim_ansi
        .strip_suffix("\x1b[2m")
        .expect("getFgAnsi appends SGR 2 for dim tokens");
    assert_eq!(
        theme.fg(ThemeColor::Dim, "x"),
        format!("{base}\x1b[2mx\x1b[22;39m")
    );

    // Non-dim tokens are unaffected.
    let muted_ansi = theme.get_fg_ansi(ThemeColor::Text);
    assert!(!muted_ansi.ends_with("\x1b[2m"));
    assert_eq!(
        theme.fg(ThemeColor::Text, "y"),
        format!("{muted_ansi}y\x1b[39m")
    );

    // Resolved colors mix 40% toward the background guess in OKLCH
    // (theme.ts:269-272; dark appearance guesses #000000).
    let (r, g, b) = okhsl_to_rgb(229.0, 0.08, 0.56);
    let (mr, mg, mb) = mix_colors((r, g, b), (0, 0, 0), 0.4, MixSpace::Oklch);
    assert_eq!(
        theme.resolved_color_hex(ThemeColor::Dim),
        Some(hex_of(mr, mg, mb))
    );
}

#[test]
fn relative_luminance_still_gates_appearance() {
    // Sanity on the exported helper used by appearance detection.
    assert!(relative_luminance(0, 0, 0) < relative_luminance(255, 255, 255));
}
