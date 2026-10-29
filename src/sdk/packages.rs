//! [`DefaultPackageManager`]: the resource side of installed packages
//! (upstream `DefaultPackageManager.resolve()`, `core/package-manager.ts`).
//!
//! pir's package installation/updating COMMANDS stay in the CLI; this facade
//! exposes the resolver an embedder needs: which extensions, skills, prompt
//! templates and theme files the `settings.packages` entries contribute.
//! Resolution is not a pure read: like upstream's `resolve()`, a missing
//! npm/git package is installed (and an npm package whose installed version
//! no longer satisfies the configured version/range is reinstalled) before its
//! resources are read. The resolution rules (project-scope wins, dedupe by
//! source identity, manifest vs convention directories) and the startup
//! install live in [`crate::coding_agent::package_manager`]; this type only
//! binds the two directories and the trust state.

use std::path::{Path, PathBuf};

use crate::coding_agent::package_manager::{resolve_package_resources, PackageResources};
use crate::config::settings::SettingsManager;

/// Resolve `settings.packages` into installed resource paths for one agent
/// directory / working directory pair.
#[derive(Debug, Clone)]
pub struct DefaultPackageManager {
    agent_dir: PathBuf,
    cwd: PathBuf,
    project_trusted: bool,
}

impl DefaultPackageManager {
    /// Create a manager with the project trust state resolved from the
    /// process environment.
    pub fn new(agent_dir: impl Into<PathBuf>, cwd: impl Into<PathBuf>) -> Self {
        let agent_dir = agent_dir.into();
        let cwd = cwd.into();
        let project_trusted = crate::config::trust::resolve_cwd_trust(&cwd, &agent_dir);
        Self {
            agent_dir,
            cwd,
            project_trusted,
        }
    }

    /// Force the project scope's trust decision.
    pub fn with_project_trusted(mut self, trusted: bool) -> Self {
        self.project_trusted = trusted;
        self
    }

    pub fn agent_dir(&self) -> &Path {
        &self.agent_dir
    }

    pub fn cwd(&self) -> &Path {
        &self.cwd
    }

    pub fn project_trusted(&self) -> bool {
        self.project_trusted
    }

    /// Resource paths contributed by the installed packages of `settings`
    /// (upstream `resolve()` + `collectPackageResources`).
    ///
    /// Not read-only: like upstream `DefaultPackageManager.resolve()`, a
    /// missing npm/git package is installed before its resources are read, and
    /// an npm package whose installed version no longer satisfies the
    /// configured version/range is reinstalled. Offline mode and failed
    /// installs skip that package instead of failing the caller (the install
    /// work lives in [`crate::coding_agent::package_manager`]).
    pub fn resolve(&self, settings: &SettingsManager) -> PackageResources {
        resolve_package_resources(settings, &self.agent_dir, &self.cwd, self.project_trusted)
    }
}
