//! Resource discovery: skills and prompt templates, loaded from
//! (upstream `resource-loader.ts` + `skills.ts` + `prompt-templates.ts`, in
//! the upstream `resourcePrecedenceRank` order with CLI paths first):
//! 1. the explicit CLI paths (`--skill` / `--prompt-template`);
//! 2. the trusted project scope: its settings `skills` / `prompts` path
//!    lists, then the auto `<cwd>/.pi/skills` / `<cwd>/.pi/prompts`
//!    directories, then the ancestor `.agents/skills` directories (skills
//!    only: `cwd` first, then outward — upstream
//!    `collectAncestorAgentsSkillDirs`); all trust-gated like the other
//!    project resources;
//! 3. the user scope: its settings path lists, then the auto
//!    `<agentDir>/skills` / `<agentDir>/prompts` directories, then the
//!    user-global `~/.agents/skills` (skills only);
//! 4. installed packages (settings.packages), last.
//!
//! Name collisions resolve first-wins in the order above (upstream
//! `mergePaths` ranks CLI paths highest) — so a same-named project/user
//! resource does NOT override the CLI path. Dropped duplicates and
//! load/parse failures are recorded as diagnostics
//! ([`DiscoveredResources::diagnostics`]) instead of being silently
//! swallowed.
//!
//! Every settings entry list is split into sources and override patterns
//! (upstream `splitPatterns`): plain entries are discovered from the scope's
//! base directory, while the `!`/`+`/`-` entries filter the discovered files
//! through [`crate::coding_agent::package_manager::TopLevelResourceFilter`]
//! (upstream `resolveLocalEntries` + `isEnabledByOverrides`, the top-level
//! settings layer). The CLI paths are explicit and never filtered, like
//! upstream's CLI sources.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use crate::coding_agent::package_manager::TopLevelResourceFilter;
use crate::coding_agent::prompt_templates::{load_templates_from_dir, PromptTemplate};
use crate::config::trust::normalize_cwd;
use crate::skills::{
    discover_skills_with_diagnostics, load_skill_from_file, SkillDiagnostic, SkillInfo,
};

/// Discovery inputs. `project_trusted` gates the `<cwd>/.pi` resource dirs
/// (upstream `settingsManager.isProjectTrusted`).
pub struct ResourceDiscoveryOptions<'a> {
    pub cwd: &'a Path,
    pub agent_dir: &'a Path,
    pub project_trusted: bool,
    /// `--skill` CLI paths (files or directories).
    pub cli_skill_paths: &'a [PathBuf],
    /// `--prompt-template` CLI paths (files or directories).
    pub cli_prompt_paths: &'a [PathBuf],
    /// `--no-skills`: keep only the CLI paths.
    pub no_skills: bool,
    /// `--no-prompt-templates`: keep only the CLI paths.
    pub no_prompt_templates: bool,
}

/// Skills and prompt templates discovered for one run, plus the load
/// diagnostics (upstream `ResourceDiagnostic`s: collision and warning
/// messages).
#[derive(Debug, Default, Clone)]
pub struct DiscoveredResources {
    pub skills: Vec<SkillInfo>,
    pub prompt_templates: Vec<PromptTemplate>,
    /// Collision and load warnings, in discovery order.
    pub diagnostics: Vec<String>,
}

/// The CLI-side resource inputs for one run: the repeatable `--skill` /
/// `--prompt-template` paths plus the `--no-skills` / `--no-prompt-templates`
/// disable flags (upstream `cliExtensionPaths` + `noSkills`/`noPromptTemplates`).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ResourcePathOptions {
    /// `--skill` CLI paths (skill files or directories).
    pub skill_paths: Vec<PathBuf>,
    /// `--prompt-template` CLI paths (template files or directories).
    pub prompt_paths: Vec<PathBuf>,
    /// `--no-skills`: keep only the CLI paths.
    pub no_skills: bool,
    /// `--no-prompt-templates`: keep only the CLI paths.
    pub no_prompt_templates: bool,
}

/// Expand a leading `~` / `~/` against `$HOME` (`$USERPROFILE` on Windows) —
/// the ambient boundary wrapper. Callers inside the discovery chain take the
/// already-resolved home explicitly (`expand_tilde_with_home`).
pub fn expand_tilde(path: &str) -> PathBuf {
    let home = home_dir();
    expand_tilde_with_home(path, home.as_deref())
}

/// [`expand_tilde`] with the home directory passed in explicitly (`None` = no
/// home available: a `~`-prefixed path is returned unchanged).
fn expand_tilde_with_home(path: &str, home: Option<&Path>) -> PathBuf {
    if path == "~" {
        if let Some(home) = home {
            return home.to_path_buf();
        }
        return PathBuf::from(path);
    }
    if let Some(rest) = path.strip_prefix("~/") {
        if let Some(home) = home {
            return home.join(rest);
        }
    }
    if let Some(rest) = path.strip_prefix("~\\") {
        if let Some(home) = home {
            return home.join(rest);
        }
    }
    PathBuf::from(path)
}

/// The process home directory (`$HOME`, then `$USERPROFILE` on Windows) — the
/// one ambient read behind this module's boundary wrappers
/// ([`DiscoveredResources::discover`], the pub [`expand_tilde`]).
fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
}

/// One scope's settings entry list for a resource kind, split into the plain
/// source entries and the `!`/`+`/`-` override patterns (upstream
/// `splitPatterns`, package-manager.ts:311-322).
#[derive(Debug, Default, Clone)]
struct ScopeEntries {
    plain: Vec<String>,
    patterns: Vec<String>,
}

impl ScopeEntries {
    /// Read one `skills` / `prompts` array out of a raw settings document.
    fn from_raw(raw: &serde_json::Value, field: &str) -> Self {
        let mut entries = Self::default();
        let Some(array) = raw.get(field).and_then(serde_json::Value::as_array) else {
            return entries;
        };
        for entry in array.iter().filter_map(serde_json::Value::as_str) {
            if entry.starts_with('!') || entry.starts_with('+') || entry.starts_with('-') {
                entries.patterns.push(entry.to_owned());
            } else {
                entries.plain.push(entry.to_owned());
            }
        }
        entries
    }

    /// The override layer this scope applies to every file discovered under
    /// it (its own plain entries AND its auto directory — upstream
    /// `addAutoDiscoveredResources` passes the same list to every auto dir).
    /// `home` is the caller's already-resolved home directory for the `~/…`
    /// patterns (`None` = no home available).
    fn filter(&self, home: Option<&Path>) -> TopLevelResourceFilter {
        TopLevelResourceFilter::from_entries_with_home(&self.patterns, home)
    }
}

/// One discovery root plus the top-level filter applied to what it yields.
struct ResourceRoot {
    path: PathBuf,
    /// The scope's override filter and the base directory its relative
    /// patterns resolve against (upstream `projectBaseDir` / `globalBaseDir`);
    /// `None` for the explicit CLI paths (upstream never filters those).
    filter: Option<(TopLevelResourceFilter, PathBuf)>,
}

impl ResourceRoot {
    /// A root outside the top-level settings scopes — the explicit CLI paths
    /// and the package-contributed resources, which the `skills` / `prompts`
    /// arrays never filter.
    fn unfiltered(path: PathBuf) -> Self {
        Self { path, filter: None }
    }
}

/// Resolve one settings entry against its scope's base directory (upstream
/// `resolvePathFromBase`): a `~` expands against `home` (the caller's
/// already-resolved home directory), an absolute path stays put, and a
/// relative path resolves against the scope base (`<cwd>/.pi` for the project
/// scope, the agent dir for the user one).
fn resolve_entry(entry: &str, base_dir: &Path, home: Option<&Path>) -> PathBuf {
    let expanded = expand_tilde_with_home(entry, home);
    if expanded.is_absolute() {
        expanded
    } else {
        base_dir.join(expanded)
    }
}

/// The `.agents`-convention skill directories for one discovery pass
/// (upstream `addAutoDiscoveredResources`): only the skills kind consumes
/// them — upstream never auto-discovers `.agents/prompts`.
#[derive(Debug, Default, Clone)]
struct AgentsSkillDirs {
    /// The project-trusted ancestor `.agents/skills` directories (`cwd`
    /// first, then outward — upstream `collectAncestorAgentsSkillDirs`), with
    /// the user-global `~/.agents/skills` filtered out like upstream. Empty
    /// when the project is untrusted (they then never load).
    project: Vec<PathBuf>,
    /// The user-global `<home>/.agents/skills` (`None` = no home available).
    user: Option<PathBuf>,
}

impl AgentsSkillDirs {
    /// Resolve the `.agents/skills` roots for one discovery pass. The
    /// ancestors are only collected when the project is trusted (upstream
    /// gates the whole list on `isProjectTrusted`); the user-global dir is
    /// never trust-gated.
    fn resolve(cwd: &Path, project_trusted: bool, home: Option<&Path>) -> Self {
        let user = home.map(|home| home.join(".agents").join("skills"));
        let project = if project_trusted {
            collect_ancestor_agents_skill_dirs(cwd)
                .into_iter()
                .filter(|dir| {
                    user.as_ref()
                        .is_none_or(|user| normalize_cwd(dir) != normalize_cwd(user))
                })
                .collect()
        } else {
            Vec::new()
        };
        Self { project, user }
    }
}

/// The base directory an `.agents/skills` root's override patterns resolve
/// against: the `.agents` dir itself (upstream `agentsBaseDir = dirname(dir)`,
/// not the project settings base).
fn agents_scope_base(skills_dir: &Path) -> PathBuf {
    skills_dir
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| skills_dir.to_path_buf())
}

/// The `.agents/skills` directories to consider at and above `cwd` (upstream
/// `collectAncestorAgentsSkillDirs`): `cwd` first, then each ancestor up to and
/// including the enclosing git repository root. Without a `.git` entry the
/// walk continues to the filesystem root (upstream keeps pushing there too;
/// the directory simply yields nothing unless it exists).
pub fn collect_ancestor_agents_skill_dirs(cwd: &Path) -> Vec<PathBuf> {
    let start = std::path::absolute(cwd).unwrap_or_else(|_| cwd.to_path_buf());
    let git_root = find_git_repo_root(&start);
    let mut dirs = Vec::new();
    let mut dir = start;
    loop {
        dirs.push(dir.join(".agents").join("skills"));
        if git_root.as_deref() == Some(dir.as_path()) {
            break;
        }
        let Some(parent) = dir.parent() else { break };
        if parent == dir {
            break;
        }
        dir = parent.to_path_buf();
    }
    dirs
}

/// The nearest ancestor of `dir` (inclusive) holding a `.git` entry (upstream
/// `findGitRepoRoot`; a `.git` file counts, as in linked worktrees).
fn find_git_repo_root(dir: &Path) -> Option<PathBuf> {
    let mut current = dir.to_path_buf();
    loop {
        if current.join(".git").exists() {
            return Some(current);
        }
        let parent = current.parent()?;
        if parent == current {
            return None;
        }
        current = parent.to_path_buf();
    }
}

/// Ordered, deduplicated directory/file search roots for one resource kind,
/// in the upstream precedence order (upstream `resourcePrecedenceRank` +
/// `mergePaths`): the CLI paths FIRST, then the project scope (settings
/// paths, then the auto `<cwd>/.pi/<kind>` dir, then — skills only — the
/// ancestor `.agents/skills` dirs), then the user scope (settings paths, then
/// the auto `<agent_dir>/<kind>` dir, then — skills only — the user-global
/// `~/.agents/skills`). Same-named later resources lose to earlier ones
/// (first-wins in the loaders).
///
/// Each non-CLI root carries its scope's top-level override filter, so a
/// `!pattern` / `+path` / `-path` entry decides which of that scope's
/// discovered files survive (upstream `resolveLocalEntries` for the listed
/// paths, `isEnabledByOverrides` for the auto directories). Every
/// `.agents/skills` root filters against its own `.agents` base
/// ([`agents_scope_base`], upstream `agentsBaseDir`).
///
/// `home` is the caller's already-resolved home directory: every `~`-prefixed
/// CLI path, plain entry and override pattern expands against it (`None` = no
/// home available), so no step of this chain reads the process environment.
// The eight parameters are this call's inputs in upstream order; bundling
// them into a struct would only rename the single call site.
#[allow(clippy::too_many_arguments)]
fn resource_roots(
    options: &ResourceDiscoveryOptions<'_>,
    project: &ScopeEntries,
    user: &ScopeEntries,
    default_dir: &str,
    cli_paths: &[PathBuf],
    include_defaults: bool,
    agents: &AgentsSkillDirs,
    home: Option<&Path>,
) -> Vec<ResourceRoot> {
    let mut roots: Vec<ResourceRoot> = Vec::new();
    let mut seen: BTreeSet<PathBuf> = BTreeSet::new();
    let push = |path: PathBuf,
                filter: Option<(TopLevelResourceFilter, PathBuf)>,
                roots: &mut Vec<ResourceRoot>,
                seen: &mut BTreeSet<PathBuf>| {
        if seen.insert(path.clone()) {
            roots.push(ResourceRoot { path, filter });
        }
    };
    for cli_path in cli_paths {
        push(
            expand_tilde_with_home(&cli_path.to_string_lossy(), home),
            None,
            &mut roots,
            &mut seen,
        );
    }
    if include_defaults {
        let project_base = options.cwd.join(".pi");
        let project_filter = project.filter(home);
        let project_layer = Some((project_filter.clone(), project_base.clone()));
        // Project scope (settings paths, then its auto dirs), trust-gated.
        if options.project_trusted {
            for entry in &project.plain {
                push(
                    resolve_entry(entry, &project_base, home),
                    project_layer.clone(),
                    &mut roots,
                    &mut seen,
                );
            }
            push(
                project_base.join(default_dir),
                project_layer,
                &mut roots,
                &mut seen,
            );
            // The ancestor `.agents/skills` dirs rank with the project auto
            // dir: the `.pi` dir first, then the ancestors from `cwd` outward
            // (upstream registers them in that order and the stable
            // precedence sort keeps it).
            for dir in &agents.project {
                push(
                    dir.clone(),
                    Some((project_filter.clone(), agents_scope_base(dir))),
                    &mut roots,
                    &mut seen,
                );
            }
        }
        // User scope (settings paths, then its auto dirs).
        let user_base = options.agent_dir.to_path_buf();
        let user_filter = user.filter(home);
        let user_layer = Some((user_filter.clone(), user_base.clone()));
        for entry in &user.plain {
            push(
                resolve_entry(entry, &user_base, home),
                user_layer.clone(),
                &mut roots,
                &mut seen,
            );
        }
        push(
            user_base.join(default_dir),
            user_layer,
            &mut roots,
            &mut seen,
        );
        // The user-global `~/.agents/skills` ranks after `<agent_dir>/skills`
        // (upstream registers it after the auto user dir).
        if let Some(dir) = &agents.user {
            push(
                dir.clone(),
                Some((user_filter, agents_scope_base(dir))),
                &mut roots,
                &mut seen,
            );
        }
    }
    roots
}

/// `true` when the root's top-level override patterns keep this file
/// (unfiltered roots keep everything).
fn root_keeps(root: &ResourceRoot, path: &Path) -> bool {
    match &root.filter {
        Some((filter, base_dir)) => filter.keeps(path, base_dir),
        None => true,
    }
}

/// Render one skill-discovery diagnostic in the same shape as this module's
/// own diagnostics: `warning: {message} ({path})` for load/validation
/// problems, and the collision shape carrying both files for a same-name
/// duplicate (upstream `ResourceDiagnostic` → `formatDiagnostic`).
fn format_skill_diagnostic(diagnostic: &SkillDiagnostic) -> String {
    match diagnostic.winner_path.as_deref() {
        // `SkillDiagnostic::collision`'s message is already
        // `name "x" collision`, so this matches this module's own
        // collision-diagnostic shape exactly.
        Some(winner) => format!(
            "{} (kept {}, ignored {})",
            diagnostic.message,
            winner.display(),
            diagnostic.path.display()
        ),
        None => format!(
            "warning: {} ({})",
            diagnostic.message,
            diagnostic.path.display()
        ),
    }
}

/// Discover skills across the configured roots. A root may be a skill
/// directory (recursed like upstream `loadSkillsFromDir`) or a single skill
/// file (upstream accepts both); the root's top-level filter drops the files
/// its scope's `!`/`+`/`-` entries reject. Dedupe by name, first wins;
/// collisions and load failures are recorded as diagnostics (upstream
/// skills.ts:437-449). Per-root diagnostics (name/description spec
/// violations, malformed frontmatter, in-directory collisions) come from the
/// structured `discover_skills_with_diagnostics` surface instead of the
/// logging-only [`crate::skills::discover_skills`] wrapper, so they reach
/// [`DiscoveredResources::diagnostics`] too.
fn discover_skills_from_roots(
    roots: &[ResourceRoot],
    diagnostics: &mut Vec<String>,
) -> Vec<SkillInfo> {
    let mut skills: Vec<SkillInfo> = Vec::new();
    let mut seen = BTreeSet::new();
    for root in roots {
        let path = &root.path;
        let loaded: Vec<SkillInfo> = if path.is_file() {
            match load_skill_from_file(path) {
                Ok(skill) => vec![skill],
                Err(error) => {
                    diagnostics.push(format!("warning: {error} ({})", path.display()));
                    Vec::new()
                }
            }
        } else {
            let (loaded, skill_diagnostics) =
                discover_skills_with_diagnostics(std::slice::from_ref(path));
            diagnostics.extend(skill_diagnostics.iter().map(format_skill_diagnostic));
            loaded
        };
        for skill in loaded {
            if !root_keeps(root, &skill.path) {
                continue;
            }
            if seen.insert(skill.name.clone()) {
                skills.push(skill);
            } else if let Some(existing) = skills.iter().find(|s| s.name == skill.name) {
                diagnostics.push(format!(
                    "name \"{}\" collision (kept {}, ignored {})",
                    skill.name,
                    existing.path.display(),
                    skill.path.display()
                ));
            }
        }
    }
    skills
}

/// Discover prompt templates across the configured roots (directories are
/// scanned non-recursively; single `.md` files load directly), applying each
/// root's top-level filter. Templates dedupe by command name, first wins;
/// collisions and load/parse warnings are recorded as diagnostics (upstream
/// prompt-templates.ts:121-127 rejects files whose frontmatter fails to
/// parse, with a warning).
fn discover_templates_from_roots(
    roots: &[ResourceRoot],
    diagnostics: &mut Vec<String>,
) -> Vec<PromptTemplate> {
    let mut templates: Vec<PromptTemplate> = Vec::new();
    let mut seen = BTreeSet::new();
    for root in roots {
        let path = &root.path;
        if path.is_file() {
            match crate::coding_agent::prompt_templates::load_template_file(path) {
                Ok(template) => {
                    if root_keeps(root, &template.file_path) {
                        record_template(template, &mut templates, &mut seen, diagnostics);
                    }
                }
                Err(error) => diagnostics.push(format!("warning: {error}")),
            }
        } else {
            let (loaded, warnings) = load_templates_from_dir(path);
            diagnostics.extend(warnings);
            for template in loaded {
                if root_keeps(root, &template.file_path) {
                    record_template(template, &mut templates, &mut seen, diagnostics);
                }
            }
        }
    }
    templates
}

/// First-wins template registration with a collision diagnostic (upstream
/// skills.ts:437-449's collision shape, applied to prompt templates).
fn record_template(
    template: PromptTemplate,
    templates: &mut Vec<PromptTemplate>,
    seen: &mut BTreeSet<String>,
    diagnostics: &mut Vec<String>,
) {
    if seen.insert(template.name.clone()) {
        templates.push(template);
    } else if let Some(existing) = templates.iter().find(|t| t.name == template.name) {
        diagnostics.push(format!(
            "name \"{}\" collision (kept {}, ignored {})",
            template.name,
            existing.file_path.display(),
            template.file_path.display()
        ));
    }
}

/// The per-scope settings entries for one resource kind (the `skills` /
/// `prompts` settings arrays): `(project, user)`, each split into its plain
/// source entries and its override patterns. The project list is empty when
/// the project scope is untrusted (the manager then returns `{}` for the
/// project raw scope).
fn settings_entries_for(
    options: &ResourceDiscoveryOptions<'_>,
    field: &str,
) -> (ScopeEntries, ScopeEntries) {
    let manager = crate::config::settings::SettingsManager::load(
        options.agent_dir,
        &options.cwd.join(".pi"),
        options.cwd,
        options.project_trusted,
    );
    (
        ScopeEntries::from_raw(&manager.project_raw, field),
        ScopeEntries::from_raw(&manager.global_raw, field),
    )
}

impl DiscoveredResources {
    /// Discover skills + prompt templates for a run (see the module docs for
    /// the source order).
    ///
    /// Boundary wrapper: resolves the ambient home directory exactly once
    /// here, for the `~` expansion of the CLI paths, the plain settings
    /// entries and the override patterns. Callers with a home in hand use
    /// [`Self::discover_with_home`] instead (hermetic: no ambient read).
    pub fn discover(options: &ResourceDiscoveryOptions<'_>) -> Self {
        let home = home_dir();
        Self::discover_with_home(options, home.as_deref())
    }

    /// [`Self::discover`] with the home directory the `~`-prefixed CLI paths,
    /// settings entries and override patterns resolve against passed in
    /// explicitly (`None` = no home available).
    pub fn discover_with_home(options: &ResourceDiscoveryOptions<'_>, home: Option<&Path>) -> Self {
        let mut diagnostics: Vec<String> = Vec::new();
        // Installed packages (settings.packages, upstream resource-loader.ts
        // `resolve()`) contribute skills/prompts ranked between the
        // settings/auto paths and the CLI paths. The ambient settings load
        // mirrors `agent_session::runtime_loader::load_settings_for`.
        let package_resources = {
            let settings = crate::config::settings::SettingsManager::load(
                options.agent_dir,
                &options.cwd.join(".pi"),
                options.cwd,
                options.project_trusted,
            );
            crate::coding_agent::package_manager::resolve_package_resources_with_home(
                &settings,
                options.agent_dir,
                options.cwd,
                options.project_trusted,
                home,
            )
        };
        let skills = if options.no_skills && options.cli_skill_paths.is_empty() {
            Vec::new()
        } else {
            let (project, user) = settings_entries_for(options, "skills");
            let agents = AgentsSkillDirs::resolve(options.cwd, options.project_trusted, home);
            let mut roots = resource_roots(
                options,
                &project,
                &user,
                "skills",
                options.cli_skill_paths,
                !options.no_skills,
                &agents,
                home,
            );
            roots.extend(
                package_resources
                    .skills
                    .iter()
                    .cloned()
                    .map(ResourceRoot::unfiltered),
            );
            discover_skills_from_roots(&roots, &mut diagnostics)
        };
        let prompt_templates = if options.no_prompt_templates && options.cli_prompt_paths.is_empty()
        {
            Vec::new()
        } else {
            let (project, user) = settings_entries_for(options, "prompts");
            let mut roots = resource_roots(
                options,
                &project,
                &user,
                "prompts",
                options.cli_prompt_paths,
                !options.no_prompt_templates,
                &AgentsSkillDirs::default(),
                home,
            );
            roots.extend(
                package_resources
                    .prompts
                    .iter()
                    .cloned()
                    .map(ResourceRoot::unfiltered),
            );
            discover_templates_from_roots(&roots, &mut diagnostics)
        };
        Self {
            skills,
            prompt_templates,
            diagnostics,
        }
    }

    /// The `/skill:<name>` slash commands (upstream `skillCommands`), built
    /// only when `enable_skill_commands` is set (default true).
    pub fn skill_commands(&self, enable_skill_commands: bool) -> Vec<(String, String)> {
        if !enable_skill_commands {
            return Vec::new();
        }
        self.skills
            .iter()
            .map(|skill| (format!("skill:{}", skill.name), skill.description.clone()))
            .collect()
    }

    /// The `/template` slash commands (upstream `templateCommands`).
    pub fn template_commands(&self) -> Vec<(String, String, Option<String>)> {
        self.prompt_templates
            .iter()
            .map(|template| {
                (
                    template.name.clone(),
                    template.description.clone(),
                    template.argument_hint.clone(),
                )
            })
            .collect()
    }
}

/// Expand `/skill:name args` to the skill content block (upstream
/// `AgentSession._expandSkillCommand`): the file body (frontmatter stripped)
/// wrapped in a `<skill>` element; trailing args are appended after a blank
/// line. Unknown skill or unreadable file: the original text is returned.
pub fn expand_skill_command(text: &str, skills: &[SkillInfo]) -> String {
    if !text.starts_with("/skill:") {
        return text.to_string();
    }
    let body = &text["/skill:".len()..];
    let (skill_name, args) = match body.find(' ') {
        Some(space_index) => (&body[..space_index], body[space_index + 1..].trim()),
        None => (body, ""),
    };
    let Some(skill) = skills.iter().find(|skill| skill.name == skill_name) else {
        return text.to_string();
    };
    let Ok(content) = std::fs::read_to_string(&skill.path) else {
        return text.to_string();
    };
    let base_dir = skill
        .path
        .parent()
        .map(|dir| dir.to_string_lossy().into_owned())
        .unwrap_or_default();
    let body = crate::skills::strip_frontmatter(content.trim_start_matches('\u{feff}')).trim();
    let skill_block = format!(
        "<skill name=\"{}\" location=\"{}\">\nReferences are relative to {}.\n\n{}\n</skill>",
        skill.name,
        skill.path.display(),
        base_dir,
        body
    );
    if args.is_empty() {
        skill_block
    } else {
        format!("{skill_block}\n\n{args}")
    }
}

/// The full prompt-command expansion applied to every submitted prompt
/// (upstream `prompt()` / `_queueUserInput`): `/skill:name` first, then
/// prompt templates.
pub fn expand_prompt_commands(
    text: &str,
    skills: &[SkillInfo],
    templates: &[PromptTemplate],
) -> String {
    let expanded = expand_skill_command(text, skills);
    crate::coding_agent::prompt_templates::expand_prompt_template(&expanded, templates)
}
