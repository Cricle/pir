use super::*;

/// Which agent directories a dispatch uses (upstream `AgentScope`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentScope {
    User,
    Project,
    Both,
}

impl AgentScope {
    pub fn as_str(self) -> &'static str {
        match self {
            AgentScope::User => "user",
            AgentScope::Project => "project",
            AgentScope::Both => "both",
        }
    }

    /// Parse the wire value; `None` for anything but `user`/`project`/`both`.
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "user" => Some(AgentScope::User),
            "project" => Some(AgentScope::Project),
            "both" => Some(AgentScope::Both),
            _ => None,
        }
    }
}

/// Where a discovered agent came from (upstream `AgentConfig.source`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentSource {
    User,
    Project,
}

impl AgentSource {
    pub fn as_str(self) -> &'static str {
        match self {
            AgentSource::User => "user",
            AgentSource::Project => "project",
        }
    }
}

/// One discovered agent (upstream `AgentConfig`).
#[derive(Debug, Clone)]
pub struct AgentConfig {
    pub name: String,
    pub description: String,
    /// Frontmatter `tools` allowlist; `None`/empty = the pir default set.
    pub tools: Option<Vec<String>>,
    /// Frontmatter `model` override (`"provider/id"`).
    pub model: Option<String>,
    /// Markdown body = the agent's system prompt.
    pub system_prompt: String,
    pub source: AgentSource,
    pub file_path: PathBuf,
}

/// Discovery result (upstream `AgentDiscoveryResult`).
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

/// Normalize a frontmatter `tools` value to a list of tool names. Both
/// spellings are valid YAML and both are in use:
///
/// ```yaml
/// tools: read, bash        # string
/// tools: [read, bash]      # array
/// ```
///
/// Anything else (a number, a map, a nested list) yields no tools rather than
/// throwing: this runs inside agent discovery, where a single bad file must
/// not take down every other agent in the same directory (upstream
/// `parseToolList`).
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
        .iter()
        .map(|tool| tool.trim().to_owned())
        .filter(|tool| !tool.is_empty())
        .collect();
    if tools.is_empty() {
        None
    } else {
        Some(tools)
    }
}

/// Split a markdown file into its YAML frontmatter block and body. Returns
/// `None` when there is no frontmatter delimiting.
pub fn split_frontmatter(text: &str) -> Option<(&str, &str)> {
    let normalized = text.strip_prefix('\u{feff}').unwrap_or(text);
    let rest = normalized
        .strip_prefix("---\r\n")
        .or_else(|| normalized.strip_prefix("---\n"))?;
    let mut offset = 0usize;
    for line in rest.split_inclusive('\n') {
        let delimiter = line.trim_end_matches(['\r', '\n']);
        if delimiter == "---" {
            let yaml = &rest[..offset];
            let body = &rest[offset + line.len()..];
            return Some((yaml, body));
        }
        offset += line.len();
    }
    None
}

/// Parse one agent markdown file (frontmatter + body). Files without string
/// `name` and `description` frontmatter are skipped (upstream discovery
/// rule).
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

/// Load every agent markdown in `dir` (upstream `loadAgentsFromDir`); unread
/// entries and files without the required frontmatter are skipped silently.
pub fn load_agents_from_dir(dir: &Path, source: AgentSource) -> Vec<AgentConfig> {
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

/// The nearest `.pi/agents` directory walking up from `cwd` (upstream
/// `findNearestProjectAgentsDir`).
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

/// Discover agents for one dispatch (upstream `discoverAgents`). Agents are
/// re-read on every call so mid-session edits take effect. In `both` scope
/// project agents override same-named user agents.
pub fn discover_agents(cwd: &Path, scope: AgentScope, user_agents_dir: &Path) -> Discovery {
    let project_agents_dir = find_nearest_project_agents_dir(cwd);
    let user_agents = if scope == AgentScope::Project {
        Vec::new()
    } else {
        load_agents_from_dir(user_agents_dir, AgentSource::User)
    };
    let project_agents = match (scope, &project_agents_dir) {
        (AgentScope::User, _) | (_, None) => Vec::new(),
        (_, Some(dir)) => load_agents_from_dir(dir, AgentSource::Project),
    };

    let mut agents: Vec<AgentConfig> = Vec::new();
    let mut upsert = |agent: AgentConfig| {
        if let Some(existing) = agents.iter_mut().find(|a| a.name == agent.name) {
            *existing = agent;
        } else {
            agents.push(agent);
        }
    };
    if scope == AgentScope::Both {
        for agent in user_agents {
            upsert(agent);
        }
        for agent in project_agents {
            upsert(agent);
        }
    } else if scope == AgentScope::User {
        agents = user_agents;
    } else {
        agents = project_agents;
    }

    Discovery {
        agents,
        project_agents_dir,
    }
}

/// `"name (source)"` list used by the invalid-parameters/aggregate texts.
pub(super) fn agent_list(agents: &[AgentConfig]) -> String {
    if agents.is_empty() {
        return "none".to_owned();
    }
    agents
        .iter()
        .map(|agent| format!("{} ({})", agent.name, agent.source.as_str()))
        .collect::<Vec<_>>()
        .join(", ")
}

/// `"name"` list used by the unknown-agent failure (upstream
/// `"${a.name}"` quoted names joined with ", "); the `name (source)` format
/// stays in the invalid-parameters message only.
pub(super) fn agent_names_quoted(agents: &[AgentConfig]) -> String {
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
