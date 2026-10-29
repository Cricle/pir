//! Extension-contributed resource paths (`resources_discover`, upstream
//! `ResourceExtensionPaths` + `DefaultResourceLoader.extendResources`,
//! resource-loader.ts:493-528 / runner.ts `emitResourcesDiscover`).
//!
//! The event's result carries `skillPaths` / `promptPaths` / `themePaths`; a
//! host that receives them resolves each entry (absolute, relative to the
//! session cwd, or `~`-prefixed) and merges the loaded resources into its
//! existing sets **first-wins**, exactly like upstream `mergePaths` (a
//! resource already present under a name keeps its position and definition).
//!
//! This module is the shared loader for both consumers — the session host
//! ([`crate::agent_session`]) and the SDK's
//! [`DefaultResourceLoader`](crate::sdk::DefaultResourceLoader) — so the path
//! grammar cannot drift between them. It contains no runtime/host state; the
//! theme half is feature-gated because theme rendering lives in the `tui`
//! layer.

use std::path::{Path, PathBuf};

use serde_json::Value as JsonValue;

use crate::coding_agent::prompt_templates::PromptTemplate;
use crate::skills::SkillInfo;

/// Paths contributed by one `resources_discover` dispatch, already resolved
/// against the session cwd.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ExtensionResourcePaths {
    pub skill_paths: Vec<PathBuf>,
    pub prompt_paths: Vec<PathBuf>,
    pub theme_paths: Vec<PathBuf>,
}

impl ExtensionResourcePaths {
    /// Parse the merged handler result (`{skillPaths, promptPaths,
    /// themePaths}`) and resolve every entry against `cwd`.
    ///
    /// Non-array values and non-string entries are ignored (the JS merge layer
    /// already coerces path lists; a malformed hand-written result must not
    /// abort the host). Empty entries are skipped.
    pub fn from_event_result(result: &JsonValue, cwd: &Path) -> Self {
        Self {
            skill_paths: resolve_paths(result.get("skillPaths"), cwd),
            prompt_paths: resolve_paths(result.get("promptPaths"), cwd),
            theme_paths: resolve_paths(result.get("themePaths"), cwd),
        }
    }

    /// Whether every list is empty.
    pub fn is_empty(&self) -> bool {
        self.skill_paths.is_empty() && self.prompt_paths.is_empty() && self.theme_paths.is_empty()
    }
}

/// Resolve one string path list against `cwd` (absolute and `~`-prefixed
/// entries keep their meaning).
fn resolve_paths(value: Option<&JsonValue>, cwd: &Path) -> Vec<PathBuf> {
    let Some(entries) = value.and_then(JsonValue::as_array) else {
        return Vec::new();
    };
    entries
        .iter()
        .filter_map(JsonValue::as_str)
        .map(str::trim)
        .filter(|entry| !entry.is_empty())
        .map(|entry| resolve_path(entry, cwd))
        .collect()
}

/// Resolve one path: `~` against the process home (the same boundary the
/// resource discovery uses), relative against `cwd`.
pub fn resolve_path(entry: &str, cwd: &Path) -> PathBuf {
    let expanded = crate::coding_agent::resources::expand_tilde(entry);
    if expanded.is_absolute() {
        expanded
    } else {
        cwd.join(expanded)
    }
}

/// What one `resources_discover` dispatch installed into a host's resource
/// set (returned by
/// [`AgentSession::reload_extension_resources`](crate::agent_session::AgentSession::reload_extension_resources)).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ExtensionResourceReload {
    /// Skills newly added to the session's skill set (collisions excluded).
    pub skills: usize,
    /// Prompt templates newly added to the session's template set.
    pub prompt_templates: usize,
    /// Theme files handed to the theme registry.
    pub theme_files: usize,
    /// Load/collision warnings, already routed to the diagnostic log.
    pub diagnostics: Vec<String>,
}

impl ExtensionResourceReload {
    /// Whether the dispatch contributed nothing.
    pub fn is_empty(&self) -> bool {
        self.skills == 0 && self.prompt_templates == 0 && self.theme_files == 0
    }
}

/// Everything one `resources_discover` result contributed, plus load
/// warnings (missing paths, malformed files, name collisions).
#[derive(Debug, Default, Clone)]
pub struct LoadedExtensionResources {
    pub skills: Vec<SkillInfo>,
    pub prompt_templates: Vec<PromptTemplate>,
    /// Expanded `*.json` theme files (directories are expanded one level).
    pub theme_files: Vec<PathBuf>,
    pub diagnostics: Vec<String>,
}

/// Load the resources behind `paths`.
///
/// A skill path may be a skill file or a directory (scanned for `SKILL.md`
/// entries and root-level markdown, the same rules as `--skill`); a prompt
/// path may be a `.md` template file or a directory scanned non-recursively
/// (upstream `loadPromptsFromDir`). Missing paths are reported once in
/// [`LoadedExtensionResources::diagnostics`] and skipped.
pub fn load_extension_resources(
    _cwd: &Path,
    paths: &ExtensionResourcePaths,
) -> LoadedExtensionResources {
    let mut loaded = LoadedExtensionResources::default();

    for path in &paths.skill_paths {
        if path.is_file() {
            match crate::skills::load_skill_from_file(path) {
                Ok(skill) => loaded.skills.push(skill),
                Err(error) => loaded
                    .diagnostics
                    .push(format!("warning: {error} ({})", path.display())),
            }
        } else if path.is_dir() {
            let (skills, diagnostics) =
                crate::skills::discover_skills_with_diagnostics(std::slice::from_ref(path));
            loaded.skills.extend(skills);
            loaded.diagnostics.extend(
                diagnostics
                    .iter()
                    .map(|diagnostic| diagnostic.message.clone()),
            );
        } else {
            loaded
                .diagnostics
                .push(format!("skill path does not exist: {}", path.display()));
        }
    }

    for path in &paths.prompt_paths {
        if path.is_file() {
            match crate::coding_agent::prompt_templates::load_template_file(path) {
                Ok(template) => loaded.prompt_templates.push(template),
                Err(error) => loaded.diagnostics.push(format!("warning: {error}")),
            }
        } else if path.is_dir() {
            let (templates, warnings) =
                crate::coding_agent::prompt_templates::load_templates_from_dir(path);
            loaded.prompt_templates.extend(templates);
            loaded.diagnostics.extend(warnings);
        } else {
            loaded.diagnostics.push(format!(
                "prompt template path does not exist: {}",
                path.display()
            ));
        }
    }

    for path in &paths.theme_paths {
        if path.is_file() {
            loaded.theme_files.push(path.clone());
        } else if path.is_dir() {
            let mut files = theme_files_in_dir(path);
            if files.is_empty() {
                loaded.diagnostics.push(format!(
                    "theme path has no *.json files: {}",
                    path.display()
                ));
            }
            loaded.theme_files.append(&mut files);
        } else {
            loaded
                .diagnostics
                .push(format!("theme path does not exist: {}", path.display()));
        }
    }

    loaded
}

/// Immediate `*.json` files of one theme directory, in file-name order.
fn theme_files_in_dir(dir: &Path) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.is_file()
                && path
                    .extension()
                    .and_then(|extension| extension.to_str())
                    .is_some_and(|extension| extension.eq_ignore_ascii_case("json"))
        })
        .collect();
    files.sort();
    files
}

/// Merge contributed skills into `existing`, first-wins by skill name.
/// Collisions are reported (the existing definition is kept).
pub fn merge_skills(
    existing: &mut Vec<SkillInfo>,
    incoming: Vec<SkillInfo>,
    diagnostics: &mut Vec<String>,
) {
    for skill in incoming {
        if existing.iter().any(|kept| kept.name == skill.name) {
            let kept = existing
                .iter()
                .find(|kept| kept.name == skill.name)
                .map(|kept| kept.path.display().to_string())
                .unwrap_or_default();
            diagnostics.push(format!(
                "skill name \"{}\" collision (kept {}, ignored {})",
                skill.name,
                kept,
                skill.path.display()
            ));
        } else {
            existing.push(skill);
        }
    }
}

/// Merge contributed prompt templates into `existing`, first-wins by command
/// name (upstream prompt-template collision ordering).
pub fn merge_prompt_templates(
    existing: &mut Vec<PromptTemplate>,
    incoming: Vec<PromptTemplate>,
    diagnostics: &mut Vec<String>,
) {
    for template in incoming {
        if existing.iter().any(|kept| kept.name == template.name) {
            let kept = existing
                .iter()
                .find(|kept| kept.name == template.name)
                .map(|kept| kept.file_path.display().to_string())
                .unwrap_or_default();
            diagnostics.push(format!(
                "prompt name \"{}\" collision (kept {}, ignored {})",
                template.name,
                kept,
                template.file_path.display()
            ));
        } else {
            existing.push(template);
        }
    }
}

/// Register contributed theme files in the process theme registry (upstream
/// `updateThemesFromPaths`).
///
/// Themes render in the TUI layer, so a build without the `tui` feature has no
/// theme consumer: the files are returned untouched with a diagnostic instead
/// of claiming they became selectable. Name collisions resolve first-wins in
/// the registry itself and are reported.
pub fn register_theme_files(files: &[PathBuf]) -> Vec<String> {
    #[cfg(feature = "tui")]
    {
        let loaded = crate::tui::theme::load_custom_theme_files(files);
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
    #[cfg(not(feature = "tui"))]
    {
        files
            .iter()
            .map(|file| {
                format!(
                    "theme {} loaded but not registered: this build has no TUI theme registry",
                    file.display()
                )
            })
            .collect()
    }
}
