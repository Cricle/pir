//! The single plugin abstraction: [`AgentPlugin`].
//!
//! Every plugin concept in pir — a compiled Rust builtin (the future
//! `subagent`, `mcp`, ...), an inline Rust factory, and the embedded QuickJS
//! JS-extension runtime — implements this one trait. Registration goes
//! through [`RegistrationContext`] into the shared [`crate::plugins::PluginRegistry`];
//! events flow through ONE ordered dispatch (`PluginHost::emit`) whose merge
//! layer implements the upstream `runner.ts` outcome semantics for both
//! language runtimes.

use std::future::Future;
use std::pin::Pin;

use serde_json::Value as JsonValue;

use crate::extensions::{
    CommandDef, CommandOutcome, FlagDef, SessionContextSnapshot, ShortcutDef, ToolAnnotationsDef,
    ToolDef, ToolNamespaceDef,
};

use super::source::PluginInfo;

// ---------------------------------------------------------------------------
// Shared host-service contract
// ---------------------------------------------------------------------------
//
// `context.rs` (the bound `PluginContext`) and `services/mod.rs` (service
// traits + the `PluginServices` builder) live at `crate::plugins::{context,
// services}`; re-exported here for the plugin module's public surface.
pub use super::context;
pub use super::services;

pub use context::{PluginContext, PluginIdentity, ToolCallScope};
pub use services::PluginServices;

/// Return type for boxed async plugin methods.
pub type PluginFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

// ---------------------------------------------------------------------------
// Events
// ---------------------------------------------------------------------------

/// One event handed to [`AgentPlugin::on_event`].
///
/// The payload is the same camelCase wire shape the JS runtime uses; typed
/// accessors avoid a JSON round-trip for Rust plugins while keeping a single
/// canonical event encoding shared with the JS bridge.
pub struct PluginEvent<'a> {
    /// Event name (`tool_call`, `session_start`, ...).
    pub name: std::borrow::Cow<'a, str>,
    /// Mutable event payload; handlers may rewrite fields in place (upstream
    /// in-place mutation contract, e.g. `tool_call` `input`).
    pub payload: &'a mut JsonValue,
}

impl<'a> PluginEvent<'a> {
    /// Build an event from a name and an owned payload.
    pub fn new(name: impl Into<std::borrow::Cow<'a, str>>, payload: &'a mut JsonValue) -> Self {
        Self {
            name: name.into(),
            payload,
        }
    }

    /// A string field of the payload.
    pub fn str_field(&self, key: &str) -> Option<&str> {
        self.payload.get(key).and_then(JsonValue::as_str)
    }

    /// Mutable view of the `tool_call` input (upstream `event.input`).
    pub fn tool_call_input_mut(&mut self) -> Option<&mut JsonValue> {
        self.payload.get_mut("input")
    }

    /// The `toolName` field.
    pub fn tool_name(&self) -> Option<&str> {
        self.str_field("toolName")
    }
}

/// Input-event outcome action (upstream `InputEventResult`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputAction {
    /// No handler decision.
    Continue,
    /// The handler consumed the input; the host must not run it.
    Handled,
    /// The handler replaced the input text.
    Transform,
}

/// Typed result of one plugin's event handler. The shared merge layer
/// ([`super::merge`]) combines these across the ordered plugin list with
/// upstream semantics.
#[derive(Debug, Clone)]
pub enum PluginEventOutcome {
    /// The handler has no opinion.
    None,
    /// The handler failed (isolated into `EmitOutcome::errors`, never fatal).
    Error { error: String },
    /// `tool_call`: block the call and/or flag batch termination (upstream
    /// `ToolCallEventResult`).
    ToolCall {
        block: bool,
        reason: Option<String>,
        terminate: bool,
    },
    /// `before_agent_start`: override the system prompt and/or inject a
    /// message ahead of the turn.
    BeforeAgentStart {
        system_prompt: Option<String>,
        /// The handler's injected custom message (upstream
        /// `BeforeAgentStartEventResult.message`).
        message: Option<JsonValue>,
        /// Every injected message accumulated so far, in handler order
        /// (upstream runner.ts collects `messages`). The single `message` is
        /// the last one; `messages` is the complete list a host injects ahead
        /// of the turn.
        messages: Vec<JsonValue>,
    },
    /// `input`: handled / transform decision.
    Input {
        action: InputAction,
        text: Option<String>,
        /// Replacement image list (`[{data, mimeType}]`) for a `transform`
        /// decision (upstream `InputEventResult.images`). `None` leaves the
        /// prompt's current images untouched.
        images: Option<JsonValue>,
    },
    /// `session_before_switch|fork|compact|tree`: cancel decision.
    SessionBefore { cancel: bool },
    /// `resources_discover`: contributed resource paths.
    Discover {
        skill_paths: Vec<String>,
        prompt_paths: Vec<String>,
        theme_paths: Vec<String>,
    },
    /// `tool_result`: field-wise result override.
    ToolResult {
        content: Option<JsonValue>,
        details: Option<JsonValue>,
        /// `structuredContent` replacement (upstream keeps it in step with
        /// `content`: a handler that replaces `content` without replacing
        /// this drops it).
        structured_content: Option<JsonValue>,
        is_error: Option<bool>,
        usage: Option<JsonValue>,
    },
    /// `terminal_input`: a raw terminal-input handler's decision (upstream
    /// `TerminalInputHandler`'s `{consume?, data?}` return shape).
    TerminalInput {
        /// Stop the input here; the built-in TUI handler must not see it.
        consume: bool,
        /// Replacement input for later handlers and the built-in path.
        data: Option<String>,
    },
    /// `project_trust` / informational events whose result the host reads
    /// directly.
    Generic(JsonValue),
    /// The QuickJS adapter's outcome: the merged JS result plus the
    /// per-handler errors the runtime already isolated.
    Js {
        result: Box<PluginEventOutcome>,
        errors: Vec<crate::extensions::EventError>,
    },
}

impl PluginEventOutcome {
    /// Interpret a raw JS-merged outcome JSON (`{block, reason}`,
    /// `{systemPrompt, message}`, ...) per event family. This is the seam the
    /// QuickJS adapter rides: the JS runtime already merged its handlers'
    /// results internally; the shared merge layer then treats the runtime as
    /// one plugin with this typed outcome.
    pub fn from_wire(event: &str, result: &JsonValue) -> Self {
        if result.is_null() {
            return Self::None;
        }
        match event {
            "tool_call" => Self::ToolCall {
                block: result.get("block").and_then(JsonValue::as_bool) == Some(true),
                reason: result
                    .get("reason")
                    .and_then(JsonValue::as_str)
                    .map(String::from),
                terminate: result.get("terminate").and_then(JsonValue::as_bool) == Some(true),
            },
            "before_agent_start" => Self::BeforeAgentStart {
                system_prompt: result
                    .get("systemPrompt")
                    .and_then(JsonValue::as_str)
                    .map(String::from),
                message: result.get("message").cloned().filter(|m| !m.is_null()),
                // The JS runtime merges its own handlers internally and
                // reports the accumulated list; carry it so nothing a handler
                // injected is lost at the adapter boundary.
                messages: result
                    .get("messages")
                    .and_then(JsonValue::as_array)
                    .map(|items| {
                        items
                            .iter()
                            .filter(|item| !item.is_null())
                            .cloned()
                            .collect()
                    })
                    .unwrap_or_default(),
            },
            "input" => {
                let action = match result.get("action").and_then(JsonValue::as_str) {
                    Some("handled") => InputAction::Handled,
                    Some("transform") => InputAction::Transform,
                    _ => InputAction::Continue,
                };
                Self::Input {
                    action,
                    text: result
                        .get("text")
                        .and_then(JsonValue::as_str)
                        .map(String::from),
                    images: result.get("images").cloned().filter(|v| !v.is_null()),
                }
            }
            // `session_before_tree` deliberately falls through to `Generic`:
            // its result also carries the mutating preparation overrides
            // (`customInstructions` / `replaceInstructions` / `summary` /
            // `label`), which a `SessionBefore { cancel }` outcome would
            // discard. The merge layer still short-circuits on its
            // `cancel: true` (see `MergedEmit::merge`).
            "session_before_switch" | "session_before_fork" | "session_before_compact" => {
                Self::SessionBefore {
                    cancel: result.get("cancel").and_then(JsonValue::as_bool) == Some(true),
                }
            }
            "resources_discover" => {
                let paths = |key: &str| -> Vec<String> {
                    result
                        .get(key)
                        .and_then(JsonValue::as_array)
                        .map(|items| {
                            items
                                .iter()
                                .filter_map(JsonValue::as_str)
                                .map(String::from)
                                .collect()
                        })
                        .unwrap_or_default()
                };
                Self::Discover {
                    skill_paths: paths("skillPaths"),
                    prompt_paths: paths("promptPaths"),
                    theme_paths: paths("themePaths"),
                }
            }
            "tool_result" => Self::ToolResult {
                content: result.get("content").cloned(),
                details: result.get("details").cloned(),
                structured_content: result.get("structuredContent").cloned(),
                is_error: result.get("isError").and_then(JsonValue::as_bool),
                usage: result.get("usage").cloned(),
            },
            "terminal_input" => Self::TerminalInput {
                consume: result.get("consume").and_then(JsonValue::as_bool) == Some(true),
                data: result
                    .get("data")
                    .and_then(JsonValue::as_str)
                    .map(String::from),
            },
            "project_trust" => Self::Generic(result.clone()),
            _ => Self::Generic(result.clone()),
        }
    }
}

/// Which events a plugin registers handlers for (backs the `has_handlers`
/// fast path the agent loop uses to skip emission round-trips).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum EventInterest {
    /// No handlers at all.
    #[default]
    None,
    /// Handlers for every event.
    All,
    /// Handlers for exactly these event names.
    List(Vec<String>),
}

impl EventInterest {
    /// Whether this plugin wants `event`.
    pub fn matches(&self, event: &str) -> bool {
        match self {
            EventInterest::None => false,
            EventInterest::All => true,
            EventInterest::List(names) => names.iter().any(|name| name == event),
        }
    }
}

// ---------------------------------------------------------------------------
// Registration
// ---------------------------------------------------------------------------

/// Async slash-command handler (upstream `RegisteredCommand.execute`).
pub type CommandHandler = Arc<dyn Fn(&str) -> PluginFuture<'static, CommandOutcome> + Send + Sync>;

/// Async keyboard-shortcut handler (upstream `RegisteredShortcut.execute`).
pub type ShortcutHandler = Arc<dyn Fn() -> PluginFuture<'static, CommandOutcome> + Send + Sync>;

use std::sync::Arc;

/// How a registered tool executes.
#[derive(Clone)]
pub enum ToolBackend {
    /// A native Rust tool object; runs without crossing into QuickJS.
    Rust(Arc<dyn crate::agent::tool_execution::AgentTool>),
    /// A JS tool; executes through the QuickJS runtime's `call_tool`.
    Js {
        execution_mode: crate::agent::tool_batch::ToolExecutionMode,
    },
}

/// How a registered command executes.
#[derive(Clone)]
pub enum CommandBackend {
    Rust(CommandHandler),
    Js,
}

/// How a registered shortcut executes.
#[derive(Clone)]
pub enum ShortcutBackend {
    Rust(ShortcutHandler),
    Js,
}

/// One plugin's registrations, captured by [`RegistrationContext`].
#[derive(Default)]
pub struct PluginRegistrations {
    pub tools: Vec<(ToolDef, ToolBackend)>,
    pub commands: Vec<(CommandDef, Option<CommandBackend>)>,
    pub flags: Vec<FlagDef>,
    pub shortcuts: Vec<(ShortcutDef, Option<ShortcutBackend>)>,
    /// `pi.registerProvider` definitions (`{id, config}`), applied by the
    /// session host to the real provider registry.
    pub providers: Vec<JsonValue>,
    /// `pi.registerMcpServer` definitions (`{name, config}` JSON), consumed
    /// by the `mcp` builtin's registry at the next rebuild (upstream
    /// `McpServerRegistry`).
    pub mcp_servers: Vec<JsonValue>,
    /// `pi.registerVirtualModel` definitions (upstream
    /// `VirtualModelDefinition` wire JSON), applied by the session host to the
    /// provider registry's virtual-model overlay.
    pub virtual_models: Vec<JsonValue>,
}

impl PluginRegistrations {
    /// The replaceable-name keys a plugin claims (upstream
    /// `omitReplacedExtensions`' `names(extension)`).
    pub fn claimed_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self
            .tools
            .iter()
            .map(|(def, _)| format!("tool:{}", def.name))
            .collect();
        names.extend(
            self.commands
                .iter()
                .map(|(def, _)| format!("command:{}", def.name)),
        );
        names.extend(self.flags.iter().map(|flag| format!("flag:{}", flag.name)));
        names
    }
}

/// Typed registration surface handed to [`AgentPlugin::register`] — the Rust
/// mirror of the JS `pi` object's register* methods. No JSON round-trip for
/// Rust tools: an [`AgentTool`] object is registered as-is.
pub struct RegistrationContext<'a> {
    registrations: &'a mut PluginRegistrations,
}

impl<'a> RegistrationContext<'a> {
    pub fn new(registrations: &'a mut PluginRegistrations) -> Self {
        Self { registrations }
    }

    /// Register a native Rust tool (upsert by name at resolution time).
    pub fn register_tool(&mut self, tool: Arc<dyn crate::agent::tool_execution::AgentTool>) {
        use crate::agent::tool_execution::ToolReach;
        let description = tool.description();
        // The def mirrors upstream `ToolDefinition.reachesTools` the same way
        // the JS registry export snapshots it: the hook's value for the empty
        // input probe (`{}`) — the probe `McpPlugin::discovery_activation`
        // makes, and what a JS tool's export carries. The tool OBJECT keeps the
        // live hook (`AgentTool::reaches_tools`), which is what the registry
        // consumers call.
        let reaches_tools = tool
            .reaches_tools(&JsonValue::Object(Default::default()))
            .map(|reach| {
                crate::extensions::ToolReachDef(match reach {
                    ToolReach::All => serde_json::json!("all"),
                    ToolReach::Names(words) => serde_json::json!(words),
                })
            });
        let def = ToolDef {
            name: tool.name().to_owned(),
            label: None,
            description: description.to_owned(),
            prompt_snippet: None,
            prompt_guidelines: Vec::new(),
            parameters: tool.parameters().unwrap_or(JsonValue::Null),
            output_schema: tool.output_schema().unwrap_or(JsonValue::Null),
            source_info: Default::default(),
            execution_mode: match tool.execution_mode() {
                crate::agent::tool_batch::ToolExecutionMode::Sequential => "sequential".into(),
                crate::agent::tool_batch::ToolExecutionMode::Parallel => "parallel".into(),
            },
            exposure: tool.exposure().as_str().to_owned(),
            namespace: tool.namespace().map(|namespace| ToolNamespaceDef {
                name: namespace.name,
                aliases: namespace.aliases,
                description: namespace.description,
                instructions: namespace.instructions,
            }),
            annotations: tool.annotations().map(|annotations| ToolAnnotationsDef {
                read_only_hint: annotations.read_only_hint,
                destructive_hint: annotations.destructive_hint,
                idempotent_hint: annotations.idempotent_hint,
                open_world_hint: annotations.open_world_hint,
            }),
            default_active: tool.default_active(),
            prepare_loadout: false,
            reaches_tools,
        };
        self.registrations
            .tools
            .push((def, ToolBackend::Rust(tool)));
    }

    /// Register a JS-backed tool from its wire definition (the QuickJS
    /// adapter uses this).
    pub fn register_tool_def(
        &mut self,
        def: ToolDef,
        execution_mode: crate::agent::tool_batch::ToolExecutionMode,
    ) {
        self.registrations
            .tools
            .push((def, ToolBackend::Js { execution_mode }));
    }

    /// Register a slash command. A `None` backend is a JS command (the
    /// adapter path); Rust plugins pass a [`CommandHandler`].
    pub fn register_command(
        &mut self,
        name: &str,
        description: Option<&str>,
        handler: Option<CommandBackend>,
    ) {
        self.registrations.commands.push((
            CommandDef {
                name: name.to_owned(),
                description: description.map(String::from),
                source: "extension".to_owned(),
                source_info: Default::default(),
                argument_completions: false,
            },
            handler,
        ));
    }

    /// Register a CLI flag (`boolean` or `string`).
    pub fn register_flag(
        &mut self,
        name: &str,
        flag_type: &str,
        description: &str,
        default: Option<crate::extensions::FlagValue>,
    ) {
        self.registrations.flags.push(FlagDef {
            name: name.to_owned(),
            flag_type: flag_type.to_owned(),
            description: description.to_owned(),
            default,
            ext_id: 0,
        });
    }

    /// Register a keyboard shortcut. A `None` backend is a JS shortcut.
    pub fn register_shortcut(
        &mut self,
        key: &str,
        description: Option<&str>,
        handler: Option<ShortcutBackend>,
    ) {
        self.registrations.shortcuts.push((
            ShortcutDef {
                key: key.to_owned(),
                description: description.map(String::from),
                source_info: Default::default(),
            },
            handler,
        ));
    }

    /// Register a provider definition (`{id, config}` JSON as upstream).
    pub fn register_provider(&mut self, id: &str, config: JsonValue) {
        self.registrations
            .providers
            .push(serde_json::json!({ "id": id, "config": config }));
    }

    /// Record one MCP server definition (`{name, config}` JSON as upstream
    /// `pi.registerMcpServer`); consumed by the `mcp` builtin's registry.
    pub fn register_mcp_server(&mut self, def: JsonValue) {
        self.registrations.mcp_servers.push(def);
    }

    /// Register a virtual model definition (upstream
    /// `registerVirtualModel`): `{provider, id, name?, physical: {provider,
    /// id}, reasoning?, systemPrompt?, modelOverride?}` wire JSON, applied by
    /// the session host to the provider registry's virtual-model overlay.
    pub fn register_virtual_model(&mut self, definition: JsonValue) {
        self.registrations.virtual_models.push(definition);
    }
}

// ---------------------------------------------------------------------------
// The trait
// ---------------------------------------------------------------------------

/// The single plugin concept shared by Rust builtins and JS extensions.
///
/// Lifecycle parity with the JS runtime: `register` runs at activation
/// (factory time), `on_init` before the first dispatched event, `on_event`
/// for every event on the unified ordered dispatch, `on_reload` on
/// `/reload`, `on_shutdown` at teardown. `session_start`/`session_shutdown`
/// are events (like the JS runtime), not separate trait methods.
pub trait AgentPlugin: Send + Sync {
    /// Identity (name, source, hidden/replaceable flags).
    fn info(&self) -> &PluginInfo;

    /// Registration hook, run once when the plugin joins the registry.
    /// Errors fail the plugin's activation (isolated as a load diagnostic,
    /// mirroring upstream's per-extension factory error isolation).
    fn register(&self, ctx: &mut RegistrationContext<'_>) -> anyhow::Result<()> {
        let _ = ctx;
        Ok(())
    }

    /// Which events this plugin handles (fast-path `has_handlers`).
    fn events(&self) -> EventInterest {
        EventInterest::None
    }

    /// Handle one event on the unified dispatch. The payload may be mutated
    /// in place (upstream in-place mutation contract).
    fn on_event<'a>(&'a self, event: PluginEvent<'a>) -> PluginFuture<'a, PluginEventOutcome> {
        let _ = event;
        Box::pin(async { PluginEventOutcome::None })
    }

    /// Host-pushed live session/model snapshot (the same value the JS `ctx`
    /// facade reads). The host calls this when it refreshes the snapshot —
    /// before each prompt and after each run — so a Rust plugin that needs the
    /// current model at event time (the `tool-repair` builtin's
    /// `before_provider_request` model gate) can cache what it needs.
    ///
    /// Synchronous and default no-op. Invariant: the snapshot is advisory and
    /// may be PARTIAL — a host with no JS extension runtime pushes the model
    /// only (`crate::agent_session::*`) — so implementations must tolerate
    /// missing fields and must not block.
    fn on_session_context(&self, snapshot: &SessionContextSnapshot) {
        let _ = snapshot;
    }

    /// Status-line entries this plugin publishes natively, as `(key, text)`
    /// pairs — the Rust counterpart of `ctx.ui.setStatus(key, text)`.
    ///
    /// The host polls this when it builds a transcript/status snapshot
    /// ([`PluginHost::statuses`](super::host::PluginHost::statuses)); it is
    /// never called per rendered frame beyond that existing status refresh.
    /// Return the CURRENT value (a status is a live cell, not an event):
    /// an empty vec means "this plugin has no status entry right now".
    ///
    /// Synchronous, allocation-light and non-blocking by contract. The
    /// default returns no entries, so JS-backed and status-less plugins pay
    /// nothing.
    fn statuses(&self) -> Vec<(String, String)> {
        Vec::new()
    }

    /// Host-side runtime attach: the host hands the plugin the JS runtime
    /// handle the unified host owns (`None` when the session has no runtime)
    /// once, right after the plugin list is assembled and before the first
    /// registry build. Plugins that must know the host's UI posture
    /// (`ctx.hasUI`) or want to reach the runtime for a notification route
    /// cache what they need here; the `mcp` builtin's equivalent seam
    /// (`McpPlugin::attach_js`) is the precedent.
    ///
    /// Default no-op. Implementations must tolerate `None` (SDK/inline-only
    /// sessions have no JS runtime) and must not block.
    fn on_runtime_attached(&self, js: Option<crate::extensions::ExtensionHandle>) {
        let _ = js;
    }

    /// Host-side service attach: the typed [`PluginContext`] for this
    /// activation.
    ///
    /// The host calls this from
    /// [`PluginHost::bind_context`](super::host::PluginHost::bind_context)
    /// **once per activation** (initial bind and every rebind after
    /// `/reload` / session replacement), before the plugin handles its first
    /// event. A plugin that needs service access during `register()` stores
    /// the context here and reads it later: `register()` runs before the
    /// bind, so `on_context` is the only asynchronous context handoff.
    ///
    /// The handed context is scoped to this plugin's identity
    /// ([`PluginContext::identity`]). Its [`PluginContext::epoch`] is the
    /// host generation at bind time; a context cached from an earlier
    /// activation reports [`PluginContext::is_active`] `false` and
    /// [`PluginContext::check_active`] `Err(Stale)` after the host rebinds.
    /// Holding a stale context must never write into the new session; call
    /// `check_active` (or the `require_*` accessors) before a mutating call.
    ///
    /// Default no-op: a host that binds no services, and every existing
    /// plugin, observes exactly the pre-service behavior.
    fn on_context(&self, ctx: &PluginContext) {
        let _ = ctx;
    }

    /// Host-side init (before the first dispatched event).
    fn on_init(&self) -> PluginFuture<'_, anyhow::Result<()>> {
        Box::pin(async { Ok(()) })
    }

    /// Host-side shutdown (after the last dispatched event / on teardown).
    fn on_shutdown(&self) -> PluginFuture<'_, anyhow::Result<()>> {
        Box::pin(async { Ok(()) })
    }

    /// Host-side reload parity (`/reload` re-runs factories for JS; Rust
    /// plugins get this hook to reset internal state).
    fn on_reload(&self) -> PluginFuture<'_, anyhow::Result<()>> {
        Box::pin(async { Ok(()) })
    }

    /// Downcast support: hosts that need to reach one concrete builtin (the
    /// `mcp` builtin's shared manager seam) use this. `None` by default.
    fn as_any(&self) -> Option<&dyn std::any::Any> {
        None
    }
}
