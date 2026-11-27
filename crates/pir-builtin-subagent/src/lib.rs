//! Independent, host-neutral core of the `subagent` builtin.
//!
//! The child agent run (spawning `pir`, an `AgentSession`, tool selection,
//! model catalog, worktree leases and process executors) stays host-side.
//! This crate owns the pure discovery and result-formatting rules:
//!
//! - agent markdown discovery: frontmatter splitting, tool-list
//!   normalization, directory loading with a caller-supplied size cap, and the
//!   nearest `.pi/agents` lookup;
//! - per-task result aggregation: failure classification, final-output
//!   extraction, bounded transcript retention and the parallel summary.
//!
//! It depends on `pir-plugin-api` and third-party crates only — never on
//! `pir-core`. Messages are handled as `serde_json::Value` so the host can pass
//! its own transcript rows without this crate knowing their types.

#![deny(unsafe_code)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde_json::Value as JsonValue;

use pir_plugin_api::{BuiltinPlugin, PluginInfo};

/// Project config directory name (`.pi`).
pub const CONFIG_DIR_NAME: &str = ".pi";

/// Cap of one per-task output block in the parallel summary (50 KiB).
pub const PER_TASK_OUTPUT_CAP: usize = 50 * 1024;

/// Per-string cap inside a retained child transcript block.
pub const RETAINED_TRANSCRIPT_TEXT_CHARS: usize = 8 * 1024;

/// Default cap for one agent markdown file when the host has no setting.
pub const DEFAULT_MAX_AGENT_FILE_BYTES: u64 = 128 * 1024;

// ---------------------------------------------------------------------------
// Discovery
// ---------------------------------------------------------------------------

/// Which agent directories a dispatch uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentScope {
    User,
    Project,
    Both,
}

impl AgentScope {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Project => "project",
            Self::Both => "both",
        }
    }

    /// Parse the wire value; `None` for anything else.
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "user" => Some(Self::User),
            "project" => Some(Self::Project),
            "both" => Some(Self::Both),
            _ => None,
        }
    }
}

/// Where a discovered agent came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentSource {
    User,
    Project,
}

impl AgentSource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Project => "project",
        }
    }
}

/// One discovered agent.
#[derive(Debug, Clone)]
pub struct AgentConfig {
    pub name: String,
    pub description: String,
    /// Frontmatter `tools` allowlist; `None` = the host's default set.
    pub tools: Option<Vec<String>>,
    /// Frontmatter `model` override (`"provider/id"`).
    pub model: Option<String>,
    /// Markdown body = the agent's system prompt.
    pub system_prompt: String,
    pub source: AgentSource,
    pub file_path: PathBuf,
}

/// Discovery result.
#[derive(Debug, Clone, Default)]
pub struct Discovery {
    pub agents: Vec<AgentConfig>,
    pub project_agents_dir: Option<PathBuf>,
}

/// Raw frontmatter parsed from one agent markdown file.
#[derive(Debug, Clone, Default)]
pub struct AgentFrontmatter {
    pub name: Option<String>,
    pub description: Option<String>,
    pub tools: Option<Vec<String>>,
    pub model: Option<String>,
    pub body: String,
}

/// Normalize a frontmatter `tools` value to a list of tool names.
///
/// Both spellings are valid YAML and both are in use:
///
/// ```yaml
/// tools: read, bash        # string
/// tools: [read, bash]      # array
/// ```
///
/// Anything else yields no tools rather than throwing: a single bad file must
/// not take down every other agent in the same directory.
pub fn parse_tool_list(value: &serde_yaml::Value) -> Option<Vec<String>> {
    let raw: Vec<String> = match value {
        serde_yaml::Value::Sequence(items) => items
            .iter()
            .filter_map(|item| item.as_str().map(str::to_owned))
            .collect(),
        serde_yaml::Value::String(text) => text.split(',').map(str::to_owned).collect(),
        _ => Vec::new(),
    };
    let tools: Vec<String> = raw
        .into_iter()
        .map(|tool| tool.trim().to_owned())
        .filter(|tool| !tool.is_empty())
        .collect();
    (!tools.is_empty()).then_some(tools)
}

/// Split a markdown file into its YAML frontmatter block and body.
pub fn split_frontmatter(text: &str) -> Option<(&str, &str)> {
    let normalized = text.strip_prefix('\u{feff}').unwrap_or(text);
    let rest = normalized
        .strip_prefix("---\r\n")
        .or_else(|| normalized.strip_prefix("---\n"))?;
    let mut offset = 0usize;
    for line in rest.split_inclusive('\n') {
        if line.trim_end_matches(['\r', '\n']) == "---" {
            return Some((&rest[..offset], &rest[offset + line.len()..]));
        }
        offset += line.len();
    }
    None
}

/// Parse one agent markdown file. Files without string `name` and
/// `description` frontmatter are skipped (upstream discovery rule).
pub fn parse_agent_markdown(text: &str) -> Option<AgentFrontmatter> {
    let (yaml, body) = split_frontmatter(text)?;
    let value: serde_yaml::Value = serde_yaml::from_str(yaml).ok()?;
    let mapping = value.as_mapping()?;
    let string_field = |key: &str| -> Option<String> {
        mapping
            .get(serde_yaml::Value::String(key.to_owned()))
            .and_then(serde_yaml::Value::as_str)
            .map(str::to_owned)
    };
    let name = string_field("name")?;
    let description = string_field("description")?;
    let tools = mapping
        .get(serde_yaml::Value::String("tools".to_owned()))
        .and_then(parse_tool_list);
    let model = string_field("model");
    Some(AgentFrontmatter {
        name: Some(name),
        description: Some(description),
        tools,
        model,
        body: body.to_owned(),
    })
}

/// Load every agent markdown in `dir`. Unreadable entries, oversized files and
/// files without the required frontmatter are skipped silently.
pub fn load_agents_from_dir(
    dir: &Path,
    source: AgentSource,
    max_file_bytes: u64,
) -> Vec<AgentConfig> {
    let mut agents = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return agents;
    };
    // Sorted for deterministic ordering across filesystems.
    let mut paths: Vec<PathBuf> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "md") && path.is_file())
        .collect();
    paths.sort();
    for path in paths {
        let size = std::fs::metadata(&path).map(|meta| meta.len()).unwrap_or(0);
        if size > max_file_bytes {
            continue;
        }
        let Ok(content) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Some(frontmatter) = parse_agent_markdown(&content) else {
            continue;
        };
        let (Some(name), Some(description)) = (frontmatter.name, frontmatter.description) else {
            continue;
        };
        agents.push(AgentConfig {
            name,
            description,
            tools: frontmatter.tools,
            model: frontmatter.model,
            system_prompt: frontmatter.body,
            source,
            file_path: path,
        });
    }
    agents
}

/// The nearest `.pi/agents` directory walking up from `cwd`.
pub fn find_nearest_project_agents_dir(cwd: &Path) -> Option<PathBuf> {
    let mut current = cwd.to_path_buf();
    loop {
        let candidate = current.join(CONFIG_DIR_NAME).join("agents");
        if candidate.is_dir() {
            return Some(candidate);
        }
        if !current.pop() {
            return None;
        }
    }
}

/// Parse the shipped default agents from `markdowns`.
///
/// The presets themselves are the host's embedded assets (`include_str!` of its
/// own `assets/agents/*.md`); this only turns that text into [`AgentConfig`]s,
/// so the embedding decision stays with the host that ships the files.
pub fn builtin_agents_from(markdowns: &[&str]) -> Vec<AgentConfig> {
    markdowns
        .iter()
        .filter_map(|markdown| {
            let frontmatter = parse_agent_markdown(markdown)?;
            let name = frontmatter.name?;
            let description = frontmatter.description?;
            let file_path = PathBuf::from(format!("<builtin>/agents/{name}.md"));
            Some(AgentConfig {
                name,
                description,
                tools: frontmatter.tools,
                model: frontmatter.model,
                system_prompt: frontmatter.body,
                source: AgentSource::User,
                file_path,
            })
        })
        .collect()
}

/// Discover agents for one dispatch. Agents are re-read on every call so
/// mid-session edits take effect; in `both` scope project agents override
/// same-named user agents.
///
/// `defaults` are the host's built-in agent definitions (an empty slice when
/// the host ships none); they are used only when no agent file exists in
/// either scope.
pub fn discover_agents(
    cwd: &Path,
    scope: AgentScope,
    user_agents_dir: &Path,
    max_file_bytes: u64,
    defaults: Vec<AgentConfig>,
) -> Discovery {
    let project_agents_dir = find_nearest_project_agents_dir(cwd);
    let user_agents = if scope == AgentScope::Project {
        Vec::new()
    } else {
        load_agents_from_dir(user_agents_dir, AgentSource::User, max_file_bytes)
    };
    let project_agents = match (scope, &project_agents_dir) {
        (AgentScope::User, _) | (_, None) => Vec::new(),
        (_, Some(dir)) => load_agents_from_dir(dir, AgentSource::Project, max_file_bytes),
    };

    let mut agents: Vec<AgentConfig> =
        if scope != AgentScope::Project && user_agents.is_empty() && project_agents.is_empty() {
            defaults
        } else {
            Vec::new()
        };
    let mut upsert = |agent: AgentConfig| {
        if let Some(existing) = agents.iter_mut().find(|a| a.name == agent.name) {
            *existing = agent;
        } else {
            agents.push(agent);
        }
    };
    for agent in user_agents {
        upsert(agent);
    }
    for agent in project_agents {
        upsert(agent);
    }

    Discovery {
        agents,
        project_agents_dir,
    }
}

/// `"name (source)"` list used by the invalid-parameters texts.
pub fn agent_list(agents: &[AgentConfig]) -> String {
    if agents.is_empty() {
        return "none".to_owned();
    }
    agents
        .iter()
        .map(|agent| format!("{} ({})", agent.name, agent.source.as_str()))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Quoted `"name"` list used by the unknown-agent failure text.
pub fn agent_names_quoted(agents: &[AgentConfig]) -> String {
    if agents.is_empty() {
        return "none".to_owned();
    }
    agents
        .iter()
        .map(|agent| format!("\"{}\"", agent.name))
        .collect::<Vec<_>>()
        .join(", ")
}

// ---------------------------------------------------------------------------
// Results
// ---------------------------------------------------------------------------

/// Aggregated child-run usage.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct UsageTotals {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    pub cost: f64,
    pub context_tokens: u64,
    pub turns: u64,
}

/// Worktree metadata for one task. `merged` means the changes reached the main
/// workspace (or there were none); `cleaned` means the lease was removed.
#[derive(Debug, Clone)]
pub struct WorktreeOutcome {
    pub repo: String,
    pub path: String,
    pub branch: String,
    pub base: String,
    pub merged: bool,
    pub cleaned: bool,
    pub error: Option<String>,
}

/// One task's outcome.
#[derive(Debug, Clone)]
pub struct SingleResult {
    pub agent: String,
    pub agent_source: &'static str,
    pub task: String,
    /// Full child transcript as JSON message rows (host-neutral).
    pub messages: Vec<JsonValue>,
    pub usage: UsageTotals,
    pub model: Option<String>,
    pub stop_reason: Option<String>,
    pub error_message: Option<String>,
    /// The child run error text (the stderr equivalent).
    pub stderr: Option<String>,
    pub step: Option<usize>,
    /// Parallel placeholder still running.
    pub pending: bool,
    pub worktree: Option<WorktreeOutcome>,
    /// `inline` or `process`.
    pub executor: &'static str,
}

impl SingleResult {
    /// A failed result without a child run (unknown agent, unresolvable
    /// model, ...).
    pub fn failed(agent: &str, task: &str, message: String, step: Option<usize>) -> Self {
        Self {
            agent: agent.to_owned(),
            agent_source: "unknown",
            task: task.to_owned(),
            messages: Vec::new(),
            usage: UsageTotals::default(),
            model: None,
            stop_reason: Some("error".to_owned()),
            error_message: Some(message.clone()),
            stderr: Some(message),
            step,
            pending: false,
            worktree: None,
            executor: "inline",
        }
    }

    /// A still-running chain-step placeholder.
    pub fn placeholder_for_step(agent: &str, task: &str, step: usize) -> Self {
        Self {
            step: Some(step),
            ..Self::placeholder(agent, task)
        }
    }

    /// A still-running parallel placeholder.
    pub fn placeholder(agent: &str, task: &str) -> Self {
        Self {
            agent: agent.to_owned(),
            agent_source: "unknown",
            task: task.to_owned(),
            messages: Vec::new(),
            usage: UsageTotals::default(),
            model: None,
            stop_reason: None,
            error_message: None,
            stderr: None,
            step: None,
            pending: true,
            worktree: None,
            executor: "inline",
        }
    }
}

/// Cap every string inside a JSON value.
fn cap_json_strings(value: &mut JsonValue, max_chars: usize) {
    match value {
        JsonValue::String(text) => {
            if text.chars().count() > max_chars {
                *text = truncate_chars_with_marker(text, max_chars);
            }
        }
        JsonValue::Array(items) => {
            for item in items {
                cap_json_strings(item, max_chars);
            }
        }
        JsonValue::Object(map) => {
            for item in map.values_mut() {
                cap_json_strings(item, max_chars);
            }
        }
        _ => {}
    }
}

/// Truncate to at most `max` characters, appending a marker when cut.
pub fn truncate_chars_with_marker(text: &str, max: usize) -> String {
    match text.char_indices().nth(max) {
        Some((byte_index, _)) => format!("{}…", text[..byte_index].trim_end()),
        None => text.to_owned(),
    }
}

/// Serialize the child transcript for details with a bounded size. Returns the
/// retained messages as one JSON array and how many messages were dropped.
pub fn bounded_messages_json(messages: &[JsonValue]) -> (JsonValue, usize) {
    let last_assistant = messages
        .iter()
        .rposition(|message| message.get("role").and_then(JsonValue::as_str) == Some("assistant"));
    let mut retained: Vec<JsonValue> = Vec::with_capacity(messages.len());
    for (index, message) in messages.iter().enumerate() {
        let mut value = message.clone();
        let role = value
            .get("role")
            .and_then(JsonValue::as_str)
            .unwrap_or_default()
            .to_owned();
        let keep_full_text = Some(index) == last_assistant;
        if let Some(blocks) = value.get_mut("content").and_then(JsonValue::as_array_mut) {
            blocks.retain_mut(|block| {
                match block.get("type").and_then(JsonValue::as_str) {
                    // Thinking/image blocks are not transcript evidence.
                    Some("thinking") | Some("image") => false,
                    Some("toolCall") => {
                        if let Some(arguments) = block.get_mut("arguments") {
                            cap_json_strings(arguments, RETAINED_TRANSCRIPT_TEXT_CHARS);
                        }
                        true
                    }
                    Some("text") if !keep_full_text => {
                        if let Some(text) = block.get("text").and_then(JsonValue::as_str) {
                            block["text"] = JsonValue::String(truncate_chars_with_marker(
                                text,
                                RETAINED_TRANSCRIPT_TEXT_CHARS,
                            ));
                        }
                        true
                    }
                    _ => true,
                }
            });
        }
        if role == "toolResult" {
            if let Some(object) = value.as_object_mut() {
                // Nested results are the recursion amplifier; no transcript
                // renderer reads them.
                object.remove("details");
                object.remove("structuredContent");
                object.remove("nestedCalls");
            }
        }
        retained.push(value);
    }

    // Hard per-result budget. Oldest non-system messages are dropped first:
    // the child's system message carries its declared tool set and
    // instructions, and the final message is never dropped.
    //
    // `sizes` is indexed in lockstep with `retained`, so it is removed from as
    // well: keeping the original index vector would (a) leave `sizes[drop_at]`
    // pointing at the wrong message once an earlier one was removed, so
    // `total` stops matching the actual payload and the loop never converges,
    // and (b) let `drop_at` advance past the end when the tail is a run of
    // system/developer messages, making `retained[drop_at]` panic. `drop_at`
    // is re-clamped every iteration for the same reason.
    let mut sizes: Vec<usize> = retained
        .iter()
        .map(|value| serde_json::to_string(value).map(|t| t.len()).unwrap_or(0))
        .collect();
    let mut total: usize = sizes.iter().sum::<usize>() + retained.len().saturating_sub(1);
    let mut dropped = 0usize;
    let mut drop_at = 0usize;
    while total > PER_TASK_OUTPUT_CAP && retained.len() - dropped > 1 {
        if drop_at >= retained.len() {
            break;
        }
        let is_system = retained[drop_at]
            .get("role")
            .and_then(JsonValue::as_str)
            .is_some_and(|role| role == "system" || role == "developer");
        if is_system {
            drop_at += 1;
            continue;
        }
        total = total.saturating_sub(sizes[drop_at] + 1);
        retained.remove(drop_at);
        sizes.remove(drop_at);
        dropped += 1;
    }
    (JsonValue::Array(retained), dropped)
}

/// Whether a result counts as failed. `length` is output truncation: the child
/// did not finish, so it must not count as a successful worker or a passing
/// reviewer.
pub fn is_failed(result: &SingleResult) -> bool {
    matches!(
        result.stop_reason.as_deref(),
        Some("error") | Some("aborted") | Some("length")
    ) || result.stderr.is_some()
}

/// The last assistant message's first text block.
pub fn get_final_output(messages: &[JsonValue]) -> Option<String> {
    messages.iter().rev().find_map(|message| {
        if message.get("role").and_then(JsonValue::as_str) != Some("assistant") {
            return None;
        }
        message
            .get("content")
            .and_then(JsonValue::as_array)?
            .iter()
            .find_map(|block| {
                (block.get("type").and_then(JsonValue::as_str) == Some("text"))
                    .then(|| {
                        block
                            .get("text")
                            .and_then(JsonValue::as_str)
                            .map(str::to_owned)
                    })
                    .flatten()
            })
    })
}

/// Model-visible output of one result with the upstream error-text precedence:
/// errorMessage → stderr → final output → "(no output)".
pub fn result_output(result: &SingleResult) -> String {
    if is_failed(result) {
        return result
            .error_message
            .clone()
            .or_else(|| result.stderr.clone())
            .or_else(|| get_final_output(&result.messages))
            .unwrap_or_else(|| "(no output)".to_owned());
    }
    get_final_output(&result.messages).unwrap_or_else(|| "(no output)".to_owned())
}

/// UTF-8-bounded 50 KiB truncation of parallel per-task output.
pub fn truncate_parallel_output(output: &str) -> String {
    let byte_length = output.len();
    if byte_length <= PER_TASK_OUTPUT_CAP {
        return output.to_owned();
    }
    let mut truncated: String = output
        .char_indices()
        .map(|(index, _)| index)
        .take_while(|index| *index <= PER_TASK_OUTPUT_CAP)
        .last()
        .and_then(|index| output.get(..index))
        .unwrap_or_default()
        .to_owned();
    while truncated.len() > PER_TASK_OUTPUT_CAP {
        truncated.pop();
    }
    let omitted = byte_length - truncated.len();
    format!(
        "{truncated}\n\n[Output truncated: {omitted} bytes omitted. Full output preserved in tool details.]"
    )
}

/// Replace every `{previous}` placeholder with the prior chain step's output.
pub fn apply_previous(task: &str, previous: &str) -> String {
    task.replace("{previous}", previous)
}

/// The aggregate parallel result text.
pub fn aggregate_parallel_text(results: &[SingleResult]) -> String {
    let success = results
        .iter()
        .filter(|result| !result.pending && !is_failed(result))
        .count();
    let blocks: Vec<String> = results
        .iter()
        .map(|result| {
            let status = if result.pending {
                "running".to_owned()
            } else if is_failed(result) {
                let reason = result
                    .stop_reason
                    .as_deref()
                    .filter(|reason| *reason != "stop")
                    .map(|reason| format!(" ({reason})"))
                    .unwrap_or_default();
                format!("failed{reason}")
            } else {
                "completed".to_owned()
            };
            format!(
                "### [{}] {}\n\n{}",
                result.agent,
                status,
                truncate_parallel_output(&result_output(result))
            )
        })
        .collect();
    format!(
        "Parallel: {success}/{} succeeded\n\n{}",
        results.len(),
        blocks.join("\n\n---\n\n")
    )
}

/// Count results by agent for the details summary.
pub fn results_by_agent(results: &[SingleResult]) -> BTreeMap<String, usize> {
    let mut counts = BTreeMap::new();
    for result in results {
        *counts.entry(result.agent.clone()).or_insert(0) += 1;
    }
    counts
}

/// Minimal protocol-facing shell for the subagent builtin.
pub struct SubagentPlugin {
    info: PluginInfo,
}

impl SubagentPlugin {
    pub fn new() -> Self {
        Self {
            info: PluginInfo::builtin(PLUGIN_NAME),
        }
    }
}

impl BuiltinPlugin for SubagentPlugin {
    fn info(&self) -> &PluginInfo {
        &self.info
    }
}

pir_plugin_api::builtin_plugin_shell! {
    plugin = SubagentPlugin,
    name = "subagent",
    factory = new_subagent_plugin,
}
