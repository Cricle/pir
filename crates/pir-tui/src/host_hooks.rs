//! Terminal-front-end implementations of the engine's UI hooks.
//!
//! `pir_core` never links a renderer. It exposes three process-wide hooks
//! ([`pir_core::host_hooks`]) for the few engine paths that have a natural UI
//! half; this module supplies the terminal implementations and
//! [`install_host_hooks`] wires all of them in one call.
//!
//! Call [`install_host_hooks`] once at process startup (the `pir` binary does
//! this before dispatching any command; `run` also calls it defensively). With
//! the hooks installed, theme-aware session export, contributed theme files
//! and the migration "press any key" pause behave exactly as they did when the
//! TUI was compiled into the engine.

use std::path::PathBuf;

use pir_core::host_hooks::{ExportPalette, ExportPaletteProvider};

/// Install every terminal implementation into the engine.
///
/// Idempotent and cheap: the setters just replace a function pointer.
pub fn install_host_hooks() {
    pir_core::host_hooks::set_export_palette_provider(export_palette as ExportPaletteProvider);
    pir_core::host_hooks::set_theme_registrar(register_theme_files);
    pir_core::host_hooks::set_keypress_waiter(wait_for_keypress);
}

/// Theme colors for the session → HTML exporter (`None` = active theme).
fn export_palette(theme_name: Option<&str>) -> ExportPalette {
    let export = crate::theme::theme_export_colors(theme_name);
    ExportPalette {
        page_bg: export.page_bg,
        card_bg: export.card_bg,
        info_bg: export.info_bg,
        text: theme_token_hex(theme_name, crate::theme::ThemeColor::Text),
        muted: theme_token_hex(theme_name, crate::theme::ThemeColor::Muted),
        user_bg: theme_token_hex(theme_name, crate::theme::ThemeColor::UserMessageBg),
        link: theme_token_hex(theme_name, crate::theme::ThemeColor::MdLink),
        success: theme_token_hex(theme_name, crate::theme::ThemeColor::Success),
        error: theme_token_hex(theme_name, crate::theme::ThemeColor::Error),
        thinking: theme_token_hex(theme_name, crate::theme::ThemeColor::ThinkingText),
    }
}

/// A theme foreground token resolved to a CSS-ready hex string. `theme_name`
/// selects the theme (`None` = the active theme, the same rule
/// [`crate::theme::theme_export_colors`] uses). Custom themes resolve to hex
/// like their export block; a value that is only a terminal palette index
/// keeps the stylesheet default.
fn theme_token_hex(theme_name: Option<&str>, color: crate::theme::ThemeColor) -> Option<String> {
    use crate::theme;
    let active = theme::current_theme_name();
    if theme_name.is_none_or(|name| name == active.as_str()) {
        return theme::with_theme(|theme| theme.resolved_color_hex(color));
    }
    let name = theme_name?;
    if let Some(theme) = theme::builtin_theme(name) {
        return theme.resolved_color_hex(color);
    }
    let snapshot = theme::theme_snapshot(name)?;
    let value = snapshot.colors.get(theme::Theme::key(color))?;
    value.starts_with('#').then(|| value.clone())
}

/// Register contributed theme files in the process theme registry.
fn register_theme_files(files: &[PathBuf]) -> Vec<String> {
    let loaded = crate::theme::load_custom_theme_files(files);
    let mut diagnostics = Vec::new();
    for (file, error) in loaded.errors {
        diagnostics.push(format!("warning: theme {file}: {error}"));
    }
    for (name, file) in loaded.collisions {
        diagnostics.push(format!(
            "theme name \"{name}\" collision (kept the registered theme, ignored {file})"
        ));
    }
    diagnostics
}

/// One raw-mode read; silently skipped when raw mode is unavailable.
fn wait_for_keypress() {
    use std::io::IsTerminal as _;

    if !std::io::stdin().is_terminal() {
        return;
    }
    if crossterm::terminal::enable_raw_mode().is_err() {
        return;
    }
    let _ = crossterm::event::read();
    let _ = crossterm::terminal::disable_raw_mode();
}
