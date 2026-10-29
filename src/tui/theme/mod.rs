//! Theme system — a faithful port of upstream `theme.ts` with the built-in
//! `dark`/`light` palettes (exact `vars`/`colors` from the shipped JSON).
//!
//! Colors resolve `var-name → hex`, then to ANSI (truecolor when the
//! terminal supports it, else the upstream 256-color cube/grayscale
//! nearest-neighbor mapping). `fg` resets only the foreground (39), `bg`
//! only the background (49), matching the component-layer assumptions.

use std::collections::{BTreeMap, BTreeSet};

use super::ansi;

use self::palette::{build_palette, detect_color_mode};
use self::theme_json::{
    all_color_keys, detect_theme_appearance, resolve_color, resolved_bg_ansi, resolved_fg_ansi,
    ResolvedColor,
};

mod color;
mod osc11;
mod palette;
mod registry;
mod state;
mod system_theme;
mod terminal_colors;
mod theme_json;
mod watcher;

use color::hex_to_rgb;

pub use color::{
    mix_colors, okhsl_to_rgb, oklch_to_rgb, relative_luminance, rgb_to_okhsl, MixSpace,
};
#[allow(unused_imports)]
// pub(crate) API surface (observability hook), no in-crate consumer yet
pub(crate) use osc11::take_pending_stdin_bytes;
pub use osc11::{
    appearance_from_background, detect_colorfgbg_theme, detect_terminal_appearance,
    osc11_query_gate_claimed, parse_osc11_background, query_terminal_background,
    query_terminal_colors, read_reply_bytes, set_terminal_color_scheme_notifications,
    terminal_color_scheme_notifications_enabled, try_claim_query_gate, ReplyReadOutcome,
    MAX_HANDED_BACK_BYTES, OSC11_QUERY, TERMINAL_QUERY_TIMEOUT,
};
pub use palette::{
    set_color_mode_override, Appearance, ColorMode, Palette, ThemeColor, DARK_PALETTE,
    LIGHT_PALETTE,
};
pub use registry::{
    all_theme_entries, builtin_theme, custom_theme_names, is_known_theme, load_custom_theme_files,
    load_custom_themes, register_custom_theme, register_custom_theme_path, theme_snapshot,
    LoadedThemes, ThemeEntry, ThemeSnapshot,
};
pub use state::{
    apply_terminal_colors, current_theme_name, get_terminal_theme, mark_terminal_colors_pending,
    parse_auto_theme_setting, reapply_for_terminal, set_terminal_color_scheme, set_terminal_colors,
    set_theme, set_theme_from_settings, system_theme, terminal_color_scheme, terminal_colors,
    terminal_colors_pending, theme_appearance, theme_export_colors, with_theme,
    ExportColorsResolved,
};
pub use system_theme::{
    generate_system_theme_colors, wcag_contrast, SystemThemeColors, SystemThemeInput,
    SYSTEM_THEME_NAME,
};
pub use terminal_colors::{
    detect_terminal_theme, is_device_attributes_response, parse_osc_color_response,
    parse_osc_color_value, parse_terminal_color_scheme_report, terminal_appearance, OscColorReply,
    OscColorTarget, Rgb, TerminalColorDelivery, TerminalColorFeed, TerminalColorQueries,
    TerminalColorQuery, TerminalColors, TERMINAL_COLOR_QUERY, TERMINAL_COLOR_REPLY_COUNT,
    TERMINAL_COLOR_SCHEME_NOTIFY_OFF, TERMINAL_COLOR_SCHEME_NOTIFY_ON, TERMINAL_PALETTE_SIZE,
};
pub use theme_json::{parse_color_value, parse_theme_json, ColorValue, ExportColors, ThemeJson};
pub use watcher::{
    notify_theme_change, reload_theme_file, set_theme_change_listener, start_theme_watcher,
    theme_watch_timeout, ThemeWatcherHandle, ThemeWatcherPlan, THEME_RELOAD_DEBOUNCE,
};

/// The active theme: resolved ANSI tables + style helpers.
pub struct Theme {
    pub name: String,
    /// The background this theme is designed for: the declared `appearance`,
    /// else detected from the theme's own colors (upstream `Theme.appearance`).
    pub appearance: Appearance,
    fg: BTreeMap<&'static str, String>,
    bg: BTreeMap<&'static str, String>,
    /// Concrete hex per foreground token (the source of the `fg` ANSI
    /// strings), used by [`Theme::resolved_color_hex`].
    fg_hex: BTreeMap<&'static str, String>,
    /// Foreground tokens rendered faint (upstream `dimTokens`,
    /// theme.ts:197-198): `fg()` prepends SGR 2 and resolved colors mix
    /// toward the background. Built-in and custom themes ship none (upstream
    /// only passes `dim` for the generated system theme).
    dim_tokens: BTreeSet<&'static str>,
}

impl Theme {
    /// Build a `Theme` from validated theme JSON (upstream `createTheme`).
    pub fn from_theme_json(json: &ThemeJson) -> Result<Theme, String> {
        Self::from_json(json)
    }

    /// Build the generated `system` theme from the terminal's reported colors
    /// (upstream `createSystemTheme`, theme.ts:537-548): every token's color
    /// is a generated hex value, an ANSI palette index, or `""` for the
    /// terminal default; the appearance is the generated one (else the
    /// terminal's), and the `dim` tokens render faint (SGR 2).
    pub fn from_system_colors(generated: &SystemThemeColors) -> Theme {
        let json = ThemeJson {
            name: SYSTEM_THEME_NAME.to_string(),
            appearance: generated.appearance,
            vars: BTreeMap::new(),
            colors: generated
                .colors
                .iter()
                .map(|(key, value)| ((*key).to_string(), value.clone()))
                .collect(),
            export: None,
        };
        // The generated token set is exactly the theme token surface, so no
        // fallback resolution is needed; `from_json` maps `""` to the
        // terminal default (39/49) and indices to their ANSI escapes.
        let mut theme = Self::from_json(&json).unwrap_or_else(|_| Self::from_builtin("dark"));
        theme.name = SYSTEM_THEME_NAME.to_string();
        if generated.appearance.is_none() {
            // Upstream `Theme.appearance` falls back to the terminal's own
            // appearance when the theme has no usable colors (theme.ts:246-252).
            theme.appearance = state::get_terminal_theme();
        }
        theme.set_dim_tokens(generated.dim.iter().copied());
        theme
    }

    fn from_builtin(name: &str) -> Theme {
        let mode = detect_color_mode();
        let (fg, bg, fg_hex) = match name {
            "light" => build_palette(&LIGHT_PALETTE, mode),
            _ => build_palette(&DARK_PALETTE, mode),
        };
        Theme {
            name: name.to_string(),
            appearance: if name == "light" {
                Appearance::Light
            } else {
                Appearance::Dark
            },
            fg,
            bg,
            fg_hex,
            dim_tokens: BTreeSet::new(),
        }
    }

    /// The token key of a color (upstream `THEME_TOKENS` names).
    pub(crate) fn key(color: ThemeColor) -> &'static str {
        match color {
            ThemeColor::Accent => "accent",
            ThemeColor::Border => "border",
            ThemeColor::BorderAccent => "borderAccent",
            ThemeColor::BorderMuted => "borderMuted",
            ThemeColor::Success => "success",
            ThemeColor::Error => "error",
            ThemeColor::Warning => "warning",
            ThemeColor::Muted => "muted",
            ThemeColor::Dim => "dim",
            ThemeColor::Text => "text",
            ThemeColor::ThinkingText => "thinkingText",
            ThemeColor::SelectedBg => "selectedBg",
            ThemeColor::ScrollbarTrack => "scrollbarTrack",
            ThemeColor::ScrollbarThumb => "scrollbarThumb",
            ThemeColor::SearchMatchBg => "searchMatchBg",
            ThemeColor::SearchMatchText => "searchMatchText",
            ThemeColor::UserMessageBg => "userMessageBg",
            ThemeColor::UserMessageText => "userMessageText",
            ThemeColor::CustomMessageBg => "customMessageBg",
            ThemeColor::CustomMessageText => "customMessageText",
            ThemeColor::CustomMessageLabel => "customMessageLabel",
            ThemeColor::ToolPendingBg => "toolPendingBg",
            ThemeColor::ToolSuccessBg => "toolSuccessBg",
            ThemeColor::ToolErrorBg => "toolErrorBg",
            ThemeColor::ToolTitle => "toolTitle",
            ThemeColor::ToolOutput => "toolOutput",
            ThemeColor::MdHeading => "mdHeading",
            ThemeColor::MdLink => "mdLink",
            ThemeColor::MdLinkUrl => "mdLinkUrl",
            ThemeColor::MdCode => "mdCode",
            ThemeColor::MdCodeBlock => "mdCodeBlock",
            ThemeColor::MdCodeBlockBorder => "mdCodeBlockBorder",
            ThemeColor::MdQuote => "mdQuote",
            ThemeColor::MdQuoteBorder => "mdQuoteBorder",
            ThemeColor::MdHr => "mdHr",
            ThemeColor::MdListBullet => "mdListBullet",
            ThemeColor::ToolDiffAdded => "toolDiffAdded",
            ThemeColor::ToolDiffRemoved => "toolDiffRemoved",
            ThemeColor::ToolDiffContext => "toolDiffContext",
            ThemeColor::SyntaxComment => "syntaxComment",
            ThemeColor::SyntaxKeyword => "syntaxKeyword",
            ThemeColor::SyntaxFunction => "syntaxFunction",
            ThemeColor::SyntaxVariable => "syntaxVariable",
            ThemeColor::SyntaxString => "syntaxString",
            ThemeColor::SyntaxNumber => "syntaxNumber",
            ThemeColor::SyntaxType => "syntaxType",
            ThemeColor::SyntaxOperator => "syntaxOperator",
            ThemeColor::SyntaxPunctuation => "syntaxPunctuation",
            ThemeColor::ThinkingOff => "thinkingOff",
            ThemeColor::ThinkingMinimal => "thinkingMinimal",
            ThemeColor::ThinkingLow => "thinkingLow",
            ThemeColor::ThinkingMedium => "thinkingMedium",
            ThemeColor::ThinkingHigh => "thinkingHigh",
            ThemeColor::ThinkingXhigh => "thinkingXhigh",
            ThemeColor::ThinkingMax => "thinkingMax",
            ThemeColor::BashMode => "bashMode",
        }
    }

    /// Foreground style: `\x1b[38;…m{text}\x1b[39m`. Dim tokens (upstream
    /// `Theme.fg`, theme.ts:297-301) emit `\x1b[38;…m\x1b[2m{text}\x1b[22;39m`.
    pub fn fg(&self, color: ThemeColor, text: &str) -> String {
        let key = Self::key(color);
        match self.fg.get(key) {
            Some(ansi) => {
                if self.dim_tokens.contains(key) {
                    return format!("{ansi}\x1b[2m{text}\x1b[22;39m");
                }
                format!("{ansi}{text}{}", ansi::FG_RESET)
            }
            None => text.to_string(),
        }
    }

    /// Background style: `\x1b[48;…m{text}\x1b[49m`.
    ///
    /// One sized allocation: the painted row is a per-frame cost of every
    /// background box (`BoxContainer` repaints each of its rows every frame
    /// because its painter is an arbitrary closure), and the `format!` this
    /// replaces grew the buffer in three steps per painted row.
    pub fn bg(&self, color: ThemeColor, text: &str) -> String {
        let key = Self::key(color);
        let Some(ansi) = self.bg.get(key) else {
            return text.to_string();
        };
        let mut out = String::with_capacity(ansi.len() + text.len() + ansi::BG_RESET.len());
        out.push_str(ansi);
        out.push_str(text);
        out.push_str(ansi::BG_RESET);
        out
    }

    /// Bold: `\x1b[1m{text}\x1b[22m` — the precise close (upstream
    /// `colors.ts` `styleTextWithAnsi`/chalk) keeps any surrounding color or
    /// background alive, where a full `\x1b[0m` reset would drop it.
    pub fn bold(&self, text: &str) -> String {
        format!("\x1b[1m{text}\x1b[22m")
    }

    /// Italic: closes with SGR 23.
    pub fn italic(&self, text: &str) -> String {
        format!("\x1b[3m{text}\x1b[23m")
    }

    /// Underline: closes with SGR 24.
    pub fn underline(&self, text: &str) -> String {
        format!("\x1b[4m{text}\x1b[24m")
    }

    /// Strikethrough: closes with SGR 29.
    pub fn strikethrough(&self, text: &str) -> String {
        format!("\x1b[9m{text}\x1b[29m")
    }

    /// Editor border color for a thinking level (upstream
    /// `getThinkingBorderColor`).
    pub fn thinking_border(&self, level: &str) -> String {
        let color = match level {
            "minimal" => ThemeColor::ThinkingMinimal,
            "low" => ThemeColor::ThinkingLow,
            "medium" => ThemeColor::ThinkingMedium,
            "high" => ThemeColor::ThinkingHigh,
            "xhigh" => ThemeColor::ThinkingXhigh,
            "max" => ThemeColor::ThinkingMax,
            _ => ThemeColor::ThinkingOff,
        };
        self.fg.get(Self::key(color)).cloned().unwrap_or_default()
    }

    /// Bash-mode (`!` prefix) editor border color.
    pub fn bash_border(&self) -> String {
        self.fg
            .get(Self::key(ThemeColor::BashMode))
            .cloned()
            .unwrap_or_default()
    }

    /// Opening escape sequence for a foreground token; dim tokens include
    /// SGR 2, which `\x1b[22m` closes (upstream `getFgAnsi`, theme.ts:335-338).
    pub fn get_fg_ansi(&self, color: ThemeColor) -> String {
        let key = Self::key(color);
        let mut ansi = self.fg.get(key).cloned().unwrap_or_default();
        if self.dim_tokens.contains(key) {
            ansi.push_str("\x1b[2m");
        }
        ansi
    }

    /// Mark foreground tokens as dim (upstream the `dim` Theme constructor
    /// option, theme.ts:210-213). The system theme upstream derives its set
    /// from the terminal's reported colors; pir's built-in and custom themes
    /// keep none.
    pub fn set_dim_tokens(&mut self, tokens: impl IntoIterator<Item = ThemeColor>) {
        self.dim_tokens = tokens.into_iter().map(Self::key).collect();
    }

    /// The concrete color for a token as hex (upstream `Theme.colors`,
    /// theme.ts:258-276): dim tokens are approximated by mixing their color
    /// 40% toward the background in OKLCH (the background guess by theme
    /// appearance, upstream `GUESSED_DEFAULT_COLORS`, theme.ts:159-162).
    pub fn resolved_color_hex(&self, color: ThemeColor) -> Option<String> {
        let key = Self::key(color);
        let hex = self.fg_hex.get(key)?;
        if !self.dim_tokens.contains(key) {
            return Some(hex.clone());
        }
        let (r, g, b) = hex_to_rgb(hex)?;
        let background = match self.appearance {
            Appearance::Light => (255u8, 255, 255),
            Appearance::Dark => (0, 0, 0),
        };
        let (r, g, b) = color::mix_colors(
            (r as u8, g as u8, b as u8),
            background,
            0.4,
            MixSpace::Oklch,
        );
        Some(color::hex_of_rgb(r, g, b))
    }
}

impl Theme {
    /// Build a `Theme` from validated custom theme JSON. The appearance is
    /// the declared one, else detected from the theme's colors (upstream
    /// `options.appearance ?? detectAppearance(...)`), else dark.
    fn from_json(json: &ThemeJson) -> Result<Theme, String> {
        let mut fg = BTreeMap::new();
        let mut bg = BTreeMap::new();
        let mut fg_hex = BTreeMap::new();
        for color in all_color_keys() {
            let Some(value) = json.colors.get(color) else {
                continue;
            };
            let resolved = resolve_color(value, &json.vars)?;
            fg.insert(color, resolved_fg_ansi(&resolved));
            if let ResolvedColor::Hex(hex) = &resolved {
                fg_hex.insert(color, hex.clone());
            }
            if color.ends_with("Bg") || color == "userMessageBg" {
                bg.insert(color, resolved_bg_ansi(&resolved));
            }
        }
        Ok(Theme {
            name: json.name.clone(),
            appearance: json
                .appearance
                .or_else(|| detect_theme_appearance(json))
                .unwrap_or(Appearance::Dark),
            fg,
            bg,
            fg_hex,
            dim_tokens: BTreeSet::new(),
        })
    }
}

/// Every [`ThemeColor`] variant, in declaration order: the token surface the
/// custom-theme JSON validator and the generated system theme both cover
/// (upstream `THEME_TOKENS` keys).
pub(crate) fn all_theme_colors() -> Vec<ThemeColor> {
    vec![
        ThemeColor::Accent,
        ThemeColor::Border,
        ThemeColor::BorderAccent,
        ThemeColor::BorderMuted,
        ThemeColor::Success,
        ThemeColor::Error,
        ThemeColor::Warning,
        ThemeColor::Muted,
        ThemeColor::Dim,
        ThemeColor::Text,
        ThemeColor::ThinkingText,
        ThemeColor::SelectedBg,
        ThemeColor::ScrollbarTrack,
        ThemeColor::ScrollbarThumb,
        ThemeColor::SearchMatchBg,
        ThemeColor::SearchMatchText,
        ThemeColor::UserMessageBg,
        ThemeColor::UserMessageText,
        ThemeColor::CustomMessageBg,
        ThemeColor::CustomMessageText,
        ThemeColor::CustomMessageLabel,
        ThemeColor::ToolPendingBg,
        ThemeColor::ToolSuccessBg,
        ThemeColor::ToolErrorBg,
        ThemeColor::ToolTitle,
        ThemeColor::ToolOutput,
        ThemeColor::MdHeading,
        ThemeColor::MdLink,
        ThemeColor::MdLinkUrl,
        ThemeColor::MdCode,
        ThemeColor::MdCodeBlock,
        ThemeColor::MdCodeBlockBorder,
        ThemeColor::MdQuote,
        ThemeColor::MdQuoteBorder,
        ThemeColor::MdHr,
        ThemeColor::MdListBullet,
        ThemeColor::ToolDiffAdded,
        ThemeColor::ToolDiffRemoved,
        ThemeColor::ToolDiffContext,
        ThemeColor::SyntaxComment,
        ThemeColor::SyntaxKeyword,
        ThemeColor::SyntaxFunction,
        ThemeColor::SyntaxVariable,
        ThemeColor::SyntaxString,
        ThemeColor::SyntaxNumber,
        ThemeColor::SyntaxType,
        ThemeColor::SyntaxOperator,
        ThemeColor::SyntaxPunctuation,
        ThemeColor::ThinkingOff,
        ThemeColor::ThinkingMinimal,
        ThemeColor::ThinkingLow,
        ThemeColor::ThinkingMedium,
        ThemeColor::ThinkingHigh,
        ThemeColor::ThinkingXhigh,
        ThemeColor::ThinkingMax,
        ThemeColor::BashMode,
    ]
}
