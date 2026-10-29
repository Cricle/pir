//! The process-global active theme and its settings-driven resolution
//! (upstream `themeController` state + `applyFromSettings`) plus the
//! terminal's reported colors (upstream `theme.ts:133-155`).

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

use super::color::{hex_of_rgb, index_to_rgb};
use super::osc11::{
    detect_colorfgbg_theme, record_auto_theme_names, recorded_auto_theme_names,
    set_terminal_color_scheme_notifications, TERMINAL_QUERY_TIMEOUT,
};
use super::palette::{
    palette_vars, resolve_palette_value, Appearance, DARK_PALETTE, LIGHT_PALETTE,
};
use super::registry::custom_themes_cell;
use super::system_theme::{generate_system_theme_colors, SystemThemeInput, SYSTEM_THEME_NAME};
use super::terminal_colors::{detect_terminal_theme, TerminalColors};
use super::theme_json::{resolve_color, ColorValue, ResolvedColor};
use super::Theme;

fn theme_cell() -> &'static Mutex<Theme> {
    static CELL: OnceLock<Mutex<Theme>> = OnceLock::new();
    CELL.get_or_init(|| Mutex::new(Theme::from_builtin("dark")))
}

/// Monotonic generation of the *active* theme: every [`set_theme`] /
/// [`set_theme_from_settings`] install bumps it, so render caches whose lines
/// carry theme-derived colors (`Markdown`'s line cache, the assistant
/// message's per-segment cache) drop stale entries after a live
/// `ctx.ui.setTheme`, a settings-menu swap, or a theme-file hot reload.
///
/// The counter is process-wide state of the active theme (not of a [`Theme`]
/// value), so it is published as the associated function
/// [`Theme::generation`] — `state` itself is a private module and `Theme` is
/// the type its callers already name.
fn theme_generation_counter() -> &'static AtomicU64 {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    &COUNTER
}

/// Record a theme install. Callers must already have stored the new theme in
/// [`theme_cell`] *and still hold its lock*: a reader that observes the new
/// generation is then guaranteed to observe the new palette, so it can never
/// cache old-palette lines under a fresh generation.
fn bump_theme_generation() {
    theme_generation_counter().fetch_add(1, Ordering::SeqCst);
}

impl Theme {
    /// The generation of the active theme (see
    /// [`theme_generation_counter`]). Caches that embed theme colors fold it
    /// into their key and re-render when it moved; generations are never
    /// reused, so a mis-tagged entry (read generation, then the theme changed
    /// mid-render) is dropped by the next frame instead of sticking.
    pub fn generation() -> u64 {
        theme_generation_counter().load(Ordering::SeqCst)
    }
}

/// The terminal's reported colors (upstream `terminalColors`, theme.ts:133).
/// `None` until the terminal reported anything at all, which is what lets a
/// timeout keep the colors an earlier query delivered (upstream keeps the
/// previous object when a field is missing).
fn terminal_colors_cell() -> &'static Mutex<Option<TerminalColors>> {
    static CELL: OnceLock<Mutex<Option<TerminalColors>>> = OnceLock::new();
    CELL.get_or_init(|| Mutex::new(None))
}

/// While the terminal color query is in flight, the system theme renders in
/// grayscale (upstream `terminalColorsPending`, theme.ts:135).
fn terminal_colors_pending_flag() -> &'static AtomicBool {
    static FLAG: OnceLock<AtomicBool> = OnceLock::new();
    FLAG.get_or_init(|| AtomicBool::new(false))
}

/// The terminal's last mode-2031 light/dark report; only used while it has
/// not reported a background (upstream `terminalColorScheme`, theme.ts:137).
fn terminal_color_scheme_cell() -> &'static Mutex<Option<Appearance>> {
    static CELL: OnceLock<Mutex<Option<Appearance>>> = OnceLock::new();
    CELL.get_or_init(|| Mutex::new(None))
}

/// Record the terminal's reported colors (upstream `setTerminalColors`,
/// theme.ts:143-147). Also ends the pending state, so the next system theme
/// is generated in color.
pub fn set_terminal_colors(colors: TerminalColors) {
    *terminal_colors_cell()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(colors);
    terminal_colors_pending_flag().store(false, Ordering::SeqCst);
}

/// Render the system theme in grayscale until [`set_terminal_colors`] reports
/// the terminal's colors (upstream `markTerminalColorsPending`, theme.ts:154-156).
pub fn mark_terminal_colors_pending() {
    terminal_colors_pending_flag().store(true, Ordering::SeqCst);
}

/// Record the terminal's mode-2031 light/dark report, the fallback for
/// terminals that do not report their background (upstream
/// `setTerminalColorScheme`, theme.ts:149-151).
pub fn set_terminal_color_scheme(scheme: Option<Appearance>) {
    *terminal_color_scheme_cell()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = scheme;
}

/// The terminal's reported colors; empty when nothing was reported.
pub fn terminal_colors() -> TerminalColors {
    terminal_colors_cell()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
        .unwrap_or_default()
}

/// The terminal's last mode-2031 report.
pub fn terminal_color_scheme() -> Option<Appearance> {
    *terminal_color_scheme_cell()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Whether the terminal color query is still in flight.
pub fn terminal_colors_pending() -> bool {
    terminal_colors_pending_flag().load(Ordering::SeqCst)
}

/// Whether the terminal is dark or light, from everything it reported so far
/// (upstream `getTerminalTheme`, theme.ts:639-641): the reported background,
/// then its mode-2031 report, then `COLORFGBG`, then dark.
pub fn get_terminal_theme() -> Appearance {
    detect_terminal_theme(
        &terminal_colors(),
        terminal_color_scheme(),
        detect_colorfgbg_theme(),
    )
}

/// Generate the `system` theme from the terminal's reported colors (upstream
/// `createSystemTheme`, theme.ts:537-548): grayscale while the color query is
/// in flight, in color once the terminal reported.
pub fn system_theme() -> Theme {
    let generated = generate_system_theme_colors(&SystemThemeInput {
        saturation: Some(if terminal_colors_pending() { 0.0 } else { 1.0 }),
        appearance_hint: Some(get_terminal_theme()),
        ..SystemThemeInput::from_terminal_colors(&terminal_colors())
    });
    Theme::from_system_colors(&generated)
}

/// Fold a terminal color report into the recorded colors and re-apply the
/// theme, the way upstream `applyTerminalColors` (theme-controller.ts:197-213)
/// does: a field the terminal did not report keeps its previous value, a
/// report that changes nothing is ignored, and a change re-applies the theme.
/// Returns whether anything changed (upstream's early-return condition).
pub fn apply_terminal_colors(reported: &TerminalColors) -> bool {
    // The query settled (answered or timed out), so the system theme's
    // grayscale first frame is over even when this report changes nothing
    // (upstream clears the flag in `setTerminalColors`, which the controller's
    // `requestTerminalColors` always reaches on the first apply).
    terminal_colors_pending_flag().store(false, Ordering::SeqCst);
    let previous = terminal_colors_cell()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    let next = match &previous {
        Some(previous) => previous.merge_reported(reported),
        None => reported.clone(),
    };
    if previous.as_ref() == Some(&next) {
        return false;
    }
    set_terminal_colors(next);
    reapply_for_terminal();
    true
}

/// Re-apply the theme after the terminal's colors or appearance changed
/// (upstream `reapplyForTerminal`, theme-controller.ts:217-224): regenerate
/// the system theme, or switch the theme of a `light/dark` pair.
pub fn reapply_for_terminal() {
    let name = current_theme_name();
    if name == SYSTEM_THEME_NAME {
        set_theme(SYSTEM_THEME_NAME);
        return;
    }
    // A pair setting resolves by the terminal's appearance; a theme set
    // explicitly (settings or `--theme`) is left alone.
    if let Some((light, dark)) = recorded_auto_theme_names() {
        let target = if get_terminal_theme() == Appearance::Light {
            light
        } else {
            dark
        };
        if target != name {
            set_theme(&target);
        }
    }
}

/// Set the global theme by name. The `system` name is reserved and generated
/// from the terminal's colors (it wins over a custom theme of the same name,
/// upstream `loadTheme`, theme.ts:557-559); custom themes registered via
/// [`super::load_custom_themes`] / [`super::register_custom_theme`] win over
/// the built-ins; unknown names fall back to dark.
pub fn set_theme(name: &str) {
    if name == SYSTEM_THEME_NAME {
        let theme = system_theme();
        let mut cell = theme_cell()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *cell = theme;
        bump_theme_generation();
        return;
    }
    let theme = custom_themes_cell()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(name)
        .cloned()
        .map(|json| Theme::from_json(&json))
        .transpose()
        .ok()
        .flatten()
        .unwrap_or_else(|| Theme::from_builtin(name));
    let mut cell = theme_cell()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    *cell = theme;
    bump_theme_generation();
}

/// Run `f` with the global theme.
pub fn with_theme<T>(f: impl FnOnce(&Theme) -> T) -> T {
    let cell = theme_cell()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    f(&cell)
}

/// The name of the currently active global theme (extension `ui.theme`).
pub fn current_theme_name() -> String {
    with_theme(|theme| theme.name.clone())
}

/// The appearance of the currently active global theme (declared in the
/// theme JSON, else detected from its colors, upstream `Theme.appearance`).
pub fn theme_appearance() -> Appearance {
    with_theme(|theme| theme.appearance)
}

/// The `export` colors of a theme resolved to CSS-ready hex strings
/// (upstream `getThemeExportColors`). `None` selects the currently active
/// theme. Built-in themes resolve their `export` block from the transcribed
/// upstream dark.json/light.json (`okhsl()` values become hex); custom
/// themes without an `export` block keep every field `None` and the HTML
/// export keeps its defaults.
pub fn theme_export_colors(name: Option<&str>) -> ExportColorsResolved {
    let theme_name = match name {
        Some(name) => name.to_string(),
        None => current_theme_name(),
    };
    let json = custom_themes_cell()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(&theme_name)
        .cloned();
    let Some(json) = json else {
        return builtin_export_colors(&theme_name);
    };
    let Some(export) = &json.export else {
        return ExportColorsResolved::default();
    };
    let resolve = |value: Option<&ColorValue>| -> Option<String> {
        match resolve_color(value?, &json.vars) {
            Ok(ResolvedColor::Hex(hex)) => Some(hex),
            Ok(ResolvedColor::Index(index)) => {
                let (r, g, b) = index_to_rgb(index);
                Some(hex_of_rgb(r, g, b))
            }
            _ => None,
        }
    };
    ExportColorsResolved {
        page_bg: resolve(export.page_bg.as_ref()),
        card_bg: resolve(export.card_bg.as_ref()),
        info_bg: resolve(export.info_bg.as_ref()),
    }
}

/// The built-in themes' `export` blocks (upstream `getThemeExportColors`
/// reads the shipped dark.json/light.json export sections).
fn builtin_export_colors(name: &str) -> ExportColorsResolved {
    let palette = match name {
        "dark" => &DARK_PALETTE,
        "light" => &LIGHT_PALETTE,
        _ => return ExportColorsResolved::default(),
    };
    let vars = palette_vars(palette);
    let resolve = |key: &str| -> Option<String> {
        let value = palette
            .export
            .iter()
            .find(|(k, _)| *k == key)
            .map(|(_, v)| *v)?;
        Some(resolve_palette_value(value, &vars)).filter(|hex| !hex.is_empty())
    };
    ExportColorsResolved {
        page_bg: resolve("pageBg"),
        card_bg: resolve("cardBg"),
        info_bg: resolve("infoBg"),
    }
}

/// Resolved `export` colors as CSS-ready hex strings (upstream
/// `getThemeExportColors`' return shape).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ExportColorsResolved {
    pub page_bg: Option<String>,
    pub card_bg: Option<String>,
    pub info_bg: Option<String>,
}

/// Upstream `parseAutoThemeSetting`: a `"lightTheme/darkTheme"` pair
/// (exactly one `/`) selects between themes by terminal appearance.
pub fn parse_auto_theme_setting(setting: &str) -> Option<(String, String)> {
    let slash = setting.find('/')?;
    if setting[slash + 1..].contains('/') {
        return None;
    }
    let light = setting[..slash].trim();
    let dark = setting[slash + 1..].trim();
    if light.is_empty() || dark.is_empty() {
        return None;
    }
    Some((light.to_string(), dark.to_string()))
}

/// Set the theme from settings (upstream `applyFromSettings` + `resolveThemeSetting`):
/// - `"lightTheme/darkTheme"` picks by the terminal's appearance;
/// - `"auto"` resolves to the detected terminal appearance (OSC 11 query
///   with a 50 ms timeout, then `COLORFGBG`, then dark — pir queries the
///   terminal synchronously during startup, before crossterm owns stdin, and
///   the reader never takes keyboard input away from the app: on Windows it
///   only consumes console records that already spell out a terminal reply, and
///   on Unix it now `poll`s fd 0 and reads only when the terminal is readable
///   (`osc11::unix_reply_reader`), handing back every byte that cannot begin a
///   recognized reply in arrival order — so a terminal that does not answer the
///   query has no keystroke taken from it on either platform);
/// - `"light"`, any registered custom theme name, and unknown names behave
///   as before (unknown names fall back to dark).
pub fn set_theme_from_settings(theme_setting: Option<&str>) {
    let setting = theme_setting.unwrap_or("system");
    // The `system` theme starts grayscale and turns colorful once the
    // terminal's colors arrive (upstream `markTerminalColorsPending` before
    // `initTheme`, theme-controller.ts:104-105).
    mark_terminal_colors_pending();
    // Query the terminal's colors once, up front: the appearance and the
    // system theme both read them (upstream queries in `startup-ui.ts:100-111`
    // before the theme applies). A terminal that reports nothing clears the
    // pending state with empty colors, which is the indexed fallback tier.
    let reported =
        crate::tui::theme::osc11::query_terminal_colors(TERMINAL_QUERY_TIMEOUT).unwrap_or_default();
    apply_terminal_colors(&reported);
    if let Some((light, dark)) = parse_auto_theme_setting(setting) {
        record_auto_theme_names(&light, &dark);
        // A pair follows terminal appearance changes (mode 2031) upstream.
        set_terminal_color_scheme_notifications(true);
        let appearance = get_terminal_theme();
        set_theme(if appearance == Appearance::Light {
            light.as_str()
        } else {
            dark.as_str()
        });
        // A settings-driven resolution is a new theme epoch even when the
        // resolved name — and therefore the palette — repeats, so every
        // render cache keyed on the generation drops (the extra bump sits
        // *after* the install to keep the "new generation ⇒ new palette"
        // ordering invariant).
        bump_theme_generation();
        return;
    }
    let resolved = match setting {
        // Plain "auto" is the ("light", "dark") pair: record it too so a late
        // color reply re-resolves the appearance (upstream `onLateReply`).
        "auto" => {
            record_auto_theme_names("light", "dark");
            set_terminal_color_scheme_notifications(true);
            get_terminal_theme().as_str()
        }
        "light" => "light",
        other => other,
    };
    // The system theme follows the terminal's light/dark reports (upstream
    // `setAutoSync`: a pair or the system theme).
    set_terminal_color_scheme_notifications(resolved == SYSTEM_THEME_NAME);
    set_theme(resolved);
    // See the pair branch above: the settings path always starts a new
    // theme epoch, after the install.
    bump_theme_generation();
}
