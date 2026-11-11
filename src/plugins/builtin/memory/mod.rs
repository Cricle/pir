//! `builtin:memory` — the cm-memory engine as a native pir plugin.
//!
//! Registers the `memory` tool, the `/memory` command, the input auto-recall
//! hook and the optional stable system-prompt brief. Every cm operation runs
//! through [`cm_memory::Cm`] on the blocking pool; no external `cm` process is
//! spawned.

mod prompt;
mod service;
mod tool;

use std::path::PathBuf;
use std::sync::Arc;

use serde_json::{Map, Value};

use crate::extensions::CommandOutcome;
use crate::plugins::plugin::{
    AgentPlugin, CommandBackend, CommandHandler, EventInterest, PluginEvent, PluginEventOutcome,
    PluginFuture, RegistrationContext,
};
use crate::plugins::source::PluginInfo;

pub use service::{MemoryConfig, MemoryService, MemoryStoreMode, MemoryWriteMode};
pub use tool::{MemoryTool, TOOL_NAME};

pub const PLUGIN_NAME: &str = "memory";
pub const COMMAND_NAME: &str = "memory";

pub struct MemoryPlugin {
    info: PluginInfo,
    service: Arc<MemoryService>,
}

impl MemoryPlugin {
    pub fn new() -> Self {
        Self::with_service(Arc::new(MemoryService::new(
            crate::ai::providers::agent_dir(),
            std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
        )))
    }

    pub fn with_paths(agent_dir: impl Into<PathBuf>, cwd: impl Into<PathBuf>) -> Self {
        Self::with_service(Arc::new(MemoryService::new(agent_dir, cwd)))
    }

    /// Test constructor: one explicit cm file, no session/cwd store
    /// discovery. Settings still resolve from the process agent directory
    /// unless the caller uses [`MemoryPlugin::with_paths`].
    pub fn with_store_path(path: impl Into<PathBuf>, cwd: impl Into<PathBuf>) -> Self {
        Self::with_service(Arc::new(MemoryService::with_store_path(path, cwd)))
    }

    pub fn with_service(service: Arc<MemoryService>) -> Self {
        let mut info = PluginInfo::builtin(PLUGIN_NAME);
        info.replaceable =
            super::builtin_extension_def(PLUGIN_NAME).is_some_and(|def| def.replaceable);
        Self { info, service }
    }

    pub fn new_plugin() -> Arc<dyn AgentPlugin> {
        Arc::new(Self::new())
    }

    pub fn service(&self) -> &Arc<MemoryService> {
        &self.service
    }
}

impl Default for MemoryPlugin {
    fn default() -> Self {
        Self::new()
    }
}

impl AgentPlugin for MemoryPlugin {
    fn info(&self) -> &PluginInfo {
        &self.info
    }

    fn register(&self, ctx: &mut RegistrationContext<'_>) -> anyhow::Result<()> {
        ctx.register_tool(Arc::new(MemoryTool::new(Arc::clone(&self.service))));

        let service = Arc::clone(&self.service);
        let handler: CommandHandler = Arc::new(move |args: &str| {
            let service = Arc::clone(&service);
            let args = args.to_string();
            Box::pin(async move {
                let value = parse_command_args(&args);
                let result = tool::dispatch_command(&service, &value).await;
                CommandOutcome {
                    ok: !result.is_error,
                    error: Some(result.content),
                }
            })
        });
        ctx.register_command(
            COMMAND_NAME,
            Some("Show or operate the project's cm memory"),
            Some(CommandBackend::Rust(handler)),
        );
        Ok(())
    }

    fn events(&self) -> EventInterest {
        EventInterest::List(vec![
            "session_start".to_string(),
            "input".to_string(),
            "before_agent_start".to_string(),
            "agent_settled".to_string(),
        ])
    }

    fn on_event<'a>(&'a self, event: PluginEvent<'a>) -> PluginFuture<'a, PluginEventOutcome> {
        Box::pin(async move {
            match event.name.as_ref() {
                "session_start" => {
                    self.service.invalidate_brief();
                    PluginEventOutcome::None
                }
                "input" => {
                    let Some(text) = event
                        .payload
                        .get("text")
                        .and_then(Value::as_str)
                        .map(str::to_owned)
                    else {
                        return PluginEventOutcome::None;
                    };
                    let source = event.str_field("source").unwrap_or("interactive");
                    if source == "extension" {
                        return PluginEventOutcome::None;
                    }
                    let Some(block) = self.service.auto_recall(&text).await else {
                        return PluginEventOutcome::None;
                    };
                    PluginEventOutcome::Input {
                        action: crate::plugins::plugin::InputAction::Transform,
                        text: Some(prompt::append_to_prompt(&text, &block)),
                        images: None,
                    }
                }
                "before_agent_start" => {
                    let Some(block) = self.service.auto_brief().await else {
                        return PluginEventOutcome::None;
                    };
                    if let Some(options) = event
                        .payload
                        .get_mut("systemPromptOptions")
                        .and_then(Value::as_object_mut)
                    {
                        if options.get("sections").map(Value::is_null).unwrap_or(true) {
                            options.insert("sections".to_string(), Value::Object(Map::new()));
                        }
                        if let Some(sections) =
                            options.get_mut("sections").and_then(Value::as_object_mut)
                        {
                            sections.insert("memory_brief".to_string(), Value::String(block));
                        }
                    }
                    PluginEventOutcome::None
                }
                "agent_settled" => {
                    if let Err(error) = self.service.checkpoint_if_dirty().await {
                        crate::ext_diag!("[memory] checkpoint after settle failed: {error:#}");
                    }
                    PluginEventOutcome::None
                }
                _ => PluginEventOutcome::None,
            }
        })
    }

    fn on_session_context(&self, snapshot: &crate::extensions::SessionContextSnapshot) {
        let cwd = snapshot
            .cwd
            .as_deref()
            .map(PathBuf::from)
            .or_else(|| std::env::current_dir().ok())
            .unwrap_or_else(|| PathBuf::from("."));
        let session_file = snapshot.session_path.as_deref().map(PathBuf::from);
        self.service.observe_session(
            &cwd,
            session_file,
            snapshot.session_id.clone(),
            snapshot.project_trusted,
        );
    }

    fn on_shutdown(&self) -> PluginFuture<'_, anyhow::Result<()>> {
        Box::pin(async move {
            if let Err(error) = self.service.flush().await {
                crate::ext_diag!("[memory] shutdown flush failed: {error:#}");
            }
            Ok(())
        })
    }

    fn on_reload(&self) -> PluginFuture<'_, anyhow::Result<()>> {
        self.service.reload();
        Box::pin(async { Ok(()) })
    }

    fn as_any(&self) -> Option<&dyn std::any::Any> {
        Some(self)
    }
}

pub fn new_memory_plugin() -> Arc<dyn AgentPlugin> {
    MemoryPlugin::new_plugin()
}

/// Parse the `/memory` command's whitespace flags into the same JSON shape the
/// `memory` tool consumes.
fn parse_command_args(input: &str) -> Value {
    let mut tokens = input.split_whitespace();
    let action = tokens.next().unwrap_or("status").to_string();
    let mut map = Map::new();
    map.insert("action".to_string(), Value::String(action.clone()));
    let rest: Vec<&str> = tokens.collect();
    let mut positionals: Vec<&str> = Vec::new();
    let mut index = 0usize;
    while index < rest.len() {
        let token = rest[index];
        if let Some(flag) = token.strip_prefix("--") {
            let key = camel_flag(flag);
            if index + 1 < rest.len() && !rest[index + 1].starts_with("--") {
                let value = rest[index + 1];
                if key == "sources" || key == "supersedes" {
                    map.insert(
                        key,
                        Value::Array(
                            value
                                .split(',')
                                .filter(|part| !part.trim().is_empty())
                                .map(|part| Value::String(part.trim().to_string()))
                                .collect(),
                        ),
                    );
                } else if is_integer_flag(&key) {
                    map.insert(
                        key,
                        value
                            .parse::<u64>()
                            .map(|number| Value::Number(number.into()))
                            .unwrap_or_else(|_| Value::String(value.to_string())),
                    );
                } else if is_float_flag(&key) {
                    map.insert(
                        key,
                        value
                            .parse::<f64>()
                            .ok()
                            .and_then(serde_json::Number::from_f64)
                            .map(Value::Number)
                            .unwrap_or_else(|| Value::String(value.to_string())),
                    );
                } else if is_boolean_flag(&key) {
                    map.insert(
                        key,
                        match value.to_ascii_lowercase().as_str() {
                            "1" | "true" | "yes" | "on" => Value::Bool(true),
                            "0" | "false" | "no" | "off" => Value::Bool(false),
                            _ => Value::String(value.to_string()),
                        },
                    );
                } else {
                    map.insert(key, Value::String(value.to_string()));
                }
                index += 2;
            } else {
                map.insert(key, Value::Bool(true));
                index += 1;
            }
        } else {
            positionals.push(token);
            index += 1;
        }
    }
    if !positionals.is_empty()
        && !map.contains_key("query")
        && matches!(
            action.as_str(),
            "recall" | "context" | "search" | "brief" | "pack" | "wiki"
        )
    {
        map.insert("query".to_string(), Value::String(positionals.join(" ")));
    }
    Value::Object(map)
}

fn is_integer_flag(key: &str) -> bool {
    matches!(
        key,
        "budget"
            | "maxItems"
            | "top"
            | "snippetLines"
            | "limit"
            | "offset"
            | "leaseMinutes"
            | "staleDays"
            | "wrongThreshold"
            | "maxTargets"
            | "maxSections"
    )
}

fn is_float_flag(key: &str) -> bool {
    matches!(key, "confidence" | "minScore" | "mergeThreshold")
}

fn is_boolean_flag(key: &str) -> bool {
    matches!(
        key,
        "empty"
            | "durable"
            | "force"
            | "replace"
            | "verifySource"
            | "includeExpired"
            | "compact"
            | "full"
            | "apply"
            | "strict"
            | "changed"
            | "merge"
            | "refresh"
            | "inPlace"
            | "allowLongText"
            | "dryRun"
            | "requireReview"
    )
}

fn camel_flag(flag: &str) -> String {
    let mut out = String::new();
    let mut upper = false;
    for ch in flag.chars() {
        if ch == '-' || ch == '_' {
            upper = true;
        } else if upper {
            out.push(ch.to_ascii_uppercase());
            upper = false;
        } else {
            out.push(ch);
        }
    }
    out
}
