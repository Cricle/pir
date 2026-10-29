//! Tool lifecycle: the `AgentTool` trait, argument validation, and execution
//! context.
//!
//! Rust equivalent of the upstream `AgentTool` interface and the
//! `beforeToolCall`/`afterToolCall` hook contracts.

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, RwLock};

use serde_json::Value as JsonValue;

use crate::agent::tool_batch::{AgentToolCall, ToolExecutionMode, ToolResult};

/// Return type for boxed async tool execution.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

// ---------------------------------------------------------------------------
// Tool exposure model (upstream `core/extensions/types.ts` ToolExposure and
// friends)
// ---------------------------------------------------------------------------

/// How the model reaches a tool (upstream `ToolExposure`). "Callable" means
/// callable from other tools through `ctx.executeTool()`.
///
/// - [`ToolExposure::Direct`]: declared to the model while active, and
///   callable while active.
/// - [`ToolExposure::ModelOnly`]: declared to the model while active, never
///   callable (orchestrating or interactive tools).
/// - [`ToolExposure::Codemode`]: callable whenever registered; not declared to
///   the model unless explicitly activated.
/// - [`ToolExposure::Deferred`]: like codemode, but codemode tools do not list
///   it; tool search can find it.
/// - [`ToolExposure::Hidden`]: registered but unreachable; activating it has
///   no effect.
///
/// The active tool set is the set declared to the model.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[non_exhaustive]
pub enum ToolExposure {
    /// Declared to the model while active, and callable while active.
    #[default]
    Direct,
    /// Declared to the model while active, never callable.
    ModelOnly,
    /// Callable whenever registered; not declared unless explicitly activated.
    Codemode,
    /// Like codemode, but codemode tools do not list it.
    Deferred,
    /// Registered but unreachable.
    Hidden,
}

impl ToolExposure {
    /// The upstream wire name (`"direct"`, `"model-only"`, ...).
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Direct => "direct",
            Self::ModelOnly => "model-only",
            Self::Codemode => "codemode",
            Self::Deferred => "deferred",
            Self::Hidden => "hidden",
        }
    }

    /// Parse an upstream wire name. `None` for unknown values.
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "direct" => Some(Self::Direct),
            "model-only" => Some(Self::ModelOnly),
            "codemode" => Some(Self::Codemode),
            "deferred" => Some(Self::Deferred),
            "hidden" => Some(Self::Hidden),
            _ => None,
        }
    }

    /// Whether the exposure declares the tool while it is active by DEFAULT:
    /// `direct` and `model-only` tools are declared while active, while
    /// `codemode`/`deferred` tools only ever enter the active set through an
    /// explicit activation (`setActiveTools`, a host tool selection, or
    /// `tool_search`'s load) and are declared once they do.
    pub fn declared_while_active(&self) -> bool {
        matches!(self, Self::Direct | Self::ModelOnly)
    }

    /// Whether the tool is callable from other tools through
    /// `ctx.executeTool()` (`direct` while active, plus `codemode` and
    /// `deferred` whenever registered).
    pub fn callable(&self, active: bool) -> bool {
        match self {
            Self::Direct => active,
            Self::Codemode | Self::Deferred => true,
            Self::ModelOnly | Self::Hidden => false,
        }
    }

    /// Whether registering the tool activates it (upstream `defaultActive`
    /// semantics: `direct`/`model-only` tools activate when their
    /// `default_active` flag is set; the others never activate on
    /// registration).
    pub fn active_on_registration(&self, default_active: bool) -> bool {
        self.declared_while_active() && default_active
    }
}

/// A group of related tools, such as the tools of one MCP server (upstream
/// `ToolNamespace`). Codemode tools list them together.
#[derive(Debug, Clone, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolNamespace {
    /// For example `mcp__docs`.
    pub name: String,
    /// Other names that refer to the namespace where tools look it up by name
    /// (codemode's `describeNamespace()` and the `namespace` option of
    /// `searchTools()`), for example `docs`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub aliases: Vec<String>,
    /// Short summary shown once with the group in model-facing tool listings.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Longer usage guidance. Not part of tool listings; tools that describe
    /// the namespace on request return it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>,
}

/// Hints about what a tool does, with the meaning of MCP tool annotations
/// (upstream `ToolAnnotations`). They come from the tool's author and are not
/// verified; permission extensions can use them to decide which calls to
/// confirm.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolAnnotations {
    /// The tool does not modify its environment.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub read_only_hint: Option<bool>,
    /// The tool may delete or overwrite data, rather than only add to it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub destructive_hint: Option<bool>,
    /// Repeating a call with the same arguments has no further effect.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idempotent_hint: Option<bool>,
    /// The tool reaches an open world of external entities, such as the web.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub open_world_hint: Option<bool>,
}

/// Which tools a call may reach (upstream `ToolDefinition.reachesTools`).
///
/// Tools that reach other tools — `codemode`, which calls them, and
/// `tool_search`, which loads them — declare their reach so owners that
/// register tools late, such as MCP servers still connecting, can finish
/// registering the tools the call needs before it runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolReach {
    /// The call may reach every tool (`"all"`).
    All,
    /// The words of the input that may name the tools the call reaches. A word
    /// matches a tool or namespace whose name it contains, so words may
    /// over-match.
    Names(Vec<String>),
}

impl ToolReach {
    /// Whether a call with this reach may need the namespace `namespace`
    /// (upstream `reachesServer`, mcp/index.ts:224-228).
    pub fn reaches_namespace(&self, namespace: &str) -> bool {
        match self {
            ToolReach::All => true,
            ToolReach::Names(words) => words.iter().any(|word| word.contains(namespace)),
        }
    }
}

/// The tools of a session as [`AgentTool::prepare_loadout`] sees them
/// (upstream `ToolLoadout`).
#[derive(Clone)]
pub struct ToolLoadout {
    /// Tools declared to the model (the active tools), in order, with their
    /// original descriptions.
    pub declared: Vec<Arc<dyn AgentTool>>,
    /// Tools callable through `ctx.executeTool()`.
    pub callable: Vec<Arc<dyn AgentTool>>,
    /// Every registered tool.
    pub registered: Vec<Arc<dyn AgentTool>>,
    /// Exposure per registered tool name (backs [`ToolLoadout::get_exposure`]).
    pub exposures: std::collections::BTreeMap<String, ToolExposure>,
    /// Namespace per registered tool name that declares one (backs
    /// [`ToolLoadout::get_namespace`]).
    pub namespaces: std::collections::BTreeMap<String, ToolNamespace>,
}

impl ToolLoadout {
    /// A registered tool's exposure (default [`ToolExposure::Direct`]).
    pub fn get_exposure(&self, name: &str) -> ToolExposure {
        self.exposures
            .get(name)
            .copied()
            .unwrap_or(ToolExposure::Direct)
    }

    /// A tool's namespace, when it declares one.
    pub fn get_namespace(&self, name: &str) -> Option<&ToolNamespace> {
        self.namespaces.get(name)
    }

    /// JSON wire shape handed to JS `prepareLoadout` hooks: names and
    /// descriptions of the declared tools, the callable/registered name lists,
    /// and the exposure/namespace maps.
    pub fn to_wire_json(&self) -> JsonValue {
        serde_json::json!({
            "declared": self.declared.iter().map(|tool| serde_json::json!({
                "name": tool.name(),
                "description": tool.description(),
            })).collect::<Vec<_>>(),
            "callable": self.callable.iter().map(|t| t.name()).collect::<Vec<_>>(),
            "registered": self.registered.iter().map(|t| t.name()).collect::<Vec<_>>(),
            "exposure": self.exposures.iter().map(|(name, exposure)| {
                (name.clone(), JsonValue::String(exposure.as_str().to_owned()))
            }).collect::<serde_json::Map<String, JsonValue>>(),
            "namespace": self.namespaces.iter().map(|(name, namespace)| {
                (name.clone(), serde_json::to_value(namespace).unwrap_or(JsonValue::Null))
            }).collect::<serde_json::Map<String, JsonValue>>(),
        })
    }
}

/// Changes [`AgentTool::prepare_loadout`] makes to what the model sees
/// (upstream `ToolLoadoutChanges`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ToolLoadoutChanges {
    /// Model-facing descriptions of declared tools, by tool name.
    pub descriptions: std::collections::BTreeMap<String, String>,
    /// Declared tools whose declarations requests leave out. They stay active
    /// and callable, and the transcript still declares them, so the active set
    /// survives resume.
    pub hidden_declarations: Vec<String>,
}

impl ToolLoadoutChanges {
    /// A changeset with one replacement description.
    pub fn with_description(name: impl Into<String>, description: impl Into<String>) -> Self {
        let mut changes = Self::default();
        changes.descriptions.insert(name.into(), description.into());
        changes
    }

    /// A changeset hiding one tool's declaration (it stays active).
    pub fn with_hidden_declaration(name: impl Into<String>) -> Self {
        let mut changes = Self::default();
        changes.hidden_declarations.push(name.into());
        changes
    }
}

/// Recovery policy for a durable tool call whose intent was recorded but whose
/// outcome is unknown (upstream `AgentTool.replay`, pkg-agent types.ts:489-490:
/// `"never" | "safe"`).
///
/// The durable harness records the intent before executing and consults this
/// policy on recovery: only [`ToolReplay::Safe`] reruns the call. The default is
/// [`ToolReplay::Never`], which is also what upstream's durable harness applies
/// when a tool omits the field (`tool.replay ?? "unsafe"`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[non_exhaustive]
pub enum ToolReplay {
    /// Never rerun; an unfinished call is abandoned and its outcome stays
    /// unknown (upstream `"never"`, the harness's `"unsafe"`).
    #[default]
    Never,
    /// Rerun after a restart: the call is deterministic enough that a second
    /// execution with the same arguments is acceptable (upstream `"safe"`).
    Safe,
}

impl ToolReplay {
    /// The upstream wire name (`"never"` / `"safe"`).
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Never => "never",
            Self::Safe => "safe",
        }
    }

    /// Parse an upstream wire name; unknown values are `None` (callers keep
    /// their default rather than guessing a policy).
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "never" => Some(Self::Never),
            "safe" => Some(Self::Safe),
            _ => None,
        }
    }

    /// Whether the call may rerun on recovery.
    pub fn is_safe(&self) -> bool {
        matches!(self, Self::Safe)
    }
}

/// Async `AgentTool::prepare_arguments` shim (upstream `AgentTool.prepareArguments`,
/// pkg-agent types.ts:469-473): raw model-supplied arguments in, an object
/// matching `parameters()` out, or the failure message the model sees.
pub type PrepareArgumentsFuture =
    Pin<Box<dyn Future<Output = Result<serde_json::Value, String>> + Send + 'static>>;

/// A registered Agent tool.
///
/// Every tool must implement `run`; the remaining methods provide metadata and
/// configuration used by the scheduler.
pub trait AgentTool: Send + Sync {
    /// Unique tool name used by the model.
    fn name(&self) -> &str;

    /// Human-readable label for UI display (upstream `AgentTool.label`,
    /// pkg-agent types.ts:466-468). Defaults to the tool name, which is what
    /// every builtin upstream declares, so an existing tool needs no change.
    fn label(&self) -> &str {
        self.name()
    }

    /// Human-readable description.
    fn description(&self) -> &str;

    /// Whether this tool declares a [`Self::prepare_arguments`] shim.
    ///
    /// The scheduler consults this BEFORE schema validation. It exists so a
    /// tool without a shim pays no future allocation on the preflight hot
    /// path (the `prepare_arguments` default is an identity future). A tool
    /// that overrides [`Self::prepare_arguments`] MUST override this to
    /// `true`, or the scheduler never calls it.
    fn prepares_arguments(&self) -> bool {
        false
    }

    /// Compatibility shim for raw tool-call arguments, run BEFORE schema
    /// validation (upstream `AgentTool.prepareArguments`, pkg-agent
    /// types.ts:469-473 and `prepareToolCallArguments`, agent-loop.ts:704-716).
    ///
    /// Async per pir's convention (upstream's hook is synchronous; awaiting a
    /// ready future is free) so a host-backed shim works too. Fail-closed: an
    /// `Err` rejects the call with that message and the shim's output never
    /// reaches validation or execution.
    ///
    /// The default is the identity. Override it together with
    /// [`Self::prepares_arguments`].
    fn prepare_arguments(&self, args: serde_json::Value) -> PrepareArgumentsFuture {
        Box::pin(async move { Ok(args) })
    }

    /// Recovery policy for a durable call whose outcome is unknown (upstream
    /// `AgentTool.replay`, pkg-agent types.ts:489-490). Defaults to
    /// [`ToolReplay::Never`], the upstream harness's `?? "unsafe"` behavior.
    fn replay(&self) -> ToolReplay {
        ToolReplay::Never
    }

    /// JSON Schema for the tool's parameters, or `None` if the tool accepts
    /// arbitrary arguments.
    fn parameters(&self) -> Option<serde_json::Value> {
        None
    }

    /// JSON Schema of `structuredContent` in successful results (upstream
    /// `AgentTool.outputSchema`). Tools that declare one should always set
    /// `structured_content` on their results.
    fn output_schema(&self) -> Option<serde_json::Value> {
        None
    }

    /// Constrained-sampling preference for this tool (upstream
    /// `ToolDefinition.constrainedSampling`). The default is `None`: the tool
    /// declares no sampling constraint. Only the four upstream core file/shell
    /// tools (`read`, `bash`, `edit`, `write`) carry
    /// `{ type: "json_schema", strict: "prefer" }`; every other tool —
    /// including extensions and the remaining built-ins — is unrestricted.
    fn constrained_sampling(&self) -> Option<crate::ai::types::ConstrainedSamplingConfig> {
        None
    }

    /// Whether the tool operates on file-system paths that must be serialized
    /// per canonical path (`read`, `write`, `edit`).
    fn is_mutation_tool(&self) -> bool {
        false
    }

    /// Whether the tool performs external side effects (shell commands, network
    /// writes, auth flows) that are not serialized by the mutation queue.
    /// Informational only: upstream runs mixed batches in parallel by default
    /// (only a tool's own `executionMode: "sequential"` degrades a batch), so
    /// this flag no longer forces sequential execution — per-file mutations
    /// stay serialized through the mutation queue regardless.
    fn has_external_side_effects(&self) -> bool {
        false
    }

    /// Prompt snippet contributed when the tool joins the declared loadout
    /// (upstream `ToolDefinition.promptSnippet`); `None` contributes nothing.
    fn prompt_snippet(&self) -> Option<String> {
        None
    }

    /// Prompt guidelines contributed when the tool joins the declared loadout
    /// (upstream `ToolDefinition.promptGuidelines`); empty contributes nothing.
    fn prompt_guidelines(&self) -> Vec<String> {
        Vec::new()
    }

    /// Tool-level execution mode. `Sequential` forces the whole batch to
    /// degrade to sequential execution.
    fn execution_mode(&self) -> ToolExecutionMode {
        ToolExecutionMode::Parallel
    }

    /// Whether a call may run in the background (`background: true`) and have
    /// its result delivered after the turn. Default `true`. Mutation tools are
    /// admitted too (step 2): their per-file queue locks are taken inside the
    /// call's own execution future, so they serialize with any other writer of
    /// the same file without spanning a turn. Only a tool that opts out here
    /// is refused by preflight.
    fn supports_background(&self) -> bool {
        true
    }

    /// How the model reaches the tool (upstream `ToolDefinition.exposure`).
    /// Default [`ToolExposure::Direct`].
    fn exposure(&self) -> ToolExposure {
        ToolExposure::Direct
    }

    /// The group the tool belongs to, for example its MCP server (upstream
    /// `ToolDefinition.namespace`).
    fn namespace(&self) -> Option<ToolNamespace> {
        None
    }

    /// Hints about what the tool does (upstream `ToolDefinition.annotations`).
    fn annotations(&self) -> Option<ToolAnnotations> {
        None
    }

    /// Whether registering the tool activates it (upstream
    /// `ToolDefinition.defaultActive`; default `true` for `direct` and
    /// `model-only` tools, which are the only exposures that activate on
    /// registration).
    fn default_active(&self) -> bool {
        true
    }

    /// Adjust how the loadout is presented to the model while this tool is
    /// active (upstream `ToolDefinition.prepareLoadout`). Called whenever the
    /// active tools change; tools that orchestrate other tools use it, for
    /// example to list the callable tools in their own description.
    fn prepare_loadout(&self, _loadout: &ToolLoadout) -> Option<ToolLoadoutChanges> {
        None
    }

    /// Which tools a call with `input` may reach (upstream
    /// `ToolDefinition.reachesTools`); `None` for a tool that does not reach
    /// other tools.
    ///
    /// JS extensions register the hook as a function of the tool definition and
    /// call it through `pi.getAllTools()`; this is the Rust mirror for builtin
    /// tools, consumed by namespace owners that finish registering late
    /// (for example MCP servers still connecting, which wait for the servers a
    /// call may reach before it runs). For a JS tool the mirror is the
    /// registry-export snapshot ([`ExtensionTool::reaches_tools`]); its live
    /// per-call value is resolved by the MCP wait through the runtime.
    fn reaches_tools(&self, _input: &serde_json::Value) -> Option<ToolReach> {
        None
    }

    /// Downcast support: consumers that must recognize one concrete tool object
    /// use this. The MCP reach wait recognizes a JS extension tool through it
    /// (`ExtensionTool::new`'s snapshot vs the live hook in QuickJS). `None` by
    /// default.
    fn as_any(&self) -> Option<&dyn std::any::Any> {
        None
    }

    /// Destructured mutation paths extracted from the tool arguments.
    ///
    /// Only called when `is_mutation_tool()` is true. Returns paths resolved
    /// against the current working directory. The scheduler uses the canonical
    /// form of these paths to serialize concurrent writes to the same file.
    fn mutation_paths(
        &self,
        _args: &serde_json::Value,
        _cwd: &std::path::Path,
    ) -> Vec<std::path::PathBuf> {
        Vec::new()
    }

    /// Execute the tool with validated arguments.
    fn run(&self, args: &serde_json::Value, ctx: &ToolContext) -> BoxFuture<'static, ToolResult>;
}

/// Live partial-result channel for long-running tools (upstream tool
/// `onUpdate`): the loop wires it to the `tool_execution_update` event so
/// hosts observe progress while the tool runs. The payload is the partial
/// `AgentToolResult` JSON (`{content, details, isError}`).
pub type ToolUpdateFn = Arc<dyn Fn(serde_json::Value) + Send + Sync>;

/// Project one live partial snapshot into the upstream
/// `tool_execution_update` payload (`AssistantMessageEvent`-adjacent wire
/// shape): the snapshot itself stays `partialResult` and the call identity the
/// scheduler stamped onto it rides alongside (`toolCallId` / `toolName` /
/// `args`, upstream `ToolExecutionUpdateEvent`). Nested calls use
/// `<parent>/<n>` ids, so the parent prefix also yields `parentToolCallId`.
/// Shared by the extension dispatch channel and the print/JSON host.
pub fn tool_update_payload(partial: &serde_json::Value) -> serde_json::Value {
    let mut payload = serde_json::json!({
        "type": "tool_execution_update",
        "partialResult": partial.clone(),
    });
    if let Some(call_id) = partial
        .get("toolCallId")
        .and_then(serde_json::Value::as_str)
    {
        payload["toolCallId"] = serde_json::Value::String(call_id.to_owned());
        if let Some((parent, _)) = call_id.rsplit_once('/') {
            payload["parentToolCallId"] = serde_json::Value::String(parent.to_owned());
        }
    }
    if let Some(tool_name) = partial.get("toolName").and_then(serde_json::Value::as_str) {
        payload["toolName"] = serde_json::Value::String(tool_name.to_owned());
    }
    if let Some(args) = partial.get("args") {
        payload["args"] = args.clone();
    }
    payload
}

/// [`tool_update_payload`] for a caller that owns `partial` and has no other
/// consumer: the snapshot MOVES into `partialResult` instead of being
/// deep-copied. The call-identity fields and `args` are extracted first (the
/// wire shape carries them beside `partialResult` as well); everything else —
/// the potentially large `content`/`details` payload — is never copied.
pub fn tool_update_payload_owned(partial: serde_json::Value) -> serde_json::Value {
    let call_id = partial
        .get("toolCallId")
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned);
    let tool_name = partial
        .get("toolName")
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned);
    let args = partial.get("args").cloned();
    let mut payload = serde_json::Map::with_capacity(6);
    payload.insert(
        "type".to_owned(),
        serde_json::Value::String("tool_execution_update".to_owned()),
    );
    payload.insert("partialResult".to_owned(), partial);
    if let Some(call_id) = call_id {
        if let Some((parent, _)) = call_id.rsplit_once('/') {
            payload.insert(
                "toolCallId".to_owned(),
                serde_json::Value::String(call_id.clone()),
            );
            payload.insert(
                "parentToolCallId".to_owned(),
                serde_json::Value::String(parent.to_owned()),
            );
        } else {
            payload.insert("toolCallId".to_owned(), serde_json::Value::String(call_id));
        }
    }
    if let Some(tool_name) = tool_name {
        payload.insert("toolName".to_owned(), serde_json::Value::String(tool_name));
    }
    if let Some(args) = args {
        payload.insert("args".to_owned(), args);
    }
    serde_json::Value::Object(payload)
}

/// One live tool update unpacked into the fields the host `AgentEvent`
/// carries. `partial` is the tool-level snapshot with the scheduler's
/// identity stamps removed (upstream `partialResult` is an
/// `AgentToolResult`, not the event envelope).
pub(crate) struct ToolUpdateParts {
    pub call_id: String,
    pub tool_name: String,
    pub parent_call_id: Option<String>,
    pub args: serde_json::Value,
    pub partial: serde_json::Value,
}

/// Split a live update payload into its call identity and partial result.
///
/// The scheduler stamps `toolCallId`/`toolName`/`args` onto the tool's raw
/// snapshot; a wire-shaped payload instead carries an explicit
/// `partialResult`. Both shapes are accepted. A nested call id is
/// `<parent>/<n>`; only a numeric suffix proves that shape, so a provider id
/// that merely contains `/` never fabricates a parent.
pub(crate) fn tool_update_parts(payload: &serde_json::Value) -> ToolUpdateParts {
    let call_id = payload
        .get("toolCallId")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let tool_name = payload
        .get("toolName")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let parent_call_id = payload
        .get("parentToolCallId")
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned)
        .or_else(|| {
            call_id.rsplit_once('/').and_then(|(parent, suffix)| {
                suffix
                    .chars()
                    .all(|ch| ch.is_ascii_digit())
                    .then(|| parent.to_owned())
            })
        });
    let args = payload
        .get("args")
        .cloned()
        .unwrap_or(serde_json::Value::Null);
    let partial = payload.get("partialResult").cloned().unwrap_or_else(|| {
        let mut partial = payload.clone();
        if let Some(object) = partial.as_object_mut() {
            object.remove("toolCallId");
            object.remove("toolName");
            object.remove("parentToolCallId");
            object.remove("args");
        }
        partial
    });
    ToolUpdateParts {
        call_id,
        tool_name,
        parent_call_id,
        args,
        partial,
    }
}

/// [`tool_update_parts`] for a caller that owns `payload` and has no other
/// consumer: the raw snapshot MOVES into `partial` (identity stamps removed
/// in place) instead of being deep-copied. Same accepted shapes and output
/// values as the borrowed form.
pub(crate) fn tool_update_parts_owned(mut payload: serde_json::Value) -> ToolUpdateParts {
    let call_id = payload
        .get("toolCallId")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let tool_name = payload
        .get("toolName")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let parent_call_id = payload
        .get("parentToolCallId")
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned)
        .or_else(|| {
            call_id.rsplit_once('/').and_then(|(parent, suffix)| {
                suffix
                    .chars()
                    .all(|ch| ch.is_ascii_digit())
                    .then(|| parent.to_owned())
            })
        });
    let args = payload
        .get("args")
        .cloned()
        .unwrap_or(serde_json::Value::Null);
    let partial = if payload.get("partialResult").is_some() {
        payload
            .as_object_mut()
            .and_then(|object| object.remove("partialResult"))
            .unwrap_or(serde_json::Value::Null)
    } else {
        if let Some(object) = payload.as_object_mut() {
            object.remove("toolCallId");
            object.remove("toolName");
            object.remove("parentToolCallId");
            object.remove("args");
        }
        payload
    };
    ToolUpdateParts {
        call_id,
        tool_name,
        parent_call_id,
        args,
        partial,
    }
}

/// The content BLOCK array of a finished tool result (upstream
/// `AgentToolResult.content: (TextContent | ImageContent)[]`): one text block
/// followed by the result's image blocks.
///
/// Shared by the extension `tool_result` payload, the `tool_execution_end`
/// payload and the JSON/print session event, so every surface carries the same
/// complete array (a text-only `content` used to drop tool-result images at
/// the extension boundary).
pub fn tool_result_content_blocks(
    result: &crate::agent::tool_batch::ToolResult,
) -> Vec<serde_json::Value> {
    let mut content = Vec::with_capacity(1 + result.images.len());
    content.push(serde_json::json!({
        "type": "text",
        "text": result.content,
    }));
    for image in &result.images {
        content.push(serde_json::json!({
            "type": "image",
            "data": image.data,
            "mimeType": image.mime_type,
        }));
    }
    content
}

/// Wire shape of a finished tool result (upstream `AgentToolResult`):
/// `content` is the block array the model sees (one text block plus any image
/// blocks), `details`/`structuredContent`/`terminate` are present only when set
/// (an absent optional is omitted, matching `JSON.stringify` of `undefined`).
///
/// Shared by the extension `tool_execution_end` payload and the JSON/print
/// session event, so both surfaces carry the result shape an upstream consumer
/// expects (`content[0].text`, not a bare string; `terminate` drives batch
/// termination upstream: `finalized.result.terminate === true`).
pub fn tool_result_event_json(result: &crate::agent::tool_batch::ToolResult) -> serde_json::Value {
    let mut value = serde_json::json!({ "content": tool_result_content_blocks(result) });
    if let Some(details) = &result.details {
        value["details"] = details.clone();
    }
    if let Some(structured) = &result.structured_content {
        value["structuredContent"] = structured.clone();
    }
    // `terminate: false` is upstream's `undefined` (the key is simply absent);
    // only a set flag is serialized, so consumers read the same truth the
    // internal batch uses.
    if result.terminate {
        value["terminate"] = serde_json::Value::Bool(true);
    }
    value
}

/// One in-flight background tool call as the model-facing tools report it.
#[derive(Debug, Clone, serde::Serialize)]
pub struct BackgroundCallInfo {
    /// Provider tool-call id of the detached call.
    pub call_id: String,
    /// Tool name.
    pub tool_name: String,
    /// The requesting call id when another tool made the detached call.
    pub parent_call_id: Option<String>,
}

/// The host's background-call control surface, exposed to the model-facing
/// `background_status` / `background_abort` tools.
///
/// Implemented by the session host (`AgentSession`'s registry) and by print
/// mode (`PrintBackground`), so both hosts give the model the same view of
/// their own in-flight calls without the tools depending on either type.
pub trait BackgroundControl: Send + Sync {
    /// The calls still running.
    fn calls(&self) -> Vec<BackgroundCallInfo>;
    /// Abort one call by provider tool-call id; `false` for an unknown or
    /// already-settled id (a successful no-op, matching the RPC surface).
    fn abort(&self, call_id: &str) -> bool;
    /// Abort every in-flight call; returns how many were cancelled.
    fn abort_all(&self) -> usize;
}

/// A provider-scoped credential carried by [`ToolSessionEnv`].
///
/// The key belongs to exactly ONE provider, mirroring
/// `AgentSession::request_api_key_for`'s scope rule: a child run inherits it
/// only when its own model's provider matches, so a subagent that switches to
/// another provider never receives the parent's key.
///
/// The key is private and [`Debug`] renders it redacted — the whole env can
/// travel into diagnostics/logs without leaking the credential.
#[derive(Clone)]
pub struct ToolCredential {
    provider: String,
    /// Only the `subagent` builtin reads the key (via [`Self::key_for`]);
    /// other builds carry the credential for embedder symmetry without ever
    /// reading it.
    #[cfg_attr(not(feature = "subagent"), allow(dead_code))]
    key: String,
}

impl ToolCredential {
    /// Create a credential scoped to `provider`.
    pub fn new(provider: impl Into<String>, key: impl Into<String>) -> Self {
        Self {
            provider: provider.into(),
            key: key.into(),
        }
    }

    /// The provider this credential belongs to.
    pub fn provider(&self) -> &str {
        &self.provider
    }

    /// The key, only when `provider` is this credential's scope.
    #[cfg_attr(not(feature = "subagent"), allow(dead_code))]
    pub(crate) fn key_for(&self, provider: &str) -> Option<&str> {
        (self.provider == provider).then_some(self.key.as_str())
    }
}

impl std::fmt::Debug for ToolCredential {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ToolCredential")
            .field("provider", &self.provider)
            .field("key", &"[redacted]")
            .finish()
    }
}

/// Host-provided run environment for tools that spawn their own agent runs
/// (the `subagent` builtin): the enclosing session's provider registry and
/// current model/thinking level (the dispatch defaults), the session's base
/// tool registry (the allowlist source for child runs), and the UI/trust
/// posture for the project-agent confirm gate. `None` for plain tool
/// executions and tool-level tests.
#[derive(Clone)]
pub struct ToolSessionEnv {
    /// Provider registry child runs stream from.
    pub provider_registry: Arc<crate::ai::provider::ProviderRegistry>,
    /// The parent session's current model (upstream `ctx.model`).
    pub model: crate::ai::types::Model,
    /// The parent session's thinking level (upstream `ctx.thinkingLevel`).
    pub thinking: crate::ai::types::ThinkingLevel,
    /// The persisted session id (upstream `ctx.sessionManager.getSessionId()`),
    /// exposed to shell children as `PI_SESSION_ID`. `None` for a host without
    /// session storage (an ephemeral print run or a bare tool execution).
    pub session_id: Option<String>,
    /// The session file path (upstream
    /// `ctx.sessionManager.getSessionFile()`), exposed to shell children as
    /// `PI_SESSION_FILE`. `None` for an in-memory session.
    pub session_file: Option<std::path::PathBuf>,
    /// The parent session's base (core) tool registry; a child run's registry
    /// is this registry filtered by the agent's `tools` allowlist.
    pub base_tools: Arc<ToolRegistry>,
    /// Whether the run has a UI (upstream `ctx.hasUI`); gates the
    /// project-agent confirm flow.
    pub has_ui: bool,
    /// Whether the current project is trusted (upstream
    /// `ctx.isProjectTrusted()`); skips the project-agent gate when true.
    pub project_trusted: bool,
    /// The user-level agent directory for subagent discovery; `None` resolves
    /// to the process agent dir (`~/.pi/agent/agents`) at call time.
    pub user_agents_dir: Option<std::path::PathBuf>,
    /// The agent directory the ENCLOSING session was built with (upstream
    /// `getAgentDir()`): child runs build their sessions against it instead
    /// of re-resolving the process environment, so an SDK `.agent_dir(..)`
    /// agent's children read that directory's `models.json`/`auth.json` and
    /// settings. `None` keeps the pre-existing process-resolution behavior.
    pub agent_dir: Option<std::path::PathBuf>,
    /// The parent session's explicit provider-scoped credential
    /// (`AgentSessionConfig::api_key` / `.endpoint(..).api_key(..)` /
    /// `.api_key(..)`). A child run inherits it only when its model's
    /// provider matches [`ToolCredential::provider`].
    pub credential: Option<ToolCredential>,
    /// Live partial-result channel (upstream tool `onUpdate`).
    pub on_update: Option<ToolUpdateFn>,
    /// The merged live registry (base + plugin tools) for tools that need to
    /// see the whole callable surface — the `tool_search` builtin's search
    /// space (upstream `getAllTools`).
    pub live_tools: Option<Arc<ToolRegistry>>,
    /// The session's active-tool override cell (upstream
    /// `setActiveTools`): writing it re-drives the loadout precedence chain
    /// on the next rebuild, which is how `tool_search` loads matches.
    pub active_override: Option<Arc<RwLock<Option<Vec<String>>>>>,
    /// The host's in-flight background calls (the `background_status` /
    /// `background_abort` tools). `None` for hosts that run no background
    /// registry; the tools then report that instead of an empty list.
    pub background: Option<Arc<dyn BackgroundControl>>,
}

impl std::fmt::Debug for ToolSessionEnv {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ToolSessionEnv")
            .field(
                "model",
                &format!("{}/{}", self.model.provider, self.model.id),
            )
            .field("thinking", &self.thinking)
            .field("session_id", &self.session_id)
            .field("session_file", &self.session_file)
            .field("base_tools", &self.base_tools.names())
            .field("has_ui", &self.has_ui)
            .field("project_trusted", &self.project_trusted)
            .field("agent_dir", &self.agent_dir)
            .field("credential", &self.credential)
            .field("on_update", &self.on_update.is_some())
            .field("background", &self.background.is_some())
            .finish()
    }
}

/// Context provided to every tool execution.
#[derive(Clone)]
pub struct ToolContext {
    /// Current working directory for file operations.
    pub cwd: std::path::PathBuf,
    /// Optional abort signal. The tool should check it periodically and
    /// return early with an error result when aborted.
    pub signal: Option<crate::ai::types::AbortSignal>,
    /// Nested-call capability (upstream `ctx.executeTool`): when set, the tool
    /// may execute further tool calls that go through the same hooks and
    /// scheduler path. The scheduler rebinds the executor's caller id per
    /// call via [`ToolContext::with_call_scope`].
    ///
    /// Shared behind an `Arc`: the executor is handed to the extension
    /// runtime's per-call registry for the duration of a call (the JS
    /// `ctx.executeTool` round-trip holds it across its `.await`), and every
    /// context clone — one per tool batch member, plus every consumer that
    /// clones the context — is then a refcount bump instead of an executor
    /// clone (which re-allocates its `caller_id` `String`).
    pub nested: Option<Arc<crate::agent::nested_tool_calls::NestedToolExecutor>>,
    /// Host-provided session environment for tools that spawn agent runs
    /// (upstream `ctx.model` / `ctx.hasUI` / `ctx.isProjectTrusted()`).
    pub session_env: Option<Arc<ToolSessionEnv>>,
    /// Canonical mutation-path keys the call chain above this call holds in
    /// the scheduler's per-file mutation queue. A mutation tool on a path an
    /// ancestor genuinely locks must skip re-locking: the ancestor's guard
    /// spans its whole execution, so re-acquiring would self-deadlock. The
    /// set is read as a snapshot; a call never marks its in-flight keys into
    /// it (the scheduler publishes its acquired keys in a fresh set scoped
    /// to the call), so concurrent nested siblings sharing this set cannot
    /// make each other skip the queue.
    pub held_mutation_keys: Option<Arc<std::sync::Mutex<std::collections::BTreeSet<String>>>>,
    /// Live partial-result channel for long-running tools (upstream tool
    /// `onUpdate`): the shell tools push throttled rolling output snapshots
    /// while a command runs. Mirrors [`ToolSessionEnv::on_update`], which
    /// stays the wiring point hosts populate.
    pub on_update: Option<ToolUpdateFn>,
    /// This execution was adopted as a background call (`background: true`).
    /// Shell tools use it to skip the *settings* command timeout: a detached
    /// call is expected to outlive the turn, so the 120 s default cap does not
    /// apply to it (an explicit per-call `timeout` still does).
    pub background: bool,
}

impl ToolContext {
    /// Create a new tool context.
    pub fn new(cwd: impl Into<std::path::PathBuf>) -> Self {
        Self {
            cwd: cwd.into(),
            signal: None,
            nested: None,
            session_env: None,
            held_mutation_keys: None,
            on_update: None,
            background: false,
        }
    }

    /// Rebind the nested-call scope to `call_id` so nested calls made by this
    /// execution are recorded against it (upstream scopes by tool call id).
    /// A context without the nested capability is returned unchanged.
    pub fn with_call_scope(&self, call_id: &str) -> Self {
        match &self.nested {
            Some(executor) => {
                // The held set must come from the scoped executor, not from
                // the one being rebound: rebinding to a fresh call id (each
                // batch sibling) starts an independent set, while an
                // already-bound nested call keeps its chain's set. Context
                // and executor start from the same set; the scheduler later
                // republishes the keys this call acquired as a fresh set
                // scoped to the call (nested executor rebind included), so
                // the marks stay visible to this call's descendants but
                // never to its concurrent siblings.
                let scoped = executor
                    .for_call(call_id)
                    .with_signal(self.signal.clone());
                // `for_call` already returns an owned executor for this call
                // id; read the shared held-key set before moving it in
                // instead of cloning the whole executor again.
                let held_mutation_keys = Arc::clone(scoped.held_mutation_keys());
                Self {
                    cwd: self.cwd.clone(),
                    signal: self.signal.clone(),
                    // One allocation per scheduled call, and the single
                    // owner from here on: every later consumer (the
                    // `ExtensionTool::run` bind, `codemode`, nested contexts)
                    // shares it by refcount.
                    nested: Some(Arc::new(scoped)),
                    session_env: self.session_env.clone(),
                    held_mutation_keys: Some(held_mutation_keys),
                    on_update: self.on_update.clone(),
                    background: self.background,
                }
            }
            None => self.clone(),
        }
    }

    /// The live partial-result channel: the context-level channel when set,
    /// else the session environment's (hosts populate the latter).
    pub fn update_channel(&self) -> Option<ToolUpdateFn> {
        self.on_update.clone().or_else(|| {
            self.session_env
                .as_ref()
                .and_then(|env| env.on_update.clone())
        })
    }

    /// Return true when the abort signal has been triggered.
    pub fn is_aborted(&self) -> bool {
        self.signal
            .as_ref()
            .is_some_and(|signal| signal.is_aborted())
    }

    /// The requesting tool call id when this batch was started by another
    /// tool (`ctx.executeTool`); `None` for a model-issued batch. This is the
    /// `parentToolCallId` the scheduler stamps on `tool_execution_*` events
    /// and the extension `tool_call`/`tool_result` payloads.
    pub(crate) fn parent_call_id(&self) -> Option<&str> {
        self.nested
            .as_ref()
            .and_then(|nested| nested.parent_call_id())
    }
}

impl std::fmt::Debug for ToolContext {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Manual impl: the `on_update` channel is an `Arc<dyn Fn>` and is not
        // `Debug`; only its presence is reported (mirrors ToolSessionEnv).
        formatter
            .debug_struct("ToolContext")
            .field("cwd", &self.cwd)
            .field("signal", &self.signal)
            .field("nested", &self.nested)
            .field("session_env", &self.session_env)
            .field("held_mutation_keys", &self.held_mutation_keys.is_some())
            .field("on_update", &self.on_update.is_some())
            .finish()
    }
}

/// A registry of named tools.
#[derive(Clone, Default)]
pub struct ToolRegistry {
    inner: std::collections::HashMap<String, Arc<dyn AgentTool>>,
    /// Per-session metadata that travels with the registry but is NOT a tool.
    ///
    /// Session builds strip [`ToolExposure::Hidden`] tools from the base
    /// registry (`AgentSessionConfig::tools` → `AgentSession::tools` →
    /// [`ToolSessionEnv::base_tools`]), so state that a nested run must read
    /// from its dispatcher cannot ride a registered tool — it would be
    /// stripped before any tool ever sees it. Keeping it here keeps it out of
    /// the model-facing registry by construction (it is not a declaration and
    /// has no name to call) while surviving the strip, cloning, and the
    /// `ToolSessionEnv` hand-off.
    session_depth: usize,
}

impl ToolRegistry {
    /// Create an empty registry.
    pub fn new() -> Self {
        Self {
            inner: std::collections::HashMap::new(),
            session_depth: 0,
        }
    }

    /// Record the subagent nesting depth of the session that owns this
    /// registry ([`Self::session_depth`] reads it back). Out-of-band, so it
    /// survives hidden-tool stripping and never reaches the model.
    pub fn set_session_depth(&mut self, depth: usize) {
        self.session_depth = depth;
    }

    /// The subagent nesting depth recorded for this registry's session (0 for
    /// a root session).
    pub fn session_depth(&self) -> usize {
        self.session_depth
    }

    /// Register a tool (upsert by name).
    pub fn register(&mut self, tool: Arc<dyn AgentTool>) {
        self.inner.insert(tool.name().to_string(), tool);
    }

    /// Remove a tool by name (used by the `setActiveTools` override filter).
    pub fn remove(&mut self, name: &str) {
        self.inner.remove(name);
    }

    /// Look up a tool by name.
    pub fn get(&self, name: &str) -> Option<Arc<dyn AgentTool>> {
        self.inner.get(name).cloned()
    }

    /// Names of all registered tools, sorted by name.
    ///
    /// `inner` is a `HashMap`, so raw key iteration order is randomized per
    /// process. That order reached the provider request (the tools array is
    /// built from these names), which moved the prompt prefix between runs and
    /// within a run's turns and defeated prompt-prefix caching. A stable name
    /// order makes every request reproducible; an explicit active list keeps
    /// its caller-provided order (see [`Self::active_names`]).
    pub fn names(&self) -> Vec<&str> {
        let mut names: Vec<&str> = self.inner.keys().map(|s| s.as_str()).collect();
        names.sort_unstable();
        names
    }

    /// Iterate all tools in stable name order (see [`Self::names`]).
    pub fn iter(&self) -> impl Iterator<Item = &Arc<dyn AgentTool>> {
        let mut tools: Vec<&Arc<dyn AgentTool>> = self.inner.values().collect();
        tools.sort_by(|left, right| left.name().cmp(right.name()));
        tools.into_iter()
    }

    /// Number of registered tools.
    pub fn len(&self) -> usize {
        self.inner.len()
    }

    /// Whether the registry is empty.
    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }

    /// A tool's exposure (default [`ToolExposure::Direct`] for unknown names).
    pub fn exposure_of(&self, name: &str) -> ToolExposure {
        self.get(name)
            .map(|tool| tool.exposure())
            .unwrap_or(ToolExposure::Direct)
    }

    /// Remove every tool with `exposure` (upstream `hidden` tools are
    /// registered but unreachable — they never join a session registry).
    /// Returns the removed names.
    pub fn remove_exposure(&mut self, exposure: ToolExposure) -> Vec<String> {
        let removed: Vec<String> = self
            .inner
            .iter()
            .filter(|(_, tool)| tool.exposure() == exposure)
            .map(|(name, _)| name.clone())
            .collect();
        for name in &removed {
            self.inner.remove(name);
        }
        removed
    }

    /// The active tool names (the declared set): `override_names` when set
    /// (upstream `setActiveTools` semantics — exactly the requested registered
    /// names, hidden ones ignored and each name at most once: upstream
    /// `_applyToolLoadout` dedupes with `new Set(toolNames)`,
    /// agent-session.ts:1580-1584), otherwise every tool whose exposure
    /// activates on registration with its `default_active` flag.
    pub fn active_names(&self, override_names: Option<&[String]>) -> Vec<String> {
        match override_names {
            Some(names) => {
                let mut seen = std::collections::BTreeSet::new();
                names
                    .iter()
                    .filter(|name| seen.insert(name.as_str()))
                    .filter(|name| {
                        self.get(name.as_str())
                            .is_some_and(|tool| tool.exposure() != ToolExposure::Hidden)
                    })
                    .cloned()
                    .collect()
            }
            None => {
                let mut names: Vec<String> = self
                    .inner
                    .iter()
                    .filter(|(_, tool)| {
                        tool.exposure()
                            .active_on_registration(tool.default_active())
                    })
                    .map(|(name, _)| name.clone())
                    .collect();
                names.sort();
                names
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Loadout helpers (upstream `_applyToolLoadout` / `_getCallableTools`)
// ---------------------------------------------------------------------------

/// Tools declared to the model: every active non-hidden tool, in `active`
/// order (upstream `_applyToolLoadout` declares the whole active set).
///
/// `codemode`/`deferred` tools are never active by default
/// ([`ToolExposure::active_on_registration`]), so they only appear here once
/// something explicitly activates them — a `setActiveTools` override, a host
/// tool selection, or `tool_search` loading the matches into the session's
/// active set. `hidden` tools never join a session registry and are guarded
/// out here as well.
pub fn declared_tools(registry: &ToolRegistry, active: &[String]) -> Vec<Arc<dyn AgentTool>> {
    let mut seen = std::collections::BTreeSet::new();
    active
        .iter()
        .filter_map(|name| {
            if !seen.insert(name.clone()) {
                return None;
            }
            registry
                .get(name)
                .filter(|tool| tool.exposure() != ToolExposure::Hidden)
        })
        .collect()
}

/// Tools callable through `ctx.executeTool()`: the active `direct` tools and
/// every registered `codemode` or `deferred` tool (upstream
/// `_getCallableTools`). `model-only` and `hidden` tools are never callable.
pub fn callable_tools(registry: &ToolRegistry, active: &[String]) -> Vec<Arc<dyn AgentTool>> {
    let active_set: std::collections::BTreeSet<&str> = active.iter().map(String::as_str).collect();
    let mut callable: Vec<Arc<dyn AgentTool>> = registry
        .inner
        .values()
        .filter(|tool| tool.exposure().callable(active_set.contains(tool.name())))
        .cloned()
        .collect();
    callable.sort_by(|left, right| left.name().cmp(right.name()));
    callable
}

/// Provider tool definitions for the declared set: `descriptions` rewrites and
/// `hidden_declarations` (omitted but still active) applied (upstream
/// `_applyToolLoadout`'s description merge plus the `_installHiddenDeclarationsProjection`).
///
/// Every declared schema gets the shared `background` argument injected (the
/// tool's own `parameters()` stays untouched — the scheduler strips the key
/// before validation, so `additionalProperties: false` schemas accept it).
pub fn declared_tool_definitions(
    declared: &[Arc<dyn AgentTool>],
    descriptions: &std::collections::BTreeMap<String, String>,
    hidden_declarations: &std::collections::BTreeSet<String>,
) -> Vec<crate::ai::types::Tool> {
    declared
        .iter()
        .filter(|tool| !hidden_declarations.contains(tool.name()))
        .map(|tool| crate::ai::types::Tool {
            name: tool.name().to_string(),
            description: descriptions
                .get(tool.name())
                .cloned()
                .unwrap_or_else(|| tool.description().to_string()),
            parameters: with_background_argument(
                tool.parameters()
                    .unwrap_or_else(|| serde_json::json!({"type": "object"})),
            ),
            constrained_sampling: tool.constrained_sampling(),
        })
        .collect()
}

/// Inject the shared `background` argument into one declared tool schema.
///
/// The argument is scheduler-level, not tool-level: the model passes it on
/// every tool, and `prepare_calls` strips it before validation, so the tool
/// itself never sees it. A non-object schema (a malformed declaration) is
/// returned unchanged.
fn with_background_argument(mut parameters: serde_json::Value) -> serde_json::Value {
    let Some(object) = parameters.as_object_mut() else {
        return parameters;
    };
    let properties = object
        .entry("properties")
        .or_insert_with(|| serde_json::json!({}));
    if let Some(properties) = properties.as_object_mut() {
        properties.insert(
            "background".to_owned(),
            serde_json::json!({
                "type": "boolean",
                "description": "Run this call in the background: it returns immediately and the real result is delivered as a separate message once it finishes.",
            }),
        );
    }
    parameters
}

// ---------------------------------------------------------------------------
// Extension integration
// ---------------------------------------------------------------------------

/// A tool registered by a JS extension.
///
/// Metadata is snapshotted from the runtime's [`crate::extensions::ToolDef`];
/// execution delegates back into QuickJS through
/// [`crate::extensions::ExtensionRuntime::call_tool`].
pub struct ExtensionTool {
    runtime: crate::extensions::ExtensionHandle,
    /// Provider-facing name. `AgentTool::name` borrows it and `run` clones the
    /// `Arc` for dispatch, so no process-wide intern pool is needed.
    name: Arc<str>,
    /// Human-readable label declared by the extension (upstream
    /// `ToolDefinition.label`); falls back to the name when the definition
    /// omitted it.
    label: Arc<str>,
    description: Arc<str>,
    parameters: Option<serde_json::Value>,
    /// Declared `outputSchema` (validated JS-side; published to the provider).
    output_schema: Option<serde_json::Value>,
    execution_mode: ToolExecutionMode,
    /// Snapshotted exposure (upstream `ToolDefinition.exposure`).
    exposure: ToolExposure,
    /// Snapshotted namespace.
    namespace: Option<ToolNamespace>,
    /// Snapshotted annotations.
    annotations: Option<ToolAnnotations>,
    /// Snapshotted `defaultActive`.
    default_active: bool,
    /// Snapshotted `reachesTools` (upstream `ToolDefinition.reachesTools`): the
    /// live hook stays inside QuickJS (indirected by name through
    /// `state.toolReachHooks`), so the registry export carries the hook's value
    /// for its empty-input probe and this is that snapshot. `None` for a tool
    /// that declares no reach.
    ///
    /// The async MCP wait resolves the LIVE value per call (a JS tool's hook is
    /// a function of the call's input); this snapshot is the synchronous face
    /// the discovery-activation probe and the registry consumers read.
    reach: Option<ToolReach>,
}

impl ExtensionTool {
    fn new(runtime: crate::extensions::ExtensionHandle, def: crate::extensions::ToolDef) -> Self {
        // Owned per live tool instead of leaked into a process-wide intern
        // pool: registry rebuilds drop the previous tools (freeing the text),
        // so distinct/attacker-controlled extension names cannot accumulate
        // for the process lifetime.
        let name: Arc<str> = Arc::from(def.name);
        let label: Arc<str> = match def.label.as_deref().map(str::trim) {
            Some(label) if !label.is_empty() => Arc::from(label),
            _ => Arc::clone(&name),
        };
        let description: Arc<str> = if def.description.is_empty() {
            Arc::clone(&name)
        } else {
            Arc::from(def.description)
        };
        let execution_mode = match def.execution_mode.as_str() {
            "sequential" => ToolExecutionMode::Sequential,
            _ => ToolExecutionMode::Parallel,
        };
        let parameters = if def.parameters.is_null() {
            None
        } else {
            Some(def.parameters)
        };
        let output_schema = if def.output_schema.is_null() {
            None
        } else {
            Some(def.output_schema)
        };
        let exposure = ToolExposure::parse(&def.exposure).unwrap_or(ToolExposure::Direct);
        let namespace = def
            .namespace
            .filter(|namespace| !namespace.name.is_empty())
            .map(|namespace| ToolNamespace {
                name: namespace.name,
                aliases: namespace.aliases,
                description: namespace.description,
                instructions: namespace.instructions,
            });
        let annotations = def.annotations.map(|annotations| ToolAnnotations {
            read_only_hint: annotations.read_only_hint,
            destructive_hint: annotations.destructive_hint,
            idempotent_hint: annotations.idempotent_hint,
            open_world_hint: annotations.open_world_hint,
        });
        // The JS `reachesTools` hook cannot cross the boundary as a function:
        // the registry export carries the probe value (D23 (f)); a declared
        // reach the probe could not serialize keeps its presence as an empty
        // word list, so the activation gate still sees the declaration.
        let reach = def.reaches_tools.map(|snapshot| {
            if snapshot.is_all() {
                ToolReach::All
            } else {
                ToolReach::Names(snapshot.words())
            }
        });
        Self {
            runtime,
            name,
            label,
            description,
            parameters,
            output_schema,
            execution_mode,
            exposure,
            namespace,
            annotations,
            default_active: def.default_active,
            reach,
        }
    }

    /// Whether the tool declares a JS `reachesTools` hook (so the MCP wait
    /// pulls its live per-call value through the runtime instead of trusting
    /// the export snapshot).
    pub fn declares_js_reach(&self) -> bool {
        self.reach.is_some()
    }
}

/// Unbinds a JS runtime's nested-call executor when dropped.
///
/// The runtime bind must survive exactly as long as the tool call's future.
/// An explicit `unbind` after `.await` only runs on normal completion; a
/// cancelled/dropped future (turn abort, scheduler shutdown) would leave the
/// executor bound forever. A drop guard covers every exit path — return,
/// cancellation and unwind.
struct NestedExecutorBindGuard {
    runtime: crate::extensions::ExtensionHandle,
    tool_call_id: String,
}

impl Drop for NestedExecutorBindGuard {
    fn drop(&mut self) {
        self.runtime.unbind_nested_executor(&self.tool_call_id);
    }
}

impl AgentTool for ExtensionTool {
    fn name(&self) -> &str {
        &self.name
    }

    /// The label the extension declared (`ToolDefinition.label`), or the name
    /// when it declared none (upstream `registerTool` requires a label, so the
    /// fallback only covers a malformed definition).
    fn label(&self) -> &str {
        &self.label
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn parameters(&self) -> Option<serde_json::Value> {
        self.parameters.clone()
    }

    fn output_schema(&self) -> Option<serde_json::Value> {
        self.output_schema.clone()
    }

    fn execution_mode(&self) -> ToolExecutionMode {
        self.execution_mode
    }

    fn exposure(&self) -> ToolExposure {
        self.exposure
    }

    fn namespace(&self) -> Option<ToolNamespace> {
        self.namespace.clone()
    }

    fn annotations(&self) -> Option<ToolAnnotations> {
        self.annotations
    }

    fn default_active(&self) -> bool {
        self.default_active
    }

    /// The export snapshot of the JS `reachesTools` hook (upstream
    /// `ToolDefinition.reachesTools`): the hook's value for the registry
    /// export's empty-input probe — `"all"` or the words it returned, an empty
    /// word list when the hook declared a reach the probe could not serialize.
    /// The hook itself is a function of the call's input, so the MCP wait
    /// resolves the live value per call through the runtime and only falls
    /// back here when that resolution is unavailable.
    fn reaches_tools(&self, _input: &serde_json::Value) -> Option<ToolReach> {
        self.reach.clone()
    }

    fn as_any(&self) -> Option<&dyn std::any::Any> {
        Some(self)
    }

    fn run(&self, args: &serde_json::Value, ctx: &ToolContext) -> BoxFuture<'static, ToolResult> {
        if ctx.is_aborted() {
            return Box::pin(async { ToolResult::error("Operation aborted") });
        }
        let runtime = self.runtime.clone();
        let tool_name = Arc::clone(&self.name);
        let args = args.clone();
        // The real tool call id: the scheduler binds the nested-call scope to
        // the executing call's id before `run`, so the executor's caller id
        // is the live id — including `<parent>/<n>` nested ids (upstream
        // createToolContext(toolCallId)).
        let tool_call_id = ctx
            .nested
            .as_ref()
            .map(|executor| executor.caller_id().to_owned())
            .unwrap_or_default();
        // Bind the nested executor for the call's duration so the JS
        // `ctx.executeTool` round-trips nested calls through the same
        // gate/scheduler path (upstream ExtensionToolContext).
        //
        // The clone is taken only when the id is real: an empty id means the
        // context carries no rebindable executor (the scheduler scopes every
        // scheduled call via `with_call_scope`), and binding under an empty
        // id would hand the JS round-trip an unscoped executor. The context
        // already shares the executor behind an `Arc`, so the clone is a
        // refcount bump and the bind moves that same `Arc` into the registry
        // — no executor (and no `caller_id` `String`) is re-allocated per
        // call.
        let bind_executor = if tool_call_id.is_empty() {
            None
        } else {
            ctx.nested.as_ref().map(Arc::clone)
        };
        Box::pin(async move {
            // RAII: the guard unbinds on every exit path, including a future
            // dropped mid-`call_tool`, where the former post-await unbind was
            // skipped and the runtime kept the executor (and its recorder)
            // bound to this call id after the call was gone.
            let _bind_guard = if let Some(executor) = bind_executor {
                runtime.bind_nested_executor(&tool_call_id, executor);
                Some(NestedExecutorBindGuard {
                    runtime: runtime.clone(),
                    tool_call_id: tool_call_id.clone(),
                })
            } else {
                None
            };
            let result = runtime.call_tool(&tool_name, &tool_call_id, &args).await;
            match result {
                Ok(result) => {
                    let text = result
                        .content
                        .iter()
                        .map(|item| item.text.as_str())
                        .collect::<Vec<_>>()
                        .join("\n");
                    let mut tool_result = ToolResult::ok(text);
                    if !result.details.is_null() {
                        tool_result.details = Some(result.details);
                    }
                    if !result.structured_content.is_null() {
                        tool_result.structured_content = Some(result.structured_content);
                    }
                    tool_result.terminate = result.terminate;
                    tool_result
                }
                Err(error) => ToolResult::error(error.to_string()),
            }
        })
    }
}

/// Register every tool currently exposed by the extension runtime (upsert by
/// name; re-merging after `session_start` picks up tools registered by its
/// handlers).
pub fn merge_extension_tools(
    registry: &mut ToolRegistry,
    runtime: &crate::extensions::ExtensionHandle,
) {
    for def in runtime.tool_defs() {
        registry.register(Arc::new(ExtensionTool::new(runtime.clone(), def)));
    }
}

/// Register every tool from the unified plugin registry into a
/// [`ToolRegistry`] (first registration per name wins across the ordered
/// plugin list — Rust builtins and JS extensions share the same rule).
///
/// Native Rust tools enter the registry as their own [`AgentTool`] objects;
/// JS tools wrap the runtime in an [`ExtensionTool`]. `active_override`
/// (the persisted `setActiveTools` names) restricts the final active set
/// when present.
pub fn merge_plugin_tools(
    registry: &mut ToolRegistry,
    host: &crate::plugins::PluginHost,
    active_override: Option<&[String]>,
) {
    let mut seen: std::collections::BTreeSet<String> =
        registry.names().into_iter().map(str::to_owned).collect();
    for (def, backend) in host.registry().resolved_tools() {
        if !seen.insert(def.name.clone()) {
            continue;
        }
        match backend {
            crate::plugins::ToolBackend::Rust(tool) => {
                registry.register(tool.clone());
            }
            crate::plugins::ToolBackend::Js { execution_mode } => {
                let runtime = match host.js_handle() {
                    Some(runtime) => runtime.clone(),
                    None => continue,
                };
                let mut def = def.clone();
                def.execution_mode = match execution_mode {
                    crate::agent::tool_batch::ToolExecutionMode::Sequential => {
                        "sequential".to_owned()
                    }
                    crate::agent::tool_batch::ToolExecutionMode::Parallel => "parallel".to_owned(),
                };
                registry.register(Arc::new(ExtensionTool::new(runtime, def)));
            }
        }
    }
    // Apply the persisted `setActiveTools` override: the active set shrinks
    // to exactly the requested names (upstream setActiveTools semantics).
    if let Some(override_names) = active_override {
        let active: std::collections::BTreeSet<&str> =
            override_names.iter().map(String::as_str).collect();
        let drop_names: Vec<String> = registry
            .names()
            .into_iter()
            .filter(|name| !active.contains(name))
            .map(str::to_owned)
            .collect();
        for name in drop_names {
            registry.remove(&name);
        }
    }
}

/// Outcome of routing a tool batch through the extension `tool_call` gate.
pub struct GatedToolBatch {
    /// Calls that survived the gate. Arguments may have been replaced by a
    /// handler's mutated `input`.
    pub calls: Vec<AgentToolCall>,
    /// Error results synthesized for calls an extension handler blocked; the
    /// corresponding tool never runs.
    pub blocked: Vec<crate::agent::tool_batch::FinalizedToolCall>,
    /// Executed input per call id (post-rewrite), used for `tool_result`.
    pub inputs: std::collections::HashMap<String, serde_json::Value>,
}

/// The extension/plugin faces a tool scheduler dispatches the per-call tool
/// contracts to.
///
/// The scheduler owns the upstream per-call timing (`beforeToolCall` /
/// `afterToolCall` run inside the tool loop): `tool_call` is dispatched after
/// schema validation and before execution, `tool_result` after the tool and
/// the `after` hook and before that call's `tool_execution_end`. The JS face
/// rides the caller's runtime handle and the native face rides the unified
/// plugin host's ordered list.
///
/// The two `dispatch_*` flags mirror the historical "is there an observer at
/// all" pre-checks: a JS runtime always dispatches (its handler table is live
/// inside QuickJS), while a native host only dispatches for an event some
/// non-JS plugin actually declared, so an idle host costs no walk per call.
#[derive(Clone)]
pub struct ToolEventFaces {
    extensions: Option<crate::extensions::ExtensionHandle>,
    plugins: Option<Arc<crate::plugins::PluginHost>>,
    tool_call: bool,
    tool_result: bool,
}

impl ToolEventFaces {
    /// Build the faces for a run. `tool_call`/`tool_result` dispatch is
    /// enabled when the JS runtime is present or the host declares a native
    /// handler for that event (the historical gate conditions).
    pub fn new(
        extensions: Option<crate::extensions::ExtensionHandle>,
        plugins: Option<Arc<crate::plugins::PluginHost>>,
    ) -> Self {
        let native_call = plugins
            .as_ref()
            .is_some_and(|host| host.has_native_handlers("tool_call"));
        let native_result = plugins
            .as_ref()
            .is_some_and(|host| host.has_native_handlers("tool_result"));
        Self {
            tool_call: extensions.is_some() || native_call,
            tool_result: extensions.is_some() || native_result,
            extensions,
            plugins,
        }
    }

    /// Whether either contract has an observer.
    pub fn enabled(&self) -> bool {
        self.tool_call || self.tool_result
    }

    /// Whether `tool_call` handlers must run.
    pub fn dispatches_tool_call(&self) -> bool {
        self.tool_call
    }

    /// Whether `tool_result` handlers must run.
    pub fn dispatches_tool_result(&self) -> bool {
        self.tool_result
    }

    /// The JS extension face.
    pub fn extensions(&self) -> Option<&crate::extensions::ExtensionHandle> {
        self.extensions.as_ref()
    }

    /// The unified native plugin face.
    pub fn plugins(&self) -> Option<&crate::plugins::PluginHost> {
        self.plugins.as_deref()
    }
}

/// Whether `plugin` is the QuickJS extensions adapter.
///
/// The adapter is the only plugin whose events are already dispatched through
/// the caller's [`crate::extensions::ExtensionHandle`] (`LoopDeps::extensions`
/// / the session's JS face); the ordered fan-out below must call the handle
/// ONCE at the adapter's list position instead of invoking the adapter a
/// second time.
fn is_js_adapter(plugin: &Arc<dyn crate::plugins::AgentPlugin>) -> bool {
    plugin
        .as_any()
        .is_some_and(|any| any.is::<crate::plugins::JsExtensionsPlugin>())
}

/// Dispatch one result-contract event (`tool_call` / `tool_result` /
/// `user_bash`, ...) through the host's ordered plugin list, keeping the JS
/// face on the caller's runtime handle and giving every native plugin its
/// outcome.
///
/// This is the merge-order twin of [`crate::plugins::PluginHost::emit`]:
///
/// - plugins are visited in the host's documented order (enabled builtins in
///   their path-plan position, the JS adapter at the position of its first
///   file path, inline plugins last), so mixed faces merge exactly as
///   `emit` documents;
/// - the JS adapter is dispatched through `runtime` AT ITS SLOT (once); the
///   adapter itself is never invoked a second time, so a host-built session
///   cannot double-dispatch the JS face;
/// - in-place payload mutations chain in that same order (a `tool_call`
///   `input` rewrite by an earlier handler is visible to later ones), and an
///   outcome that short-circuits (`block: true`, `cancel: true`, ...) stops
///   the walk exactly like `emit`;
/// - `host == None` preserves the historical JS-only route; `runtime == None`
///   dispatches the host's own JS adapter (if present) plus the natives.
///
/// `structuredContent` is pir's extension of the `tool_result` wire shape and
/// is not part of [`crate::plugins::PluginEventOutcome`]; the JS face's value
/// is carried through explicitly so switching a host on never drops it.
pub(crate) async fn dispatch_hosted_event(
    runtime: Option<&crate::extensions::ExtensionHandle>,
    host: Option<&crate::plugins::PluginHost>,
    event: &str,
    payload: &mut JsonValue,
) -> anyhow::Result<crate::extensions::EmitOutcome> {
    let Some(host) = host else {
        return match runtime {
            Some(runtime) => runtime.emit(event, payload).await,
            None => Ok(crate::extensions::EmitOutcome {
                result: JsonValue::Null,
                payload: JsonValue::Null,
                errors: Vec::new(),
            }),
        };
    };

    let mut acc = crate::plugins::MergedEmit::new();
    let mut js_result: Option<JsonValue> = None;
    let js_in_list = host.plugins().iter().any(is_js_adapter);
    if !js_in_list {
        // A caller-supplied handle the host does not own (not a session
        // shape): keep the historical route — the JS face runs before the
        // native list, whose outcomes then merge last-wins.
        if let Some(runtime) = runtime {
            let outcome = runtime.emit(event, payload).await?;
            // Build the merged outcome from a borrow, then MOVE the result
            // into `js_result`: a later structured-content override only reads
            // `js_result`, so the former clone copied a potentially large
            // handler result once per event for nothing.
            let js_outcome = crate::plugins::PluginEventOutcome::Js {
                result: Box::new(crate::plugins::PluginEventOutcome::from_wire(
                    event,
                    &outcome.result,
                )),
                errors: outcome.errors,
            };
            js_result = Some(outcome.result);
            acc.merge(event, "<js-extensions>", js_outcome);
        }
    }

    for plugin in host.plugins() {
        if acc.stop {
            break;
        }
        if is_js_adapter(plugin) {
            match runtime {
                Some(runtime) => {
                    let outcome = runtime.emit(event, payload).await?;
                    // See the runtime-only branch above: move, never clone,
                    // the handler result into the deferred JS override.
                    let js_outcome = crate::plugins::PluginEventOutcome::Js {
                        result: Box::new(crate::plugins::PluginEventOutcome::from_wire(
                            event,
                            &outcome.result,
                        )),
                        errors: outcome.errors,
                    };
                    js_result = Some(outcome.result);
                    acc.merge(event, &plugin.info().path, js_outcome);
                }
                // The host owns the handle: dispatch through the adapter,
                // which calls the same runtime and writes payload mutations
                // back in place.
                None => {
                    let outcome = plugin
                        .on_event(crate::plugins::PluginEvent::new(event, payload))
                        .await;
                    acc.merge(event, &plugin.info().path, outcome);
                }
            }
            continue;
        }
        let outcome = plugin
            .on_event(crate::plugins::PluginEvent::new(event, payload))
            .await;
        acc.merge(event, &plugin.info().path, outcome);
    }

    let mut result = acc.result;
    if event == "tool_result" && result.is_object() {
        // Move the JS face's structured content (its only consumer is this
        // override); the former path cloned it into `result` while the
        // deferred `js_result` was about to be dropped.
        if let Some(structured) = js_result
            .as_mut()
            .and_then(JsonValue::as_object_mut)
            .and_then(|js| js.remove("structuredContent"))
            .filter(|value| !value.is_null())
        {
            result["structuredContent"] = structured;
        }
    }
    Ok(crate::extensions::EmitOutcome {
        result,
        payload: if event == "tool_call" && payload.is_object() {
            payload.clone()
        } else {
            JsonValue::Null
        },
        errors: acc.errors,
    })
}

/// Route every tool call through the extension `tool_call` event before
/// execution.
///
/// For each call a `{type, toolCallId, toolName, input}` payload is emitted;
/// a merged `{block: true, reason}` outcome turns the call into an error
/// result (the tool must not run), a `terminate: true` hint is recorded on
/// that blocked result (honored by the batch-level `should_terminate` only
/// when every finalized result in the batch carries it, types.ts:1430-1434),
/// and a mutated payload `input` replaces the arguments passed to execution.
/// A failed dispatch blocks the call with the dispatch error, mirroring
/// upstream's "extension failed, blocking execution" behavior.
/// `parent_call_id` adds `parentToolCallId` to the payload for calls another
/// tool made (upstream nested-call semantics).
pub async fn gate_tool_calls(
    runtime: &crate::extensions::ExtensionHandle,
    calls: Vec<AgentToolCall>,
    parent_call_id: Option<&str>,
) -> GatedToolBatch {
    gate_tool_calls_with_host(Some(runtime), None, calls, parent_call_id).await
}

/// One call's `tool_call` verdict (upstream `ToolCallEventResult` plus the
/// possibly-rewritten `input`).
#[derive(Debug, Clone)]
pub struct ToolCallGateDecision {
    /// The call must not execute; `reason` is the model-visible failure.
    pub blocked: bool,
    /// Block reason (the default is filled in by the caller).
    pub reason: Option<String>,
    /// Upstream `ToolCallEventResult.terminate` hint for a blocked call.
    pub terminate: bool,
    /// The payload the tool must execute with. Handlers mutate `input` in
    /// place; the result is NOT re-validated (upstream: "No re-validation is
    /// performed after mutation").
    pub input: serde_json::Value,
}

/// Dispatch one `tool_call` event through the given faces and return the
/// merged verdict.
///
/// The caller owns the timing (after schema validation, before execution).
/// Dispatch failures are returned so the caller can block the call, mirroring
/// upstream's "extension failed, blocking execution" behavior.
pub async fn gate_single_tool_call(
    runtime: Option<&crate::extensions::ExtensionHandle>,
    host: Option<&crate::plugins::PluginHost>,
    call_id: &str,
    tool_name: &str,
    input: serde_json::Value,
    parent_call_id: Option<&str>,
) -> anyhow::Result<ToolCallGateDecision> {
    let mut payload = serde_json::json!({
        "type": "tool_call",
        "toolCallId": call_id,
        "toolName": tool_name,
        "input": input,
    });
    if let Some(parent) = parent_call_id {
        payload["parentToolCallId"] = serde_json::Value::String(parent.to_owned());
    }
    let outcome = dispatch_hosted_event(runtime, host, "tool_call", &mut payload).await?;
    let blocked = outcome
        .result
        .get("block")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    let reason = outcome
        .result
        .get("reason")
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned);
    let terminate = outcome
        .result
        .get("terminate")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    // The in-place mutated payload is the executed input. A handler that
    // deleted the key leaves `Null`, exactly what the event carried.
    let input = payload
        .as_object_mut()
        .and_then(|object| object.remove("input"))
        .unwrap_or(serde_json::Value::Null);
    Ok(ToolCallGateDecision {
        blocked,
        reason,
        terminate,
        input,
    })
}

/// [`gate_tool_calls`] with the unified plugin host's native fan-out.
///
/// The event is dispatched through [`dispatch_hosted_event`]: the JS face
/// (when `runtime` is supplied) and every Rust plugin merge in the host's
/// ordered-list order, so a native `tool_call` handler can block exactly like
/// a JS one (`{block: true, reason}` → the synthesized error result is the
/// model-visible failure and the tool never runs) and can rewrite `input`
/// in place exactly like a JS one (the rewritten payload flows to the
/// scheduler). `host == None` is the historical JS-only gate.
pub async fn gate_tool_calls_with_host(
    runtime: Option<&crate::extensions::ExtensionHandle>,
    host: Option<&crate::plugins::PluginHost>,
    calls: Vec<AgentToolCall>,
    parent_call_id: Option<&str>,
) -> GatedToolBatch {
    let mut gated = GatedToolBatch {
        calls: Vec::with_capacity(calls.len()),
        blocked: Vec::new(),
        inputs: std::collections::HashMap::new(),
    };
    for mut call in calls {
        match gate_single_tool_call(
            runtime,
            host,
            &call.call_id,
            &call.tool_name,
            call.arguments.clone(),
            parent_call_id,
        )
        .await
        {
            Ok(decision) => {
                if decision.blocked {
                    let reason = decision.reason.unwrap_or_else(|| {
                        format!("Tool '{}' was blocked by an extension", call.tool_name)
                    });
                    // Upstream `ToolCallEventResult.terminate` (types.ts:1430-
                    // 1434): a hint that the agent should stop after the
                    // current tool batch when this call is blocked — honored
                    // only when EVERY finalized result in the batch carries
                    // it (ToolBatchOutcome::should_terminate's all() rule;
                    // a mixed batch keeps going). Record the hint on the
                    // synthesized blocked result so the agent-side batch
                    // decision sees it; the blocked call itself never runs.
                    let mut result = ToolResult::error(reason);
                    result.terminate = decision.terminate;
                    gated
                        .blocked
                        .push(crate::agent::tool_batch::FinalizedToolCall {
                            index: call.index,
                            call_id: call.call_id.clone(),
                            tool_name: call.tool_name.clone(),
                            result,
                            usage: None,
                            duration_ms: None,
                        });
                    continue;
                }
                call.arguments = decision.input;
                gated
                    .inputs
                    .insert(call.call_id.clone(), call.arguments.clone());
                gated.calls.push(call);
            }
            Err(error) => {
                gated
                    .blocked
                    .push(crate::agent::tool_batch::FinalizedToolCall {
                        index: call.index,
                        call_id: call.call_id.clone(),
                        tool_name: call.tool_name.clone(),
                        result: ToolResult::error(format!(
                            "Extension failed, blocking execution: {error}"
                        )),
                        usage: None,
                        duration_ms: None,
                    });
            }
        }
    }
    gated
}

/// A `tool_result` handler's applied changes for one finalized call (upstream
/// `ToolResultEventResult`), keyed by tool call id. The host merges these back
/// onto the finalized tool results before they become transcript messages.
#[derive(Debug, Clone, Default)]
pub struct ToolResultMerge {
    /// Flattened text of the handler's replacement `content` (newline-joined
    /// text blocks). Kept for consumers that read the text view; the full
    /// array, images included, rides [`Self::content_blocks`]. A handler that
    /// returns a bare string (legacy native shape) sets only this field.
    pub content: Option<String>,
    /// Full replacement content BLOCK array (upstream
    /// `ToolResultEventResult.content`): text blocks replace the result text,
    /// image blocks replace the result images. This is the upstream
    /// "replace the whole array" semantics: a text-only array removes the
    /// old images instead of leaving them behind.
    pub content_blocks: Option<Vec<ToolResultContentBlock>>,
    /// Replacement image blocks alone (a handler that returns `images`
    /// without `content`); applied after any `content_blocks`.
    pub images: Option<Vec<crate::ai::types::ImageContent>>,
    pub details: Option<serde_json::Value>,
    pub structured_content: Option<serde_json::Value>,
    pub is_error: Option<bool>,
    /// Usage reported by the handler; applied to the tool-result message.
    pub usage: Option<serde_json::Value>,
}

/// Best-effort `tool_result` notification for each finalized call, returning
/// the per-call handler merge so the host can apply it to the transcript.
/// Dispatch failures are ignored so tool execution is never affected by them.
/// `parent_call_id` adds `parentToolCallId` to the payload for calls another
/// tool made (upstream nested-call semantics).
pub async fn emit_tool_results(
    runtime: &crate::extensions::ExtensionHandle,
    inputs: &std::collections::HashMap<String, serde_json::Value>,
    results: &[crate::agent::tool_batch::FinalizedToolCall],
    parent_call_id: Option<&str>,
) -> std::collections::HashMap<String, ToolResultMerge> {
    emit_tool_results_skipping(
        runtime,
        inputs,
        results,
        parent_call_id,
        &std::collections::BTreeSet::new(),
    )
    .await
}

/// [`emit_tool_results`] with the unified plugin host's native fan-out: a
/// Rust `tool_result` handler's field overlay (`content`/`details`/
/// `isError`/`usage`) merges with the JS face's in the host's ordered-list
/// order, and the finalized merge is applied to the transcript exactly like
/// the JS-only route. `host == None` keeps the historical behavior.
pub async fn emit_tool_results_with_host(
    runtime: Option<&crate::extensions::ExtensionHandle>,
    host: Option<&crate::plugins::PluginHost>,
    inputs: &std::collections::HashMap<String, serde_json::Value>,
    results: &[crate::agent::tool_batch::FinalizedToolCall],
    parent_call_id: Option<&str>,
) -> std::collections::HashMap<String, ToolResultMerge> {
    emit_tool_results_skipping_with_host(
        runtime,
        host,
        inputs,
        results,
        parent_call_id,
        &std::collections::BTreeSet::new(),
    )
    .await
}

/// [`emit_tool_results`] with call ids excluded: a detached (background)
/// call's in-batch result is the PLACEHOLDER, and its real completion fires
/// `tool_result` again with the same id — exactly one event per call is the
/// contract (the nested path enforces it by skipping detached calls). The
/// completion owns the event; the placeholder must not leak one.
pub async fn emit_tool_results_skipping(
    runtime: &crate::extensions::ExtensionHandle,
    inputs: &std::collections::HashMap<String, serde_json::Value>,
    results: &[crate::agent::tool_batch::FinalizedToolCall],
    parent_call_id: Option<&str>,
    skip_call_ids: &std::collections::BTreeSet<String>,
) -> std::collections::HashMap<String, ToolResultMerge> {
    emit_tool_results_skipping_with_host(
        Some(runtime),
        None,
        inputs,
        results,
        parent_call_id,
        skip_call_ids,
    )
    .await
}

/// [`emit_tool_results_skipping`] with the unified plugin host's native
/// fan-out (see [`dispatch_hosted_event`] for the ordered, once-per-face
/// dispatch and merge rules).
#[allow(clippy::too_many_arguments)]
pub async fn emit_tool_results_skipping_with_host(
    runtime: Option<&crate::extensions::ExtensionHandle>,
    host: Option<&crate::plugins::PluginHost>,
    inputs: &std::collections::HashMap<String, serde_json::Value>,
    results: &[crate::agent::tool_batch::FinalizedToolCall],
    parent_call_id: Option<&str>,
    skip_call_ids: &std::collections::BTreeSet<String>,
) -> std::collections::HashMap<String, ToolResultMerge> {
    let mut merges = std::collections::HashMap::new();
    for finalized in results {
        if skip_call_ids.contains(&finalized.call_id) {
            continue;
        }
        if let Some(merge) = emit_tool_result_for_call(
            runtime,
            host,
            inputs.get(&finalized.call_id),
            finalized,
            parent_call_id,
        )
        .await
        {
            merges.insert(finalized.call_id.clone(), merge);
        }
    }
    merges
}

/// Dispatch one finalized call's `tool_result` notification and return the
/// handler merge (upstream `ToolResultEventResult`), if any field was touched.
///
/// The payload is the upstream base event: complete `(TextContent |
/// ImageContent)[]` content, `isError`, optional `usage`/`input`/`details`/
/// `structuredContent` and (for nested calls) `parentToolCallId`. Dispatch
/// failures are swallowed: tool execution is never affected by a handler
/// error.
pub async fn emit_tool_result_for_call(
    runtime: Option<&crate::extensions::ExtensionHandle>,
    host: Option<&crate::plugins::PluginHost>,
    input: Option<&serde_json::Value>,
    finalized: &crate::agent::tool_batch::FinalizedToolCall,
    parent_call_id: Option<&str>,
) -> Option<ToolResultMerge> {
    // Upstream `ToolResultEventBase`: `content` is the complete
    // `(TextContent | ImageContent)[]` the tool returned, and `usage` is
    // the tool execution's own usage when available. Both were missing
    // here (text-only content dropped images; usage was never sent).
    let mut payload = serde_json::json!({
        "type": "tool_result",
        "toolCallId": finalized.call_id,
        "toolName": finalized.tool_name,
        "content": tool_result_content_blocks(&finalized.result),
        "isError": finalized.result.is_error,
    });
    if let Some(usage) = &finalized.usage {
        payload["usage"] = serde_json::to_value(usage).unwrap_or(serde_json::Value::Null);
    }
    if let Some(parent) = parent_call_id {
        payload["parentToolCallId"] = serde_json::Value::String(parent.to_owned());
    }
    if let Some(input) = input {
        payload["input"] = input.clone();
    }
    if let Some(details) = &finalized.result.details {
        payload["details"] = details.clone();
    }
    if let Some(structured) = &finalized.result.structured_content {
        payload["structuredContent"] = structured.clone();
    }
    let outcome = dispatch_hosted_event(runtime, host, "tool_result", &mut payload)
        .await
        .ok()?;
    let result = outcome.result;
    let content_value = result.get("content");
    let merge = ToolResultMerge {
        // A bare string is the legacy text-only form (native
        // plugins); an array is the upstream full block form.
        content: match content_value {
            Some(JsonValue::String(text)) => Some(text.clone()),
            Some(value) => flattened_text(value).filter(|text| !text.is_empty()),
            None => None,
        },
        // The upstream array form: parse every text/image block
        // so the apply step can replace the FULL content (old
        // images included).
        content_blocks: content_value.and_then(parse_content_blocks),
        images: result.get("images").and_then(parse_image_blocks),
        details: result.get("details").cloned().filter(|d| !d.is_null()),
        structured_content: result
            .get("structuredContent")
            .cloned()
            .filter(|s| !s.is_null()),
        is_error: result.get("isError").and_then(JsonValue::as_bool),
        usage: result.get("usage").cloned().filter(|u| !u.is_null()),
    };
    let touched = merge.content.is_some()
        || merge.content_blocks.is_some()
        || merge.images.is_some()
        || merge.details.is_some()
        || merge.structured_content.is_some()
        || merge.is_error.is_some()
        || merge.usage.is_some();
    touched.then_some(merge)
}

/// Apply a `tool_result` handler merge onto a finalized call's result, at the
/// scheduler's per-call finalize point (after the tool and its `after` hook,
/// before that call's `tool_execution_end`).
///
/// The handler-reported usage REPLACES the `after` hook's usage on the
/// finalized call (upstream `afterResult.usage ?? result.usage`); an
/// unparseable report is ignored, exactly like the former loop-side merge.
/// A no-op when no face observes `tool_result`.
pub async fn dispatch_tool_result_merge(
    faces: &ToolEventFaces,
    input: Option<&serde_json::Value>,
    parent_call_id: Option<&str>,
    finalized: &mut crate::agent::tool_batch::FinalizedToolCall,
) {
    if !faces.dispatches_tool_result() {
        return;
    }
    if let Some(merge) = emit_tool_result_for_call(
        faces.extensions(),
        faces.plugins(),
        input,
        finalized,
        parent_call_id,
    )
    .await
    {
        if let Some(value) = apply_tool_result_merge(&mut finalized.result, &merge) {
            if let Some(usage) = parse_usage_json(&value) {
                finalized.usage = Some(usage);
            }
        }
    }
}

/// The `tool_execution_end` wire result of a finalized call: the upstream
/// `AgentToolResult` shape plus its `usage` when the tool or a `tool_result`
/// handler reported one, so the end frame and the tool-result message agree.
pub fn finalized_tool_result_event_json(
    finalized: &crate::agent::tool_batch::FinalizedToolCall,
) -> serde_json::Value {
    let mut value = tool_result_event_json(&finalized.result);
    if let Some(usage) = &finalized.usage {
        if let Ok(usage) = serde_json::to_value(usage) {
            value["usage"] = usage;
        }
    }
    value
}

/// Joined text of a `tool_result` handler's replacement `content` (the event
/// carries `{type: "text"|"image", ...}` blocks; the host tool result keeps
/// the flattened text form).
fn flattened_text(value: &JsonValue) -> Option<String> {
    let blocks = value.as_array()?;
    let mut text = String::new();
    for block in blocks {
        if let Some(part) = block.get("text").and_then(JsonValue::as_str) {
            if !text.is_empty() {
                text.push('\n');
            }
            text.push_str(part);
        }
    }
    Some(text)
}

/// Parse a handler's upstream `content: (TextContent | ImageContent)[]`
/// replacement into the internal block form. Unknown block types are ignored
/// (they cannot be represented in a tool result); an empty array parses to
/// `Some(vec![])`, which is upstream's valid "replace with nothing".
fn parse_content_blocks(value: &JsonValue) -> Option<Vec<ToolResultContentBlock>> {
    let blocks = value.as_array()?;
    let mut parsed = Vec::with_capacity(blocks.len());
    for block in blocks {
        match block.get("type").and_then(JsonValue::as_str) {
            Some("image") => {
                let (Some(data), Some(mime_type)) = (
                    block.get("data").and_then(JsonValue::as_str),
                    block.get("mimeType").and_then(JsonValue::as_str),
                ) else {
                    continue;
                };
                parsed.push(ToolResultContentBlock::Image(
                    crate::ai::types::ImageContent {
                        data: data.to_owned(),
                        mime_type: mime_type.to_owned(),
                    },
                ));
            }
            // `text` (and untyped blocks, the legacy wire shape) contribute
            // their `text` field when present.
            _ => {
                if let Some(text) = block.get("text").and_then(JsonValue::as_str) {
                    parsed.push(ToolResultContentBlock::Text(text.to_owned()));
                }
            }
        }
    }
    Some(parsed)
}

/// Parse a handler's `images` replacement array (`[{data, mimeType}]`, the
/// same block shape `ImageContent` serializes to).
fn parse_image_blocks(value: &JsonValue) -> Option<Vec<crate::ai::types::ImageContent>> {
    let blocks = value.as_array()?;
    let mut images = Vec::with_capacity(blocks.len());
    for block in blocks {
        let (Some(data), Some(mime_type)) = (
            block.get("data").and_then(JsonValue::as_str),
            block.get("mimeType").and_then(JsonValue::as_str),
        ) else {
            continue;
        };
        images.push(crate::ai::types::ImageContent {
            data: data.to_owned(),
            mime_type: mime_type.to_owned(),
        });
    }
    Some(images)
}

/// Apply a `tool_result` handler merge onto a finalized tool call result
/// (upstream runner.ts:1183-1240 semantics; the usage entry is returned for
/// the caller to attach to the tool-result message, which owns usage).
pub fn apply_tool_result_merge(
    result: &mut crate::agent::tool_batch::ToolResult,
    merge: &ToolResultMerge,
) -> Option<serde_json::Value> {
    let content_replaced =
        merge.content_blocks.is_some() || merge.content.is_some() || merge.images.is_some();
    if let Some(blocks) = &merge.content_blocks {
        // Upstream replaces the whole content ARRAY: text blocks become the
        // result text, image blocks become the result images. A text-only
        // replacement therefore REMOVES the previous images instead of
        // leaving them behind next to the new text.
        let mut text = String::new();
        let mut images = Vec::new();
        for block in blocks {
            match block {
                ToolResultContentBlock::Text(part) => {
                    if !text.is_empty() {
                        text.push('\n');
                    }
                    text.push_str(part);
                }
                ToolResultContentBlock::Image(image) => images.push(image.clone()),
            }
        }
        result.content = text;
        result.images = images;
    } else if let Some(content) = &merge.content {
        // Legacy text-only form (a bare string): replace the text and keep
        // the existing images, matching its historical behavior.
        result.content = content.clone();
    }
    if let Some(images) = &merge.images {
        result.images = images.clone();
    }
    if content_replaced {
        // Structured content not replaced along with the content may no longer
        // match it (upstream merge rule).
        if merge.structured_content.is_none() {
            result.structured_content = None;
        }
    }
    if let Some(details) = &merge.details {
        result.details = Some(details.clone());
    }
    if let Some(structured) = &merge.structured_content {
        result.structured_content = Some(structured.clone());
    }
    if let Some(is_error) = merge.is_error {
        result.is_error = is_error;
    }
    merge.usage.clone()
}

/// Lenient parse of a `tool_result` handler's reported usage into
/// [`crate::ai::types::Usage`] (handler JSON may omit fields; missing numbers
/// default to 0 and `totalTokens` falls back to `input + output`).
pub fn parse_usage_json(value: &serde_json::Value) -> Option<crate::ai::types::Usage> {
    use crate::ai::types::{Cost, Usage};
    if let Ok(usage) = serde_json::from_value::<Usage>(value.clone()) {
        return Some(usage);
    }
    let object = value.as_object()?;
    let get_u64 = |key: &str| object.get(key).and_then(JsonValue::as_u64).unwrap_or(0);
    let input = get_u64("input");
    let output = get_u64("output");
    Some(Usage {
        input,
        output,
        cache_read: get_u64("cacheRead"),
        cache_write: get_u64("cacheWrite"),
        cache_write_1h: object.get("cacheWrite1h").and_then(JsonValue::as_u64),
        reasoning: object.get("reasoning").and_then(JsonValue::as_u64),
        total_tokens: get_u64("totalTokens").max(input.saturating_add(output)),
        cost: Cost {
            input: 0.0,
            output: 0.0,
            cache_read: 0.0,
            cache_write: 0.0,
            total: 0.0,
            // A handler-reported record is not a provider-billed total.
            total_from_provider: None,
        },
    })
}

// ---------------------------------------------------------------------------
// Hook types (optional, for the scheduler)
// ---------------------------------------------------------------------------

/// Best-effort, bounded text for a caught panic payload. Tool-result text is
/// model-facing, so an arbitrary `panic_any` value is not formatted and the
/// message is capped.
pub(crate) fn panic_payload_message(payload: &(dyn std::any::Any + Send)) -> String {
    let text = if let Some(message) = payload.downcast_ref::<&str>() {
        (*message).to_owned()
    } else if let Some(message) = payload.downcast_ref::<String>() {
        message.clone()
    } else {
        "non-string panic payload".to_owned()
    };
    let bounded: String = text.chars().take(500).collect();
    if bounded.is_empty() {
        "panic with an empty payload".to_owned()
    } else {
        bounded
    }
}

/// Result of a `beforeToolCall` check.
#[derive(Debug, Clone, Default)]
pub struct BeforeToolCallResult {
    /// When true, the tool call is blocked before execution.
    pub block: bool,
    /// Reason shown in the error result when blocked.
    pub reason: Option<String>,
    /// Whether the blocked call participates in batch termination.
    pub terminate: bool,
}

/// One content block in an `afterToolCall` content override.
///
/// Upstream `AfterToolCallResult.content` is an array of `TextContent` /
/// `ImageContent` blocks. pir's execution-time [`ToolResult`] keeps text and
/// images in separate fields (`content` string, `images` vector), so the
/// block array is flattened on apply: text blocks are joined with newlines
/// and image blocks are appended after the text. Interleaving order between
/// text and images cannot be represented by [`ToolResult`] and is therefore
/// not preserved.
#[derive(Debug, Clone, PartialEq)]
pub enum ToolResultContentBlock {
    /// A model-facing text block.
    Text(String),
    /// An image block appended after the text.
    Image(crate::ai::types::ImageContent),
}

/// Result of an `afterToolCall` override.
#[derive(Debug, Clone, Default)]
pub struct AfterToolCallResult {
    /// Replaces the text content when set (legacy string form; image blocks
    /// are left untouched). [`Self::content_blocks`] is the full-array form.
    pub content: Option<String>,
    /// Replaces the full content BLOCK array when set (upstream
    /// `AfterToolCallResult.content`). Takes precedence over [`Self::content`]
    /// when both are set.
    pub content_blocks: Option<Vec<ToolResultContentBlock>>,
    /// Replaces details when set.
    pub details: Option<serde_json::Value>,
    /// Replaces the structured content when set. When content is replaced
    /// without `structured_content`, the structured content is dropped — it
    /// may no longer match the content (upstream agent-loop.ts merge rule).
    pub structured_content: Option<serde_json::Value>,
    /// Replaces the error flag when set.
    pub is_error: Option<bool>,
    /// Replaces the tool result usage when set (upstream
    /// `AfterToolCallResult.usage`). [`ToolResult`] has no usage field, so the
    /// value is returned alongside the rewritten result and carried on the
    /// finalized tool call to the tool-result message.
    pub usage: Option<crate::ai::types::Usage>,
    /// Replaces the terminate hint when set.
    pub terminate: Option<bool>,
}

/// Apply an `afterToolCall` override field-by-field (no deep merge).
///
/// Upstream structured-content rule: an explicit `structuredContent` replaces
/// the recorded one; replacing `content` without also returning
/// `structuredContent` drops it, because it may no longer match the content.
///
/// The returned usage is the hook's reported usage (upstream keeps it on the
/// executed tool result; pir's [`ToolResult`] has no usage slot, so the caller
/// threads it to the tool-result message).
pub fn apply_after_tool_call_result(
    mut result: ToolResult,
    overrides: AfterToolCallResult,
) -> (ToolResult, Option<crate::ai::types::Usage>) {
    let content_replaced = overrides.content_blocks.is_some() || overrides.content.is_some();
    if let Some(blocks) = overrides.content_blocks {
        let mut text = String::new();
        let mut images = Vec::new();
        for block in blocks {
            match block {
                ToolResultContentBlock::Text(part) => {
                    if !text.is_empty() {
                        text.push('\n');
                    }
                    text.push_str(&part);
                }
                ToolResultContentBlock::Image(image) => images.push(image),
            }
        }
        result.content = text;
        result.images = images;
    } else if let Some(content) = overrides.content {
        result.content = content;
    }
    if let Some(details) = overrides.details {
        result.details = Some(details);
    }
    match overrides.structured_content {
        Some(structured) => result.structured_content = Some(structured),
        // Content was replaced without new structured content: drop it, it
        // may no longer match the content.
        None => {
            if content_replaced {
                result.structured_content = None;
            }
        }
    }
    if let Some(is_error) = overrides.is_error {
        result.is_error = is_error;
    }
    if let Some(terminate) = overrides.terminate {
        result.terminate = terminate;
    }
    (result, overrides.usage)
}

/// Async before-hook.
pub type BeforeHook = Box<
    dyn Fn(
            &crate::agent::tool_batch::PreparedToolCall,
            &ToolContext,
        ) -> BoxFuture<'static, BeforeToolCallResult>
        + Send
        + Sync,
>;

/// Async after-hook.
pub type AfterHook = Box<
    dyn Fn(
            &crate::agent::tool_batch::PreparedToolCall,
            &ToolResult,
            &ToolContext,
        ) -> BoxFuture<'static, AfterToolCallResult>
        + Send
        + Sync,
>;

/// Optional lifecycle hooks attached to a batch execution.
#[derive(Default)]
pub struct ToolHooks {
    /// Called before each tool is executed (after argument validation).
    pub before: Option<BeforeHook>,
    /// Called after each tool finishes (before events are emitted).
    pub after: Option<AfterHook>,
}

/// Session-installed sink that adopts background (`background: true`) tool
/// calls (see [`crate::agent::tool_scheduler::ToolScheduler::with_detach`]).
///
/// The scheduler hands over a fully-owned
/// [`crate::agent::tool_batch::DetachedToolCall`]; the sink takes the execution
/// future (via [`crate::agent::tool_batch::DetachedToolCall::take_future`]),
/// spawns it, and owns delivery of the real result. Returning `false` refuses
/// the call — the host's background limit is full — and the batch finalizes it
/// as a refusal error instead. A scheduler that cannot detach (no sink, or
/// borrowed hooks) likewise REFUSES the call with an actionable error; it never
/// silently runs a `background: true` call in the foreground.
pub type DetachSink =
    Arc<dyn Fn(&mut crate::agent::tool_batch::DetachedToolCall) -> bool + Send + Sync>;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Extract all `ToolCall` content blocks from an assistant message and convert
/// them to `AgentToolCall`s.
pub fn tool_calls_from_message(message: &crate::ai::types::AssistantMessage) -> Vec<AgentToolCall> {
    let mut calls = Vec::new();
    for (content_index, block) in message.content.iter().enumerate() {
        if let crate::ai::types::ContentBlock::ToolCall(tool_call) = block {
            // Build the `serde_json` map in one pass: cloning the source map
            // and then collecting it again would allocate it twice.
            let arguments: serde_json::Map<String, JsonValue> = tool_call
                .arguments
                .iter()
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect();
            let mut call = AgentToolCall::new(
                content_index,
                &tool_call.id,
                &tool_call.name,
                serde_json::Value::Object(arguments),
            );
            call.arguments_error = tool_call.arguments_error.clone();
            calls.push(call);
        }
    }
    calls
}

/// Produce error `FinalizedToolCall`s for every tool call, used when
/// `stop_reason` is `Length`.
///
/// The text is upstream `failToolCallsFromTruncatedMessage`
/// (`agent-loop.ts:489-492`): it says WHY the call did not run and what to do
/// next ("Re-issue the tool call with complete arguments"), which the previous
/// "Model input was truncated" wording did not.
pub fn error_results_for_length(
    calls: &[AgentToolCall],
) -> Vec<crate::agent::tool_batch::FinalizedToolCall> {
    calls
        .iter()
        .map(|call| crate::agent::tool_batch::FinalizedToolCall {
            index: call.index,
            call_id: call.call_id.clone(),
            tool_name: call.tool_name.clone(),
            result: ToolResult::error(format!(
                "Tool call \"{}\" was not executed: the response hit the output token limit, \
                 so its arguments may be truncated. Re-issue the tool call with complete \
                 arguments.",
                call.tool_name
            )),
            usage: None,
            duration_ms: None,
        })
        .collect()
}
