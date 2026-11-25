//! [`DefaultResourceLoader`]: skills, prompt templates and extension-
//! contributed resource paths for an SDK session (upstream
//! `DefaultResourceLoader`, `core/resource-loader.ts`).
//!
//! The discovery rules themselves live in
//! [`crate::coding_agent::resources`] (the CLI's one implementation); this
//! wrapper adds the upstream object shape (`reload` / `getSkills` /
//! `getPromptTemplates` / `extendResources`) and the trust/path options an
//! embedder configures.
//!
//! The loader is **synchronous**: discovery is a handful of bounded
//! filesystem reads/listings. Async SDK hosts that care about blocking a
//! Tokio worker can call it from `spawn_blocking`.
//!
//! [`DefaultResourceLoader::extend_resources`] is the `resources_discover`
//! half: contributed paths are resolved, loaded, merged first-wins, and theme
//! files are registered in the process theme registry (the `tui` feature's
//! registry; a non-TUI build reports that no registry exists). The session
//! host performs the same work through
//! [`crate::extensions::resources`] when a JS/native handler returns paths.

use std::path::{Path, PathBuf};

use crate::coding_agent::prompt_templates::PromptTemplate;
use crate::coding_agent::resources::{DiscoveredResources, ResourceDiscoveryOptions};
use crate::extensions::resources::{
    load_extension_resources, merge_prompt_templates, merge_skills, register_theme_files,
    ExtensionResourcePaths,
};
use crate::skills::SkillInfo;

/// The resource-facing surface an SDK session consumes (upstream
/// `ResourceLoader`).
pub trait ResourceLoader {
    /// Re-read every configured source and return the merged result.
    fn reload(&mut self) -> DiscoveredResources;
    /// The skills discovered by the last [`ResourceLoader::reload`].
    fn skills(&mut self) -> Vec<SkillInfo>;
    /// The prompt templates discovered by the last
    /// [`ResourceLoader::reload`].
    fn prompt_templates(&mut self) -> Vec<PromptTemplate>;
}

/// The default resource loader: the CLI's discovery order (CLI paths, trusted
/// project scope, user scope, installed packages) plus a merge slot for
/// `resources_discover` contributions.
#[derive(Debug, Clone)]
pub struct DefaultResourceLoader {
    cwd: PathBuf,
    agent_dir: PathBuf,
    project_trusted: bool,
    cli_skill_paths: Vec<PathBuf>,
    cli_prompt_paths: Vec<PathBuf>,
    no_skills: bool,
    no_prompt_templates: bool,
    /// Explicit home directory for `~` expansion and the user-global
    /// `.agents/skills` scan (`None` = the process environment).
    home: Option<PathBuf>,
    /// Result of the last [`ResourceLoader::reload`], with contributed
    /// resources merged in.
    loaded: Option<DiscoveredResources>,
    /// `resources_discover` contributions, accumulated across calls (upstream
    /// `extendResources` merges into the last path lists rather than replacing
    /// them).
    extension_skills: Vec<SkillInfo>,
    extension_prompt_templates: Vec<PromptTemplate>,
    extension_theme_files: Vec<PathBuf>,
    extension_diagnostics: Vec<String>,
}

impl DefaultResourceLoader {
    /// Create a loader for `cwd` with the agent directory and trust state
    /// resolved from the process environment (upstream `create` + `reload`).
    pub fn new(cwd: impl Into<PathBuf>, agent_dir: impl Into<PathBuf>) -> Self {
        let cwd = cwd.into();
        let agent_dir = agent_dir.into();
        let project_trusted = crate::config::trust::resolve_cwd_trust(&cwd, &agent_dir);
        Self {
            cwd,
            agent_dir,
            project_trusted,
            cli_skill_paths: Vec::new(),
            cli_prompt_paths: Vec::new(),
            no_skills: false,
            no_prompt_templates: false,
            home: None,
            loaded: None,
            extension_skills: Vec::new(),
            extension_prompt_templates: Vec::new(),
            extension_theme_files: Vec::new(),
            extension_diagnostics: Vec::new(),
        }
    }

    /// Force the project scope's trust decision (upstream
    /// `SettingsManager.setProjectTrusted` + reload).
    pub fn with_project_trusted(mut self, trusted: bool) -> Self {
        self.project_trusted = trusted;
        self.loaded = None;
        self
    }

    /// Explicit skill paths (`--skill`: files or directories).
    pub fn with_skill_paths<I, P>(mut self, paths: I) -> Self
    where
        I: IntoIterator<Item = P>,
        P: Into<PathBuf>,
    {
        self.cli_skill_paths = paths.into_iter().map(Into::into).collect();
        self.loaded = None;
        self
    }

    /// Explicit prompt-template paths (`--prompt-template`: files or
    /// directories).
    pub fn with_prompt_paths<I, P>(mut self, paths: I) -> Self
    where
        I: IntoIterator<Item = P>,
        P: Into<PathBuf>,
    {
        self.cli_prompt_paths = paths.into_iter().map(Into::into).collect();
        self.loaded = None;
        self
    }

    /// `--no-skills`: keep only the explicit skill paths.
    pub fn without_skills(mut self) -> Self {
        self.no_skills = true;
        self.loaded = None;
        self
    }

    /// `--no-prompt-templates`: keep only the explicit template paths.
    pub fn without_prompt_templates(mut self) -> Self {
        self.no_prompt_templates = true;
        self.loaded = None;
        self
    }

    /// Resolve `~` and the user-global `.agents/skills` scan against `home`
    /// instead of the process environment (hermetic hosts/tests).
    pub fn with_home(mut self, home: impl Into<PathBuf>) -> Self {
        self.home = Some(home.into());
        self.loaded = None;
        self
    }

    /// The working directory used for relative resource paths and the project
    /// scope.
    pub fn cwd(&self) -> &Path {
        &self.cwd
    }

    /// The agent directory used for the user scope and installed packages.
    pub fn agent_dir(&self) -> &Path {
        &self.agent_dir
    }

    /// Whether the project scope is trusted.
    pub fn project_trusted(&self) -> bool {
        self.project_trusted
    }

    /// The theme files contributed by `resources_discover` (already
    /// registered in the TUI registry when that feature is compiled in).
    pub fn theme_files(&self) -> &[PathBuf] {
        &self.extension_theme_files
    }

    /// Diagnostics from the last discovery plus extension contributions.
    pub fn diagnostics(&mut self) -> Vec<String> {
        let mut diagnostics = self
            .loaded
            .as_ref()
            .map(|loaded| loaded.diagnostics.clone())
            .unwrap_or_default();
        diagnostics.extend(self.extension_diagnostics.iter().cloned());
        diagnostics
    }

    /// Merge one `resources_discover` result into the resource set
    /// (upstream `extendResources`): skills/templates load now, first-wins;
    /// theme files register in the process theme registry. Returns the load
    /// diagnostics.
    pub fn extend_resources(&mut self, paths: &ExtensionResourcePaths) -> Vec<String> {
        // Collisions are reported against the CURRENT resource set, so make
        // sure it exists (the first extend is usually called right after the
        // loader is created).
        if self.loaded.is_none() {
            let _ = self.reload();
        }
        let loaded = load_extension_resources(&self.cwd, paths);
        let mut diagnostics = loaded.diagnostics;
        // Report against a view of the live set: the discovered resources plus
        // every contribution installed so far.
        let base = self.loaded.clone().unwrap_or_default();
        let mut view_skills = base.skills;
        merge_skills(&mut view_skills, loaded.skills.clone(), &mut diagnostics);
        let mut view_templates = base.prompt_templates;
        merge_prompt_templates(
            &mut view_templates,
            loaded.prompt_templates.clone(),
            &mut diagnostics,
        );
        // Record the contributions (their own collisions were reported above;
        // the second merge must not duplicate them).
        let mut recorded = Vec::new();
        merge_skills(&mut self.extension_skills, loaded.skills, &mut recorded);
        merge_prompt_templates(
            &mut self.extension_prompt_templates,
            loaded.prompt_templates,
            &mut recorded,
        );
        let mut new_theme_files = Vec::new();
        for file in loaded.theme_files {
            if !self.extension_theme_files.contains(&file) {
                self.extension_theme_files.push(file.clone());
                new_theme_files.push(file);
            }
        }
        if !new_theme_files.is_empty() {
            diagnostics.extend(register_theme_files(&new_theme_files));
        }
        self.extension_diagnostics = diagnostics.clone();
        self.loaded = None;
        diagnostics
    }
}

impl ResourceLoader for DefaultResourceLoader {
    fn reload(&mut self) -> DiscoveredResources {
        let options = ResourceDiscoveryOptions {
            cwd: &self.cwd,
            agent_dir: &self.agent_dir,
            project_trusted: self.project_trusted,
            cli_skill_paths: &self.cli_skill_paths,
            cli_prompt_paths: &self.cli_prompt_paths,
            no_skills: self.no_skills,
            no_prompt_templates: self.no_prompt_templates,
        };
        let mut discovered = match self.home.as_deref() {
            Some(home) => DiscoveredResources::discover_with_home(&options, Some(home)),
            None => DiscoveredResources::discover(&options),
        };
        // Contributed resources rank after the discovered ones (upstream
        // merges them into the extension path tail and keeps first-wins).
        merge_skills(
            &mut discovered.skills,
            self.extension_skills.clone(),
            &mut discovered.diagnostics,
        );
        merge_prompt_templates(
            &mut discovered.prompt_templates,
            self.extension_prompt_templates.clone(),
            &mut discovered.diagnostics,
        );
        self.loaded = Some(discovered.clone());
        discovered
    }

    fn skills(&mut self) -> Vec<SkillInfo> {
        if self.loaded.is_none() {
            self.reload();
        }
        self.loaded
            .as_ref()
            .map(|loaded| loaded.skills.clone())
            .unwrap_or_default()
    }

    fn prompt_templates(&mut self) -> Vec<PromptTemplate> {
        if self.loaded.is_none() {
            self.reload();
        }
        self.loaded
            .as_ref()
            .map(|loaded| loaded.prompt_templates.clone())
            .unwrap_or_default()
    }
}
