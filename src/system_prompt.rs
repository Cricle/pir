//! System prompt construction and section-based replay.
//!
//! Port of upstream `packages/coding-agent/src/core/system-prompt.ts`. The prompt is
//! assembled as an ordered map of named sections: `preamble` is untagged text, every
//! other section is wrapped in an XML tag of the same name so later turns can patch
//! individual sections via [`diff_sections`].

use std::path::{Path, PathBuf};

use crate::agent::tool_execution::AgentTool;
use crate::skills::{SkillFileReadTool, SkillInfo};

/// Default preamble when no override (`--system-prompt` / `SYSTEM.md`) is supplied.
const DEFAULT_PREAMBLE: &str = "You are an expert coding assistant operating inside pir, a \
coding agent harness. You help users by reading files, executing commands, editing code, and \
writing new files.";

/// Minimal tool identity used for prompt assembly (upstream `toolSnippets` /
/// `toolGuidelines` keyed by tool name): the model-facing description, the
/// tool's one-line `promptSnippet` (only snippet-carrying tools join the
/// tools section), and its guideline bullets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolInfo {
    pub name: String,
    pub description: String,
    /// One-line snippet for the tools section (upstream `promptSnippet`).
    pub snippet: Option<String>,
    /// Guideline bullets contributed to the rules section (upstream
    /// `promptGuidelines`).
    pub guidelines: Vec<String>,
}

impl ToolInfo {
    /// A plain tool identity with no prompt contributions.
    pub fn new(name: impl Into<String>, description: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            snippet: None,
            guidelines: Vec::new(),
        }
    }

    /// Collect a registered tool's identity and prompt contributions
    /// (`promptSnippet` / `promptGuidelines`).
    pub fn from_agent_tool(tool: &dyn AgentTool) -> Self {
        Self {
            name: tool.name().to_string(),
            description: tool.description().to_string(),
            snippet: tool.prompt_snippet(),
            guidelines: tool.prompt_guidelines(),
        }
    }
}

/// `ToolInfo` list for the section builder, from a tool registry.
pub fn tool_infos_from_registry(
    registry: &crate::agent::tool_execution::ToolRegistry,
) -> Vec<ToolInfo> {
    registry
        .iter()
        .map(|tool| ToolInfo::from_agent_tool(tool.as_ref()))
        .collect()
}

/// Ordered system prompt sections, keyed by name. `preamble` is untagged text; every
/// other section is wrapped in a tag of the same name so the model can match later
/// updates to it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SystemPromptSections {
    entries: Vec<(SystemPromptSection, String)>,
}

impl SystemPromptSections {
    /// Insert or replace a section, keeping first-insertion order (upstream relies on
    /// JS object insertion order; sections are inserted in the fixed builder order).
    pub fn insert(&mut self, section: SystemPromptSection, content: String) {
        if let Some(existing) = self.entries.iter_mut().find(|(name, _)| *name == section) {
            existing.1 = content;
        } else {
            self.entries.push((section, content));
        }
    }

    pub fn get(&self, section: impl std::borrow::Borrow<SystemPromptSection>) -> Option<&str> {
        self.entries
            .iter()
            .find(|(name, _)| *name == *section.borrow())
            .map(|(_, content)| content.as_str())
    }

    pub fn iter(&self) -> impl Iterator<Item = (&SystemPromptSection, &str)> {
        self.entries
            .iter()
            .map(|(name, content)| (name, content.as_str()))
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }
}

/// A named section of the structured system prompt, in the upstream assembly order.
/// `Custom` carries extension-supplied named sections (upstream accepts arbitrary
/// extension sections, regex-validated, appended after the built-ins).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum SystemPromptSection {
    Preamble,
    Tools,
    Rules,
    /// Upstream `docs`: pi's own documentation paths, read on demand.
    Docs,
    /// User/appended configuration (upstream `addendum`).
    Addendum,
    ProjectContext,
    Skills,
    Cwd,
    Custom(String),
}

impl SystemPromptSection {
    /// The XML tag / transcript key for this section.
    pub fn tag(&self) -> String {
        match self {
            Self::Preamble => "preamble".to_owned(),
            Self::Tools => "tools".to_owned(),
            Self::Rules => "rules".to_owned(),
            Self::Docs => "docs".to_owned(),
            Self::Addendum => "addendum".to_owned(),
            Self::ProjectContext => "project_context".to_owned(),
            Self::Skills => "skills".to_owned(),
            Self::Cwd => "cwd".to_owned(),
            Self::Custom(name) => name.clone(),
        }
    }
}

/// Upstream `SYSTEM_PROMPT_SECTION_NAME`: `^[a-z][a-z0-9_-]*$`.
fn is_valid_section_name(name: &str) -> bool {
    let mut chars = name.chars();
    match chars.next() {
        Some(first) if first.is_ascii_lowercase() => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
}

/// Inputs for [`build_system_prompt_sections`].
#[derive(Debug, Clone)]
pub struct SystemPromptInput<'a> {
    /// Custom system prompt replacing the default preamble (`--system-prompt` / `SYSTEM.md`).
    pub preamble_override: Option<String>,
    /// Appendable system prompt additions (upstream `addendum` section).
    pub append: Vec<String>,
    /// Active tools, in display order.
    pub tools: &'a [ToolInfo],
    /// Active tools whose declarations requests leave out (upstream
    /// `hiddenTools`: `prepareLoadout` hidden declarations plus
    /// `codemode`/`deferred`/`hidden` exposures). They are reachable only
    /// through another tool, so the tool list and rules leave them out too.
    pub hidden_tools: &'a [String],
    /// Discovered context files as `(path, content)`, in load order.
    pub project_files: &'a [(PathBuf, String)],
    /// Discovered skills.
    pub skills: &'a [SkillInfo],
    /// Additional XML-wrapped prompt sections keyed by tag name (upstream
    /// `sections`); appended after the built-ins, in order.
    pub custom_sections: Vec<(String, String)>,
    /// Working directory.
    pub cwd: &'a Path,
    /// Package asset directory (upstream `getPackageDir`): callers resolve
    /// `PI_PACKAGE_DIR` once ([`crate::config::env::package_dir`]) and thread
    /// the value in, mirroring `cwd`, so the builder never reads the process
    /// environment. The `docs` section is emitted only when this directory's
    /// `README.md` exists.
    pub package_dir: Option<&'a Path>,
}

/// A patch produced by [`diff_sections`]: sections whose content changed (or are new),
/// and sections present previously but removed now.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SectionChanges {
    pub changed: Vec<(SystemPromptSection, String)>,
    pub removed: Vec<SystemPromptSection>,
}

impl SectionChanges {
    pub fn is_empty(&self) -> bool {
        self.changed.is_empty() && self.removed.is_empty()
    }
}

fn render_tools_section(tools: &[ToolInfo]) -> String {
    // Only tools with a `promptSnippet` are listed; the description is never
    // used as a fallback (upstream builds its `toolSnippets` map from
    // snippet-carrying tools only, agent-session.ts:1701-1706, and lists
    // exactly those).
    let bullets: Vec<String> = tools
        .iter()
        .filter_map(|tool| {
            let snippet = tool
                .snippet
                .as_deref()
                .map(str::trim)
                .filter(|snippet| !snippet.is_empty())?;
            Some(format!("- {}: {}", tool.name, snippet))
        })
        .collect();
    let bullets = if bullets.is_empty() {
        "(none)".to_string()
    } else {
        bullets.join("\n")
    };
    format!(
        "{bullets}\n\nIn addition to the tools above, you may have access to other custom tools \
depending on the project."
    )
}

/// Upstream `buildRules`: the conditional file-operations rule, then each
/// declared tool's guidelines in order, then the fixed rules — all dedup'd.
fn render_rules_section(tools: &[ToolInfo]) -> String {
    let mut rules: Vec<String> = Vec::new();
    let mut seen = std::collections::BTreeSet::new();
    let mut add_rule = |rule: &str, rules: &mut Vec<String>| {
        let normalized = rule.trim();
        if normalized.is_empty() || seen.contains(normalized) {
            return;
        }
        seen.insert(normalized.to_string());
        rules.push(normalized.to_string());
    };

    let has_bash = tools.iter().any(|tool| tool.name == "bash");
    let has_powershell = tools.iter().any(|tool| tool.name == "powershell");
    let has_grep = tools.iter().any(|tool| tool.name == "grep");
    let has_find = tools.iter().any(|tool| tool.name == "find");
    let has_ls = tools.iter().any(|tool| tool.name == "ls");

    if (has_bash || has_powershell) && !has_grep && !has_find && !has_ls {
        if has_bash && has_powershell {
            add_rule(
                "Use bash or PowerShell for file operations like listing, searching, and finding files",
                &mut rules,
            );
        } else if has_powershell {
            add_rule(
                "Use PowerShell for file operations like listing, searching, and finding files",
                &mut rules,
            );
        } else {
            add_rule("Use bash for file operations like ls, rg, find", &mut rules);
        }
    }

    for tool in tools {
        for guideline in &tool.guidelines {
            add_rule(guideline, &mut rules);
        }
    }
    add_rule("Be concise in your responses", &mut rules);
    add_rule(
        "Show file paths clearly when working with files",
        &mut rules,
    );
    rules
        .iter()
        .map(|rule| format!("- {rule}"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Name of the system prompt section listing the configured MCP servers
/// (upstream `MCP_SERVERS_SECTION`, `extensions/mcp/index.ts:156`).
pub const MCP_SERVERS_SECTION: &str = "mcp_servers";

/// Characters of one server's failure reason in the section (CL24's
/// `bounded_reason` cap; a reason is one line, never a stderr/config dump).
pub const MAX_MCP_SERVER_REASON_CHARS: usize = 200;

/// Characters of the whole section (upstream `MAX_SERVERS_SECTION_CHARS`):
/// server lines shrink to fit, and when the names alone do not fit the last
/// servers are left out and counted in a closing line.
pub const MAX_MCP_SERVERS_SECTION_CHARS: usize = 4096;

/// One MCP server's readiness for the `mcp_servers` prompt section. The
/// states mirror upstream `describeState` (`extensions/mcp/index.ts:154-166`)
/// as far as the bounded startup report and the published snapshot allow.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum McpServerState {
    /// Connected and its tools are published: `connected · N tools`.
    Ready {
        /// Number of tools the last `tools/list` published.
        tool_count: usize,
    },
    /// Still connecting (the startup budget elapsed, or no connect was
    /// attempted yet): upstream `connecting…`.
    Pending,
    /// The last connect failed; `reason` is the bounded one-line cause. A
    /// server that needs a sign-in reports here too: the bounded report keeps
    /// the sign-in instruction as the reason.
    Failed {
        /// Bounded reason (see [`bounded_mcp_reason`]).
        reason: String,
    },
}

/// One row of the `mcp_servers` prompt section (upstream
/// `McpServerListing`): the server's namespace, how its tools are reached
/// (`codemode` / `deferred` / `direct`), and its state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpServerListing {
    /// The configured server name (rendered as `mcp__<name>`).
    pub name: String,
    /// The effective exposure label (`codemode` / `deferred` / `direct`).
    pub reach: &'static str,
    /// The server's readiness.
    pub state: McpServerState,
}

/// The tool namespace of a server (upstream `mcpNamespace`): `mcp__<server>`
/// with `-` replaced by `_`. Inlined here so this core module carries no
/// dependency on the `mcp` feature.
fn mcp_namespace(server: &str) -> String {
    format!("mcp__{}", server.replace('-', "_"))
}

/// One line, length-bounded reason text for the section.
///
/// Mirrors `mcp::manager::bounded_reason` (which stays the diagnostics
/// spelling): the first line carries the cause, later lines are dropped, and
/// the result is capped. The section repeats the bound defensively so a
/// caller handing it raw text cannot leak a multi-line stderr tail into the
/// prompt.
pub fn bounded_mcp_reason(text: &str) -> String {
    let first = text.lines().next().unwrap_or("").trim();
    if first.chars().count() <= MAX_MCP_SERVER_REASON_CHARS {
        return first.to_owned();
    }
    let mut bounded: String = first.chars().take(MAX_MCP_SERVER_REASON_CHARS).collect();
    bounded.push('…');
    bounded
}

/// The `mcp_servers` section text, or `None` when there is nothing to list.
///
/// Upstream renders one line per server as `- mcp__<name> (<reach>)` with a
/// one-line summary (`renderServersSection`, `extensions/mcp/index.ts:195-223`).
/// pir's summary is the bounded state (`connected · N tools` / `connecting…` /
/// `failed: <reason>`) rather than the server description, because the
/// failure/pending report is the reason the section exists: the model must
/// learn which servers it can reach and why one cannot be reached, without a
/// config/env/stderr dump.
pub fn render_mcp_servers_section(servers: &[McpServerListing]) -> Option<String> {
    if servers.is_empty() {
        return None;
    }
    let intro =
        "MCP servers configured for this session; their tools are named mcp__<server>__<tool>.";
    let summary = |state: &McpServerState| match state {
        McpServerState::Ready { tool_count } => format!(
            "connected · {tool_count} {}",
            if *tool_count == 1 { "tool" } else { "tools" }
        ),
        McpServerState::Pending => "connecting…".to_owned(),
        McpServerState::Failed { reason } => {
            format!("failed: {}", bounded_mcp_reason(reason))
        }
    };
    let heads: Vec<String> = servers
        .iter()
        .map(|server| {
            format!(
                "- {} ({}): {}",
                mcp_namespace(&server.name),
                server.reach,
                summary(&server.state)
            )
        })
        .collect();
    // Keep the whole section within upstream's bound: drop trailing server
    // lines and count them, never cut a line in half.
    let omitted = |count: usize| {
        (count > 0).then(|| {
            format!(
                "- … {count} more server{}",
                if count == 1 { "" } else { "s" }
            )
        })
    };
    let mut kept = heads.len();
    let size = |kept: usize| {
        let mut parts = vec![intro.to_owned()];
        parts.extend(heads.iter().take(kept).cloned());
        parts.extend(omitted(heads.len() - kept));
        parts.join("\n").chars().count()
    };
    while kept > 0 && size(kept) > MAX_MCP_SERVERS_SECTION_CHARS {
        kept -= 1;
    }
    if kept == 0 {
        return None;
    }
    let mut lines = vec![intro.to_owned()];
    lines.extend(heads.iter().take(kept).cloned());
    lines.extend(omitted(heads.len() - kept));
    Some(lines.join("\n"))
}

fn render_project_context(project_files: &[(PathBuf, String)]) -> String {
    let mut parts = vec!["Project-specific instructions and guidelines:".to_string()];
    for (path, content) in project_files {
        parts.push(format!(
            "<project_instructions path=\"{}\">\n{}\n</project_instructions>",
            path.display(),
            content
        ));
    }
    parts.join("\n\n")
}

/// The docs paths under a resolved package asset directory (upstream
/// `getPackageDir`): the directory arrives via
/// [`SystemPromptInput::package_dir`] — callers read `PI_PACKAGE_DIR` once
/// through [`crate::config::env::package_dir`] — so the builder itself never
/// touches the process environment. pir bundles no package assets, so the docs
/// section is emitted only when the package root (with its `README.md`) exists.
fn package_docs_paths(package_dir: Option<&Path>) -> Option<(PathBuf, PathBuf, PathBuf)> {
    let package_dir = package_dir?;
    let readme = package_dir.join("README.md");
    if !readme.exists() {
        return None;
    }
    Some((
        readme,
        package_dir.join("docs"),
        package_dir.join("examples"),
    ))
}

/// Upstream `docs` section: pi's own documentation, read only on demand.
fn render_docs_section(package_dir: Option<&Path>) -> Option<String> {
    let (readme, docs, examples) = package_docs_paths(package_dir)?;
    let readme = readme.to_string_lossy().replace('\\', "/");
    let docs = docs.to_string_lossy().replace('\\', "/");
    let examples = examples.to_string_lossy().replace('\\', "/");
    Some(format!(
        "Pi documentation (read only when the user asks about pi itself, its SDK, extensions, \
themes, skills, or TUI):\n\
- Main documentation: {readme}\n\
- Additional docs: {docs}\n\
- Examples: {examples} (extensions, custom tools, SDK)\n\
- When reading pi docs or examples, resolve docs/... under Additional docs and examples/... \
under Examples, not the current working directory\n\
- When asked about: extensions (docs/extensions.md, examples/extensions/), themes \
(docs/themes.md), skills (docs/skills.md), prompt templates (docs/prompt-templates.md), TUI \
components (docs/tui.md), keybindings (docs/keybindings.md), SDK integrations (docs/sdk.md), \
custom providers (docs/custom-provider.md), adding models (docs/models.md), pi packages \
(docs/packages.md), environment variables (docs/environment-variables.md), MCP servers \
(docs/mcp.md), codemode scripts and non-LLM models such as classifiers and image models \
(docs/codemode.md)\n\
- When working on pi topics, read the docs and examples, and follow .md cross-references \
before implementing\n\
- Always read pi .md files completely and follow links to related docs (e.g., tui.md for TUI \
API details)"
    ))
}

/// Build the ordered, independently replaceable sections of the structured system prompt.
pub fn build_system_prompt_sections(input: &SystemPromptInput<'_>) -> SystemPromptSections {
    let mut sections = SystemPromptSections::default();

    // Extension-supplied custom sections are regex-validated (upstream
    // throws on invalid names; pir warns and skips them).
    let custom_sections: Vec<(String, String)> = input
        .custom_sections
        .iter()
        .filter(|(name, _)| {
            let valid = is_valid_section_name(name) && name != "preamble";
            if !valid {
                tracing::warn!("Invalid system prompt section name: {name}");
            }
            valid
        })
        .cloned()
        .collect();

    // Hidden declarations (upstream `hiddenTools`) stay active but are
    // reachable only through another tool: the tool list and rules leave them
    // out (a hidden reader still backs skills, via the `indirect` hint).
    let declared: Vec<ToolInfo> = input
        .tools
        .iter()
        .filter(|tool| !input.hidden_tools.iter().any(|name| name == &tool.name))
        .cloned()
        .collect();

    match &input.preamble_override {
        Some(custom) if !custom.is_empty() => {
            sections.insert(SystemPromptSection::Preamble, custom.clone());
        }
        _ => {
            sections.insert(SystemPromptSection::Preamble, DEFAULT_PREAMBLE.to_string());
            sections.insert(SystemPromptSection::Tools, render_tools_section(&declared));
            sections.insert(SystemPromptSection::Rules, render_rules_section(&declared));
            if let Some(docs) = render_docs_section(input.package_dir) {
                sections.insert(SystemPromptSection::Docs, docs);
            }
        }
    }

    if !input.append.is_empty() {
        sections.insert(SystemPromptSection::Addendum, input.append.join("\n\n"));
    }
    if !input.project_files.is_empty() {
        sections.insert(
            SystemPromptSection::ProjectContext,
            render_project_context(input.project_files),
        );
    }
    let skills_prompt = prompt_skills_text(input, &declared);
    if !skills_prompt.is_empty() {
        sections.insert(SystemPromptSection::Skills, skills_prompt);
    }
    sections.insert(
        SystemPromptSection::Cwd,
        input.cwd.to_string_lossy().replace('\\', "/"),
    );

    // Extension-supplied named sections are appended in order (upstream
    // iterates `Object.entries(customSections)` last).
    for (name, content) in custom_sections {
        if content.is_empty() {
            continue;
        }
        sections.insert(SystemPromptSection::Custom(name), content);
    }

    // Wrap every non-preamble section in an XML tag of the same name.
    let mut wrapped = SystemPromptSections::default();
    for (section, content) in sections.iter() {
        if *section == SystemPromptSection::Preamble {
            wrapped.insert(section.clone(), content.to_string());
        } else {
            let tag = section.tag();
            wrapped.insert(section.clone(), format!("<{tag}>\n{content}\n</{tag}>"));
        }
    }
    wrapped
}

/// The `skills` section text, or empty when there is nothing to render. A
/// hidden reader is still reachable through another tool, so skills stay but
/// the hint names no tool (upstream `skillFileReadTool`).
fn prompt_skills_text(input: &SystemPromptInput<'_>, declared: &[ToolInfo]) -> String {
    if !input
        .skills
        .iter()
        .any(|skill| !skill.disable_model_invocation)
    {
        return String::new();
    }
    let reads = |name: &str| declared.iter().any(|tool| tool.name == name);
    let active_reads = |name: &str| input.tools.iter().any(|tool| tool.name == name);
    let read_tool = if reads("read") {
        SkillFileReadTool::Read
    } else if reads("bash") {
        SkillFileReadTool::Bash
    } else if active_reads("read") || active_reads("bash") {
        SkillFileReadTool::Indirect
    } else {
        return String::new();
    };
    crate::skills::available_skills_xml(input.skills, read_tool)
        .trim()
        .to_string()
}

/// Build the system prompt text, rendered exactly as the transcript's system message
/// replays it: sections joined by a blank line, preamble untagged.
pub fn build_system_prompt(input: &SystemPromptInput<'_>) -> String {
    build_system_prompt_sections(input)
        .iter()
        .map(|(_, content)| content.to_string())
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// Diff the sections the model currently has (replayed from the transcript, so never
/// null) against the desired ones. Returns the patch of changed/removed sections, or an
/// empty `SectionChanges` when nothing changed.
pub fn diff_sections(
    previous: &SystemPromptSections,
    current: &SystemPromptSections,
) -> SectionChanges {
    let mut changes = SectionChanges::default();
    for (section, text) in current.iter() {
        if previous.get(section) != Some(text) {
            changes.changed.push((section.clone(), text.to_string()));
        }
    }
    for (section, _) in previous.iter() {
        if current.get(section).is_none() {
            changes.removed.push(section.clone());
        }
    }
    changes
}
