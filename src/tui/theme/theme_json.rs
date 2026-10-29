//! Custom themes: `themes/*.json` next to the agent dir, validated against
//! the upstream theme schema simplified to the palette keys this module
//! consumes (upstream `theme-json.ts` + `theme-controller.ts`).

use std::collections::BTreeMap;

use crate::tui::ansi;

use super::color::{
    hex_of_rgb, hex_to_rgb, index_to_rgb, okhsl_to_rgb, oklch_to_rgb, rgb_to_oklab,
};
use super::palette::{bg_ansi, detect_color_mode, fg_ansi, Appearance};
use super::Theme;

/// One color value in a theme file: `#rrggbb`/`#rgb` hex, an `oklch()` or
/// `okhsl()` CSS color string, a `vars` reference, `""` (reset), or a
/// 256-color index (upstream `ColorValueSchema`).
#[derive(Debug, Clone, PartialEq)]
pub enum ColorValue {
    Hex(String),
    Oklch {
        /// Lightness, 0-1.
        l: f64,
        /// Chroma.
        c: f64,
        /// Hue in degrees (normalized to 0-360).
        h: f64,
    },
    Okhsl {
        /// Hue in degrees (normalized to 0-360).
        h: f64,
        /// Saturation, 0-1 (relative to the sRGB gamut).
        s: f64,
        /// Lightness, 0-1.
        l: f64,
    },
    Var(String),
    Index(u8),
    Empty,
}

/// Colors for HTML export (upstream theme schema `export` object:
/// `pageBg`/`cardBg`/`infoBg`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ExportColors {
    pub page_bg: Option<ColorValue>,
    pub card_bg: Option<ColorValue>,
    pub info_bg: Option<ColorValue>,
}

/// A parsed and validated theme file (upstream `ValidatedThemeJson`).
#[derive(Debug, Clone, PartialEq)]
pub struct ThemeJson {
    pub name: String,
    /// Declared appearance; `None` when omitted (detected from the theme's
    /// own colors at build time, upstream `detectAppearance`).
    pub appearance: Option<Appearance>,
    pub vars: BTreeMap<String, ColorValue>,
    pub colors: BTreeMap<String, ColorValue>,
    pub export: Option<ExportColors>,
}

/// Color keys that may be omitted (upstream `Type.Optional` entries).
const OPTIONAL_COLOR_KEYS: [&str; 5] = [
    "scrollbarTrack",
    "scrollbarThumb",
    "searchMatchBg",
    "searchMatchText",
    "thinkingMax",
];

/// Every color key the port consumes (`ThemeColor::key` outputs).
pub(super) fn all_color_keys() -> Vec<&'static str> {
    super::all_theme_colors()
        .into_iter()
        .map(Theme::key)
        .collect()
}

/// Parse one CSS color-function token: a finite number with an optional
/// `%`/`deg` suffix (upstream `NUMBER_PATTERN` + optional units).
fn parse_css_number_token(token: &str) -> Option<(f64, u8)> {
    let (number, unit): (&str, u8) = if let Some(rest) = token.strip_suffix('%') {
        (rest, b'%')
    } else if token.len() >= 3
        && token.is_char_boundary(token.len() - 3)
        && token[token.len() - 3..].eq_ignore_ascii_case("deg")
    {
        (&token[..token.len() - 3], b'd')
    } else {
        (token, b' ')
    };
    let valid = !number.is_empty()
        && number
            .chars()
            .all(|c| c.is_ascii_digit() || matches!(c, '.' | 'e' | 'E' | '+' | '-'))
        && number.chars().any(|c| c.is_ascii_digit());
    if !valid {
        return None;
    }
    let value: f64 = number.parse().ok()?;
    if !value.is_finite() {
        return None;
    }
    Some((value, unit))
}

/// ASCII-case-insensitive `text.starts_with(prefix)` that never slices
/// through a multi-byte character boundary.
fn starts_with_ignore_case(text: &str, prefix: &str) -> bool {
    text.len() >= prefix.len()
        && text.is_char_boundary(prefix.len())
        && text[..prefix.len()].eq_ignore_ascii_case(prefix)
}

/// Split `name( inner )` into `inner` (case-insensitive `name`, no nested
/// parentheses).
fn css_function_inner<'a>(text: &'a str, name: &str) -> Option<&'a str> {
    let inner = text.strip_suffix(')')?;
    if inner.is_char_boundary(name.len()) && inner[..name.len()].eq_ignore_ascii_case(name) {
        Some(&inner[name.len()..])
    } else {
        None
    }
}

/// Parse `oklch( L[%] C H[deg] )` (upstream `OKLCH_PATTERN` + `oklchColor`'s
/// range checks).
fn parse_oklch_value(text: &str) -> Option<(f64, f64, f64)> {
    let inner = css_function_inner(text, "oklch(")?;
    let tokens: Vec<&str> = inner.split_whitespace().collect();
    if tokens.len() != 3 {
        return None;
    }
    let (l, l_unit) = parse_css_number_token(tokens[0])?;
    let (c, c_unit) = parse_css_number_token(tokens[1])?;
    let (h, h_unit) = parse_css_number_token(tokens[2])?;
    if l_unit == b'd' || c_unit != b' ' || h_unit == b'%' {
        return None;
    }
    let l = if l_unit == b'%' { l / 100.0 } else { l };
    if !(0.0..=1.0).contains(&l) || c < 0.0 {
        return None;
    }
    Some((l, c, h.rem_euclid(360.0)))
}

/// Parse `okhsl( H[deg] S[%] L[%] )` (upstream `OKHSL_PATTERN` + `okhslColor`'s
/// range checks).
fn parse_okhsl_value(text: &str) -> Option<(f64, f64, f64)> {
    let inner = css_function_inner(text, "okhsl(")?;
    let tokens: Vec<&str> = inner.split_whitespace().collect();
    if tokens.len() != 3 {
        return None;
    }
    let (h, h_unit) = parse_css_number_token(tokens[0])?;
    let (s, s_unit) = parse_css_number_token(tokens[1])?;
    let (l, l_unit) = parse_css_number_token(tokens[2])?;
    if h_unit == b'%' || s_unit == b'd' || l_unit == b'd' {
        return None;
    }
    let s = if s_unit == b'%' { s / 100.0 } else { s };
    let l = if l_unit == b'%' { l / 100.0 } else { l };
    if !(0.0..=1.0).contains(&s) || !(0.0..=1.0).contains(&l) {
        return None;
    }
    Some((h.rem_euclid(360.0), s, l))
}

/// Parse one color value from its JSON form (upstream `ColorValueSchema`'s
/// string/integer union): hex, `oklch()`/`okhsl()`, var reference, empty
/// string, or 0-255 index.
pub fn parse_color_value(json: &serde_json::Value) -> Result<ColorValue, String> {
    match json {
        serde_json::Value::Number(number) => {
            let index = number
                .as_u64()
                .ok_or_else(|| format!("color index {number} must be an integer in 0..=255"))?;
            if index > 255 {
                return Err(format!("color index {index} must be in 0..=255"));
            }
            Ok(ColorValue::Index(index as u8))
        }
        serde_json::Value::String(text) => {
            if text.is_empty() {
                return Ok(ColorValue::Empty);
            }
            if let Some(hex) = text.strip_prefix('#') {
                let valid = (hex.len() == 3 || hex.len() == 6)
                    && hex.chars().all(|c| c.is_ascii_hexdigit());
                if !valid {
                    return Err(format!("invalid hex color \"{text}\""));
                }
                return Ok(ColorValue::Hex(text.to_string()));
            }
            if starts_with_ignore_case(text, "oklch(") {
                let (l, c, h) = parse_oklch_value(text).ok_or_else(|| {
                    format!(
                        "invalid oklch color \"{text}\" (expected \"oklch( L[%] C H[deg] )\" with L in 0..=1 and C >= 0)"
                    )
                })?;
                return Ok(ColorValue::Oklch { l, c, h });
            }
            if starts_with_ignore_case(text, "okhsl(") {
                let (h, s, l) = parse_okhsl_value(text).ok_or_else(|| {
                    format!(
                        "invalid okhsl color \"{text}\" (expected \"okhsl( H[deg] S[%] L[%] )\" with S and L in 0..=1)"
                    )
                })?;
                return Ok(ColorValue::Okhsl { h, s, l });
            }
            Ok(ColorValue::Var(text.clone()))
        }
        other => Err(format!(
            "color value must be a hex string, oklch()/okhsl() color, var name, or 0-255 index, got {other}"
        )),
    }
}

/// Top-level fields allowed in a theme document (upstream schema
/// `additionalProperties: false`).
const ALLOWED_TOP_LEVEL_FIELDS: [&str; 6] =
    ["$schema", "name", "appearance", "vars", "colors", "export"];

/// Parse the optional `appearance` field (upstream schema `appearance`:
/// `"dark" | "light"`).
fn parse_appearance(json: Option<&serde_json::Value>) -> Result<Option<Appearance>, String> {
    match json {
        None | Some(serde_json::Value::Null) => Ok(None),
        Some(serde_json::Value::String(text)) => match text.as_str() {
            "dark" => Ok(Some(Appearance::Dark)),
            "light" => Ok(Some(Appearance::Light)),
            other => Err(format!(
                "\"appearance\" must be \"dark\" or \"light\", got \"{other}\""
            )),
        },
        Some(other) => Err(format!(
            "\"appearance\" must be \"dark\" or \"light\", got {other}"
        )),
    }
}

/// Parse the optional `export` block (upstream schema: optional `pageBg`,
/// `cardBg`, `infoBg`; `additionalProperties: false`).
fn parse_export_block(json: Option<&serde_json::Value>) -> Result<Option<ExportColors>, String> {
    let Some(value) = json else {
        return Ok(None);
    };
    if value.is_null() {
        return Ok(None);
    }
    let map = value
        .as_object()
        .ok_or_else(|| "\"export\" must be an object".to_string())?;
    let mut export = ExportColors::default();
    for (key, value) in map {
        let parsed = parse_color_value(value).map_err(|e| format!("export.{key}: {e}"))?;
        match key.as_str() {
            "pageBg" => export.page_bg = Some(parsed),
            "cardBg" => export.card_bg = Some(parsed),
            "infoBg" => export.info_bg = Some(parsed),
            other => {
                return Err(format!(
                    "\"export\" has unknown field \"{other}\" (allowed: pageBg, cardBg, infoBg)"
                ))
            }
        }
    }
    Ok(Some(export))
}

/// Parse and validate one theme document (upstream `validateThemeJson`).
/// Mirrors the upstream schema reduced to the keys this port consumes:
/// `name` (required, no `/`), optional `$schema`/`appearance`/`export`,
/// optional `vars`, required `colors` (all [`all_color_keys`] entries except
/// [`OPTIONAL_COLOR_KEYS`]); unknown fields are rejected.
pub fn parse_theme_json(text: &str) -> Result<ThemeJson, String> {
    let json: serde_json::Value =
        serde_json::from_str(text).map_err(|error| format!("invalid JSON: {error}"))?;
    let object = json
        .as_object()
        .ok_or_else(|| "theme must be a JSON object".to_string())?;

    let unknown: Vec<&str> = object
        .keys()
        .filter(|key| !ALLOWED_TOP_LEVEL_FIELDS.contains(&key.as_str()))
        .map(String::as_str)
        .collect();
    if !unknown.is_empty() {
        return Err(format!(
            "unknown field(s): {}. Allowed fields: {}",
            unknown.join(", "),
            ALLOWED_TOP_LEVEL_FIELDS.join(", ")
        ));
    }

    let name = object
        .get("name")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| "missing required field \"name\"".to_string())?;
    if name.is_empty() {
        return Err("theme name must not be empty".to_string());
    }
    if name.contains('/') {
        return Err(format!(
            "invalid theme name \"{name}\": theme names cannot contain \"/\""
        ));
    }
    let appearance =
        parse_appearance(object.get("appearance")).map_err(|e| format!("appearance: {e}"))?;

    let vars = match object.get("vars") {
        None | Some(serde_json::Value::Null) => BTreeMap::new(),
        Some(value) => {
            let map = value
                .as_object()
                .ok_or_else(|| "\"vars\" must be an object".to_string())?;
            let mut vars = BTreeMap::new();
            for (key, value) in map {
                vars.insert(
                    key.clone(),
                    parse_color_value(value).map_err(|e| format!("vars.{key}: {e}"))?,
                );
            }
            vars
        }
    };

    let colors_json = object
        .get("colors")
        .and_then(serde_json::Value::as_object)
        .ok_or_else(|| "missing required field \"colors\" (an object)".to_string())?;
    let mut colors = BTreeMap::new();
    for (key, value) in colors_json {
        if !all_color_keys().contains(&key.as_str()) {
            return Err(format!(
                "colors.{key}: unknown color token (see the built-in themes dark.json/light.json for the token list)"
            ));
        }
        colors.insert(
            key.clone(),
            parse_color_value(value).map_err(|e| format!("colors.{key}: {e}"))?,
        );
    }

    let missing: Vec<&str> = all_color_keys()
        .into_iter()
        .filter(|key| !colors.contains_key(*key) && !OPTIONAL_COLOR_KEYS.contains(key))
        .collect();
    if !missing.is_empty() {
        return Err(format!(
            "missing required color tokens: {}. See the built-in themes (dark.json, light.json) for reference values.",
            missing.join(", ")
        ));
    }

    let export = parse_export_block(object.get("export"))?;

    Ok(ThemeJson {
        name: name.to_string(),
        appearance,
        vars,
        colors,
        export,
    })
}

/// OKLab lightness of a concrete (fully resolved) color value; `None` for
/// values the terminal renders itself (empty strings and palette indices < 16,
/// upstream `averageLightness`).
fn concrete_lightness(value: &ColorValue, vars: &BTreeMap<String, ColorValue>) -> Option<f64> {
    let resolved = resolve_color(value, vars).ok()?;
    match resolved {
        ResolvedColor::Hex(hex) => {
            let (r, g, b) = hex_to_rgb(&hex)?;
            Some(rgb_to_oklab(r as u8, g as u8, b as u8)[0])
        }
        ResolvedColor::Index(index) if index >= 16 => {
            let (r, g, b) = index_to_rgb(index);
            Some(rgb_to_oklab(r, g, b)[0])
        }
        _ => None,
    }
}

/// Detect the background a theme is designed for from the lightness of its
/// own colors (upstream `detectAppearance`).
pub(super) fn detect_theme_appearance(json: &ThemeJson) -> Option<Appearance> {
    let mut fg_values: Vec<f64> = Vec::new();
    let mut bg_values: Vec<f64> = Vec::new();
    for (key, value) in &json.colors {
        let Some(lightness) = concrete_lightness(value, &json.vars) else {
            continue;
        };
        if key.ends_with("Bg") {
            bg_values.push(lightness);
        } else {
            fg_values.push(lightness);
        }
    }
    let average = |values: &[f64]| -> Option<f64> {
        if values.is_empty() {
            None
        } else {
            Some(values.iter().sum::<f64>() / values.len() as f64)
        }
    };
    let fg = average(&fg_values);
    let bg = average(&bg_values);
    if let (Some(fg), Some(bg)) = (fg, bg) {
        return Some(if bg < fg {
            Appearance::Dark
        } else {
            Appearance::Light
        });
    }
    if let Some(bg) = bg {
        return Some(if bg < 0.5 {
            Appearance::Dark
        } else {
            Appearance::Light
        });
    }
    if let Some(fg) = fg {
        return Some(if fg > 0.5 {
            Appearance::Dark
        } else {
            Appearance::Light
        });
    }
    None
}

/// Resolve a color value against `vars` into a concrete color. `oklch()`/
/// `okhsl()` values are converted to hex here, so everything downstream
/// (ANSI tables, export CSS) only sees hex or 256-color indices.
pub(super) fn resolve_color(
    value: &ColorValue,
    vars: &BTreeMap<String, ColorValue>,
) -> Result<ResolvedColor, String> {
    let mut current = value;
    // Guard against (invalid) var chains; single lookup is the upstream shape.
    for _ in 0..2 {
        match current {
            ColorValue::Empty => return Ok(ResolvedColor::None),
            ColorValue::Hex(hex) => return Ok(ResolvedColor::Hex(hex.clone())),
            ColorValue::Index(index) => return Ok(ResolvedColor::Index(*index)),
            ColorValue::Oklch { l, c, h } => {
                let (r, g, b) = oklch_to_rgb(*l, *c, *h);
                return Ok(ResolvedColor::Hex(hex_of_rgb(r, g, b)));
            }
            ColorValue::Okhsl { h, s, l } => {
                let (r, g, b) = okhsl_to_rgb(*h, *s, *l);
                return Ok(ResolvedColor::Hex(hex_of_rgb(r, g, b)));
            }
            ColorValue::Var(name) => {
                current = vars
                    .get(name)
                    .ok_or_else(|| format!("unknown var reference \"{name}\""))?;
            }
        }
    }
    Err("color var reference is too deep".to_string())
}

#[derive(Debug, Clone)]
pub(super) enum ResolvedColor {
    Hex(String),
    Index(u8),
    None,
}

pub(super) fn resolved_fg_ansi(color: &ResolvedColor) -> String {
    match color {
        ResolvedColor::Hex(hex) => fg_ansi(hex, detect_color_mode()),
        ResolvedColor::Index(index) => format!("\x1b[38;5;{index}m"),
        ResolvedColor::None => ansi::FG_RESET.to_string(),
    }
}

pub(super) fn resolved_bg_ansi(color: &ResolvedColor) -> String {
    match color {
        ResolvedColor::Hex(hex) => bg_ansi(hex, detect_color_mode()),
        ResolvedColor::Index(index) => format!("\x1b[48;5;{index}m"),
        ResolvedColor::None => ansi::BG_RESET.to_string(),
    }
}
