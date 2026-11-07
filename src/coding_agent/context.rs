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
        &PromptSources {
            cwd,
            tools,
            hidden_tools,
            files: &files,
            skills: &skills,
            package_dir: options.package_dir,
            mcp_servers: mcp_servers.as_deref(),
        },
        PromptOverrides {
            preamble_override,
            append,
        },
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

/// [`context_system_prompt_with_hidden_in`] with NO "nothing to inject"
/// check: the base preamble, `<tools>`/`<rules>` (from the declared set) and
/// `<cwd>` are always produced (upstream `buildSystemPrompt`,
/// system-prompt.ts:60-199, has no empty-resources arm — audit M5).
///
/// The subagent child builds its body-bearing prompt from this: the agent
/// body rides the always-present default prompt as `appendSystemPrompt`, it
/// must not become the entire prompt when the workspace is bare.
#[cfg(feature = "subagent")]
pub(crate) fn context_system_prompt_with_hidden_in_unconditional(
    cwd: &Path,
    tools: &[ToolInfo],
    hidden_tools: &[String],
    agent_dir: &Path,
) -> String {
    let package_dir = crate::config::env::package_dir();
    let options = ContextOptions {
        disabled: !context_files_enabled(),
        agent_dir,
        package_dir: package_dir.as_deref(),
    };
    let files = resolve_context_files_with(cwd, &options);
    let skills = discover_prompt_skills_with(cwd, &options);
    context_system_prompt_unconditional(
        cwd,
        tools,
        hidden_tools,
        &files,
        &skills,
        package_dir.as_deref(),
        None,
    )
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
        &PromptSources {
            cwd,
            tools,
            hidden_tools,
            files: &inputs.files,
            skills: &inputs.skills,
            package_dir,
            mcp_servers: inputs.mcp_servers.as_deref(),
        },
        PromptOverrides {
            preamble_override,
            append,
        },
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

/// The resolved prompt sources every system-prompt variant renders.
struct PromptSources<'a> {
    cwd: &'a Path,
    tools: &'a [ToolInfo],
    hidden_tools: &'a [String],
    files: &'a [(PathBuf, String)],
    skills: &'a [crate::skills::SkillInfo],
    package_dir: Option<&'a Path>,
    mcp_servers: Option<&'a str>,
}

/// CLI prompt overrides applied on top of [`PromptSources`].
#[derive(Default)]
struct PromptOverrides<'a> {
    /// Replaces the default preamble when present.
    preamble_override: Option<&'a str>,
    /// Entries rendered as `addendum` sections.
    append: &'a [String],
}

/// Build the prompt from resolved parts with NO "nothing to inject" check: the
/// base preamble, `<tools>`/`<rules>` (from the declared set) and `<cwd>` are
/// always produced (upstream `buildSystemPrompt`, system-prompt.ts:60-199,
/// which has no empty-resources arm).
///
/// This is what the SESSION hosts use (P7): the TUI/RPC/server build kept
/// `system_prompt: None` on a bare workspace, so the model got no preamble, no
/// tool list, no rules and no `<cwd>`. Print builds its own prompt
/// (`coding_agent/print.rs`) and the subagent child body-bearing prompt now
/// uses the explicit `_in` unconditional wrapper below, so both produce a
/// full prompt on a bare workspace too.
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
        &PromptSources {
            cwd,
            tools,
            hidden_tools,
            files,
            skills,
            package_dir,
            mcp_servers,
        },
        PromptOverrides::default(),
    )
}

/// [`context_system_prompt_unconditional`] with explicit CLI prompt
/// overrides. `preamble_override` replaces the default preamble;
/// `append` entries render as the `addendum` section in its upstream position.
fn context_system_prompt_unconditional_with(
    sources: &PromptSources<'_>,
    overrides: PromptOverrides<'_>,
) -> String {
    build_system_prompt(&SystemPromptInput {
        preamble_override: overrides.preamble_override.map(str::to_owned),
        append: overrides.append.to_vec(),
        tools: sources.tools,
        hidden_tools: sources.hidden_tools,
        project_files: sources.files,
        skills: sources.skills,
        cwd: sources.cwd,
        // The package asset dir is a resolved option (upstream
        // `getPackageDir`; read once at the boundary); the builder itself
        // never reads the process environment.
        package_dir: sources.package_dir,
        custom_sections: mcp_custom_sections(sources.mcp_servers),
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

// ---------------------------------------------------------------------------
// `@file` / `@image` prompt references
// ---------------------------------------------------------------------------
//
// One implementation shared by the CLI hosts (`pir @file`, `pir print
// "@file"` — `coding_agent::print`) and by the session prompt input
// (`AgentSession::prompt_with_images_and_source`, the TUI worker path). The
// semantics are upstream `cli/file-processor.ts` + `main.ts`
// `prepareInitialMessage`:
//
// - text files render as `<file name="…">\n{content}\n</file>\n` (BOM
//   stripped, lossy UTF-8, empty files skipped);
// - image files become `ImageContent` attachments (PNG/JPEG/GIF/WebP pass
//   through; BMP and other sniffed formats are normalized to PNG when the
//   image stack is built in) and contribute their upstream marker line;
// - missing files are hard errors with the absolute path.
//
// Path handling is the shared boundary: `~`/`~/`/`~\`, cwd-relative and
// absolute paths, and quoted / backslash-escaped whitespace all resolve the
// same way for every entry point.

/// Resize profile for `@file` image attachments. `Some` runs supported inline
/// formats (PNG/JPEG/GIF/WebP) through the shared `process_image` resize path;
/// `None` keeps the upstream print-mode pass-through (`autoResizeImages:
/// false`). Text files are unaffected either way.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ImageAttachmentOptions {
    pub max_width: u32,
    pub max_height: u32,
    /// Maximum base64 payload size (matching `process_image`'s `max_bytes`).
    pub max_bytes: usize,
    pub jpeg_quality: u8,
}

/// [`partition_file_args`] with an explicit resize profile.
///
/// `options = None` is byte-for-byte the old print-mode behavior: PNG/JPEG/
/// GIF/WebP pass through untouched, BMP and other sniffed formats are
/// normalized to PNG. `Some(options)` additionally resizes the supported
/// inline formats through [`crate::coding_agent::tools::image::process_image`]
/// (dimension + encoded-byte limits, JPEG quality ladder); a decodable image
/// that cannot fit the limits renders the upstream failure text and attaches
/// nothing, exactly like the existing BMP path. An inline-format payload that
/// cannot be decoded at all (truncated/synthetic bytes) keeps the old raw
/// pass-through, because there is nothing to resize.
pub fn partition_file_args_with_options(
    file_args: &[String],
    cwd: &Path,
    options: Option<ImageAttachmentOptions>,
) -> crate::Result<(String, Vec<crate::ai::types::ImageContent>)> {
    use std::io::Read as _;

    let mut text = String::new();
    let mut images = Vec::new();
    for file_arg in file_args {
        let path = resolve_file_arg_path(file_arg, cwd);
        if !path.is_file() {
            return Err(anyhow::anyhow!("File not found: {}", path.display()));
        }
        let mut file = std::fs::File::open(&path)
            .map_err(|error| anyhow::anyhow!("Could not read file {}: {error}", path.display()))?;
        let size = file
            .metadata()
            .map_err(|error| anyhow::anyhow!("Could not stat file {}: {error}", path.display()))?
            .len();
        // Upstream skips empty files before the MIME sniff.
        if size == 0 {
            continue;
        }
        // Sniff from a small read instead of the full file; these bytes are
        // then reused for the text block or extended for the image payload.
        // The image sniffer is compiled only when the print host or read tool
        // pulls in `tools::image`; a minimal SDK build treats every file as
        // text and never references that module.
        #[cfg(any(feature = "print", feature = "tool-read"))]
        {
            let mut head = [0u8; crate::coding_agent::tools::image::IMAGE_TYPE_SNIFF_BYTES];
            let mut filled = 0;
            while filled < head.len() {
                match file.read(&mut head[filled..]) {
                    Ok(0) => break,
                    Ok(read) => filled += read,
                    Err(error) => {
                        return Err(anyhow::anyhow!(
                            "Could not read file {}: {error}",
                            path.display()
                        ))
                    }
                }
            }
            let mut bytes = Vec::with_capacity(filled);
            bytes.extend_from_slice(&head[..filled]);
            file.read_to_end(&mut bytes).map_err(|error| {
                anyhow::anyhow!("Could not read file {}: {error}", path.display())
            })?;
            let sniff_head = &bytes[..bytes
                .len()
                .min(crate::coding_agent::tools::image::IMAGE_TYPE_SNIFF_BYTES)];
            match crate::coding_agent::tools::image::detect_supported_image_mime_type(sniff_head) {
                Some(mime_type) => {
                    append_file_image(&path, bytes, mime_type, &mut text, &mut images, options)
                }
                None => append_file_text(&path, &bytes, &mut text),
            }
        }
        #[cfg(not(any(feature = "print", feature = "tool-read")))]
        {
            let _ = options;
            let mut bytes = Vec::new();
            file.read_to_end(&mut bytes).map_err(|error| {
                anyhow::anyhow!("Could not read file {}: {error}", path.display())
            })?;
            append_file_text(&path, &bytes, &mut text);
        }
    }
    Ok((text, images))
}

/// Process `@file` arguments with the upstream print-mode image behavior
/// (`autoResizeImages: false`): see [`partition_file_args_with_options`].
pub fn partition_file_args(
    file_args: &[String],
    cwd: &Path,
) -> crate::Result<(String, Vec<crate::ai::types::ImageContent>)> {
    partition_file_args_with_options(file_args, cwd, None)
}

/// One whitespace-delimited token of a prompt value.
struct PromptToken {
    /// Byte range of the raw token in the source prompt (quotes included).
    start: usize,
    end: usize,
    /// The token with quotes removed and escapes resolved.
    value: String,
    /// The raw token begins with `\@`: an escaped literal at-sign, never a
    /// file reference.
    escaped_at: bool,
    /// The token is a file reference.
    file: bool,
}

/// Whether an `@`-stripped token should be treated as a file reference.
///
/// Top-level argv treats every `@x` positional as a file, but a prompt is
/// free-form text: `Use @ here` or `follow @pi` must stay literal. A token is
/// a file reference when the whole prompt is a list of `@` tokens (the
/// explicit `pir print "@a.txt @b.txt"` shape), or when the path looks like
/// one (separator, `~`, an alphabetic extension) or already exists on disk.
fn is_prompt_file_reference(path: &str, cwd: &Path) -> bool {
    if path.is_empty() {
        return false;
    }
    if path.starts_with('~') || path.contains('/') || path.contains('\\') {
        return true;
    }
    let extension = path.rsplit_once('.').map(|(_, extension)| extension);
    let extension_like = extension.is_some_and(|extension| {
        !extension.is_empty() && extension.chars().any(|ch| ch.is_alphabetic())
    });
    extension_like || resolve_file_arg_path(path, cwd).exists()
}

/// Scan one prompt string into whitespace-delimited tokens.
///
/// `pir print` takes a single prompt positional, so its value never passes
/// through the argv-level `split_at_args` split. This is the string-level
/// equivalent: a leading `@` makes a token a file reference; single/double
/// quotes group whitespace inside a path; `\` escapes whitespace, quotes, a
/// backslash or `@` (other backslashes stay literal so Windows path
/// separators survive). A token that merely CONTAINS `@` (`user@example.com`)
/// is ordinary text. Quotes around the WHOLE token (`"@my notes.txt"`) are
/// accepted too, matching the shell-stripped argv shape.
fn scan_prompt_tokens(prompt: &str, cwd: &Path) -> Vec<PromptToken> {
    let mut tokens: Vec<PromptToken> = Vec::new();
    let mut chars = prompt.char_indices().peekable();
    while let Some(&(start, first)) = chars.peek() {
        if first.is_whitespace() {
            chars.next();
            continue;
        }
        let mut value = String::new();
        let mut quote: Option<char> = None;
        let mut escaped = false;
        let mut end = start;
        while let Some(&(index, ch)) = chars.peek() {
            if quote.is_none() && !escaped && ch.is_whitespace() {
                break;
            }
            chars.next();
            end = index + ch.len_utf8();
            if escaped {
                // Inside quotes only the active quote and a literal
                // backslash are escapable; outside, whitespace/quotes/
                // backslash/`@` are. Any other pair keeps its backslash
                // (`C:\Users\...`).
                let unescape = match quote {
                    Some(active) => ch == active || ch == '\\',
                    None => {
                        ch.is_whitespace() || ch == '\\' || ch == '\'' || ch == '"' || ch == '@'
                    }
                };
                if unescape {
                    value.push(ch);
                } else {
                    value.push('\\');
                    value.push(ch);
                }
                escaped = false;
                continue;
            }
            match quote {
                Some(active) if ch == active => quote = None,
                Some(_) if ch == '\\' => escaped = true,
                Some(_) => value.push(ch),
                None if ch == '\\' => escaped = true,
                None if ch == '\'' || ch == '"' => quote = Some(ch),
                None => value.push(ch),
            }
        }
        if escaped {
            value.push('\\');
        }
        let raw = &prompt[start..end];
        tokens.push(PromptToken {
            start,
            end,
            escaped_at: raw.starts_with("\\@"),
            file: false,
            value,
        });
    }
    // A prompt made up exclusively of `@` tokens is an explicit file list:
    // even an extensionless/relative entry (`@missing`) is a reference, the
    // same way the bare argv path treats every `@x` positional.
    let whole_prompt_files = !tokens.is_empty()
        && tokens
            .iter()
            .all(|token| !token.escaped_at && token.value.starts_with('@'));
    for token in &mut tokens {
        token.file = !token.escaped_at
            && token.value.starts_with('@')
            && (whole_prompt_files
                || token
                    .value
                    .strip_prefix('@')
                    .is_some_and(|path| is_prompt_file_reference(path, cwd)));
    }
    tokens
}

/// Remove every file-reference token (and the whitespace separating it from
/// its neighbour) from the prompt, leaving the ordinary text. A leading `\@`
/// escape loses its backslash and stays as literal text.
fn prompt_text_without_file_args(prompt: &str, tokens: &[PromptToken]) -> String {
    let mut ranges: Vec<std::ops::Range<usize>> = Vec::new();
    for token in tokens {
        if token.file {
            let mut start = token.start;
            let mut end = token.end;
            let mut trailing = end;
            for (offset, ch) in prompt[end..].char_indices() {
                if ch.is_whitespace() {
                    trailing = end + offset + ch.len_utf8();
                } else {
                    break;
                }
            }
            if trailing > end {
                end = trailing;
            } else {
                while start > 0 {
                    let Some((index, ch)) = prompt[..start].char_indices().next_back() else {
                        break;
                    };
                    if ch.is_whitespace() {
                        start = index;
                    } else {
                        break;
                    }
                }
            }
            ranges.push(start..end);
        } else if token.escaped_at {
            ranges.push(token.start..token.start + 1);
        }
    }
    ranges.sort_by_key(|range| range.start);
    let mut message = String::with_capacity(prompt.len());
    let mut cursor = 0;
    for range in ranges {
        if range.start > cursor {
            message.push_str(&prompt[cursor..range.start]);
        }
        cursor = cursor.max(range.end);
    }
    message.push_str(&prompt[cursor..]);
    if tokens.iter().any(|token| token.file) {
        // File tokens leave whitespace holes where they were removed; the
        // remaining message is otherwise passed through verbatim.
        message.trim().to_string()
    } else {
        message
    }
}

/// Expand `@file` references inside one prompt value.
///
/// The bare CLI entry shapes (`pir @notes.txt`, `pir -p @notes.txt`,
/// `pir --mode json @notes.txt`) split `@path` positionals in argv and pass
/// them through [`partition_file_args`]. The `pir print` subcommand and the
/// session prompt input receive their whole prompt as ONE string, so the same
/// rules are applied to that string here:
///
/// - every whitespace-delimited token beginning with `@` is a file reference
///   and is expanded IN ORDER through [`partition_file_args`] (text files
///   render as `<file name="…">…</file>`, images become attachments and keep
///   their marker line; missing files are hard errors; empty files skip);
/// - the `<file …>` text is placed BEFORE the remaining prompt text, exactly
///   like `build_print_prompts` joins argv file text and the first message;
/// - `@"my notes.txt"`, `"@my notes.txt"`, `@'my notes.txt'` and
///   `@my\ notes.txt` quote/escape whitespace in a path;
/// - `\@literal` escapes a leading `@` and stays in the message as
///   `@literal`;
/// - a token that does not START with `@` is untouched, and a lone `@` or a
///   word-like token (`@pi`, `@2.0`) inside ordinary prose is not forced
///   into a file reference, so `user@example.com` and `Use @ here` stay
///   literal. A prompt made only of `@` tokens, or a token with a path
///   separator / `~` / file extension / existing file, is a reference.
pub fn expand_prompt_file_args(
    prompt: &str,
    cwd: &Path,
) -> crate::Result<(String, Vec<crate::ai::types::ImageContent>)> {
    expand_prompt_file_args_with_options(prompt, cwd, None)
}

/// [`expand_prompt_file_args`] with an explicit image resize profile.
///
/// `None` keeps the print-mode pass-through; `Some(options)` resizes supported
/// inline image attachments (Interactive TUI prompts pass the current model's
/// `inputLimits.images.resize` resolved to these options).
pub fn expand_prompt_file_args_with_options(
    prompt: &str,
    cwd: &Path,
    options: Option<ImageAttachmentOptions>,
) -> crate::Result<(String, Vec<crate::ai::types::ImageContent>)> {
    let tokens = scan_prompt_tokens(prompt, cwd);
    let has_file = tokens.iter().any(|token| token.file);
    let has_escape = tokens.iter().any(|token| token.escaped_at);
    if !has_file && !has_escape {
        return Ok((prompt.to_owned(), Vec::new()));
    }
    let file_args: Vec<String> = tokens
        .iter()
        .filter(|token| token.file)
        .map(|token| {
            token
                .value
                .strip_prefix('@')
                .unwrap_or(&token.value)
                .to_owned()
        })
        .collect();
    let (file_text, images) = partition_file_args_with_options(&file_args, cwd, options)?;
    let message = prompt_text_without_file_args(prompt, &tokens);
    Ok((format!("{file_text}{message}"), images))
}

/// Text half of `processFileArguments`: lossy UTF-8 with a BOM strip.
fn append_file_text(path: &Path, bytes: &[u8], text: &mut String) {
    let raw = String::from_utf8_lossy(bytes);
    let content = raw.strip_prefix('\u{feff}').unwrap_or(&raw);
    text.push_str(&format!(
        "<file name=\"{}\">\n{content}\n</file>\n",
        path.display()
    ));
}

/// Image half of `processFileArguments`: with `options = None` this is
/// `processImage` with `autoResizeImages: false` (upstream print mode) —
/// normalize to a supported inline format, never resize. With
/// `Some(options)` PNG/JPEG/GIF/WebP also go through `process_image` with the
/// resolved limits. Either way, render the conversion/resize hints or the
/// processing failure text exactly like upstream.
#[cfg(any(feature = "print", feature = "tool-read"))]
#[cfg(any(feature = "image-tools", all(feature = "print", feature = "tui")))]
fn append_file_image(
    path: &Path,
    bytes: Vec<u8>,
    mime_type: &'static str,
    text: &mut String,
    images: &mut Vec<crate::ai::types::ImageContent>,
    options: Option<ImageAttachmentOptions>,
) {
    use crate::coding_agent::tools::image::{
        process_image, ProcessImageError, DEFAULT_JPEG_QUALITY,
    };

    // The formats `processImage` normalizes to pass through untouched under
    // `autoResizeImages: false`: upstream returns the original bytes without
    // decoding them (only a non-inline format such as BMP is converted).
    // A resize profile opts the supported inline formats INTO `process_image`
    // below; `None` preserves that pass-through exactly.
    let inline_format = matches!(
        mime_type,
        "image/png" | "image/jpeg" | "image/gif" | "image/webp"
    );
    if options.is_none() && inline_format {
        use base64::Engine as _;
        images.push(crate::ai::types::ImageContent {
            data: base64::engine::general_purpose::STANDARD.encode(&bytes),
            mime_type: mime_type.to_owned(),
        });
        text.push_str(&format!("<file name=\"{}\"></file>\n", path.display()));
        return;
    }
    // BMP (and any other sniffed non-inline format): `processImage` decodes
    // and re-encodes it as PNG. Unlimited limits express `autoResizeImages:
    // false`: the dimension/byte checks always pass, so no resize ladder runs
    // and only the conversion hint is produced.
    let (max_width, max_height, max_bytes, jpeg_quality) = match options {
        Some(options) => (
            options.max_width,
            options.max_height,
            options.max_bytes,
            options.jpeg_quality,
        ),
        None => (u32::MAX, u32::MAX, usize::MAX, DEFAULT_JPEG_QUALITY),
    };
    let processed = process_image(
        &bytes,
        mime_type,
        max_width,
        max_height,
        max_bytes,
        jpeg_quality,
    );
    match processed {
        Ok(processed) => {
            if processed.hints.is_empty() {
                text.push_str(&format!("<file name=\"{}\"></file>\n", path.display()));
            } else {
                text.push_str(&format!(
                    "<file name=\"{}\">{}</file>\n",
                    path.display(),
                    processed.hints.join("\n")
                ));
            }
            images.push(crate::ai::types::ImageContent {
                data: processed.data_base64,
                mime_type: processed.mime_type,
            });
        }
        // A supported inline format that cannot be decoded (truncated or
        // synthetic payloads) keeps the previous pass-through behavior
        // instead of dropping the attachment: there is nothing to resize.
        // A decodable image that cannot fit the byte limit still reports the
        // upstream failure and attaches nothing.
        Err(ProcessImageError::Convert) if inline_format => {
            use base64::Engine as _;
            images.push(crate::ai::types::ImageContent {
                data: base64::engine::general_purpose::STANDARD.encode(&bytes),
                mime_type: mime_type.to_owned(),
            });
            text.push_str(&format!("<file name=\"{}\"></file>\n", path.display()));
        }
        Err(error) => {
            text.push_str(&format!(
                "<file name=\"{}\">{}</file>\n",
                path.display(),
                error.message()
            ));
        }
    }
}

/// Image half without the image stack: no normalize/decode is possible, so
/// keep the previous pass-through attachment (raw bytes).
#[cfg(any(feature = "print", feature = "tool-read"))]
#[cfg(not(any(feature = "image-tools", all(feature = "print", feature = "tui"))))]
fn append_file_image(
    path: &Path,
    bytes: Vec<u8>,
    mime_type: &'static str,
    text: &mut String,
    images: &mut Vec<crate::ai::types::ImageContent>,
    _options: Option<ImageAttachmentOptions>,
) {
    use base64::Engine as _;

    images.push(crate::ai::types::ImageContent {
        data: base64::engine::general_purpose::STANDARD.encode(&bytes),
        mime_type: mime_type.to_owned(),
    });
    text.push_str(&format!("<file name=\"{}\"></file>\n", path.display()));
}

/// Resolve an `@file` argument against `~`/`~/` and the cwd (same shape as
/// cli.rs's private `expand_tilde` + cwd join).
fn resolve_file_arg_path(file_arg: &str, cwd: &Path) -> PathBuf {
    let path = if file_arg == "~" || file_arg.starts_with("~/") || file_arg.starts_with("~\\") {
        match std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")) {
            Some(home) => PathBuf::from(home).join(
                file_arg
                    .trim_start_matches('~')
                    .trim_start_matches(['/', '\\']),
            ),
            None => PathBuf::from(file_arg),
        }
    } else {
        PathBuf::from(file_arg)
    };
    if path.is_absolute() {
        path
    } else {
        cwd.join(path)
    }
}
