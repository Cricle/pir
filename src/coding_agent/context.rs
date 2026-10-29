//! Context-file (`AGENTS.md` / `CLAUDE.md`) discovery and system-prompt
//! injection, mirroring upstream `packages/coding-agent/src/core/resource-loader.ts`
//! (`loadProjectContextFiles`) and the `--no-context-files` CLI flag
//! (`packages/coding-agent/src/cli/args.ts`).
//!
//! Discovery rules (upstream `loadContextFileFromDir` / `loadProjectContextFiles`):
//! - candidate filenames per directory, first existing wins:
//!   `AGENTS.override.md`, `AGENTS.md`, `AGENTS.MD`, `CLAUDE.md`, `CLAUDE.MD`;
//! - the global agent dir's context file is loaded first, then every ancestor
//!   of cwd from root-most down to cwd itself (deduplicated, BOM stripped);
//! - a linked worktree's own copy shadows the main repo's file at the same
//!   logical scope (implemented in [`crate::config::settings`]).
//!
//! pir hardening: a context file larger than [`MAX_CONTEXT_FILE_BYTES`] is
//! skipped (upstream reads without a cap).
//!
//! Ambient policy (the boundary shape of
//! [`crate::coding_agent::package_manager::TopLevelResourceFilter::from_entries`]):
//! every process-environment read — the [`NO_CONTEXT_FILES_ENV`] flag,
//! [`agent_dir`], `PI_PACKAGE_DIR` — lives in the private
//! `with_process_context_options` helper, which the documented BOUNDARY
//! WRAPPERS ([`resolve_context_files`], [`context_system_prompt`], ...) call
//! exactly once and delegate to the explicit `*_with` variants taking
//! [`ContextOptions`]. The cores never read the process environment, so a
//! caller (a test, an embedder) can drive discovery hermetically.

use std::path::{Path, PathBuf};

use crate::system_prompt::{build_system_prompt, SystemPromptInput, ToolInfo};

/// Set to `1`/`true` (or via `--no-context-files`) to disable discovery.
pub const NO_CONTEXT_FILES_ENV: &str = "PIR_NO_CONTEXT_FILES";

/// Upper bound for one context file (pir hardening; upstream is unbounded).
pub const MAX_CONTEXT_FILE_BYTES: u64 = 1024 * 1024;

/// Whether context-file discovery is enabled for this process.
pub fn context_files_enabled() -> bool {
    match std::env::var(NO_CONTEXT_FILES_ENV) {
        Ok(value) => !matches!(value.trim(), "1" | "true"),
        Err(_) => true,
    }
}

/// The global agent directory: the shared agent-dir resolution
/// (`PIR_AGENT_DIR` / `PIR_CODING_AGENT_DIR` / `PI_CODING_AGENT_DIR`, else
/// `~/.pi/agent`; `packages/coding-agent/src/config.ts`
/// `getDefaultAgentDir`).
pub fn agent_dir() -> PathBuf {
    crate::ai::providers::agent_dir()
}

/// The resolved ambient inputs of context-file and prompt-skill discovery.
///
/// The `*_with` cores take this struct and never read the process
/// environment; the boundary wrappers ([`resolve_context_files`],
/// [`context_system_prompt`], [`context_system_prompt_with_hidden`],
/// [`context_system_prompt_with_skills`], [`discover_prompt_skills`]) build it
/// from the process values in the private `with_process_context_options`
/// helper exactly once per call and delegate.
#[derive(Debug, Clone, Copy)]
pub struct ContextOptions<'a> {
    /// `--no-context-files` / `PIR_NO_CONTEXT_FILES=1|true`: discovery of
    /// context files is disabled (skills discovery is not gated by this flag).
    pub disabled: bool,
    /// The global agent directory (`PIR_AGENT_DIR` ... else `~/.pi/agent`):
    /// the global context file's directory, the `<agentDir>/skills` root, and
    /// the global-settings / trust-store anchor of the trust lookup that gates
    /// the project skill dirs.
    pub agent_dir: &'a Path,
    /// `PI_PACKAGE_DIR` (upstream `getPackageDir`): the package docs anchors of
    /// the default prompt; `None` = not installed as a package.
    pub package_dir: Option<&'a Path>,
}

/// Resolve the ambient inputs of context discovery exactly once and run `f`
/// with them. This is the module's only process-environment read: every
/// `*_with` core takes the resolved [`ContextOptions`] and never reads the
/// process environment.
fn with_process_context_options<R>(f: impl FnOnce(&ContextOptions<'_>) -> R) -> R {
    let agent_dir = agent_dir();
    let package_dir = crate::config::env::package_dir();
    f(&ContextOptions {
        disabled: !context_files_enabled(),
        agent_dir: &agent_dir,
        package_dir: package_dir.as_deref(),
    })
}

/// Discover context files for `cwd`, or an empty list when discovery is
/// disabled (`--no-context-files` / `PIR_NO_CONTEXT_FILES=1`).
///
/// Boundary wrapper: resolves the ambient flag and agent dir exactly once and
/// delegates to [`resolve_context_files_with`] (hermetic: no ambient read).
pub fn resolve_context_files(cwd: &Path) -> Vec<(PathBuf, String)> {
    with_process_context_options(|options| resolve_context_files_with(cwd, options))
}

/// [`resolve_context_files`] with the resolved [`ContextOptions`] passed in
/// explicitly.
pub fn resolve_context_files_with(
    cwd: &Path,
    options: &ContextOptions<'_>,
) -> Vec<(PathBuf, String)> {
    if options.disabled {
        return Vec::new();
    }
    crate::config::settings::discover_context_files(cwd, options.agent_dir)
}

/// Build a system prompt from the ordered section builder with the discovered
/// context files as the `project_context` section.
///
/// `hidden_tools` are active tools whose declarations requests leave out
/// (upstream `hiddenTools`); the tool list and rules omit them.
///
/// Boundary wrapper: delegates to [`context_system_prompt_with_hidden`], which
/// resolves the ambient options exactly once (`with_process_context_options`).
///
/// Returns `None` when there is nothing to inject — discovery disabled or no
/// context files AND no prompt skills — the caller then keeps its previous
/// behavior (typically no system prompt). When files or skills exist, the full
/// default structured prompt (preamble, tools, rules, `project_context`, cwd)
/// is produced.
pub fn context_system_prompt(cwd: &Path, tools: &[ToolInfo]) -> Option<String> {
    context_system_prompt_with_hidden(cwd, tools, &[])
}

/// [`context_system_prompt`] with the resolved [`ContextOptions`] passed in
/// explicitly (hermetic: no ambient read); prompt skills are discovered with
/// the same options. No hidden declarations (see
/// [`context_system_prompt_with_hidden_and_options`]).
pub fn context_system_prompt_with(
    cwd: &Path,
    tools: &[ToolInfo],
    options: &ContextOptions<'_>,
) -> Option<String> {
    context_system_prompt_with_hidden_and_options(cwd, tools, &[], options)
}

/// [`context_system_prompt_with`] with the hidden-declaration list (upstream
/// `BuildSystemPromptOptions.hiddenTools`) while the caller keeps control of
/// the ambient inputs.
///
/// This is the pair the SESSION build needs: it resolves the agent directory
/// once at construction (D1) and must not re-read the process environment,
/// yet its `prepareLoadout` hidden declarations (upstream
/// `_rebuildSystemPrompt`'s `hiddenTools`, agent-session.ts:1727) have to
/// leave the `<tools>`/`<rules>` sections.
pub fn context_system_prompt_with_hidden_and_options(
    cwd: &Path,
    tools: &[ToolInfo],
    hidden_tools: &[String],
    options: &ContextOptions<'_>,
) -> Option<String> {
    context_system_prompt_with_hidden_and_options_and_inputs(cwd, tools, hidden_tools, options).0
}

/// The discovered inputs of the default structured prompt (context files and
/// prompt skills), returned alongside the prompt so a caller that later
/// rebuilds only the tool-dependent sections can reuse them.
///
/// The session facade needs exactly this: upstream re-runs
/// `_rebuildSystemPrompt` on every active-set change
/// (`agent-session.ts:1535`, `:3335`) from the loadout's already-loaded
/// context files and skills, never from a fresh disk read. Caching the inputs
/// here keeps a steady turn IO-free; only an actual active-set change calls
/// [`context_system_prompt_from_inputs`].
#[derive(Debug, Clone, Default)]
pub struct PromptContextInputs {
    /// Context files as discovered by [`resolve_context_files_with`].
    pub files: Vec<(PathBuf, String)>,
    /// Prompt skills the builder rendered.
    pub skills: Vec<crate::skills::SkillInfo>,
    /// The `mcp_servers` section rendered from the MCP manager's synchronous
    /// status snapshot at build/reload time (`None` when MCP is off or has no
    /// enabled servers). Cached like the files and skills: a rebuild from
    /// these inputs re-renders the same snapshot, and a status transition
    /// lands on the next build/reload, never mid-prompt.
    pub mcp_servers: Option<String>,
}

/// The `mcp_servers` prompt section for a plugin host, or `None` when the
/// host has no `mcp` builtin (feature off / not on the plan) or MCP has no
/// enabled server.
///
/// The section is read from the plugin's SYNCHRONOUS status snapshot (the
/// manager's state mirror): no disk or network IO, so a session build/reload
/// and print's per-prompt cache can call it per prompt.
pub fn mcp_servers_prompt_section(plugins: Option<&crate::plugins::PluginHost>) -> Option<String> {
    #[cfg(feature = "mcp")]
    {
        let plugins = plugins?;
        let plugin = plugins.plugins().iter().find_map(|plugin| {
            plugin
                .as_any()?
                .downcast_ref::<crate::plugins::builtin::McpPlugin>()
        })?;
        plugin.prompt_section()
    }
    #[cfg(not(feature = "mcp"))]
    {
        let _ = plugins;
        None
    }
}

/// The `mcp_servers` custom section as the builder's `custom_sections` input:
/// empty when there is no section, so an MCP-less prompt keeps exactly the
/// sections and order it had before the section existed. Public for the print
/// host's per-prompt renderer.
pub fn mcp_custom_sections(mcp_servers: Option<&str>) -> Vec<(String, String)> {
    match mcp_servers.filter(|section| !section.is_empty()) {
        Some(section) => vec![(
            crate::system_prompt::MCP_SERVERS_SECTION.to_owned(),
            section.to_owned(),
        )],
        None => Vec::new(),
    }
}

/// [`context_system_prompt_with_hidden_and_options`] plus the discovered inputs
/// it rendered from, so the caller can rebuild the prompt later without
/// repeating context-file/skill discovery.
pub fn context_system_prompt_with_hidden_and_options_and_inputs(
    cwd: &Path,
    tools: &[ToolInfo],
    hidden_tools: &[String],
    options: &ContextOptions<'_>,
) -> (Option<String>, PromptContextInputs) {
    let files = resolve_context_files_with(cwd, options);
    let skills = discover_prompt_skills_with(cwd, options);
    let prompt = context_system_prompt_from_parts(
        cwd,
        tools,
        hidden_tools,
        &files,
        &skills,
        options.package_dir,
        None,
    );
    (
        prompt,
        PromptContextInputs {
            files,
            skills,
            mcp_servers: None,
        },
    )
}

/// [`context_system_prompt_with_hidden_and_options_and_inputs`] for the SESSION
/// hosts: ALWAYS returns the prompt (P7), plus the discovered inputs.
///
/// Upstream builds the base prompt unconditionally
/// (`system-prompt.ts:60-199`); the "nothing to inject → `None`" rule only
/// exists for callers that append to a caller-supplied prompt (the subagent
/// child) or build their own (print). A session on a bare workspace used to
/// send no system prompt at all — no preamble, no tool list, no rules, no
/// `<cwd>`.
pub fn session_system_prompt_with_hidden_and_options_and_inputs(
    cwd: &Path,
    tools: &[ToolInfo],
    hidden_tools: &[String],
    options: &ContextOptions<'_>,
) -> (String, PromptContextInputs) {
    let files = resolve_context_files_with(cwd, options);
    let skills = discover_prompt_skills_with(cwd, options);
    let prompt = context_system_prompt_unconditional(
        cwd,
        tools,
        hidden_tools,
        &files,
        &skills,
        options.package_dir,
        None,
    );
    (
        prompt,
        PromptContextInputs {
            files,
            skills,
            mcp_servers: None,
        },
    )
}

/// [`session_system_prompt_with_hidden_and_options_and_inputs`] with the MCP
/// manager's `mcp_servers` section (see [`mcp_servers_prompt_section`])
/// rendered into the prompt and cached in the returned inputs.
///
/// This is the SESSION build path: the section is computed once from the
/// synchronous status snapshot and carried in [`PromptContextInputs`], so a
/// later `/reload` or active-set rebuild re-renders the SAME snapshot unless
/// the caller hands a fresh one.
pub fn session_system_prompt_with_hidden_and_options_and_inputs_and_mcp(
    cwd: &Path,
    tools: &[ToolInfo],
    hidden_tools: &[String],
    options: &ContextOptions<'_>,
    mcp_servers: Option<String>,
) -> (String, PromptContextInputs) {
    let files = resolve_context_files_with(cwd, options);
    let skills = discover_prompt_skills_with(cwd, options);
    let prompt = context_system_prompt_unconditional(
        cwd,
        tools,
        hidden_tools,
        &files,
        &skills,
        options.package_dir,
        mcp_servers.as_deref(),
    );
    (
        prompt,
        PromptContextInputs {
            files,
            skills,
            mcp_servers,
        },
    )
}

/// [`session_system_prompt_with_hidden_and_options_and_inputs_and_mcp`] with
/// the CLI prompt overrides: `--system-prompt` replaces the default preamble
/// while every structured section is still built, and each
/// `--append-system-prompt` entry becomes an `addendum` section (upstream
/// `buildSystemPrompt`'s `systemPrompt`/`appendSystemPrompt` options).
pub fn session_system_prompt_with_hidden_and_options_and_inputs_and_mcp_with_overrides(
    cwd: &Path,
    tools: &[ToolInfo],
    hidden_tools: &[String],
    options: &ContextOptions<'_>,
    mcp_servers: Option<String>,
    preamble_override: Option<&str>,
    append: &[String],
) -> (String, PromptContextInputs) {
    let files = resolve_context_files_with(cwd, options);
    let skills = discover_prompt_skills_with(cwd, options);
    let prompt = context_system_prompt_unconditional_with(
        cwd,
        tools,
        hidden_tools,
        &files,
        &skills,
        options.package_dir,
        mcp_servers.as_deref(),
        preamble_override,
        append,
    );
    (
        prompt,
        PromptContextInputs {
            files,
            skills,
            mcp_servers,
        },
    )
}

/// [`context_system_prompt`] with the hidden-declaration list (upstream
/// `BuildSystemPromptOptions.hiddenTools`).
///
/// Boundary wrapper: resolves the ambient options exactly once and delegates
/// to the private `build_context_prompt` core.
pub fn context_system_prompt_with_hidden(
    cwd: &Path,
    tools: &[ToolInfo],
    hidden_tools: &[String],
) -> Option<String> {
    // Skills join the system prompt as the `skills` section (upstream
    // `formatSkillsForPrompt`); project skill dirs are trust-gated.
    with_process_context_options(|options| {
        let skills = discover_prompt_skills_with(cwd, options);
        build_context_prompt(cwd, tools, hidden_tools, &skills, options)
    })
}

/// [`context_system_prompt_with_hidden`] against an EXPLICIT agent directory
/// (the enclosing session's resolved directory, D1).
///
/// Child runs (`subagent`) must not re-read the process environment: their
/// context files, prompt skills and trust lookup all resolve against the
/// parent session's agent directory, so an SDK `.agent_dir(..)` agent's
/// children cannot silently switch to `~/.pi/agent` mid-process.
#[cfg(feature = "subagent")]
pub(crate) fn context_system_prompt_with_hidden_in(
    cwd: &Path,
    tools: &[ToolInfo],
    hidden_tools: &[String],
    agent_dir: &Path,
) -> Option<String> {
    let package_dir = crate::config::env::package_dir();
    let options = ContextOptions {
        disabled: !context_files_enabled(),
        agent_dir,
        package_dir: package_dir.as_deref(),
    };
    let skills = discover_prompt_skills_with(cwd, &options);
    build_context_prompt(cwd, tools, hidden_tools, &skills, &options)
}

/// Build the structured system prompt from an already-discovered skill list.
/// A host that installed the run's resources on the session
/// ([`crate::agent_session::AgentSession::set_prompt_resources`]) passes the
/// same list here so the `skills` section matches the `/skill:<name>`
/// commands — including the `--skill` CLI paths.
///
/// Boundary wrapper: resolves the ambient options exactly once and delegates
/// to the private `build_context_prompt` core.
pub fn context_system_prompt_with_skills(
    cwd: &Path,
    tools: &[ToolInfo],
    hidden_tools: &[String],
    skills: &[crate::skills::SkillInfo],
) -> Option<String> {
    with_process_context_options(|options| {
        context_system_prompt_with_skills_and_options(cwd, tools, hidden_tools, skills, options)
    })
}

/// [`context_system_prompt_with_skills`] with the resolved [`ContextOptions`]
/// passed in explicitly (hermetic: no ambient read of the agent dir or the
/// package dir). This is the `/reload` pair: the session re-reads its context
/// files but keeps its BUILD-TIME agent directory and package directory (D1 —
/// a mid-process `PIR_AGENT_DIR`/`PI_PACKAGE_DIR` change must not re-point a
/// live session).
pub fn context_system_prompt_with_skills_and_options(
    cwd: &Path,
    tools: &[ToolInfo],
    hidden_tools: &[String],
    skills: &[crate::skills::SkillInfo],
    options: &ContextOptions<'_>,
) -> Option<String> {
    context_system_prompt_with_skills_and_options_and_inputs(
        cwd,
        tools,
        hidden_tools,
        skills,
        options,
    )
    .0
}

/// [`context_system_prompt_with_skills_and_options`] plus the context files it
/// discovered, for a caller that rebuilds on later active-set changes (D6).
pub fn context_system_prompt_with_skills_and_options_and_inputs(
    cwd: &Path,
    tools: &[ToolInfo],
    hidden_tools: &[String],
    skills: &[crate::skills::SkillInfo],
    options: &ContextOptions<'_>,
) -> (Option<String>, PromptContextInputs) {
    let files = resolve_context_files_with(cwd, options);
    let prompt = context_system_prompt_from_parts(
        cwd,
        tools,
        hidden_tools,
        &files,
        skills,
        options.package_dir,
        None,
    );
    (
        prompt,
        PromptContextInputs {
            files,
            skills: skills.to_vec(),
            mcp_servers: None,
        },
    )
}

/// The SESSION `/reload` twin of
/// [`context_system_prompt_with_skills_and_options_and_inputs`]: always
/// produces the prompt (see
/// [`session_system_prompt_with_hidden_and_options_and_inputs`]).
pub fn session_system_prompt_with_skills_and_options_and_inputs(
    cwd: &Path,
    tools: &[ToolInfo],
    hidden_tools: &[String],
    skills: &[crate::skills::SkillInfo],
    options: &ContextOptions<'_>,
) -> (String, PromptContextInputs) {
    let files = resolve_context_files_with(cwd, options);
    let prompt = context_system_prompt_unconditional(
        cwd,
        tools,
        hidden_tools,
        &files,
        skills,
        options.package_dir,
        None,
    );
    (
        prompt,
        PromptContextInputs {
            files,
            skills: skills.to_vec(),
            mcp_servers: None,
        },
    )
}

/// [`session_system_prompt_with_skills_and_options_and_inputs`] with the MCP
/// manager's `mcp_servers` section (see [`mcp_servers_prompt_section`])
/// rendered into the prompt and cached in the returned inputs. This is the
/// SESSION `/reload` path: the caller hands the `mcp` builtin's current
/// section, so a reload refreshes it from the synchronous snapshot.
pub fn session_system_prompt_with_skills_and_options_and_inputs_and_mcp(
    cwd: &Path,
    tools: &[ToolInfo],
    hidden_tools: &[String],
    skills: &[crate::skills::SkillInfo],
    options: &ContextOptions<'_>,
    mcp_servers: Option<String>,
) -> (String, PromptContextInputs) {
    let files = resolve_context_files_with(cwd, options);
    let prompt = context_system_prompt_unconditional(
        cwd,
        tools,
        hidden_tools,
        &files,
        skills,
        options.package_dir,
        mcp_servers.as_deref(),
    );
    (
        prompt,
        PromptContextInputs {
            files,
            skills: skills.to_vec(),
            mcp_servers,
        },
    )
}

/// Rebuild the default structured prompt from already-discovered inputs — the
/// cached pair [`PromptContextInputs`] carries — with NO context-file or skill
/// discovery. The tool list and hidden declarations are the only inputs that
/// change, which is exactly the work upstream `_rebuildSystemPrompt` redoes on
/// an active-set change (`agent-session.ts:1705-1731`).
pub fn context_system_prompt_from_inputs(
    cwd: &Path,
    tools: &[ToolInfo],
    hidden_tools: &[String],
    inputs: &PromptContextInputs,
    package_dir: Option<&Path>,
) -> Option<String> {
    context_system_prompt_from_parts(
        cwd,
        tools,
        hidden_tools,
        &inputs.files,
        &inputs.skills,
        package_dir,
        inputs.mcp_servers.as_deref(),
    )
}

/// The SESSION active-set rebuild twin of [`context_system_prompt_from_inputs`]:
/// always produces the prompt, so a bare-workspace session keeps following the
/// declared set after the first build (the cache is always installed there).
pub fn session_system_prompt_from_inputs(
    cwd: &Path,
    tools: &[ToolInfo],
    hidden_tools: &[String],
    inputs: &PromptContextInputs,
    package_dir: Option<&Path>,
) -> String {
    context_system_prompt_unconditional(
        cwd,
        tools,
        hidden_tools,
        &inputs.files,
        &inputs.skills,
        package_dir,
        inputs.mcp_servers.as_deref(),
    )
}

/// [`session_system_prompt_from_inputs`] re-applying the CLI prompt overrides
/// (`--system-prompt` preamble + `--append-system-prompt` addendum sections)
/// on every active-set rebuild.
pub fn session_system_prompt_from_inputs_with_overrides(
    cwd: &Path,
    tools: &[ToolInfo],
    hidden_tools: &[String],
    inputs: &PromptContextInputs,
    package_dir: Option<&Path>,
    preamble_override: Option<&str>,
    append: &[String],
) -> String {
    context_system_prompt_unconditional_with(
        cwd,
        tools,
        hidden_tools,
        &inputs.files,
        &inputs.skills,
        package_dir,
        inputs.mcp_servers.as_deref(),
        preamble_override,
        append,
    )
}

/// The explicit core of the prompt builders: everything (the disable flag, the
/// agent dir, the package dir, the skill list) is a parameter — no ambient
/// read, so a hermetic caller controls the whole input.
fn build_context_prompt(
    cwd: &Path,
    tools: &[ToolInfo],
    hidden_tools: &[String],
    skills: &[crate::skills::SkillInfo],
    options: &ContextOptions<'_>,
) -> Option<String> {
    let files = resolve_context_files_with(cwd, options);
    context_system_prompt_from_parts(
        cwd,
        tools,
        hidden_tools,
        &files,
        skills,
        options.package_dir,
        None,
    )
}

/// Build the prompt text from resolved parts: the single place that applies
/// the "nothing to inject → no prompt" rule shared by every variant. Borrows
/// its inputs (no discovery, no clone), so an active-set rebuild can pass a
/// cache's slices directly.
fn context_system_prompt_from_parts(
    cwd: &Path,
    tools: &[ToolInfo],
    hidden_tools: &[String],
    files: &[(PathBuf, String)],
    skills: &[crate::skills::SkillInfo],
    package_dir: Option<&Path>,
    mcp_servers: Option<&str>,
) -> Option<String> {
    if files.is_empty() && skills.is_empty() && mcp_servers.is_none() {
        return None;
    }
    Some(context_system_prompt_unconditional(
        cwd,
        tools,
        hidden_tools,
        files,
        skills,
        package_dir,
        mcp_servers,
    ))
}

/// Build the prompt from resolved parts with NO "nothing to inject" check: the
/// base preamble, `<tools>`/`<rules>` (from the declared set) and `<cwd>` are
/// always produced (upstream `buildSystemPrompt`, system-prompt.ts:60-199,
/// which has no empty-resources arm).
///
/// This is what the SESSION hosts use (P7): the TUI/RPC/server build kept
/// `system_prompt: None` on a bare workspace, so the model got no preamble, no
/// tool list, no rules and no `<cwd>`. Print builds its own prompt
/// (`coding_agent/print.rs`) and the subagent child keeps the
/// append-to-nothing semantics, so both stay on [`context_system_prompt_with_hidden`].
fn context_system_prompt_unconditional(
    cwd: &Path,
    tools: &[ToolInfo],
    hidden_tools: &[String],
    files: &[(PathBuf, String)],
    skills: &[crate::skills::SkillInfo],
    package_dir: Option<&Path>,
    mcp_servers: Option<&str>,
) -> String {
    context_system_prompt_unconditional_with(
        cwd,
        tools,
        hidden_tools,
        files,
        skills,
        package_dir,
        mcp_servers,
        None,
        &[],
    )
}

/// [`context_system_prompt_unconditional`] with explicit CLI prompt
/// overrides. `preamble_override` replaces the default preamble;
/// `append` entries render as the `addendum` section in its upstream position.
#[allow(clippy::too_many_arguments)]
fn context_system_prompt_unconditional_with(
    cwd: &Path,
    tools: &[ToolInfo],
    hidden_tools: &[String],
    files: &[(PathBuf, String)],
    skills: &[crate::skills::SkillInfo],
    package_dir: Option<&Path>,
    mcp_servers: Option<&str>,
    preamble_override: Option<&str>,
    append: &[String],
) -> String {
    build_system_prompt(&SystemPromptInput {
        preamble_override: preamble_override.map(str::to_owned),
        append: append.to_vec(),
        tools,
        hidden_tools,
        project_files: files,
        skills,
        cwd,
        // The package asset dir is a resolved option (upstream
        // `getPackageDir`; read once at the boundary); the builder itself
        // never reads the process environment.
        package_dir,
        custom_sections: mcp_custom_sections(mcp_servers),
    })
}

/// Skills that join the system prompt when a caller builds the default prompt
/// itself: the settings `skills` paths, `<agentDir>/skills`,
/// `~/.agents/skills`, and (trust-gated) `<cwd>/.pi/skills` plus the ancestor
/// `.agents/skills` dirs. Prompt templates never join the system prompt, so
/// their scan is skipped.
///
/// Boundary wrapper: resolves the ambient options exactly once and delegates
/// to [`discover_prompt_skills_with`].
pub fn discover_prompt_skills(cwd: &Path) -> Vec<crate::skills::SkillInfo> {
    with_process_context_options(|options| discover_prompt_skills_with(cwd, options))
}

/// [`discover_prompt_skills`] with the resolved [`ContextOptions`] passed in
/// explicitly (hermetic: no ambient read). The project skill dirs stay
/// trust-gated against the global settings and the trust store under
/// `options.agent_dir` (the trust lookup's path is explicit; its
/// `PIR_TRUST_PROJECT` override lives in
/// [`crate::config::trust::resolve_cwd_trust`]).
pub fn discover_prompt_skills_with(
    cwd: &Path,
    options: &ContextOptions<'_>,
) -> Vec<crate::skills::SkillInfo> {
    let trusted = crate::config::trust::resolve_cwd_trust(cwd, options.agent_dir);
    let resources = crate::coding_agent::resources::DiscoveredResources::discover(
        &crate::coding_agent::resources::ResourceDiscoveryOptions {
            cwd,
            agent_dir: options.agent_dir,
            project_trusted: trusted,
            cli_skill_paths: &[],
            cli_prompt_paths: &[],
            no_skills: false,
            no_prompt_templates: true,
        },
    );
    // This layer only builds the system prompt (no user transcript and no
    // stderr contract of its own), so load diagnostics go to the routed
    // diagnostic log rather than being dropped.
    for diagnostic in &resources.diagnostics {
        crate::ext_diag!("[context] skill discovery: {diagnostic}");
    }
    resources.skills
}

/// `ToolInfo` list for the section builder, from a tool registry.
pub fn tool_infos(registry: &crate::agent::tool_execution::ToolRegistry) -> Vec<ToolInfo> {
    registry
        .iter()
        .map(|tool| ToolInfo::from_agent_tool(tool.as_ref()))
        .collect()
}
