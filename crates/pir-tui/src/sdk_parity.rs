//! Upstream SDK parity helpers that belong to the terminal front end.
//!
//! These are the parts of upstream `core/sdk.ts` that wrap interactive/theme
//! utilities. They live in `pir-tui` (not the engine) so `pir-core` stays free
//! of terminal code; the `pir-sdk` facade re-exports this module into
//! `pir::sdk` when the `tui` feature is enabled.

// ---------------------------------------------------------------------------
// Interactive mode (upstream `InteractiveMode`, `modes/interactive-mode.ts`)
// ---------------------------------------------------------------------------

/// Upstream `InteractiveModeOptions`: the CLI-level overrides the interactive
/// host applies to the session it builds.
///
/// Deviation: upstream's `InteractiveMode` is a class owning a UI instance.
/// pir's interactive host is the native crossterm event loop
/// [`crate::run`], re-exported here as [`run_interactive_mode`]; the
/// session/extension surfaces it drives are the same ones
/// [`pir_core::sdk::AgentSession`] exposes, so there is no second mode object.
pub use crate::TuiOverrides as InteractiveModeOptions;

/// Upstream `InteractiveMode.run()`: enter the native interactive TUI.
///
/// Re-export of [`crate::run`] under the upstream mode name. Blocks the
/// calling thread until the user exits.
pub use crate::run as run_interactive_mode;

// ---------------------------------------------------------------------------
// Theme and keybinding utilities (upstream `modes/interactive/theme/theme.ts`,
// `components/keybinding-hints.ts`, and the exported `Theme` type)
// ---------------------------------------------------------------------------

pub use crate::keybindings::{Keybindings, TuiAction};
pub use crate::markdown::MarkdownTheme;
pub use crate::messages::format_read_line_range;
pub use crate::theme::{Appearance, Theme, ThemeColor};

/// Upstream `KeybindingsManager`: the resolved settings/override key map.
///
/// Alias, not a second type: [`Keybindings`] already implements `create`-style
/// construction ([`Keybindings::new`], [`Keybindings::from_settings`]) and the
/// lookup/matching methods (`keys_for_id`, `matches_id`, `hint_key_text`).
pub type KeybindingsManager = Keybindings;

/// Upstream `ThemeToken`: the color/token identifier accepted by
/// [`Theme::fg`]/[`Theme::bg`].
pub type ThemeToken = ThemeColor;

/// Upstream `ThemeAppearance`.
pub type ThemeAppearance = Appearance;

/// Upstream `getMarkdownTheme`: the markdown style hooks wired to the active
/// theme. The returned [`MarkdownTheme`] resolves colors at call time through
/// `with_theme`, so it stays correct after a theme switch.
pub fn get_markdown_theme() -> MarkdownTheme {
    MarkdownTheme::default()
}

/// Upstream `getLanguageFromPath`: the syntax-highlighting language for a file
/// path (`None` for unknown extensions).
pub fn get_language_from_path(file_path: &str) -> Option<&'static str> {
    crate::syntax::language_from_path(file_path)
}

/// Upstream `highlightCode`: highlight one fenced code block into styled
/// lines. `language` is the fence's info string (`None`/unknown keeps the
/// plain code-block color).
pub fn highlight_code(code: &str, language: Option<&str>) -> Vec<String> {
    crate::syntax::highlight_code(code, language)
}

/// Upstream `initTheme`: switch the process theme by name.
///
/// Deviation: upstream takes a `Theme` object; pir's theme registry owns
/// named built-in/custom themes ([`crate::theme::register_custom_theme`]
/// registers an object), so the equivalent entry point is the name-based
/// [`crate::theme::set_theme`].
pub fn init_theme(name: &str) {
    crate::theme::set_theme(name);
}

/// Upstream `formatKeyText`: render a key spec for display (`/`-joined chords,
/// `+`-joined parts; `alt` becomes `option` on macOS).
pub fn format_key_text(key: &str) -> String {
    key.split('/')
        .map(|chord| {
            chord
                .split('+')
                .map(|part| {
                    if cfg!(target_os = "macos") && part.eq_ignore_ascii_case("alt") {
                        "option".to_owned()
                    } else {
                        part.to_owned()
                    }
                })
                .collect::<Vec<_>>()
                .join("+")
        })
        .collect::<Vec<_>>()
        .join("/")
}

/// Upstream `keyText`: the resolved display text for one keybinding id
/// (settings override when present, else the platform default).
pub fn key_text(keybindings: &KeybindingsManager, id: &str) -> String {
    format_key_text(&keybindings.hint_key_text(id))
}

/// Upstream `keyHint`: dim key text plus the muted description, resolved from
/// the active theme.
pub fn key_hint(keybindings: &KeybindingsManager, id: &str, description: &str) -> String {
    let key = key_text(keybindings, id);
    format!(
        "{}{}",
        crate::theme::with_theme(|theme| theme.fg(ThemeColor::Dim, &key)),
        crate::theme::with_theme(|theme| {
            theme.fg(ThemeColor::Muted, &format!(" {description}"))
        })
    )
}

/// Upstream `rawKeyHint`: dim raw key text plus the muted description.
///
/// The key string is formatted with [`format_key_text`]; no keybinding lookup
/// happens, matching upstream `rawKeyHint`.
pub fn raw_key_hint(key: &str, description: &str) -> String {
    let key = format_key_text(key);
    format!(
        "{}{}",
        crate::theme::with_theme(|theme| theme.fg(ThemeColor::Dim, &key)),
        crate::theme::with_theme(|theme| {
            theme.fg(ThemeColor::Muted, &format!(" {description}"))
        })
    )
}
