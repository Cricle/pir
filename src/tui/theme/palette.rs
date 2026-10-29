//! ANSI mode detection, the builtin palettes, and the style-table builder.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU8, Ordering};

use crate::tui::ansi;

use super::color::{hex_to_rgb, rgb_to_256};
use super::theme_json::{parse_color_value, resolve_color, ColorValue, ResolvedColor};

/// Upstream `ColorMode` selection: truecolor when the terminal advertises
/// it (COLORTERM=truecolor|24bit or known emulators), else 256.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColorMode {
    TrueColor,
    Color256,
}

/// A pinned color mode (upstream `setCapabilityOverrides({ trueColor })`).
/// `AUTO` follows the environment detection; the other two force a mode.
static COLOR_MODE_OVERRIDE: AtomicU8 = AtomicU8::new(COLOR_MODE_AUTO);
const COLOR_MODE_AUTO: u8 = 0;
const COLOR_MODE_TRUE: u8 = 1;
const COLOR_MODE_256: u8 = 2;

/// Pin the color mode (upstream `setCapabilityOverrides({ trueColor })`):
/// `Some(true)` forces truecolor, `Some(false)` forces the 256-color fallback,
/// `None` returns to the detected default.
///
/// Both the style-table build ([`super::build_style_table`]) and the inline
/// theme builder read the mode at render time, so an override lands before the
/// first frame exactly like upstream's cached capability.
pub fn set_color_mode_override(true_color: Option<bool>) {
    COLOR_MODE_OVERRIDE.store(
        match true_color {
            Some(true) => COLOR_MODE_TRUE,
            Some(false) => COLOR_MODE_256,
            None => COLOR_MODE_AUTO,
        },
        Ordering::SeqCst,
    );
}

pub(super) fn detect_color_mode() -> ColorMode {
    match COLOR_MODE_OVERRIDE.load(Ordering::SeqCst) {
        COLOR_MODE_TRUE => return ColorMode::TrueColor,
        COLOR_MODE_256 => return ColorMode::Color256,
        _ => {}
    }
    let colorterm = std::env::var("COLORTERM")
        .unwrap_or_default()
        .to_lowercase();
    if colorterm == "truecolor" || colorterm == "24bit" {
        return ColorMode::TrueColor;
    }
    let term_program = std::env::var("TERM_PROGRAM")
        .unwrap_or_default()
        .to_lowercase();
    if matches!(
        term_program.as_str(),
        "iterm.app" | "vscode" | "ghostty" | "wezterm" | "hyper" | "contour"
    ) {
        return ColorMode::TrueColor;
    }
    ColorMode::Color256
}

pub(super) fn fg_ansi(color: &str, mode: ColorMode) -> String {
    if color.is_empty() {
        return ansi::FG_RESET.to_string();
    }
    if let Some((r, g, b)) = hex_to_rgb(color) {
        return match mode {
            ColorMode::TrueColor => format!("\x1b[38;2;{r};{g};{b}m"),
            ColorMode::Color256 => format!("\x1b[38;5;{}m", rgb_to_256(r, g, b)),
        };
    }
    ansi::FG_RESET.to_string()
}

pub(super) fn bg_ansi(color: &str, mode: ColorMode) -> String {
    if color.is_empty() {
        return ansi::BG_RESET.to_string();
    }
    if let Some((r, g, b)) = hex_to_rgb(color) {
        return match mode {
            ColorMode::TrueColor => format!("\x1b[48;2;{r};{g};{b}m"),
            ColorMode::Color256 => format!("\x1b[48;5;{}m", rgb_to_256(r, g, b)),
        };
    }
    ansi::BG_RESET.to_string()
}

// ---------------------------------------------------------------------------
// The theme table: exactly the upstream keys the chat surface consumes.
// ---------------------------------------------------------------------------

/// A theme color name (subset of upstream `ThemeColor` the port uses).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThemeColor {
    Accent,
    Border,
    BorderAccent,
    BorderMuted,
    Success,
    Error,
    Warning,
    Muted,
    Dim,
    Text,
    ThinkingText,
    SelectedBg,
    ScrollbarTrack,
    ScrollbarThumb,
    SearchMatchBg,
    SearchMatchText,
    UserMessageBg,
    UserMessageText,
    CustomMessageBg,
    CustomMessageText,
    CustomMessageLabel,
    ToolPendingBg,
    ToolSuccessBg,
    ToolErrorBg,
    ToolTitle,
    ToolOutput,
    MdHeading,
    MdLink,
    MdLinkUrl,
    MdCode,
    MdCodeBlock,
    MdCodeBlockBorder,
    MdQuote,
    MdQuoteBorder,
    MdHr,
    MdListBullet,
    ToolDiffAdded,
    ToolDiffRemoved,
    ToolDiffContext,
    SyntaxComment,
    SyntaxKeyword,
    SyntaxFunction,
    SyntaxVariable,
    SyntaxString,
    SyntaxNumber,
    SyntaxType,
    SyntaxOperator,
    SyntaxPunctuation,
    ThinkingOff,
    ThinkingMinimal,
    ThinkingLow,
    ThinkingMedium,
    ThinkingHigh,
    ThinkingXhigh,
    ThinkingMax,
    BashMode,
}

/// The two builtin palettes, transcribed from upstream
/// `theme/dark.json` and `theme/light.json` (`packages/coding-agent/src/
/// modes/interactive/theme/`): the current OKHSL token themes, verbatim.
pub struct Palette {
    pub vars: &'static [(&'static str, &'static str)],
    pub colors: &'static [(&'static str, &'static str)],
    /// The optional `export` block (`pageBg`/`cardBg`/`infoBg`).
    pub export: &'static [(&'static str, &'static str)],
}

pub const DARK_PALETTE: Palette = Palette {
    vars: &[
        ("text", "okhsl(234 3% 89%)"),
        ("muted", "okhsl(229 6% 67%)"),
        ("violet", "okhsl(295 50% 67%)"),
        ("blue", "okhsl(232 54% 67%)"),
        ("green", "okhsl(159 59% 67%)"),
        ("red", "okhsl(20 72% 67%)"),
        ("yellow", "okhsl(83 88% 67%)"),
        ("blueBg", "okhsl(233 41% 24%)"),
    ],
    colors: &[
        ("accent", "violet"),
        ("border", "okhsl(231 57% 65%)"),
        ("borderAccent", "okhsl(295 53% 64%)"),
        ("borderMuted", "okhsl(229 8% 53%)"),
        ("success", "green"),
        ("error", "red"),
        ("warning", "yellow"),
        ("muted", "muted"),
        ("dim", "okhsl(229 8% 56%)"),
        ("text", "text"),
        ("thinkingText", "okhsl(226 7% 65%)"),
        ("selectedBg", "blueBg"),
        ("scrollbarTrack", "okhsl(237 7% 33%)"),
        ("scrollbarThumb", "okhsl(232 7% 65%)"),
        ("searchMatchBg", "okhsl(53 51% 24%)"),
        ("searchMatchText", "muted"),
        ("userMessageBg", "blueBg"),
        ("userMessageText", "text"),
        ("customMessageBg", "okhsl(295 42% 24%)"),
        ("customMessageText", "muted"),
        ("customMessageLabel", "violet"),
        ("toolPendingBg", "okhsl(229 5% 24%)"),
        ("toolSuccessBg", "okhsl(158 46% 25%)"),
        ("toolErrorBg", "okhsl(19 54% 25%)"),
        ("toolTitle", "text"),
        ("toolOutput", "muted"),
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
        ("syntaxOperator", "muted"),
        ("syntaxPunctuation", "muted"),
        ("thinkingOff", "okhsl(229 8% 49%)"),
        ("thinkingMinimal", "okhsl(232 20% 52%)"),
        ("thinkingLow", "okhsl(232 45% 54%)"),
        ("thinkingMedium", "okhsl(263 59% 56%)"),
        ("thinkingHigh", "okhsl(295 73% 59%)"),
        ("thinkingXhigh", "okhsl(337 81% 61%)"),
        ("thinkingMax", "okhsl(20 99% 63%)"),
        ("bashMode", "okhsl(159 64% 65%)"),
    ],
    export: &[
        ("pageBg", "okhsl(262 14% 16%)"),
        ("cardBg", "okhsl(264 13% 19%)"),
        ("infoBg", "okhsl(53 51% 24%)"),
    ],
};

pub const LIGHT_PALETTE: Palette = Palette {
    vars: &[
        ("text", "okhsl(225 5% 27%)"),
        ("muted", "okhsl(229 8% 47%)"),
        ("violet", "okhsl(295 60% 46%)"),
        ("blue", "okhsl(231 68% 47%)"),
        ("green", "okhsl(159 75% 46%)"),
        ("red", "okhsl(20 91% 47%)"),
        ("yellow", "okhsl(83 99% 47%)"),
        ("blueBg", "okhsl(235 19% 91%)"),
    ],
    colors: &[
        ("accent", "violet"),
        ("border", "okhsl(231 67% 55%)"),
        ("borderAccent", "okhsl(295 59% 55%)"),
        ("borderMuted", "okhsl(235 7% 66%)"),
        ("success", "green"),
        ("error", "red"),
        ("warning", "yellow"),
        ("muted", "muted"),
        ("dim", "okhsl(229 7% 59%)"),
        ("text", "text"),
        ("thinkingText", "okhsl(234 8% 55%)"),
        ("selectedBg", "blueBg"),
        ("scrollbarTrack", "okhsl(248 3% 90%)"),
        ("scrollbarThumb", "okhsl(226 7% 65%)"),
        ("searchMatchBg", "okhsl(56 22% 91%)"),
        ("searchMatchText", "muted"),
        ("userMessageBg", "blueBg"),
        ("userMessageText", "text"),
        ("customMessageBg", "okhsl(295 25% 91%)"),
        ("customMessageText", "muted"),
        ("customMessageLabel", "violet"),
        ("toolPendingBg", "okhsl(248 3% 91%)"),
        ("toolSuccessBg", "okhsl(156 21% 91%)"),
        ("toolErrorBg", "okhsl(24 23% 91%)"),
        ("toolTitle", "text"),
        ("toolOutput", "muted"),
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
        ("syntaxVariable", "okhsl(203 73% 46%)"),
        ("syntaxString", "okhsl(52 84% 46%)"),
        ("syntaxNumber", "green"),
        ("syntaxType", "violet"),
        ("syntaxOperator", "muted"),
        ("syntaxPunctuation", "muted"),
        ("thinkingOff", "okhsl(223 5% 80%)"),
        ("thinkingMinimal", "okhsl(229 14% 78%)"),
        ("thinkingLow", "okhsl(232 33% 76%)"),
        ("thinkingMedium", "okhsl(264 48% 74%)"),
        ("thinkingHigh", "okhsl(295 62% 72%)"),
        ("thinkingXhigh", "okhsl(337 74% 70%)"),
        ("thinkingMax", "okhsl(20 98% 68%)"),
        ("bashMode", "okhsl(159 74% 55%)"),
    ],
    export: &[
        ("pageBg", "okhsl(17 3% 94%)"),
        ("cardBg", "okhsl(17 5% 97%)"),
        ("infoBg", "okhsl(56 22% 91%)"),
    ],
};

/// Parse a palette's `vars` into color values (all okhsl()/hex strings).
pub(super) fn palette_vars(palette: &Palette) -> BTreeMap<String, ColorValue> {
    palette
        .vars
        .iter()
        .filter_map(|(name, value)| {
            parse_color_value(&serde_json::Value::String((*value).to_string()))
                .ok()
                .map(|parsed| ((*name).to_string(), parsed))
        })
        .collect()
}

/// Resolve one palette value (var reference, okhsl()/oklch() string or hex)
/// into concrete hex ("" when unresolvable).
pub(super) fn resolve_palette_value(value: &str, vars: &BTreeMap<String, ColorValue>) -> String {
    match parse_color_value(&serde_json::Value::String(value.to_string()))
        .ok()
        .and_then(|parsed| resolve_color(&parsed, vars).ok())
    {
        Some(ResolvedColor::Hex(hex)) => hex,
        Some(ResolvedColor::Index(index)) => {
            let (r, g, b) = super::color::index_to_rgb(index);
            super::color::hex_of_rgb(r, g, b)
        }
        Some(ResolvedColor::None) | None => String::new(),
    }
}

/// One `build_palette` result: fg ANSI per token, bg ANSI per token, and the
/// resolved source hex per token.
type PaletteMaps = (
    BTreeMap<&'static str, String>,
    BTreeMap<&'static str, String>,
    BTreeMap<&'static str, String>,
);

/// Resolve a palette into concrete fg ANSI strings plus the source hex per
/// token (the bg table derives by translating each `38;` prefix to `48;`).
pub(super) fn build_palette(palette: &Palette, mode: ColorMode) -> PaletteMaps {
    let vars = palette_vars(palette);
    let mut fg = BTreeMap::new();
    let mut fg_hex = BTreeMap::new();
    for (name, value) in palette.colors {
        let hex = resolve_palette_value(value, &vars);
        fg.insert(*name, fg_ansi(&hex, mode));
        fg_hex.insert(*name, hex);
    }
    let mut bg = BTreeMap::new();
    for (name, value) in palette.colors {
        if name.ends_with("Bg") || *name == "userMessageBg" {
            let hex = resolve_palette_value(value, &vars);
            bg.insert(*name, bg_ansi(&hex, mode));
        }
    }
    (fg, bg, fg_hex)
}

/// The background a theme (or terminal) is designed for (upstream
/// `ThemeAppearance`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Appearance {
    Dark,
    Light,
}

impl Appearance {
    pub fn as_str(self) -> &'static str {
        match self {
            Appearance::Dark => "dark",
            Appearance::Light => "light",
        }
    }
}
