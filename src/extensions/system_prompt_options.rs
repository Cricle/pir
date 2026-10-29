//! Full upstream `BuildSystemPromptOptions` across the extension boundary
//! (`core/system-prompt.ts:9-39`, normalized shape `:39-50`; the event field
//! `before_agent_start.systemPromptOptions`, `extensions/types.ts:932-938`).
//!
//! The session host pushes the same option object it derives its current
//! system prompt from into the JS `before_agent_start` event. A handler may
//! mutate `sections`, `selectedTools`, `toolSnippets`, `skills`, ... in place
//! (upstream `runner.ts:1431-1470` shares ONE mutable object across handlers
//! and re-renders `event.systemPrompt` from it); after the dispatch the host
//! renders the mutated object back into this run's system prompt.
//!
//! The renderer reuses the ordered section builder in
//! [`crate::system_prompt`], so a prompt rendered from JS-mutated options is
//! byte-identical to one built by the session itself for the same inputs.
//! Engine-free (no QuickJS dependency): the OptionsData shape is also used by
//! the native-plugin bridge and by tests.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;

use crate::skills::SkillInfo;
use crate::system_prompt::{build_system_prompt, SystemPromptInput, ToolInfo};

/// Upstream `normalizeBuildSystemPromptOptions`'s default `selectedTools`
/// (`system-prompt.ts:64`).
pub const DEFAULT_SELECTED_TOOLS: [&str; 4] = ["read", "bash", "edit", "write"];

/// A context file in the option object (upstream
/// `BuildSystemPromptOptions.contextFiles`, `{path, content}`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ContextFileOption {
    #[serde(default)]
    pub path: String,
    #[serde(default)]
    pub content: String,
}

/// The upstream `SourceInfo` fields a skill carries (pir only tracks the
/// path; the shape keeps extensions that read `.sourceInfo.path` working).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct SkillSourceInfo {
    #[serde(default)]
    pub path: String,
}

/// One `skills` entry in the option object, in the upstream `Skill` shape
/// (`core/skills.ts:74-81`: `{name, description, filePath, baseDir,
/// sourceInfo, disableModelInvocation}`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct SkillOption {
    pub name: String,
    pub description: String,
    pub file_path: String,
    pub base_dir: String,
    pub source_info: SkillSourceInfo,
    pub disable_model_invocation: bool,
}

impl SkillOption {
    /// The upstream projection of a discovered skill: `filePath` is the skill
    /// file, `baseDir` its directory, `sourceInfo.path` the same file.
    pub fn from_skill(skill: &SkillInfo) -> Self {
        let file_path = skill.path.display().to_string();
        let base_dir = skill
            .path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."))
            .display()
            .to_string();
        Self {
            name: skill.name.clone(),
            description: skill.description.clone(),
            source_info: SkillSourceInfo {
                path: file_path.clone(),
            },
            file_path,
            base_dir,
            disable_model_invocation: skill.disable_model_invocation,
        }
    }

    /// Back to the builder's [`SkillInfo`]. A JS handler may rename or add a
    /// skill: the path is `filePath`, else `<baseDir>/SKILL.md`, else the
    /// source path — an entry with no usable path is not renderable and is
    /// dropped by the caller.
    pub fn to_skill(&self) -> Option<SkillInfo> {
        let path = if !self.file_path.is_empty() {
            PathBuf::from(&self.file_path)
        } else if !self.base_dir.is_empty() {
            PathBuf::from(&self.base_dir).join("SKILL.md")
        } else if !self.source_info.path.is_empty() {
            PathBuf::from(&self.source_info.path)
        } else {
            return None;
        };
        Some(SkillInfo {
            name: self.name.clone(),
            description: self.description.clone(),
            path,
            disable_model_invocation: self.disable_model_invocation,
        })
    }
}

/// Upstream `NormalizedBuildSystemPromptOptions` (the mutable event field).
///
/// `sections` keeps insertion order (`serde_json` runs with
/// `preserve_order`), matching the reference object iteration order: custom
/// sections are appended after the built-ins in registration order.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct SystemPromptOptions {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub custom_prompt: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub force_system_prompt: Option<String>,
    pub selected_tools: Vec<String>,
    pub hidden_tools: Vec<String>,
    pub tool_snippets: BTreeMap<String, String>,
    pub tool_guidelines: BTreeMap<String, Vec<String>>,
    pub prompt_guidelines: Vec<String>,
    pub append_system_prompt: String,
    pub sections: serde_json::Map<String, JsonValue>,
    pub cwd: String,
    pub context_files: Vec<ContextFileOption>,
    pub skills: Vec<SkillOption>,
}

impl Default for SystemPromptOptions {
    fn default() -> Self {
        Self {
            custom_prompt: None,
            force_system_prompt: None,
            selected_tools: DEFAULT_SELECTED_TOOLS
                .iter()
                .map(|name| (*name).to_owned())
                .collect(),
            hidden_tools: Vec::new(),
            tool_snippets: BTreeMap::new(),
            tool_guidelines: BTreeMap::new(),
            prompt_guidelines: Vec::new(),
            append_system_prompt: String::new(),
            sections: serde_json::Map::new(),
            cwd: String::new(),
            context_files: Vec::new(),
            skills: Vec::new(),
        }
    }
}

impl SystemPromptOptions {
    /// Parse the wire object. Unknown fields are ignored (extensions may add
    /// their own bookkeeping); a missing `selectedTools` takes the upstream
    /// default, an explicitly empty list stays empty.
    pub fn from_json(value: &JsonValue) -> Self {
        let mut options: Self = serde_json::from_value(value.clone()).unwrap_or_default();
        if value.get("selectedTools").is_none() {
            options.selected_tools = DEFAULT_SELECTED_TOOLS
                .iter()
                .map(|name| (*name).to_owned())
                .collect();
        }
        options
    }

    /// Serialize back to the event wire shape (every normalized field present,
    /// exactly like upstream's normalized object).
    pub fn to_json(&self) -> JsonValue {
        serde_json::to_value(self).unwrap_or(JsonValue::Null)
    }

    /// Whether a handler replaced the complete prompt
    /// (`forceSystemPrompt`, upstream `buildSystemPromptState`).
    pub fn forced_prompt(&self) -> Option<&str> {
        self.force_system_prompt.as_deref()
    }

    /// Render the complete prompt from these options with the ordered section
    /// builder (upstream `buildSystemPrompt`, `system-prompt.ts:208`).
    pub fn render(&self, package_dir: Option<&Path>) -> String {
        if let Some(forced) = self.forced_prompt() {
            return forced.to_owned();
        }
        // `selectedTools` order is the reference order for the tools and
        // rules sections. Snippet/guideline maps are keyed by name; a tool
        // without a snippet is not listed in `<tools>` but still contributes
        // its guidelines (upstream buildRules iterates selectedTools).
        let mut tools: Vec<ToolInfo> = self
            .selected_tools
            .iter()
            .map(|name| ToolInfo {
                name: name.clone(),
                description: self.tool_snippets.get(name).cloned().unwrap_or_default(),
                snippet: self.tool_snippets.get(name).cloned(),
                guidelines: self.tool_guidelines.get(name).cloned().unwrap_or_default(),
            })
            .collect();
        // `promptGuidelines` are appended after every selected tool's
        // guidelines and before the fixed rules. The builder exposes no
        // separate input, so they ride a synthetic entry whose name cannot
        // match a real tool (it is never listed: it has no snippet, and the
        // builder's `bash`/`grep`/... checks are exact-name comparisons).
        if !self.prompt_guidelines.is_empty() {
            tools.push(ToolInfo {
                name: String::new(),
                description: String::new(),
                snippet: None,
                guidelines: self.prompt_guidelines.clone(),
            });
        }
        let project_files: Vec<(PathBuf, String)> = self
            .context_files
            .iter()
            .map(|file| (PathBuf::from(&file.path), file.content.clone()))
            .collect();
        let skills: Vec<SkillInfo> = self
            .skills
            .iter()
            .filter_map(SkillOption::to_skill)
            .collect();
        let custom_sections: Vec<(String, String)> = self
            .sections
            .iter()
            .filter_map(|(name, value)| {
                value
                    .as_str()
                    .map(|text| (name.clone(), text.to_owned()))
                    .filter(|(_, text)| !text.is_empty())
            })
            .collect();
        build_system_prompt(&SystemPromptInput {
            preamble_override: self.custom_prompt.clone(),
            append: if self.append_system_prompt.is_empty() {
                Vec::new()
            } else {
                vec![self.append_system_prompt.clone()]
            },
            tools: &tools,
            hidden_tools: &self.hidden_tools,
            project_files: &project_files,
            skills: &skills,
            custom_sections,
            cwd: Path::new(&self.cwd),
            package_dir,
        })
    }
}

/// Render a raw options object (the host fn behind
/// `ctx.getSystemPrompt()` / `event.systemPrompt` re-rendering). `None` on a
/// shape that is not a JSON object.
pub fn render_json(value: &JsonValue, package_dir: Option<&Path>) -> Option<String> {
    value
        .is_object()
        .then(|| SystemPromptOptions::from_json(value).render(package_dir))
}
