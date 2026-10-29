//! G9 theme-system tests: OKLCH/OKHSL color values, the `appearance` field,
//! the `export` block, unknown-field validation, and OSC 11 background-reply
//! parsing (upstream `oklab.ts`, `theme-schema.ts`, `system-theme.ts`).

use pir::tui::theme::{
    appearance_from_background, okhsl_to_rgb, oklch_to_rgb, parse_auto_theme_setting,
    parse_color_value, parse_osc11_background, parse_theme_json, register_custom_theme,
    relative_luminance, set_theme, set_theme_from_settings, theme_appearance, theme_export_colors,
    with_theme, Appearance, ColorValue, DARK_PALETTE, LIGHT_PALETTE,
};

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

/// A valid theme JSON body from a palette, with per-case overrides applied.
fn theme_json_from_palette(
    palette: &pir::tui::theme::Palette,
    name: &str,
    overrides: &[(&str, serde_json::Value)],
) -> String {
    let mut vars = serde_json::Map::new();
    for (var_name, hex) in palette.vars {
        vars.insert((*var_name).to_string(), serde_json::json!(hex));
    }
    let mut colors = serde_json::Map::new();
    for (color_name, value) in palette.colors {
        colors.insert((*color_name).to_string(), serde_json::json!(value));
    }
    let mut body = serde_json::json!({ "name": name, "vars": vars, "colors": colors });
    for (pointer, value) in overrides {
        // "a/b" sets body["a"]["b"]; a bare key sets body[key].
        let mut parts = pointer.split('/');
        let first = parts.next().unwrap_or(pointer);
        match parts.next() {
            None => body[first] = value.clone(),
            Some(second) => body[first][second] = value.clone(),
        }
    }
    body.to_string()
}

// ---------------------------------------------------------------------------
// OKLCH / OKHSL conversion (numeric precision, Björn Ottosson reference
// values: sRGB red → oklab L=0.6279554 C=0.2576833 h=29.2338).
// ---------------------------------------------------------------------------

#[test]
fn oklch_anchor_colors_convert_exactly() {
    assert_eq!(oklch_to_rgb(0.0, 0.0, 0.0), (0, 0, 0));
    assert_eq!(oklch_to_rgb(1.0, 0.0, 0.0), (255, 255, 255));
    let (r, g, b) = oklch_to_rgb(0.6279554, 0.2576833, 29.2338);
    assert!((i32::from(r) - 255).abs() <= 2, "red channel {r}");
    assert!(g <= 2, "green channel {g}");
    assert!(b <= 2, "blue channel {b}");
    // Hue is periodic: 360 degrees wraps to 0.
    assert_eq!(oklch_to_rgb(0.5, 0.1, 360.0), oklch_to_rgb(0.5, 0.1, 0.0));
}

#[test]
fn okhsl_anchor_colors_convert_exactly() {
    assert_eq!(okhsl_to_rgb(0.0, 0.0, 0.0), (0, 0, 0));
    assert_eq!(okhsl_to_rgb(0.0, 0.0, 1.0), (255, 255, 255));
    // Zero saturation is a neutral gray for every hue; okhsl lightness 0.5
    // maps to oklab L ≈ 0.5757 → sRGB ≈ 121 (reference: Ottosson's
    // colorpicker post).
    for hue in [0.0, 90.0, 210.0, 359.9] {
        let (r, g, b) = okhsl_to_rgb(hue, 0.0, 0.5);
        for channel in [r, g, b] {
            assert!(
                (i32::from(channel) - 121).abs() <= 2,
                "hue {hue}: {channel}"
            );
        }
    }
    // sRGB red is fully saturated at its own okhsl lightness (l ≈ 0.568).
    let (r, g, b) = okhsl_to_rgb(29.2338, 1.0, 0.5680);
    assert!((i32::from(r) - 255).abs() <= 2, "red channel {r}");
    assert!(g <= 2, "green channel {g}");
    assert!(b <= 2, "blue channel {b}");
}

#[test]
fn okhsl_never_leaves_the_srgb_gamut() {
    // Saturation is defined relative to the gamut, so any channel combination
    // must stay valid; out-of-gamut math clips instead of panicking.
    // `okhsl_to_rgb` returns u8, so a "gamut leave" can only show up as a
    // broken value: all-black output (a clamp-everything bug), a collapsed
    // neutral, or a hue that no longer drives its channel. Pin those.
    assert_eq!(
        okhsl_to_rgb(123.4, 1.0, 0.0),
        (0, 0, 0),
        "lightness 0 is black for every hue/saturation"
    );
    for step in 0..=10 {
        let s = f64::from(step) / 10.0;
        let l = f64::from(step) / 10.0 + 0.05;
        let (r, g, b) = okhsl_to_rgb(123.4, s, l.min(0.99));
        // Always parseable values: conversion clamps, never overflows.
        let max = r.max(g).max(b);
        assert!(
            max > 0,
            "s={s} l={l}: conversion must not collapse to black, got ({r}, {g}, {b})"
        );
        if s == 0.0 {
            assert_eq!(
                (r, g, b),
                (r, r, r),
                "zero saturation is neutral at any lightness"
            );
        }
        if s > 0.0 && l >= 0.95 {
            assert!(
                max >= 250,
                "s={s} l={l}: near-white OKHSL must stay near-white, got ({r}, {g}, {b})"
            );
        }
    }
    // At full saturation and near-white lightness the hue still drives its
    // primary channel (0 = red, 120 = green, 240 = blue); a hue-rotation or
    // black-clamp regression breaks this.
    let (r, g, b) = okhsl_to_rgb(0.0, 1.0, 0.99);
    assert!(
        r >= 250 && r >= g && r >= b,
        "hue 0 must stay red: {r},{g},{b}"
    );
    let (r, g, b) = okhsl_to_rgb(120.0, 1.0, 0.99);
    assert!(
        g >= 250 && g >= r && g >= b,
        "hue 120 must stay green: {r},{g},{b}"
    );
    let (r, g, b) = okhsl_to_rgb(240.0, 1.0, 0.99);
    assert!(
        b >= 250 && b >= r && b >= g,
        "hue 240 must stay blue: {r},{g},{b}"
    );
}

// ---------------------------------------------------------------------------
// Color value parsing (upstream OKLCH_PATTERN / OKHSL_PATTERN shapes)
// ---------------------------------------------------------------------------

#[test]
fn color_value_parses_oklch_and_okhsl_strings() {
    let oklch = parse_color_value(&serde_json::json!("oklch(62.79554% 0.2576833 29.2338)"))
        .expect("oklch percent form");
    assert_eq!(
        oklch,
        ColorValue::Oklch {
            l: 62.79554f64 / 100.0,
            c: 0.2576833,
            h: 29.2338
        }
    );
    let oklch_deg = parse_color_value(&serde_json::json!("oklch(0.5 0.1 30deg)")).expect("deg");
    assert_eq!(
        oklch_deg,
        ColorValue::Oklch {
            l: 0.5,
            c: 0.1,
            h: 30.0
        }
    );
    let okhsl = parse_color_value(&serde_json::json!("okhsl(29.2338deg 100% 56.805%)"))
        .expect("okhsl percent form");
    assert_eq!(
        okhsl,
        ColorValue::Okhsl {
            h: 29.2338,
            s: 1.0,
            l: 56.805f64 / 100.0
        }
    );
}

#[test]
fn color_value_rejects_malformed_function_colors() {
    // Lightness out of range.
    assert!(parse_color_value(&serde_json::json!("oklch(1.5 0.1 30)")).is_err());
    // Negative chroma.
    assert!(parse_color_value(&serde_json::json!("oklch(0.5 -0.1 30)")).is_err());
    // Comma separators are not CSS-space syntax.
    assert!(parse_color_value(&serde_json::json!("oklch(0.5, 0.1, 30)")).is_err());
    // Saturation out of range.
    assert!(parse_color_value(&serde_json::json!("okhsl(30 1.2 0.5)")).is_err());
    // Lightness out of range.
    assert!(parse_color_value(&serde_json::json!("okhsl(30 0.5 150%)")).is_err());
    // Wrong arity.
    assert!(parse_color_value(&serde_json::json!("oklch(0.5 0.1)")).is_err());
    // Non-numeric garbage.
    assert!(parse_color_value(&serde_json::json!("oklch(a b c)")).is_err());
    // Original shapes still parse.
    assert_eq!(
        parse_color_value(&serde_json::json!("accent")).expect("var"),
        ColorValue::Var("accent".to_string())
    );
    assert_eq!(
        parse_color_value(&serde_json::json!(42)).expect("index"),
        ColorValue::Index(42)
    );
    assert_eq!(
        parse_color_value(&serde_json::json!("")).expect("empty"),
        ColorValue::Empty
    );
}

#[test]
fn theme_json_oklch_colors_resolve_to_hex() {
    // The process-global theme: serialize with the sibling suites' swaps.
    let _guard = super::tui_system_theme::lock();
    // accent = an oklch color; border = var ref to an okhsl var. The built-in
    // dark palette stays untouched so every other required token resolves.
    let text = theme_json_from_palette(
        &DARK_PALETTE,
        "g9-oklch",
        &[
            (
                "colors/accent",
                serde_json::json!("oklch(62.79554% 0.2576833 29.2338)"),
            ),
            ("vars/brand", serde_json::json!("okhsl(0 0 0.5)")),
            ("colors/border", serde_json::json!("brand")),
        ],
    );
    let theme = parse_theme_json(&text).expect("valid theme");
    register_custom_theme(theme);
    set_theme("g9-oklch");
    let accent = with_theme(|theme| theme.fg(pir::tui::theme::ThemeColor::Accent, "x"));
    assert!(
        accent.starts_with("\x1b[38;2;") || accent.starts_with("\x1b[38;5;"),
        "oklch accent must resolve to a real SGR color: {accent:?}"
    );
    let border = with_theme(|theme| theme.fg(pir::tui::theme::ThemeColor::Border, "x"));
    assert_ne!(
        border, accent,
        "okhsl var must not collapse onto the oklch token"
    );
    // The 256-color-indexed conversion path stays available for export
    // resolution (index 196 = pure red in the xterm cube).
    assert_eq!(
        theme_export_colors(Some("g9-oklch")),
        pir::tui::theme::ExportColorsResolved::default(),
        "no export block → default (empty) export colors"
    );
    set_theme("dark");
}

// ---------------------------------------------------------------------------
// appearance field (declared, else detected from the theme's own colors)
// ---------------------------------------------------------------------------

#[test]
fn theme_appearance_field_is_parsed() {
    let _guard = super::tui_system_theme::lock();
    let text = theme_json_from_palette(
        &DARK_PALETTE,
        "g9-appearance",
        &[("appearance", serde_json::json!("light"))],
    );
    let theme = parse_theme_json(&text).expect("valid theme");
    assert_eq!(theme.appearance, Some(Appearance::Light));
    register_custom_theme(theme);
    set_theme("g9-appearance");
    assert_eq!(theme_appearance(), Appearance::Light);
    set_theme("dark");
    assert_eq!(theme_appearance(), Appearance::Dark);

    let bad = theme_json_from_palette(
        &DARK_PALETTE,
        "g9-appearance",
        &[("appearance", serde_json::json!("blue"))],
    );
    let error = parse_theme_json(&bad).expect_err("invalid appearance");
    assert!(error.contains("appearance"), "{error}");
}

#[test]
fn theme_appearance_is_detected_from_color_lightness_when_omitted() {
    let _guard = super::tui_system_theme::lock();
    // Light palette with no appearance: every background is bright → light.
    let light = theme_json_from_palette(&LIGHT_PALETTE, "g9-light-detected", &[]);
    let theme = parse_theme_json(&light).expect("valid theme");
    assert_eq!(theme.appearance, None, "field stays as authored");
    register_custom_theme(theme);
    set_theme("g9-light-detected");
    assert_eq!(theme_appearance(), Appearance::Light);

    // Dark palette with no appearance → detected dark.
    let dark = theme_json_from_palette(&DARK_PALETTE, "g9-dark-detected", &[]);
    let dark_theme = parse_theme_json(&dark).expect("valid");
    assert_eq!(dark_theme.appearance, None);
    register_custom_theme(dark_theme);
    set_theme("g9-dark-detected");
    assert_eq!(theme_appearance(), Appearance::Dark);
    set_theme("dark");
}

// ---------------------------------------------------------------------------
// export block (upstream theme schema `export`: pageBg/cardBg/infoBg)
// ---------------------------------------------------------------------------

#[test]
fn theme_export_block_resolves_to_css_hex() {
    let text = theme_json_from_palette(
        &DARK_PALETTE,
        "g9-export",
        &[
            ("export/pageBg", serde_json::json!("#112233")),
            ("export/cardBg", serde_json::json!("yellow")),
            ("export/infoBg", serde_json::json!(196)),
        ],
    );
    parse_theme_json(&text)
        .map(register_custom_theme)
        .expect("valid theme");
    let export = theme_export_colors(Some("g9-export"));
    assert_eq!(export.page_bg.as_deref(), Some("#112233"));
    // Var reference resolved against the palette's vars: `yellow` is now the
    // upstream okhsl value and resolves to concrete hex.
    let yellow = DARK_PALETTE
        .vars
        .iter()
        .find(|(name, _)| *name == "yellow")
        .map(|(_, value)| *value)
        .unwrap_or("");
    assert_eq!(yellow, "okhsl(83 88% 67%)");
    let (r, g, b) = okhsl_to_rgb(83.0, 0.88, 0.67);
    assert_eq!(
        export.card_bg.as_deref(),
        Some(format!("#{r:02x}{g:02x}{b:02x}").as_str())
    );
    // 256-color index 196 → the xterm cube's pure red.
    assert_eq!(export.info_bg.as_deref(), Some("#ff0000"));

    // Built-ins carry the upstream export blocks (dark.json/light.json),
    // with okhsl values resolved to hex.
    let dark_export = theme_export_colors(Some("dark"));
    let (r, g, b) = okhsl_to_rgb(262.0, 0.14, 0.16);
    assert_eq!(
        dark_export.page_bg.as_deref(),
        Some(format!("#{r:02x}{g:02x}{b:02x}").as_str())
    );
    let (r, g, b) = okhsl_to_rgb(53.0, 0.51, 0.24);
    assert_eq!(
        dark_export.info_bg.as_deref(),
        Some(format!("#{r:02x}{g:02x}{b:02x}").as_str())
    );
    // Unknown names resolve to nothing.
    assert_eq!(
        theme_export_colors(Some("does-not-exist")),
        pir::tui::theme::ExportColorsResolved::default()
    );
}

#[test]
fn theme_export_block_rejects_unknown_fields_and_bad_values() {
    let text = theme_json_from_palette(
        &DARK_PALETTE,
        "g9-export-bad",
        &[("export/sidebar", serde_json::json!("#112233"))],
    );
    let error = parse_theme_json(&text).expect_err("unknown export field");
    assert!(error.contains("sidebar"), "{error}");
    assert!(error.contains("pageBg"), "{error}");

    let text = theme_json_from_palette(
        &DARK_PALETTE,
        "g9-export-bad2",
        &[("export/pageBg", serde_json::json!("#12g45z"))],
    );
    assert!(parse_theme_json(&text).is_err());
}

// ---------------------------------------------------------------------------
// Unknown-field validation (upstream schema additionalProperties: false)
// ---------------------------------------------------------------------------

#[test]
fn theme_json_rejects_unknown_fields() {
    let text = theme_json_from_palette(
        &DARK_PALETTE,
        "g9-unknown",
        &[("version", serde_json::json!(2))],
    );
    let error = parse_theme_json(&text).expect_err("unknown top-level field");
    assert!(error.contains("version"), "{error}");
    assert!(error.contains("unknown field"), "{error}");

    let text = theme_json_from_palette(
        &DARK_PALETTE,
        "g9-unknown2",
        &[("colors/notAToken", serde_json::json!("#112233"))],
    );
    let error = parse_theme_json(&text).expect_err("unknown color token");
    assert!(error.contains("notAToken"), "{error}");

    // $schema is an allowed top-level field (upstream Type.Optional).
    let text = theme_json_from_palette(
        &DARK_PALETTE,
        "g9-schema",
        &[(
            "$schema",
            serde_json::json!("https://example.com/theme.schema.json"),
        )],
    );
    parse_theme_json(&text).expect("$schema must be accepted");
}

// ---------------------------------------------------------------------------
// OSC 11 background replies (upstream parseOscColorResponse restricted to
// target 11) + appearance classification
// ---------------------------------------------------------------------------

#[test]
fn osc11_replies_parse_in_all_reported_shapes() {
    // BEL-terminated, 16-bit rgb: channels.
    assert_eq!(
        parse_osc11_background("\x1b]11;rgb:1c1c/1c1c/1c1c\x07"),
        Some((0x1c, 0x1c, 0x1c))
    );
    // ST-terminated reply.
    assert_eq!(
        parse_osc11_background("\x1b]11;rgb:ffff/0000/8080\x1b\\"),
        Some((255, 0, 128))
    );
    // 8-bit channels scale by their own length.
    assert_eq!(
        parse_osc11_background("\x1b]11;rgb:cc/22/11\x07"),
        Some((0xcc, 0x22, 0x11))
    );
    // Hash-prefixed payloads.
    assert_eq!(
        parse_osc11_background("\x1b]11;#1a2b3c\x07"),
        Some((26, 43, 60))
    );
    assert_eq!(
        parse_osc11_background("\x1b]11;#1a1a2b2b3c3c\x07"),
        Some((26, 43, 60))
    );
    // Case-insensitive color space prefix.
    assert_eq!(
        parse_osc11_background("\x1b]11;RGB:ff/00/ff\x07"),
        Some((255, 0, 255))
    );
    // Wrong target (OSC 10 foreground) is not an OSC 11 reply.
    assert_eq!(parse_osc11_background("\x1b]10;rgb:ff/ff/ff\x07"), None);
    // Palette reply (OSC 4) is not either.
    assert_eq!(parse_osc11_background("\x1b]4;1;rgb:ff/00/00\x07"), None);
    // Missing terminator / garbage.
    assert_eq!(parse_osc11_background("\x1b]11;rgb:ff/00/00"), None);
    assert_eq!(parse_osc11_background("nope"), None);
    assert_eq!(parse_osc11_background(""), None);
}

#[test]
fn appearance_from_background_classifies_by_wcag_contrast() {
    assert_eq!(appearance_from_background((0, 0, 0)), Appearance::Dark);
    assert_eq!(
        appearance_from_background((255, 255, 255)),
        Appearance::Light
    );
    // Mid-gray: black text has more contrast than white → light terminal.
    assert_eq!(
        appearance_from_background((128, 128, 128)),
        Appearance::Light
    );
    // A typical dark editor background.
    assert_eq!(
        appearance_from_background((0x1e, 0x1e, 0x24)),
        Appearance::Dark
    );
}

#[test]
fn relative_luminance_matches_wcag_reference_values() {
    assert!((relative_luminance(0, 0, 0)).abs() < f64::EPSILON);
    assert!((relative_luminance(255, 255, 255) - 1.0).abs() < 1e-9);
    // WCAG's own example: sRGB #ff0000 → 0.2126.
    assert!((relative_luminance(255, 0, 0) - 0.2126).abs() < 1e-6);
}

// ---------------------------------------------------------------------------
// Settings resolution: auto / light-dark pairs
// ---------------------------------------------------------------------------

#[test]
fn parse_auto_theme_setting_pairs() {
    assert_eq!(
        parse_auto_theme_setting("light/dark"),
        Some(("light".to_string(), "dark".to_string()))
    );
    assert_eq!(
        parse_auto_theme_setting(" my-light / my-dark "),
        Some(("my-light".to_string(), "my-dark".to_string()))
    );
    // Exactly one slash is required.
    assert_eq!(parse_auto_theme_setting("a/b/c"), None);
    assert_eq!(parse_auto_theme_setting("/dark"), None);
    assert_eq!(parse_auto_theme_setting("light/"), None);
    assert_eq!(parse_auto_theme_setting("auto"), None);
    assert_eq!(parse_auto_theme_setting("dark"), None);
}

#[test]
fn unset_theme_setting_defaults_to_the_system_theme() {
    let _guard = super::tui_system_theme::lock();
    // Upstream `theme.ts:682-683`: the default theme is `system`.
    set_theme("dark");
    set_theme_from_settings(None);
    with_theme(|theme| assert_eq!(theme.name, "system"));
    set_theme("dark");
}

#[test]
fn settings_theme_selector_resolves_without_terminal_reports() {
    let _guard = super::tui_system_theme::lock();
    // In tests (piped stdout, no COLORFGBG) detection falls back to dark;
    // the pair's dark half and "auto" must agree with that fallback.
    let expected = theme_appearance();
    set_theme_from_settings(Some("light/dark"));
    let pair_name = with_theme(|theme| theme.name.clone());
    set_theme_from_settings(Some("auto"));
    let auto_name = with_theme(|theme| theme.name.clone());
    if expected == Appearance::Dark {
        assert_eq!(pair_name, "dark");
        assert_eq!(auto_name, "dark");
    }
    // Non-auto names are untouched.
    set_theme_from_settings(Some("light"));
    with_theme(|theme| assert_eq!(theme.name, "light"));
    set_theme("dark");
}
