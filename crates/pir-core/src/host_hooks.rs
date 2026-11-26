//! Optional hooks a front end can install into the engine.
//!
//! `pir-core` is UI-agnostic: it never names a renderer, a terminal library or
//! a theme registry. A few engine paths still have a natural UI half (the
//! session-HTML export palette, theme-file registration, the "press any key"
//! pause in the CLI migration notice). Those call sites resolve through the
//! process-wide hooks in this module.
//!
//! A front-end crate installs its implementations once at startup — for the
//! terminal UI, `pir_tui::install_host_hooks()` — and everything keeps working
//! exactly as it did when the TUI was compiled into the engine. With no hook
//! installed, every path has a documented headless fallback.

use std::path::PathBuf;
use std::sync::RwLock;

/// Theme colors used by the session → HTML exporter.
///
/// Each field is a CSS-ready value (`#rrggbb` or a terminal color when the
/// active theme is palette-indexed); `None` keeps the stylesheet default.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ExportPalette {
    pub page_bg: Option<String>,
    pub card_bg: Option<String>,
    pub info_bg: Option<String>,
    pub text: Option<String>,
    pub muted: Option<String>,
    pub user_bg: Option<String>,
    pub link: Option<String>,
    pub success: Option<String>,
    pub error: Option<String>,
    pub thinking: Option<String>,
}

/// Resolve the export palette for `theme_name` (`None` = the active theme).
pub type ExportPaletteProvider = fn(Option<&str>) -> ExportPalette;

/// Register contributed theme files and return human-readable diagnostics.
pub type ThemeRegistrar = fn(&[PathBuf]) -> Vec<String>;

/// Wait for one keypress (the CLI migration notice); return immediately when
/// raw mode is unavailable.
pub type KeypressWaiter = fn();

static EXPORT_PALETTE_PROVIDER: RwLock<Option<ExportPaletteProvider>> = RwLock::new(None);
static THEME_REGISTRAR: RwLock<Option<ThemeRegistrar>> = RwLock::new(None);
static KEYPRESS_WAITER: RwLock<Option<KeypressWaiter>> = RwLock::new(None);

/// Install the export-palette provider. Idempotent; the last install wins.
pub fn set_export_palette_provider(provider: ExportPaletteProvider) {
    if let Ok(mut slot) = EXPORT_PALETTE_PROVIDER.write() {
        *slot = Some(provider);
    }
}

/// Resolve the export palette, or `None` when no front end is installed (the
/// exporter then keeps its built-in dark stylesheet colors).
pub fn export_palette(theme_name: Option<&str>) -> Option<ExportPalette> {
    let provider = EXPORT_PALETTE_PROVIDER.read().ok().and_then(|slot| *slot)?;
    Some(provider(theme_name))
}

/// Install the theme-file registrar. Idempotent; the last install wins.
pub fn set_theme_registrar(registrar: ThemeRegistrar) {
    if let Ok(mut slot) = THEME_REGISTRAR.write() {
        *slot = Some(registrar);
    }
}

/// Register theme files through the installed front end. `None` means no UI
/// owns a theme registry in this process.
pub fn register_theme_files(files: &[PathBuf]) -> Option<Vec<String>> {
    let registrar = THEME_REGISTRAR.read().ok().and_then(|slot| *slot)?;
    Some(registrar(files))
}

/// Install the keypress waiter. Idempotent; the last install wins.
pub fn set_keypress_waiter(waiter: KeypressWaiter) {
    if let Ok(mut slot) = KEYPRESS_WAITER.write() {
        *slot = Some(waiter);
    }
}

/// Wait for one keypress when a front end is installed; otherwise return
/// immediately (the headless behavior).
pub fn wait_for_keypress() {
    if let Some(waiter) = KEYPRESS_WAITER.read().ok().and_then(|slot| *slot) {
        waiter();
    }
}
