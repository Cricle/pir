//! The custom-theme registry: registered themes, source paths, and
//! serializable snapshots for the extension API.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use crate::coding_agent::package_manager::TopLevelResourceFilter;

use super::palette::{DARK_PALETTE, LIGHT_PALETTE};
use super::theme_json::{parse_theme_json, ColorValue, ThemeJson};

pub(super) fn custom_themes_cell() -> &'static Mutex<BTreeMap<String, ThemeJson>> {
    static CELL: OnceLock<Mutex<BTreeMap<String, ThemeJson>>> = OnceLock::new();
    CELL.get_or_init(|| Mutex::new(BTreeMap::new()))
}

/// Register one parsed custom theme (replacing any theme with the same name).
pub fn register_custom_theme(theme: ThemeJson) {
    custom_themes_cell()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(theme.name.clone(), theme);
}

/// Register one parsed custom theme ONLY when no theme with the same name is
/// registered yet (first-wins, upstream resource-loader.ts:1186-1211).
/// Returns whether the theme was newly registered; `false` means a same-name
/// theme was already present and this one was skipped.
pub fn try_register_custom_theme(theme: ThemeJson) -> bool {
    let mut themes = custom_themes_cell()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if themes.contains_key(&theme.name) {
        return false;
    }
    let name = theme.name.clone();
    themes.insert(name, theme);
    true
}

/// Names [`load_custom_themes`] registered itself, so a later run can REPLACE
/// its own previous entries instead of colliding with them (upstream
/// `setRegisteredThemes` clears the registry and re-fills it from the fresh
/// discovery, theme.ts:672-680).
///
/// Themes registered by other surfaces — the extension API's
/// [`register_custom_theme`] and the package contribution's
/// [`load_custom_theme_files`] — are deliberately NOT tracked here: each is
/// re-supplied by its own owner, so a reload must not delete it (pir keeps
/// them first-wins, matching upstream's "an earlier registration wins" dedupe
/// order).
///
/// Bounded by the number of theme files one run discovered: every run replaces
/// the whole set with what it registered, so the cell never grows past one
/// run's registrations.
fn loader_owned_cell() -> &'static Mutex<std::collections::BTreeSet<String>> {
    static CELL: OnceLock<Mutex<std::collections::BTreeSet<String>>> = OnceLock::new();
    CELL.get_or_init(|| Mutex::new(std::collections::BTreeSet::new()))
}

/// Take the previous run's loader-owned names and drop their entries from the
/// registry, so the run about to start can register a same-name theme from
/// the CURRENT roots (a moved/changed agent dir, an edited theme file) instead
/// of colliding with the stale entry.
///
/// Only names a previous [`load_custom_themes`] actually registered are
/// dropped, so a theme the extension API registered ([`register_custom_theme`])
/// or a package contributed ([`load_custom_theme_files`]) — both of which
/// register through [`try_register_custom_theme`] and are therefore never in
/// this set — survives untouched. If a package/extension theme holds a name
/// the loader also reads, the loader collided with it first-wins and that name
/// is NOT in this set either, so the package/extension copy keeps winning
/// exactly as it does at startup.
fn take_loader_owned() -> std::collections::BTreeSet<String> {
    let previous = {
        let mut owned = loader_owned_cell()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        std::mem::take(&mut *owned)
    };
    if previous.is_empty() {
        return previous;
    }
    custom_themes_cell()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .retain(|name, _| !previous.contains(name));
    custom_theme_paths_cell()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .retain(|name, _| !previous.contains(name));
    previous
}

/// Publish the names this run registered as loader-owned.
fn store_loader_owned(names: std::collections::BTreeSet<String>) {
    *loader_owned_cell()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = names;
}

/// Result of [`load_custom_themes`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LoadedThemes {
    /// Names of successfully registered themes (per root, sorted by file
    /// name).
    pub names: Vec<String>,
    /// `(file, error)` for every skipped theme file — reported, never fatal.
    pub errors: Vec<(String, String)>,
    /// `(theme name, file)` for every file whose theme name collided with an
    /// already-registered theme and was skipped first-wins (upstream
    /// resource-loader.ts:1186-1211 records the same collision diagnostics;
    /// terminal.rs prints them like `errors`).
    pub collisions: Vec<(String, String)>,
}

/// Load and register the run's custom theme files.
///
/// `dir` is the user auto directory `<agent_dir>/themes` (the caller's
/// convention; its parent is treated as the agent dir, which anchors the
/// settings scopes). Besides scanning `dir` itself, the top-level settings
/// `themes` arrays are consumed as a source + filter layer, one per scope
/// (upstream `resolve()` -> `resolveLocalEntries` / `addAutoDiscoveredResources`,
/// package-manager.ts:946-976/2396-2479): plain entries are theme files (or
/// directories of `.json` files) resolved against that scope's base directory
/// (upstream `resolvePathFromBase`), while the same scope's `!`/`+`/`-` entries
/// decide which of the files it yields survive (upstream `splitPatterns` +
/// `applyPatterns` / `isEnabledByOverrides`, compiled by
/// [`TopLevelResourceFilter`]). The roots load in the upstream
/// `resourcePrecedenceRank` order: project settings entries, project auto dir
/// (`<cwd>/.pi/themes`, trust gated), user settings entries, then `dir`.
///
/// Every `*.json` file is parsed and validated; valid themes are registered
/// under their `name` field, invalid files are reported in
/// [`LoadedThemes::errors`] and skipped. Missing auto directories are not an
/// error; a missing settings-listed theme path is reported (upstream "theme
/// path does not exist" warning). This function never panics.
///
/// Integrator wiring for a future `--theme/--use-theme <name>` flag:
/// call this once at startup before [`super::set_theme`] /
/// [`super::set_theme_from_settings`]; a custom name passed to those (or via
/// the existing `settings.theme` field) then resolves to the registered
/// theme, falling back to the built-ins for unknown names:
///
/// ```ignore
/// let loaded = pir::tui::theme::load_custom_themes(&agent_dir.join("themes"))?;
/// for (file, error) in &loaded.errors {
///     eprintln!("pir: skipping theme {file}: {error}");
/// }
/// ```
pub fn load_custom_themes(dir: &Path) -> std::io::Result<LoadedThemes> {
    // The settings context of the run `dir` belongs to: the agent dir is the
    // directory the auto themes dir lives in, and project trust resolves
    // through the same headless path the TUI's resource discovery uses.
    let agent_dir = dir.parent().unwrap_or(dir).to_path_buf();
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let project_trusted = crate::config::trust::resolve_cwd_trust(&cwd, &agent_dir);
    let settings = crate::config::settings::SettingsManager::load(
        &agent_dir,
        &cwd.join(".pi"),
        &cwd,
        project_trusted,
    );
    load_custom_themes_with_context(dir, &settings, &agent_dir, &cwd, project_trusted)
}

/// [`load_custom_themes`] with the settings context passed in (keeps tests
/// hermetic: no ambient agent dir / cwd / trust resolution).
fn load_custom_themes_with_context(
    dir: &Path,
    settings: &crate::config::settings::SettingsManager,
    agent_dir: &Path,
    cwd: &Path,
    project_trusted: bool,
) -> std::io::Result<LoadedThemes> {
    // The caller's own directory keeps the original error contract: a
    // missing directory is empty, any other read failure is a hard error
    // (the root loader below re-reads it for the actual files).
    match std::fs::read_dir(dir) {
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    // A repeat load (a `/reload`, or a host re-running startup after an
    // agent-dir change) must not collide with the entries the PREVIOUS load
    // registered: those came from roots this run may no longer read. Release
    // them first so this run's own themes win and a theme that disappeared
    // stops resolving (upstream `setRegisteredThemes` replaces the whole set,
    // theme.ts:672-680).
    take_loader_owned();
    let mut loaded = LoadedThemes::default();
    for root in theme_roots(dir, settings, agent_dir, cwd, project_trusted) {
        load_theme_root(&root, &mut loaded);
    }
    store_loader_owned(loaded.names.iter().cloned().collect());
    Ok(loaded)
}

/// One scope's top-level `themes` settings array, split into the plain source
/// entries and the compiled `!`/`+`/`-` override layer (upstream
/// `splitPatterns` + `getOverridePatterns`, package-manager.ts:311-322,
/// 720-745; the same split `coding_agent::resources` applies to
/// `skills`/`prompts`).
struct ThemeScopeEntries {
    plain: Vec<String>,
    filter: TopLevelResourceFilter,
}

impl ThemeScopeEntries {
    /// Read one scope's raw settings document (`SettingsManager::project_raw`
    /// / `global_raw`; an untrusted project scope is already empty).
    fn from_raw(raw: &serde_json::Value) -> Self {
        let entries: Vec<String> = raw
            .get("themes")
            .and_then(serde_json::Value::as_array)
            .map(|array| {
                array
                    .iter()
                    .filter_map(serde_json::Value::as_str)
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default();
        // Plain entries are sources, never filters (upstream `splitPatterns`).
        let filter = TopLevelResourceFilter::from_entries(&entries);
        let plain = entries
            .into_iter()
            .filter(|entry| {
                !entry.starts_with('!') && !entry.starts_with('+') && !entry.starts_with('-')
            })
            .collect();
        Self { plain, filter }
    }
}

/// One theme discovery root: a directory (scanned for `.json` files) or a
/// single theme file, plus the scope filter its scope's `!`/`+`/`-` entries
/// apply to the files it yields.
struct ThemeRoot {
    path: PathBuf,
    filter: TopLevelResourceFilter,
    /// The base the scope's relative patterns resolve against (upstream
    /// `resolveLocalEntries` / `isEnabledByOverrides` `baseDir`).
    base_dir: PathBuf,
    /// `true` for settings-listed roots: a missing path is reported (upstream
    /// "theme path does not exist" warning). The auto dirs are silently
    /// skipped, like the plain directory scan.
    report_missing: bool,
}

/// The theme roots for one run in the upstream `resourcePrecedenceRank`
/// order (project settings entries, project auto dir, user settings entries,
/// then the caller's `dir` — the user auto `<agent_dir>/themes`).
fn theme_roots(
    dir: &Path,
    settings: &crate::config::settings::SettingsManager,
    agent_dir: &Path,
    cwd: &Path,
    project_trusted: bool,
) -> Vec<ThemeRoot> {
    let project_base = cwd.join(".pi");
    let project = ThemeScopeEntries::from_raw(&settings.project_raw);
    let user = ThemeScopeEntries::from_raw(&settings.global_raw);
    let mut roots: Vec<ThemeRoot> = Vec::new();
    if project_trusted {
        for entry in &project.plain {
            roots.push(ThemeRoot {
                path: resolve_scope_entry(entry, &project_base),
                filter: project.filter.clone(),
                base_dir: project_base.clone(),
                report_missing: true,
            });
        }
        roots.push(ThemeRoot {
            path: project_base.join("themes"),
            filter: project.filter.clone(),
            base_dir: project_base.clone(),
            report_missing: false,
        });
    }
    for entry in &user.plain {
        roots.push(ThemeRoot {
            path: resolve_scope_entry(entry, agent_dir),
            filter: user.filter.clone(),
            base_dir: agent_dir.to_path_buf(),
            report_missing: true,
        });
    }
    roots.push(ThemeRoot {
        path: dir.to_path_buf(),
        filter: user.filter,
        base_dir: agent_dir.to_path_buf(),
        report_missing: false,
    });
    roots
}

/// Resolve one settings entry against its scope's base directory (upstream
/// `resolvePathFromBase`; the same rule as `resources::resolve_entry` for
/// `skills`/`prompts`): `~` expands against the home directory, an absolute
/// path stays put, and a relative path resolves against the scope base
/// (`<cwd>/.pi` for the project scope, the agent dir for the user one).
fn resolve_scope_entry(entry: &str, base_dir: &Path) -> PathBuf {
    let expanded = crate::coding_agent::resources::expand_tilde(entry);
    if expanded.is_absolute() {
        expanded
    } else {
        base_dir.join(expanded)
    }
}

/// Load one root, applying the root's scope filter to every file it yields
/// before registration (upstream `isEnabledByOverrides` for the auto dirs,
/// `applyPatterns` for the files the settings entries expand to).
fn load_theme_root(root: &ThemeRoot, loaded: &mut LoadedThemes) {
    if root.path.is_dir() {
        let Ok(entries) = std::fs::read_dir(&root.path) else {
            return;
        };
        let mut files: Vec<PathBuf> = entries
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| path.extension().is_some_and(|ext| ext == "json"))
            .collect();
        files.sort();
        for file in files {
            if root.filter.keeps(&file, &root.base_dir) {
                register_theme_file(&file, loaded);
            }
        }
        return;
    }
    if root.path.is_file() {
        if root.path.extension().is_some_and(|ext| ext == "json") {
            if root.filter.keeps(&root.path, &root.base_dir) {
                register_theme_file(&root.path, loaded);
            }
        } else {
            loaded.errors.push((
                root.path.display().to_string(),
                "theme path is not a json file".to_string(),
            ));
        }
        return;
    }
    if root.report_missing {
        loaded.errors.push((
            root.path.display().to_string(),
            "theme path does not exist".to_string(),
        ));
    }
}

/// Parse and register one theme file: first-wins on a name collision
/// (upstream resource-loader.ts:1186-1211), parse/read failures and
/// collisions recorded in `loaded`.
fn register_theme_file(path: &Path, loaded: &mut LoadedThemes) {
    let file_name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let result = std::fs::read_to_string(path)
        .map_err(|error| error.to_string())
        .and_then(|text| parse_theme_json(&text));
    match result {
        Ok(theme) => {
            let name = theme.name.clone();
            // First-wins (upstream resource-loader.ts:1186-1211): a
            // same-name theme already registered — from an earlier root or
            // the extension API — is NOT replaced; the collision is
            // reported instead of silently hijacking the earlier theme.
            if try_register_custom_theme(theme) {
                loaded.names.push(name);
            } else {
                loaded.collisions.push((name, file_name));
            }
        }
        Err(error) => loaded.errors.push((file_name, error)),
    }
}

/// Load explicit theme files (the packages contribution, upstream
/// resource-loader.ts merges package-contributed theme paths into the same
/// registry; name collisions resolve FIRST-WINS via
/// [`try_register_custom_theme`] and are reported in
/// [`LoadedThemes::collisions`] instead of letting a package theme silently
/// hijack an already-registered name). Missing/unreadable files are reported
/// and skipped.
pub fn load_custom_theme_files(files: &[std::path::PathBuf]) -> LoadedThemes {
    let mut loaded = LoadedThemes::default();
    for path in files {
        register_theme_file(path, &mut loaded);
    }
    loaded
}

/// All registered custom theme names.
pub fn custom_theme_names() -> Vec<String> {
    custom_themes_cell()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .keys()
        .cloned()
        .collect()
}

/// One theme registry entry (upstream `getAllThemes()` items): the theme
/// name plus the file it was loaded from (`None` for the built-ins).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThemeEntry {
    pub name: String,
    pub path: Option<PathBuf>,
}

fn custom_theme_paths_cell() -> &'static Mutex<BTreeMap<String, PathBuf>> {
    static CELL: OnceLock<Mutex<BTreeMap<String, PathBuf>>> = OnceLock::new();
    CELL.get_or_init(|| Mutex::new(BTreeMap::new()))
}

/// Record the source path of a registered custom theme (companion to
/// [`register_custom_theme`]; used by the extension `getAllThemes` API).
pub fn register_custom_theme_path(name: &str, path: PathBuf) {
    custom_theme_paths_cell()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(name.to_string(), path);
}

/// All themes available for [`super::set_theme`] / `ui.setTheme`: built-ins
/// first (`dark`, `light`), the generated `system` theme (no file), then
/// every registered custom theme with its source path (upstream
/// `themeController.getAllThemes()`).
///
/// Deviation: upstream sorts `system` first (it is the default there,
/// `getAvailableThemesWithPaths`, theme.ts:425-433); pir keeps `dark` first
/// because the default theme remains `dark` and the extension-surface
/// contract lists the built-ins first.
pub fn all_theme_entries() -> Vec<ThemeEntry> {
    let mut entries = vec![
        ThemeEntry {
            name: "dark".to_string(),
            path: None,
        },
        ThemeEntry {
            name: "light".to_string(),
            path: None,
        },
        ThemeEntry {
            name: super::SYSTEM_THEME_NAME.to_string(),
            path: None,
        },
    ];
    for name in custom_theme_names() {
        let path = custom_theme_paths_cell()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&name)
            .cloned();
        entries.push(ThemeEntry { name, path });
    }
    entries
}

/// Whether `name` resolves in [`super::set_theme`] (built-in, generated
/// system theme, or registered custom theme). Extension `setTheme` validates
/// against this before applying.
pub fn is_known_theme(name: &str) -> bool {
    name == "dark"
        || name == "light"
        || name == super::SYSTEM_THEME_NAME
        || custom_theme_names().iter().any(|n| n == name)
}

/// A serializable snapshot of one theme for the extension API
/// (`ui.getTheme(name)` / `ui.theme`): `{name, vars, colors}` with concrete
/// resolved color strings.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct ThemeSnapshot {
    pub name: String,
    pub vars: BTreeMap<String, String>,
    pub colors: BTreeMap<String, String>,
}

/// Snapshot a registered theme by name without switching to it; `None` when
/// the name does not resolve (extension `getTheme(name)` returning
/// `undefined`).
pub fn theme_snapshot(name: &str) -> Option<ThemeSnapshot> {
    let json = custom_themes_cell()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(name)
        .cloned();
    if let Some(json) = json {
        return theme_snapshot_from_json(&json);
    }
    if name == super::SYSTEM_THEME_NAME {
        return Some(system_theme_snapshot());
    }
    if name != "dark" && name != "light" {
        return None;
    }
    // Built-ins: snapshot the transcribed upstream palettes directly.
    Some(builtin_theme_snapshot(name))
}

fn builtin_theme_snapshot(name: &str) -> ThemeSnapshot {
    let palette = if name == "dark" {
        &DARK_PALETTE
    } else {
        &LIGHT_PALETTE
    };
    // Concrete resolved hex strings, matching the custom-theme path (the
    // okhsl()/var values of the upstream built-in JSONs resolve to hex).
    let vars_map = super::palette::palette_vars(palette);
    ThemeSnapshot {
        name: name.to_string(),
        vars: palette
            .vars
            .iter()
            .map(|(k, v)| {
                (
                    (*k).to_string(),
                    super::palette::resolve_palette_value(v, &vars_map),
                )
            })
            .collect(),
        colors: palette
            .colors
            .iter()
            .map(|(k, v)| {
                (
                    (*k).to_string(),
                    super::palette::resolve_palette_value(v, &vars_map),
                )
            })
            .collect(),
    }
}

/// Build a `Theme` for a built-in name WITHOUT switching the process-global
/// theme — the explicit-theme render seam (tests and callers that must not
/// race the global state). `None` when the name is not a built-in.
pub fn builtin_theme(name: &str) -> Option<super::Theme> {
    if name != "dark" && name != "light" {
        return None;
    }
    let snapshot = builtin_theme_snapshot(name);
    let mut json = ThemeJson {
        name: snapshot.name,
        appearance: None,
        vars: BTreeMap::new(),
        colors: BTreeMap::new(),
        export: None,
    };
    for (key, hex) in snapshot.vars {
        json.vars.insert(key, ColorValue::Hex(hex));
    }
    for (key, hex) in snapshot.colors {
        json.colors.insert(key, ColorValue::Hex(hex));
    }
    super::Theme::from_theme_json(&json).ok()
}

/// Snapshot the generated `system` theme (extension `ui.getTheme("system")`):
/// the colors the terminal's reports produce right now, as concrete hex,
/// ANSI-index (`color:N`), or the terminal default (`""`) strings.
fn system_theme_snapshot() -> ThemeSnapshot {
    let generated = super::generate_system_theme_colors(&super::SystemThemeInput {
        saturation: Some(if super::terminal_colors_pending() {
            0.0
        } else {
            1.0
        }),
        appearance_hint: Some(super::get_terminal_theme()),
        ..super::SystemThemeInput::from_terminal_colors(&super::terminal_colors())
    });
    let text_of = |value: &ColorValue| -> String {
        match value {
            ColorValue::Hex(hex) => hex.clone(),
            ColorValue::Index(index) => format!("color:{index}"),
            ColorValue::Empty => String::new(),
            ColorValue::Oklch { l, c, h } => format!("oklch({l:.6} {c:.6} {h:.6})"),
            ColorValue::Okhsl { h, s, l } => format!("okhsl({h:.6} {s:.6} {l:.6})"),
            ColorValue::Var(name) => name.clone(),
        }
    };
    ThemeSnapshot {
        name: super::SYSTEM_THEME_NAME.to_string(),
        vars: BTreeMap::new(),
        colors: generated
            .colors
            .iter()
            .map(|(key, value)| ((*key).to_string(), text_of(value)))
            .collect(),
    }
}

fn theme_snapshot_from_json(json: &ThemeJson) -> Option<ThemeSnapshot> {
    // Resolve a color value against the theme vars (depth-bounded, mirrors
    // `resolve_color`); unresolvable references degrade to `None` (empty
    // string in the snapshot — never an error).
    fn text_of(
        value: &ColorValue,
        vars: &BTreeMap<String, ColorValue>,
        depth: usize,
    ) -> Option<String> {
        match value {
            ColorValue::Hex(hex) => Some(hex.clone()),
            ColorValue::Index(index) => Some(format!("color:{index}")),
            ColorValue::Empty => Some(String::new()),
            ColorValue::Oklch { l, c, h } => Some(format!("oklch({l:.6} {c:.6} {h:.6})")),
            ColorValue::Okhsl { h, s, l } => Some(format!("okhsl({h:.6} {s:.6} {l:.6})")),
            ColorValue::Var(name) if depth < 4 => vars
                .get(name)
                .and_then(|resolved| text_of(resolved, vars, depth + 1)),
            ColorValue::Var(_) => None,
        }
    }
    let mut vars = BTreeMap::new();
    for (key, value) in &json.vars {
        if let Some(text) = text_of(value, &json.vars, 0) {
            vars.insert(key.clone(), text);
        }
    }
    let mut colors = BTreeMap::new();
    for (key, value) in &json.colors {
        colors.insert(
            key.clone(),
            text_of(value, &json.vars, 0).unwrap_or_default(),
        );
    }
    Some(ThemeSnapshot {
        name: json.name.clone(),
        vars,
        colors,
    })
}
